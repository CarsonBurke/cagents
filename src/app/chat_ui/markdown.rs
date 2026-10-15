//! Native Markdown model and parser, without discarded widget highlighting.
//!
//! Adapted from iced_widget/src/markdown.rs at libcosmic revision
//! caec74c2559924443f12fc6faf97a5bcefe6271d. Rendering remains in prose.rs.
//! Keeping the parser model here exposes cheap logical text/byte ranges to
//! selection and virtualization without forcing styled spans for hidden rows.
/*
Copyright 2019 Héctor Ramón, Iced contributors

Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
*/
use cosmic::iced::{Font, font};
use cosmic::iced::widget::{span, text};
use std::{borrow::BorrowMut, cell::{Cell, RefCell}, collections::{HashMap, HashSet},
    mem, ops::Range, rc::Rc, sync::Arc};
pub use cosmic::widget::markdown::{Settings, Style, HeadingLevel};
pub type Uri = String;

/// A bunch of Markdown that has been parsed.
#[derive(Debug, Default, Clone)]
pub struct Content {
    items: Vec<Item>,
    incomplete: HashMap<usize, Section>,
    state: State,
    open_fence: Option<OpenFence>,
    open_plain: bool,
    #[cfg(test)]
    parsed_bytes: usize,
}

#[derive(Debug, Clone)]
struct Section {
    content: String,
    broken_links: HashSet<String>,
}

impl Content {
    /// Creates a new empty [`Content`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates some new [`Content`] by parsing the given Markdown.
    pub fn parse(markdown: &str) -> Self {
        let mut content = Self::new();
        content.push_str(markdown);
        content
    }

    /// Pushes more Markdown into the [`Content`]; parsing incrementally!
    ///
    /// This is specially useful when you have long streams of Markdown; like
    /// big files or potentially long replies.
    pub fn push_str(&mut self, markdown: &str) {
        if markdown.is_empty() {
            return;
        }

        // A syntax-free single-line paragraph cannot change interpretation
        // while more ordinary text arrives. Delimiters/newlines return to the
        // CommonMark path, including markers split across provider deltas.
        if self.open_plain && markdown.chars().all(plain_character)
            && let Some(Item::Paragraph(text)) = self.items.last_mut()
        {
            self.state.leftover.push_str(markdown);
            text.append_plain(markdown);
            return;
        }
        self.open_plain = false;

        // Once a top-level fence is open, its body has no Markdown syntax.
        // Append directly until a possible closing line requires the parser.
        if !markdown.contains(['\r', '\0'])
            && let Some(mut fence) = self.open_fence.take()
            && !fence.advance(markdown)
            && let Some(Item::CodeBlock { code, reader_source, .. }) = self.items.last_mut()
        {
            self.state.leftover.push_str(markdown);
            reader_source.append(code, markdown);
            code.push_str(markdown);
            self.open_fence = Some(fence);
            return;
        }
        self.open_fence = None;
        // Append to last leftover text
        let mut leftover = std::mem::take(&mut self.state.leftover);
        leftover.push_str(markdown);

        let input = if leftover.trim_end().ends_with('|') {
            leftover.trim_end().trim_end_matches('|')
        } else {
            leftover.as_str()
        };

        #[cfg(test)]
        { self.parsed_bytes += input.len(); }
        // Retain only enough prior parser state to prove a rendered styled
        // prefix after reparsing. Raw-source append alone is insufficient.
        let previous_index = self.items.len().saturating_sub(1);
        let previous = self.items.pop();

        // Re-parse last item and new text
        for (item, source, broken_links) in parse_with(&mut self.state, input) {
            if !broken_links.is_empty() {
                let _ = self.incomplete.insert(
                    self.items.len(),
                    Section {
                        content: source.to_owned(),
                        broken_links,
                    },
                );
            }

            self.items.push(item);
        }

        self.state.leftover.push_str(&leftover[input.len()..]);

        self.open_fence = self.items.last().and_then(|item| {
            let Item::CodeBlock { code, .. } = item else { return None; };
            OpenFence::detect(&self.state.leftover, code)
        });

        self.open_plain = self.state.leftover.chars().next().is_some_and(char::is_alphabetic)
            && self.state.leftover.chars().all(plain_character)
            && matches!(self.items.last(), Some(Item::Paragraph(text)) if text.is_plain_source(&self.state.leftover));

        // Re-parse incomplete sections if new references are available
        if !self.incomplete.is_empty() {
            self.incomplete.retain(|index, section| {
                if self.items.len() <= *index {
                    return false;
                }

                let broken_links_before = section.broken_links.len();

                section
                    .broken_links
                    .retain(|link| !self.state.references.contains_key(link));

                if broken_links_before != section.broken_links.len() {
                    let mut state = State {
                        leftover: String::new(),
                        references: self.state.references.clone(),
                        images: HashSet::new(),
                    };

                    if let Some((item, _source, _broken_links)) =
                        parse_with(&mut state, &section.content).next()
                    {
                        self.items[*index] = item;
                    }

                    self.state.images.extend(state.images.drain());
                    drop(state);
                }

                !section.broken_links.is_empty()
            });
        }
        if let (Some(previous), Some(current)) = (previous.as_ref(), self.items.get_mut(previous_index)) {
            match (previous, current) {
                (Item::Paragraph(previous), Item::Paragraph(current)) => current.record_prefix(previous),
                (Item::Heading(old_level, previous), Item::Heading(level, current)) if old_level == level => current.record_prefix(previous),
                (Item::CodeBlock { language: old_language, code: old, reader_source: previous, .. },
                 Item::CodeBlock { language, code, reader_source, .. }) if old_language == language => {
                    *reader_source = super::reader_source::Source::updated(previous, old, code);
                }
                _ => {},
            }
        }
    }

