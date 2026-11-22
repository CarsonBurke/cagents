//! Syntax colouring for the editable buffer.
//!
//! Code is coloured by iced's syntect highlighter in the palette the user
//! chose. Markdown is prose, and a code palette paints it as code: a numbered
//! item, a quote or a task took the colour a theme gives a list or a deleted
//! line across the whole line, so the writing itself read as an error. A
//! Markdown buffer is coloured here instead: its words keep the editor's ink,
//! the marks that give it structure share one muted accent, emphasis is drawn
//! as weight and slant, and only code keeps a tint of its own.

use std::{collections::HashMap, ops::Range};

use cosmic::iced::{
    Color, Font,
    advanced::text::highlighter::{self as text_highlighter, Format},
    font, highlighter,
};
use two_face::re_exports::syntect::{highlighting, parsing};

use crate::diff_view::{SyntaxLease, syntax_lease, syntect_theme};

/// How many lines one saved parser state covers, as in iced's highlighter: an
/// edit reparses from the snapshot before it rather than from the top.
const LINES_PER_SNAPSHOT: usize = 50;

/// What the editor highlights a buffer with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Settings {
    /// The grammar and palette a code buffer is highlighted with. A Markdown
    /// buffer uses the palette only for the code its fences hold.
    pub code: highlighter::Settings,
    /// Whether the buffer is Markdown, which is coloured as prose.
    pub markdown: bool,
    /// The editor's font, which a bold or italic span keeps the family of.
    pub font: Font,
}

/// The editor's highlighter: iced's for code, and [`MarkdownHighlighter`]
/// for Markdown.
pub(crate) enum Highlighter {
    Code(highlighter::Highlighter),
    Markdown(MarkdownHighlighter),
}

/// A span's highlight, resolved against the theme when it is drawn.
#[derive(Debug)]
pub(crate) enum Highlight {
    Code(highlighter::Highlight),
    Markdown(MarkdownHighlight),
}

impl text_highlighter::Highlighter for Highlighter {
    type Settings = Settings;
    type Highlight = Highlight;
    type Iterator<'a> = Box<dyn Iterator<Item = (Range<usize>, Highlight)> + 'a>;

    fn new(settings: &Settings) -> Self {
        if settings.markdown {
            Self::Markdown(MarkdownHighlighter::new(settings))
        } else {
            Self::Code(highlighter::Highlighter::new(&settings.code))
        }
    }

    fn update(&mut self, settings: &Settings) {
        match self {
            Self::Code(code) if !settings.markdown => code.update(&settings.code),
            _ => *self = Self::new(settings),
        }
    }

    fn change_line(&mut self, line: usize) {
        match self {
            Self::Code(code) => code.change_line(line),
            Self::Markdown(markdown) => markdown.change_line(line),
        }
    }

    fn highlight_line(&mut self, line: &str) -> Self::Iterator<'_> {
        match self {
            Self::Code(code) => Box::new(
                code.highlight_line(line)
                    .map(|(range, highlight)| (range, Highlight::Code(highlight))),
            ),
            Self::Markdown(markdown) => Box::new(
                markdown
                    .highlight_line(line)
                    .into_iter()
                    .map(|(range, highlight)| (range, Highlight::Markdown(highlight))),
            ),
        }
    }

    fn current_line(&self) -> usize {
        match self {
            Self::Code(code) => code.current_line(),
            Self::Markdown(markdown) => markdown.current_line,
        }
    }
}

/// The text editor's `highlight_with` format function.
pub(crate) fn format(highlight: &Highlight, theme: &cosmic::Theme) -> Format<Font> {
    match highlight {
        Highlight::Code(highlight) => highlight.to_format(),
        Highlight::Markdown(highlight) => highlight.format(&Palette::of(theme)),
    }
}

/// The part a span of Markdown source plays, which decides its ink.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Tone {
    /// The writing itself, in the editor's own ink.
    Text,
    /// A mark that gives a block its structure: a list bullet or number, a
    /// quote's `>`, a task box, a heading's hashes, a table's pipes, a fence.
    Marker,
    /// The words of a link, which are what a reader follows.
    Link,
    /// Inline punctuation and a link's destination: present, but receding
    /// behind the words they wrap.
    Dim,
    /// Code the grammar does not colour: an inline span, or a fence with no
    /// language.
    Code,
    /// A token of a fence's language, in the code palette's colour for it.
    Syntax(Color),
}

