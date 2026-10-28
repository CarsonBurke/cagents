//! Immutable native diff models computed away from the application update thread.

use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
    path::Path,
    sync::{Arc, Condvar, LazyLock, Mutex},
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

/// A displayed line borrows an immutable source allocation. Only explicit
/// end-of-line annotations own a separate, small string.
#[derive(Clone)]
pub struct SharedLine {
    source: Arc<str>,
    start: u32,
    end: u32,
}

impl SharedLine {
    fn slice(source: &Arc<str>, text: &str) -> Self {
        let start = (text.as_ptr() as usize)
            .checked_sub(source.as_ptr() as usize)
            .expect("display line belongs to its source");
        let end = start.checked_add(text.len()).expect("line end fits source");
        assert!(end <= source.len(), "display line belongs to its source");
        Self {
            source: Arc::clone(source),
            start: u32::try_from(start).expect("bounded diff source"),
            end: u32::try_from(end).expect("bounded diff source"),
        }
    }
    pub fn as_str(&self) -> &str {
        &self.source[self.start as usize..self.end as usize]
    }
}
impl std::ops::Deref for SharedLine {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl AsRef<str> for SharedLine {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl std::fmt::Debug for SharedLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}
impl std::fmt::Display for SharedLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}
impl PartialEq for SharedLine {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}
impl Eq for SharedLine {}
impl PartialEq<&str> for SharedLine {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
impl PartialEq<String> for SharedLine {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}
impl From<Arc<str>> for SharedLine {
    fn from(source: Arc<str>) -> Self {
        let end = u32::try_from(source.len()).expect("bounded diff source");
        Self {
            source,
            start: 0,
            end,
        }
    }
}
impl From<String> for SharedLine {
    fn from(text: String) -> Self {
        Arc::<str>::from(text).into()
    }
}
impl From<&str> for SharedLine {
    fn from(text: &str) -> Self {
        Arc::<str>::from(text).into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineDiffRow {
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: SharedLine,
    pub kind: DiffRowKind,
    /// UTF-8 byte ranges in the displayed text, computed once on the diff worker.
    pub changed: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitDiffRow {
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub old_text: Option<SharedLine>,
    pub new_text: Option<SharedLine>,
    pub kind: DiffRowKind,
    pub old_changed: Vec<Range<usize>>,
    pub new_changed: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffModel {
    /// Complete line extents, retained for constant-time review anchor checks.
    pub line_counts: [usize; 2],
    /// Whether each side exists; an empty file differs from an absent side.
    pub present: [bool; 2],
    /// Reader-pinned HEAD revision for mutations of a staged comparison.
    pub source_head: Option<crate::repository::GitHeadGuard>,
    /// File modes read with the sources; unknown for pure in-memory builders.
    pub modes: [Option<u32>; 2],
    /// Exact source baseline, including original line endings and final newline.
    pub before: Arc<str>,
    /// Exact new-side source, retained for patch and selection operations.
    pub after: Arc<str>,
    pub inline_rows: Vec<InlineDiffRow>,
    pub split_rows: Vec<SplitDiffRow>,
    pub inline_change_starts: Vec<usize>,
    pub split_change_starts: Vec<usize>,
    /// Maximal runs of unchanged rows, in file order, for `[inline, split]`.
    /// Both layouts hold the same runs with the same lengths, so run `i` of
    /// one names exactly the lines of run `i` of the other; only their row
    /// indices differ. Presentation decides how much of each run to fold.
    pub unchanged: [Vec<Range<usize>>; 2],
    pub additions: u32,
    pub deletions: u32,
    /// Shaped logical-pixel extents for [light, dark], rounded up on the worker.
    pub max_line_width: [u32; 2],
    pub syntax: DiffSyntax,
}

/// Self-contained native styles retain exact RGBA8 colors and the bundled
/// highlighter's monospace bold/italic variants in sixteen bytes. Rendering
/// requires no shared palette lookup or font-system lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxSpan {
    start: u32,
    end: u32,
    color: Option<[u8; 4]>,
    font: u8,
}

impl SyntaxSpan {
    fn new(range: Range<usize>, color: Option<[u8; 4]>, font: Option<cosmic::iced::Font>) -> Self {
        use cosmic::iced::font;
        Self {
            start: u32::try_from(range.start).expect("bounded syntax line"),
            end: u32::try_from(range.end).expect("bounded syntax line"),
            color,
            font: font.map_or(0, |font| {
                1 | u8::from(font.weight == font::Weight::Bold) << 1
                    | u8::from(font.style == font::Style::Italic) << 2
            }),
        }
    }
    fn range(&self) -> Range<usize> {
        self.start as usize..self.end as usize
    }
    fn font(&self) -> Option<cosmic::iced::Font> {
        use cosmic::iced::{Font, font};
        (self.font != 0).then_some(Font {
            weight: if self.font & 2 != 0 {
                font::Weight::Bold
            } else {
                font::Weight::Normal
            },
            style: if self.font & 4 != 0 {
                font::Style::Italic
            } else {
                font::Style::Normal
            },
            ..Font::MONOSPACE
        })
    }
}

/// Full-side lexical state, cached for the light and dark palettes of one
/// syntax theme. A context row can have different syntax on each side after a
/// comment delimiter changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffSyntax {
    old: Arc<SyntaxLines>,
    new: Arc<SyntaxLines>,
    /// Which palette pair `old` and `new` were coloured from. Two models built
    /// from the same text under different themes are genuinely different
    /// models, so this belongs in the identity the cache compares.
    theme: crate::settings::EditorSyntaxTheme,
}

impl DiffSyntax {
    pub(crate) fn line(&self, old: bool, line: Option<u32>, dark: bool) -> &[SyntaxSpan] {
        let side = if old { &self.old } else { &self.new };
        line.and_then(|line| line.checked_sub(1))
            .and_then(|line| side[usize::from(dark)].get(line as usize))
            .map_or(&[], |spans| spans.as_ref())
    }
}

/// Native bundled grammar names are shared by the editable buffer and diffs.
/// CUDA uses the bundled C++ grammar (there is no bundled CUDA grammar).
pub(crate) fn highlighter_settings(
    path: &Path,
    theme: crate::settings::EditorSyntaxTheme,
    dark: bool,
) -> cosmic::iced::highlighter::Settings {
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
        theme: syntax_highlighter_theme(theme, dark),
        token,
    }
}

pub(crate) fn syntax_highlighter_theme(
    theme: crate::settings::EditorSyntaxTheme,
    dark: bool,
) -> cosmic::iced::highlighter::Theme {
    use cosmic::iced::highlighter::Theme;
    match theme {
        crate::settings::EditorSyntaxTheme::Auto => {
            if dark {
                Theme::Base16Ocean
            } else {
                Theme::InspiredGitHub
            }
        }
        crate::settings::EditorSyntaxTheme::SolarizedDark => Theme::SolarizedDark,
        crate::settings::EditorSyntaxTheme::Mocha => Theme::Base16Mocha,
        crate::settings::EditorSyntaxTheme::Ocean => Theme::Base16Ocean,
        crate::settings::EditorSyntaxTheme::Eighties => Theme::Base16Eighties,
        crate::settings::EditorSyntaxTheme::InspiredGitHub => Theme::InspiredGitHub,
    }
}

/// syntect's own name for the palette [`syntax_highlighter_theme`] selects.
///
/// The diff worker drives syntect directly — it colours every scope for both
/// palettes from one parse, which iced's per-line `Stream` cannot do — and
/// iced keeps `Theme::key` private, so the name has to be repeated here. It is
/// derived from the same function the editable buffer uses, so the two
/// surfaces cannot disagree about *which* palette the setting means.
fn syntect_theme_name(theme: crate::settings::EditorSyntaxTheme, dark: bool) -> &'static str {
    use cosmic::iced::highlighter::Theme;
    match syntax_highlighter_theme(theme, dark) {
        Theme::SolarizedDark => "Solarized (dark)",
        Theme::Base16Mocha => "base16-mocha.dark",
        Theme::Base16Ocean => "base16-ocean.dark",
        Theme::Base16Eighties => "base16-eighties.dark",
        Theme::InspiredGitHub => "InspiredGitHub",
    }
}

/// One text's highlighted lines, the light palette first and the dark second.
pub(crate) type SyntaxLines = [Vec<Arc<[SyntaxSpan]>>; 2];

/// Immutable grammar/theme tables, identical to iced's highlighter. Parsing is
/// theme-independent: advance one parser and scope stack, then color each scope
/// for both of `theme`'s palettes without reparsing or cloning the parser at
/// every line.
///
/// Both palettes are produced because the application's light/dark mode can
/// change without the document changing, and a theme that is not `Auto` simply
/// yields the same palette twice.
#[cfg(test)]
fn highlight_side(
    text: &str,
    path: &Path,
    theme: crate::settings::EditorSyntaxTheme,
    cancellation: &CancellationToken,
) -> Result<SyntaxLines, Cancelled> {
    let settings = highlighter_settings(path, theme, false);
    highlight_lines(text, &settings.token, theme, cancellation)
}

/// A whole snippet highlighted in one pass, which is what highlighting it a
/// line at a time through [`SnippetHighlighter`] is checked against.
#[cfg(test)]
pub(crate) fn highlight_snippet(
    code: &str,
    token: &str,
    theme: crate::settings::EditorSyntaxTheme,
) -> SyntaxLines {
    highlight_lines(code, token, theme, &CancellationToken::new())
        .unwrap_or_else(|Cancelled| unreachable!("a fresh token is never cancelled"))
}

/// Highlights a snippet whose language is named by `token` - a Markdown
/// fence's info string, say - in both of `theme`'s palettes, a line at a
/// time.
///
/// This is the highlighting a diff gets, applied to text that has no path:
/// the same grammars, the same palettes, and the same spans, so code an agent
/// quotes reads the way the same code reads in the editor beside it.
///
/// It takes lines rather than the snippet because a fence an agent is
/// streaming grows by a few words per frame, and highlighting all of it again
/// for each of them costs its whole length every time. The lexical state
/// after a line depends only on the lines before it, so the caller keeps this
/// at the last whole line it has and feeds it what came after.
pub(crate) struct SnippetHighlighter {
    parser: HighlightParser,
    /// A snippet is highlighted where it is cached, so nothing cancels it.
    uncancelled: CancellationToken,
}

impl SnippetHighlighter {
    pub(crate) fn new(token: &str, theme: crate::settings::EditorSyntaxTheme) -> Self {
        Self {
            parser: HighlightParser::new(token, theme),
            uncancelled: CancellationToken::new(),
        }
    }

    /// Highlights the snippet's next line, in both palettes, and moves past
    /// it.
    pub(crate) fn line(&mut self, line: &str) -> [Vec<SyntaxSpan>; 2] {
        self.parser
            .line(line, &self.uncancelled)
            .unwrap_or_else(|Cancelled| unreachable!("a fresh token is never cancelled"))
    }

    /// Highlights a last line that has no ending yet without moving past it,
    /// so the rest of the line can still arrive.
    pub(crate) fn unfinished(&mut self, line: &str) -> [Vec<SyntaxSpan>; 2] {
        let state = (self.parser.parser.clone(), self.parser.stack.clone());
        let spans = self.line(line);
        (self.parser.parser, self.parser.stack) = state;
        spans
    }
}

/// The spans that draw one line of `text` under its highlights alone, with no
/// change to mark.
pub(crate) fn syntax_spans<'a>(
    text: &'a str,
    syntax: &[SyntaxSpan],
) -> Vec<cosmic::iced::widget::text::Span<'a>> {
    line_spans(text, &[], syntax, cosmic::iced::Color::TRANSPARENT)
}

use two_face::re_exports::syntect::{highlighting, parsing};

#[cfg(test)]
thread_local! { static WORK_COUNTS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0,0)) }; }
/// Parsers and a preparation job share grammar/regex storage. The rendezvous
/// keeps no strong owner after the last job or streaming parser has finished.
#[derive(Default)]
struct SyntaxCache {
    current: std::sync::Mutex<std::sync::Weak<parsing::SyntaxSet>>,
    #[cfg(test)]
    loads: std::sync::atomic::AtomicUsize,
}
impl SyntaxCache {
    fn acquire(&self) -> SyntaxLease {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(syntaxes) = current.upgrade() {
            return SyntaxLease(syntaxes);
        }
        let syntaxes = Arc::new(two_face::syntax::extra_no_newlines());
        #[cfg(test)]
        self.loads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *current = Arc::downgrade(&syntaxes);
        SyntaxLease(syntaxes)
    }
}

pub(crate) struct SyntaxLease(Arc<parsing::SyntaxSet>);

pub(crate) fn syntax_lease() -> SyntaxLease {
    static CACHE: LazyLock<SyntaxCache> = LazyLock::new(SyntaxCache::default);
    CACHE.acquire()
}

struct HighlightParser {
    syntaxes: SyntaxLease,
    parser: parsing::ParseState,
    stack: parsing::ScopeStack,
    highlighters: [highlighting::Highlighter<'static>; 2],
    styles: HashMap<Vec<parsing::Scope>, [highlighting::StyleModifier; 2]>,
}

impl HighlightParser {
    fn new(token: &str, theme: crate::settings::EditorSyntaxTheme) -> Self {
        Self::with_syntaxes(token, theme, syntax_lease())
    }