    /// Returns the Markdown items, ready to be rendered.
    ///
    /// You can use [`view`] to turn them into an [`Element`].
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// Returns the URLs of the Markdown images present in the [`Content`].
    pub fn images(&self) -> &HashSet<Uri> {
        &self.state.images
    }
}

fn plain_character(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, ' ' | '.' | ',' | ':' | ';' | '?' | '!' | '/' | '(' | ')' | '=' | '+' | '-')
}

/// Only a zero-indented top-level fence is fast-pathed. Nested/indented
/// fences, CRLF and parser-normalized content retain the general parser path.
#[derive(Debug, Clone)]
struct OpenFence {
    marker: u8,
    length: usize,
    leading: usize,
    markers: usize,
    trailing: bool,
    other: bool,
}
impl OpenFence {
    fn detect(source: &str, code: &str) -> Option<Self> {
        let (header, body) = source.split_once('\n')?;
        let marker = *header.as_bytes().first()?;
        if !matches!(marker, b'`' | b'~') || body.contains(['\r', '\0']) || body != code { return None; }
        let length = header.bytes().take_while(|byte| *byte == marker).count();
        if length < 3 { return None; }
        let mut fence = Self { marker, length, leading: 0, markers: 0, trailing: false, other: false };
        if fence.advance(body) { None } else { Some(fence) }
    }
    /// A possible closing fence delegates the complete block to CommonMark;
    /// otherwise only finite line-prefix state is needed across appends.
    fn advance(&mut self, text: &str) -> bool {
        for byte in text.bytes() {
            if byte == b'\n' {
                if !self.other && self.markers >= self.length { return true; }
                self.leading = 0; self.markers = 0; self.trailing = false; self.other = false;
            } else if !self.other {
                if self.markers == 0 && byte == b' ' && self.leading < 3 { self.leading += 1; }
                else if byte == self.marker && !self.trailing { self.markers += 1; }
                else if matches!(byte, b' ' | b'\t') && self.markers > 0 { self.trailing = true; }
                else { self.other = true; }
            }
        }
        !self.other && self.markers >= self.length
    }
}

/// A Markdown item.
#[derive(Debug, Clone)]
pub enum Item {
    /// A heading.
    Heading(pulldown_cmark::HeadingLevel, Text),
    /// A paragraph.
    Paragraph(Text),
    /// A code block. Highlighting is prepared only by the native renderer.
    CodeBlock {
        /// The language of the code block, if any.
        language: Option<String>,
        /// The raw code of the code block.
        code: String,
        /// The styled lines of text in the code block.
        lines: Vec<Text>,
        /// Stable source digest and bounded append proof for nested reader geometry.
        reader_source: super::reader_source::Source,
    },
    /// A list.
    List {
        /// The first number of the list, if it is ordered.
        start: Option<u64>,
        /// The items of the list.
        bullets: Vec<Bullet>,
    },
    /// An image.
    Image {
        /// The destination URL of the image.
        url: Uri,
        /// The title of the image.
        title: String,
        /// The alternative text of the image.
        alt: Text,
    },
    /// A quote.
    Quote(Vec<Item>),
    /// A horizontal separator.
    Rule,
    /// A table.
    Table {
        /// The columns of the table.
        columns: Vec<Column>,
        /// The rows of the table.
        rows: Vec<Row>,
    },
}

/// The column of a table.
#[derive(Debug, Clone)]
pub struct Column {
    /// The header of the column.
    pub header: Vec<Item>,
    /// The alignment of the column.
    pub alignment: pulldown_cmark::Alignment,
}

/// The row of a table.
#[derive(Debug, Clone)]
pub struct Row {
    /// The cells of the row.
    pub cells: Vec<Vec<Item>>,
}

#[cfg(test)]
thread_local! { static MATERIALIZED_BYTES: Cell<usize> = const { Cell::new(0) }; }
#[cfg(test)]
pub(super) fn materialized_bytes() -> usize { MATERIALIZED_BYTES.with(Cell::get) }

