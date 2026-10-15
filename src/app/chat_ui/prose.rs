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
    /// Owned span storage and logical text bytes. Plain palettes share their
    /// Arc; count that backing only once. Highlighter state is not estimated.
    pub(super) fn retained_span_bytes(&self) -> usize {
        let bytes = |spans: &Arc<[Span<'static, String>]>| {
            spans.len() * std::mem::size_of::<Span<'static, String>>()
                + spans
                    .iter()
                    .map(|span| {
                        let text: &str = span.text.as_ref();
                        text.len()
                    })
                    .sum::<usize>()
        };
        bytes(&self.spans[0])
            + if Arc::ptr_eq(&self.spans[0], &self.spans[1]) {
                0
            } else {
                bytes(&self.spans[1])
            }
    }


    /// Share immutable prepared palettes across worker snapshots. Streaming
    /// highlighter continuations are never cloned or transferred this way.
    pub(super) fn settled_clone(&self) -> Self {
        Self { spans: self.spans.clone(), tail: None }
    }

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
    if cached.placeholder {
        return widget::text::caption("Loading message…").into();
    }
    Renderer {
        target,
        blocks: cached
            .blocks
            .iter()
            .map(|(key, block)| (*key, block.spans(cosmic::theme::is_dark()).clone()))
            .collect::<HashMap<_, _>>()
            .into(),
        revision: cached.revision,
        flow_cache: cached.flow_cache.clone(),
        reader_memory: cached.reader_memory.clone(), reader_context: cached.reader_context.clone(),
        item: cached.item.as_ref().map_or_else(|| format!("unbound-{}", cached.revision), |item| item.id.clone()).into(),
        path: vec![0],
        dark: cosmic::theme::is_dark(),
        find: find.map(Arc::from),
        paint: cached.find_paint.clone(),
    }
    .items(cached.markdown.items(), settings)
}

/// Markdown the item carries beside its own body, such as a message an
/// agent sent, drawn as the body is and with its fences highlighted from the
/// item's cache.
pub(super) fn content_view(
    cached: &CachedItem,
    content: &markdown::Content,
    settings: markdown::Settings,
    target: Target,
    find: Option<&str>,
) -> Element<'static, Message> {
    Renderer {
        target,
        blocks: cached
            .blocks
            .iter()
            .map(|(key, block)| (*key, block.spans(cosmic::theme::is_dark()).clone()))
            .collect::<HashMap<_, _>>()
            .into(),
        revision: cached.revision,
        flow_cache: cached.flow_cache.clone(),
        reader_memory: cached.reader_memory.clone(), reader_context: cached.reader_context.clone(),
        item: cached.item.as_ref().map_or_else(|| format!("unbound-{}", cached.revision), |item| item.id.clone()).into(),
        path: vec![1, cached.coordination.as_ref().and_then(|coordinated| coordinated.contents().position(|candidate| std::ptr::eq(candidate, content))).unwrap_or_default() as u32],
        dark: cosmic::theme::is_dark(),
        find: find.map(Arc::from),
        paint: cached.find_paint.clone(),
    }
    .items(content.items(), settings)
}

#[cfg(test)]
pub(super) fn items_for_test(
    cached: &CachedItem,
    items: &[markdown::Item],
    target: Target,
) -> Element<'static, Message> {
    Renderer {
        target,
        blocks: cached
            .blocks
            .iter()
            .map(|(key, block)| (*key, block.spans(cosmic::theme::is_dark()).clone()))
            .collect::<HashMap<_, _>>()
            .into(),
        revision: cached.revision,
        flow_cache: cached.flow_cache.clone(),
        reader_memory: cached.reader_memory.clone(), reader_context: cached.reader_context.clone(),
        item: cached.item.as_ref().map_or_else(|| format!("unbound-{}", cached.revision), |item| item.id.clone()).into(),
        path: vec![0],
        dark: true,
        find: None,
        paint: None,
    }
    .items(
        items,
        design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE),
    )
}

