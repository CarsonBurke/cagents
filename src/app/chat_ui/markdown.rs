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
use cosmic::iced::widget::{span, text};
use cosmic::iced::{Font, font};
pub use cosmic::widget::markdown::{HeadingLevel, Settings, Style};
#[cfg(test)]
use std::cell::Cell;
use std::{
    borrow::BorrowMut,
    cell::RefCell,
    collections::{HashMap, HashSet},
    mem,
    ops::Range,
    rc::Rc,
    sync::Arc,
};
use unicase::UniCase;
pub type Uri = Arc<str>;

/// A bunch of Markdown that has been parsed.
#[derive(Debug, Default, Clone)]
pub struct Content {
    items: Vec<Item>,
    incomplete: HashMap<usize, Section>,
    state: State,
    open_fence: Option<OpenFence>,
    open_plain: bool,
    /// Raw EOF spaces become rendered only when later nonspace arrives.
    pending_spaces: usize,
    // A partially typed table row can temporarily parse as a following
    // paragraph. Keep both items in the unfinished source until separated.
    reparse_items: usize,
    #[cfg(test)]
    parsed_bytes: usize,
    #[cfg(test)]
    disposal: Option<Arc<Disposal>>,
}
#[cfg(test)]
#[derive(Debug)]
struct Disposal(std::sync::mpsc::Sender<std::thread::ThreadId>);
#[cfg(test)]
impl Drop for Disposal {
    fn drop(&mut self) {
        let _ = self.0.send(std::thread::current().id());
    }
}

#[derive(Debug, Clone)]
struct Section {
    // One raw CommonMark unit may produce several display items (images
    // surrounded by text). Rebuild the whole unit exactly once.
    items: usize,
    content: String,
    references: HashSet<ReferenceLabel>,
}

impl Content {
    #[cfg(test)]
    pub(super) fn has_open_fence_for_test(&self) -> bool {
        self.open_fence.is_some()
    }

    #[cfg(test)]
    pub(super) fn parsed_work_bytes(&self) -> usize {
        self.parsed_bytes
    }

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