/// A bunch of parsed Markdown text.
#[derive(Debug, Clone)]
pub struct Text {
    spans: Arc<Vec<Span>>,
    starts: Arc<[usize]>,
    plain: std::cell::OnceCell<Arc<str>>,
    last_style: Cell<Option<Style>>,
    last_styled_spans: RefCell<Arc<[text::Span<'static, Uri>]>>,
    lineage: Arc<std::sync::Mutex<StyledLineage>>,
}
#[derive(Debug, Default)]
struct StyledLineage {
    edges: std::collections::VecDeque<(std::sync::Weak<Vec<Span>>, std::sync::Weak<Vec<Span>>)>,
}

impl Text {
    fn new(spans: Vec<Span>) -> Self {
        let mut starts = Vec::with_capacity(spans.len() + 1);
        starts.push(0);
        for span in &spans { starts.push(starts.last().copied().unwrap() + span.text().len()); }
        Self {
            spans: spans.into(),
            starts: starts.into(),
            plain: std::cell::OnceCell::new(),
            last_style: Cell::default(),
            last_styled_spans: RefCell::default(),
            lineage: Default::default(),
        }
    }

    fn is_plain_source(&self, source: &str) -> bool {
        matches!(self.spans.as_slice(), [Span::Standard { text, strikethrough: false,
            link: None, strong: false, emphasis: false, code: false }] if text == source)
    }
    fn append_plain(&mut self, suffix: &str) {
        let previous = Arc::downgrade(&self.spans);
        let [Span::Standard { text, .. }] = Arc::make_mut(&mut self.spans).as_mut_slice() else { unreachable!("plain append requires one plain span") };
        text.push_str(suffix);
        let length = self.starts.last().copied().unwrap_or_default() + suffix.len();
        self.starts = Arc::from([0, length]);
        self.plain.take();
        self.last_style.set(None);
        *self.last_styled_spans.get_mut() = Arc::from([]);
        self.record_edge(previous);
    }

    fn record_edge(&mut self, previous: std::sync::Weak<Vec<Span>>) {
        let mut lineage = self.lineage.lock().unwrap_or_else(|poison| poison.into_inner());
        if lineage.edges.len() == 256 { lineage.edges.pop_front(); }
        lineage.edges.push_back((previous, Arc::downgrade(&self.spans)));
    }
    fn record_prefix(&mut self, previous: &Text) {
        if self.spans.len() < previous.spans.len() { return; }
        if !previous.spans.iter().zip(self.spans.iter()).enumerate().all(|(index, (old, new))|
            old.prefix_of(new, index + 1 == previous.spans.len())) { return; }
        self.lineage = previous.lineage.clone();
        self.record_edge(Arc::downgrade(&previous.spans));
    }
    /// Bounded, lineage-only proof for a previously rendered styled prefix.
    /// This never scans text or materializes plain/styled spans. Reparsing
    /// records exact conservative proofs before publication; rewrites and
    /// Markdown reinterpretation establish a new lineage instead.
    pub fn append_proven(&self, previous: &Text) -> bool {
        if Arc::ptr_eq(&self.spans, &previous.spans) { return true; }
        if !Arc::ptr_eq(&self.lineage, &previous.lineage) { return false; }
        let mut cursor = Arc::downgrade(&previous.spans);
        let target = Arc::downgrade(&self.spans);
        let lineage = self.lineage.lock().unwrap_or_else(|poison| poison.into_inner());
        for (old, next) in &lineage.edges {
            if !old.ptr_eq(&cursor) { continue; }
            if next.ptr_eq(&target) { return true; }
            cursor = next.clone();
        }
        false
    }

    /// Logical text without constructing styled presentation spans. Shared by
    /// the active transcript's selection/find index and bounded visible rows.
    pub fn plain(&self) -> Arc<str> {
        self.plain.get_or_init(|| {
            #[cfg(test)]
            MATERIALIZED_BYTES.with(|bytes| bytes.set(bytes.get() + self.len()));
            self.spans.iter().map(Span::text).collect::<String>().into()
        }).clone()
    }
    pub fn len(&self) -> usize { self.starts.last().copied().unwrap_or_default() }

    /// Style just a bounded rendered byte range; hidden chunks never populate
    /// the AST's whole-paragraph styled-spans cache.
    pub fn spans_range(&self, style: Style, range: Range<usize>) -> Arc<[text::Span<'static, Uri>]> {
        let first = self.starts.partition_point(|start| *start <= range.start).saturating_sub(1);
        self.spans.iter().enumerate().skip(first).take_while(|(index, _)| self.starts[*index] < range.end)
            .filter_map(|(index, span)| {
                let start = range.start.saturating_sub(self.starts[index]);
                let end = range.end.saturating_sub(self.starts[index]).min(span.text().len());
                (start < end).then(|| span.view_part(&style, start..end))
            }).collect()
    }