/// How a span of Markdown source is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Style {
    pub tone: Tone,
    pub bold: bool,
    pub italic: bool,
}

impl Style {
    const TEXT: Self = Self {
        tone: Tone::Text,
        bold: false,
        italic: false,
    };
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MarkdownHighlight {
    style: Style,
    font: Font,
}

impl MarkdownHighlight {
    fn format(&self, palette: &Palette) -> Format<Font> {
        let Style { tone, bold, italic } = self.style;
        Format {
            color: match tone {
                Tone::Text => None,
                Tone::Marker => Some(palette.marker),
                Tone::Link => Some(palette.link),
                Tone::Dim => Some(palette.dim),
                Tone::Code => Some(palette.code),
                Tone::Syntax(color) => Some(color),
            },
            font: (bold || italic).then_some(Font {
                weight: if bold {
                    font::Weight::Bold
                } else {
                    self.font.weight
                },
                style: if italic {
                    font::Style::Italic
                } else {
                    self.font.style
                },
                ..self.font
            }),
        }
    }
}

/// The inks Markdown is drawn in, from the theme the editor is drawn with.
struct Palette {
    marker: Color,
    link: Color,
    dim: Color,
    code: Color,
}

impl Palette {
    fn of(theme: &cosmic::Theme) -> Self {
        use cosmic::iced::{Background, widget::text_editor::Catalog};

        let editor = theme.style(
            &<cosmic::Theme as Catalog>::default(),
            cosmic::iced::widget::text_editor::Status::Active,
        );
        let cosmic = theme.cosmic();
        let ink = editor.value;
        let ground = match editor.background {
            Background::Color(color) => color,
            Background::Gradient(_) => cosmic.bg_color().into(),
        };
        let accent: Color = cosmic.accent_text_color().into();
        Self {
            // Muted so a column of bullets or a run of table pipes reads as
            // structure beside the words rather than above them.
            marker: crate::design::mix(ground, accent, 0.78),
            link: accent,
            dim: crate::design::mix(ground, ink, 0.55),
            code: cosmic.palette.accent_warm_grey.into(),
        }
    }
}

/// Highlights Markdown source as prose, and the code its fences hold in the
/// code palette.
pub(crate) struct MarkdownHighlighter {
    syntaxes: SyntaxLease,
    /// The parser state at the top of the buffer.
    initial: parsing::ParseState,
    code: highlighting::Highlighter<'static>,
    font: Font,
    /// The parser state every [`LINES_PER_SNAPSHOT`] lines so far; see
    /// [`MarkdownHighlighter::change_line`].
    snapshots: Vec<(parsing::ParseState, parsing::ScopeStack)>,
    current_line: usize,
    /// Each scope stack's style. Stacks recur throughout a document, and
    /// classifying one builds its scopes' names.
    styles: HashMap<Vec<parsing::Scope>, Style>,
}

impl MarkdownHighlighter {
    fn new(settings: &Settings) -> Self {
        let syntaxes = syntax_lease();
        let markdown = syntaxes
            .syntaxes()
            .find_syntax_by_name("Markdown")
            .unwrap_or_else(|| syntaxes.syntaxes().find_syntax_plain_text());
        let initial = parsing::ParseState::new(markdown);
        Self {
            snapshots: vec![(initial.clone(), parsing::ScopeStack::new())],
            initial,
            syntaxes,
            code: highlighting::Highlighter::new(syntect_theme_for(&settings.code.theme)),
            font: settings.font,
            current_line: 0,
            styles: HashMap::new(),
        }
    }