    fn with_syntaxes(
        token: &str,
        theme: crate::settings::EditorSyntaxTheme,
        syntaxes: SyntaxLease,
    ) -> Self {
        static THEMES: LazyLock<highlighting::ThemeSet> =
            LazyLock::new(highlighting::ThemeSet::load_defaults);
        let syntax = syntaxes
            .0
            .find_syntax_by_token(token)
            .unwrap_or_else(|| syntaxes.0.find_syntax_plain_text());
        let parser = parsing::ParseState::new(syntax);
        Self {
            syntaxes,
            parser,
            stack: parsing::ScopeStack::new(),
            highlighters: [false, true].map(|dark| {
                highlighting::Highlighter::new(&THEMES.themes[syntect_theme_name(theme, dark)])
            }),
            styles: HashMap::new(),
        }
    }

    fn line(
        &mut self,
        line: &str,
        cancellation: &CancellationToken,
    ) -> Result<[Vec<SyntaxSpan>; 2], Cancelled> {
        check_cancelled(cancellation)?;
        #[cfg(test)]
        WORK_COUNTS.with(|counts| {
            let (highlight, shape) = counts.get();
            counts.set((highlight + 1, shape));
        });
        let line = line
            .strip_suffix("\r\n")
            .or_else(|| line.strip_suffix('\n'))
            .unwrap_or(line);
        let ops = self
            .parser
            .parse_line(line, &self.syntaxes.0)
            .unwrap_or_default();
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
                // Theme selector matching allocates and sorts; identical lexical
                // scopes recur throughout a file, so resolve each stack once.
                let resolved =
                    self.styles
                        .get(self.stack.scopes.as_slice())
                        .copied()
                        .unwrap_or_else(|| {
                            let resolved = self.highlighters.each_ref().map(|highlighter| {
                                highlighter.style_mod_for_stack(&self.stack.scopes)
                            });
                            if self.styles.len() < 4096 {
                                self.styles.insert(self.stack.scopes.clone(), resolved);
                            }
                            resolved
                        });
                for (palette, style) in resolved.into_iter().enumerate() {
                    let color = style
                        .foreground
                        .map(|color| [color.r, color.g, color.b, color.a]);
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
                    spans[palette].push(SyntaxSpan::new(start..end, color, font));
                }
            }
            if let Some(op) = op {
                let _ = self.stack.apply(op);
            }
            start = end;
        }

        Ok(spans)
    }
}

#[cfg(test)]
fn highlight_lines(
    text: &str,
    token: &str,
    theme: crate::settings::EditorSyntaxTheme,
    cancellation: &CancellationToken,
) -> Result<SyntaxLines, Cancelled> {
    let mut parser = HighlightParser::new(token, theme);
    let mut lines: SyntaxLines = Default::default();
    for line in text.split_inclusive('\n') {
        for (lines, spans) in lines.iter_mut().zip(parser.line(line, cancellation)?) {
            lines.push(spans.into());
        }
    }
    Ok(lines)
}

/// Only source and immutable colored spans are retained. Native parser state
/// owns private, potentially deep grammar stacks and is never cached per line.
struct BaselineSyntax {
    text: Arc<str>,
    hash: blake3::Hash,
    token: String,
    theme: crate::settings::EditorSyntaxTheme,
    syntax: Arc<SyntaxLines>,
    retained_bytes: usize,
}

struct BaselineFlight {
    text: Arc<str>,
    hash: blake3::Hash,
    token: String,
    theme: crate::settings::EditorSyntaxTheme,
    finished: Mutex<bool>,
    ready: Condvar,
}

/// Cancellation or a failed builder wakes waiters, which may retry. The
/// global cache lock never spans parsing, waiting, or renderer/font work.
struct BaselineBuilder(Arc<BaselineFlight>);
impl Drop for BaselineBuilder {
    fn drop(&mut self) {
        BASELINES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .retain(|entry| !Arc::ptr_eq(entry, &self.0));
        *self
            .0
            .finished
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = true;
        self.0.ready.notify_all();
    }
}

#[derive(Default)]
struct BaselineCache {
    entries: VecDeque<Arc<BaselineSyntax>>,
    pending: Vec<Arc<BaselineFlight>>,
    bytes: usize,
}

const BASELINE_CACHE_BYTES: usize = 64 * 1024 * 1024;
const BASELINE_CACHE_ENTRIES: usize = 8;
static BASELINES: LazyLock<Mutex<BaselineCache>> =
    LazyLock::new(|| Mutex::new(BaselineCache::default()));

fn baseline_syntax(
    before: &Arc<str>,
    token: &str,
    theme: crate::settings::EditorSyntaxTheme,
    cancellation: &CancellationToken,
) -> Result<Arc<BaselineSyntax>, Cancelled> {
    let hash = blake3::hash(before.as_bytes());
    let builder = loop {
        check_cancelled(cancellation)?;
        let mut cache = BASELINES.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(index) = cache.entries.iter().position(|entry| {
            entry.hash == hash
                && entry.token == token
                && entry.theme == theme
                && entry.text.as_ref() == before.as_ref()
        }) {
            let entry = cache.entries.remove(index).expect("matched baseline");
            cache.entries.push_back(Arc::clone(&entry));
            return Ok(entry);
        }
        if let Some(flight) = cache
            .pending
            .iter()
            .find(|entry| {
                entry.hash == hash
                    && entry.token == token
                    && entry.theme == theme
                    && entry.text.as_ref() == before.as_ref()
            })
            .cloned()
        {
            drop(cache);
            let mut finished = flight
                .finished
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            while !*finished {
                check_cancelled(cancellation)?;
                finished = flight
                    .ready
                    .wait_timeout(finished, std::time::Duration::from_millis(25))
                    .unwrap_or_else(|error| error.into_inner())
                    .0;
            }
        } else {
            let flight = Arc::new(BaselineFlight {
                text: Arc::clone(before),
                hash,
                token: token.to_owned(),
                theme,
                finished: Mutex::new(false),
                ready: Condvar::new(),
            });
            cache.pending.push(Arc::clone(&flight));
            break BaselineBuilder(flight);
        }
    };
    let mut parser = HighlightParser::new(token, theme);
    let mut syntax: SyntaxLines = Default::default();
    for text in before.split_inclusive('\n') {
        for (lines, spans) in syntax.iter_mut().zip(parser.line(text, cancellation)?) {
            lines.push(spans.into());
        }
    }
    // This is a payload bound, including Arc headers/vector capacities. The
    // allocator's metadata/fragmentation and in-flight requests are measured
    // separately; no opaque parser heap is retained inside this cache.
    let retained_bytes = std::mem::size_of::<BaselineSyntax>()
        + before.len()
        + 6 * std::mem::size_of::<usize>()
        + token.len()
        + syntax
            .iter()
            .map(|lines| {
                lines.capacity() * std::mem::size_of::<Arc<[SyntaxSpan]>>()
                    + lines
                        .iter()
                        .map(|spans| {
                            std::mem::size_of_val(spans.as_ref()) + 2 * std::mem::size_of::<usize>()
                        })
                        .sum::<usize>()
            })
            .sum::<usize>();
    let baseline = Arc::new(BaselineSyntax {
        text: Arc::clone(before),
        hash,
        token: token.to_owned(),
        theme,
        syntax: Arc::new(syntax),
        retained_bytes,
    });
    {
        let mut cache = BASELINES.lock().unwrap_or_else(|error| error.into_inner());
        if retained_bytes <= BASELINE_CACHE_BYTES {
            while cache.entries.len() >= BASELINE_CACHE_ENTRIES
                || cache.bytes + retained_bytes > BASELINE_CACHE_BYTES
            {
                let Some(oldest) = cache.entries.pop_front() else {
                    break;
                };
                cache.bytes -= oldest.retained_bytes;
            }
            cache.bytes += retained_bytes;
            cache.entries.push_back(Arc::clone(&baseline));
        }
    }
    drop(builder);
    Ok(baseline)
}

/// State-only forward parsing is linear in the baseline, with at most one
/// live old-side stack. Sharing a colored run never stores deep checkpoints.
fn advance_lexical(
    parser: &mut HighlightParser,
    lines: &[&str],
    cursor: &mut usize,
    end: usize,
    cancellation: &CancellationToken,
) -> Result<(), Cancelled> {
    debug_assert!(*cursor <= end);
    for text in &lines[*cursor..end] {
        check_cancelled(cancellation)?;
        let text = text
            .strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .unwrap_or(text);
        for (_, op) in parser
            .parser
            .parse_line(text, parser.syntaxes)
            .unwrap_or_default()
        {
            let _ = parser.stack.apply(&op);
        }
    }
    *cursor = end;
    Ok(())
}

/// Reuse each unchanged run independently. Several distant edits must not
/// recolor the entire interval between their first and last changed lines.
#[cfg(test)]
fn highlight_comparison(
    before: &Arc<str>,
    after: &str,
    path: &Path,
    theme: crate::settings::EditorSyntaxTheme,
    cancellation: &CancellationToken,
) -> Result<DiffSyntax, Cancelled> {
    let model = build_diff_rows(before, after, cancellation)?;
    highlight_comparison_model(before, after, path, theme, cancellation, &model)
}

fn highlight_comparison_model(
    before: &Arc<str>,
    after: &str,
    path: &Path,
    theme: crate::settings::EditorSyntaxTheme,
    cancellation: &CancellationToken,
    model: &DiffModel,
) -> Result<DiffSyntax, Cancelled> {
    let token = highlighter_settings(path, theme, false).token;
    let baseline = baseline_syntax(before, &token, theme, cancellation)?;
    if before.as_ref() == after {
        return Ok(DiffSyntax {
            old: Arc::clone(&baseline.syntax),
            new: Arc::clone(&baseline.syntax),
            theme,
        });
    }
    let old_lines: Vec<_> = before.split_inclusive('\n').collect();
    let new_lines: Vec<_> = after.split_inclusive('\n').collect();
    let mut corresponding = vec![None; new_lines.len()];
    for row in &model.split_rows {
        check_cancelled(cancellation)?;
        if let (Some(old), Some(new)) = (row.old_line, row.new_line) {
            let old = old as usize - 1;
            let new = new as usize - 1;
            // Ignore-whitespace context is not necessarily lexically equal.
            if old_lines[old] == new_lines[new] {
                corresponding[new] = Some(old);
            }
        }
    }
    let mut parser = HighlightParser::new(&token, theme);
    let mut old_parser = HighlightParser::new(&token, theme);
    let mut old_cursor = 0;
    let mut new: SyntaxLines = std::array::from_fn(|_| Vec::with_capacity(new_lines.len()));
    let mut index = 0;
    let mut probe = 0;
    while index < new_lines.len() {
        check_cancelled(cancellation)?;
        if let Some(old) = corresponding[index] {
            let starts_run =
                index == 0 || corresponding[index - 1].is_none_or(|previous| previous + 1 != old);
            if starts_run || index >= probe {
                advance_lexical(
                    &mut old_parser,
                    &old_lines,
                    &mut old_cursor,
                    old,
                    cancellation,
                )?;
                if parser.parser == old_parser.parser && parser.stack == old_parser.stack {
                    let mut end = index + 1;
                    while end < corresponding.len() && corresponding[end] == Some(old + end - index)
                    {
                        end += 1;
                    }
                    for (palette, lines) in new.iter_mut().enumerate() {
                        for spans in &baseline.syntax[palette][old..old + end - index] {
                            check_cancelled(cancellation)?;
                            lines.push(Arc::clone(spans));
                        }
                    }
                    advance_lexical(
                        &mut old_parser,
                        &old_lines,
                        &mut old_cursor,
                        old + end - index,
                        cancellation,
                    )?;
                    parser.parser = old_parser.parser.clone();
                    parser.stack = old_parser.stack.clone();
                    index = end;
                    probe = index;
                    continue;
                }
                // A multiline delimiter may alter an arbitrarily long suffix.
                // Check convergence at bounded intervals while advancing the old state only once.
                probe = index + 128 - old % 128;
            }
        } else {
            probe = index + 1;
        }
        for (lines, spans) in new
            .iter_mut()
            .zip(parser.line(new_lines[index], cancellation)?)
        {
            lines.push(spans.into());
        }
        index += 1;
    }
    Ok(DiffSyntax {
        old: Arc::clone(&baseline.syntax),
        new: Arc::new(new),
        theme,
    })
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

/// Builds a highlighted comparison against an exact in-memory document.
///
/// Uses the same rows, syntax, and native metrics as repository comparisons,
/// without invoking Git. `None` means cancellation; no partial model is exposed.
/// Pass an existing `Arc<str>` to retain the baseline without copying its bytes.
pub fn build_document_diff(
    before: impl Into<Arc<str>>,
    after: &str,
    path: &Path,
    theme: crate::settings::EditorSyntaxTheme,
    cancellation: &CancellationToken,
) -> Option<DiffModel> {
    check_cancelled(cancellation).ok()?;
    build_document_diff_with_options(before, after, path, theme, false, cancellation)
}

/// Compare original source lines while optionally ignoring horizontal whitespace.
/// Normalization changes line identities only; rendered text and line numbers
/// always refer to the original sources.
pub fn build_document_diff_with_options(
    before: impl Into<Arc<str>>,
    after: &str,
    path: &Path,
    theme: crate::settings::EditorSyntaxTheme,
    ignore_whitespace: bool,
    cancellation: &CancellationToken,
) -> Option<DiffModel> {
    build_document_diff_sources(
        before.into(),
        Arc::from(after),
        path,
        theme,
        ignore_whitespace,
        cancellation,
    )
}

/// Snapshot entry point: both exact sources are retained without recopying.
/// A Send editor snapshot can materialize its Arc on the worker and pass it here.
pub fn build_document_diff_sources(
    before: Arc<str>,
    after: Arc<str>,
    path: &Path,
    theme: crate::settings::EditorSyntaxTheme,
    ignore_whitespace: bool,
    cancellation: &CancellationToken,
) -> Option<DiffModel> {
    build_document_diff_sources_with_font(
        before,
        after,
        path,
        theme,
        ignore_whitespace,
        cosmic::iced::Font::MONOSPACE,
        cancellation,
    )
}

/// Retain exact sources and measure with the same family rendered by the editor.
/// Syntax colors are family-independent and keep sharing the baseline cache.
pub fn build_document_diff_sources_with_font(
    before: Arc<str>,
    after: Arc<str>,
    path: &Path,
    theme: crate::settings::EditorSyntaxTheme,
    ignore_whitespace: bool,
    font: cosmic::iced::Font,
    cancellation: &CancellationToken,
) -> Option<DiffModel> {
    check_cancelled(cancellation).ok()?;
    let mut model =
        build_diff_rows_from_sources(&before, &after, ignore_whitespace, cancellation).ok()?;
    model.syntax =
        highlight_comparison_model(&before, &after, path, theme, cancellation, &model).ok()?;
    measure_diff_with_font(&mut model, font, cancellation).ok()?;
    check_cancelled(cancellation).ok()?;
    Some(model)
}

/// The native shaper joins adjacent spans with the same font attributes.
/// Color, change backgrounds and link metadata do not change glyph advances.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ShapeKey<'a> {
    text: &'a str,
    fonts: Vec<(Range<usize>, cosmic::iced::Font)>,
}