    /// Returns the [`rich_text()`] spans ready to be used for the given style.
    ///
    /// This method performs caching for you. It will only reallocate if the [`Style`]
    /// provided changes.
    pub fn spans(&self, style: Style) -> Arc<[text::Span<'static, Uri>]> {
        if Some(style) != self.last_style.get() {
            #[cfg(test)]
            MATERIALIZED_BYTES.with(|bytes| bytes.set(bytes.get() + self.len()));
            *self.last_styled_spans.borrow_mut() =
                self.spans.iter().map(|span| span.view(&style)).collect();

            self.last_style.set(Some(style));
        }

        self.last_styled_spans.borrow().clone()
    }
}

#[derive(Debug, Clone)]
enum Span {
    Standard {
        text: String,
        strikethrough: bool,
        link: Option<Uri>,
        strong: bool,
        emphasis: bool,
        code: bool,
    },
}

impl Span {
    fn prefix_of(&self, new: &Self, last: bool) -> bool {
        let Self::Standard { text: old, strikethrough: a, link: b, strong: c, emphasis: d, code: e } = self;
        let Self::Standard { text: new, strikethrough: f, link: g, strong: h, emphasis: i, code: j } = new;
        (a,b,c,d,e) == (f,g,h,i,j) && if last { new.starts_with(old) } else { new == old }
    }
    fn text(&self) -> &str { match self { Self::Standard { text, .. } => text } }
    fn view(&self, style: &Style) -> text::Span<'static, Uri> {
        self.view_part(style, 0..self.text().len())
    }
    fn view_part(&self, style: &Style, range: Range<usize>) -> text::Span<'static, Uri> {
        match self {
            Span::Standard {
                text,
                strikethrough,
                link,
                strong,
                emphasis,
                code,
            } => {
                let span = span(text[range].to_owned()).strikethrough(*strikethrough);

                let span = if *code {
                    span.font(style.inline_code_font)
                        .color(style.inline_code_color)
                        .background(style.inline_code_highlight.background)
                        .border(style.inline_code_highlight.border)
                        .padding(style.inline_code_padding)
                } else if *strong || *emphasis {
                    span.font(Font {
                        weight: if *strong {
                            font::Weight::Bold
                        } else {
                            font::Weight::Normal
                        },
                        style: if *emphasis {
                            font::Style::Italic
                        } else {
                            font::Style::Normal
                        },
                        ..style.font
                    })
                } else {
                    span.font(style.font)
                };

                if let Some(link) = link.as_ref() {
                    span.color(style.link_color).link(link.clone())
                } else {
                    span
                }
            }
        }
    }
}

/// The item of a list.
#[derive(Debug, Clone)]
pub enum Bullet {
    /// A simple bullet point.
    Point {
        /// The contents of the bullet point.
        items: Vec<Item>,
    },
    /// A task.
    Task {
        /// The contents of the task.
        items: Vec<Item>,
        /// Whether the task is done or not.
        done: bool,
    },
}

impl Bullet {
    fn items(&self) -> &[Item] {
        match self {
            Bullet::Point { items } | Bullet::Task { items, .. } => items,
        }
    }

    fn push(&mut self, item: Item) {
        let (Bullet::Point { items } | Bullet::Task { items, .. }) = self;

        items.push(item);
    }
}

#[derive(Debug, Default, Clone)]
struct State {
    leftover: String,
    references: HashMap<String, String>,
    images: HashSet<Uri>,
}