    /// Rewinds to the last snapshot at or before `line`, as iced's
    /// highlighter does: `snapshots[k]` holds the state at line
    /// `(k + 1) * LINES_PER_SNAPSHOT`, and the last entry is the one being
    /// advanced.
    ///
    /// A line the highlighter has not reached yet needs no rewind. iced's
    /// rewinds to the block it falls in all the same, which resumes a block
    /// it never finished from the wrong state.
    fn change_line(&mut self, line: usize) {
        if line >= self.current_line {
            return;
        }
        let snapshot = line / LINES_PER_SNAPSHOT;
        if snapshot < self.snapshots.len() {
            self.snapshots.truncate(snapshot);
            self.current_line = snapshot * LINES_PER_SNAPSHOT;
        } else {
            self.snapshots.clear();
            self.current_line = 0;
        }
        let start = self
            .snapshots
            .last()
            .cloned()
            .unwrap_or_else(|| (self.initial.clone(), parsing::ScopeStack::new()));
        self.snapshots.push(start);
    }

    fn highlight_line(&mut self, line: &str) -> Vec<(Range<usize>, MarkdownHighlight)> {
        if self.current_line / LINES_PER_SNAPSHOT >= self.snapshots.len() {
            let snapshot = self
                .snapshots
                .last()
                .expect("one snapshot is always kept")
                .clone();
            self.snapshots.push(snapshot);
        }
        self.current_line += 1;

        let (parser, stack) = self
            .snapshots
            .last_mut()
            .expect("one snapshot is always kept");
        let ops = parser
            .parse_line(line, self.syntaxes.syntaxes())
            .unwrap_or_default();
        let mut spans = Vec::new();
        let mut start = 0;
        // Zero-length operations still carry state from line to line, so
        // every one is applied, as iced's highlighter does.
        for (end, op) in ops
            .iter()
            .map(|(end, op)| (*end, Some(op)))
            .chain(std::iter::once((line.len(), None)))
        {
            if start < end {
                let style = match self.styles.get(stack.scopes.as_slice()) {
                    Some(style) => *style,
                    None => {
                        let style = classify(&stack.scopes, &self.code);
                        if self.styles.len() < 4096 {
                            self.styles.insert(stack.scopes.clone(), style);
                        }
                        style
                    }
                };
                spans.push((start..end, style));
            }
            if let Some(op) = op {
                let _ = stack.apply(op);
            }
            start = end;
        }
        if let Some(task) = task_box(line) {
            spans = mark_task_box(spans, task);
        }
        let font = self.font;
        spans
            .into_iter()
            .filter(|(_, style)| *style != Style::TEXT)
            .map(|(range, style)| (range, MarkdownHighlight { style, font }))
            .collect()
    }
}

/// Where a task item's box, `[ ]` or `[x]`, sits in `line`, if the line
/// opens one.
///
/// The grammar scopes a task's box as part of the item's words, so it is
/// found here: a bullet or number, whitespace, the box, and whitespace after
/// it, as GitHub requires.
fn task_box(line: &str) -> Option<Range<usize>> {
    let body = line.trim_start_matches([' ', '\t', '>']);
    let rest = match body.strip_prefix(['-', '*', '+']) {
        Some(rest) => rest,
        None => {
            let digits = body.bytes().take_while(u8::is_ascii_digit).count();
            if !(1..=9).contains(&digits) {
                return None;
            }
            body[digits..].strip_prefix(['.', ')'])?
        }
    };
    let words = rest.trim_start_matches([' ', '\t']);
    if words.len() == rest.len() {
        return None;
    }
    let at = line.len() - words.len();
    let mark = words.get(..3)?;
    (matches!(mark, "[ ]" | "[x]" | "[X]") && words[3..].starts_with([' ', '\t']))
        .then_some(at..at + 3)
}

/// `spans` with the plain words inside `task` drawn as a marker. Only words
/// the grammar left plain are taken, so a line that merely looks like a task
/// inside a code block keeps its code style.
fn mark_task_box(
    spans: Vec<(Range<usize>, Style)>,
    task: Range<usize>,
) -> Vec<(Range<usize>, Style)> {
    let mut marked = Vec::with_capacity(spans.len() + 2);
    for (range, style) in spans {
        let inside = range.start.max(task.start)..range.end.min(task.end);
        if style.tone != Tone::Text || inside.is_empty() {
            marked.push((range, style));
            continue;
        }
        let marker = Style {
            tone: Tone::Marker,
            ..style
        };
        for (part, style) in [
            (range.start..inside.start, style),
            (inside.clone(), marker),
            (inside.end..range.end, style),
        ] {
            if !part.is_empty() {
                marked.push((part, style));
            }
        }
    }
    marked
}

/// The syntect palette for iced's `theme`, which [`Settings::code`] names.
fn syntect_theme_for(theme: &highlighter::Theme) -> &'static highlighting::Theme {
    use crate::settings::EditorSyntaxTheme;
    let (setting, dark) = match theme {
        highlighter::Theme::SolarizedDark => (EditorSyntaxTheme::SolarizedDark, true),
        highlighter::Theme::Base16Mocha => (EditorSyntaxTheme::Mocha, true),
        highlighter::Theme::Base16Ocean => (EditorSyntaxTheme::Ocean, true),
        highlighter::Theme::Base16Eighties => (EditorSyntaxTheme::Eighties, true),
        highlighter::Theme::InspiredGitHub => (EditorSyntaxTheme::InspiredGitHub, false),
    };
    syntect_theme(setting, dark)
}

