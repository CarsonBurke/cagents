//! The transcript's Markdown, drawn into widgets that own what they show.
//!
//! iced's own `markdown::view` ties every element to the parsed items it was
//! built from. That is fine for a view rebuilt from scratch on every frame, but
//! it is exactly what stops a finished exchange from being kept between frames:
//! a retained element cannot borrow from a cache that the next refresh is free
//! to change. Everything here is built from shared, immutable pieces instead -
//! the styled spans iced already caches per paragraph behind an `Arc`, and the
//! highlighted spans this module caches per fenced block - so the result can
//! outlive the borrow it was built under.
//!
//! The layout follows iced's defaults item for item, so moving an answer onto
//! this renderer changes what it costs, not what it looks like. The
//! deliberate differences are the shell's own surfaces: a fenced block is one
//! paragraph rather than one text widget per line, and it, an image's
//! alternative text and a wide table use the transcript's sunken surface and
//! scrollbar rather than iced's stock ones.

use super::*;
use iced::widget::text::Span;

/// How a link in an answer reaches the surface.
fn link(target: Target, uri: markdown::Uri) -> Message {
    action(Action::OpenDocument(target, uri.to_string()))
}

/// A fenced block ready to draw: all of its lines as one paragraph of spans,
/// once per palette, in the order [`crate::diff_view::SyntaxLines`] uses.
///
/// Built once when the block's text changes, so a frame costs a reference
/// count rather than a pass over the block's lines. A block opened to be
/// [extended](Self::extend) keeps where its last whole line left the
/// highlighter, so the fence an answer is still writing costs a refresh the
/// lines that arrived in it rather than every line it has.
pub(super) struct CodeBlock {
    spans: [Arc<[Span<'static, String>]>; 2],
    /// Where the block resumes if its text grows, for as long as it may.
    tail: Option<Tail>,
}

/// A block as its last whole line left it. Anything after that line is drawn
/// provisionally and done again when more of it arrives.
struct Tail {
    /// The fence's info string, which chooses the grammar.
    language: Option<String>,
    /// The grammar's state after the whole lines; `None` for a block drawn
    /// plain.
    highlighter: Option<crate::diff_view::SnippetHighlighter>,
    /// How many bytes of the block its whole lines are.
    text: usize,
    /// How many of each palette's spans draw them.
    spans: [usize; 2],
}

impl CodeBlock {
    /// The font a block's paragraph is set in. Its spans name their own only
    /// where highlighting or [`monospace`] chose one, relative to this.
    pub(super) const FONT: iced::Font = iced::Font::MONOSPACE;

    /// A block that will not change.
    pub(super) fn new(
        language: Option<&str>,
        code: &str,
        theme: crate::settings::EditorSyntaxTheme,
    ) -> Self {
        let mut block = Self::open(language, code, theme);
        block.settle();
        block
    }

    /// A block that more text may be [added to](Self::extend).
    pub(super) fn open(
        language: Option<&str>,
        code: &str,
        theme: crate::settings::EditorSyntaxTheme,
    ) -> Self {
        let mut block = Self {
            spans: Default::default(),
            tail: Some(Tail {
                language: language.map(str::to_owned),
                highlighter: language
                    .filter(|_| Self::highlighted(code))
                    .map(|language| crate::diff_view::SnippetHighlighter::new(language, theme)),
                text: 0,
                spans: [0; 2],
            }),
        };
        block.write(code);
        block
    }

    /// Whether a block holding `code` is small enough to highlight.
    fn highlighted(code: &str) -> bool {
        code.len() <= CachedItem::HIGHLIGHT_LIMIT
    }

    /// Makes this the block for `code`, and says whether it could: `code` has
    /// to be the same fence with the same whole lines and anything at all
    /// after them. A block that could not is left as it was.
    ///
    /// The lines are compared rather than assumed, because the caller only
    /// knows that this was the last fence it saw, not that the Markdown still
    /// reads the same text into it.
    pub(super) fn extend(&mut self, language: Option<&str>, code: &str) -> bool {
        let Some(tail) = &self.tail else {
            return false;
        };
        let wanted = language.is_some() && Self::highlighted(code);
        let mut rest = code.get(..tail.text);
        let unchanged = self.spans[0][..tail.spans[0]].iter().all(|span| {
            rest = rest.and_then(|rest| rest.strip_prefix(span.text.as_ref()));
            rest.is_some()
        });
        if tail.language.as_deref() != language
            || tail.highlighter.is_some() != wanted
            || !unchanged
            || rest != Some("")
        {
            return false;
        }
        self.write(code);
        true
    }

    /// Gives up the state a block is extended from, once it will not be.
    pub(super) fn settle(&mut self) {
        self.tail = None;
    }

    /// Draws everything in `code` past the block's whole lines.
    fn write(&mut self, code: &str) {
        let Some(tail) = &mut self.tail else {
            return;
        };
        let palettes = if tail.highlighter.is_some() { 2 } else { 1 };
        let mut spans: [Vec<Span<'static, String>>; 2] = Default::default();
        for (index, spans) in spans.iter_mut().enumerate().take(palettes) {
            spans.extend_from_slice(&self.spans[index][..tail.spans[index]]);
        }
        for source in code[tail.text..].split_inclusive('\n') {
            let whole = source.ends_with('\n');
            let syntax = tail.highlighter.as_mut().map(|highlighter| {
                if whole {
                    highlighter.line(source)
                } else {
                    highlighter.unfinished(source)
                }
            });
            for (index, spans) in spans.iter_mut().enumerate().take(palettes) {
                line(
                    source,
                    syntax.as_ref().map(|syntax| syntax[index].as_slice()),
                    spans,
                );
            }
            if whole {
                tail.text += source.len();
                tail.spans = [spans[0].len(), spans[1].len()];
            }
        }
        let [light, dark] = spans;
        let light: Arc<[_]> = light.into();
        // A plain block is the same in both palettes.
        let dark = if palettes == 2 {
            dark.into()
        } else {
            light.clone()
        };
        self.spans = [light, dark];
    }

    /// The block's spans in the palette the current appearance uses.
    pub(super) fn spans(&self, dark: bool) -> &Arc<[Span<'static, String>]> {
        &self.spans[usize::from(dark)]
    }
}

/// Adds one source line of a block to `spans`, under `syntax` where the block
/// is highlighted.
fn line(
    source: &str,
    syntax: Option<&[crate::diff_view::SyntaxSpan]>,
    spans: &mut Vec<Span<'static, String>>,
) {
    let (line, ending) = if let Some(line) = source.strip_suffix("\r\n") {
        (line, "\r\n")
    } else if let Some(line) = source.strip_suffix('\n') {
        (line, "\n")
    } else {
        (source, "")
    };
    let start = spans.len();
    match syntax {
        Some(syntax) => spans.extend(
            crate::diff_view::syntax_spans(line, syntax)
                .into_iter()
                .map(|span| {
                    let span = span.to_static();
                    Span {
                        text: span.text,
                        size: span.size,
                        line_height: span.line_height,
                        font: span.font,
                        color: span.color,
                        link: None,
                        highlight: span.highlight,
                        padding: span.padding,
                        underline: span.underline,
                        strikethrough: span.strikethrough,
                    }
                }),
        ),
        None => spans.push(Span::new(line.to_owned())),
    }
    // The same glyphs, found without ranking every installed face per word.
    if monospace::covers(line) {
        for span in &mut spans[start..] {
            span.font = Some(monospace::concrete(span.font.unwrap_or(CodeBlock::FONT)));
        }
    }
    if !ending.is_empty() {
        spans.push(Span::new(ending));
    }
}

/// The language a fence's info string names: its first word.
///
/// What follows the language is for whatever runs the block - `rust,ignore`,
/// `ts title="x.ts"` - and is neither a grammar to look up nor a caption.
pub(super) fn fence_language(info: &str) -> Option<&str> {
    info.split(|character: char| character == ',' || character.is_whitespace())
        .find(|word| !word.is_empty())
}

/// An item's parsed Markdown as an element that owns what it draws.
pub(super) fn view(
    cached: &CachedItem,
    settings: markdown::Settings,
    target: Target,
    find: Option<&str>,
) -> Element<'static, Message> {
    Renderer {
        target,
        cached,
        dark: cosmic::theme::is_dark(),
        find,
    }
    .items(cached.markdown.items(), settings)
}

struct Renderer<'c> {
    find: Option<&'c str>,
    target: Target,
    cached: &'c CachedItem,
    dark: bool,
}

impl Renderer<'_> {
    fn items(
        &self,
        items: &[markdown::Item],
        settings: markdown::Settings,
    ) -> Element<'static, Message> {
        widget::column::with_children(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| self.item(item, index, settings))
                .collect::<Vec<_>>(),
        )
        .spacing(settings.spacing.0)
        .into()
    }

    fn item(
        &self,
        item: &markdown::Item,
        index: usize,
        settings: markdown::Settings,
    ) -> Element<'static, Message> {
        use markdown::Item;
        match item {
            Item::Heading(level, text) => {
                heading(settings, *level, text, index, self.target, self.find)
            }
            Item::Paragraph(text) => paragraph(settings, text, self.target, self.find),
            Item::CodeBlock { language, code, .. } => {
                self.code_block(settings, language.as_deref(), code)
            }
            Item::List { start, bullets } => self.list(settings, *start, bullets),
            Item::Quote(items) => {
                iced::widget::row![iced::widget::rule::vertical(4), self.items(items, settings),]
                    .height(Length::Shrink)
                    .spacing(settings.spacing.0)
                    .into()
            }
            Item::Rule => iced::widget::rule::horizontal(2).into(),
            Item::Table { columns, rows } => self.table(settings, columns, rows),
            Item::Image { url, title, alt } => {
                let mut rows = Vec::new();
                let alt = alt.spans(settings.style);
                if !alt.is_empty() {
                    rows.push(selectable_prose::text(
                        alt,
                        settings.text_size,
                        iced::Font::default(),
                        self.target,
                        self.find,
                    ));
                }
                if !title.is_empty() {
                    rows.push(selectable_prose::text(
                        vec![Span::new(title.clone())].into(),
                        design::TEXT_CAPTION.into(),
                        iced::Font::default(),
                        self.target,
                        self.find,
                    ));
                }
                rows.push(
                    widget::button::link("Open image source")
                        .on_press(link(self.target, url.clone()))
                        .into(),
                );
                widget::container(widget::column::with_children(rows).spacing(settings.spacing.0))
                    .padding(settings.spacing.0)
                    .class(design::surface(
                        Some(design::sunken),
                        [design::RADIUS_S; 4],
                        true,
                    ))
                    .into()
            }
        }
    }

    fn list(
        &self,
        settings: markdown::Settings,
        start: Option<u64>,
        bullets: &[markdown::Bullet],
    ) -> Element<'static, Message> {
        let nested = markdown::Settings {
            spacing: settings.spacing * 0.6,
            ..settings
        };
        let digits = start.map(|start| {
            ((start + bullets.len() as u64).max(1) as f32)
                .log10()
                .ceil()
        });
        let rows: Vec<Element<'static, Message>> = bullets
            .iter()
            .enumerate()
            .map(|(index, bullet)| {
                let (marker, items): (Element<'static, Message>, _) = match (bullet, start) {
                    (markdown::Bullet::Point { items }, Some(start))
                    | (markdown::Bullet::Task { items, .. }, Some(start)) => (
                        iced::widget::text(format!("{}.", index as u64 + start))
                            .size(settings.text_size)
                            .align_x(iced::alignment::Horizontal::Right)
                            .width(
                                settings.text_size * ((digits.unwrap_or(1.0) / 2.0).ceil() + 1.0),
                            )
                            .into(),
                        items,
                    ),
                    (markdown::Bullet::Point { items }, None) => (
                        iced::widget::text("•").size(settings.text_size).into(),
                        items,
                    ),
                    (markdown::Bullet::Task { items, done }, None) => (
                        widget::container(iced::widget::checkbox(*done).size(settings.text_size))
                            .center_y(
                                iced::widget::text::LineHeight::default()
                                    .to_absolute(settings.text_size),
                            )
                            .into(),
                        items,
                    ),
                };
                iced::widget::row![marker, self.items(items, nested)]
                    .spacing(settings.spacing.0)
                    .into()
            })
            .collect();
        let list = widget::column::with_children(rows).spacing(settings.spacing.0 * 0.75);
        if start.is_some() {
            list.into()
        } else {
            list.padding([0.0, settings.spacing.0]).into()
        }
    }

    fn table(
        &self,
        settings: markdown::Settings,
        columns: &[markdown::Column],
        rows: &[markdown::Row],
    ) -> Element<'static, Message> {
        use iced::widget::table;
        let table = table::table(
            columns.iter().enumerate().map(|(index, column)| {
                table::column(
                    self.items(&column.header, settings),
                    move |row: usize| match rows[row].cells.get(index) {
                        Some(cells) => self.items(cells, settings),
                        None => iced::widget::text("").into(),
                    },
                )
                .align_x(alignment(column))
            }),
            0..rows.len(),
        )
        .padding_x(settings.spacing.0)
        .padding_y(settings.spacing.0 / 2.0)
        .separator_x(0);
        iced::widget::scrollable(table)
            .direction(iced::widget::scrollable::Direction::Horizontal(
                design::scrollbar(),
            ))
            .spacing(settings.spacing.0 / 2.0)
            .into()
    }

    /// A fenced block on the sunken surface commands and outputs use, drawn
    /// from its cached spans in the palette the editor's syntax theme chose
    /// for the current appearance.
    fn code_block(
        &self,
        settings: markdown::Settings,
        language: Option<&str>,
        code: &str,
    ) -> Element<'static, Message> {
        let spans = self
            .cached
            .block(language, code)
            .map(|block| block.spans(self.dark).clone())
            .unwrap_or_else(|| vec![Span::new(code.to_owned())].into());
        let text = selectable_prose::text(
            spans,
            settings.code_size,
            CodeBlock::FONT,
            self.target,
            self.find,
        );
        step::code_frame(
            widget::scrollable(
                widget::container(text).padding([step::GAP_S as u16, step::GAP_M as u16]),
            )
            .direction(iced::widget::scrollable::Direction::Horizontal(
                design::scrollbar(),
            ))
            .into(),
            Some(
                language
                    .and_then(fence_language)
                    .unwrap_or("Code")
                    .to_owned(),
            ),
            Some(Action::CopyLiteral(code.to_owned())),
        )
    }
}