fn parse_with<'a>(
    mut state: impl BorrowMut<State> + 'a,
    markdown: &'a str,
) -> impl Iterator<Item = (Item, &'a str, HashSet<String>)> + 'a {
    enum Scope {
        List(List),
        Quote(Vec<Item>),
        Table {
            alignment: Vec<pulldown_cmark::Alignment>,
            columns: Vec<Column>,
            rows: Vec<Row>,
            current: Vec<Item>,
        },
    }

    struct List {
        start: Option<u64>,
        bullets: Vec<Bullet>,
    }

    let broken_links = Rc::new(RefCell::new(HashSet::new()));

    let mut spans = Vec::new();
    let mut code = String::new();
    let mut code_language = None;
    let mut code_lines = Vec::new();
    let mut strong = false;
    let mut emphasis = false;
    let mut strikethrough = false;
    let mut metadata = false;
    let mut code_block = false;
    let mut link = None;
    let mut image = None;
    let mut stack = Vec::new();


    let parser = pulldown_cmark::Parser::new_with_broken_link_callback(
        markdown,
        pulldown_cmark::Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
            | pulldown_cmark::Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS
            | pulldown_cmark::Options::ENABLE_TABLES
            | pulldown_cmark::Options::ENABLE_STRIKETHROUGH
            | pulldown_cmark::Options::ENABLE_TASKLISTS,
        {
            let references = state.borrow().references.clone();
            let broken_links = broken_links.clone();

            Some(move |broken_link: pulldown_cmark::BrokenLink<'_>| {
                if let Some(reference) =
                    references.get(broken_link.reference.as_ref())
                {
                    Some((
                        pulldown_cmark::CowStr::from(reference.to_owned()),
                        broken_link.reference.into_static(),
                    ))
                } else {
                    let _ = RefCell::borrow_mut(&broken_links)
                        .insert(broken_link.reference.into_string());

                    None
                }
            })
        },
    );

    let references = &mut state.borrow_mut().references;

    for reference in parser.reference_definitions().iter() {
        let _ = references
            .insert(reference.0.to_owned(), reference.1.dest.to_string());
    }

    let produce = move |state: &mut State,
                        stack: &mut Vec<Scope>,
                        item,
                        source: Range<usize>| {
        if let Some(scope) = stack.last_mut() {
            match scope {
                Scope::List(list) => {
                    list.bullets.last_mut().expect("item context").push(item);
                }
                Scope::Quote(items) => {
                    items.push(item);
                }
                Scope::Table { current, .. } => {
                    current.push(item);
                }
            }

            None
        } else {
            state.leftover = markdown[source.start..].to_owned();

            Some((
                item,
                &markdown[source.start..source.end],
                broken_links.take(),
            ))
        }
    };

    let parser = parser.into_offset_iter();

    // We want to keep the `spans` capacity
    #[allow(clippy::drain_collect)]
    parser.filter_map(move |(event, source)| match event {
        pulldown_cmark::Event::Start(tag) => match tag {
            pulldown_cmark::Tag::Strong if !metadata => {
                strong = true;
                None
            }
            pulldown_cmark::Tag::Emphasis if !metadata => {
                emphasis = true;
                None
            }
            pulldown_cmark::Tag::Strikethrough if !metadata => {
                strikethrough = true;
                None
            }
            pulldown_cmark::Tag::Link { dest_url, .. } if !metadata => {
                link = Some(dest_url.into_string());
                None
            }
            pulldown_cmark::Tag::Image {
                dest_url, title, ..
            } if !metadata => {
                image = Some((dest_url.into_string(), title.into_string()));
                None
            }
            pulldown_cmark::Tag::List(first_item) if !metadata => {
                let prev = if spans.is_empty() {
                    None
                } else {
                    produce(
                        state.borrow_mut(),
                        &mut stack,
                        Item::Paragraph(Text::new(spans.drain(..).collect())),
                        source,
                    )
                };

                stack.push(Scope::List(List {
                    start: first_item,
                    bullets: Vec::new(),
                }));

                prev
            }
            pulldown_cmark::Tag::Item => {
                if let Some(Scope::List(list)) = stack.last_mut() {
                    list.bullets.push(Bullet::Point { items: Vec::new() });
                }

                None
            }
            pulldown_cmark::Tag::BlockQuote(_kind) if !metadata => {
                let prev = if spans.is_empty() {
                    None
                } else {
                    produce(
                        state.borrow_mut(),
                        &mut stack,
                        Item::Paragraph(Text::new(spans.drain(..).collect())),
                        source,
                    )
                };

                stack.push(Scope::Quote(Vec::new()));

                prev
            }
            pulldown_cmark::Tag::CodeBlock(
                pulldown_cmark::CodeBlockKind::Fenced(language),
            ) if !metadata => {

                code_block = true;
                code_language =
                    (!language.is_empty()).then(|| language.into_string());

                if spans.is_empty() {
                    None
                } else {
                    produce(
                        state.borrow_mut(),
                        &mut stack,
                        Item::Paragraph(Text::new(spans.drain(..).collect())),
                        source,
                    )
                }
            }
            pulldown_cmark::Tag::MetadataBlock(_) => {
                metadata = true;
                None
            }
            pulldown_cmark::Tag::Table(alignment) => {
                stack.push(Scope::Table {
                    columns: Vec::with_capacity(alignment.len()),
                    alignment,
                    current: Vec::new(),
                    rows: Vec::new(),
                });

                None
            }
            pulldown_cmark::Tag::TableHead => {
                strong = true;
                None
            }
            pulldown_cmark::Tag::TableRow => {
                let Scope::Table { rows, .. } = stack.last_mut()? else {
                    return None;
                };

                rows.push(Row { cells: Vec::new() });
                None
            }
            _ => None,
        },
        pulldown_cmark::Event::End(tag) => match tag {
            pulldown_cmark::TagEnd::Heading(level) if !metadata => produce(
                state.borrow_mut(),
                &mut stack,
                Item::Heading(level, Text::new(spans.drain(..).collect())),
                source,
            ),
            pulldown_cmark::TagEnd::Strong if !metadata => {
                strong = false;
                None
            }
            pulldown_cmark::TagEnd::Emphasis if !metadata => {
                emphasis = false;
                None
            }
            pulldown_cmark::TagEnd::Strikethrough if !metadata => {
                strikethrough = false;
                None
            }
            pulldown_cmark::TagEnd::Link if !metadata => {
                link = None;
                None
            }
            pulldown_cmark::TagEnd::Paragraph if !metadata => {
                if spans.is_empty() {
                    None
                } else {
                    produce(
                        state.borrow_mut(),
                        &mut stack,
                        Item::Paragraph(Text::new(spans.drain(..).collect())),
                        source,
                    )
                }
            }
            pulldown_cmark::TagEnd::Item if !metadata => {
                if spans.is_empty() {
                    None
                } else {
                    produce(
                        state.borrow_mut(),
                        &mut stack,
                        Item::Paragraph(Text::new(spans.drain(..).collect())),
                        source,
                    )
                }
            }
            pulldown_cmark::TagEnd::List(_) if !metadata => {
                let scope = stack.pop()?;

                let Scope::List(list) = scope else {
                    return None;
                };

                produce(
                    state.borrow_mut(),
                    &mut stack,
                    Item::List {
                        start: list.start,
                        bullets: list.bullets,
                    },
                    source,
                )
            }
            pulldown_cmark::TagEnd::BlockQuote(_kind) if !metadata => {
                let scope = stack.pop()?;

                let Scope::Quote(quote) = scope else {
                    return None;
                };

                produce(
                    state.borrow_mut(),
                    &mut stack,
                    Item::Quote(quote),
                    source,
                )
            }
            pulldown_cmark::TagEnd::Image if !metadata => {
                let (url, title) = image.take()?;
                let alt = Text::new(spans.drain(..).collect());

                let state = state.borrow_mut();
                let _ = state.images.insert(url.clone());

                produce(
                    state,
                    &mut stack,
                    Item::Image { url, title, alt },
                    source,
                )
            }
            pulldown_cmark::TagEnd::CodeBlock if !metadata => {
                code_block = false;


                produce(
                    state.borrow_mut(),
                    &mut stack,
                    Item::CodeBlock {
                        language: code_language.take(),
                        reader_source: super::reader_source::Source::new(&code),
                        code: mem::take(&mut code),
                        lines: code_lines.drain(..).collect(),
                    },
                    source,
                )
            }
            pulldown_cmark::TagEnd::MetadataBlock(_) => {
                metadata = false;
                None
            }
            pulldown_cmark::TagEnd::Table => {
                let scope = stack.pop()?;

                let Scope::Table { columns, rows, .. } = scope else {
                    return None;
                };

                produce(
                    state.borrow_mut(),
                    &mut stack,
                    Item::Table { columns, rows },
                    source,
                )
            }
            pulldown_cmark::TagEnd::TableHead => {
                strong = false;
                None
            }
            pulldown_cmark::TagEnd::TableCell => {
                if !spans.is_empty() {
                    let _ = produce(
                        state.borrow_mut(),
                        &mut stack,
                        Item::Paragraph(Text::new(spans.drain(..).collect())),
                        source,
                    );
                }

                let Scope::Table {
                    alignment,
                    columns,
                    rows,
                    current,
                } = stack.last_mut()?
                else {
                    return None;
                };

                if columns.len() < alignment.len() {
                    columns.push(Column {
                        header: std::mem::take(current),
                        alignment: alignment[columns.len()],
                    });
                } else {
                    rows.last_mut()
                        .expect("table row")
                        .cells
                        .push(std::mem::take(current));
                }

                None
            }
            _ => None,
        },
        pulldown_cmark::Event::Text(text) if !metadata => {
            if code_block {
                code.push_str(&text);



                return None;
            }

            let span = Span::Standard {
                text: text.into_string(),
                strong,
                emphasis,
                strikethrough,
                link: link.clone(),
                code: false,
            };

            spans.push(span);

            None
        }
        pulldown_cmark::Event::Code(code) if !metadata => {
            let span = Span::Standard {
                text: code.into_string(),
                strong,
                emphasis,
                strikethrough,
                link: link.clone(),
                code: true,
            };

            spans.push(span);
            None
        }
        pulldown_cmark::Event::SoftBreak if !metadata => {
            spans.push(Span::Standard {
                text: String::from(" "),
                strikethrough,
                strong,
                emphasis,
                link: link.clone(),
                code: false,
            });
            None
        }
        pulldown_cmark::Event::HardBreak if !metadata => {
            spans.push(Span::Standard {
                text: String::from("\n"),
                strikethrough,
                strong,
                emphasis,
                link: link.clone(),
                code: false,
            });
            None
        }
        pulldown_cmark::Event::InlineHtml(h)
            if !metadata && h.eq_ignore_ascii_case("<br>") =>
        {
            spans.push(Span::Standard {
                text: String::from("\n"),
                strikethrough,
                strong,
                emphasis,
                link: link.clone(),
                code: false,
            });
            None
        }
        pulldown_cmark::Event::Rule => {
            produce(state.borrow_mut(), &mut stack, Item::Rule, source)
        }
        pulldown_cmark::Event::TaskListMarker(done) => {
            if let Some(Scope::List(list)) = stack.last_mut()
                && let Some(item) = list.bullets.last_mut()
                && let Bullet::Point { items } = item
            {
                *item = Bullet::Task {
                    items: std::mem::take(items),
                    done,
                };
            }

            None
        }
        _ => None,
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::chat_ui::markdown as native;
    use cosmic::widget::markdown as original;

    macro_rules! outline {
        ($name:ident, $module:ident, $plain:expr) => {
            fn $name(items: &[$module::Item]) -> Vec<String> {
                let mut out = Vec::new();
                for item in items {
                    match item {
                        $module::Item::Heading(level, text) => out.push(format!("heading {level:?} {}", ($plain)(text))),
                        $module::Item::Paragraph(text) => out.push(format!("paragraph {}", ($plain)(text))),
                        $module::Item::CodeBlock { language, code, .. } => out.push(format!("code {language:?} {code}")),
                        $module::Item::Quote(items) => { out.push("quote".into()); out.extend($name(items)); out.push("/quote".into()); }
                        $module::Item::List { start, bullets } => {
                            out.push(format!("list {start:?}"));
                            for bullet in bullets {
                                let items = match bullet {
                                    $module::Bullet::Point { items } => { out.push("bullet".into()); items },
                                    $module::Bullet::Task { items, done } => { out.push(format!("task {done}")); items },
                                };
                                out.extend($name(items)); out.push("/bullet".into());
                            }
                            out.push("/list".into());
                        }
                        $module::Item::Table { columns, rows } => {
                            out.push("table".into());
                            for column in columns { out.push(format!("column {:?}", column.alignment)); out.extend($name(&column.header)); }
                            for row in rows { out.push("row".into()); for cell in &row.cells { out.push("cell".into()); out.extend($name(cell)); } }
                            out.push("/table".into());
                        }
                        $module::Item::Image { url, title, alt } => out.push(format!("image {url} {title} {}", ($plain)(alt))),
                        $module::Item::Rule => out.push("rule".into()),
                    }
                }
                out
            }
        };
    }
    outline!(native_outline, native, |text: &native::Text| text.plain().to_string());
    outline!(original_outline, original, |text: &original::Text| {
        let style = crate::design::markdown_settings(&cosmic::Theme::dark(), 14.0).style;
        text.spans(style).iter().map(|span| span.text.as_ref()).collect::<String>()
    });

    fn paragraph_text(content: &Content) -> Text {
        match content.items() { [Item::Paragraph(text)] => text.clone(), other => panic!("expected paragraph: {other:?}") }
    }

    #[test]
    fn styled_append_lineage_accepts_coalesced_prefixes_and_releases_old_bodies() {
        let mut content = Content::parse(&"words ".repeat(1000));
        let first = paragraph_text(&content);
        content.push_str("more words");
        let middle = paragraph_text(&content);
        let released = Arc::downgrade(&middle.spans);
        content.push_str(" still more");
        let latest = paragraph_text(&content);
        assert!(latest.append_proven(&first));
        assert!(latest.append_proven(&middle));
        drop(middle);
        assert!(released.upgrade().is_none(), "proof edges must not own source bodies");
        let mut styled = Content::parse("word **bold** tail");
        let previous = paragraph_text(&styled);
        styled.push_str(" continuing");
        assert!(paragraph_text(&styled).append_proven(&previous),
            "unchanged emphasis/link/style prefix may complete progressively");
    }

    #[test]
    fn styled_append_lineage_rejects_reinterpretation_rewrites_and_expired_proofs() {
        let mut content = Content::parse("word **bold");
        let previous = paragraph_text(&content);
        content.push_str("**");
        assert!(!paragraph_text(&content).append_proven(&previous),
            "raw append that changes emphasis is not a rendered prefix");
        let previous = paragraph_text(&Content::parse("[label](https://old.example) tail"));
        let changed = paragraph_text(&Content::parse("[label](https://new.example) tail"));
        assert!(!changed.append_proven(&previous));
        let mut content = Content::parse("plain words");
        let first = paragraph_text(&content);
        for _ in 0..300 { content.push_str(" more"); }
        let latest = paragraph_text(&content);
        assert!(!latest.append_proven(&first), "expired proof requires worker-side exact fallback");
        assert!(latest.lineage.lock().unwrap().edges.len() <= 256);
    }

    const CASES: &[&str] = &[
        "# Heading\n\nA **bold** and *italic* [link](https://example.com), `inline code`, ~~strike~~.\n",
        "```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\nAfter.\n",
        "> A quote.\n>\n> - nested\n> - list\n\nOutside.\n",
        "1. one\n2. two\n   - nested\n   - [x] done\n\n- [ ] pending\n",
        "| alpha | beta |\n| :--- | ---: |\n| a | **b** |\n| | empty |\n",
        "![alternative](https://example.com/a.png \"title\")\n\n[label][ref]\n\n[ref]: https://example.com\n",
        "A Unicode αβ emoji 👩🏽‍💻 e\u{301} link [Straße](https://example.com).\n\n----\n",
        "one long paragraph with **bold crossing\nlines** and `code\nspanning lines`, [link\nlabel](https://example.com).\n",
    ];

    #[test]
    fn native_model_matches_pinned_markdown_semantics_without_discarded_highlight_lines() {
        for source in CASES {
            let native = Content::parse(source);
            let original = original::Content::parse(source);
            assert_eq!(native_outline(native.items()), original_outline(original.items()), "{source:?}");
            for item in native.items() {
                if let Item::CodeBlock { lines, .. } = item { assert!(lines.is_empty()); }
            }
        }
    }

    #[test]
    fn all_utf8_append_boundaries_match_a_whole_parse() {
        for source in CASES {
            let expected = native_outline(Content::parse(source).items());
            for stride in [1, 2, 7, 31] {
                let mut content = Content::new();
                let mut previous = 0;
                for end in (1..=source.len()).filter(|end| source.is_char_boundary(*end)).step_by(stride)
                    .chain(std::iter::once(source.len())) {
                    if end > previous { content.push_str(&source[previous..end]); previous = end; }
                }
                assert_eq!(native_outline(content.items()), expected, "stride {stride}, {source:?}");
            }
        }
    }

    #[test]
    fn a_plain_stream_is_linear_and_delimiters_restore_commonmark_semantics() {
        let mut content = Content::parse("Answer");
        let before = content.parsed_bytes;
        let mut source = "Answer".to_owned();
        for _ in 0..10000 { content.push_str(" more ordinary α text"); source.push_str(" more ordinary α text"); }
        assert_eq!(content.parsed_bytes, before);
        for suffix in [" **", "bold", "**", "\n", "- item", "\n\n", "[link", "](https://example.com)"] {
            content.push_str(suffix); source.push_str(suffix);
            assert_eq!(native_outline(content.items()), original_outline(original::Content::parse(&source).items()), "suffix {suffix:?}");
        }
        for source in ["Alpha ordinary text.", "Alpha **strong** text", "Alpha &amp; entity", "Alpha <tag>text</tag>", "Alpha `code` end", "Alpha\n---", "Alpha [link](https://example.com)"] {
            let mut content = Content::new();
            for end in (1..=source.len()).filter(|end| source.is_char_boundary(*end)) {
                let previous = source[..end].char_indices().last().unwrap().0;
                content.push_str(&source[previous..end]);
                assert_eq!(native_outline(content.items()), original_outline(original::Content::parse(&source[..end]).items()), "prefix {:?}", &source[..end]);
            }
        }
    }

    #[test]
    fn an_open_top_level_fence_parses_new_bytes_without_reparsing_its_body() {
        let mut content = Content::parse("```rust\nfn main() {}\n");
        let before = content.parsed_bytes;
        let mut source = "```rust\nfn main() {}\n".to_owned();
        for _ in 0..10000 { content.push_str("let value = 42;\n"); source.push_str("let value = 42;\n"); }
        assert_eq!(content.parsed_bytes, before, "the open body never re-enters CommonMark");
        content.push_str("```\n\nAfter the fence.\n"); source.push_str("```\n\nAfter the fence.\n");
        assert_eq!(native_outline(content.items()), original_outline(original::Content::parse(&source).items()));
        assert!(content.parsed_bytes < source.len() * 2, "closing costs one linear parse, not quadratic appends");
    }

    #[test]
    fn extracting_a_visible_span_range_does_not_style_the_hidden_paragraph() {
        let source = format!("{} **linked [αβ](https://example.com)** {}", "plain ".repeat(10000), "tail ".repeat(10000));
        let content = Content::parse(&source);
        let Item::Paragraph(text) = &content.items()[0] else { panic!("paragraph") };
        let plain = text.plain();
        let style = crate::design::markdown_settings(&cosmic::Theme::dark(), 14.0).style;
        let at = plain.find("linked").unwrap();
        let spans = text.spans_range(style, at..at + "linked αβ".len());
        assert_eq!(spans.iter().map(|span| span.text.as_ref()).collect::<String>(), "linked αβ");
        assert!(spans.iter().any(|span| span.link.as_deref() == Some("https://example.com")));
        assert!(text.last_style.get().is_none(), "hidden text has no styled-span cache");
        assert!(text.last_styled_spans.borrow().is_empty());
    }
}