/// The style of a span whose scopes, outermost first, are `scopes`.
///
/// A fence's contents defer to the code palette for the language it names;
/// everything else is decided by the innermost scope that means something to
/// a reader of Markdown, with bold and italic accumulated from every scope
/// around it.
pub(crate) fn classify(scopes: &[parsing::Scope], code: &highlighting::Highlighter<'_>) -> Style {
    let names: Vec<String> = scopes.iter().map(|scope| scope.build_string()).collect();
    let has = |prefix: &str| names.iter().any(|name| name.starts_with(prefix));
    let bold = has("markup.bold") || has("markup.heading");
    let italic = has("markup.italic");
    let style = |tone| Style { tone, bold, italic };

    // A fence's body: the language it names keeps its own colours, from the
    // scopes the embedded grammar added, and anything that grammar leaves
    // uncoloured is code all the same.
    if let Some(embedded) = names
        .iter()
        .skip(1)
        .position(|name| name.starts_with("source.") || name.starts_with("text."))
        .map(|index| index + 1)
    {
        let fence = names[..embedded]
            .iter()
            .any(|name| name.starts_with("markup.raw"));
        if fence {
            let tone = code
                .style_mod_for_stack(&scopes[embedded..])
                .foreground
                .map_or(Tone::Code, |color| {
                    Tone::Syntax(Color::from_rgba8(
                        color.r,
                        color.g,
                        color.b,
                        f32::from(color.a) / 255.0,
                    ))
                });
            return Style {
                tone,
                bold: false,
                italic: false,
            };
        }
    }

    for name in names.iter().rev() {
        let name = name.as_str();
        let tone = if name.starts_with("punctuation.definition.heading")
            || name.starts_with("punctuation.definition.list_item")
            || name.starts_with("punctuation.definition.blockquote")
            || name.starts_with("punctuation.separator.table-cell")
            || name.starts_with("punctuation.definition.raw.code-fence")
            || name.starts_with("punctuation.definition.checkbox")
            || name.starts_with("markup.checkbox")
            || name.starts_with("meta.table.header-separator")
            || name.starts_with("meta.code-fence.definition")
            || name.starts_with("markup.list.numbered.bullet")
            || name.starts_with("markup.list.unnumbered.bullet")
            || name.starts_with("meta.separator")
        {
            Some(Tone::Marker)
        } else if name.starts_with("markup.underline.link") {
            // A destination recedes behind the words that name it, or an
            // image's description; a bare or angle-bracketed URL has no
            // other words, so it is the link.
            Some(
                if has("meta.link.inline") || has("meta.link.reference") || has("meta.image") {
                    Tone::Dim
                } else {
                    Tone::Link
                },
            )
        } else if name.starts_with("punctuation.definition.link")
            || name.starts_with("punctuation.definition.metadata")
            || name.starts_with("punctuation.definition.string")
            || name.starts_with("punctuation.definition.bold")
            || name.starts_with("punctuation.definition.italic")
            || name.starts_with("punctuation.definition.raw")
            || name.starts_with("comment")
        {
            Some(Tone::Dim)
        } else if name.starts_with("string.other.link")
            || name.starts_with("meta.link.reference.description")
            || name.starts_with("meta.link.inline.description")
        {
            Some(Tone::Link)
        } else if name.starts_with("markup.raw") {
            Some(Tone::Code)
        } else {
            None
        };
        if let Some(tone) = tone {
            return style(tone);
        }
    }
    style(Tone::Text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::advanced::text::Highlighter as _;

    /// The Markdown a user reported reading as errors: every construct the
    /// editor has to draw as prose.
    const DOCUMENT: &str = "## Summary

The nightly build failed because the **cache key** ignored `Cargo.lock`, so CI restored a stale target directory.

### What I changed

1. Keyed the cache on the lockfile hash.
2. Added a *cold* build to the weekly job.
3. Documented the cache in [the CI guide](docs/ci.md).

| Job | Before | After |
|---|---|---|
| nightly | 14 min | 6 min |

```sh
cargo clean && cargo build --locked
```

```
- [ ] not a task
```

> The old cache entries expire on their own within a week.

- [x] Verified on a fresh runner
- [ ] Watch the next nightly run
";

    fn settings(markdown: bool, token: &str) -> Settings {
        Settings {
            code: highlighter::Settings {
                theme: highlighter::Theme::Base16Ocean,
                token: token.to_owned(),
            },
            markdown,
            font: Font::MONOSPACE,
        }
    }

    /// One highlighted range of `lines[line]`.
    struct Span {
        line: usize,
        range: Range<usize>,
        highlight: Highlight,
    }

    struct Highlights {
        lines: Vec<String>,
        spans: Vec<Span>,
    }

    impl Highlights {
        fn text(&self, span: &Span) -> &str {
            &self.lines[span.line][span.range.clone()]
        }
    }

    /// Every highlighted span of `text`, with its lines.
    fn highlights(settings: &Settings, text: &str) -> Highlights {
        let mut highlighter = Highlighter::new(settings);
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let mut spans = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            spans.extend(
                highlighter
                    .highlight_line(line)
                    .map(|(range, highlight)| Span {
                        line: index,
                        range,
                        highlight,
                    }),
            );
        }
        Highlights { lines, spans }
    }

    /// The style of the first `text` on `line` (counting from 0), which every
    /// byte of it must share; bytes no span covers are plain text.
    fn style(highlights: &Highlights, line: usize, text: &str) -> Style {
        let start = highlights.lines[line]
            .find(text)
            .unwrap_or_else(|| panic!("{text:?} is not on line {line}"));
        let styles: Vec<Style> = (start..start + text.len())
            .map(|byte| {
                highlights
                    .spans
                    .iter()
                    .find(|span| span.line == line && span.range.contains(&byte))
                    .map_or(Style::TEXT, |span| match &span.highlight {
                        Highlight::Markdown(highlight) => highlight.style,
                        Highlight::Code(_) => panic!("Markdown is drawn as code"),
                    })
            })
            .collect();
        assert!(
            styles.iter().all(|style| *style == styles[0]),
            "{text:?} on line {line} is drawn in several styles: {styles:?}"
        );
        styles[0]
    }

    fn tone(tone: Tone) -> Style {
        Style {
            tone,
            ..Style::TEXT
        }
    }

    #[test]
    fn markdown_words_keep_the_ink_and_only_structure_is_accented() {
        let spans = highlights(&settings(true, "md"), DOCUMENT);
        let plain = |line: usize, text: &str| {
            assert_eq!(style(&spans, line, text), Style::TEXT, "{text:?}");
        };
        // The words of a list item, a quote and a task are prose.
        plain(6, "Keyed the cache on the lockfile hash.");
        plain(
            22,
            " The old cache entries expire on their own within a week.",
        );
        plain(24, " Verified on a fresh runner");
        plain(25, " Watch the next nightly run");
        plain(2, "The nightly build failed because the ");

        // Only the marks that give a block its shape are accented.
        for (line, marker) in [
            (6, "1"),
            (6, "."),
            (22, ">"),
            (24, "-"),
            (24, "[x]"),
            (25, "[ ]"),
            (10, "|"),
            (11, "|"),
            (11, "---"),
            (14, "```"),
            (16, "```"),
        ] {
            assert_eq!(style(&spans, line, marker).tone, Tone::Marker, "{marker:?}");
        }

        // Headings are bold in the text's own ink, hashes included.
        assert_eq!(
            style(&spans, 0, "Summary"),
            Style {
                bold: true,
                ..Style::TEXT
            }
        );
        assert_eq!(
            style(&spans, 0, "##"),
            Style {
                tone: Tone::Marker,
                bold: true,
                italic: false
            }
        );
        assert_eq!(
            style(&spans, 4, "What I changed"),
            Style {
                bold: true,
                ..Style::TEXT
            }
        );

        // Emphasis is weight and slant; its delimiters recede.
        assert_eq!(
            style(&spans, 2, "cache key"),
            Style {
                bold: true,
                ..Style::TEXT
            }
        );
        assert_eq!(style(&spans, 2, "**").tone, Tone::Dim);
        assert_eq!(
            style(&spans, 7, "cold"),
            Style {
                italic: true,
                ..Style::TEXT
            }
        );

        // Code is tinted, a link's words are accented and its target dimmed.
        assert_eq!(style(&spans, 2, "Cargo.lock"), tone(Tone::Code));
        assert_eq!(style(&spans, 2, "`"), tone(Tone::Dim));
        assert_eq!(style(&spans, 8, "the CI guide"), tone(Tone::Link));
        assert_eq!(style(&spans, 8, "docs/ci.md"), tone(Tone::Dim));

        // A fence keeps its language's colours, and a fence without one is
        // code, even where its text looks like a task.
        assert!(matches!(style(&spans, 15, "cargo").tone, Tone::Syntax(_)));
        assert_eq!(style(&spans, 19, "- [ ] not a task"), tone(Tone::Code));
    }

    #[test]
    fn a_url_with_no_words_of_its_own_is_the_link() {
        let spans = highlights(
            &settings(true, "md"),
            "See <https://example.com/a> and https://example.com/b.\n\
             Read [the guide][guide] or [this](https://example.com/c).\n\
             \n\
             [guide]: https://example.com/d\n\
             ![A diagram](img/diagram.png)\n",
        );
        assert_eq!(style(&spans, 0, "https://example.com/a"), tone(Tone::Link));
        assert_eq!(style(&spans, 0, "https://example.com/b"), tone(Tone::Link));
        assert_eq!(style(&spans, 1, "the guide"), tone(Tone::Link));
        assert_eq!(style(&spans, 1, "this"), tone(Tone::Link));
        assert_eq!(style(&spans, 1, "https://example.com/c"), tone(Tone::Dim));
        assert_eq!(style(&spans, 3, "https://example.com/d"), tone(Tone::Dim));
        assert_eq!(style(&spans, 4, "img/diagram.png"), tone(Tone::Dim));
    }

    #[test]
    fn markdown_inks_come_from_the_theme_and_none_is_the_error_red() {
        for (name, theme) in [
            ("dark", cosmic::Theme::dark()),
            ("light", cosmic::Theme::light()),
        ] {
            let palette = Palette::of(&theme);
            let inks = [palette.marker, palette.link, palette.dim, palette.code];
            for red in [
                crate::design::destructive_color(&theme),
                crate::design::diff_removed_color(&theme),
            ] {
                assert!(!inks.contains(&red), "{red:?} in {name}");
            }
            let spans = highlights(&settings(true, "md"), DOCUMENT);
            for span in &spans.spans {
                let (text, highlight) = (spans.text(span), &span.highlight);
                let format = format(highlight, &theme);
                if let Highlight::Markdown(MarkdownHighlight {
                    style:
                        Style {
                            tone: Tone::Syntax(_),
                            ..
                        },
                    ..
                }) = highlight
                {
                    continue;
                }
                // Headings and emphasis keep the ink and change only weight.
                if let Some(color) = format.color {
                    assert!(inks.contains(&color), "{text:?} drawn in {color:?}");
                }
            }
        }
    }

    #[test]
    fn bold_and_italic_keep_the_editor_font_family() {
        let mut settings = settings(true, "md");
        settings.font = Font::with_name("Iosevka");
        let spans = highlights(&settings, "## Plan\n\n**bold** and *slant*\n");
        let theme = cosmic::Theme::dark();
        let font = |line: usize, text: &str| {
            let span = spans
                .spans
                .iter()
                .find(|span| span.line == line && spans.text(span) == text)
                .unwrap();
            format(&span.highlight, &theme).font.unwrap()
        };
        assert_eq!(font(0, "Plan").family, settings.font.family);
        assert_eq!(font(0, "Plan").weight, font::Weight::Bold);
        assert_eq!(font(2, "bold").weight, font::Weight::Bold);
        assert_eq!(font(2, "slant").style, font::Style::Italic);
        assert_eq!(font(2, "slant").family, settings.font.family);
    }

    /// Another language is still drawn by iced's highlighter, span for span.
    #[test]
    fn code_is_highlighted_exactly_as_before() {
        let source = "/// Docs.\npub fn main() -> u32 {\n    let x = \"s\"; // note\n    0\n}\n";
        let ours = highlights(&settings(false, "rs"), source);
        let mut iced = highlighter::Highlighter::new(&settings(false, "rs").code);
        let mut theirs = Vec::new();
        for (index, line) in source.lines().enumerate() {
            for (range, highlight) in iced.highlight_line(line) {
                theirs.push((index, range, highlight.to_format()));
            }
        }
        let theme = cosmic::Theme::dark();
        let ours: Vec<_> = ours
            .spans
            .iter()
            .map(|span| {
                (
                    span.line,
                    span.range.clone(),
                    format(&span.highlight, &theme),
                )
            })
            .collect();
        assert!(!ours.is_empty());
        assert_eq!(ours, theirs);
    }

    #[test]
    fn a_change_past_the_highlighted_lines_keeps_the_position() {
        let mut highlighter = Highlighter::new(&settings(true, "md"));
        for line in ["```sh", "echo 1"] {
            highlighter.highlight_line(line).for_each(drop);
        }
        highlighter.change_line(60);
        assert_eq!(highlighter.current_line(), 2);
        let spans: Vec<_> = highlighter.highlight_line("echo 2").collect();
        assert!(spans.iter().all(|(_, highlight)| matches!(
            highlight,
            Highlight::Markdown(MarkdownHighlight {
                style: Style {
                    tone: Tone::Syntax(_) | Tone::Code,
                    ..
                },
                ..
            })
        )));
    }

    #[test]
    fn task_boxes_follow_a_bullet_or_number() {
        assert_eq!(task_box("- [ ] todo"), Some(2..5));
        assert_eq!(task_box("  * [x] done"), Some(4..7));
        assert_eq!(task_box("12. [X] done"), Some(4..7));
        assert_eq!(task_box("> - [ ] quoted"), Some(4..7));
        assert_eq!(task_box("- [ ]"), None);
        assert_eq!(task_box("-[ ] tight"), None);
        assert_eq!(task_box("[ ] bare"), None);
        assert_eq!(task_box("- [y] other"), None);
    }

    /// An edit part-way down a long document reparses from the snapshot
    /// before it and colours the rest as a fresh pass would.
    #[test]
    fn an_edit_reparses_from_the_nearest_snapshot() {
        let text: String = (0..120)
            .map(|index| match index % 4 {
                0 => "```sh\n".to_owned(),
                1 => format!("echo {index}\n"),
                2 => "```\n".to_owned(),
                _ => format!("- [ ] item {index}\n"),
            })
            .collect();
        let settings = settings(true, "md");
        let fresh = highlights(&settings, &text);
        let mut highlighter = Highlighter::new(&settings);
        let lines: Vec<&str> = text.lines().collect();
        for line in &lines {
            highlighter.highlight_line(line).for_each(drop);
        }
        highlighter.change_line(75);
        assert_eq!(highlighter.current_line(), 50);
        let theme = cosmic::Theme::dark();
        let mut again = Vec::new();
        for (index, line) in lines.iter().enumerate().skip(50) {
            for (range, highlight) in highlighter.highlight_line(line) {
                again.push((index, range, format(&highlight, &theme)));
            }
        }
        let fresh: Vec<_> = fresh
            .spans
            .iter()
            .filter(|span| span.line >= 50)
            .map(|span| {
                (
                    span.line,
                    span.range.clone(),
                    format(&span.highlight, &theme),
                )
            })
            .collect();
        assert_eq!(again, fresh);
    }
}
