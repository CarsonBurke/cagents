//! Immutable publication roots shared by UI factories without cloning subtrees.
//!
//! Text has UI-local caches, so roots use Rc rather than claiming Sync. Only
//! the raw Send graph crosses to the persistent reclaimer at its final release.
use super::*;
use std::rc::Rc;

/// Aggregate layout inputs are prepared with the immutable graph, never
/// discovered by recursively materializing short UI paragraphs.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) struct Stats {
    pub(super) bytes: usize,
    pub(super) leaves: usize,
    nodes: usize,
    prose_bytes: usize,
    prose_rows: usize,
    code_rows: usize,
    blocks: usize,
}
impl Stats {
    pub(super) fn inline(self) -> bool {
        self.bytes <= 4096 && self.nodes <= 16
    }
    fn vertical(&mut self, other: Self) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.leaves = self.leaves.saturating_add(other.leaves);
        self.nodes = self.nodes.saturating_add(other.nodes);
        self.prose_bytes = self.prose_bytes.saturating_add(other.prose_bytes);
        self.prose_rows = self.prose_rows.saturating_add(other.prose_rows);
        self.code_rows = self.code_rows.saturating_add(other.code_rows);
        self.blocks = self.blocks.saturating_add(other.blocks);
    }
    fn horizontal(&mut self, other: Self) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.leaves = self.leaves.saturating_add(other.leaves);
        self.nodes = self.nodes.saturating_add(other.nodes);
        self.prose_bytes = self.prose_bytes.max(other.prose_bytes);
        self.prose_rows = self.prose_rows.max(other.prose_rows);
        self.code_rows = self.code_rows.max(other.code_rows);
        self.blocks = self.blocks.max(other.blocks);
    }
    pub(super) fn estimate(self, settings: markdown::Settings) -> f32 {
        let prose_line = iced::widget::text::LineHeight::default()
            .to_absolute(settings.text_size)
            .0 as f64;
        let code_line = iced::widget::text::LineHeight::default()
            .to_absolute(settings.code_size)
            .0 as f64;
        let wrapped = (self.prose_bytes as f64 * settings.text_size.0 as f64 * 0.5
            / design::READABLE_MEASURE as f64)
            .ceil();
        let rows = (self.prose_rows as f64).max(wrapped);
        let value = rows * prose_line
            + self.code_rows as f64 * code_line
            + self.blocks.saturating_sub(1) as f64 * settings.spacing.0 as f64;
        // Estimates never become glyph/source authority. Bound the arithmetic
        // so an extreme recipe cannot publish NaN/Infinity into prefix sums.
        if value.is_finite() {
            value.clamp(prose_line.min(1.0).max(0.0), 1.0e12) as f32
        } else {
            1.0e12
        }
    }
}
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    #[default]
    Items,
    Item,
    Bullet,
    TableRow,
    Cell,
    ImageText,
}
#[derive(Default)]
struct Metadata {
    kind: Kind,
    stats: Stats,
    outline: std::ops::Range<usize>,
    children: Vec<Metadata>,
}
struct Leaf {
    path: Box<[u32]>,
    bytes: usize,
}
#[derive(Hash, PartialEq, Eq)]
struct Binding {
    item: Arc<str>,
    prefix: Box<[u32]>,
}
impl Metadata {
    fn items(
        items: &[markdown::Item],
        path: &mut Vec<u32>,
        outline: &mut Vec<Leaf>,
        kind: Kind,
    ) -> Self {
        let start = outline.len();
        let mut stats = Stats::default();
        let children = items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                path.push(index as u32);
                let metadata = Self::item(item, path, outline);
                path.pop();
                stats.vertical(metadata.stats);
                metadata
            })
            .collect();
        stats.nodes = stats.nodes.saturating_add(1);
        Self {
            kind,
            stats,
            outline: start..outline.len(),
            children,
        }
    }
    fn text(bytes: usize, path: &[u32], outline: &mut Vec<Leaf>, kind: Kind) -> Self {
        let start = outline.len();
        outline.push(Leaf {
            path: path.into(),
            bytes,
        });
        Self {
            kind,
            stats: Stats {
                bytes,
                leaves: 1,
                nodes: 1,
                prose_bytes: bytes,
                prose_rows: 1,
                blocks: 1,
                ..Stats::default()
            },
            outline: start..outline.len(),
            children: vec![],
        }
    }
    fn item(item: &markdown::Item, path: &mut Vec<u32>, outline: &mut Vec<Leaf>) -> Self {
        use markdown::Item;
        let start = outline.len();
        match item {
            Item::Paragraph(text) | Item::Heading(_, text) => {
                Self::text(text.len(), path, outline, Kind::Item)
            }
            Item::CodeBlock { code, .. } => {
                let mut metadata = Self::text(code.len(), path, outline, Kind::Item);
                metadata.stats.prose_bytes = 0;
                metadata.stats.prose_rows = 0;
                metadata.stats.code_rows = code
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count()
                    .saturating_add(usize::from(!code.ends_with('\n')))
                    .max(1);
                metadata
            }
            Item::Quote(items) => Self::items(items, path, outline, Kind::Item),
            Item::List { bullets, .. } => {
                let mut stats = Stats::default();
                let children = bullets
                    .iter()
                    .enumerate()
                    .map(|(index, bullet)| {
                        let (markdown::Bullet::Point { items }
                        | markdown::Bullet::Task { items, .. }) = bullet;
                        path.push(index as u32);
                        let metadata = Self::items(items, path, outline, Kind::Bullet);
                        path.pop();
                        stats.vertical(metadata.stats);
                        metadata
                    })
                    .collect();
                stats.nodes = stats.nodes.saturating_add(1);
                Self {
                    kind: Kind::Item,
                    stats,
                    outline: start..outline.len(),
                    children,
                }
            }
            Item::Table { columns, rows } => {
                let mut stats = Stats::default();
                let children = (0..=rows.len())
                    .map(|row| {
                        path.push(row as u32);
                        let row_start = outline.len();
                        let mut row_stats = Stats::default();
                        let cells = columns
                            .iter()
                            .enumerate()
                            .map(|(column, header)| {
                                let items = if row == 0 {
                                    header.header.as_slice()
                                } else {
                                    rows[row - 1]
                                        .cells
                                        .get(column)
                                        .map_or(&[][..], Vec::as_slice)
                                };
                                path.push(column as u32);
                                let metadata = Self::items(items, path, outline, Kind::Cell);
                                path.pop();
                                row_stats.horizontal(metadata.stats);
                                metadata
                            })
                            .collect();
                        path.pop();
                        row_stats.nodes = row_stats.nodes.saturating_add(1);
                        stats.vertical(row_stats);
                        Self {
                            kind: Kind::TableRow,
                            stats: row_stats,
                            outline: row_start..outline.len(),
                            children: cells,
                        }
                    })
                    .collect();
                stats.nodes = stats.nodes.saturating_add(1);
                Self {
                    kind: Kind::Item,
                    stats,
                    outline: start..outline.len(),
                    children,
                }
            }
            Item::Image {
                alt,
                title,
                title_text,
                ..
            } => {
                let mut children = Vec::new();
                let mut stats = Stats::default();
                for (index, text, shown) in
                    [(0, alt, alt.len() > 0), (1, title_text, !title.is_empty())]
                {
                    path.push(index);
                    let metadata = if shown {
                        Self::text(text.len(), path, outline, Kind::ImageText)
                    } else {
                        Self::default()
                    };
                    path.pop();
                    stats.vertical(metadata.stats);
                    children.push(metadata);
                }
                stats.prose_rows = stats.prose_rows.saturating_add(2);
                stats.nodes = stats.nodes.saturating_add(1);
                Self {
                    kind: Kind::Item,
                    stats,
                    outline: start..outline.len(),
                    children,
                }
            }
            Item::Rule => Self {
                kind: Kind::Item,
                stats: Stats {
                    nodes: 1,
                    prose_rows: 1,
                    blocks: 1,
                    ..Stats::default()
                },
                outline: start..start,
                children: vec![],
            },
        }
    }
}