impl<'a> ShapeKey<'a> {
    fn new(text: &'a str, syntax: &[SyntaxSpan]) -> Self {
        Self::with_font(text, syntax, cosmic::iced::Font::MONOSPACE)
    }

    fn with_font(text: &'a str, syntax: &[SyntaxSpan], font: cosmic::iced::Font) -> Self {
        let mut fonts: Vec<(Range<usize>, cosmic::iced::Font)> = Vec::new();
        let mut append = |range: Range<usize>, font| {
            if range.is_empty() {
                return;
            }
            if let Some((previous, previous_font)) = fonts.last_mut()
                && *previous_font == font
                && previous.end == range.start
            {
                previous.end = range.end;
            } else {
                fonts.push((range, font));
            }
        };
        let mut cursor = 0;
        for span in syntax {
            let start = (span.start as usize).min(text.len());
            let end = (span.end as usize).min(text.len());
            if cursor < start {
                append(cursor..start, font);
            }
            let styled = span
                .font()
                .map(|mut styled| {
                    styled.family = font.family;
                    styled
                })
                .unwrap_or(font);
            append(start..end, styled);
            cursor = end;
        }
        if cursor < text.len() {
            append(cursor..text.len(), font);
        }
        Self { text, fonts }
    }

    fn width(&self, words: &mut HashMap<ShapeKey<'a>, f32>) -> f32 {
        if !self.text.is_ascii() || self.text.bytes().any(|byte| byte.is_ascii_control()) {
            return self.native_width();
        }
        let mut width = 0.0;
        let mut start = 0;
        for (end, _) in unicode_linebreak::linebreaks(self.text) {
            // Only split at actual native whitespace breaks. Punctuation
            // ligature probes and forbidden breaks stay inside each segment.
            if end != self.text.len() && self.text.as_bytes()[end - 1] != b' ' {
                continue;
            }
            let fonts = self
                .fonts
                .iter()
                .filter_map(|(range, font)| {
                    let a = range.start.max(start);
                    let b = range.end.min(end);
                    (a < b).then(|| (a - start..b - start, *font))
                })
                .collect();
            let key = ShapeKey {
                text: &self.text[start..end],
                fonts,
            };
            let measured = if let Some(width) = words.get(&key) {
                *width
            } else {
                let width = key.native_width();
                if words.len() < 32768 {
                    words.insert(key, width);
                }
                width
            };
            width += measured;
            start = end;
        }
        width
    }

    fn native_width(&self) -> f32 {
        use cosmic::iced::Font;
        if self.fonts.len() <= 1 {
            return plain_line_width(
                self.text,
                self.fonts
                    .first()
                    .map_or(Font::MONOSPACE, |(_, font)| *font),
            );
        }
        let spans: Vec<_> = self
            .fonts
            .iter()
            .map(|(range, font)| {
                cosmic::iced::widget::text::Span::new(&self.text[range.clone()]).font(*font)
            })
            .collect();
        shaped_spans_width(&spans)
    }
}

fn plain_line_width(line: &str, font: cosmic::iced::Font) -> f32 {
    use cosmic::iced::{
        Pixels, Size,
        advanced::text::{self, Paragraph as _},
    };
    let paragraph = <cosmic::Renderer as text::Renderer>::Paragraph::with_text(text::Text {
        content: line,
        bounds: Size::new(f32::INFINITY, f32::INFINITY),
        size: Pixels(DIFF_FONT_SIZE),
        line_height: text::LineHeight::Absolute(Pixels(DIFF_LINE_HEIGHT)),
        font,
        align_x: text::Alignment::Left,
        align_y: cosmic::iced::alignment::Vertical::Top,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::None,
        ellipsize: text::Ellipsize::None,
    });
    paragraph.min_bounds().width
}

/// Shape native fallbacks under one lock per small batch. The bounded batch
/// releases the renderer's global font lock between groups of at most 64 rows.
fn native_key_width(
    key: &ShapeKey<'_>,
    fonts: &mut cosmic::iced::advanced::graphics::text::cosmic_text::FontSystem,
) -> f32 {
    use cosmic::iced::advanced::graphics::text::{self, cosmic_text};
    #[cfg(test)]
    WORK_COUNTS.with(|counts| {
        let (highlight, shape) = counts.get();
        counts.set((highlight, shape + 1));
    });
    let mut buffer = cosmic_text::Buffer::new(
        fonts,
        cosmic_text::Metrics::new(DIFF_FONT_SIZE, DIFF_LINE_HEIGHT),
    );
    buffer.set_size(Some(f32::INFINITY), Some(f32::INFINITY));
    buffer.set_wrap(cosmic_text::Wrap::None);
    buffer.set_rich_text(
        key.fonts.iter().enumerate().map(|(index, (range, font))| {
            (
                &key.text[range.clone()],
                text::to_attributes(*font).metadata(index),
            )
        }),
        &text::to_attributes(cosmic::iced::Font::MONOSPACE),
        cosmic_text::Shaping::Advanced,
        Some(cosmic_text::Align::Left),
    );
    buffer.shape_until_scroll(fonts, false);
    text::align(
        &mut buffer,
        fonts,
        cosmic::iced::advanced::text::Alignment::Left,
    )
    .width
}

fn ascii_cell(
    font: cosmic::iced::Font,
    fonts: &mut cosmic::iced::advanced::graphics::text::cosmic_text::FontSystem,
) -> Option<f32> {
    let measure = |text: &str, fonts: &mut _| {
        native_key_width(
            &ShapeKey {
                text,
                fonts: vec![(0..text.len(), font)],
            },
            fonts,
        )
    };
    let cell = measure("0000000000", fonts) / 10.0;
    // Font::MONOSPACE is resolved by the platform. Verify fixed advances for
    // narrow/wide ASCII and common programming ligatures before arithmetic.
    ["iiiiiiiiii", "WWWWWWWWWW", "-> <= != === ffi ffl", " !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~"]
        .iter().all(|probe| (measure(probe, fonts) - probe.len() as f32 * cell).abs() < 0.02)
        .then_some(cell)
}

fn arithmetic_width(
    key: &ShapeKey<'_>,
    cells: &HashMap<cosmic::iced::Font, Option<f32>>,
) -> Option<f32> {
    if !key.text.is_ascii() || key.text.bytes().any(|byte| byte.is_ascii_control()) {
        return None;
    }
    key.fonts.iter().try_fold(0.0, |width, (range, font)| {
        Some(width + range.len() as f32 * cells.get(font).copied().flatten()?)
    })
}

fn measure_diff(model: &mut DiffModel, cancellation: &CancellationToken) -> Result<(), Cancelled> {
    measure_diff_with_font(model, cosmic::iced::Font::MONOSPACE, cancellation)
}

/// Re-measure retained rows on a CPU worker without repeating source/highlight work.
pub fn measure_diff_for_font(
    model: &mut DiffModel,
    font: cosmic::iced::Font,
    cancellation: &CancellationToken,
) -> Option<()> {
    measure_diff_with_font(model, font, cancellation).ok()
}

