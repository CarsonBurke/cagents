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
/// count rather than a pass over the block's lines.
pub(super) struct CodeBlock {
    spans: [Arc<[Span<'static, String>]>; 2],
}

impl CodeBlock {
    pub(super) fn new(
        language: Option<&str>,
        code: &str,
        theme: crate::settings::EditorSyntaxTheme,
    ) -> Self {
        let syntax = language
            .filter(|_| code.len() <= CachedItem::HIGHLIGHT_LIMIT)
            .map(|language| crate::diff_view::highlight_snippet(code, language, theme));
        let palette = |index: usize| -> Arc<[Span<'static, String>]> {
            let mut spans = Vec::new();
            for (number, source) in code.split_inclusive('\n').enumerate() {
                let (line, ending) = if let Some(line) = source.strip_suffix("\r\n") {
                    (line, "\r\n")
                } else if let Some(line) = source.strip_suffix('\n') {
                    (line, "\n")
                } else {
                    (source, "")
                };
                match &syntax {
                    Some(lines) => spans.extend(
                        crate::diff_view::syntax_spans(
                            line,
                            lines[index].get(number).map_or(&[][..], Vec::as_slice),
                        )
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
                if !ending.is_empty() {
                    spans.push(Span::new(ending));
                }
            }
            spans.into()
        };
        let light = palette(0);
        let dark = if syntax.is_some() {
            palette(1)
        } else {
            light.clone()
        };
        Self {
            spans: [light, dark],
        }
    }

    /// The block's spans in the palette the current appearance uses.
    pub(super) fn spans(&self, dark: bool) -> &Arc<[Span<'static, String>]> {
        &self.spans[usize::from(dark)]
    }
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
            Item::Image { alt, .. } => widget::container(selectable_prose::text(
                alt.spans(settings.style),
                settings.text_size,
                iced::Font::default(),
                self.target,
                self.find,
            ))
            .padding(settings.spacing.0)
            .class(design::surface(
                Some(design::sunken),
                [design::RADIUS_S; 4],
                true,
            ))
            .into(),
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
            settings.style.code_block_font,
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
            Some(language.unwrap_or("Code").to_owned()),
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
        text.spans(settings.style),
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
}