pub(super) struct Data {
    items: Vec<markdown::Item>,
    metadata: Metadata,
    outline: Vec<Leaf>,
    bound: std::cell::RefCell<HashMap<Binding, Arc<[virtualized::LogicalRange]>>>,
    #[cfg(test)]
    pub(super) created_on: std::thread::ThreadId,
    #[cfg(test)]
    disposed: Option<std::sync::mpsc::Sender<(std::thread::ThreadId, usize)>>,
}
impl Data {
    /// Full transcript publications and rich coordination parts snapshot on
    /// admitted parser workers. The bounded inline cache route also uses this
    /// constructor; it does not intrinsically bound the supplied graph.
    pub(super) fn snapshot(content: &markdown::Content) -> Self {
        Self::items(content.items().to_vec())
    }
    pub(super) fn items(items: Vec<markdown::Item>) -> Self {
        let mut outline = Vec::new();
        let metadata = Metadata::items(&items, &mut Vec::new(), &mut outline, Kind::Items);
        Self {
            items,
            metadata,
            outline,
            bound: Default::default(),
            #[cfg(test)]
            created_on: std::thread::current().id(),
            #[cfg(test)]
            disposed: None,
        }
    }
    #[cfg(test)]
    pub(super) fn watch_disposal(
        &mut self,
        sender: std::sync::mpsc::Sender<(std::thread::ThreadId, usize)>,
    ) {
        self.disposed = Some(sender);
    }
}
impl Drop for Data {
    fn drop(&mut self) {
        let items = std::mem::take(&mut self.items);
        #[cfg(test)]
        let count = items.len();
        // This is the actual graph, not an unrelated lease/token. Reclaimer
        // batches drop raw Data inline; no nested retirement can bypass a FIFO
        // generation barrier or release admission before these destructors.
        drop(items);
        drop(std::mem::take(&mut self.metadata));
        drop(std::mem::take(&mut self.outline));
        self.bound.get_mut().clear();
        #[cfg(test)]
        if let Some(disposed) = self.disposed.take() {
            let _ = disposed.send((std::thread::current().id(), count));
        }
    }
}
struct Owner(Option<Data>);
impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(data) = self.0.take() {
            preparation::retire_document(data);
        }
    }
}
#[derive(Clone)]
pub(super) struct Document(Rc<Owner>);
pub(super) enum Node<'a> {
    Items(&'a [markdown::Item]),
    Item(&'a markdown::Item),
    Bullet(&'a markdown::Bullet),
}
impl Document {
    pub(super) fn new(data: Data) -> Self {
        Self(Rc::new(Owner(Some(data))))
    }
    /// Snapshot CachedItem::extend's inline cache input once per cache change,
    /// including empty loading placeholders. This clones the supplied graph;
    /// Runtime routes full changed prose through admission instead. Disclosure
    /// and row factories reuse prepared roots; full workers transfer Data via new.
    pub(super) fn snapshot(content: &markdown::Content) -> Self {
        Self::new(Data::snapshot(content))
    }
    #[cfg(test)]
    pub(super) fn from_items(items: &[markdown::Item]) -> Self {
        Self::new(Data::items(items.to_vec()))
    }
    pub(super) fn items(&self) -> &[markdown::Item] {
        &self.0.0.as_ref().expect("live document root").items
    }
    fn metadata(&self, path: &[u32]) -> Option<&Metadata> {
        let mut metadata = &self.0.0.as_ref()?.metadata;
        for &index in path {
            metadata = metadata.children.get(index as usize)?;
        }
        Some(metadata)
    }
    pub(super) fn stats(&self, path: &[u32]) -> Stats {
        self.metadata(path)
            .map_or_else(Stats::default, |metadata| metadata.stats)
    }
    /// Logical identities depend on the caller's item/root prefix. Bind the
    /// worker's flat outline once per immutable document and namespace; future
    /// rows share slices rather than walking or cloning the complete graph.
    /// The first bind remains O(logical metadata) UI work, without body scans.
    pub(super) fn logical_ranges(
        &self,
        item: &Arc<str>,
        prefix: &[u32],
        path: &[u32],
        chunk: bool,
    ) -> virtualized::LogicalRanges {
        let Some(metadata) = self.metadata(path) else {
            return Default::default();
        };
        let data = self.0.0.as_ref().expect("live document");
        let key = Binding {
            item: item.clone(),
            prefix: prefix.into(),
        };
        let mut bound = data.bound.borrow_mut();
        let source = bound
            .entry(key)
            .or_insert_with(|| {
                data.outline
                    .iter()
                    .map(|leaf| {
                        let mut path = Vec::with_capacity(prefix.len() + leaf.path.len());
                        path.extend_from_slice(prefix);
                        path.extend_from_slice(&leaf.path);
                        virtualized::LogicalRange {
                            identity: selection::logical_identity(item, &path),
                            bytes: 0..leaf.bytes,
                            chunk: false,
                        }
                    })
                    .collect::<Vec<_>>()
                    .into()
            })
            .clone();
        virtualized::LogicalRanges::shared(source, metadata.outline.clone(), chunk)
    }
    pub(super) fn node(&self, path: &[u32]) -> Option<Node<'_>> {
        fn descend<'a>(items: &'a [markdown::Item], path: &[u32]) -> Option<Node<'a>> {
            let Some((&index, rest)) = path.split_first() else {
                return Some(Node::Items(items));
            };
            let item = items.get(index as usize)?;
            if rest.is_empty() {
                return Some(Node::Item(item));
            }
            match item {
                markdown::Item::Quote(items) => descend(items, rest),
                markdown::Item::List { bullets, .. } => {
                    let (&index, rest) = rest.split_first()?;
                    let bullet = bullets.get(index as usize)?;
                    if rest.is_empty() {
                        return Some(Node::Bullet(bullet));
                    }
                    let (markdown::Bullet::Point { items } | markdown::Bullet::Task { items, .. }) =
                        bullet;
                    descend(items, rest)
                }
                markdown::Item::Table { columns, rows } => {
                    let (&row, rest) = rest.split_first()?;
                    let (&column, rest) = rest.split_first()?;
                    let items: &[markdown::Item] = if row == 0 {
                        &columns.get(column as usize)?.header
                    } else {
                        rows.get(row as usize - 1)?
                            .cells
                            .get(column as usize)
                            .map(Vec::as_slice)
                            .unwrap_or(&[])
                    };
                    descend(items, rest)
                }
                _ => None,
            }
        }
        descend(self.items(), path)
    }
}