    /// Append source while retaining completed parsed blocks. The unfinished
    /// block may be reparsed; syntax-free paragraphs and open top-level fences
    /// have specialized append paths. This does not promise linear work for
    /// every growing CommonMark document.
    pub fn push_str(&mut self, markdown: &str) {
        if markdown.is_empty() {
            return;
        }

        // A syntax-free single-line paragraph cannot change interpretation
        // while more ordinary text arrives. Delimiters/newlines return to the
        // CommonMark path, including markers split across provider deltas.
        if self.open_plain
            && markdown.chars().all(plain_character)
            && let Some(Item::Paragraph(text)) = self.items.last_mut()
        {
            let suffix = visible_suffix(markdown, &mut self.pending_spaces);
            self.state.leftover.push_str(markdown);
            if !suffix.is_empty() {
                text.append_plain(&suffix);
            }
            return;
        }
        self.open_plain = false;

        // Once a top-level fence is open, its body has no Markdown syntax.
        // Append directly until a possible closing line requires the parser.
        if !markdown.contains(['\r', '\0'])
            && let Some(mut fence) = self.open_fence.take()
            && !fence.advance(markdown)
            && let Some(Item::CodeBlock {
                code,
                reader_source,
                source,
                ..
            }) = self.items.last_mut()
        {
            self.state.leftover.push_str(markdown);
            reader_source.append(code, markdown);
            let mut appended = String::with_capacity(code.len() + markdown.len());
            appended.push_str(code);
            appended.push_str(markdown);
            *code = Arc::from(appended);
            *source = code.clone();
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
        {
            self.parsed_bytes += input.len();
            PARSER_WORK_BYTES.with(|bytes| bytes.set(bytes.get() + input.len()));
        }
        // Retain only enough prior parser state to prove a rendered styled
        // prefix after reparsing. Raw-source append alone is insufficient.
        let reparse = self.reparse_items.min(self.items.len());
        let previous_index = self.items.len().saturating_sub(reparse);
        let previous = (reparse == 1).then(|| self.items.pop()).flatten();
        self.items.truncate(previous_index);
        self.incomplete.retain(|index, _| *index < previous_index);
        let mut table_tail = None;
        let input_origin = self.state.leftover_origin;
        let previous_references = self.state.references.clone();

        // Re-parse the unfinished source, including a table whose next row
        // was not yet recognizable at the preceding publication boundary.
        let mut last_unit_items = 0;
        for (unit, source, references) in parse_with(&mut self.state, input, Some(input_origin)) {
            let start = source.as_ptr().addr() - input.as_ptr().addr();
            if matches!(unit.as_slice(), [Item::Table { .. }]) {
                table_tail = Some((self.items.len(), start));
            } else if table_tail.is_some() {
                let prefix = input[..start].strip_suffix('\n').unwrap_or(&input[..start]);
                let separated = prefix
                    .rsplit('\n')
                    .next()
                    .is_some_and(|line| line.trim().is_empty());
                if separated || !matches!(unit.as_slice(), [Item::Paragraph(_)]) {
                    table_tail = None;
                }
            }
            if !references.is_empty() {
                self.incomplete.insert(
                    self.items.len(),
                    Section {
                        items: unit.len(),
                        content: source.to_owned(),
                        references,
                    },
                );
            }
            last_unit_items = unit.len();
            self.items.extend(unit);
        }

        self.reparse_items = last_unit_items;
        if self.reparse_items == 0 {
            // Definitions and metadata can consume raw source without
            // producing a widget. Keep that tail, but do not remove a prior
            // completed block when the next provider delta arrives.
            self.state.leftover = input.to_owned();
            self.state.leftover_origin = input_origin;
        }
        if let Some((index, start)) = table_tail {
            let count = self.items.len() - index;
            if count > 1 {
                self.state.leftover = input[start..].to_owned();
                self.state.leftover_origin = input_origin + start;
                self.reparse_items = count;
            }
        }
        self.state.leftover.push_str(&leftover[input.len()..]);

        self.open_fence = self.items.last().and_then(|item| {
            let Item::CodeBlock { code, .. } = item else {
                return None;
            };
            OpenFence::detect(&self.state.leftover, code)
        });

        let rendered_source = self.state.leftover.trim_end_matches(' ');
        self.pending_spaces = self.state.leftover.len() - rendered_source.len();
        self.open_plain = self
            .state
            .leftover
            .chars()
            .next()
            .is_some_and(char::is_alphabetic)
            && self.state.leftover.chars().all(plain_character)
            && matches!(self.items.last(), Some(Item::Paragraph(text)) if text.is_plain_source(rendered_source));

        // A reference destination/title may grow, disappear, or be shadowed
        // while its definition is unfinished. Keep dependent sections even
        // after their first resolution and update them from exact current
        // definitions, including changes in their number of rendered items.
        let changed = previous_references
            .keys()
            .chain(self.state.references.keys())
            .filter(|label| previous_references.get(*label) != self.state.references.get(*label))
            .cloned()
            .collect::<HashSet<_>>();
        let mut previous_index = previous_index;
        if !changed.is_empty() {
            let old_tail = previous_index;
            let mut sections = std::mem::take(&mut self.incomplete);
            let mut rebuilt = Vec::with_capacity(self.items.len());
            let mut old_items = std::mem::take(&mut self.items).into_iter().enumerate();
            while let Some((index, item)) = old_items.next() {
                let section = sections.remove(&index);
                if index < old_tail
                    && section.as_ref().is_some_and(|section| {
                        section
                            .references
                            .iter()
                            .any(|label| changed.contains(label))
                    })
                {
                    let section = section.unwrap();
                    for _ in 1..section.items {
                        old_items.next().expect("complete raw unit items");
                    }
                    #[cfg(test)]
                    {
                        self.parsed_bytes += section.content.len();
                        PARSER_WORK_BYTES
                            .with(|bytes| bytes.set(bytes.get() + section.content.len()));
                    }
                    let mut state = State {
                        references: self.state.references.clone(),
                        ..State::default()
                    };
                    let before = rebuilt.len();
                    for (unit, source, references) in parse_with(&mut state, &section.content, None)
                    {
                        if !references.is_empty() {
                            self.incomplete.insert(
                                rebuilt.len(),
                                Section {
                                    items: unit.len(),
                                    content: source.to_owned(),
                                    references,
                                },
                            );
                        }
                        rebuilt.extend(unit);
                    }
                    previous_index = previous_index + rebuilt.len() - before - section.items;
                } else {
                    if let Some(section) = section {
                        self.incomplete.insert(rebuilt.len(), section);
                    }
                    rebuilt.push(item);
                }
            }
            self.items = rebuilt;
            self.state.images.clear();
            collect_images(&self.items, &mut self.state.images);
        }
        if let (Some(previous), Some(current)) =
            (previous.as_ref(), self.items.get_mut(previous_index))
        {
            match (previous, current) {
                (Item::Paragraph(previous), Item::Paragraph(current)) => {
                    current.record_prefix(previous)
                }
                (Item::Heading(old_level, previous), Item::Heading(level, current))
                    if old_level == level =>
                {
                    current.record_prefix(previous)
                }
                (
                    Item::CodeBlock {
                        language: old_language,
                        code: old,
                        reader_source: previous,
                        ..
                    },
                    Item::CodeBlock {
                        language,
                        code,
                        reader_source,
                        ..
                    },
                ) if old_language == language => {
                    *reader_source = super::reader_source::Source::updated(previous, old, code);
                }
                _ => {}
            }
        }
    }

    /// Release parser-only source after its publication has finished. The
    /// owning normalizer forces a complete restart before another append.
    pub(super) fn discard_continuation(&mut self) {
        self.state.leftover = String::new();
        self.state.references = Arc::default();
        self.incomplete = HashMap::new();
        self.open_fence = None;
        self.open_plain = false;
        self.pending_spaces = 0;
        self.reparse_items = 0;
    }

    /// A document can own substantial references or unfinished input without
    /// producing a single rendered item. Cache disposal must account for all
    /// these owners, using constant-time container checks.
    pub(super) fn has_retained_allocations(&self) -> bool {
        self.items.capacity() != 0
            || self.incomplete.capacity() != 0
            || self.state.leftover.capacity() != 0
            || self.state.references.capacity() != 0
            || self.state.images.capacity() != 0
            || self.open_fence.is_some()
    }
    #[cfg(test)]
    pub(super) fn watch_disposal(
        &mut self,
        sender: std::sync::mpsc::Sender<std::thread::ThreadId>,
    ) {
        self.disposal = Some(Arc::new(Disposal(sender)));
    }

    #[cfg(test)]
    pub(super) fn watch_first_raw_spans(
        &mut self,
        disposed: std::sync::mpsc::Sender<(std::thread::ThreadId, usize)>,
    ) -> usize {
        let Some(Item::Paragraph(text)) = self.items.first_mut() else {
            panic!("a real paragraph owns the observed raw spans")
        };
        text.watch_raw_spans(disposed);
        text.len()
    }
    #[cfg(test)]
    pub(super) fn reference_count(&self) -> usize {
        self.state.references.len()
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

/// CommonMark discards ASCII spaces at paragraph EOF. Keep them in raw source
/// and publish them only when a subsequent nonspace makes them interior text.
fn visible_suffix(suffix: &str, pending: &mut usize) -> String {
    let visible = suffix.trim_end_matches(' ');
    if visible.is_empty() {
        *pending += suffix.len();
        return String::new();
    }
    let mut result = String::with_capacity(*pending + visible.len());
    result.extend(std::iter::repeat_n(' ', *pending));
    result.push_str(visible);
    *pending = suffix.len() - visible.len();
    result
}

fn plain_character(ch: char) -> bool {
    ch.is_alphanumeric()
        || matches!(
            ch,
            ' ' | '.' | ',' | ':' | ';' | '?' | '!' | '/' | '(' | ')' | '=' | '+' | '-'
        )
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
        if !matches!(marker, b'`' | b'~') || body.contains(['\r', '\0']) || body != code {
            return None;
        }
        let length = header.bytes().take_while(|byte| *byte == marker).count();
        if length < 3 {
            return None;
        }
        let mut fence = Self {
            marker,
            length,
            leading: 0,
            markers: 0,
            trailing: false,
            other: false,
        };
        if fence.advance(body) {
            None
        } else {
            Some(fence)
        }
    }
    /// A possible closing fence delegates the complete block to CommonMark;
    /// otherwise only finite line-prefix state is needed across appends.
    fn advance(&mut self, text: &str) -> bool {
        for byte in text.bytes() {
            if byte == b'\n' {
                if !self.other && self.markers >= self.length {
                    return true;
                }
                self.leading = 0;
                self.markers = 0;
                self.trailing = false;
                self.other = false;
            } else if !self.other {
                if self.markers == 0 && byte == b' ' && self.leading < 3 {
                    self.leading += 1;
                } else if byte == self.marker && !self.trailing {
                    self.markers += 1;
                } else if matches!(byte, b' ' | b'\t') && self.markers > 0 {
                    self.trailing = true;
                } else {
                    self.other = true;
                }
            }
        }
        !self.other && self.markers >= self.length
    }
}

#[cfg(test)]
thread_local! { static ITEM_CLONES: Cell<usize> = const { Cell::new(0) }; }
#[cfg(test)]
pub(super) fn cloned_items() -> usize {
    ITEM_CLONES.with(Cell::get)
}

/// A Markdown item.
#[derive(Debug)]
#[cfg_attr(not(test), derive(Clone))]
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
        code: Arc<str>,
        /// Immutable display input prepared once with this parser revision.
        source: Arc<str>,
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
        /// Literal title text prepared alongside the image, never parsed as Markdown.
        title_text: Text,
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

#[cfg(test)]
impl Clone for Item {
    fn clone(&self) -> Self {
        ITEM_CLONES.with(|count| count.set(count.get() + 1));
        match self {
            Self::Heading(level, text) => Self::Heading(*level, text.clone()),
            Self::Paragraph(text) => Self::Paragraph(text.clone()),
            Self::CodeBlock {
                language,
                code,
                source,
                lines,
                reader_source,
            } => Self::CodeBlock {
                language: language.clone(),
                code: code.clone(),
                source: source.clone(),
                lines: lines.clone(),
                reader_source: reader_source.clone(),
            },
            Self::List { start, bullets } => Self::List {
                start: *start,
                bullets: bullets.clone(),
            },
            Self::Image {
                url,
                title,
                title_text,
                alt,
            } => Self::Image {
                url: url.clone(),
                title: title.clone(),
                title_text: title_text.clone(),
                alt: alt.clone(),
            },
            Self::Quote(items) => Self::Quote(items.clone()),
            Self::Rule => Self::Rule,
            Self::Table { columns, rows } => Self::Table {
                columns: columns.clone(),
                rows: rows.clone(),
            },
        }
    }
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
thread_local! { static PARSER_WORK_BYTES: Cell<usize> = const { Cell::new(0) }; }
#[cfg(test)]
pub(super) fn parser_work_bytes_on_thread() -> usize {
    PARSER_WORK_BYTES.with(Cell::get)
}

#[cfg(test)]
thread_local! { static MATERIALIZED_BYTES: Cell<usize> = const { Cell::new(0) }; }
#[cfg(test)]
pub(super) fn materialized_bytes() -> usize {
    MATERIALIZED_BYTES.with(Cell::get)
}

/// A bunch of parsed Markdown text.
#[derive(Debug, Clone)]
pub struct Text {
    spans: Arc<Spans>,
    starts: Arc<[usize]>,
    plain: std::cell::OnceCell<Arc<str>>,
    plain_source: std::cell::OnceCell<super::reader_source::Source>,
    // Most parsed nodes are never styled. Keep the large Style and empty
    // Arc allocation out of every cold Text (and therefore every Item).
    styled_cache: RefCell<Option<Box<StyledCache>>>,
    lineage: Arc<std::sync::Mutex<StyledLineage>>,
}
#[derive(Debug, Clone)]
struct StyledCache {
    style: Style,
    spans: Arc<[text::Span<'static, Uri>]>,
}

#[derive(Debug, Default)]
struct StyledLineage {
    edges: std::collections::VecDeque<(std::sync::Weak<Spans>, std::sync::Weak<Spans>)>,
}

#[derive(Debug)]
struct Spans {
    values: Vec<Span>,
    #[cfg(test)]
    disposed: Option<std::sync::mpsc::Sender<(std::thread::ThreadId, usize)>>,
}
impl Clone for Spans {
    fn clone(&self) -> Self {
        Self {
            values: self.values.clone(),
            #[cfg(test)]
            disposed: None,
        }
    }
}
impl std::ops::Deref for Spans {
    type Target = Vec<Span>;
    fn deref(&self) -> &Self::Target {
        &self.values
    }
}
impl std::ops::DerefMut for Spans {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.values
    }
}
#[cfg(test)]
impl Drop for Spans {
    fn drop(&mut self) {
        let values = std::mem::take(&mut self.values);
        let bytes = values.iter().map(|span| span.text().len()).sum();
        // Observe the real raw span Vec and its owned Strings after disposal,
        // not an unrelated token whose lifetime might end before the source.
        drop(values);
        if let Some(disposed) = self.disposed.take() {
            let _ = disposed.send((std::thread::current().id(), bytes));
        }
    }
}

/// Immutable input for a shared Flow owner. UI-local Text cells never cross
/// this boundary. Capture only existing backing, without plain/style work.
pub(super) struct FlowText {
    spans: Arc<Spans>,
    starts: Arc<[usize]>,
    plain: std::sync::OnceLock<Arc<str>>,
    // Retain every source/cache owner until raw Flow retirement, including
    // already-primed proof state and a cached style captured from the Text.
    _plain_source: Option<super::reader_source::Source>,
    style: Option<Style>,
    styled: Arc<[text::Span<'static, Uri>]>,
    lineage: Arc<std::sync::Mutex<StyledLineage>>,
}
impl FlowText {
    pub(super) fn len(&self) -> usize {
        self.starts.last().copied().unwrap_or_default()
    }
    pub(super) fn append_proven(&self, previous: &Self) -> bool {
        append_proven(
            &self.spans,
            &previous.spans,
            &self.lineage,
            &previous.lineage,
        )
    }
    pub(super) fn plain(&self) -> Arc<str> {
        self.plain
            .get_or_init(|| {
                #[cfg(test)]
                MATERIALIZED_BYTES.with(|bytes| bytes.set(bytes.get() + self.len()));
                self.spans.iter().map(Span::text).collect::<String>().into()
            })
            .clone()
    }
    pub(super) fn spans(&self, style: Style) -> Arc<[text::Span<'static, Uri>]> {
        if self.style == Some(style) {
            self.styled.clone()
        } else {
            #[cfg(test)]
            MATERIALIZED_BYTES.with(|bytes| bytes.set(bytes.get() + self.len()));
            self.spans.iter().map(|span| span.view(&style)).collect()
        }
    }
}

fn append_proven(
    spans: &Arc<Spans>,
    previous: &Arc<Spans>,
    lineage: &Arc<std::sync::Mutex<StyledLineage>>,
    old_lineage: &Arc<std::sync::Mutex<StyledLineage>>,
) -> bool {
    if Arc::ptr_eq(spans, previous) {
        return true;
    }
    if !Arc::ptr_eq(lineage, old_lineage) {
        return false;
    }
    let mut cursor = Arc::downgrade(previous);
    let target = Arc::downgrade(spans);
    let lineage = lineage.lock().unwrap_or_else(|poison| poison.into_inner());
    for (old, next) in &lineage.edges {
        if !old.ptr_eq(&cursor) {
            continue;
        }
        if next.ptr_eq(&target) {
            return true;
        }
        cursor = next.clone();
    }
    false
}

impl Text {
    /// Plain literal source; this never interprets Markdown or creates links.
    pub(super) fn literal(text: String) -> Self {
        Self::literal_linked(text, None)
    }
    pub(super) fn literal_linked(text: String, link: Option<Uri>) -> Self {
        Self::new(vec![Span::Standard {
            text,
            strikethrough: false,
            link,
            strong: false,
            emphasis: false,
            code: false,
        }])
    }
    pub(super) fn prepared_plain_proof(&self) -> Option<std::sync::Weak<str>> {
        self.plain.get().map(Arc::downgrade)
    }
    /// Semantic parser output, excluding materialization caches and weak
    /// append-proof lineage. Tests retain every raw span attribute and link.
    #[cfg(test)]
    pub(super) fn semantic_spans(&self) -> impl std::fmt::Debug + '_ {
        &self.spans.values
    }
    fn new(spans: Vec<Span>) -> Self {
        let mut starts = Vec::with_capacity(spans.len() + 1);
        starts.push(0);
        for span in &spans {
            starts.push(starts.last().copied().unwrap() + span.text().len());
        }
        Self {
            spans: Arc::new(Spans {
                values: spans,
                #[cfg(test)]
                disposed: None,
            }),
            starts: starts.into(),
            plain: std::cell::OnceCell::new(),
            plain_source: std::cell::OnceCell::new(),
            styled_cache: RefCell::default(),
            lineage: Default::default(),
        }
    }

    fn is_plain_source(&self, source: &str) -> bool {
        matches!(self.spans.as_slice(), [Span::Standard { text, strikethrough: false,
            link: None, strong: false, emphasis: false, code: false }] if text == source)
    }
    fn append_plain(&mut self, suffix: &str) {
        let previous = Arc::downgrade(&self.spans);
        if let Some(source) = self.plain_source.get_mut() {
            // Primed parser outputs share the old plain backing; update only
            // the suffix hash state before invalidating that display string.
            let [Span::Standard { text, .. }] = self.spans.as_slice() else {
                unreachable!("plain append has one source span")
            };
            source.append(text, suffix);
        }
        let [Span::Standard { text, .. }] = Arc::make_mut(&mut self.spans).as_mut_slice() else {
            unreachable!("plain append requires one plain span")
        };
        text.push_str(suffix);
        let length = self.starts.last().copied().unwrap_or_default() + suffix.len();
        self.starts = Arc::from([0, length]);
        self.plain.take();
        *self.styled_cache.get_mut() = None;
        self.record_edge(previous);
    }

    fn record_edge(&mut self, previous: std::sync::Weak<Spans>) {
        let mut lineage = self
            .lineage
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if lineage.edges.len() == 256 {
            lineage.edges.pop_front();
        }
        lineage
            .edges
            .push_back((previous, Arc::downgrade(&self.spans)));
    }
    fn record_prefix(&mut self, previous: &Text) {
        if self.spans.len() < previous.spans.len() {
            return;
        }
        if !previous
            .spans
            .iter()
            .zip(self.spans.iter())
            .enumerate()
            .all(|(index, (old, new))| old.prefix_of(new, index + 1 == previous.spans.len()))
        {
            return;
        }
        self.lineage = previous.lineage.clone();
        self.record_edge(Arc::downgrade(&previous.spans));
    }
    /// Bounded, lineage-only proof for a previously rendered styled prefix.
    /// This never scans text or materializes plain/styled spans. Reparsing
    /// records exact conservative proofs before publication; rewrites and
    /// Markdown reinterpretation establish a new lineage instead.
    pub fn append_proven(&self, previous: &Text) -> bool {
        append_proven(
            &self.spans,
            &previous.spans,
            &self.lineage,
            &previous.lineage,
        )
    }

    pub(super) fn flow_source(&self) -> FlowText {
        let plain = std::sync::OnceLock::new();
        if let Some(ready) = self.plain.get() {
            let _ = plain.set(ready.clone());
        }
        let (style, styled) = match self.styled_cache.borrow().as_ref() {
            Some(cached) => (Some(cached.style), cached.spans.clone()),
            None => (None, Arc::from([])),
        };
        FlowText {
            spans: self.spans.clone(),
            starts: self.starts.clone(),
            plain,
            _plain_source: self.plain_source.get().cloned(),
            style,
            styled,
            lineage: self.lineage.clone(),
        }
    }

    #[cfg(test)]
    pub(super) fn watch_raw_spans(
        &mut self,
        disposed: std::sync::mpsc::Sender<(std::thread::ThreadId, usize)>,
    ) {
        Arc::make_mut(&mut self.spans).disposed = Some(disposed);
    }

    /// Logical text without constructing styled presentation spans. Shared by
    /// the active transcript's selection/find index and bounded visible rows.
    #[cfg(test)]
    pub(super) fn plain_ready(&self) -> bool {
        self.plain.get().is_some()
    }
    pub fn plain(&self) -> Arc<str> {
        self.plain
            .get_or_init(|| {
                #[cfg(test)]
                MATERIALIZED_BYTES.with(|bytes| bytes.set(bytes.get() + self.len()));
                self.spans.iter().map(Span::text).collect::<String>().into()
            })
            .clone()
    }
    pub fn plain_prefixes(&self) -> Arc<Vec<[u8; 32]>> {
        self.plain_source
            .get_or_init(|| super::reader_source::Source::new(&self.plain()))
            .prefixes()
    }
    pub fn len(&self) -> usize {
        self.starts.last().copied().unwrap_or_default()
    }

    /// Style just a bounded rendered byte range; hidden chunks never populate
    /// the AST's whole-paragraph styled-spans cache.
    pub fn spans_range(
        &self,
        style: Style,
        range: Range<usize>,
    ) -> Arc<[text::Span<'static, Uri>]> {
        let first = self
            .starts
            .partition_point(|start| *start <= range.start)
            .saturating_sub(1);
        self.spans
            .iter()
            .enumerate()
            .skip(first)
            .take_while(|(index, _)| self.starts[*index] < range.end)
            .filter_map(|(index, span)| {
                let start = range.start.saturating_sub(self.starts[index]);
                let end = range
                    .end
                    .saturating_sub(self.starts[index])
                    .min(span.text().len());
                (start < end).then(|| span.view_part(&style, start..end))
            })
            .collect()
    }

    /// Returns the [`rich_text()`] spans ready to be used for the given style.
    ///
    /// This method performs caching for you. It will only reallocate if the [`Style`]
    /// provided changes.
    pub fn spans(&self, style: Style) -> Arc<[text::Span<'static, Uri>]> {
        let mut cached = self.styled_cache.borrow_mut();
        if cached.as_ref().is_none_or(|cached| cached.style != style) {
            #[cfg(test)]
            MATERIALIZED_BYTES.with(|bytes| bytes.set(bytes.get() + self.len()));
            *cached = Some(Box::new(StyledCache {
                style,
                spans: self.spans.iter().map(|span| span.view(&style)).collect(),
            }));
        }

        cached
            .as_ref()
            .expect("styled spans installed")
            .spans
            .clone()
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
        let Self::Standard {
            text: old,
            strikethrough: a,
            link: b,
            strong: c,
            emphasis: d,
            code: e,
        } = self;
        let Self::Standard {
            text: new,
            strikethrough: f,
            link: g,
            strong: h,
            emphasis: i,
            code: j,
        } = new;
        (a, b, c, d, e) == (f, g, h, i, j)
            && if last {
                new.starts_with(old)
            } else {
                new == old
            }
    }
    fn text(&self) -> &str {
        match self {
            Self::Standard { text, .. } => text,
        }
    }
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

type ReferenceLabel = UniCase<String>;
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reference {
    origin: usize,
    destination: Uri,
    title: String,
}
#[derive(Debug, Default, Clone)]
struct State {
    leftover: String,
    leftover_origin: usize,
    references: Arc<HashMap<ReferenceLabel, Reference>>,
    images: HashSet<Uri>,
}

fn collect_images(items: &[Item], images: &mut HashSet<Uri>) {
    for item in items {
        match item {
            Item::Image { url, .. } => {
                images.insert(url.clone());
            }
            Item::Quote(items) => collect_images(items, images),
            Item::List { bullets, .. } => {
                for bullet in bullets {
                    collect_images(bullet.items(), images);
                }
            }
            Item::Table { columns, rows } => {
                for column in columns {
                    collect_images(&column.header, images);
                }
                for row in rows {
                    for cell in &row.cells {
                        collect_images(cell, images);
                    }
                }
            }
            _ => {}
        }
    }
}

fn parse_with<'a>(
    mut state: impl BorrowMut<State> + 'a,
    markdown: &'a str,
    origin: Option<usize>,
) -> impl Iterator<Item = (Vec<Item>, &'a str, HashSet<ReferenceLabel>)> + 'a {
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

    let reference_labels = Rc::new(RefCell::new(HashSet::new()));

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

    let inherited: Arc<HashMap<ReferenceLabel, Reference>> = match origin {
        Some(origin) => Arc::new(
            state
                .borrow()
                .references
                .iter()
                .filter(|(_, reference)| reference.origin < origin)
                .map(|(label, reference)| (label.clone(), reference.clone()))
                .collect(),
        ),
        None => state.borrow().references.clone(),
    };
    let parser = pulldown_cmark::Parser::new_with_broken_link_callback(
        markdown,
        pulldown_cmark::Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
            | pulldown_cmark::Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS
            | pulldown_cmark::Options::ENABLE_TABLES
            | pulldown_cmark::Options::ENABLE_STRIKETHROUGH
            | pulldown_cmark::Options::ENABLE_TASKLISTS,
        {
            let references = inherited.clone();
            let reference_labels = reference_labels.clone();

            Some(move |broken_link: pulldown_cmark::BrokenLink<'_>| {
                let label = UniCase::new(broken_link.reference.into_string());
                RefCell::borrow_mut(&reference_labels).insert(label.clone());
                references.get(&label).map(|reference| {
                    (
                        pulldown_cmark::CowStr::from(reference.destination.to_string()),
                        pulldown_cmark::CowStr::from(reference.title.clone()),
                    )
                })
            })
        },
    );

    if let Some(origin) = origin {
        let mut references = inherited.as_ref().clone();
        for (label, definition) in parser.reference_definitions().iter() {
            references
                .entry(UniCase::new(label.to_owned()))
                .or_insert_with(|| Reference {
                    origin: origin + definition.span.start,
                    destination: Arc::from(definition.dest.as_ref()),
                    title: definition.title.as_deref().unwrap_or("").to_owned(),
                });
        }
        state.borrow_mut().references = Arc::new(references);
    }
    let references = state.borrow().references.clone();

    let produced_references = reference_labels.clone();
    let inline_items = Rc::new(RefCell::new(Vec::new()));
    let produce_batch = Rc::new(
        move |state: &mut State, stack: &mut Vec<Scope>, items: Vec<Item>, source: Range<usize>| {
            if items.is_empty() {
                return None;
            }
            if let Some(scope) = stack.last_mut() {
                for item in items {
                    match &mut *scope {
                        Scope::List(list) => {
                            list.bullets.last_mut().expect("item context").push(item)
                        }
                        Scope::Quote(items) => items.push(item),
                        Scope::Table { current, .. } => current.push(item),
                    }
                }
                None
            } else {
                state.leftover = markdown[source.start..].to_owned();
                if let Some(origin) = origin {
                    state.leftover_origin = origin + source.start;
                }
                Some((
                    items,
                    &markdown[source.start..source.end],
                    produced_references.take(),
                ))
            }
        },
    );
    let produce = {
        let inline_items = inline_items.clone();
        let produce_batch = produce_batch.clone();
        move |state: &mut State, stack: &mut Vec<Scope>, item: Item, source: Range<usize>| {
            let mut items = inline_items.take();
            items.push(item);
            produce_batch(state, stack, items, source)
        }
    };
    let flush_inline = {
        let inline_items = inline_items.clone();
        let produce_batch = produce_batch.clone();
        move |state: &mut State, stack: &mut Vec<Scope>, source: Range<usize>| {
            produce_batch(state, stack, inline_items.take(), source)
        }
    };
    let mut inline_heading = None;

    let parser = parser.into_offset_iter();

    // We want to keep the `spans` capacity
    #[allow(clippy::drain_collect)]
    parser.filter_map(move |(event, source)| match event {
        pulldown_cmark::Event::Start(tag) => match tag {
            pulldown_cmark::Tag::Heading { level, .. } if !metadata => {
                inline_heading = Some(level);
                None
            }
            pulldown_cmark::Tag::Paragraph if !metadata => {
                inline_heading = None;
                None
            }
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
            pulldown_cmark::Tag::Link { dest_url, id, .. } if !metadata => {
                let definition = if id.is_empty() {
                    None
                } else {
                    let label = UniCase::new(id.into_string());
                    RefCell::borrow_mut(&reference_labels).insert(label.clone());
                    references.get(&label)
                };
                link = Some(definition.map_or_else(
                    || Arc::from(dest_url.as_ref()),
                    |reference| reference.destination.clone(),
                ));
                None
            }
            pulldown_cmark::Tag::Image {
                dest_url,
                title,
                id,
                ..
            } if !metadata => {
                if !spans.is_empty() {
                    let text = Text::new(spans.drain(..).collect());
                    RefCell::borrow_mut(&inline_items).push(match inline_heading {
                        Some(level) => Item::Heading(level, text),
                        None => Item::Paragraph(text),
                    });
                }
                let definition = if id.is_empty() {
                    None
                } else {
                    let label = UniCase::new(id.into_string());
                    RefCell::borrow_mut(&reference_labels).insert(label.clone());
                    references.get(&label)
                };
                image = Some(definition.map_or_else(
                    || (Arc::from(dest_url.as_ref()), title.into_string()),
                    |reference| (reference.destination.clone(), reference.title.clone()),
                ));
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
            pulldown_cmark::Tag::CodeBlock(pulldown_cmark::CodeBlockKind::Fenced(language))
                if !metadata =>
            {
                code_block = true;
                code_language = (!language.is_empty()).then(|| language.into_string());

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
            pulldown_cmark::TagEnd::Heading(level) if !metadata => {
                inline_heading = None;
                if spans.is_empty() {
                    flush_inline(state.borrow_mut(), &mut stack, source)
                } else {
                    produce(
                        state.borrow_mut(),
                        &mut stack,
                        Item::Heading(level, Text::new(spans.drain(..).collect())),
                        source,
                    )
                }
            }
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
                    flush_inline(state.borrow_mut(), &mut stack, source)
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
                    flush_inline(state.borrow_mut(), &mut stack, source)
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

                produce(state.borrow_mut(), &mut stack, Item::Quote(quote), source)
            }
            pulldown_cmark::TagEnd::Image if !metadata => {
                let (url, title) = image.take()?;
                let alt = Text::new(spans.drain(..).collect());

                let state = state.borrow_mut();
                let _ = state.images.insert(url.clone());

                let title_text = Text::new(vec![Span::Standard {
                    text: title.clone(),
                    strikethrough: false,
                    link: None,
                    strong: false,
                    emphasis: false,
                    code: false,
                }]);
                RefCell::borrow_mut(&inline_items).push(Item::Image {
                    url,
                    title,
                    title_text,
                    alt,
                });
                None
            }
            pulldown_cmark::TagEnd::CodeBlock if !metadata => {
                code_block = false;

                let display: Arc<str> = Arc::from(mem::take(&mut code));
                produce(
                    state.borrow_mut(),
                    &mut stack,
                    Item::CodeBlock {
                        language: code_language.take(),
                        source: display.clone(),
                        reader_source: super::reader_source::Source::new(&display),
                        code: display,
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
                        source.clone(),
                    );
                }

                let _ = flush_inline(state.borrow_mut(), &mut stack, source.clone());

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
        pulldown_cmark::Event::InlineHtml(h) if !metadata && h.eq_ignore_ascii_case("<br>") => {
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
        pulldown_cmark::Event::Rule => produce(state.borrow_mut(), &mut stack, Item::Rule, source),
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
                        $module::Item::Heading(level, text) => {
                            out.push(format!("heading {level:?} {}", ($plain)(text)))
                        }
                        $module::Item::Paragraph(text) => {
                            out.push(format!("paragraph {}", ($plain)(text)))
                        }
                        $module::Item::CodeBlock { language, code, .. } => {
                            out.push(format!("code {language:?} {code}"))
                        }
                        $module::Item::Quote(items) => {
                            out.push("quote".into());
                            out.extend($name(items));
                            out.push("/quote".into());
                        }
                        $module::Item::List { start, bullets } => {
                            out.push(format!("list {start:?}"));
                            for bullet in bullets {
                                let items = match bullet {
                                    $module::Bullet::Point { items } => {
                                        out.push("bullet".into());
                                        items
                                    }
                                    $module::Bullet::Task { items, done } => {
                                        out.push(format!("task {done}"));
                                        items
                                    }
                                };
                                out.extend($name(items));
                                out.push("/bullet".into());
                            }
                            out.push("/list".into());
                        }
                        $module::Item::Table { columns, rows } => {
                            out.push("table".into());
                            for column in columns {
                                out.push(format!("column {:?}", column.alignment));
                                out.extend($name(&column.header));
                            }
                            for row in rows {
                                out.push("row".into());
                                for cell in &row.cells {
                                    out.push("cell".into());
                                    out.extend($name(cell));
                                }
                            }
                            out.push("/table".into());
                        }
                        $module::Item::Image {
                            url, title, alt, ..
                        } => out.push(format!("image {url} {title} {}", ($plain)(alt))),
                        $module::Item::Rule => out.push("rule".into()),
                    }
                }
                out
            }
        };
    }
    outline!(native_outline, native, |text: &native::Text| text
        .plain()
        .to_string());
    outline!(original_outline, original, |text: &original::Text| {
        let style = crate::design::markdown_settings(&cosmic::Theme::dark(), 14.0).style;
        text.spans(style)
            .iter()
            .map(|span| span.text.as_ref())
            .collect::<String>()
    });

    fn paragraph_text(content: &Content) -> Text {
        match content.items() {
            [Item::Paragraph(text)] => text.clone(),
            other => panic!("expected paragraph: {other:?}"),
        }
    }

    #[test]
    fn cold_parsed_graph_and_document_clone_keep_compact_unstyled_text() {
        let source = "paragraph **λ linked [text](https://example.test)**\n\n![alt](image.png \"title\")\n\n".repeat(1024);
        let content = Content::parse(&source);
        assert!(content.items().len() >= 2048);
        let Item::Paragraph(first) = &content.items()[0] else {
            panic!("real parsed paragraph")
        };
        let materialized = materialized_bytes();
        let cloned = content.clone();
        let document = super::super::prose_document::Document::snapshot(&content);
        assert_eq!(
            materialized_bytes(),
            materialized,
            "cloning cold publications must not style text"
        );
        assert_eq!(
            native_outline(content.items()),
            native_outline(cloned.items())
        );
        assert_eq!(
            native_outline(content.items()),
            native_outline(document.items())
        );
        for items in [content.items(), cloned.items(), document.items()] {
            let Item::Paragraph(copy) = &items[0] else {
                panic!("same parsed shape")
            };
            assert!(
                Arc::ptr_eq(&first.spans, &copy.spans),
                "actual raw span ownership stays shared"
            );
            assert!(
                copy.styled_cache.borrow().is_none(),
                "cold clone has no styled allocation"
            );
        }
        // Item's largest variant embeds two Text nodes. Bound the cold graph
        // representation independently of whether a particular theme is used.
        assert!(
            std::mem::size_of::<Text>() <= 192,
            "cold Text bytes: {}",
            std::mem::size_of::<Text>()
        );
        assert!(
            std::mem::size_of::<Item>() <= 448,
            "cold Item bytes: {}",
            std::mem::size_of::<Item>()
        );
    }

    #[test]
    fn parsed_style_cache_clone_flow_append_and_restyle_keep_exact_spans() {
        let mut content = Content::parse("hello α");
        let style = crate::design::markdown_settings(&cosmic::Theme::dark(), 14.0).style;
        let Item::Paragraph(text) = &content.items()[0] else {
            panic!("paragraph")
        };
        let original = text.spans(style);
        assert!(Arc::ptr_eq(&original, &text.spans(style)));
        let cloned = text.clone();
        let flow = cloned.flow_source();
        assert!(Arc::ptr_eq(&original, &cloned.spans(style)));
        assert!(Arc::ptr_eq(&original, &flow.spans(style)));

        content.push_str(" β");
        let Item::Paragraph(text) = &content.items()[0] else {
            panic!("appended paragraph")
        };
        assert!(
            text.styled_cache.borrow().is_none(),
            "append invalidates old presentation"
        );
        let appended = text.spans(style);
        assert!(!Arc::ptr_eq(&original, &appended));
        assert_eq!(
            appended
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>(),
            "hello α β"
        );
        assert_eq!(
            flow.spans(style)
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>(),
            "hello α"
        );

        let mut changed = style;
        changed.link_color = cosmic::iced::Color::from_rgb(0.8, 0.2, 0.1);
        let restyled = text.spans(changed);
        assert!(!Arc::ptr_eq(&appended, &restyled));
        assert_eq!(
            restyled
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>(),
            "hello α β"
        );
        assert!(Arc::ptr_eq(&restyled, &text.spans(changed)));
        assert_eq!(
            text.spans_range(changed, 0..5)
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>(),
            "hello"
        );
        assert!(
            Arc::ptr_eq(&restyled, &text.spans(changed)),
            "range extraction preserves the full style cache"
        );

        let linked = Content::parse("[link](https://example.test)");
        let Item::Paragraph(link) = &linked.items()[0] else {
            panic!("parsed link")
        };
        assert_eq!(link.spans(style)[0].color, Some(style.link_color));
        assert_eq!(link.spans(changed)[0].color, Some(changed.link_color));
    }

    #[test]
    fn cloned_code_descriptors_share_the_entire_body_and_release_it_together() {
        let content = Content::parse(&format!("```text\n{}\n```", "x".repeat(1024 * 1024)));
        let Item::CodeBlock { code, .. } = &content.items()[0] else {
            panic!("code fixture")
        };
        let weak = Arc::downgrade(code);
        let copies: Vec<_> = (0..1000).map(|_| content.items()[0].clone()).collect();
        assert_eq!(Arc::strong_count(code), 2002);
        assert!(
            copies
                .iter()
                .all(|item| matches!(item,Item::CodeBlock{code:copy,..} if Arc::ptr_eq(code,copy)))
        );
        drop(copies);
        drop(content);
        assert!(weak.upgrade().is_none());
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
        assert!(
            released.upgrade().is_none(),
            "proof edges must not own source bodies"
        );
        let mut styled = Content::parse("word **bold** tail");
        let previous = paragraph_text(&styled);
        styled.push_str(" continuing");
        assert!(
            paragraph_text(&styled).append_proven(&previous),
            "unchanged emphasis/link/style prefix may complete progressively"
        );
    }

    #[test]
    fn styled_append_lineage_rejects_reinterpretation_rewrites_and_expired_proofs() {
        let mut content = Content::parse("word **bold");
        let previous = paragraph_text(&content);
        content.push_str("**");
        assert!(
            !paragraph_text(&content).append_proven(&previous),
            "raw append that changes emphasis is not a rendered prefix"
        );
        let previous = paragraph_text(&Content::parse("[label](https://old.example) tail"));
        let changed = paragraph_text(&Content::parse("[label](https://new.example) tail"));
        assert!(!changed.append_proven(&previous));
        let mut content = Content::parse("plain words");
        let first = paragraph_text(&content);
        for _ in 0..300 {
            content.push_str(" more");
        }
        let latest = paragraph_text(&content);
        assert!(
            !latest.append_proven(&first),
            "expired proof requires worker-side exact fallback"
        );
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
            assert_eq!(
                native_outline(native.items()),
                original_outline(original.items()),
                "{source:?}"
            );
            for item in native.items() {
                if let Item::CodeBlock { lines, .. } = item {
                    assert!(lines.is_empty());
                }
            }
        }
    }

    #[test]
    fn partial_table_rows_keep_their_header_context_at_every_utf8_boundary() {
        for source in [
            "| alpha | beta |\n| :--- | ---: |\n| a | **b** |\n| | empty |\n",
            "| H | B |\n|---|---|\n| V | W |\n\nOutside table.\n",
            "| H | B |\n|---|---|\n| V | W |\n  \nOutside table.\n",
        ] {
            let mut streamed = Content::new();
            let mut previous = 0;
            for end in (1..=source.len()).filter(|end| source.is_char_boundary(*end)) {
                streamed.push_str(&source[previous..end]);
                assert_eq!(
                    native_outline(streamed.items()),
                    native_outline(Content::parse(&source[..end]).items()),
                    "table prefix {:?}",
                    &source[..end]
                );
                previous = end;
            }
        }
    }

    fn reference_text(items: &[Item]) -> Vec<(char, Option<Uri>)> {
        let mut output = Vec::new();
        for item in items {
            match item {
                Item::Paragraph(text) | Item::Heading(_, text) | Item::Image { alt: text, .. } => {
                    for span in text.spans.iter() {
                        let Span::Standard { text, link, .. } = span;
                        output.extend(text.chars().map(|ch| (ch, link.clone())));
                    }
                }
                Item::Quote(items) => output.extend(reference_text(items)),
                Item::List { bullets, .. } => {
                    for bullet in bullets {
                        output.extend(reference_text(bullet.items()));
                    }
                }
                Item::Table { columns, rows } => {
                    for column in columns {
                        output.extend(reference_text(&column.header));
                    }
                    for row in rows {
                        for cell in &row.cells {
                            output.extend(reference_text(cell));
                        }
                    }
                }
                _ => {}
            }
        }
        output
    }

    #[test]
    fn reference_definitions_match_full_prefixes_without_losing_prior_items_or_targets() {
        assert_eq!(
            native_outline(Content::parse("before ![alt](/i) after").items()),
            vec!["paragraph before ", "image /i  alt", "paragraph  after"]
        );
        assert_eq!(
            native_outline(Content::parse("# before ![alt](/i) after").items()),
            vec!["heading H1 before ", "image /i  alt", "heading H1  after"]
        );
        for source in [
            "![alternative](https://example.com/a.png \"title\")\n\n[label][ref]\n\n[ref]: https://example.com\n",
            "[label][ref]\n\n[ref]: https://example.com \"growing title\"\n",
            "[ref]: https://first.example\n\n[label][REF]\n\n[ref]: https://second.example\n",
            "[Straße][strasse]\n\n[STRASSE]: /destination\n",
            "[label][ref]\n\n[ref]: <https://example.com>\n",
            "![alt][img]\n\n[img]: /one \"caption\"\n\n[img]: /two\n",
            "before ![alt][img] after\n\n[img]: /image.png\n",
            "# before ![alt][img] after\n\n[img]: /image.png\n",
            "before ![one][img] between ![two][img] after\n\n[img]: /image.png\n",
            "- before ![alt][img] after\n\n[img]: /image.png\n",
            "| H |\n|---|\n| before ![alt][img] after |\n\n[img]: /image.png\n",
            "- [label][ref]\n\n| H |\n|---|\n| [label][ref] |\n\n[ref]: /destination\n",
        ] {
            let mut streamed = Content::new();
            let mut previous = 0;
            for end in (1..=source.len()).filter(|end| source.is_char_boundary(*end)) {
                streamed.push_str(&source[previous..end]);
                let full = Content::parse(&source[..end]);
                assert_eq!(
                    native_outline(streamed.items()),
                    native_outline(full.items()),
                    "source prefix {:?}",
                    &source[..end]
                );
                assert_eq!(
                    reference_text(streamed.items()),
                    reference_text(full.items()),
                    "link prefix {:?}",
                    &source[..end]
                );
                assert_eq!(
                    streamed.images(),
                    full.images(),
                    "image prefix {:?}",
                    &source[..end]
                );
                previous = end;
            }
        }
        let content = Content::parse(
            "[ref]: https://first.example\n\n[label][REF]\n\n[ref]: https://second.example\n",
        );
        assert!(
            reference_text(content.items())
                .iter()
                .filter_map(|(_, link)| link.as_deref())
                .all(|link| link == "https://first.example")
        );
        assert!(
            reference_text(content.items())
                .iter()
                .any(|(_, link)| link.as_deref() == Some("https://first.example"))
        );
        let folded = Content::parse("[Straße][strasse]\n\n[STRASSE]: /destination\n");
        assert!(
            reference_text(folded.items())
                .iter()
                .any(|(_, link)| link.as_deref() == Some("/destination"))
        );
    }

    #[test]
    fn all_utf8_append_boundaries_match_a_whole_parse() {
        for source in CASES {
            let expected = native_outline(Content::parse(source).items());
            for stride in [1, 2, 7, 31] {
                let mut content = Content::new();
                let mut previous = 0;
                for end in (1..=source.len())
                    .filter(|end| source.is_char_boundary(*end))
                    .step_by(stride)
                    .chain(std::iter::once(source.len()))
                {
                    if end > previous {
                        content.push_str(&source[previous..end]);
                        previous = end;
                    }
                }
                assert_eq!(
                    native_outline(content.items()),
                    expected,
                    "stride {stride}, {source:?}"
                );
            }
        }
    }

    #[test]
    fn a_plain_stream_is_linear_and_delimiters_restore_commonmark_semantics() {
        let mut content = Content::parse("Answer");
        let before = content.parsed_bytes;
        let mut source = "Answer".to_owned();
        for _ in 0..10000 {
            content.push_str(" more ordinary α text");
            source.push_str(" more ordinary α text");
        }
        assert_eq!(content.parsed_bytes, before);
        for suffix in [
            " **",
            "bold",
            "**",
            "\n",
            "- item",
            "\n\n",
            "[link",
            "](https://example.com)",
        ] {
            content.push_str(suffix);
            source.push_str(suffix);
            assert_eq!(
                native_outline(content.items()),
                original_outline(original::Content::parse(&source).items()),
                "suffix {suffix:?}"
            );
        }
        for source in [
            "Alpha ordinary text.",
            "Alpha **strong** text",
            "Alpha &amp; entity",
            "Alpha <tag>text</tag>",
            "Alpha `code` end",
            "Alpha\n---",
            "Alpha [link](https://example.com)",
        ] {
            let mut content = Content::new();
            for end in (1..=source.len()).filter(|end| source.is_char_boundary(*end)) {
                let previous = source[..end].char_indices().last().unwrap().0;
                content.push_str(&source[previous..end]);
                assert_eq!(
                    native_outline(content.items()),
                    original_outline(original::Content::parse(&source[..end]).items()),
                    "prefix {:?}",
                    &source[..end]
                );
            }
        }
    }

    #[test]
    fn raw_eof_spaces_remain_pending_until_the_next_visible_character() {
        for initial in ["Answer", "Answer **bold** tail"] {
            let mut source = initial.to_owned();
            let mut content = Content::parse(initial);
            for suffix in [
                " ",
                " ",
                "α",
                " ",
                "new ",
                " ",
                "words",
                "  ",
                "**markup** ",
                " ",
                "after",
            ] {
                source.push_str(suffix);
                content.push_str(suffix);
                assert_eq!(
                    native_outline(content.items()),
                    original_outline(original::Content::parse(&source).items()),
                    "{source:?}"
                );
            }
        }
    }

    #[test]
    fn an_open_top_level_fence_parses_new_bytes_without_reparsing_its_body() {
        let mut content = Content::parse("```rust\nfn main() {}\n");
        let before = content.parsed_bytes;
        let mut source = "```rust\nfn main() {}\n".to_owned();
        for _ in 0..10000 {
            content.push_str("let value = 42;\n");
            source.push_str("let value = 42;\n");
        }
        assert_eq!(
            content.parsed_bytes, before,
            "the open body never re-enters CommonMark"
        );
        content.push_str("```\n\nAfter the fence.\n");
        source.push_str("```\n\nAfter the fence.\n");
        assert_eq!(
            native_outline(content.items()),
            original_outline(original::Content::parse(&source).items())
        );
        assert!(
            content.parsed_bytes < source.len() * 2,
            "closing costs one linear parse, not quadratic appends"
        );
    }

    #[test]
    fn extracting_a_visible_span_range_does_not_style_the_hidden_paragraph() {
        let source = format!(
            "{} **linked [αβ](https://example.com)** {}",
            "plain ".repeat(10000),
            "tail ".repeat(10000)
        );
        let content = Content::parse(&source);
        let Item::Paragraph(text) = &content.items()[0] else {
            panic!("paragraph")
        };
        let plain = text.plain();
        let style = crate::design::markdown_settings(&cosmic::Theme::dark(), 14.0).style;
        let at = plain.find("linked").unwrap();
        let spans = text.spans_range(style, at..at + "linked αβ".len());
        assert_eq!(
            spans
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>(),
            "linked αβ"
        );
        assert!(
            spans
                .iter()
                .any(|span| span.link.as_deref() == Some("https://example.com"))
        );
        assert!(
            text.styled_cache
                .borrow()
                .as_ref()
                .map(|cache| cache.style)
                .is_none(),
            "hidden text has no styled-span cache"
        );
        assert!(
            text.styled_cache
                .borrow()
                .as_ref()
                .is_none_or(|cache| cache.spans.is_empty())
        );
    }
}

#[test]
fn completed_source_discards_parser_scratch_without_discarding_rendered_content() {
    let mut content = Content::parse(&format!(
        "![alt](https://example.test/image.png)\n\n{}",
        "word ".repeat(16000)
    ));
    let before = format!("{:?}", content.items());
    let images = content.images().clone();
    assert!(content.state.leftover.capacity() > 64000);
    content.discard_continuation();
    assert_eq!(content.state.leftover.capacity(), 0);
    assert!(content.incomplete.is_empty());
    assert_eq!(format!("{:?}", content.items()), before);
    assert_eq!(content.images(), &images);
}