fn measure_diff_with_font(
    model: &mut DiffModel,
    font: cosmic::iced::Font,
    cancellation: &CancellationToken,
) -> Result<(), Cancelled> {
    use cosmic::iced::advanced::graphics::text;
    let palettes = if model
        .syntax
        .old
        .iter()
        .chain(model.syntax.new.iter())
        .all(Vec::is_empty)
    {
        1
    } else {
        2
    };
    let mut widths = [0.0_f32; 2];
    let mut measured = HashMap::new();
    let mut cells = HashMap::new();
    for rows in model.split_rows.chunks(64) {
        check_cancelled(cancellation)?;
        let mut pending = Vec::new();
        for row in rows {
            for (old, line, source) in [
                (true, row.old_line, row.old_text.as_deref()),
                (false, row.new_line, row.new_text.as_deref()),
            ] {
                if let Some(source) = source {
                    for (palette, width) in widths.iter_mut().enumerate().take(palettes) {
                        let key = ShapeKey::with_font(
                            source,
                            model.syntax.line(old, line, palette == 1),
                            font,
                        );
                        if let Some(cached) = measured.get(&key) {
                            *width = width.max(*cached);
                        } else if let Some(arithmetic) = arithmetic_width(&key, &cells) {
                            *width = width.max(arithmetic);
                        } else {
                            pending.push((palette, key));
                        }
                    }
                }
            }
        }
        if pending.is_empty() {
            continue;
        }
        let mut system = text::font_system().write().expect("Write font system");
        for (palette, key) in pending {
            check_cancelled(cancellation)?;
            for (_, font) in &key.fonts {
                cells
                    .entry(*font)
                    .or_insert_with(|| ascii_cell(*font, system.raw()));
            }
            let width = if let Some(cached) = measured.get(&key) {
                *cached
            } else if let Some(arithmetic) = arithmetic_width(&key, &cells) {
                arithmetic
            } else {
                native_key_width(&key, system.raw())
            };
            widths[palette] = widths[palette].max(width);
            if measured.len() < 32768 {
                measured.insert(key, width);
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

    /// Runs of unchanged rows in `mode`, in file order. See
    /// [`DiffModel::unchanged`].
    pub fn unchanged_runs(&self, mode: DiffMode) -> &[Range<usize>] {
        &self.unchanged[usize::from(mode == DiffMode::Split)]
    }

    pub fn row_count(&self, mode: DiffMode) -> usize {
        match mode {
            DiffMode::Inline => self.inline_rows.len(),
            DiffMode::Split => self.split_rows.len(),
        }
    }

    /// New-side source line nearest a logical row, including deleted rows.
    pub fn new_line_at_row(&self, mode: DiffMode, row: usize) -> Option<u32> {
        fn nearest(
            lines: impl DoubleEndedIterator<Item = Option<u32>> + Clone,
            row: usize,
        ) -> Option<u32> {
            lines
                .clone()
                .skip(row)
                .flatten()
                .next()
                .or_else(|| lines.take(row.saturating_add(1)).flatten().last())
        }
        match mode {
            DiffMode::Inline => nearest(self.inline_rows.iter().map(|row| row.new_line), row),
            DiffMode::Split => nearest(self.split_rows.iter().map(|row| row.new_line), row),
        }
    }

    pub fn row_at_new_line(&self, mode: DiffMode, line: u32) -> Option<usize> {
        match mode {
            DiffMode::Inline => self
                .inline_rows
                .iter()
                .position(|row| row.new_line.is_some_and(|n| n >= line)),
            DiffMode::Split => self
                .split_rows
                .iter()
                .position(|row| row.new_line.is_some_and(|n| n >= line)),
        }
        .or_else(|| self.row_count(mode).checked_sub(1))
    }

    /// The syntax theme this model's spans were coloured with, so a cached
    /// model can be rebuilt when the user changes the setting.
    #[must_use]
    pub fn syntax_theme(&self) -> crate::settings::EditorSyntaxTheme {
        self.syntax.theme
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
    /// `main (a1b2c3d4)`/`HEAD (e5f6a7b8)`, or a pair of commit ids —
    /// instead of one wording for every basis.
    Text {
        model: Arc<DiffModel>,
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
        theme: crate::settings::EditorSyntaxTheme,
        cancellation: &CancellationToken,
    ) -> Self {
        Self::from_content_with_options(content, path, theme, false, cancellation)
    }

    pub fn from_content_with_options(
        content: DiffContent,
        path: &Path,
        theme: crate::settings::EditorSyntaxTheme,
        ignore_whitespace: bool,
        cancellation: &CancellationToken,
    ) -> Self {
        Self::from_content_with_options_and_font(
            content,
            path,
            theme,
            ignore_whitespace,
            cosmic::iced::Font::MONOSPACE,
            cancellation,
        )
    }

    pub fn from_content_with_options_and_font(
        content: DiffContent,
        path: &Path,
        theme: crate::settings::EditorSyntaxTheme,
        ignore_whitespace: bool,
        font: cosmic::iced::Font,
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
                source_head,
                modes,
            } => {
                if !old.is_diffable() || !new.is_diffable() {
                    return Self::NoText(DiffContent::Comparison {
                        old,
                        new,
                        old_label,
                        new_label,
                        source_head,
                        modes,
                    });
                }
                let present = [
                    !matches!(old, crate::repository::DiffSide::Absent),
                    !matches!(new, crate::repository::DiffSide::Absent),
                ];
                let mut model = match build_document_diff_sources_with_font(
                    Arc::from(old.text().unwrap_or_default()),
                    Arc::from(new.text().unwrap_or_default()),
                    path,
                    theme,
                    ignore_whitespace,
                    font,
                    cancellation,
                ) {
                    Some(model) => model,
                    None => return Self::Error("repository operation cancelled".into()),
                };
                model.present = present;
                model.source_head = source_head;
                model.modes = modes;
                Self::Text {
                    model: Arc::new(model),
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
    theme: crate::settings::EditorSyntaxTheme,
    cancellation: &CancellationToken,
) -> RenderedDiff {
    load_repository_diff_with_options(
        worktree,
        relative_path,
        basis,
        previous_path,
        theme,
        false,
        cancellation,
    )
}

pub fn load_repository_diff_with_options(
    worktree: &Path,
    relative_path: &Path,
    basis: &DiffBasis,
    previous_path: Option<&Path>,
    theme: crate::settings::EditorSyntaxTheme,
    ignore_whitespace: bool,
    cancellation: &CancellationToken,
) -> RenderedDiff {
    load_repository_diff_with_options_and_font(
        worktree,
        relative_path,
        basis,
        previous_path,
        theme,
        ignore_whitespace,
        cosmic::iced::Font::MONOSPACE,
        cancellation,
    )
}

pub fn load_repository_diff_with_options_and_font(
    worktree: &Path,
    relative_path: &Path,
    basis: &DiffBasis,
    previous_path: Option<&Path>,
    theme: crate::settings::EditorSyntaxTheme,
    ignore_whitespace: bool,
    font: cosmic::iced::Font,
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
        Ok(content) => RenderedDiff::from_content_with_options_and_font(
            content,
            relative_path,
            theme,
            ignore_whitespace,
            font,
            cancellation,
        ),
        Err(error) => RenderedDiff::Error(error.to_string()),
    }
}

/// Whitespace comparison keys retain one token for every source line, even
/// when an unterminated final line becomes empty. Pool normalized bytes rather
/// than allocate a String per line; ranges preserve empty tokens explicitly.
struct NormalizedDiffLines {
    text: String,
    lines: Vec<Range<usize>>,
}

impl NormalizedDiffLines {
    fn new(source: &str, cancellation: &CancellationToken) -> Result<Self, Cancelled> {
        let mut result = Self {
            text: String::with_capacity(source.len()),
            lines: Vec::new(),
        };
        for line in source.split_inclusive('\n') {
            check_cancelled(cancellation)?;
            let start = result.text.len();
            result
                .text
                .extend(line.chars().filter(|ch| *ch == '\n' || !ch.is_whitespace()));
            result.lines.push(start..result.text.len());
        }
        Ok(result)
    }

    fn input<'a>(&'a self, after: &'a Self) -> InternedInput<&'a str> {
        let mut input = InternedInput::default();
        input.update_before(self.lines.iter().map(|range| &self.text[range.clone()]));
        input.update_after(after.lines.iter().map(|range| &after.text[range.clone()]));
        input
    }
}

/// The editor gutter needs only new-side changed line numbers. This path
/// creates neither display rows nor syntax/width work, including for large files.
pub fn build_line_changes(
    before: &str,
    after: &str,
    ignore_whitespace: bool,
    cancellation: &CancellationToken,
) -> Option<Vec<usize>> {
    check_cancelled(cancellation).ok()?;
    if before == after {
        return Some(Vec::new());
    }
    let normalized = if ignore_whitespace {
        Some((
            NormalizedDiffLines::new(before, cancellation).ok()?,
            NormalizedDiffLines::new(after, cancellation).ok()?,
        ))
    } else {
        None
    };
    let input = match &normalized {
        Some((before, after)) => before.input(after),
        None => InternedInput::new(before, after),
    };
    check_cancelled(cancellation).ok()?;
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    check_cancelled(cancellation).ok()?;
    diff.postprocess_lines(&input);
    let mut additions = Vec::new();
    for hunk in diff.hunks() {
        check_cancelled(cancellation).ok()?;
        additions.extend(hunk.after.start as usize..hunk.after.end as usize);
    }
    Some(additions)
}

pub fn build_diff(before: &str, after: &str) -> DiffModel {
    let cancellation = CancellationToken::new();
    let before = Arc::from(before);
    let mut model =
        build_diff_rows(&before, after, &cancellation).expect("local token is not cancelled");
    measure_diff(&mut model, &cancellation).expect("local token is not cancelled");
    model
}

fn build_diff_rows(
    before: &Arc<str>,
    after: &str,
    cancellation: &CancellationToken,
) -> Result<DiffModel, Cancelled> {
    build_diff_rows_with_options(before, after, false, cancellation)
}

fn build_diff_rows_with_options(
    before: &Arc<str>,
    after: &str,
    ignore_whitespace: bool,
    cancellation: &CancellationToken,
) -> Result<DiffModel, Cancelled> {
    let after_source = if before.as_ref() == after {
        Arc::clone(before)
    } else {
        Arc::<str>::from(after)
    };
    build_diff_rows_from_sources(before, &after_source, ignore_whitespace, cancellation)
}

fn build_diff_rows_from_sources(
    before: &Arc<str>,
    after_source: &Arc<str>,
    ignore_whitespace: bool,
    cancellation: &CancellationToken,
) -> Result<DiffModel, Cancelled> {
    check_cancelled(cancellation)?;
    let after_source = if before.as_ref() == after_source.as_ref() {
        before
    } else {
        after_source
    };
    let after = after_source.as_ref();
    if before.as_ref() == after {
        // Keep every context row, but skip interning and line diffing entirely.
        // Display lines borrow the source; output rows are the only text copies.
        let lines = before
            .split_inclusive('\n')
            .map(|line| {
                check_cancelled(cancellation)?;
                Ok(display_line(line))
            })
            .collect::<Result<Vec<_>, Cancelled>>()?;
        let mut model = DiffModel {
            line_counts: [lines.len(); 2],
            present: [true; 2],
            source_head: None,
            modes: [None; 2],
            before: Arc::clone(before),
            after: Arc::clone(after_source),
            inline_rows: Vec::with_capacity(lines.len()),
            split_rows: Vec::with_capacity(lines.len()),
            inline_change_starts: Vec::new(),
            split_change_starts: Vec::new(),
            unchanged: Default::default(),
            additions: 0,
            deletions: 0,
            max_line_width: [0; 2],
            syntax: DiffSyntax::default(),
        };
        let mut old_cursor = 0;
        let mut new_cursor = 0;
        append_context(
            before,
            after_source,
            &lines,
            &lines,
            &mut old_cursor,
            lines.len(),
            &mut new_cursor,
            lines.len(),
            &mut model.inline_rows,
            &mut model.split_rows,
            &mut model.unchanged,
            cancellation,
        )?;
        check_cancelled(cancellation)?;
        return Ok(model);
    }
    let normalized = if ignore_whitespace {
        Some((
            NormalizedDiffLines::new(before, cancellation)?,
            NormalizedDiffLines::new(after, cancellation)?,
        ))
    } else {
        None
    };
    let input = match &normalized {
        Some((before, after)) => before.input(after),
        None => InternedInput::new(before.as_ref(), after),
    };
    check_cancelled(cancellation)?;
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    check_cancelled(cancellation)?;
    diff.postprocess_lines(&input);
    check_cancelled(cancellation)?;

    let before_lines = before
        .split_inclusive('\n')
        .map(display_line)
        .collect::<Vec<_>>();
    let after_lines = after
        .split_inclusive('\n')
        .map(display_line)
        .collect::<Vec<_>>();
    let mut inline_rows = Vec::new();
    let mut split_rows = Vec::new();
    let mut inline_change_starts = Vec::new();
    let mut split_change_starts = Vec::new();
    let mut unchanged = <[Vec<Range<usize>>; 2]>::default();
    let mut old_cursor = 0usize;
    let mut new_cursor = 0usize;
    let mut before_endings = HashMap::<&str, u8>::new();
    let mut after_endings = HashMap::<&str, u8>::new();
    let mut intraline_budget = 4_000_000usize;
    let mut pairing_budget = 1_000_000usize;

    for hunk in diff.hunks() {
        check_cancelled(cancellation)?;
        append_context(
            before,
            after_source,
            &before_lines,
            &after_lines,
            &mut old_cursor,
            hunk.before.start as usize,
            &mut new_cursor,
            hunk.after.start as usize,
            &mut inline_rows,
            &mut split_rows,
            &mut unchanged,
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
            *before_endings.entry(line.text).or_default() |= line.ending.bit();
        }
        for line in &after_lines[new_start..new_end] {
            check_cancelled(cancellation)?;
            *after_endings.entry(line.text).or_default() |= line.ending.bit();
        }
        for (offset, line) in before_lines[old_start..old_end].iter().enumerate() {
            check_cancelled(cancellation)?;
            let ending_changed = line.ending_differs(&after_endings);
            inline_rows.push(InlineDiffRow {
                old_line: Some((old_start + offset + 1) as u32),
                new_line: None,
                text: if ending_changed {
                    line.changed_text().into()
                } else {
                    SharedLine::slice(before, line.text)
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
                    line.changed_text().into()
                } else {
                    SharedLine::slice(after_source, line.text)
                },
                kind: DiffRowKind::Addition,
                changed: Vec::new(),
            });
        }

        let matches = replacement_matches(
            &before_lines[old_start..old_end],
            &after_lines[new_start..new_end],
            &mut pairing_budget,
            cancellation,
        )?;
        let mut matches = matches.into_iter().peekable();
        let mut old_offset = 0;
        let mut new_offset = 0;
        let old_len = old_end - old_start;
        let new_len = new_end - new_start;
        let inline_start = *inline_change_starts.last().expect("current hunk");
        while old_offset < old_len || new_offset < new_len {
            check_cancelled(cancellation)?;
            let (next_old, next_new) = matches.peek().copied().unwrap_or((old_len, new_len));
            let similar = old_offset == next_old && new_offset == next_new;
            let old_index = (similar || old_offset < next_old).then_some(old_offset);
            let new_index = (similar || new_offset < next_new).then_some(new_offset);
            if similar {
                matches.next();
            }
            // Reuse the exact inline display text, including ending markers.
            let old_display = old_index.map(|index| inline_rows[inline_start + index].text.clone());
            let new_display =
                new_index.map(|index| inline_rows[inline_start + old_len + index].text.clone());
            let old_str = old_display.as_deref().unwrap_or("");
            let new_str = new_display.as_deref().unwrap_or("");
            let (old_changed, new_changed) = if similar {
                intraline_changes(old_str, new_str, &mut intraline_budget)
            } else {
                // Adjacency is not evidence of a corresponding edited line.
                (whole_line_change(old_str), whole_line_change(new_str))
            };
            if let Some(index) = old_index {
                inline_rows[inline_start + index]
                    .changed
                    .clone_from(&old_changed);
                old_offset += 1;
            }
            if let Some(index) = new_index {
                inline_rows[inline_start + old_len + index]
                    .changed
                    .clone_from(&new_changed);
                new_offset += 1;
            }
            split_rows.push(SplitDiffRow {
                old_line: old_index.map(|index| (old_start + index + 1) as u32),
                new_line: new_index.map(|index| (new_start + index + 1) as u32),
                old_text: old_display,
                new_text: new_display,
                old_changed,
                new_changed,
                kind: match (old_index, new_index) {
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
        before,
        after_source,
        &before_lines,
        &after_lines,
        &mut old_cursor,
        before_lines.len(),
        &mut new_cursor,
        after_lines.len(),
        &mut inline_rows,
        &mut split_rows,
        &mut unchanged,
        cancellation,
    )?;

    Ok(DiffModel {
        line_counts: [before_lines.len(), after_lines.len()],
        present: [true; 2],
        source_head: None,
        modes: [None; 2],
        before: Arc::clone(before),
        after: Arc::clone(after_source),
        inline_rows,
        split_rows,
        inline_change_starts,
        split_change_starts,
        unchanged,
        additions: diff.count_additions(),
        deletions: diff.count_removals(),
        max_line_width: [0; 2],
        syntax: DiffSyntax::default(),
    })
}

pub(crate) const DIFF_FONT_SIZE: f32 = 14.0;
pub(crate) const DIFF_LINE_HEIGHT: f32 = 21.0;

/// Use the same native paragraph, advanced shaping, font and no-wrap policy as
/// Rich text. Character counts are not pixel advances: combining marks, tabs,
/// fallback glyphs and ligatures must be measured by the renderer.
#[cfg(test)]
fn shaped_line_width(line: &str, changed: &[Range<usize>], syntax: &[SyntaxSpan]) -> f32 {
    shaped_spans_width(&line_spans(
        line,
        changed,
        syntax,
        cosmic::iced::Color::TRANSPARENT,
    ))
}

fn shaped_spans_width(spans: &[cosmic::iced::widget::text::Span<'_>]) -> f32 {
    use cosmic::iced::{
        Font, Pixels, Size,
        advanced::text::{self, Paragraph as _},
    };

    let paragraph = <cosmic::Renderer as text::Renderer>::Paragraph::with_spans(text::Text {
        content: spans,
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
            .is_some_and(|span| span.end as usize <= cursor)
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
            .min(style.map_or(text.len(), |span| next_boundary(&span.range())))
            .min(text.len());
        let mut span = Span::new(&text[cursor..end]);
        if change.is_some_and(|range| range.contains(&cursor)) {
            span = span.background(background);
        }
        if let Some(style) = style.filter(|span| span.range().contains(&cursor)) {
            if let Some(color) = style.color {
                span = span.color(cosmic::iced::Color::from_rgba8(
                    color[0],
                    color[1],
                    color[2],
                    color[3] as f32 / 255.0,
                ));
            }
            if let Some(font) = style.font() {
                span = span.font(font);
            }
        }
        spans.push(span);
        cursor = end;
    }
    spans
}

/// Find corresponding changed lines without using indentation or punctuation
/// as evidence. A bounded weighted sequence alignment keeps matches monotonic,
/// allowing leading insertions/deletions without shifting every intraline pair.
/// Oversized or exhausted hunks conservatively remain whole-line changes.
fn replacement_matches(
    before: &[DisplayLine<'_>],
    after: &[DisplayLine<'_>],
    budget: &mut usize,
    cancellation: &CancellationToken,
) -> Result<Vec<(usize, usize)>, Cancelled> {
    // Stable trim-equal anchors partition a reindent into tiny replacement
    // windows. This retains the matrix bound without dropping word highlights
    // on the one substantive edit in a very large reindented block.
    let mut anchors = InternedInput::default();
    anchors.update_before(before.iter().map(|line| line.text.trim()));
    anchors.update_after(after.iter().map(|line| line.text.trim()));
    let alignment = Diff::compute(Algorithm::Histogram, &anchors);
    let mut matches = Vec::new();
    let (mut old, mut new) = (0usize, 0usize);
    for hunk in alignment.hunks() {
        check_cancelled(cancellation)?;
        while old < hunk.before.start as usize && new < hunk.after.start as usize {
            matches.push((old, new));
            old += 1;
            new += 1;
        }
        let old_end = hunk.before.end as usize;
        let new_end = hunk.after.end as usize;
        matches.extend(
            weighted_replacement_matches(
                &before[old..old_end],
                &after[new..new_end],
                budget,
                cancellation,
            )?
            .into_iter()
            .map(|(a, b)| (old + a, new + b)),
        );
        old = old_end;
        new = new_end;
    }
    while old < before.len() && new < after.len() {
        check_cancelled(cancellation)?;
        matches.push((old, new));
        old += 1;
        new += 1;
    }
    Ok(matches)
}

fn weighted_replacement_matches(
    before: &[DisplayLine<'_>],
    after: &[DisplayLine<'_>],
    budget: &mut usize,
    cancellation: &CancellationToken,
) -> Result<Vec<(usize, usize)>, Cancelled> {
    let columns = after.len() + 1;
    let cells = (before.len() + 1).checked_mul(columns);
    if before.is_empty() || after.is_empty() || cells.is_none_or(|cells| cells > 16_384) {
        return Ok(Vec::new());
    }
    let mut bytes = 0usize;
    for line in before.iter().chain(after) {
        check_cancelled(cancellation)?;
        bytes = bytes.saturating_add(line.text.len());
        if line.text.len() > 8192 || bytes > *budget {
            return Ok(Vec::new());
        }
    }
    *budget -= bytes;
    let mut words = Vec::with_capacity(before.len() + after.len());
    for line in before.iter().chain(after) {
        check_cancelled(cancellation)?;
        let mut line_words = line
            .text
            .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
            .filter(|word| word.chars().any(char::is_alphanumeric))
            .collect::<Vec<_>>();
        line_words.sort_unstable();
        words.push(line_words);
    }
    let (old_words, new_words) = words.split_at(before.len());
    let cells = cells.expect("bounded matrix");
    let mut scores = vec![0u32; cells];
    let mut directions = vec![0u8; cells];
    for (old, old_words) in old_words.iter().enumerate() {
        for (new, new_words) in new_words.iter().enumerate() {
            check_cancelled(cancellation)?;
            // Charge bytes, not only token counts: long common identifier
            // prefixes also cost work in lexical comparisons.
            let work = old_words
                .iter()
                .chain(new_words)
                .map(|word| word.len() + 1)
                .sum::<usize>()
                + 1;
            if work > *budget {
                return Ok(Vec::new());
            }
            *budget -= work;
            let similarity = substantive_similarity(old_words, new_words);
            let cell = (old + 1) * columns + new + 1;
            let above = scores[cell - columns];
            let left = scores[cell - 1];
            let paired = scores[cell - columns - 1] + similarity;
            if similarity != 0 && paired >= above.max(left) {
                scores[cell] = paired;
                directions[cell] = 1;
            } else if above >= left {
                scores[cell] = above;
                directions[cell] = 2;
            } else {
                scores[cell] = left;
                directions[cell] = 3;
            }
        }
    }
    let mut matches = Vec::new();
    let mut old = before.len();
    let mut new = after.len();
    while old != 0 && new != 0 {
        check_cancelled(cancellation)?;
        match directions[old * columns + new] {
            1 => {
                old -= 1;
                new -= 1;
                matches.push((old, new));
            }
            2 => old -= 1,
            _ => new -= 1,
        }
    }
    matches.reverse();
    Ok(matches)
}

/// Dice overlap of substantive bytes, counting repeated words only as often as
/// they occur on both sides. A shared keyword alone must not align whole blocks.
fn substantive_similarity(before: &[&str], after: &[&str]) -> u32 {
    let total: usize = before.iter().chain(after).map(|word| word.len()).sum();
    if total == 0 {
        return 0;
    }
    let mut shared = 0;
    let mut old = 0;
    let mut new = 0;
    while old < before.len() && new < after.len() {
        match before[old].cmp(after[new]) {
            std::cmp::Ordering::Less => old += 1,
            std::cmp::Ordering::Greater => new += 1,
            std::cmp::Ordering::Equal => {
                shared += before[old].len();
                old += 1;
                new += 1;
            }
        }
    }
    if shared * 10 < total * 3 {
        0
    } else {
        (shared * 2000 / total) as u32
    }
}

fn whole_line_change(text: &str) -> Vec<Range<usize>> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![0..text.len()]
    }
}

/// Word/whitespace/punctuation tokens preserve the original UTF-8 slices. The
/// byte, token-product and whole-file budgets bound pathological replacement
/// hunks; when exhausted the actual entire changed line remains highlighted.
fn intraline_changes(
    before: &str,
    after: &str,
    budget: &mut usize,
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let whole = whole_line_change;
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
    before_source: &Arc<str>,
    after_source: &Arc<str>,
    before: &[DisplayLine<'_>],
    after: &[DisplayLine<'_>],
    old_cursor: &mut usize,
    old_end: usize,
    new_cursor: &mut usize,
    new_end: usize,
    inline: &mut Vec<InlineDiffRow>,
    split: &mut Vec<SplitDiffRow>,
    unchanged: &mut [Vec<Range<usize>>; 2],
    cancellation: &CancellationToken,
) -> Result<(), Cancelled> {
    debug_assert_eq!(old_end - *old_cursor, new_end - *new_cursor);
    let starts = [inline.len(), split.len()];
    while *old_cursor < old_end && *new_cursor < new_end {
        check_cancelled(cancellation)?;
        let old_line = *old_cursor + 1;
        let new_line = *new_cursor + 1;
        let text = SharedLine::slice(before_source, before[*old_cursor].text);
        inline.push(InlineDiffRow {
            old_line: Some(old_line as u32),
            new_line: Some(new_line as u32),
            text: SharedLine::slice(after_source, after[*new_cursor].text),
            kind: DiffRowKind::Context,
            changed: Vec::new(),
        });
        split.push(SplitDiffRow {
            old_line: Some(old_line as u32),
            new_line: Some(new_line as u32),
            old_text: Some(text),
            new_text: Some(SharedLine::slice(after_source, after[*new_cursor].text)),
            kind: DiffRowKind::Context,
            old_changed: Vec::new(),
            new_changed: Vec::new(),
        });
        *old_cursor += 1;
        *new_cursor += 1;
    }
    if inline.len() > starts[0] {
        unchanged[0].push(starts[0]..inline.len());
        unchanged[1].push(starts[1]..split.len());
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
struct DisplayLine<'a> {
    text: &'a str,
    ending: LineEnding,
}

impl DisplayLine<'_> {
    fn ending_differs(&self, other: &HashMap<&str, u8>) -> bool {
        other
            .get(self.text)
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

fn display_line(line: &str) -> DisplayLine<'_> {
    if let Some(text) = line.strip_suffix("\r\n") {
        DisplayLine {
            text,
            ending: LineEnding::CrLf,
        }
    } else if let Some(text) = line.strip_suffix('\n') {
        DisplayLine {
            text,
            ending: LineEnding::Lf,
        }
    } else {
        DisplayLine {
            text: line,
            ending: LineEnding::None,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rendered_comparison_retains_reader_revision_and_modes_for_text_and_nontext() {
        let head = Some(crate::repository::GitHeadGuard::Unborn);
        let modes = [None, Some(0o100755)];
        for new in [
            DiffSide::Text {
                text: "script\n".into(),
                bytes: 7,
            },
            DiffSide::Binary { bytes: 7 },
        ] {
            let rendered = RenderedDiff::from_content(
                DiffContent::Comparison {
                    old: DiffSide::Absent,
                    new,
                    old_label: "HEAD".into(),
                    new_label: "Index".into(),
                    source_head: head.clone(),
                    modes,
                },
                Path::new("script.sh"),
                AUTO,
                &CancellationToken::new(),
            );
            match rendered {
                RenderedDiff::Text { model, .. } => {
                    assert_eq!(model.source_head, head);
                    assert_eq!(model.modes, modes);
                    assert_eq!(model.present, [false, true]);
                }
                RenderedDiff::NoText(DiffContent::Comparison {
                    source_head,
                    modes: actual,
                    ..
                }) => {
                    assert_eq!(source_head, head);
                    assert_eq!(actual, modes);
                }
                _ => panic!("comparison should preserve its reader metadata"),
            }
        }
    }

    #[test]
    fn whitespace_normalization_keeps_source_text_and_real_changes() {
        let token = super::CancellationToken::new();
        let before = std::sync::Arc::from("fn main() {\n    let answer = 42;\n}\n");
        let after = "fn main() {\n\tlet answer = 42;  \n}\n";
        let plain = super::build_diff_rows_with_options(&before, after, false, &token).unwrap();
        let ignored = super::build_diff_rows_with_options(&before, after, true, &token).unwrap();
        assert!(!plain.is_empty());
        assert!(ignored.is_empty());
        assert_eq!(ignored.before.as_ref(), before.as_ref());
        assert_eq!(ignored.after.as_ref(), after);
        assert_eq!(
            ignored.split_rows[1].new_text.as_deref(),
            Some("\tlet answer = 42;  ")
        );
        let real =
            super::build_diff_rows_with_options(&before, &after.replace("42", "43"), true, &token)
                .unwrap();
        assert_eq!((real.additions, real.deletions), (1, 1));
        let new_line =
            super::build_diff_rows_with_options(&before, &format!("{after}\n"), true, &token)
                .unwrap();
        assert_eq!(
            new_line.additions, 1,
            "normalization preserves line separators"
        );
    }

    #[test]
    fn whitespace_normalization_retains_unterminated_empty_line_tokens_and_coordinates() {
        let cancel = CancellationToken::new();
        for (before, after, expected_changes) in [
            ("a\n    ", "a\n  x", vec![1]),
            ("a\n  x", "a\n    ", vec![1]),
            ("    ", "x", vec![0]),
            ("x", "    ", vec![0]),
            ("", "    ", vec![0]),
            ("    ", "", Vec::new()),
            ("a\r\n\t  ", "a\r\n  雪", vec![1]),
            ("a\n    ", "a\n\t", Vec::new()),
            ("a\n    ", "a\n", Vec::new()),
            ("a\n", "a\n    ", vec![1]),
        ] {
            let baseline = Arc::<str>::from(before);
            let model = build_diff_rows_with_options(&baseline, after, true, &cancel).unwrap();
            assert_eq!(
                model.line_counts,
                [
                    before.split_inclusive('\n').count(),
                    after.split_inclusive('\n').count()
                ]
            );
            assert_eq!(
                build_line_changes(before, after, true, &cancel).unwrap(),
                expected_changes,
                "{before:?} → {after:?}"
            );
            for (side, source) in [before, after].into_iter().enumerate() {
                let line_count = source.split_inclusive('\n').count();
                let coordinates = model
                    .inline_rows
                    .iter()
                    .filter_map(|row| {
                        if side == 0 {
                            row.old_line
                        } else {
                            row.new_line
                        }
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    coordinates,
                    (1..=line_count as u32).collect::<Vec<_>>(),
                    "{before:?} → {after:?}, side{side}"
                );
            }
            for mode in [DiffMode::Inline, DiffMode::Split] {
                let range = 0..model.row_count(mode);
                if !model.is_empty() {
                    assert_eq!(
                        crate::diff_patch::selected_content(
                            &model,
                            after,
                            mode,
                            range.clone(),
                            false
                        )
                        .as_deref(),
                        Some(after)
                    );
                    assert_eq!(
                        crate::diff_patch::selected_content(&model, after, mode, range, true)
                            .as_deref(),
                        Some(before)
                    );
                }
            }
        }
    }

    #[test]
    fn large_reindent_keeps_word_pairing_for_the_real_edit() {
        let before = (0..180)
            .map(|i| format!("    let value_{i} = {i};\n"))
            .collect::<String>();
        let after = before
            .replace("    ", "\t")
            .replace("value_90 = 90", "value_90 = 900");
        let model = super::build_diff_rows(
            &std::sync::Arc::from(before),
            &after,
            &super::CancellationToken::new(),
        )
        .unwrap();
        let row = model
            .split_rows
            .iter()
            .find(|row| row.new_line == Some(91))
            .unwrap();
        let changed = row
            .new_changed
            .iter()
            .map(|range| &row.new_text.as_ref().unwrap()[range.clone()])
            .collect::<Vec<_>>();
        assert!(
            changed.contains(&"900"),
            "substantive edit must have a word highlight: {changed:?}"
        );
        assert!(
            !row.new_changed
                .contains(&(0..row.new_text.as_ref().unwrap().len()))
        );
    }

    #[test]
    fn diff_source_mapping_survives_deleted_rows_and_both_layouts() {
        let model = super::build_diff("one\ndeleted\ntwo\n", "one\ntwo\n");
        for mode in [super::DiffMode::Inline, super::DiffMode::Split] {
            let second = model.row_at_new_line(mode, 2).unwrap();
            assert_eq!(model.new_line_at_row(mode, second), Some(2));
            assert_eq!(model.new_line_at_row(mode, 1), Some(2));
            assert_eq!(model.new_line_at_row(mode, usize::MAX), Some(2));
        }
    }

    use std::{fs, path::PathBuf, process::Command};

    use tempfile::tempdir;

    use super::*;
    use crate::repository::{DiffSide, Oid, ValidatedRefName};

    /// The default syntax theme, spelled once for the call sites that only
    /// need *a* theme rather than a particular one.
    const AUTO: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;

    #[test]
    fn syntax_job_shares_892_real_parsers_and_releases_grammar_after_last_owner() {
        let cache = SyntaxCache::default();
        let job = cache.acquire();
        let grammar = Arc::downgrade(&job.0);
        let source = "let answer = values.iter().map(|value| value + 1);\n";
        let mut expected = None;
        for _ in 0..892 {
            let lease = cache.acquire();
            assert!(Arc::ptr_eq(&job.0, &lease.0));
            let mut parser = HighlightParser::with_syntaxes("rust", AUTO, lease);
            let spans = parser.line(source, &CancellationToken::new()).unwrap();
            assert!(!spans[0].is_empty());
            if let Some(expected) = &expected {
                assert_eq!(&spans, expected);
            } else {
                expected = Some(spans);
            }
        }
        assert_eq!(cache.loads.load(std::sync::atomic::Ordering::Relaxed), 1);
        let mut streaming = HighlightParser::with_syntaxes("rust", AUTO, cache.acquire());
        streaming
            .line("/* open\n", &CancellationToken::new())
            .unwrap();
        drop(job);
        assert!(
            grammar.upgrade().is_some(),
            "live streaming parser owns its grammar"
        );
        streaming
            .line("still comment */ let x = 1;\n", &CancellationToken::new())
            .unwrap();
        drop(streaming);
        assert!(
            grammar.upgrade().is_none(),
            "no process strong owner remains"
        );
        let next_job = cache.acquire();
        assert_eq!(cache.loads.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert!(next_job.0.find_syntax_by_token("rust").is_some());
    }

    #[test]
    fn concurrent_syntax_leases_share_one_actual_grammar_until_all_parsers_finish() {
        let cache = Arc::new(SyntaxCache::default());
        let (sender, receiver) = std::sync::mpsc::channel();
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let cache = cache.clone();
                let sender = sender.clone();
                std::thread::spawn(move || {
                    let mut parser = HighlightParser::with_syntaxes("rust", AUTO, cache.acquire());
                    let spans = parser
                        .line("fn main() {}\n", &CancellationToken::new())
                        .unwrap();
                    assert!(!spans[0].is_empty());
                    sender.send(parser).unwrap();
                })
            })
            .collect();
        drop(sender);
        let mut parsers = Vec::new();
        for _ in 0..4 {
            parsers.push(
                receiver
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .unwrap(),
            );
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let grammar = Arc::downgrade(&parsers[0].syntaxes.0);
        assert!(
            parsers
                .iter()
                .all(|parser| Arc::ptr_eq(&parsers[0].syntaxes.0, &parser.syntaxes.0))
        );
        assert_eq!(cache.loads.load(std::sync::atomic::Ordering::Relaxed), 1);
        while parsers.len() > 1 {
            parsers.pop();
        }
        assert!(grammar.upgrade().is_some());
        parsers.clear();
        assert!(grammar.upgrade().is_none());
    }

    #[test]
    fn cancelled_and_unwound_syntax_jobs_release_their_actual_grammar() {
        let cache = SyntaxCache::default();
        let job = cache.acquire();
        let grammar = Arc::downgrade(&job.0);
        let mut parser = HighlightParser::with_syntaxes("rust", AUTO, cache.acquire());
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(parser.line("fn cancelled() {}\n", &cancellation).is_err());
        drop(parser);
        drop(job);
        assert!(grammar.upgrade().is_none());

        let job = cache.acquire();
        let grammar = Arc::downgrade(&job.0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut parser = HighlightParser::with_syntaxes("rust", AUTO, job);
            parser
                .line("fn actual() {}\n", &CancellationToken::new())
                .unwrap();
            panic!("fixture job unwind after actual grammar work");
        }));
        assert!(result.is_err());
        assert!(grammar.upgrade().is_none());
    }

    /// The diff surface and the editable buffer must agree for *every* theme
    /// the setting offers, not only the default: they are two independent
    /// highlighters over the same grammar, so the same file would otherwise
    /// show different colours depending on which tab it is open in.
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
            for &theme in crate::settings::EditorSyntaxTheme::ALL {
                let actual =
                    highlight_side(source, Path::new(path), theme, &CancellationToken::new())
                        .unwrap();
                for (palette, dark) in [false, true].into_iter().enumerate() {
                    let mut stream = cosmic::iced::highlighter::Stream::new(&highlighter_settings(
                        Path::new(path),
                        theme,
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
                                .map(|(range, highlight)| {
                                    SyntaxSpan::new(
                                        range,
                                        highlight.color().map(|color| {
                                            [
                                                (color.r * 255.0).round() as u8,
                                                (color.g * 255.0).round() as u8,
                                                (color.b * 255.0).round() as u8,
                                                (color.a * 255.0).round() as u8,
                                            ]
                                        }),
                                        highlight.font(),
                                    )
                                })
                                .collect::<Vec<_>>();
                            stream.commit();
                            Arc::<[SyntaxSpan]>::from(spans)
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(actual[palette], expected, "{path}, {theme:?}, dark={dark}");
                }
            }
        }
    }

    #[test]
    fn cached_measurement_matches_every_native_paragraph_in_both_palettes() {
        let before = "/*\nlet office = \"ffi 雪 e\u{301}\";\n*/\n\tlet repeated = 42;\n\tlet repeated = 42;\n";
        let after = "//\nlet office = \"ffl 雪 e\u{301}\";\n//\n\tlet repeated = 42;\n\tlet repeated = 42;\n";
        let model = build_document_diff(
            before,
            after,
            Path::new("sample.rs"),
            AUTO,
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
    fn immutable_rendered_diff_clones_share_all_model_rows() {
        let original = RenderedDiff::Text {
            model: Arc::new(build_diff("old\nkept\n", "new\nkept\n")),
            old_label: "Old".into(),
            new_label: "New".into(),
        };
        let copied = original.clone();
        let (
            RenderedDiff::Text {
                model: original, ..
            },
            RenderedDiff::Text { model: copied, .. },
        ) = (&original, &copied)
        else {
            unreachable!()
        };
        assert!(Arc::ptr_eq(original, copied));
        assert_eq!(original.inline_rows.as_ptr(), copied.inline_rows.as_ptr());
        assert_eq!(original.split_rows.as_ptr(), copied.split_rows.as_ptr());
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
                source_head: None,
                modes: [None; 2],
            },
            Path::new("sample.rs"),
            AUTO,
            &cancellation,
        );
        assert!(matches!(result, RenderedDiff::Error(_)));
        assert!(
            highlight_side(
                "let value = 1;",
                Path::new("sample.rs"),
                AUTO,
                &cancellation
            )
            .is_err()
        );
        assert!(build_diff_rows(&Arc::from("old\n"), "new\n", &cancellation).is_err());
        assert!(
            build_document_diff(
                "old\n",
                "new\n",
                Path::new("sample.rs"),
                AUTO,
                &cancellation
            )
            .is_none()
        );
        assert!(build_document_diff("", "", Path::new("sample.rs"), AUTO, &cancellation).is_none());
    }

    #[test]
    fn restoring_exact_document_baseline_preserves_unchanged_context() {
        let before = "/* comment\r\n雪 e\u{301}\n*/\r\n\nlast";
        let edited = build_document_diff(
            before,
            "/* changed\n雪 e\u{301}\n*/\n\nlast\n",
            Path::new("sample.rs"),
            AUTO,
            &CancellationToken::new(),
        )
        .unwrap();
        let restored = build_document_diff(
            Arc::clone(&edited.before),
            before,
            Path::new("sample.rs"),
            AUTO,
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(restored.is_empty());
        assert!(restored.inline_change_starts.is_empty());
        assert!(restored.split_change_starts.is_empty());
        let expected = ["/* comment", "雪 e\u{301}", "*/", "", "last"];
        assert_eq!(restored.inline_rows.len(), expected.len());
        assert_eq!(restored.split_rows.len(), expected.len());
        for (index, text) in expected.into_iter().enumerate() {
            let line = Some(index as u32 + 1);
            let inline = &restored.inline_rows[index];
            assert_eq!((inline.old_line, inline.new_line), (line, line));
            assert_eq!(inline.text, text);
            assert_eq!(inline.kind, DiffRowKind::Context);
            assert!(inline.changed.is_empty());
            let split = &restored.split_rows[index];
            assert_eq!((split.old_line, split.new_line), (line, line));
            assert_eq!(split.old_text.as_deref(), Some(text));
            assert_eq!(split.new_text.as_deref(), Some(text));
            assert_eq!(split.kind, DiffRowKind::Context);
            assert!(split.old_changed.is_empty());
            assert!(split.new_changed.is_empty());
        }
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
                highlight_side(source, Path::new(path), AUTO, &CancellationToken::new()).unwrap();
            for lines in palettes {
                let colors = lines
                    .iter()
                    .flat_map(|line| line.iter())
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
            highlight_side(
                source,
                Path::new("kernel.cu"),
                AUTO,
                &CancellationToken::new()
            )
            .unwrap(),
            highlight_side(
                source,
                Path::new("kernel.cpp"),
                AUTO,
                &CancellationToken::new()
            )
            .unwrap(),
            "CUDA's C++ constructs must use the actual bundled C++ grammar",
        );
        assert!(
            highlight_side(
                source,
                Path::new("unknown.unsupported"),
                AUTO,
                &CancellationToken::new()
            )
            .unwrap()[1]
                .iter()
                .flat_map(|line| line.iter())
                .all(|span| span.color.is_none())
        );
    }

    #[test]
    fn diff_syntax_keeps_side_specific_multiline_state_and_change_backgrounds() {
        let before = "/*\nint value = 1;\n*/\nvoid old_kernel() {}\n";
        let after = "// comment\nint value = 1;\n// end\nvoid new_kernel() {}\n";
        let model = build_document_diff(
            before,
            after,
            Path::new("kernel.cu"),
            AUTO,
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
                .find(|span| span.range().contains(&index))
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
                        .any(|style| style.range().contains(&offset) && style.color.is_some())
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
    fn unchanged_runs_are_the_context_rows_of_both_layouts() {
        let before: String = (1..=40).map(|line| format!("line {line}\n")).collect();
        let mut after: Vec<String> = (1..=40).map(|line| format!("line {line}\n")).collect();
        after[9] = "changed 10\n".into();
        after.insert(30, "inserted\n".into());
        let model = build_diff(&before, &after.concat());
        assert_eq!(
            model.unchanged_runs(DiffMode::Inline),
            &[0..9, 11..31, 32..42]
        );
        assert_eq!(
            model.unchanged_runs(DiffMode::Split),
            &[0..9, 10..30, 31..41]
        );
        for mode in [DiffMode::Inline, DiffMode::Split] {
            let runs = model.unchanged_runs(mode);
            for row in 0..model.row_count(mode) {
                let kind = match mode {
                    DiffMode::Inline => model.inline_rows[row].kind,
                    DiffMode::Split => model.split_rows[row].kind,
                };
                assert_eq!(
                    runs.iter().any(|run| run.contains(&row)),
                    kind == DiffRowKind::Context,
                    "{mode:?} row {row}"
                );
            }
        }
        // Run `i` names the same lines in both layouts.
        for (inline, split) in model.unchanged[0].iter().zip(&model.unchanged[1]) {
            assert_eq!(inline.len(), split.len());
            assert_eq!(
                model.inline_rows[inline.start].new_line,
                model.split_rows[split.start].new_line
            );
        }

        let identical = build_diff("a\nb\nc\n", "a\nb\nc\n");
        assert_eq!(identical.unchanged, [vec![0..3], vec![0..3]]);
        assert!(
            build_diff("", "added\n")
                .unchanged
                .iter()
                .all(Vec::is_empty)
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
    fn changed_line_alignment_skips_leading_block_insertions_and_deletions() {
        let shorter = "let value = old;\nlet next = old;\n";
        let longer = "audit();\nlet value = new;\nlet next = new;\n";
        for deleting in [false, true] {
            let model = if deleting {
                build_diff(longer, shorter)
            } else {
                build_diff(shorter, longer)
            };
            let leading = &model.split_rows[0];
            assert_eq!(
                leading.kind,
                if deleting {
                    DiffRowKind::Deletion
                } else {
                    DiffRowKind::Addition
                }
            );
            assert_eq!(
                (leading.old_line, leading.new_line),
                if deleting {
                    (Some(1), None)
                } else {
                    (None, Some(1))
                }
            );
            assert_eq!(model.split_rows.len(), 3);
            for (index, row) in model.split_rows[1..].iter().enumerate() {
                assert_eq!(
                    (row.old_line, row.new_line),
                    if deleting {
                        (Some(index as u32 + 2), Some(index as u32 + 1))
                    } else {
                        (Some(index as u32 + 1), Some(index as u32 + 2))
                    }
                );
                assert_eq!(
                    highlighted(row.old_text.as_deref().unwrap(), &row.old_changed),
                    [if deleting { "new" } else { "old" }]
                );
                assert_eq!(
                    highlighted(row.new_text.as_deref().unwrap(), &row.new_changed),
                    [if deleting { "old" } else { "new" }]
                );
            }
        }
    }

    #[test]
    fn indentation_edits_keep_substantive_line_correspondence() {
        let model = build_diff("\tlet café = 1;\n", "    let café = 1;\n");
        let row = &model.split_rows[0];
        assert_eq!(row.kind, DiffRowKind::Modification);
        assert_eq!(
            highlighted(row.old_text.as_deref().unwrap(), &row.old_changed),
            ["\t"]
        );
        assert_eq!(
            highlighted(row.new_text.as_deref().unwrap(), &row.new_changed),
            ["    "]
        );
    }

    #[test]
    fn unrelated_block_lines_do_not_anchor_on_repeated_spaces_or_punctuation() {
        let model = build_diff(
            "    alpha  ( old );\n    beta  ( stale );\n;;  ,,\n",
            "    gamma  ( fresh );\n    delta  ( modern );\n,,  ;;\n",
        );
        for row in &model.inline_rows {
            assert_eq!(highlighted(&row.text, &row.changed), [row.text.as_str()]);
        }
        for row in &model.split_rows {
            if let Some(text) = &row.old_text {
                assert_eq!(highlighted(text, &row.old_changed), [text.as_str()]);
            }
            if let Some(text) = &row.new_text {
                assert_eq!(highlighted(text, &row.new_changed), [text.as_str()]);
            }
        }
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
                    source_head: None,
                    modes: [None; 2],
                },
                Path::new("sample.txt"),
                AUTO,
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
                AUTO,
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
                AUTO,
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
            AUTO,
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

        let merge_base = merge_base(root.path(), "main", "HEAD");
        let branch = DiffBasis::MergeBaseToHead {
            base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
            head: head.clone(),
            merge_base: merge_base.clone(),
        };
        let compared = text_diff(root.path(), "base.txt", &branch, None);
        // The merge base has `one`; HEAD has `one` and `two`. The staged and
        // working changes are deliberately absent: a committed comparison
        // never reads the index or the worktree.
        assert_eq!((compared.0.additions, compared.0.deletions), (1, 0));
        assert_eq!(compared.0.inline_rows[1].text, "two");
        let expected_old = format!("main ({})", merge_base.short());
        let expected_new = format!("HEAD ({})", head.short());
        assert_eq!(
            (compared.1.as_str(), compared.2.as_str()),
            (expected_old.as_str(), expected_new.as_str())
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
            AUTO,
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
            AUTO,
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
            AUTO,
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
        (
            Arc::try_unwrap(model).expect("fixture owns its complete model"),
            old_label,
            new_label,
        )
    }

    /// The working-tree side of an untracked or replaced path, which the
    /// unstaged basis reports as its new side.
    fn working_side(worktree: &Path, path: &str) -> DiffSide {
        let content = load_repository_diff(
            worktree,
            Path::new(path),
            &DiffBasis::IndexToWorktree,
            None,
            AUTO,
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
    #[test]
    #[ignore = "manual diff pipeline benchmark; set CAGENTS_DIFF_BENCH_PATH to a source file"]
    fn benchmark_large_source_diff() {
        let path = std::env::var_os("CAGENTS_DIFF_BENCH_PATH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app.rs"));
        let text = std::fs::read_to_string(&path).unwrap();
        let before: Arc<str> = Arc::from(text.as_str());
        let after = format!("// inserted header\n{text}");
        let cancellation = CancellationToken::new();
        for iteration in 0..3 {
            let started = std::time::Instant::now();
            let mut model = build_diff_rows(&before, &after, &cancellation).unwrap();
            let rows = started.elapsed();
            model.syntax =
                highlight_comparison(&before, &after, &path, AUTO, &cancellation).unwrap();
            let highlighted = started.elapsed();
            measure_diff(&mut model, &cancellation).unwrap();
            eprintln!(
                "diff bench {iteration}: {} lines; rows={rows:?}, syntax={:?}, measure={:?}, total={:?}",
                text.lines().count(),
                highlighted - rows,
                started.elapsed() - highlighted,
                started.elapsed()
            );
        }
    }
    #[test]
    fn cached_shaping_runs_match_full_rich_text_including_fonts_and_spaces() {
        use cosmic::iced::{Font, font};
        let mut words = HashMap::new();
        let samples = [
            "",
            " ",
            "    ",
            "  leading and trailing  ",
            "( foo",
            "[ foo",
            "\" foo bar\"",
            "if a != b |> c -> d { return foo(); }",
            "a\tb",
            "café = \"日本語\"",
            "let שלום = 2;",
            "combining e\u{301}",
            "ffi => office",
        ];
        for text in samples {
            let split = text
                .char_indices()
                .nth(text.chars().count() / 2)
                .map_or(text.len(), |(index, _)| index);
            for font in [
                None,
                Some(Font {
                    weight: font::Weight::Bold,
                    ..Font::MONOSPACE
                }),
                Some(Font {
                    style: font::Style::Italic,
                    ..Font::MONOSPACE
                }),
            ] {
                let syntax = vec![
                    SyntaxSpan::new(0..split, None, font),
                    SyntaxSpan::new(split..text.len(), None, None),
                ];
                let changed = vec![0..split];
                let native = shaped_line_width(text, &changed, &syntax);
                let cached = ShapeKey::new(text, &syntax).width(&mut words);
                assert!(
                    (native - cached).abs() < 0.02,
                    "{text:?}: native {native}, cached {cached}, font {font:?}"
                );
            }
        }
    }

    #[test]
    fn reused_syntax_preserves_multiline_parser_state_and_line_endings() {
        for (before, after) in [
            (
                "fn main() {\n    run();\n}\n",
                "// header\nfn main() {\n    run();\n}\n",
            ),
            ("let a = 1;\nrun();\n", "/*\nrun();\n"),
            ("/* comment */\nrun();\n", "/* comment\nrun();\n"),
            ("let a = 1;\nrun();\n", "let a = 2;\nrun();\n"),
            ("first\nsecond\nthird", "second\nthird"),
            ("same\r\nlast", "same\nlast\n"),
            ("", "new\n"),
            ("old\n", ""),
            ("same", "same"),
        ] {
            let cancellation = CancellationToken::new();
            let result = highlight_comparison(
                &Arc::from(before),
                after,
                Path::new("sample.rs"),
                AUTO,
                &cancellation,
            )
            .unwrap();
            assert_eq!(
                *result.old,
                highlight_side(before, Path::new("sample.rs"), AUTO, &cancellation).unwrap(),
                "old side {before:?}"
            );
            assert_eq!(
                *result.new,
                highlight_side(after, Path::new("sample.rs"), AUTO, &cancellation).unwrap(),
                "new side {after:?}"
            );
        }
    }
}

#[cfg(test)]
mod performance_receipt {
    use super::*;
    #[test]
    #[ignore = "50,000-line diff measurement; run in an otherwise fresh test process"]
    fn fifty_thousand_line_typing_pause() {
        let before: Arc<str> = (0..50_000).map(|line| format!("fn function_{line:05}() {{ let value = {line}; println!(\"value {{value}}\"); }}\n")).collect::<String>().into();
        let mut after = before.to_string();
        for line in [100, 25_000, 49_900] {
            after = after.replacen(
                &format!("let value = {line};"),
                &format!("let value = {line} + 1;"),
                1,
            );
        }
        let started = std::time::Instant::now();
        let model = build_document_diff(
            Arc::clone(&before),
            &after,
            Path::new("huge.rs"),
            crate::settings::EditorSyntaxTheme::Auto,
            &CancellationToken::new(),
        )
        .unwrap();
        let cold_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = std::time::Instant::now();
        let second = build_document_diff(
            Arc::clone(&before),
            &after,
            Path::new("huge.rs"),
            crate::settings::EditorSyntaxTheme::Auto,
            &CancellationToken::new(),
        )
        .unwrap();
        let warm_ms = started.elapsed().as_secs_f64() * 1000.;
        let resident = std::fs::read_to_string("/proc/self/status").unwrap();
        let memory: Vec<_> = resident
            .lines()
            .filter(|line| line.starts_with("VmRSS:") || line.starts_with("VmHWM:"))
            .collect();
        println!(
            "DIFF_PERF lines=50000 source_bytes={} cold_ms={cold_ms:.3} warm_ms={warm_ms:.3} span_bytes={} inline_row_bytes={} split_row_bytes={} rows={} memory={memory:?}",
            before.len(),
            std::mem::size_of::<SyntaxSpan>(),
            std::mem::size_of::<InlineDiffRow>(),
            std::mem::size_of::<SplitDiffRow>(),
            model.inline_rows.len()
        );
        assert_eq!(model.additions, 3);
        assert_eq!(second.deletions, 3);
        std::hint::black_box((model, second));
    }
}

#[cfg(test)]
mod memory_contracts {
    use super::*;
    const AUTO: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;

    #[test]
    fn rows_reference_two_exact_sources_without_context_text_copies() {
        let before: Arc<str> = Arc::from("fn preserved() {}\r\nlet old = 1;\nlast");
        let after: Arc<str> = Arc::from("fn preserved() {}\r\nlet new = 2;\nlast");
        let model = build_document_diff_sources(
            Arc::clone(&before),
            Arc::clone(&after),
            Path::new("storage-contract.rs"),
            AUTO,
            false,
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&model.before, &before));
        assert!(Arc::ptr_eq(&model.after, &after));
        for row in &model.inline_rows {
            assert!(
                Arc::ptr_eq(&row.text.source, &before) || Arc::ptr_eq(&row.text.source, &after)
            );
        }
        assert!(Arc::ptr_eq(
            &model.split_rows[0].old_text.as_ref().unwrap().source,
            &before
        ));
        assert!(Arc::ptr_eq(
            &model.split_rows[0].new_text.as_ref().unwrap().source,
            &after
        ));
        assert_eq!(
            std::mem::size_of::<SharedLine>(),
            std::mem::size_of::<String>()
        );
        assert_eq!(std::mem::size_of::<SyntaxSpan>(), 16);
    }

    #[test]
    fn gutter_build_does_no_highlighting_or_native_shaping() {
        WORK_COUNTS.with(|counts| counts.set((0, 0)));
        let before = "fn gutter_contract() {\n    old();\n}\n";
        let after = "fn gutter_contract() {\n    new();\n    added();\n}\n";
        assert_eq!(
            build_line_changes(before, after, false, &CancellationToken::new()),
            Some(vec![1, 2])
        );
        assert_eq!(WORK_COUNTS.with(std::cell::Cell::get), (0, 0));
        assert_eq!(
            build_line_changes(
                before,
                &before.replace("    ", "\t"),
                true,
                &CancellationToken::new()
            ),
            Some(vec![])
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(build_line_changes(before, after, false, &cancellation).is_none());
    }

    #[test]
    fn baseline_and_unchanged_line_spans_are_shared_across_edits() {
        let before: Arc<str> = Arc::from(
            "fn immutable_cache_contract() {\n    let first = 1;\n    let second = 2;\n}\n",
        );
        let a = build_document_diff(
            Arc::clone(&before),
            &before.replace("first = 1", "first = 3"),
            Path::new("cached.rs"),
            AUTO,
            &CancellationToken::new(),
        )
        .unwrap();
        WORK_COUNTS.with(|counts| counts.set((0, 0)));
        let b = build_document_diff(
            Arc::clone(&before),
            &before.replace("first = 1", "first = 4"),
            Path::new("cached.rs"),
            AUTO,
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&a.syntax.old, &b.syntax.old));
        assert!(Arc::ptr_eq(&b.syntax.old[0][2], &b.syntax.new[0][2]));
        assert_eq!(
            WORK_COUNTS.with(std::cell::Cell::get).0,
            1,
            "only the changed new-side line is recolored"
        );
        let alternate = build_document_diff(
            Arc::clone(&before),
            &before,
            Path::new("cached.rs"),
            crate::settings::EditorSyntaxTheme::Mocha,
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(!Arc::ptr_eq(&a.syntax.old, &alternate.syntax.old));
        let cache = BASELINES.lock().unwrap();
        assert!(cache.entries.len() <= BASELINE_CACHE_ENTRIES);
        assert!(cache.bytes <= BASELINE_CACHE_BYTES);
    }

    #[test]
    fn nested_grammar_cache_retains_only_exact_source_and_colored_spans() {
        let text: Arc<str> = ("/*\n".repeat(512) + &"*/\n".repeat(512)).into();
        let baseline = baseline_syntax(&text, "rs", AUTO, &CancellationToken::new()).unwrap();
        let spans: usize = baseline
            .syntax
            .iter()
            .flat_map(|lines| lines.iter())
            .map(|line| line.len())
            .sum();
        let payload = std::mem::size_of::<BaselineSyntax>()
            + text.len()
            + 6 * std::mem::size_of::<usize>()
            + baseline.token.len()
            + baseline
                .syntax
                .iter()
                .map(|lines| {
                    lines.capacity() * std::mem::size_of::<Arc<[SyntaxSpan]>>()
                        + lines.len() * 2 * std::mem::size_of::<usize>()
                })
                .sum::<usize>()
            + spans * std::mem::size_of::<SyntaxSpan>();
        assert_eq!(baseline.retained_bytes, payload);
        assert!(
            baseline.retained_bytes < text.len() * 100,
            "no retained quadratic parser checkpoint heap"
        );
    }

    #[test]
    fn same_blob_baseline_builds_once_under_concurrent_requests() {
        let text: Arc<str> = (0..64)
            .map(|line| format!("fn concurrent_cache_{line}() {{}}\n"))
            .collect::<String>()
            .into();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let text = Arc::clone(&text);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    WORK_COUNTS.with(|counts| counts.set((0, 0)));
                    barrier.wait();
                    let baseline =
                        baseline_syntax(&text, "rs", AUTO, &CancellationToken::new()).unwrap();
                    (baseline, WORK_COUNTS.with(std::cell::Cell::get).0)
                })
            })
            .collect();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(results.iter().map(|(_, count)| count).sum::<usize>(), 64);
        assert!(
            results
                .iter()
                .all(|(baseline, _)| Arc::ptr_eq(baseline, &results[0].0))
        );
    }

    #[test]
    fn waiting_baseline_can_cancel_while_unrelated_blob_completes() {
        let text: Arc<str> = Arc::from("fn held_cache_miss() {}\n");
        let flight = Arc::new(BaselineFlight {
            text: Arc::clone(&text),
            hash: blake3::hash(text.as_bytes()),
            token: "rs".into(),
            theme: AUTO,
            finished: Mutex::new(false),
            ready: Condvar::new(),
        });
        BASELINES.lock().unwrap().pending.push(Arc::clone(&flight));
        let builder = BaselineBuilder(flight);
        let cancellation = CancellationToken::new();
        let worker_cancel = cancellation.clone();
        let worker =
            std::thread::spawn(move || baseline_syntax(&text, "rs", AUTO, &worker_cancel).is_err());
        let other: Arc<str> = Arc::from("fn unrelated_cache_miss() {}\n");
        assert!(baseline_syntax(&other, "rs", AUTO, &CancellationToken::new()).is_ok());
        cancellation.cancel();
        assert!(worker.join().unwrap());
        drop(builder);
    }

    #[test]
    fn compact_style_rendering_snapshot_preserves_colors_fonts_and_overlays() {
        use cosmic::iced::{Color, Font, font};
        let bold = Font {
            weight: font::Weight::Bold,
            ..Font::MONOSPACE
        };
        let syntax = [
            SyntaxSpan::new(0..3, Some([12, 34, 56, 255]), Some(bold)),
            SyntaxSpan::new(3..5, Some([200, 100, 50, 128]), None),
        ];
        let spans = line_spans("let x", &[4..5], &syntax, Color::from_rgb8(1, 2, 3));
        let text: Vec<_> = spans.iter().map(|span| span.text.as_ref()).collect();
        assert_eq!(text, vec!["let", " ", "x"]);
        assert_eq!(spans[0].font, Some(bold));
        assert_eq!(spans[1].font, None);
        assert_eq!(spans[0].color, Some(Color::from_rgba8(12, 34, 56, 1.0)));
        assert_eq!(
            spans[2].color,
            Some(Color::from_rgba8(200, 100, 50, 128.0 / 255.0))
        );
        assert!(spans[0].highlight.is_none());
        assert!(spans[2].highlight.is_some());
    }
}

#[cfg(test)]
mod snapshot_performance_receipt {
    use super::*;
    #[test]
    #[ignore = "50,000-line owning-thread edit/snapshot measurement"]
    fn fifty_thousand_line_edit_and_capture() {
        use crate::repository_widget::text_editor::{Content, Cursor, Edit, Position};
        let source = (0..50_000).map(|line| format!("fn function_{line:05}() {{ let value = {line}; println!(\"value {{value}}\"); }}\n")).collect::<String>();
        let mut content: Content<cosmic::Renderer> = Content::with_text(&source);
        content.set_cursor(Cursor {
            position: Position {
                line: 25_000,
                column: 4,
            },
            selection: None,
        });
        let mut edit_us = Vec::new();
        let mut capture_us = Vec::new();
        for character in "abcdefghij".chars() {
            let started = std::time::Instant::now();
            content.perform_edit(Edit::Insert(character));
            std::hint::black_box(content.text_len());
            edit_us.push(started.elapsed().as_secs_f64() * 1_000_000.);
            let started = std::time::Instant::now();
            let snapshot = content.text_snapshot();
            capture_us.push(started.elapsed().as_secs_f64() * 1_000_000.);
            std::hint::black_box(snapshot);
        }
        let started = std::time::Instant::now();
        let snapshot = content.text_snapshot();
        let pause_capture_us = started.elapsed().as_secs_f64() * 1_000_000.;
        println!(
            "DIFF_TYPING_PERF lines=50000 source_bytes={} edit_us={edit_us:?} capture_us={capture_us:?} pause_capture_us={pause_capture_us:.3} snapshot_bytes={}",
            source.len(),
            snapshot.len()
        );
        assert_eq!(snapshot.len(), source.len() + 10);
    }
}

#[cfg(test)]
mod separated_change_contract {
    use super::*;
    #[test]
    fn distant_edits_reuse_each_intervening_unchanged_lexical_run() {
        let before: Arc<str> = (0..1000)
            .map(|line| format!("fn spaced_{line}() {{ let value = {line}; }}\n"))
            .collect::<String>()
            .into();
        let after = before
            .replace("value = 10;", "value = 11;")
            .replace("value = 900;", "value = 901;");
        let cancellation = CancellationToken::new();
        let theme = crate::settings::EditorSyntaxTheme::Auto;
        let _ = build_document_diff(
            Arc::clone(&before),
            &before,
            Path::new("separated.rs"),
            theme,
            &cancellation,
        )
        .unwrap();
        WORK_COUNTS.with(|counts| counts.set((0, 0)));
        let model = build_document_diff(
            Arc::clone(&before),
            &after,
            Path::new("separated.rs"),
            theme,
            &cancellation,
        )
        .unwrap();
        assert_eq!(WORK_COUNTS.with(std::cell::Cell::get).0, 2);
        assert!(Arc::ptr_eq(
            &model.syntax.old[0][500],
            &model.syntax.new[0][500]
        ));
        assert_eq!(
            model.syntax.new.as_ref(),
            &highlight_side(&after, Path::new("separated.rs"), theme, &cancellation).unwrap()
        );
    }
}

#[cfg(test)]
mod selected_font_contracts {
    use super::*;
    use cosmic::iced::{Font, font};

    #[test]
    fn selected_family_preserves_syntax_weight_and_style() {
        let selected = Font {
            family: font::Family::SansSerif,
            ..Font::MONOSPACE
        };
        let bold_italic = Font {
            weight: font::Weight::Bold,
            style: font::Style::Italic,
            ..Font::MONOSPACE
        };
        let key = ShapeKey::with_font(
            "bold tail",
            &[SyntaxSpan::new(0..4, None, Some(bold_italic))],
            selected,
        );
        assert!(
            key.fonts
                .iter()
                .all(|(_, font)| font.family == selected.family)
        );
        assert_eq!(key.fonts[0].1.weight, font::Weight::Bold);
        assert_eq!(key.fonts[0].1.style, font::Style::Italic);
        assert_eq!(key.fonts[1].1, selected);
    }

    #[test]
    fn selected_family_extents_match_the_actual_rich_text_and_share_baseline_syntax() {
        let before: Arc<str> =
            Arc::from("fn font_family_contract() { let iii = \"WWWW café\"; }\n");
        let after: Arc<str> =
            Arc::from("fn font_family_contract() { let iii = \"WWWW changed café\"; }\n");
        let cancel = CancellationToken::new();
        let mono = build_document_diff_sources_with_font(
            before.clone(),
            after.clone(),
            Path::new("font-family.rs"),
            crate::settings::EditorSyntaxTheme::Auto,
            false,
            Font::MONOSPACE,
            &cancel,
        )
        .unwrap();
        let selected = Font {
            family: font::Family::SansSerif,
            ..Font::MONOSPACE
        };
        let model = build_document_diff_sources_with_font(
            before,
            after,
            Path::new("font-family.rs"),
            crate::settings::EditorSyntaxTheme::Auto,
            false,
            selected,
            &cancel,
        )
        .unwrap();
        assert!(Arc::ptr_eq(&mono.syntax.old, &model.syntax.old));
        for (palette, extent) in model.max_line_width.into_iter().enumerate() {
            let mut expected = 0.0_f32;
            for row in &model.split_rows {
                for (old, line, text) in [
                    (true, row.old_line, row.old_text.as_deref()),
                    (false, row.new_line, row.new_text.as_deref()),
                ] {
                    if let Some(text) = text {
                        let mut spans = line_spans(
                            text,
                            &[],
                            model.syntax.line(old, line, palette == 1),
                            cosmic::iced::Color::TRANSPARENT,
                        );
                        for span in &mut spans {
                            if let Some(font) = &mut span.font {
                                font.family = selected.family;
                            } else {
                                span.font = Some(selected);
                            }
                        }
                        expected = expected.max(shaped_spans_width(&spans));
                    }
                }
            }
            assert!(
                (extent as f32 - expected.ceil()).abs() <= 1.0,
                "selected-family extent {extent}, native {expected}"
            );
        }
    }
}