#[cfg(test)]
pub(super) fn recursive_quote_fixture(depth: usize) -> String {
    fn quote(source: &str) -> String {
        source
            .split('\n')
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn tree(depth: usize, next: &mut usize, last: usize) -> String {
        if depth == 0 {
            let index = *next;
            *next += 1;
            return format!(
                "leaf {index} {}{}",
                "word ".repeat(380),
                if index == last {
                    " FW16_RECURSIVE_NEEDLE"
                } else {
                    ""
                }
            );
        }
        let first = tree(depth - 1, next, last);
        let second = tree(depth - 1, next, last);
        format!(
            "lead {depth}\n\n{}\n\nseparator {depth}\n\n{}",
            quote(&first),
            quote(&second)
        )
    }
    quote(&tree(depth, &mut 0, (1usize << depth) - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_recursive_quotes_prepare_aggregate_topology_and_flat_outline_on_worker() {
        let source = recursive_quote_fixture(8);
        assert!(source.len() > 500_000);
        let data = std::thread::spawn(move || {
            let parsed = markdown::Content::parse(&source);
            Data::snapshot(&parsed)
        })
        .join()
        .unwrap();
        assert_ne!(data.created_on, std::thread::current().id());
        fn inspect(items: &[markdown::Item], small: &mut usize, four: &mut usize) {
            assert!(
                items.len() <= 4,
                "actual recursive grammar never triggers the old local >4 gate"
            );
            if items.len() == 4 {
                *four += 1;
            }
            for item in items {
                match item {
                    markdown::Item::Paragraph(text) => {
                        assert!(text.len() <= 2048);
                        *small += 1;
                    }
                    markdown::Item::Quote(items) => inspect(items, small, four),
                    _ => panic!("expected real paragraph/quote parser topology"),
                }
            }
        }
        let (mut small, mut four) = (0, 0);
        inspect(&data.items, &mut small, &mut four);
        assert!(small > 500 && four > 200);
        assert_eq!(data.metadata.stats.leaves, small);
        assert!(!data.metadata.stats.inline());
        assert_eq!(data.outline.len(), small);
        let settings = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE);
        assert!(
            data.metadata.stats.estimate(settings) > 800.0 * 10.0,
            "large composite estimates prevent each nested initial window mounting its complete tree"
        );
        assert!(data.metadata.stats.estimate(settings).is_finite());
    }
    #[test]
    fn shared_outline_matches_actual_catalog_paths_for_quote_list_table_image_and_code() {
        let source = "# Heading\n\n> before **bold**\n>\n> - one\n> - two\n\n| First | Second |\n| --- | --- |\n| cell one | cell two |\n\n![alt text](https://example.test/image \"literal title\")\n\n```text\nfirst\nsecond\n```\n";
        let data = std::thread::spawn(move || {
            let parsed = markdown::Content::parse(source);
            Data::snapshot(&parsed)
        })
        .join()
        .unwrap();
        let document = Document::new(data);
        let id: Arc<str> = Arc::from("opaque:item");
        let mut builder = selection::Catalog::builder();
        builder.markdown(&id, &[0], document.items());
        let catalog = builder.finish();
        let expected = catalog.owned_search_blocks(&HashSet::from([id.to_string()]));
        let actual = document
            .logical_ranges(&id, &[0], &[], false)
            .iter()
            .map(|logical| (logical.identity, logical.bytes.end))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            expected
                .iter()
                .map(|(_, identity, text)| (*identity, text.len()))
                .collect::<Vec<_>>()
        );
        let code = document
            .items()
            .iter()
            .position(|item| matches!(item, markdown::Item::CodeBlock { .. }))
            .unwrap();
        assert_eq!(
            document.stats(&[code as u32]).code_rows,
            2,
            "physical terminal newline does not invent another code row"
        );
        let table = document
            .items()
            .iter()
            .position(|item| matches!(item, markdown::Item::Table { .. }))
            .unwrap();
        assert_eq!(
            document.metadata(&[table as u32, 0]).unwrap().kind,
            Kind::TableRow
        );
        assert_eq!(
            document.metadata(&[table as u32, 0, 0]).unwrap().kind,
            Kind::Cell
        );
        let chunk = document.logical_ranges(&id, &[0], &[table as u32, 0, 0], true);
        assert!(chunk.iter().all(|logical| logical.chunk));
        assert!(chunk.iter().all(|logical| {
            actual
                .iter()
                .any(|(identity, _)| *identity == logical.identity)
        }));
        assert_eq!(
            document.0.0.as_ref().unwrap().bound.borrow().len(),
            1,
            "all composite rows reuse the same first namespace binding"
        );
        let first = document
            .0
            .0
            .as_ref()
            .unwrap()
            .bound
            .borrow()
            .values()
            .next()
            .unwrap()
            .clone();
        drop(document.logical_ranges(&id, &[0], &[], false));
        let second = document
            .0
            .0
            .as_ref()
            .unwrap()
            .bound
            .borrow()
            .values()
            .next()
            .unwrap()
            .clone();
        assert!(
            Arc::ptr_eq(&first, &second),
            "view rebuilds share the immutable outline allocation"
        );
    }
}