#[derive(Clone)]
struct Renderer {
    flow_cache: flow_layout::Cache,
    reader_memory: scroll::Memory,
    reader_context: Option<String>,
    item: Arc<str>,
    path: Vec<u32>,
    paint: Option<Arc<find::Paint>>,
    find: Option<Arc<str>>,
    target: Target,
    blocks: Arc<HashMap<u64, Arc<[Span<'static, String>]>>>,
    revision: u64,
    dark: bool,
}

impl Renderer {
    /// Describe logical leaves without plain-text extraction, styling, widget
    /// creation or shaping. Composite ancestors expose these aliases so Find
    /// can realize the correct nested row before measuring its exact glyph.
    fn logical_descendants(&self, items: &[markdown::Item]) -> Vec<virtualized::LogicalRange> {
        let mut leaves = Vec::new();
        for (index, item) in items.iter().enumerate() {
            self.child(index as u32).describe(item, &mut leaves);
        }
        leaves
    }
    fn describe(&self, item: &markdown::Item, leaves: &mut Vec<virtualized::LogicalRange>) {
        let length = match item {
            markdown::Item::Heading(_, text) | markdown::Item::Paragraph(text) => Some(text.len()),
            markdown::Item::CodeBlock { code, .. } => Some(code.len()),
            _ => None,
        };
        if let Some(length) = length {
            leaves.push(virtualized::LogicalRange {
                identity: selection::logical_identity(&self.item, &self.path),
                bytes: 0..length,
                chunk: false,
            });
            return;
        }
        match item {
            markdown::Item::Quote(items) => leaves.extend(self.logical_descendants(items)),
            markdown::Item::List { bullets, .. } => {
                for (index, bullet) in bullets.iter().enumerate() {
                    let (markdown::Bullet::Point { items } | markdown::Bullet::Task { items, .. }) =
                        bullet;
                    leaves.extend(self.child(index as u32).logical_descendants(items));
                }
            }
            markdown::Item::Table { columns, rows } => {
                for (column, header) in columns.iter().enumerate() {
                    leaves.extend(
                        self.child(0)
                            .child(column as u32)
                            .logical_descendants(&header.header),
                    );
                }
                for (row, cells) in rows.iter().enumerate() {
                    for (column, cells) in cells.cells.iter().enumerate() {
                        leaves.extend(
                            self.child(row as u32 + 1)
                                .child(column as u32)
                                .logical_descendants(cells),
                        );
                    }
                }
            }
            markdown::Item::Image { alt, title, .. } => {
                if alt.len() > 0 {
                    leaves.push(virtualized::LogicalRange {
                        identity: selection::logical_identity(&self.item, &self.child(0).path),
                        bytes: 0..alt.len(),
                        chunk: false,
                    });
                }
                if !title.is_empty() {
                    leaves.push(virtualized::LogicalRange {
                        identity: selection::logical_identity(&self.item, &self.child(1).path),
                        bytes: 0..title.len(),
                        chunk: false,
                    });
                }
            }
            _ => {}
        }
    }
    fn child(&self, index: u32) -> Self {
        let mut path = self.path.clone(); path.push(index);
        Self { paint: self.paint.clone(), flow_cache: self.flow_cache.clone(), item: self.item.clone(), path, find: self.find.clone(), target: self.target,
            reader_memory: self.reader_memory.clone(), reader_context: self.reader_context.clone(),
            blocks: self.blocks.clone(), revision: self.revision, dark: self.dark }
    }
    fn logical(&self, _text: Arc<str>, start: usize) -> selectable_prose::Logical { self.logical_identity(start) }
    fn logical_identity(&self, start: usize) -> selectable_prose::Logical {
        selectable_prose::Logical { paint: self.paint.clone(), identity: selection::logical_identity(&self.item, &self.path), start }
    }
    fn items(
        &self,
        items: &[markdown::Item],
        settings: markdown::Settings,
    ) -> Element<'static, Message> {
        if items.len() <= 4 {
            return widget::column::with_children(
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| self.child(index as u32).item(item, index, settings))
                    .collect::<Vec<_>>(),
            )
            .spacing(settings.spacing.0)
            .into();
        }
        let rows = items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                use std::hash::{Hash, Hasher};
                let renderer = self.child(index as u32);
                let key = selection::logical_identity(&renderer.item, &renderer.path);
                let mut revision = std::hash::DefaultHasher::new();
                (
                    self.revision,
                    settings.text_size.0.to_bits(),
                    settings.style.font,
                    &self.find,
                    self.paint.as_ref().map(|paint| {
                        (
                            paint.generation,
                            paint
                                .current
                                .as_ref()
                                .map(|found| (found.identity, found.range.start)),
                        )
                    }),
                    self.dark,
                )
                    .hash(&mut revision);
                let revision = revision.finish();
                let estimate = match item {
                    markdown::Item::Paragraph(text) | markdown::Item::Heading(_, text) => {
                        (text.len() as f32 * settings.text_size.0 * 0.5 / design::READABLE_MEASURE)
                            .ceil()
                            .max(1.0)
                            * settings.text_size.0
                            * 1.3
                    }
                    markdown::Item::CodeBlock { code, .. } => {
                        (code.bytes().filter(|byte| *byte == b'\n').count().max(1) as f32)
                            * settings.code_size.0
                            * 1.3
                            + 48.0
                    }
                    _ => 80.0,
                } + if index == 0 { 0.0 } else { settings.spacing.0 };
                let logical_len = match item {
                    markdown::Item::Paragraph(text) | markdown::Item::Heading(_, text) => {
                        Some(text.len())
                    }
                    markdown::Item::CodeBlock { code, .. } => Some(code.len()),
                    _ => None,
                };
                let descendants = if logical_len.is_none() {
                    let mut leaves = Vec::new();
                    renderer.describe(item, &mut leaves);
                    leaves
                } else {
                    vec![]
                };
                let item = item.clone();
                let mut row = virtualized::Row::new(key, revision, estimate, None, move || {
                    let renderer = renderer.clone();
                    let item = item.clone();
                    retained::kept(revision, move || {
                        widget::container(renderer.item(&item, index, settings))
                            .padding(iced::Padding::ZERO.top(if index == 0 {
                                0.0
                            } else {
                                settings.spacing.0
                            }))
                            .into()
                    })
                });
                row.anchor = true;
                row.descendants = descendants;
                row.logical = logical_len.map(|len| virtualized::LogicalRange {
                    identity: key,
                    bytes: 0..len,
                    chunk: false,
                });
                row
            })
            .collect();
        virtualized::column(rows, Default::default())
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
                heading(settings, *level, text, index, self.target, self.find.as_deref(), Some(self.logical_identity(0)), Some(&self.flow_cache))
            }
            Item::Paragraph(text) => paragraph(settings, text, self.target, self.find.as_deref(), Some(self.logical_identity(0)), Some(&self.flow_cache)),
            Item::CodeBlock { language, code, reader_source, .. } => {
                self.code_block(settings, language.as_deref(), code, reader_source)
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
                    rows.push(leaf_text(
                        alt,
                        settings.text_size,
                        iced::Font::default(),
                        self.target,
                        self.find.as_deref(),
                        Some(self.child(0).logical(Arc::from(""), 0)),
                    ));
                }
                if !title.is_empty() {
                    rows.push(leaf_text(
                        vec![Span::new(title.clone())].into(),
                        design::TEXT_CAPTION.into(),
                        iced::Font::default(),
                        self.target,
                        self.find.as_deref(),
                        Some(self.child(1).logical(Arc::from(""), 0)),
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
        let rows = bullets
            .iter()
            .enumerate()
            .map(|(index, bullet)| {
                let renderer = self.clone();
                let (markdown::Bullet::Point { items } | markdown::Bullet::Task { items, .. }) =
                    bullet;
                let descendants = self.child(index as u32).logical_descendants(items);
                let bullet = bullet.clone();
                let key = selection::logical_identity(&self.item, &self.child(index as u32).path);
                let mut descriptor = virtualized::Row::new(
                    key,
                    self.revision,
                    64.0 + settings.spacing.0 * 0.75,
                    None,
                    move || {
                        let (marker, items): (Element<'static, Message>, _) = match (&bullet, start)
                        {
                            (markdown::Bullet::Point { items }, Some(start))
                            | (markdown::Bullet::Task { items, .. }, Some(start)) => (
                                iced::widget::text(format!("{}.", index as u64 + start))
                                    .size(settings.text_size)
                                    .align_x(iced::alignment::Horizontal::Right)
                                    .width(
                                        settings.text_size
                                            * ((digits.unwrap_or(1.0) / 2.0).ceil() + 1.0),
                                    )
                                    .into(),
                                items,
                            ),
                            (markdown::Bullet::Point { items }, None) => (
                                iced::widget::text("•").size(settings.text_size).into(),
                                items,
                            ),
                            (markdown::Bullet::Task { items, done }, None) => (
                                widget::container(
                                    iced::widget::checkbox(*done).size(settings.text_size),
                                )
                                .center_y(
                                    iced::widget::text::LineHeight::default()
                                        .to_absolute(settings.text_size),
                                )
                                .into(),
                                items,
                            ),
                        };
                        let mut prefix = start.map_or_else(
                            || "-".to_owned(),
                            |start| format!("{}.", index as u64 + start),
                        );
                        if let markdown::Bullet::Task { done, .. } = &bullet {
                            prefix.push_str(if *done { " [x]" } else { " [ ]" });
                        }
                        widget::container(
                            iced::widget::row![
                                marker,
                                selection::wrap(
                                    renderer.child(index as u32).items(items, nested),
                                    selection::Kind::ListItem(prefix)
                                )
                            ]
                            .spacing(settings.spacing.0),
                        )
                        .padding(iced::Padding::ZERO.bottom(settings.spacing.0 * 0.75))
                        .into()
                    },
                );
                descriptor.descendants = descendants;
                descriptor.anchor = true;
                descriptor
            })
            .collect();
        widget::container(virtualized::column(rows, Default::default()))
            .padding(if start.is_none() {
                iced::Padding::ZERO
                    .left(settings.spacing.0)
                    .right(settings.spacing.0)
            } else {
                iced::Padding::ZERO
            })
            .into()
    }

    fn table(
        &self,
        settings: markdown::Settings,
        columns: &[markdown::Column],
        rows: &[markdown::Row],
    ) -> Element<'static, Message> {
        // Stable column widths let independently mounted rows agree without
        // shaping every hidden cell to discover its natural width.
        let width = 240.0;
        let alignments: Arc<[_]> = columns.iter().map(alignment).collect::<Vec<_>>().into();
        let header = columns
            .iter()
            .map(|column| column.header.clone())
            .collect::<Vec<_>>();
        let descriptors = std::iter::once(header)
            .chain(rows.iter().map(|row| row.cells.clone()))
            .enumerate()
            .map(|(row, cells)| {
                let renderer = self.child(row as u32);
                let key = selection::logical_identity(&self.item, &renderer.path);
                let descendants = cells
                    .iter()
                    .enumerate()
                    .flat_map(|(column, cells)| {
                        renderer.child(column as u32).logical_descendants(cells)
                    })
                    .collect();
                let alignments = alignments.clone();
                let mut descriptor = virtualized::Row::new(
                    key,
                    self.revision,
                    48.0 + settings.spacing.0,
                    None,
                    move || {
                        let descriptors = alignments
                            .iter()
                            .enumerate()
                            .map(|(column, alignment)| {
                                let renderer = renderer.child(column as u32);
                                let key =
                                    selection::logical_identity(&renderer.item, &renderer.path);
                                let alignment = *alignment;
                                let cells = cells.get(column).cloned().unwrap_or_default();
                                let mut descendants = renderer.logical_descendants(&cells);
                                // Horizontal estimates can realize a far-away cell
                                // before an exact glyph exists in the mounted tree.
                                for logical in &mut descendants {
                                    logical.chunk = true;
                                }
                                let mut descriptor = virtualized::Row::new(
                                    key,
                                    renderer.revision,
                                    width,
                                    None,
                                    move || {
                                        let content = renderer.items(&cells, settings);
                                        widget::container(selection::wrap(
                                            content,
                                            selection::Kind::Cell { row, column },
                                        ))
                                        .width(width)
                                        .align_x(alignment)
                                        .padding([settings.spacing.0 / 2.0, settings.spacing.0])
                                        .into()
                                    },
                                );
                                descriptor.descendants = descendants;
                                descriptor
                            })
                            .collect();
                        virtualized::row(descriptors, 48.0 + settings.spacing.0, Default::default())
                    },
                );
                descriptor.descendants = descendants;
                descriptor.anchor = true;
                descriptor
            })
            .collect();
        let table = widget::container(virtualized::column(descriptors, Default::default()))
            .width(width * columns.len().max(1) as f32);
        scroll::block(
            iced::widget::scrollable(selection::wrap(
                table,
                selection::Kind::Table {
                    columns: columns.len(),
                    rows: rows.len() + 1,
                },
            ))
            .direction(iced::widget::scrollable::Direction::Horizontal(
                design::scrollbar(),
            ))
            .id(scroll::block_id(
                self.target,
                self.reader_context.as_deref(),
                selection::logical_identity(&self.item, &self.path),
            ))
            .spacing(settings.spacing.0 / 2.0),
            self.reader_memory.clone(),
            self.reader_context.clone(),
            selection::logical_identity(&self.item, &self.path),
            false,
        )
    }

    /// A fenced block on the sunken surface commands and outputs use, drawn
    /// from its cached spans in the palette the editor's syntax theme chose
    /// for the current appearance.
    fn code_block(
        &self,
        settings: markdown::Settings,
        language: Option<&str>,
        code: &str,
        reader_source: &super::reader_source::Source,
    ) -> Element<'static, Message> {
        let spans = (code.len() <= CachedItem::HIGHLIGHT_LIMIT)
            .then(|| {
                self.blocks
                    .get(&fence_key(language.unwrap_or_default(), code))
            })
            .flatten()
            .cloned()
            .unwrap_or_else(|| {
                if code.len() > 2048 {
                    Arc::from([])
                } else {
                    vec![Span::new(code.to_owned())].into()
                }
            });
        let text = code_text(
            Arc::from(code),
            spans,
            settings.code_size,
            CodeBlock::FONT,
            self.target,
            self.find.as_deref(),
            Some(self.logical(Arc::from(""), 0)),
        );
        step::code_frame(
            scroll::block_source(widget::scrollable(
                widget::container(text).padding([step::GAP_S as u16, step::GAP_M as u16])
                    .min_width(self.reader_memory.reader_width_for(self.reader_context.as_deref(),
                        selection::logical_identity(&self.item, &self.path), reader_source)),
            )
            .direction(iced::widget::scrollable::Direction::Horizontal(
                design::scrollbar(),
            )).id(scroll::block_id(self.target, self.reader_context.as_deref(), selection::logical_identity(&self.item, &self.path))), self.reader_memory.clone(), self.reader_context.clone(),
                selection::logical_identity(&self.item, &self.path), false, Some(reader_source.clone())),
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
    logical: Option<selectable_prose::Logical>,
    flow_cache: Option<&flow_layout::Cache>,
) -> Element<'static, Message> {
    use markdown::HeadingLevel;
    widget::container(paragraph(markdown::Settings {
        text_size: match level {
            HeadingLevel::H1 => settings.h1_size,
            HeadingLevel::H2 => settings.h2_size,
            HeadingLevel::H3 => settings.h3_size,
            HeadingLevel::H4 => settings.h4_size,
            HeadingLevel::H5 => settings.h5_size,
            HeadingLevel::H6 => settings.h6_size,
        }, style: heading_style(settings, level), ..settings
    }, text, target, find, logical, flow_cache))
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

fn leaf_text(
    spans: Arc<[Span<'static, String>]>,
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
    find: Option<&str>,
    logical: Option<selectable_prose::Logical>,
) -> Element<'static, Message> {
    if let Some(logical) = logical {
        selectable_prose::logical_text(spans, size, font, target, find, logical)
    } else {
        selectable_prose::text(spans, size, font, target, find)
    }
}

/// Read-only code shares one source, and only styles/shapes bounded mounted
/// ranges. The logical block keeps source newlines intact for selection/copy.
pub(super) fn code_text(
    text: Arc<str>,
    spans: Arc<[Span<'static, String>]>,
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
    find: Option<&str>,
    logical: Option<selectable_prose::Logical>,
) -> Element<'static, Message> {
    if text.len() <= 2048 {
        let spans = if spans.is_empty() {
            vec![Span::new(text.to_string())].into()
        } else {
            spans
        };
        return leaf_text(spans, size, font, target, find, logical);
    }
    let mut starts = Vec::with_capacity(spans.len() + 1);
    starts.push(0usize);
    for span in spans.iter() {
        starts.push(starts.last().copied().unwrap() + span.text.len());
    }
    let starts: Arc<[usize]> = starts.into();
    let find: Option<Arc<str>> = find.map(Arc::from);
    let mut ranges = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        ranges.push(start..start + line.len());
        start += line.len();
    }
    if text.ends_with('\n') {
        ranges.push(text.len()..text.len());
    }
    let rows = ranges
        .into_iter()
        .map(|range| {
            use std::hash::{Hash, Hasher};
            let mut key = std::hash::DefaultHasher::new();
            (
                logical.as_ref().map(|logical| logical.identity),
                range.start,
            )
                .hash(&mut key);
            let key = key.finish();
            let mut revision = std::hash::DefaultHasher::new();
            (
                Arc::as_ptr(&text) as *const u8 as usize,
                &range,
                &find,
                logical.as_ref().and_then(|l| l.paint.as_ref()).map(|p| {
                    (
                        p.generation,
                        p.current.as_ref().map(|m| (m.identity, m.range.start)),
                    )
                }),
                font,
                size.0.to_bits(),
            )
                .hash(&mut revision);
            let revision = revision.finish();
            let text = text.clone();
            let spans = spans.clone();
            let starts = starts.clone();
            let find = find.clone();
            let descriptor = logical.as_ref().map(|logical| virtualized::LogicalRange {
                identity: logical.identity,
                bytes: logical.start + range.start..logical.start + range.end,
                chunk: false,
            });
            let logical = logical.clone().map(|mut logical| {
                logical.start += range.start;
                logical
            });
            let mut row = virtualized::Row::new(key, revision, size.0 * 1.3, None, move || {
                // Newlines belong to the logical document. Each physical line
                // contributes one visual row, including the real terminal empty
                // line, instead of an extra empty paragraph at every chunk seam.
                let mut display = range.clone();
                if text[display.clone()].ends_with('\n') {
                    display.end -= 1;
                    if text[display.clone()].ends_with('\r') {
                        display.end -= 1;
                    }
                }
                if display.is_empty() {
                    return widget::Space::new().height(size.0 * 1.3).into();
                }
                code_line(
                    text.clone(),
                    spans.clone(),
                    starts.clone(),
                    display,
                    size,
                    font,
                    target,
                    find.clone(),
                    logical.clone(),
                )
            });
            row.anchor = true;
            row.logical = descriptor;
            row
        })
        .collect();
    virtualized::column(rows, Default::default())
}

fn code_line(
    text: Arc<str>,
    spans: Arc<[Span<'static, String>]>,
    starts: Arc<[usize]>,
    range: std::ops::Range<usize>,
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
    find: Option<Arc<str>>,
    logical: Option<selectable_prose::Logical>,
) -> Element<'static, Message> {
    // The pinned shaping API has no context-aware horizontal range layout.
    // Preserve physical-line Bidi, joining, clusters and tab origin rather
    // than independently shaping a complex run at artificial boundaries.
    // These lines remain an explicit performance qualification requirement.
    let pieces = if range.len() <= 2048 || !text[range.clone()].is_ascii() || text[range.clone()].contains('\t') {
        vec![0..range.len()]
    } else {
        chunk_ranges(&text[range.clone()])
    };
    let chunks = pieces
        .into_iter()
        .map(|piece| {
            use std::hash::{Hash, Hasher};
            let source = range.start + piece.start..range.start + piece.end;
            let mut hash = std::hash::DefaultHasher::new();
            (
                logical.as_ref().map(|logical| logical.identity),
                source.start,
            )
                .hash(&mut hash);
            let key = hash.finish();
            (
                Arc::as_ptr(&text) as *const u8 as usize,
                &source,
                &find,
                logical.as_ref().and_then(|l| l.paint.as_ref()).map(|p| {
                    (
                        p.generation,
                        p.current.as_ref().map(|m| (m.identity, m.range.start)),
                    )
                }),
                font,
                size.0.to_bits(),
            )
                .hash(&mut hash);
            let revision = hash.finish();
            let estimate =
                unicode_width::UnicodeWidthStr::width(&text[source.clone()]) as f32 * size.0 * 0.6;
            let descriptor = logical.as_ref().map(|logical| virtualized::LogicalRange {
                identity: logical.identity,
                bytes: logical.start + piece.start..logical.start + piece.end,
                chunk: true,
            });
            let logical = logical.clone().map(|mut logical| {
                logical.start += piece.start;
                logical
            });
            let text = text.clone();
            let spans = spans.clone();
            let starts = starts.clone();
            let find = find.clone();
            let mut row = virtualized::Row::new(key, revision, estimate, None, move || {
                let text = text.clone();
                let spans = spans.clone();
                let starts = starts.clone();
                let find = find.clone();
                let logical = logical.clone();
                let source = source.clone();
                retained::kept(revision, move || {
                    let styled = if spans.is_empty() {
                        vec![Span::new(text[source.clone()].to_owned())].into()
                    } else {
                        sliced_spans(&spans, &starts, source.clone())
                    };
                    leaf_text(styled, size, font, target, find.as_deref(), logical.clone())
                })
            });
            row.logical = descriptor;
            row
        })
        .collect();


    virtualized::row(chunks, size.0 * 1.3, Default::default())
}

fn sliced_spans(
    spans: &[Span<'static, String>],
    starts: &[usize],
    range: std::ops::Range<usize>,
) -> Arc<[Span<'static, String>]> {
    let first = starts
        .partition_point(|start| *start <= range.start)
        .saturating_sub(1);
    spans
        .iter()
        .enumerate()
        .skip(first)
        .take_while(|(index, _)| starts[*index] < range.end)
        .filter_map(|(index, span)| {
            let start = range.start.saturating_sub(starts[index]);
            let end = range.end.saturating_sub(starts[index]).min(span.text.len());
            (start < end).then(|| Span {
                text: span.text[start..end].to_owned().into(),
                size: span.size,
                line_height: span.line_height,
                font: span.font,
                color: span.color,
                link: span.link.clone(),
                highlight: span.highlight,
                padding: span.padding,
                underline: span.underline,
                strikethrough: span.strikethrough,
            })
        })
        .collect()
}

/// A bounded visual chunk. Copy and selection keep the original logical
/// paragraph, so splitting here introduces no whitespace into clipboard text.
pub(super) fn chunk_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    const BYTES: usize = 2048;
    const LINES: usize = 40;
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + BYTES).min(text.len());
        use unicode_segmentation::UnicodeSegmentation;
        let boundary = text[start..]
            .grapheme_indices(true)
            .map(|(at, _)| start + at)
            .chain(std::iter::once(text.len()))
            .take_while(|at| *at <= end)
            .last()
            .unwrap_or(start);
        if boundary > start {
            end = boundary;
        } else {
            end = start + text[start..].graphemes(true).next().map_or(0, str::len);
        }
        if let Some((line, _)) = text[start..end].match_indices('\n').nth(LINES - 1) {
            end = start + line + 1;
        }
        // Prefer a source whitespace boundary near the cap. A giant token is
        // still bounded and wraps with the same glyph fallback as short prose.
        if end < text.len() && let Some((at, grapheme)) = text[start..end].grapheme_indices(true).rev().find(|(_, grapheme)| grapheme.chars().all(char::is_whitespace)) {
            let boundary = start + at + grapheme.len();
            if boundary >= start + BYTES / 2 { end = boundary; }
        }
        ranges.push(start..end);
        start = end;
    }
    ranges
}