fn heading(
    settings: markdown::Settings,
    level: markdown::HeadingLevel,
    text: &markdown::Text,
    index: usize,
    target: Target,
    find: Option<&str>,
) -> Element<'static, Message> {
    use markdown::HeadingLevel;
    widget::container(selectable_prose::text(
        text.spans(heading_style(settings, level)),
        match level {
            HeadingLevel::H1 => settings.h1_size,
            HeadingLevel::H2 => settings.h2_size,
            HeadingLevel::H3 => settings.h3_size,
            HeadingLevel::H4 => settings.h4_size,
            HeadingLevel::H5 => settings.h5_size,
            HeadingLevel::H6 => settings.h6_size,
        },
        settings.style.font,
        target,
        find,
    ))
    .padding(iced::Padding::ZERO.top(if index > 0 {
        settings.text_size.0 / 2.0
    } else {
        0.0
    }))
    .into()
}

/// The style a heading's text is set in.
///
/// The three largest levels are told from a paragraph by their size. The
/// scale is compact, so the three smallest are the body's own size, and
/// without a weight of their own a `####` heading read as one more line of
/// the paragraph under it.
fn heading_style(settings: markdown::Settings, level: markdown::HeadingLevel) -> markdown::Style {
    use markdown::HeadingLevel;
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3 => settings.style,
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => markdown::Style {
            font: iced::Font {
                weight: iced::font::Weight::Semibold,
                ..settings.style.font
            },
            ..settings.style
        },
    }
}