fn paragraph(
    settings: markdown::Settings,
    text: &markdown::Text,
    target: Target,
    find: Option<&str>,
    logical: Option<selectable_prose::Logical>,
    flow_cache: Option<&flow_layout::Cache>,
) -> Element<'static, Message> {
    if text.len() > 2048
        && let Some(cache) = flow_cache
        && let Some(logical) = logical.clone()
    {
        let flow = cache.markdown_entry(logical.identity, text.clone(), settings.style);
        return selectable_prose::flow_source(settings.text_size, settings.style.font, target, find, logical, flow);
    }
    leaf_text(text.spans(settings.style), settings.text_size, settings.style.font, target, find, logical)
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
    fn indexed_find_match_crossing_two_mounted_code_chunks_paints_both_halves() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree},
        };
        let renderer = crate::app::tests::headless_renderer();
        let identity = 7;
        let found = find::Match {
            turn: "t".into(),
            item: "i".into(),
            identity,
            range: 2046..2052,
        };
        let paint = Arc::new(find::Paint::new(1, &[found.clone()], Some(found)));
        let source: Arc<str> = format!("{}needle{}", "a".repeat(2046), "z".repeat(2048)).into();
        let mut element = code_text(
            source,
            Arc::from([]),
            design::TEXT_BODY.into(),
            iced::Font::MONOSPACE,
            Target {
                card: CardId::new(),
                generation: 0,
            },
            Some("needle"),
            Some(selectable_prose::Logical {
                identity,
                start: 0,
                paint: Some(paint),
            }),
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(720.0, 12000.0)),
        );
        #[derive(Default)]
        struct Painted(
            Vec<(
                usize,
                Vec<std::ops::Range<usize>>,
                Option<std::ops::Range<usize>>,
            )>,
        );
        impl Operation for Painted {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn custom(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(prose) = state.downcast_ref::<selectable_prose::State>()
                    && let Some(logical) = &prose.logical
                    && !prose.search_paint.0.is_empty()
                {
                    self.0.push((
                        logical.start,
                        prose.search_paint.0.clone(),
                        prose.search_paint.1.clone(),
                    ));
                }
            }
        }
        let mut painted = Painted::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut painted);
        painted.0.sort_by_key(|(start, _, _)| *start);
        assert_eq!(
            painted.0,
            vec![
                (0, vec![2046..2048], Some(2046..2048)),
                (2048, vec![0..4], Some(0..4))
            ],
            "one indexed occurrence keeps ordinary and current paint on both actual mounted fragments"
        );
    }

    #[test]
    fn huge_inline_paragraph_does_not_materialize_presentation_on_ui_layout() {
        use iced::advanced::{layout, widget::Tree};
        let source = "**bold** ordinary *emphasis* [link](https://example.com) `code` ".repeat(4000);
        let content = markdown::Content::parse(&source);
        let markdown::Item::Paragraph(text) = &content.items()[0] else { panic!("paragraph fixture") };
        let before = markdown::materialized_bytes();
        let cache = flow_layout::Cache::default();
        let mut element = paragraph(
            design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE), text,
            Target { card: CardId::new(), generation: 0 }, None,
            Some(selectable_prose::Logical { identity: 7, start: 0 }), Some(&cache));
        let mut tree = Tree::new(&element);
        element.as_widget_mut().layout(&mut tree, &crate::app::tests::headless_renderer(),
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(720.0, f32::INFINITY)));
        assert_eq!(markdown::materialized_bytes(), before,
            "UI realization must not copy full plain text or style thousands of inline spans");
    }

    #[test]
    fn giant_single_list_table_and_heading_keep_only_viewport_widget_trees() {
        use iced::advanced::{layout, widget::Tree};
        fn residents(tree: &Tree) -> (usize, usize) {
            let prose = usize::from(
                tree.tag == iced::advanced::widget::tree::Tag::of::<selectable_prose::State>(),
            );
            tree.children
                .iter()
                .fold((1, prose), |(widgets, prose), child| {
                    let (children, leaves) = residents(child);
                    (widgets + children, prose + leaves)
                })
        }
        let renderer = crate::app::tests::headless_renderer();
        let sources = [
            "- list row with meaningful text\n".repeat(20_000),
            format!(
                "| First | Second |\n| --- | --- |\n{}",
                "| table cell | other cell |\n".repeat(20_000)
            ),
            format!("# {}", "heading-word ".repeat(40_000)),
        ];
        for source in sources {
            let cached = CachedItem::new(
                &ItemContent::Text {
                    text: source,
                    thinking: false,
                    phase: chat::Phase::Final,
                },
                crate::settings::EditorSyntaxTheme::Auto,
            );
            assert_eq!(
                cached.markdown.items().len(),
                1,
                "adversarial single top-level item"
            );
            let mut element = view(
                &cached,
                design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE),
                Target {
                    card: CardId::new(),
                    generation: 0,
                },
                None,
            );
            let mut tree = Tree::new(&element);
            element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(720.0, f32::INFINITY)),
            );
            let (widgets, prose) = residents(&tree);
            assert!(
                prose < 100,
                "hidden leaf shaping escaped viewport bound: {prose}"
            );
            assert!(
                widgets < 2000,
                "hidden widget construction escaped viewport bound: {widgets}"
            );
        }
    }

    #[test]
    fn physical_code_lines_keep_height_and_horizontal_extent() {
        use iced::advanced::{layout, widget::Tree};
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let size: iced::Pixels = 14.0.into();
        let limits = layout::Limits::new(
            iced::Size::ZERO,
            iced::Size::new(f32::INFINITY, f32::INFINITY),
        );
        for source in [
            "a".repeat(6000),
            format!("{}e\u{301}{}", "a".repeat(2047), "z".repeat(4000)),
            format!("{}👩‍💻{}", "a".repeat(2047), "z".repeat(4000)),
            format!("{}\t{}", "a".repeat(2047), "z".repeat(4000)),
            format!("{}العربية עברית{}", "a".repeat(2047), "z".repeat(4000)),
            "line\n".repeat(1000),
            "first\n\nthird\n".repeat(400),
            "first\r\nsecond\r\n".repeat(400),
        ] {
            let mut reference = leaf_text(
                vec![Span::new(source.clone())].into(),
                size,
                CodeBlock::FONT,
                target,
                None,
                None,
            );
            let mut reference_tree = Tree::new(&reference);
            let reference_node =
                reference
                    .as_widget_mut()
                    .layout(&mut reference_tree, &renderer, &limits);
            let mut actual = code_text(
                Arc::from(source.as_str()),
                Arc::from([]),
                size,
                CodeBlock::FONT,
                target,
                None,
                None,
            );
            let mut tree = Tree::new(&actual);
            let node = actual.as_widget_mut().layout(&mut tree, &renderer, &limits);
            assert!(
                (node.size().height - reference_node.size().height).abs() < 1.0,
                "physical line count changed: {:?} vs {:?}",
                node.size(),
                reference_node.size()
            );
            if !source.contains('\n') {
                assert!(
                    node.size().width > 3000.0 * size.0,
                    "one physical line must extend horizontally: {:?}",
                    node.size()
                );
                assert!(
                    (node.size().width / reference_node.size().width - 1.0).abs() < 0.03,
                    "horizontal estimates must preserve physical extent"
                );
            }
        }
    }

    #[test]
    fn chunk_boundaries_preserve_complete_graphemes() {
        use unicode_segmentation::UnicodeSegmentation;
        for seam in ["e\u{301}", "👩‍💻", "🇨🇦", "\r\n", " \u{301}"] {
            let source = format!("{}{seam}{}", "a".repeat(if seam.starts_with(' ') { 1500 } else { 2047 }), "z".repeat(4096));
            let boundaries: HashSet<_> = source.grapheme_indices(true)
                .map(|(at, _)| at).chain(std::iter::once(source.len())).collect();
            let ranges = chunk_ranges(&source);
            assert_eq!(
                ranges
                    .iter()
                    .map(|range| &source[range.clone()])
                    .collect::<String>(),
                source
            );
            for range in ranges {
                assert!(boundaries.contains(&range.start) && boundaries.contains(&range.end));
            }
        }
    }

    #[test]
    fn wide_table_only_shapes_cells_near_horizontal_viewport() {
        use iced::advanced::{layout, widget::Tree};
        let source = format!(
            "|{}|\n|{}|\n|{}|",
            "cell|".repeat(10000),
            "---|".repeat(10000),
            "value|".repeat(10000)
        );
        let cached = CachedItem::new(
            &ItemContent::Text {
                text: source,
                thinking: false,
                phase: chat::Phase::Final,
            },
            crate::settings::EditorSyntaxTheme::Auto,
        );
        let mut element = view(
            &cached,
            design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE),
            Target {
                card: CardId::new(),
                generation: 0,
            },
            None,
        );
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(&element);
        let before = virtualized::constructed_rows();
        element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(720.0, f32::INFINITY)),
        );
        assert!(
            virtualized::constructed_rows() - before < 100,
            "wide rows must not instantiate all cells"
        );
    }

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