fn paragraph(
    settings: markdown::Settings,
    text: &markdown::Text,
    target: Target,
    find: Option<&str>,
) -> Element<'static, Message> {
    selectable_prose::text(
        text.spans(settings.style),
        settings.text_size,
        settings.style.font,
        target,
        find,
    )
}

/// A table column's alignment as the layout takes it.
///
/// iced hands the alignment over as `pulldown_cmark`'s own type without
/// re-exporting it, and naming it would make the parser a dependency of this
/// crate for the sake of one match; its variant names are the contract.
fn alignment(column: &markdown::Column) -> iced::alignment::Horizontal {
    match format!("{:?}", column.alignment).as_str() {
        "Center" => iced::alignment::Horizontal::Center,
        "Right" => iced::alignment::Horizontal::Right,
        _ => iced::alignment::Horizontal::Left,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_images_keep_selectable_text_and_emit_scoped_source_actions() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, widget::Tree};
        use cosmic::iced::{Event, Point, Rectangle, Size, mouse};
        use iced_accessibility::accesskit::Role;

        for (source, uri, alt, title) in [
            (
                "![diagram](docs/diagram.png \"Architecture\")",
                "docs/diagram.png",
                "diagram",
                "Architecture",
            ),
            (
                "![](https://example.com/image.png)",
                "https://example.com/image.png",
                "",
                "",
            ),
        ] {
            let target = Target {
                card: CardId::new(),
                generation: 17,
            };
            let cached = CachedItem::new(
                &ItemContent::Text {
                    text: source.to_owned(),
                    thinking: false,
                    phase: chat::Phase::Final,
                },
                crate::settings::EditorSyntaxTheme::Auto,
            );
            assert!(matches!(
                cached.markdown.items(),
                [markdown::Item::Image { .. }]
            ));
            let settings = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE);
            let mut element = view(&cached, settings, target, None);
            let renderer = crate::app::tests::headless_renderer();
            let mut tree = Tree::new(element.as_widget());
            let viewport = Rectangle::with_size(Size::new(520.0, 1000.0));
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                mouse::Cursor::Unavailable,
            );
            let nodes: Vec<_> = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .collect();
            for text in [alt, title].into_iter().filter(|text| !text.is_empty()) {
                assert!(
                    nodes.iter().any(|node| node.node().value() == Some(text)),
                    "selectable image text missing: {text}"
                );
            }
            let button = nodes
                .iter()
                .find(|node| {
                    node.node().role() == Role::Button
                        && node.node().label() == Some("Open image source")
                })
                .expect("image source has a named accessible action");
            let bounds = button.node().bounds().unwrap();
            let cursor = mouse::Cursor::Available(Point::new(
                ((bounds.x0 + bounds.x1) / 2.0) as f32,
                ((bounds.y0 + bounds.y1) / 2.0) as f32,
            ));
            let mut messages = Vec::new();
            for event in [
                mouse::Event::ButtonPressed(mouse::Button::Left),
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ] {
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Mouse(event),
                    Layout::new(&node),
                    cursor,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
            }
            assert!(
                matches!(messages.as_slice(), [Message::ChatUi(Action::OpenDocument(found, path))]
                if *found == target && path == uri),
                "source action must retain its card/generation and exact URI"
            );
        }
    }

    const THEMES: [crate::settings::EditorSyntaxTheme; 2] = [
        crate::settings::EditorSyntaxTheme::Auto,
        crate::settings::EditorSyntaxTheme::SolarizedDark,
    ];

    /// Every heading level is told from body text by its size or its weight.
    #[test]
    fn every_heading_level_differs_from_a_paragraph() {
        use markdown::HeadingLevel;
        let settings = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE);
        for (level, size) in [
            (HeadingLevel::H1, settings.h1_size),
            (HeadingLevel::H2, settings.h2_size),
            (HeadingLevel::H3, settings.h3_size),
            (HeadingLevel::H4, settings.h4_size),
            (HeadingLevel::H5, settings.h5_size),
            (HeadingLevel::H6, settings.h6_size),
        ] {
            let style = heading_style(settings, level);
            assert!(
                size.0 > settings.text_size.0 || style.font.weight != settings.style.font.weight,
                "{level:?}"
            );
            // A heading keeps the family and the colours of the text around it.
            assert_eq!(style.font.family, settings.style.font.family);
            assert_eq!(style.link_color, settings.style.link_color);
        }
    }

    #[test]
    fn code_selection_preserves_source_line_endings_and_trailing_newlines() {
        for code in ["", "line", "line\n", "first\r\nsecond\r\n", "\n\n"] {
            for language in [None, Some("rust")] {
                let block =
                    CodeBlock::new(language, code, crate::settings::EditorSyntaxTheme::Auto);
                for dark in [false, true] {
                    let rendered = block
                        .spans(dark)
                        .iter()
                        .map(|span| span.text.as_ref())
                        .collect::<String>();
                    assert_eq!(rendered, code);
                }
            }
        }
    }

    /// A block highlighted the way it was before it could be continued: the
    /// whole text through [`crate::diff_view::highlight_snippet`], a palette
    /// at a time. The fonts are then named where [`line`] names them, which
    /// is the one thing it adds.
    fn reference(
        language: Option<&str>,
        code: &str,
        theme: crate::settings::EditorSyntaxTheme,
    ) -> [Vec<Span<'static, String>>; 2] {
        let syntax = language
            .filter(|_| code.len() <= CachedItem::HIGHLIGHT_LIMIT)
            .map(|language| crate::diff_view::highlight_snippet(code, language, theme));
        [0, 1].map(|index| {
            let mut spans = Vec::new();
            for (number, source) in code.split_inclusive('\n').enumerate() {
                let line = source
                    .strip_suffix("\r\n")
                    .or_else(|| source.strip_suffix('\n'))
                    .unwrap_or(source);
                let start = spans.len();
                match &syntax {
                    Some(lines) => spans.extend(
                        crate::diff_view::syntax_spans(
                            line,
                            lines[index].get(number).map_or(&[][..], Vec::as_slice),
                        )
                        .into_iter()
                        .map(|span| {
                            let mut owned = Span::new(span.text.into_owned());
                            owned.font = span.font;
                            owned.color = span.color;
                            owned
                        }),
                    ),
                    None => spans.push(Span::new(line.to_owned())),
                }
                if monospace::covers(line) {
                    for span in &mut spans[start..] {
                        span.font = Some(monospace::concrete(span.font.unwrap_or(CodeBlock::FONT)));
                    }
                }
                if source.len() > line.len() {
                    spans.push(Span::new(source[line.len()..].to_owned()));
                }
            }
            spans
        })
    }

    fn assert_draws(block: &CodeBlock, expected: &[Vec<Span<'static, String>>; 2], case: &str) {
        for dark in [false, true] {
            assert_eq!(
                block.spans(dark).as_ref(),
                expected[usize::from(dark)].as_slice(),
                "{case}, dark: {dark}"
            );
        }
    }

    /// Text whose highlighting depends on the lines before it: a comment and
    /// a string that each run over several lines, so a block continued from
    /// the wrong state colors what follows them wrongly.
    const STATEFUL: &str = "/* a comment\n   that runs on */ fn main() {\n    let text = \"a string\n        over two lines\";\n    // caf\u{e9} \u{2192} done\n\tlet tabbed = 1;\n}\n\n/* open";

    fn sources() -> Vec<(&'static str, String)> {
        vec![
            ("stateful", STATEFUL.to_owned()),
            ("crlf", STATEFUL.replace('\n', "\r\n")),
            ("no final newline", "fn main() {}\nlet x = 1;".to_owned()),
            ("blank lines", "\n\nfn a() {}\n\n\n".to_owned()),
            (
                "a very long line",
                format!(
                    "let long = \"{}\";\nlet next = {}1;\n",
                    "x".repeat(3_000),
                    "1 + ".repeat(300)
                ),
            ),
        ]
    }

    /// Whether a source is short enough to cut finely and check at every
    /// piece; an unfinished line is highlighted again for each piece of it.
    fn short(code: &str) -> bool {
        code.len() < 512
    }

    /// The ways of cutting `code` into the pieces a stream delivers: where a
    /// fixed sequence scatters the cuts, in frames of a set size, and for a
    /// short source a character at a time.
    fn splits(code: &str) -> Vec<Vec<usize>> {
        let boundaries: Vec<usize> = (1..=code.len())
            .filter(|end| code.is_char_boundary(*end))
            .collect();
        let every = |step: usize| -> Vec<usize> {
            boundaries
                .iter()
                .copied()
                .filter(|end| end % step == 0 || *end == code.len())
                .collect()
        };
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let sparse = if short(code) { 61 } else { 56 };
        let scattered = boundaries
            .iter()
            .copied()
            .filter(|end| {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                seed >> sparse == 0 || *end == code.len()
            })
            .collect();
        if short(code) {
            vec![boundaries.clone(), every(7), every(48), scattered]
        } else {
            vec![every(251), scattered]
        }
    }

    /// A fence streamed in any pieces is the fence highlighted in one go, in
    /// both palettes, at every point on the way.
    #[test]
    fn a_block_continued_in_pieces_draws_what_the_whole_block_does() {
        for theme in THEMES {
            for (name, code) in sources() {
                for language in [Some("rust"), Some("no-such-language"), None] {
                    for ends in splits(&code) {
                        let mut block = CodeBlock::open(language, "", theme);
                        for end in ends {
                            let prefix = &code[..end];
                            assert!(block.extend(language, prefix), "{name} at {end}");
                            if short(&code) || end == code.len() {
                                assert_draws(
                                    &block,
                                    &reference(language, prefix, theme),
                                    &format!("{name} {language:?} at {end}"),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Built in one go, a block is what it was before blocks could be
    /// continued.
    #[test]
    fn a_whole_block_draws_what_it_always_did() {
        for theme in THEMES {
            for (name, code) in sources() {
                for language in [Some("rust"), Some("sh"), None] {
                    assert_draws(
                        &CodeBlock::new(language, &code, theme),
                        &reference(language, &code, theme),
                        name,
                    );
                }
            }
        }
    }

    /// The last line of an open fence is whatever the stream has written of
    /// it, including the start of the fence that will close it. When the
    /// rest arrives that line leaves the block, which goes back to its last
    /// whole line rather than starting again.
    #[test]
    fn a_fence_that_closes_mid_piece_gives_back_its_unfinished_line() {
        let theme = crate::settings::EditorSyntaxTheme::Auto;
        let mut block = CodeBlock::open(Some("rust"), "let a = 1;\n/* open\n``", theme);
        assert!(block.extend(Some("rust"), "let a = 1;\n/* open\n"));
        assert_draws(
            &block,
            &reference(Some("rust"), "let a = 1;\n/* open\n", theme),
            "the closed fence",
        );
        assert!(block.extend(Some("rust"), "let a = 1;\n/* open\nstill */ b();\n"));
        assert_draws(
            &block,
            &reference(Some("rust"), "let a = 1;\n/* open\nstill */ b();\n", theme),
            "a fence that carried on instead",
        );
    }

    /// Anything other than more of the same fence is refused, and refusing
    /// leaves the block drawing what it drew.
    #[test]
    fn a_block_is_only_continued_by_its_own_text() {
        let theme = crate::settings::EditorSyntaxTheme::Auto;
        let code = "fn main() {\n    let x = 1;\n";
        let mut block = CodeBlock::open(Some("rust"), code, theme);
        for (language, other) in [
            (Some("rust"), "fn main() {\n    let y = 1;\n}\n"),
            (Some("rust"), "fn main() {\n"),
            (Some("python"), "fn main() {\n    let x = 1;\n}\n"),
            (None, "fn main() {\n    let x = 1;\n}\n"),
        ] {
            assert!(!block.extend(language, other), "{language:?} {other:?}");
            assert_draws(&block, &reference(Some("rust"), code, theme), "refused");
        }
        let mut settled = CodeBlock::new(Some("rust"), code, theme);
        assert!(!settled.extend(Some("rust"), "fn main() {\n    let x = 1;\n}\n"));
    }

    /// A fence that outgrows what is highlighted is drawn plain from its
    /// first line, as one that arrived that large is.
    #[test]
    fn a_block_that_outgrows_highlighting_starts_again_plain() {
        let theme = crate::settings::EditorSyntaxTheme::Auto;
        let line = "let value = cache.get(&key).map(|entry| entry.len());\n";
        let small = line.repeat(CachedItem::HIGHLIGHT_LIMIT / line.len());
        let large = format!("{small}{line}");
        assert!(small.len() <= CachedItem::HIGHLIGHT_LIMIT && large.len() > small.len());
        let mut block = CodeBlock::open(Some("rust"), &small[..small.len() / 2], theme);
        assert!(block.extend(Some("rust"), &small));
        assert!(block.spans(true).iter().any(|span| span.color.is_some()));
        assert!(!block.extend(Some("rust"), &large));
        let mut plain = CodeBlock::open(Some("rust"), &large, theme);
        assert_draws(
            &plain,
            &reference(Some("rust"), &large, theme),
            "over the limit",
        );
        assert!(plain.spans(true).iter().all(|span| span.color.is_none()));
        let larger = format!("{large}{line}let tail");
        assert!(plain.extend(Some("rust"), &larger));
        assert_draws(
            &plain,
            &reference(Some("rust"), &larger, theme),
            "and growing",
        );
    }
}
