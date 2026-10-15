//! One selection across the transcript's independently shaped prose widgets.
//! Operations discover the current widget tree, including retained exchanges;
//! endpoints use widget identities so reflow never changes what was selected.
use super::selectable_prose::State as Prose;
use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use iced::{Event, Point, Rectangle, Size, Vector};
use std::any::Any;
use std::ops::Range;
use std::sync::atomic::AtomicU64;
use std::{cell::RefCell, rc::Rc};
use unicode_segmentation::UnicodeSegmentation;

mod boundary;
mod document;
pub(super) use boundary::{Kind, wrap};
pub(super) use document::{
    Bookmark, Builder as DocumentBuilder, Catalog, identity as logical_identity,
};

static NEXT: AtomicU64 = AtomicU64::new(1);
static ACTIVE: AtomicU64 = AtomicU64::new(0);

pub(super) fn transcript<'a>(
    content: impl Into<Element<'a, Message>>,
    target: Target,
    subagent: Option<&'a str>,
) -> Element<'a, Message> {
    Element::new(Selection {
        catalog: None,
        bookmark: None,
        content: content.into(),
        context: Some((target, subagent)),
        click_clock: iced::time::Instant::now,
    })
}

pub(super) fn transcript_virtualized<'a>(
    content: impl Into<Element<'a, Message>>,
    target: Target,
    subagent: Option<&'a str>,
    catalog: Arc<Catalog>,
    bookmark: Rc<RefCell<Bookmark>>,
) -> Element<'a, Message> {
    Element::new(Selection {
        content: content.into(),
        context: Some((target, subagent)),
        catalog: Some(catalog),
        bookmark: Some(bookmark),
        click_clock: iced::time::Instant::now,
    })
}

struct Selection<'a> {
    catalog: Option<Arc<Catalog>>,
    bookmark: Option<Rc<RefCell<Bookmark>>>,
    content: Element<'a, Message>,
    context: Option<(Target, Option<&'a str>)>,
    click_clock: fn() -> iced::time::Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Endpoint {
    identity: u64,
    byte: usize,
}

struct State {
    identity: u64,
    context: Option<(Target, Option<String>)>,
    reset: bool,
    anchor: Option<Endpoint>,
    end: Option<Endpoint>,
    dragging: bool,
    focused: bool,
    link: Option<(Target, String)>,
    pointer: Option<Point>,
    origin: Option<Point>,
    moved: bool,
    modifiers: keyboard::Modifiers,
    click: Option<mouse::Click>,
    unit: Unit,
    initial: Option<(Endpoint, Endpoint)>,
    next_scroll: Option<iced::time::Instant>,
}

#[derive(Clone, Copy, Default)]
enum Unit {
    #[default]
    Character,
    Word,
    Paragraph,
}

impl Unit {
    fn range(self, text: &str, byte: usize) -> Range<usize> {
        let byte = byte.min(text.len());
        match self {
            Self::Character => byte..byte,
            Self::Word => text
                .split_word_bound_indices()
                .find(|(start, word)| *start <= byte && byte < start + word.len())
                .map(|(start, word)| start..start + word.len())
                .or_else(|| {
                    (byte == text.len())
                        .then(|| text.split_word_bound_indices().last())
                        .flatten()
                        .map(|(start, word)| start..start + word.len())
                })
                .unwrap_or(byte..byte),
            Self::Paragraph => {
                let start = text[..byte].rfind('\n').map_or(0, |newline| newline + 1);
                let end = text[byte..]
                    .find('\n')
                    .map_or(text.len(), |newline| byte + newline + 1);
                start..end
            }
        }
    }
}

impl State {
    fn extend(&mut self, read: &Read, block: &Block) {
        let range = self.unit.range(&block.plain, block.hit);
        let low = Endpoint {
            identity: block.identity,
            byte: range.start,
        };
        let high = Endpoint {
            identity: block.identity,
            byte: range.end,
        };
        if let Some((start, end)) = self.initial {
            let index = |point: Endpoint| {
                if let Some(catalog) = &read.catalog {
                    catalog
                        .position(point.identity)
                        .map(|index| (index, point.byte))
                } else {
                    read.blocks
                        .iter()
                        .position(|block| block.identity == point.identity)
                        .map(|index| (index, point.byte))
                }
            };
            if index(low) < index(start) {
                self.anchor = Some(end);
                self.end = Some(low);
            } else {
                self.anchor = Some(start);
                self.end = Some(high);
            }
        } else {
            self.end = Some(high);
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            identity: NEXT.fetch_add(1, Ordering::Relaxed),
            context: None,
            reset: false,
            anchor: None,
            end: None,
            dragging: false,
            focused: false,
            link: None,
            pointer: None,
            origin: None,
            moved: false,
            modifiers: keyboard::Modifiers::empty(),
            click: None,
            unit: Unit::Character,
            initial: None,
            next_scroll: None,
        }
    }
}

impl operation::Focusable for State {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn focus(&mut self) {
        self.focused = true;
        ACTIVE.store(self.identity, Ordering::Relaxed);
    }
    fn unfocus(&mut self) {
        self.focused = false;
        self.dragging = false;
        self.link = None;
    }
}

struct Frame {
    identity: u64,
    kind: Kind,
    first_block: usize,
}
#[derive(Clone)]
struct ListMarker {
    marker: String,
    indent: usize,
    content_indent: usize,
    first: bool,
}
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Cell {
    table: u64,
    row: usize,
    column: usize,
}
struct Table {
    columns: usize,
    rows: usize,
    cells: HashMap<(usize, usize), usize>,
    first_block: usize,
    last_block: usize,
    has_text: bool,
    lists: Vec<ListMarker>,
}
struct Ranges {
    blocks: Vec<Range<usize>>,
    lo: (usize, usize),
    hi: (usize, usize),
    logical: Option<(Endpoint, Endpoint)>,
}
impl std::ops::Deref for Ranges {
    type Target = [Range<usize>];
    fn deref(&self) -> &Self::Target {
        &self.blocks
    }
}

struct Block {
    identity: u64,
    plain: Arc<str>,
    bounds: Rectangle,
    visible: Option<Rectangle>,
    hit: usize,
    link: Option<(Target, String)>,
    lists: Vec<ListMarker>,
    cell: Option<Cell>,
}

struct LogicalText {
    identity: u64,
    plain: Arc<str>,
}

struct Read {
    catalog: Option<Arc<Catalog>>,
    blocks: Vec<Block>,
    pointer: Option<Point>,
    translation: Vector,
    pending: Option<(Vector, Rectangle)>,
    clip: Option<Rectangle>,
    frames: Vec<Frame>,
    tables: HashMap<u64, Table>,
    table_order: Vec<u64>,
}

impl Read {
    fn new(pointer: Option<Point>) -> Self {
        Self {
            catalog: None,
            blocks: Vec::new(),
            pointer,
            translation: Vector::ZERO,
            pending: None,
            clip: None,
            frames: Vec::new(),
            tables: HashMap::new(),
            table_order: Vec::new(),
        }
    }

    fn with_catalog(pointer: Option<Point>, catalog: Option<Arc<Catalog>>) -> Self {
        Self {
            catalog,
            ..Self::new(pointer)
        }
    }
    fn list_markers(&self) -> Vec<ListMarker> {
        let mut lists = Vec::new();
        let mut indent = 0;
        for frame in &self.frames {
            if let Kind::ListItem(marker) = &frame.kind {
                // Checkbox text is content, not the Markdown list marker width.
                let width = marker.split_whitespace().next().unwrap_or_default().len() + 1;
                lists.push(ListMarker {
                    marker: marker.clone(),
                    indent,
                    content_indent: indent + width,
                    first: frame.first_block == self.blocks.len(),
                });
                indent += width;
            }
        }
        lists
    }

    fn endpoint(&self, inside: bool) -> Option<&Block> {
        let point = self.pointer?;
        let distance = |bounds: Rectangle| {
            let dx = (bounds.x - point.x)
                .max(0.0)
                .max(point.x - bounds.x - bounds.width);
            let dy = (bounds.y - point.y)
                .max(0.0)
                .max(point.y - bounds.y - bounds.height);
            dx * dx + dy * dy
        };
        self.blocks
            .iter()
            .filter(|block| block.visible.is_some())
            .filter(|block| !inside || block.visible.is_some_and(|visible| visible.contains(point)))
            .min_by(|a, b| distance(a.visible.unwrap()).total_cmp(&distance(b.visible.unwrap())))
    }

    fn ranges(&self, anchor: Endpoint, end: Endpoint) -> Option<Ranges> {
        if let Some(catalog) = &self.catalog {
            catalog.bounds(anchor, end)?;
            return Some(Ranges {
                lo: (0, 0),
                hi: (self.blocks.len().saturating_sub(1), 0),
                blocks: self
                    .blocks
                    .iter()
                    .map(|block| catalog.range(anchor, end, block.identity))
                    .collect(),
                logical: Some((anchor, end)),
            });
        }
        let locate = |point: Endpoint| {
            self.blocks
                .iter()
                .position(|block| {
                    block.identity == point.identity && block.plain.is_char_boundary(point.byte)
                })
                .map(|index| (index, point.byte))
        };
        let anchor = locate(anchor)?;
        let end = locate(end)?;
        let (lo, hi) = if anchor <= end {
            (anchor, end)
        } else {
            (end, anchor)
        };
        Some(Ranges {
            lo,
            hi,
            logical: None,
            blocks: self
                .blocks
                .iter()
                .enumerate()
                .map(|(index, block)| {
                    if index < lo.0 || index > hi.0 {
                        0..0
                    } else {
                        let start = if index == lo.0 { lo.1 } else { 0 };
                        let end = if index == hi.0 {
                            hi.1
                        } else {
                            block.plain.len()
                        };
                        start..end
                    }
                })
                .collect(),
        })
    }

    // One pass over selected prose, independent of the number of tables.
    fn table_values(&self, ranges: &Ranges) -> HashMap<u64, HashMap<(usize, usize), String>> {
        let mut tables: HashMap<u64, HashMap<(usize, usize), String>> = HashMap::new();
        let mut previous_lists = HashSet::new();
        for (block, range) in self.blocks.iter().zip(ranges.iter()) {
            if range.is_empty() {
                continue;
            }
            if let Some(cell) = block.cell {
                let value = tables
                    .entry(cell.table)
                    .or_default()
                    .entry((cell.row, cell.column))
                    .or_default();
                let parents = &self.tables[&cell.table].lists;
                let lists = &block.lists[parents.len()..];
                let first = range.start == 0 && lists.iter().any(|list| list.first);
                if !value.is_empty() {
                    value.push_str(if first && previous_lists.contains(&cell) {
                        "\n"
                    } else {
                        "\n\n"
                    });
                }
                let indent = parents.last().map_or(0, |list| list.content_indent);
                value.push_str(&Self::list_text(
                    &block.plain[range.clone()],
                    lists,
                    range.start == 0,
                    indent,
                ));
                if lists.is_empty() {
                    previous_lists.remove(&cell);
                } else {
                    previous_lists.insert(cell);
                }
            }
        }
        tables
    }

    fn list_text(text: &str, lists: &[ListMarker], beginning: bool, base_indent: usize) -> String {
        if !beginning || lists.is_empty() {
            return text.to_owned();
        }
        let mut result = String::new();
        for list in lists.iter().filter(|list| list.first) {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&" ".repeat(list.indent.saturating_sub(base_indent)));
            result.push_str(&list.marker);
            result.push(' ');
        }
        let indent = lists
            .last()
            .unwrap()
            .content_indent
            .saturating_sub(base_indent);
        if result.is_empty() {
            result.push_str(&" ".repeat(indent));
        }
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                result.push('\n');
                result.push_str(&" ".repeat(indent));
            }
            result.push_str(line);
        }
        result
    }

    fn copy(&self, ranges: &Ranges) -> String {
        if let Some(catalog) = &self.catalog {
            return ranges
                .logical
                .and_then(|(anchor, end)| catalog.copy(anchor, end))
                .unwrap_or_default();
        }
        let values = self.table_values(ranges);
        let mut result = String::new();
        let mut emitted = false;
        let mut previous_list = false;
        let mut append = |text: String, lists: &[ListMarker], beginning: bool| {
            let first_item = beginning && lists.iter().any(|list| list.first);
            if emitted {
                result.push_str(if first_item && !lists.is_empty() && previous_list {
                    "\n"
                } else {
                    "\n\n"
                });
            }
            result.push_str(&Self::list_text(&text, lists, beginning, 0));
            emitted = true;
            previous_list = !lists.is_empty();
        };
        let mut tables = self.table_order.iter().peekable();
        for (index, (block, range)) in self.blocks.iter().zip(ranges.iter()).enumerate() {
            while let Some(&&identity) = tables.peek() {
                let table = &self.tables[&identity];
                if table.first_block > index {
                    break;
                }
                tables.next();
                let fully_empty = !table.has_text
                    && ranges.lo.0 < table.first_block
                    && table.last_block <= ranges.hi.0;
                if let Some(values) = values.get(&identity) {
                    let beginning = ranges.lo.0 < table.first_block
                        || (ranges.lo.0 == table.first_block && ranges.lo.1 == 0);
                    append(
                        self.copy_table(identity, ranges, values),
                        &table.lists,
                        beginning,
                    );
                } else if fully_empty {
                    append(
                        self.copy_table(identity, ranges, &HashMap::new()),
                        &table.lists,
                        true,
                    );
                }
            }
            if !range.is_empty() && block.cell.is_none() {
                append(
                    block.plain[range.clone()].to_owned(),
                    &block.lists,
                    range.start == 0,
                );
            }
        }
        result
    }

    fn copy_table(
        &self,
        identity: u64,
        ranges: &Ranges,
        values: &HashMap<(usize, usize), String>,
    ) -> String {
        let Some(table) = self.tables.get(&identity) else {
            return String::new();
        };
        if table.columns == 0 || table.rows == 0 {
            return String::new();
        }
        let coordinate = |cell: Cell| cell.row * table.columns + cell.column;
        let lo_cell = self.blocks[ranges.lo.0]
            .cell
            .filter(|cell| cell.table == identity);
        let hi_cell = self.blocks[ranges.hi.0]
            .cell
            .filter(|cell| cell.table == identity);
        let first = lo_cell.map_or(0, coordinate);
        let last = if let Some(cell) = hi_cell {
            let end = coordinate(cell);
            if ranges.hi.1 == 0
                && table.cells.get(&(cell.row, cell.column)).copied() == Some(ranges.hi.0)
            {
                let Some(previous) = end.checked_sub(1) else {
                    return String::new();
                };
                previous
            } else {
                end
            }
        } else {
            table.rows * table.columns - 1
        };
        if first > last {
            return String::new();
        }
        if first == last {
            return values
                .get(&(first / table.columns, first % table.columns))
                .cloned()
                .unwrap_or_default();
        }
        let mut result = String::new();
        for coordinate in first..=last {
            let row = coordinate / table.columns;
            let column = coordinate % table.columns;
            if coordinate != first {
                result.push(if column == 0 { '\n' } else { '\t' });
            }
            let value = if table.cells.contains_key(&(row, column)) {
                values
                    .get(&(row, column))
                    .map(String::as_str)
                    .unwrap_or_default()
            } else {
                ""
            };
            if value.contains(['\t', '\n', '\r', '"']) {
                result.push('"');
                result.push_str(&value.replace('"', "\"\""));
                result.push('"');
            } else {
                result.push_str(value);
            }
        }
        result
    }
}

impl Operation for Read {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        let previous = self.translation;
        let clip = self.clip;
        if let Some((translation, bounds)) = self.pending.take() {
            let bounds = Rectangle {
                x: bounds.x - self.translation.x,
                y: bounds.y - self.translation.y,
                ..bounds
            };
            self.clip = Some(self.clip.map_or(bounds, |clip| {
                clip.intersection(&bounds).unwrap_or_default()
            }));
            self.translation += translation;
        }
        operate(self);
        self.translation = previous;
        self.clip = clip;
    }

    fn scrollable(
        &mut self,
        _: Option<&Id>,
        bounds: Rectangle,
        _: Rectangle,
        translation: Vector,
        _: &mut dyn operation::Scrollable,
    ) {
        self.pending = Some((translation, bounds));
    }

    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn Any) {
        if let Some(policy) = state.downcast_mut::<virtualized::PlaceholderPolicy>() {
            policy.visit = false;
            return;
        }
        if let Some(enter) = state.downcast_ref::<boundary::Enter>() {
            if let Kind::Table { columns, rows } = &enter.kind {
                self.table_order.push(enter.identity);
                let lists = self.list_markers();
                self.tables.insert(
                    enter.identity,
                    Table {
                        columns: *columns,
                        rows: *rows,
                        cells: HashMap::new(),
                        first_block: self.blocks.len(),
                        last_block: self.blocks.len(),
                        has_text: false,
                        lists,
                    },
                );
            }
            if let Kind::Cell { row, column } = &enter.kind {
                if let Some(table) = self
                    .frames
                    .iter()
                    .rev()
                    .find_map(|frame| {
                        matches!(frame.kind, Kind::Table { .. }).then_some(frame.identity)
                    })
                    .and_then(|identity| self.tables.get_mut(&identity))
                {
                    table.cells.insert((*row, *column), self.blocks.len());
                }
            }
            self.frames.push(Frame {
                identity: enter.identity,
                kind: enter.kind.clone(),
                first_block: self.blocks.len(),
            });
            return;
        }
        if state.is::<boundary::Exit>() {
            if let Some(frame) = self.frames.pop() {
                if let Some(table) = self.tables.get_mut(&frame.identity) {
                    table.last_block = self.blocks.len();
                }
            }
            return;
        }
        let bounds = Rectangle {
            x: bounds.x - self.translation.x,
            y: bounds.y - self.translation.y,
            ..bounds
        };
        let (identity, plain, hit, link, mounted) =
            if let Some(prose) = state.downcast_mut::<Prose>() {
                prose.managed = true;
                prose.logical_active = self.catalog.is_some() && prose.logical.is_some();
                let logical = prose.logical.as_ref().filter(|_| prose.logical_active);
                let position = self
                    .clip
                    .map_or(self.pointer, |clip| clipped_pointer(self.pointer, clip))
                    .map(|point| point - Vector::new(bounds.x, bounds.y));
                (
                    logical.map_or(prose.identity, |logical| logical.identity),
                    logical
                        .and_then(|logical| self.catalog.as_ref()?.text(logical.identity))
                        .unwrap_or_else(|| prose.plain.clone()),
                    position.map_or(0, |point| {
                        prose.hit(point) + logical.map_or(0, |logical| logical.start)
                    }),
                    position
                        .filter(|_| self.pointer.is_some_and(|point| bounds.contains(point)))
                        .and_then(|point| prose.target.zip(prose.link(point))),
                    true,
                )
            } else if let Some(logical) = state.downcast_ref::<LogicalText>() {
                (logical.identity, logical.plain.clone(), 0, None, false)
            } else {
                return;
            };
        let lists = self.list_markers();
        let mut table = None;
        let mut cell = None;
        for frame in &self.frames {
            match &frame.kind {
                Kind::ListItem(_) => {}
                Kind::Table { .. } => {
                    table = Some(frame.identity);
                    cell = None;
                }
                Kind::Cell { row, column } => {
                    cell = table.map(|table| Cell {
                        table,
                        row: *row,
                        column: *column,
                    })
                }
            }
        }
        if !plain.is_empty() {
            for frame in &self.frames {
                if let Some(table) = self.tables.get_mut(&frame.identity) {
                    table.has_text = true;
                }
            }
        }
        self.blocks.push(Block {
            identity,
            lists,
            cell,
            plain,
            bounds,
            visible: mounted
                .then(|| {
                    self.clip
                        .map_or(Some(bounds), |clip| bounds.intersection(&clip))
                })
                .flatten(),
            hit,
            link,
        });
    }
}

struct Apply<'a> {
    ranges: &'a [Range<usize>],
    index: usize,
    changed: usize,
    exchanges: Vec<usize>,
}

struct DragScroll {
    delta: f32,
    visited: bool,
    changed: bool,
    bottom_id: Option<Id>,
}
impl Operation for DragScroll {
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn Any) {
        if let Some(policy) = state.downcast_mut::<virtualized::PlaceholderPolicy>() {
            policy.visit = false;
        }
    }
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        if !self.visited {
            visit(self);
        }
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        if self.visited {
            return;
        }
        self.visited = true;
        let top =
            (translation.y + self.delta).clamp(0.0, (content.height - bounds.height).max(0.0));
        self.changed = (top - translation.y).abs() > f32::EPSILON;
        if self.changed {
            let delta = top - translation.y;
            state.scroll_by(
                operation::scrollable::AbsoluteOffset {
                    x: 0.0,
                    y: if id.is_some() && id == self.bottom_id.as_ref() {
                        -delta
                    } else {
                        delta
                    },
                },
                bounds,
                content,
            );
        }
    }
}

fn edge_delta(pointer: Point, viewport: Rectangle) -> f32 {
    if pointer.y < viewport.y {
        -((viewport.y - pointer.y) * 0.3).clamp(2.0, 30.0)
    } else if pointer.y > viewport.y + viewport.height {
        ((pointer.y - viewport.y - viewport.height) * 0.3).clamp(2.0, 30.0)
    } else {
        0.0
    }
}

fn clipped_pointer(pointer: Option<Point>, viewport: Rectangle) -> Option<Point> {
    pointer.map(|point| {
        Point::new(
            point.x.clamp(viewport.x, viewport.x + viewport.width),
            point
                .y
                .clamp(viewport.y, viewport.y + (viewport.height - 0.01).max(0.0)),
        )
    })
}
impl Operation for Apply<'_> {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn Any) {
        if let Some(policy) = state.downcast_mut::<virtualized::PlaceholderPolicy>() {
            policy.visit = false;
            return;
        }
        if let Some(invalidation) = state.downcast_mut::<retained::LayoutInvalidation>() {
            match invalidation {
                retained::LayoutInvalidation::Enter => self.exchanges.push(self.changed),
                retained::LayoutInvalidation::Exit { changed } => {
                    *changed = self
                        .exchanges
                        .pop()
                        .is_some_and(|before| before != self.changed);
                }
            }
            return;
        }
        let Some(prose) = state.downcast_mut::<Prose>() else {
            return;
        };
        let mut range = self.ranges.get(self.index).cloned().unwrap_or(0..0);
        if let Some(logical) = prose.logical.as_ref().filter(|_| prose.logical_active) {
            range = range
                .start
                .saturating_sub(logical.start)
                .min(prose.plain.len())
                ..range
                    .end
                    .saturating_sub(logical.start)
                    .min(prose.plain.len());
        }
        self.index += 1;
        if (prose.anchor, prose.end) != (range.start, range.end) {
            prose.anchor = range.start;
            prose.end = range.end;
            // Propagated through each exchange's update shell, which also
            // invalidates its retained layout. Only changed exchanges reflow.
            prose.dirty = true;
            self.changed += 1;
        }
    }
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Selection<'_> {
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }
    fn state(&self) -> tree::State {
        let bookmark = self
            .bookmark
            .as_ref()
            .map(|bookmark| *bookmark.borrow())
            .unwrap_or_default();
        tree::State::new(State {
            anchor: bookmark.anchor,
            end: bookmark.end,
            context: self
                .context
                .map(|(target, subagent)| (target, subagent.map(str::to_owned))),
            ..State::default()
        })
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if state
            .context
            .as_ref()
            .map(|(target, subagent)| (*target, subagent.as_deref()))
            != self.context
        {
            *state = State {
                context: self
                    .context
                    .map(|(target, subagent)| (target, subagent.map(str::to_owned))),
                reset: true,
                anchor: self
                    .bookmark
                    .as_ref()
                    .and_then(|bookmark| bookmark.borrow().anchor),
                end: self
                    .bookmark
                    .as_ref()
                    .and_then(|bookmark| bookmark.borrow().end),
                ..State::default()
            };
        }
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let node = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let state = tree.state.downcast_mut::<State>();
        if let (Some(catalog), Some(bookmark)) = (&self.catalog, &self.bookmark) {
            let saved = *bookmark.borrow();
            let changed = saved
                .anchor
                .zip(saved.anchor_prefix)
                .is_some_and(|(point, hash)| {
                    catalog.fingerprint(point).is_some_and(|now| now != hash)
                })
                || saved
                    .end
                    .zip(saved.end_prefix)
                    .is_some_and(|(point, hash)| {
                        catalog.fingerprint(point).is_some_and(|now| now != hash)
                    });
            if changed {
                state.anchor = None;
                state.end = None;
                *bookmark.borrow_mut() = Bookmark::default();
            }
        }
        if state.anchor.is_some() || std::mem::take(&mut state.reset) {
            let mut read = Read::with_catalog(None, self.catalog.clone());
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Layout::new(&node),
                renderer,
                &mut read,
            );
            let ranges = state
                .anchor
                .zip(state.end)
                .and_then(|(anchor, end)| read.ranges(anchor, end));
            if ranges.is_none()
                && !self
                    .catalog
                    .as_ref()
                    .is_some_and(|catalog| catalog.is_empty())
            {
                state.anchor = None;
                state.end = None;
                state.dragging = false;
                state.link = None;
            }
            let mut apply = Apply {
                ranges: ranges.as_deref().unwrap_or(&[]),
                index: 0,
                changed: 0,
                exchanges: Vec::new(),
            };
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Layout::new(&node),
                renderer,
                &mut apply,
            );
            if apply.changed > 0 {
                return self.content.as_widget_mut().layout(
                    &mut tree.children[0],
                    renderer,
                    limits,
                );
            }
        }
        node
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.focusable(None, layout.bounds(), tree.state.downcast_mut::<State>());
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        if let Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) = event {
            state.modifiers = *modifiers;
        }
        if let Some(pointer) = cursor.position() {
            state.pointer = Some(pointer);
        }
        if matches!(event, Event::Window(iced::window::Event::Unfocused)) {
            // A release outside the application may never reach this tree.
            // Preserve the selected text, but do not resume the old gesture
            // when the pointer returns to the window.
            state.dragging = false;
            state.link = None;
            state.next_scroll = None;
        }
        let press = matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
        );
        let motion = matches!(event, Event::Mouse(mouse::Event::CursorMoved { .. }));
        let release = matches!(
            event,
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
        );
        let command = match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(key),
                modifiers,
                ..
            }) if state.focused
                && ACTIVE.load(Ordering::Relaxed) == state.identity
                && modifiers.command()
                && !shell.is_event_captured()
                && (key.eq_ignore_ascii_case("c") || key.eq_ignore_ascii_case("a")) =>
            {
                Some(key.as_str())
            }
            _ => None,
        };
        let mut captured = false;
        let visible = layout.bounds().intersection(viewport).unwrap_or_default();
        if state
            .pointer
            .is_some_and(|point| edge_delta(point, visible) == 0.0)
        {
            state.next_scroll = None;
        }
        let mut autoscrolled = false;
        if let Event::Window(iced::window::Event::RedrawRequested(now)) = event
            && state.dragging
            && state.next_scroll.is_none_or(|next| *now >= next)
        {
            if let Some(pointer) = state.pointer {
                let elapsed = state.next_scroll.map_or(0.016, |next| {
                    now.saturating_duration_since(next - std::time::Duration::from_millis(16))
                        .as_secs_f32()
                        .min(0.05)
                });
                let delta = edge_delta(pointer, visible) * (elapsed / 0.016);
                if delta != 0.0 {
                    let mut scroll = DragScroll {
                        delta,
                        visited: false,
                        changed: false,
                        bottom_id: self
                            .context
                            .map(|(target, _)| card_control_id(target.card, "chat-transcript")),
                    };
                    self.content.as_widget_mut().operate(
                        &mut tree.children[0],
                        layout,
                        renderer,
                        &mut scroll,
                    );
                    autoscrolled = scroll.changed;
                    state.next_scroll = Some(*now + std::time::Duration::from_millis(16));
                }
            }
        }
        if press || (state.dragging && (motion || release)) || command.is_some() {
            let mut read = Read::with_catalog(
                if state.dragging && !press {
                    clipped_pointer(cursor.position(), visible)
                } else {
                    cursor.position()
                },
                self.catalog.clone(),
            );
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut read,
            );
            if press {
                state.focused = cursor.is_over(layout.bounds())
                    && cursor.is_over(*viewport)
                    && read.endpoint(true).is_some()
                    && !shell.is_event_captured();
                state.dragging = state.focused;
                if state.focused {
                    let block = read.endpoint(true).unwrap();
                    ACTIVE.store(state.identity, Ordering::Relaxed);
                    let point = cursor.position().unwrap();
                    state.origin = Some(point);
                    state.moved = false;
                    state.next_scroll = None;
                    let click = mouse::Click::new_at(
                        point,
                        mouse::Button::Left,
                        state.click,
                        (self.click_clock)(),
                    );
                    state.click = Some(click);
                    if state.modifiers.shift() && state.anchor.is_some() {
                        state.unit = Unit::Character;
                        state.initial = None;
                        state.end = Some(Endpoint {
                            identity: block.identity,
                            byte: block.hit,
                        });
                        state.link = None;
                    } else {
                        state.unit = match click.kind() {
                            mouse::click::Kind::Single => Unit::Character,
                            mouse::click::Kind::Double => Unit::Word,
                            mouse::click::Kind::Triple => Unit::Paragraph,
                        };
                        let range = state.unit.range(&block.plain, block.hit);
                        let start = Endpoint {
                            identity: block.identity,
                            byte: range.start,
                        };
                        let end = Endpoint {
                            identity: block.identity,
                            byte: range.end,
                        };
                        state.anchor = Some(start);
                        state.end = Some(end);
                        state.initial = Some((start, end));
                        state.link = matches!(state.unit, Unit::Character)
                            .then(|| block.link.clone())
                            .flatten();
                    }
                } else {
                    state.link = None;
                    state.anchor = None;
                    state.end = None;
                }
                captured = state.focused;
            } else if state.dragging {
                state.moved |= state
                    .origin
                    .zip(cursor.position())
                    .is_some_and(|(origin, pointer)| origin.distance(pointer) > 3.0);
                if let Some(block) = read.endpoint(false) {
                    state.extend(&read, block);
                }
                if release {
                    state.dragging = false;
                    state.next_scroll = None;
                    if !state.moved
                        && state.anchor == state.end
                        && let Some((target, link)) = state.link.take()
                        && read.endpoint(true).is_some_and(|block| {
                            block.link.as_ref() == Some(&(target, link.clone()))
                        })
                    {
                        shell.publish(action(Action::OpenDocument(target, link)));
                    }
                }
                captured = true;
            } else if command.is_some_and(|key| key.eq_ignore_ascii_case("a")) {
                if let Some(catalog) = &self.catalog {
                    let endpoints = catalog.endpoints();
                    state.anchor = endpoints.map(|(anchor, _)| anchor);
                    state.end = endpoints.map(|(_, end)| end);
                } else {
                    state.anchor = read.blocks.first().map(|block| Endpoint {
                        identity: block.identity,
                        byte: 0,
                    });
                    state.end = read.blocks.last().map(|block| Endpoint {
                        identity: block.identity,
                        byte: block.plain.len(),
                    });
                }
                captured = state.anchor.is_some();
            }
            let ranges = state
                .anchor
                .zip(state.end)
                .and_then(|(anchor, end)| read.ranges(anchor, end));
            if ranges.is_none()
                && !self
                    .catalog
                    .as_ref()
                    .is_some_and(|catalog| catalog.is_empty())
            {
                state.anchor = None;
                state.end = None;
                state.dragging = false;
                state.link = None;
            }
            if command.is_some_and(|key| key.eq_ignore_ascii_case("c")) {
                if let Some(ranges) = &ranges {
                    let text = read.copy(ranges);
                    if !text.is_empty() {
                        clipboard.write(iced::advanced::clipboard::Kind::Standard, text);
                        captured = true;
                    }
                }
            }
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut Apply {
                    ranges: ranges.as_deref().unwrap_or(&[]),
                    index: 0,
                    changed: 0,
                    exchanges: Vec::new(),
                },
            );
        }
        // Let changed prose invalidate its retained exchange through its own
        // shell before consuming the event at the transcript boundary.
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if (autoscrolled || matches!(event, Event::Mouse(mouse::Event::WheelScrolled { .. })))
            && tree.state.downcast_ref::<State>().dragging
        {
            // Scrollables update their native offsets above. Hit-test again
            // in that coordinate space even when the pointer stayed still.
            let mut read = Read::with_catalog(
                clipped_pointer(tree.state.downcast_ref::<State>().pointer, visible),
                self.catalog.clone(),
            );
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut read,
            );
            let state = tree.state.downcast_mut::<State>();
            if let Some(block) = read.endpoint(false) {
                state.extend(&read, block);
                state.moved = true;
            }
            let ranges = state
                .anchor
                .zip(state.end)
                .and_then(|(anchor, end)| read.ranges(anchor, end));
            let mut apply = Apply {
                ranges: ranges.as_deref().unwrap_or(&[]),
                index: 0,
                changed: 0,
                exchanges: Vec::new(),
            };
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut apply,
            );
            if apply.changed > 0 {
                shell.invalidate_layout();
            }
        }
        if let Some(bookmark) = &self.bookmark {
            let state = tree.state.downcast_ref::<State>();
            *bookmark.borrow_mut() = Bookmark {
                anchor: state.anchor,
                end: state.end,
                anchor_prefix: state
                    .anchor
                    .and_then(|point| self.catalog.as_ref()?.fingerprint(point)),
                end_prefix: state
                    .end
                    .and_then(|point| self.catalog.as_ref()?.fingerprint(point)),
            };
        }
        if captured {
            shell.capture_event();
        }
        let state = tree.state.downcast_ref::<State>();
        if state.dragging
            && state
                .pointer
                .is_some_and(|point| edge_delta(point, visible) != 0.0)
        {
            shell.request_redraw_at(state.next_scroll.unwrap_or_else(|| {
                iced::time::Instant::now() + std::time::Duration::from_millis(16)
            }));
        }
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, cosmic::Theme, cosmic::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rendered_markdown(
        body: &str,
        change: impl FnOnce(&mut Vec<markdown::Item>),
    ) -> Element<'static, Message> {
        let cached = CachedItem::new(
            &ItemContent::Text {
                text: body.to_owned(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            crate::settings::EditorSyntaxTheme::Auto,
        );
        let mut items = cached.markdown.items().to_vec();
        change(&mut items);
        prose::items_for_test(
            &cached,
            &items,
            Target {
                card: CardId::new(),
                generation: 3,
            },
        )
    }
    fn read_rendered(element: &mut Element<'static, Message>, tree: &mut Tree) -> Read {
        let renderer = crate::app::tests::headless_renderer();
        let node = element.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(600.0, 2000.0)),
        );
        let mut read = Read::new(None);
        element
            .as_widget_mut()
            .operate(tree, Layout::new(&node), &renderer, &mut read);
        assert!(
            read.frames.is_empty(),
            "structure scope must close after traversal"
        );
        read
    }
    fn copy_between(read: &Read, first: usize, start: usize, last: usize, end: usize) -> String {
        let a = Endpoint {
            identity: read.blocks[first].identity,
            byte: start,
        };
        let b = Endpoint {
            identity: read.blocks[last].identity,
            byte: end,
        };
        let forward = read.copy(&read.ranges(a, b).unwrap());
        assert_eq!(read.copy(&read.ranges(b, a).unwrap()), forward);
        forward
    }
    #[test]
    fn rendered_lists_preserve_markers_tasks_nesting_and_partial_unicode() {
        let body = "7. α first\n8. β second\n\n- [x] done\n  - nested\n- [ ] pending\n";
        let mut element = rendered_markdown(body, |_| {});
        let mut tree = Tree::new(element.as_widget());
        let read = read_rendered(&mut element, &mut tree);
        assert_eq!(
            read.blocks
                .iter()
                .map(|block| block.plain.as_ref())
                .collect::<Vec<_>>(),
            ["α first", "β second", "done", "nested", "pending"]
        );
        assert_eq!(
            copy_between(&read, 0, 0, 4, "pending".len()),
            "7. α first\n8. β second\n- [x] done\n  - nested\n- [ ] pending"
        );
        assert_eq!(
            copy_between(&read, 0, "α ".len(), 1, "β".len()),
            "first\n8. β"
        );
        assert_eq!(copy_between(&read, 3, 1, 3, 4), "est");
        let identities = read
            .blocks
            .iter()
            .map(|block| block.identity)
            .collect::<Vec<_>>();
        element = rendered_markdown(body.replace("7.", "12.").as_str(), |_| {});
        tree.diff(&mut element);
        let changed = read_rendered(&mut element, &mut tree);
        assert_eq!(
            changed
                .blocks
                .iter()
                .map(|block| block.identity)
                .collect::<Vec<_>>(),
            identities
        );
        assert_eq!(
            copy_between(&changed, 0, 0, 1, "β second".len()),
            "12. α first\n13. β second"
        );
    }
    #[test]
    fn rendered_tables_copy_linear_intervals_empty_cells_and_delimiters() {
        let body = "| A | B | C |\n|---|---|---|\n| α | | β |\n| γ | delta | ε |\n";
        let mut element = rendered_markdown(body, |_| {});
        let mut tree = Tree::new(element.as_widget());
        let read = read_rendered(&mut element, &mut tree);
        assert_eq!(
            read.tables.values().next().unwrap().cells.len(),
            9,
            "empty cells still publish coordinates"
        );
        assert_eq!(
            copy_between(&read, 0, 0, read.blocks.len() - 1, "ε".len()),
            "A\tB\tC\nα\t\tβ\nγ\tdelta\tε"
        );
        assert_eq!(
            copy_between(&read, 1, 0, 3, "α".len()),
            "B\tC\nα",
            "a linear drag must not pad unselected outer cells"
        );
        assert_eq!(
            copy_between(&read, 3, 0, 4, 0),
            "α\t",
            "empty selected cell before a zero-byte endpoint is retained"
        );
        assert_eq!(copy_between(&read, 3, 0, 3, "α".len()), "α");
        let content = "x\ty\n\"z\"";
        let mut element = rendered_markdown(body, |items| {
            let markdown::Item::Table { rows, .. } = &mut items[0] else {
                panic!("fixture table")
            };
            rows[0].cells[1] = vec![markdown::Item::CodeBlock {
                language: None,
                code: content.into(),
                source: Arc::from(content),
                reader_source: super::reader_source::Source::new(content),
                lines: Vec::new(),
            }];
        });
        let mut tree = Tree::new(element.as_widget());
        let read = read_rendered(&mut element, &mut tree);
        assert_eq!(
            copy_between(&read, 4, 0, 4, content.len()),
            content,
            "single-cell text is raw"
        );
        assert_eq!(
            copy_between(&read, 3, 0, 5, "β".len()),
            "α\t\"x\ty\n\"\"z\"\"\"\tβ"
        );
        let mut element = rendered_markdown(body, |items| {
            let markdown::Item::Table { rows, .. } = &mut items[0] else {
                panic!("fixture table")
            };
            rows[0].cells[1] = [content, "tail"]
                .into_iter()
                .map(|code| markdown::Item::CodeBlock {
                    language: None,
                    code: code.into(),
                    source: Arc::from(code),
                    reader_source: super::reader_source::Source::new(code),
                    lines: Vec::new(),
                })
                .collect();
        });
        let mut tree = Tree::new(element.as_widget());
        let read = read_rendered(&mut element, &mut tree);
        assert_eq!(
            copy_between(&read, 4, 0, 5, 0),
            content,
            "zero-byte endpoint in a later block must not remove the selected earlier block in its cell"
        );
    }

    #[test]
    fn rendered_empty_tables_and_many_tables_keep_structure() {
        let body = "before\n\n| | | |\n|---|---|---|\n| | | |\n\nafter";
        let mut element = rendered_markdown(body, |_| {});
        let mut tree = Tree::new(element.as_widget());
        let read = read_rendered(&mut element, &mut tree);
        assert_eq!(read.tables.len(), 1);
        assert_eq!(
            read.blocks
                .iter()
                .filter(|block| !block.plain.is_empty())
                .count(),
            2
        );
        let last = read.blocks.len() - 1;
        assert_eq!(
            copy_between(&read, 0, 0, last, 5),
            "before\n\n\t\t\n\t\t\n\nafter"
        );
        assert_eq!(copy_between(&read, 0, 0, last, 0), "before\n\n\t\t\n\t\t");

        let body = (0..128)
            .map(|index| format!("| H{index} | B |\n|---|---|\n| V{index} | W |\n\n"))
            .collect::<String>();
        let mut element = rendered_markdown(&body, |_| {});
        let mut tree = Tree::new(element.as_widget());
        let read = read_rendered(&mut element, &mut tree);
        let last = read.blocks.len() - 1;
        let ranges = read
            .ranges(
                Endpoint {
                    identity: read.blocks[0].identity,
                    byte: 0,
                },
                Endpoint {
                    identity: read.blocks[last].identity,
                    byte: 1,
                },
            )
            .unwrap();
        // The clipboard formatter receives one index shared by all table chunks.
        let values = read.table_values(&ranges);
        assert_eq!(values.len(), 128);
        assert_eq!(values.values().map(HashMap::len).sum::<usize>(), 512);
        assert_eq!(
            read.copy(&ranges),
            (0..128)
                .map(|index| format!("H{index}\tB\nV{index}\tW"))
                .collect::<Vec<_>>()
                .join("\n\n")
        );
    }

    #[test]
    fn rendered_ordered_list_continuations_nested_items_and_table_children() {
        let body = "10. first\n\n    continuation\n\n    - nested\n\n11. next";
        let mut element = rendered_markdown(body, |_| {});
        let mut tree = Tree::new(element.as_widget());
        let read = read_rendered(&mut element, &mut tree);
        assert_eq!(
            read.blocks
                .iter()
                .map(|block| block.plain.as_ref())
                .collect::<Vec<_>>(),
            ["first", "continuation", "nested", "next"]
        );
        assert_eq!(
            copy_between(&read, 0, 0, 3, 4),
            "10. first\n\n    continuation\n    - nested\n11. next"
        );
        assert_eq!(copy_between(&read, 1, 2, 1, 7), "ntinu");
        assert_eq!(copy_between(&read, 1, 0, 1, 12), "    continuation");

        // Markdown's rendered item model supports a table as the first list child.
        let body = "10. placeholder\n\n| A | B |\n|---|---|\n| α | β |";
        let mut element = rendered_markdown(body, |items| {
            let table = items.pop().expect("table");
            assert!(matches!(table, markdown::Item::Table { .. }));
            let markdown::Item::List { bullets, .. } = &mut items[0] else {
                panic!("list")
            };
            let markdown::Bullet::Point { items } = &mut bullets[0] else {
                panic!("point")
            };
            *items = vec![table];
        });
        let mut tree = Tree::new(element.as_widget());
        let read = read_rendered(&mut element, &mut tree);
        assert_eq!(
            copy_between(&read, 0, 0, 3, "β".len()),
            "10. A\tB\n    α\tβ"
        );
        assert_eq!(copy_between(&read, 1, 0, 3, "β".len()), "B\nα\tβ");
        assert_eq!(copy_between(&read, 2, "α".len(), 3, "β".len()), "\tβ");
    }

    #[test]
    fn rendered_cell_lists_preserve_inner_structure_without_outer_prefix_duplication() {
        let body = "10. outside\n\n- α\n- β\n\n| A | B |\n|---|---|\n| left | right |";
        let mut element = rendered_markdown(body, |items| {
            let mut table = items.pop().expect("table");
            let list = items.pop().expect("inner list");
            assert!(matches!(list, markdown::Item::List { .. }));
            let markdown::Item::Table { rows, .. } = &mut table else {
                panic!("table")
            };
            rows[0].cells[0] = vec![list];
            let markdown::Item::List { bullets, .. } = &mut items[0] else {
                panic!("outer list")
            };
            let markdown::Bullet::Point { items } = &mut bullets[0] else {
                panic!("point")
            };
            *items = vec![table];
        });
        let mut tree = Tree::new(element.as_widget());
        let read = read_rendered(&mut element, &mut tree);
        assert_eq!(
            copy_between(&read, 0, 0, 4, 5),
            "10. A\tB\n    \"- α\n    - β\"\tright"
        );
        assert_eq!(copy_between(&read, 2, 0, 3, "β".len()), "- α\n- β");
        assert_eq!(copy_between(&read, 2, "α".len(), 3, "β".len()), "- β");
    }

    #[test]
    fn selection_preserves_accessible_child_state_and_offset() {
        let element = surface(Element::new(crate::ui_controls::accessibility_probe::Probe));
        let tree = Tree::new(&element);
        let node = layout::Node::new(Size::new(80.0, 40.0));
        let nodes = element.as_widget().a11y_nodes(
            Layout::new(&node).with_virtual_offset(Vector::new(0.0, 37.0)),
            &tree,
            mouse::Cursor::Unavailable,
        );
        assert_eq!(nodes.root().len(), 1);
        assert_eq!(nodes.root()[0].node().label(), Some("Retained child"));
    }
    fn surface<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
        Element::new(Selection {
            catalog: None,
            bookmark: None,
            content: content.into(),
            context: None,
            click_clock: iced::time::Instant::now,
        })
    }
    fn read() -> Read {
        let mut read = Read::new(None);
        read.blocks = ["α first", "middle\ncode", "β last"]
            .into_iter()
            .enumerate()
            .map(|(index, plain)| Block {
                identity: index as u64 + 1,
                lists: Vec::new(),
                cell: None,
                plain: Arc::from(plain),
                bounds: Rectangle::default(),
                visible: Some(Rectangle::default()),
                hit: 0,
                link: None,
            })
            .collect();
        read
    }
    #[test]
    fn word_and_paragraph_units_preserve_unicode_and_extend_in_both_directions() {
        let text = "cafe\u{301} naïve\nsecond line\n";
        assert_eq!(&text[Unit::Word.range(text, 2)], "cafe\u{301}");
        assert_eq!(&text[Unit::Paragraph.range(text, 2)], "cafe\u{301} naïve\n");
        assert_eq!(
            &text[Unit::Paragraph.range(text, text.find("second").unwrap())],
            "second line\n"
        );
        let mut read = read();
        let mut state = State {
            unit: Unit::Word,
            initial: Some((
                Endpoint {
                    identity: 2,
                    byte: 0,
                },
                Endpoint {
                    identity: 2,
                    byte: 6,
                },
            )),
            ..State::default()
        };
        read.blocks[2].hit = "β ".len();
        state.extend(&read, &read.blocks[2]);
        assert_eq!(
            read.copy(
                &read
                    .ranges(state.anchor.unwrap(), state.end.unwrap())
                    .unwrap()
            ),
            "middle\ncode\n\nβ last"
        );
        read.blocks[0].hit = "α ".len();
        state.extend(&read, &read.blocks[0]);
        assert_eq!(
            read.copy(
                &read
                    .ranges(state.anchor.unwrap(), state.end.unwrap())
                    .unwrap()
            ),
            "first\n\nmiddle"
        );
    }

    #[test]
    fn click_units_shift_extension_and_returned_link_drags_use_the_shared_selection() {
        thread_local! {
            static CLICK_TIME: std::cell::Cell<iced::time::Instant> =
                std::cell::Cell::new(iced::time::Instant::now());
        }
        fn next_click_time() -> iced::time::Instant {
            CLICK_TIME.with(|time| {
                let next = time.get() + std::time::Duration::from_millis(10);
                time.set(next);
                next
            })
        }
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let content = super::super::selectable_prose::text(
            vec![
                iced::widget::text::Span::new("one two three")
                    .link("https://example.com".to_owned()),
            ]
            .into(),
            16.0.into(),
            iced::Font::default(),
            target,
            None,
        );
        // Renderer/font work may exceed the real double-click deadline under
        // parallel tests. Keep the actual event path with explicit click times.
        let mut element = Element::new(Selection {
            catalog: None,
            bookmark: None,
            content: content.into(),
            context: None,
            click_clock: next_click_time,
        });
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(300.0, 100.0));
        let mut node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let viewport = Rectangle::with_size(Size::new(300.0, 100.0));
        let mut clipboard = ClipboardText::default();
        let mut messages = Vec::new();
        let mut send = |event: Event,
                        pointer: Point,
                        element: &mut Element<'_, Message>,
                        tree: &mut Tree,
                        node: &mut layout::Node| {
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                tree,
                &event,
                Layout::new(node),
                mouse::Cursor::Available(pointer),
                &renderer,
                &mut clipboard,
                &mut shell,
                &viewport,
            );
            if shell.is_layout_invalid() {
                *node = element.as_widget_mut().layout(tree, &renderer, &limits);
            }
        };
        let press = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
        let release = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
        let point = Point::new(4.0, 4.0);
        send(press.clone(), point, &mut element, &mut tree, &mut node);
        send(
            Event::Mouse(mouse::Event::CursorMoved {
                position: Point::new(90.0, 4.0),
            }),
            Point::new(90.0, 4.0),
            &mut element,
            &mut tree,
            &mut node,
        );
        send(
            Event::Mouse(mouse::Event::CursorMoved { position: point }),
            point,
            &mut element,
            &mut tree,
            &mut node,
        );
        send(release.clone(), point, &mut element, &mut tree, &mut node);
        assert!(tree.state.downcast_ref::<State>().moved);
        send(press.clone(), point, &mut element, &mut tree, &mut node);
        send(release.clone(), point, &mut element, &mut tree, &mut node);
        let mut read = Read::new(None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        let state = tree.state.downcast_ref::<State>();
        assert_eq!(
            read.copy(
                &read
                    .ranges(state.anchor.unwrap(), state.end.unwrap())
                    .unwrap()
            ),
            "one"
        );
        send(press.clone(), point, &mut element, &mut tree, &mut node);
        send(release.clone(), point, &mut element, &mut tree, &mut node);
        let state = tree.state.downcast_ref::<State>();
        assert_eq!(
            read.copy(
                &read
                    .ranges(state.anchor.unwrap(), state.end.unwrap())
                    .unwrap()
            ),
            "one two three"
        );
        send(
            Event::Keyboard(keyboard::Event::ModifiersChanged(
                keyboard::Modifiers::SHIFT,
            )),
            point,
            &mut element,
            &mut tree,
            &mut node,
        );
        send(
            press,
            Point::new(38.0, 4.0),
            &mut element,
            &mut tree,
            &mut node,
        );
        send(
            release,
            Point::new(38.0, 4.0),
            &mut element,
            &mut tree,
            &mut node,
        );
        let state = tree.state.downcast_ref::<State>();
        assert_eq!(
            state.anchor.unwrap().byte,
            0,
            "Shift-click keeps the original selection anchor"
        );
        assert!(state.end.unwrap().byte > 0 && state.end.unwrap().byte < "one two three".len());
        drop(send);
        assert!(
            messages.is_empty(),
            "a moved drag returning to the link never activates it"
        );
    }

    #[test]
    fn forward_and_reverse_drag_copy_partial_endpoints_and_entire_middle_blocks() {
        let read = read();
        let start = Endpoint {
            identity: 1,
            byte: "α ".len(),
        };
        let end = Endpoint {
            identity: 3,
            byte: "β".len(),
        };
        for (anchor, end) in [(start, end), (end, start)] {
            let ranges = read.ranges(anchor, end).unwrap();
            assert_eq!(read.copy(&ranges), "first\n\nmiddle\ncode\n\nβ");
        }
    }
    #[test]
    fn removed_or_shortened_endpoints_cancel_without_slicing_invalid_unicode() {
        let read = read();
        let end = Endpoint {
            identity: 3,
            byte: 2,
        };
        for anchor in [
            Endpoint {
                identity: 4,
                byte: 0,
            },
            Endpoint {
                identity: 1,
                byte: 99,
            },
            Endpoint {
                identity: 1,
                byte: 1,
            },
        ] {
            assert!(read.ranges(anchor, end).is_none());
        }
    }
    #[test]
    fn drag_in_whitespace_reaches_nearest_block_and_reverse_drag_can_leave_viewport() {
        let mut read = read();
        for (index, block) in read.blocks.iter_mut().enumerate() {
            block.bounds = Rectangle {
                x: 20.0,
                y: index as f32 * 60.0,
                width: 200.0,
                height: 30.0,
            };
            block.visible = Some(block.bounds);
        }
        read.pointer = Some(Point::new(30.0, 45.0));
        assert!(read.endpoint(true).is_none());
        assert_eq!(read.endpoint(false).unwrap().identity, 1);
        read.pointer = Some(Point::new(30.0, -100.0));
        assert_eq!(read.endpoint(false).unwrap().identity, 1);
        read.pointer = Some(Point::new(30.0, 300.0));
        assert_eq!(read.endpoint(false).unwrap().identity, 3);
    }

    #[test]
    fn changing_conversations_clears_selection_even_when_prose_is_identical() {
        let renderer = crate::app::tests::headless_renderer();
        let first = Target {
            card: CardId::new(),
            generation: 0,
        };
        let second = Target {
            card: CardId::new(),
            generation: 0,
        };
        let mut element = transcript(prose("identical response"), first, None);
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut read = Read::new(None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        let block = &read.blocks[0];
        let state = tree.state.downcast_mut::<State>();
        state.anchor = Some(Endpoint {
            identity: block.identity,
            byte: 0,
        });
        state.end = Some(Endpoint {
            identity: block.identity,
            byte: block.plain.len(),
        });
        state.focused = true;
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut replacement = transcript(prose("identical response"), second, None);
        tree.diff(&mut replacement);
        replacement
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let state = tree.state.downcast_ref::<State>();
        assert!(state.anchor.is_none());
        assert!(!state.focused);
        let prose = tree.children[0].state.downcast_ref::<Prose>();
        assert_eq!((prose.anchor, prose.end), (0, 0));
    }

    #[derive(Default)]
    struct ClipboardText(String);
    impl Clipboard for ClipboardText {
        fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
            Some(self.0.clone())
        }
        fn write(&mut self, _: iced::advanced::clipboard::Kind, contents: String) {
            self.0 = contents;
        }
    }
    fn command(key: &str) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(key.into()),
            modified_key: keyboard::Key::Character(key.into()),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL,
            text: None,
            repeat: false,
        })
    }
    fn prose(text: &str) -> Element<'static, Message> {
        super::super::selectable_prose::text(
            vec![iced::widget::text::Span::new(text.to_owned())].into(),
            16.0.into(),
            iced::Font::default(),
            Target {
                card: CardId::new(),
                generation: 0,
            },
            None,
        )
    }

    #[test]
    fn widget_drag_updates_retained_exchanges_and_copy_respects_focus_operations() {
        // Other gesture tests set the process-wide active selection. Keep the
        // drag and copy in one isolated process so they exercise real focus
        // arbitration without another test taking focus between these events.
        const CHILD: &str = "CAGENTS_TEST_TRANSCRIPT_FOCUS";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    concat!(
                        "app::chat_ui::selection::tests::",
                        "widget_drag_updates_retained_exchanges_and_copy_respects_focus_operations"
                    ),
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        let renderer = crate::app::tests::headless_renderer();
        let mut element = surface(
            widget::column::with_children(vec![
                retained::kept(1, || prose("α first")),
                retained::kept(2, || prose("middle\ncode")),
                retained::kept(3, || prose("β last")),
            ])
            .spacing(20),
        );
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
        let mut node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let viewport = Rectangle::with_size(Size::new(400.0, 400.0));
        let mut clipboard = ClipboardText::default();
        let mut read = Read::new(None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        let first = read.blocks[0].bounds;
        let last = read.blocks[2].bounds;
        let start = Point::new(first.x, first.y + 3.0);
        let end = Point::new(last.x + last.width + 10.0, last.y + last.height + 1.0);
        for (event, point) in [
            (
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                start,
            ),
            (
                Event::Mouse(mouse::Event::CursorMoved { position: end }),
                end,
            ),
            (
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                end,
            ),
            (command("c"), end),
        ] {
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(point),
                &renderer,
                &mut clipboard,
                &mut shell,
                &viewport,
            );
            assert!(shell.is_event_captured());
            if shell.is_layout_invalid() {
                node = element
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
            }
        }
        assert_eq!(clipboard.0, "α first\n\nmiddle\ncode\n\nβ last");
        // Reused retained layouts must have rebuilt their highlighted spans.
        fn highlights(tree: &Tree, count: &mut usize) {
            for child in &tree.children {
                if child.tag == tree::Tag::of::<Prose>() {
                    let prose = child.state.downcast_ref::<Prose>();
                    assert_eq!((prose.anchor, prose.end), (0, prose.plain.len()));
                    assert!(!prose.dirty);
                    *count += 1;
                }
                highlights(child, count);
            }
        }
        let mut count = 0;
        highlights(&tree, &mut count);
        assert_eq!(count, 3);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus(Id::new("chat-find")),
        );
        clipboard.0 = "find query".into();
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &command("c"),
            Layout::new(&node),
            mouse::Cursor::Available(end),
            &renderer,
            &mut clipboard,
            &mut shell,
            &viewport,
        );
        assert_eq!(clipboard.0, "find query");
        assert!(!shell.is_event_captured());

        // A selected middle block changes while the endpoints survive.
        element = surface(
            widget::column::with_children(vec![
                retained::kept(1, || prose("α first")),
                retained::kept(4, || prose("new middle block")),
                retained::kept(3, || prose("β last")),
            ])
            .spacing(20),
        );
        tree.diff(element.as_widget_mut());
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut count = 0;
        highlights(&tree, &mut count);
        assert_eq!(
            count, 3,
            "layout reconciles all selected blocks before another input event"
        );

        // Paging replaces an endpoint in the same positional widget tree.
        element = surface(
            widget::column::with_children(vec![
                retained::kept(5, || prose("different first")),
                retained::kept(4, || prose("new middle block")),
                retained::kept(3, || prose("β last")),
            ])
            .spacing(20),
        );
        tree.diff(element.as_widget_mut());
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(tree.state.downcast_ref::<State>().anchor.is_none());
        fn cleared(tree: &Tree) {
            for child in &tree.children {
                if child.tag == tree::Tag::of::<Prose>() {
                    let prose = child.state.downcast_ref::<Prose>();
                    assert_eq!((prose.anchor, prose.end), (0, 0));
                }
                cleared(child);
            }
        }
        cleared(&tree);
    }

    #[test]
    fn dragging_tracks_wheel_scroll_under_a_stationary_pointer_and_stops_on_window_blur() {
        let renderer = crate::app::tests::headless_renderer();
        let mut element = surface(
            widget::scrollable(
                widget::column::with_children(
                    (0..12)
                        .map(|index| prose(&format!("line {index} selection")))
                        .collect::<Vec<_>>(),
                )
                .spacing(10),
            )
            .height(100)
            .width(300),
        );
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(300.0, 100.0));
        let mut node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let viewport = Rectangle::with_size(Size::new(300.0, 100.0));
        let mut clipboard = ClipboardText::default();
        let pointer = Point::new(20.0, 50.0);
        let mut send = |event: Event,
                        point: Point,
                        element: &mut Element<'_, Message>,
                        tree: &mut Tree,
                        node: &mut layout::Node| {
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                tree,
                &event,
                Layout::new(node),
                mouse::Cursor::Available(point),
                &renderer,
                &mut clipboard,
                &mut shell,
                &viewport,
            );
            if shell.is_layout_invalid() {
                *node = element.as_widget_mut().layout(tree, &renderer, &limits);
            }
        };
        send(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Point::new(5.0, 4.0),
            &mut element,
            &mut tree,
            &mut node,
        );
        send(
            Event::Mouse(mouse::Event::CursorMoved { position: pointer }),
            pointer,
            &mut element,
            &mut tree,
            &mut node,
        );
        let before = tree.state.downcast_ref::<State>().end;
        send(
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: -2.0 },
            }),
            pointer,
            &mut element,
            &mut tree,
            &mut node,
        );
        let mut read = Read::new(Some(pointer));
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        let block = read.endpoint(false).unwrap();
        let after = tree.state.downcast_ref::<State>().end;
        assert_eq!(
            after,
            Some(Endpoint {
                identity: block.identity,
                byte: block.hit
            })
        );
        assert_ne!(
            after, before,
            "scrolling changes the text under the pointer without another motion event"
        );
        let outside = Point::new(20.0, 140.0);
        send(
            Event::Mouse(mouse::Event::CursorMoved { position: outside }),
            outside,
            &mut element,
            &mut tree,
            &mut node,
        );
        let mut before_scroll = Read::new(None);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut before_scroll,
        );
        send(
            Event::Window(iced::window::Event::RedrawRequested(
                iced::time::Instant::now(),
            )),
            outside,
            &mut element,
            &mut tree,
            &mut node,
        );
        let mut after_scroll = Read::new(Some(Point::new(20.0, 99.99)));
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut after_scroll,
        );
        assert!(
            after_scroll.blocks[0].bounds.y < before_scroll.blocks[0].bounds.y,
            "dragging beyond the viewport scrolls on redraw without pointer motion"
        );
        let endpoint = after_scroll.endpoint(false).unwrap();
        let after = Some(Endpoint {
            identity: endpoint.identity,
            byte: endpoint.hit,
        });
        assert_eq!(tree.state.downcast_ref::<State>().end, after);
        send(
            Event::Window(iced::window::Event::Unfocused),
            pointer,
            &mut element,
            &mut tree,
            &mut node,
        );
        assert!(!tree.state.downcast_ref::<State>().dragging);
        send(
            Event::Mouse(mouse::Event::CursorMoved {
                position: Point::new(10.0, 90.0),
            }),
            Point::new(10.0, 90.0),
            &mut element,
            &mut tree,
            &mut node,
        );
        assert_eq!(tree.state.downcast_ref::<State>().end, after);
    }

    #[test]
    fn bottom_anchored_transcript_scrolls_toward_the_drag_and_ignores_early_redraws() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let mut element = transcript(
            scroll::transcript(
                widget::column::with_children(
                    (0..30)
                        .map(|index| prose(&format!("line {index} text")))
                        .collect::<Vec<_>>(),
                )
                .spacing(10),
                target,
                None,
                Vec::new(),
                true,
                Vec::new(),
            ),
            target,
            None,
        );
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(300.0, 100.0));
        let mut node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let viewport = Rectangle::with_size(Size::new(300.0, 100.0));
        let mut clipboard = ClipboardText::default();
        let mut send = |event: Event,
                        point: Point,
                        element: &mut Element<'_, Message>,
                        tree: &mut Tree,
                        node: &mut layout::Node| {
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                tree,
                &event,
                Layout::new(node),
                mouse::Cursor::Available(point),
                &renderer,
                &mut clipboard,
                &mut shell,
                &viewport,
            );
            if shell.is_layout_invalid() {
                *node = element.as_widget_mut().layout(tree, &renderer, &limits);
            }
        };
        let top = |element: &mut Element<'_, Message>, tree: &mut Tree, node: &layout::Node| {
            let mut read = Read::new(None);
            element
                .as_widget_mut()
                .operate(tree, Layout::new(node), &renderer, &mut read);
            read.blocks[0].bounds.y
        };
        let mut initial_read = Read::new(None);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut initial_read,
        );
        let visible = initial_read
            .blocks
            .iter()
            .filter_map(|block| block.visible)
            .find(|bounds| bounds.height > 1.0)
            .unwrap();
        send(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Point::new(visible.x + 2.0, visible.y + visible.height / 2.0),
            &mut element,
            &mut tree,
            &mut node,
        );
        let outside = Point::new(5.0, -40.0);
        send(
            Event::Mouse(mouse::Event::CursorMoved { position: outside }),
            outside,
            &mut element,
            &mut tree,
            &mut node,
        );
        let before = top(&mut element, &mut tree, &node);
        let now = iced::time::Instant::now();
        send(
            Event::Window(iced::window::Event::RedrawRequested(now)),
            outside,
            &mut element,
            &mut tree,
            &mut node,
        );
        let first = top(&mut element, &mut tree, &node);
        assert!(
            first > before,
            "upward dragging scrolls toward earlier text with Anchor::End"
        );
        send(
            Event::Window(iced::window::Event::RedrawRequested(
                now + std::time::Duration::from_millis(8),
            )),
            outside,
            &mut element,
            &mut tree,
            &mut node,
        );
        assert_eq!(
            top(&mut element, &mut tree, &node),
            first,
            "stream/high-refresh redraws cannot accelerate drag scrolling"
        );
        send(
            Event::Window(iced::window::Event::RedrawRequested(
                now + std::time::Duration::from_millis(32),
            )),
            outside,
            &mut element,
            &mut tree,
            &mut node,
        );
        let second = top(&mut element, &mut tree, &node);
        assert!(
            ((second - first) - 2.0 * (first - before)).abs() < 0.1,
            "scroll movement scales with elapsed time"
        );
    }

    #[test]
    fn nested_output_hidden_bounds_do_not_steal_the_following_paragraph_endpoint() {
        let renderer = crate::app::tests::headless_renderer();
        let output = (0..50)
            .map(|index| format!("output {index}\n"))
            .collect::<String>();
        let mut element: Element<'_, Message> = widget::column::with_children(vec![
            widget::scrollable(prose(&output))
                .height(80)
                .width(300)
                .into(),
            prose("following paragraph"),
        ])
        .spacing(space::M)
        .into();
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 300.0)),
        );
        let mut read = Read::new(None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        assert!(read.blocks[0].bounds.height > 300.0);
        let point = Point::new(5.0, read.blocks[1].bounds.y + 4.0);
        let mut read = Read::new(Some(point));
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        assert_eq!(
            read.endpoint(false).unwrap().identity,
            read.blocks[1].identity
        );
        assert!(
            read.blocks[0].hit < output.len() / 2,
            "an endpoint outside nested output clamps to visible lines"
        );
    }

    #[test]
    fn streaming_tool_append_preserves_transcript_selection_endpoints() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let theme = crate::settings::EditorSyntaxTheme::default();
        let initial = "first λ line\nselected text\n";
        let mut block = tool_text::ToolText::update(None, initial, tool_text::Language::Plain);
        let mut element = surface(block.view(theme, None, target, None));
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(300.0, 300.0));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut read = Read::new(None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        let identity = read.blocks[0].identity;
        let state = tree.state.downcast_mut::<State>();
        state.anchor = Some(Endpoint {
            identity,
            byte: "first λ line\n".len(),
        });
        state.end = Some(Endpoint {
            identity,
            byte: "first λ line\nselected".len(),
        });
        let expected = (state.anchor, state.end);
        block = tool_text::ToolText::update(
            Some(block),
            &format!("{initial}appended output\n"),
            tool_text::Language::Plain,
        );
        element = surface(block.view(theme, None, target, None));
        tree.diff(&mut element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let state = tree.state.downcast_ref::<State>();
        assert_eq!((state.anchor, state.end), expected);
        let mut read = Read::new(None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        assert_eq!(
            read.copy(
                &read
                    .ranges(expected.0.unwrap(), expected.1.unwrap())
                    .unwrap()
            ),
            "selected"
        );
    }

    #[test]
    fn nested_scroll_translation_and_clipping_exclude_hidden_code_from_clicks() {
        let renderer = crate::app::tests::headless_renderer();
        let id = Id::new("code");
        let mut element: Element<'_, Message> = widget::scrollable(
            widget::container(
                widget::scrollable(prose(&"long code ".repeat(30)))
                    .direction(iced::widget::scrollable::Direction::Horizontal(
                        design::scrollbar(),
                    ))
                    .id(id.clone())
                    .width(120),
            )
            .padding(20),
        )
        .height(100)
        .width(300)
        .into();
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 100.0)),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::scrollable::scroll_to(
                id,
                operation::scrollable::AbsoluteOffset {
                    x: Some(60.0),
                    y: None,
                },
            ),
        );
        let mut read = Read::new(Some(Point::new(150.0, 25.0)));
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        assert_eq!(read.blocks.len(), 1);
        assert!(read.blocks[0].bounds.x < 20.0);
        assert!(read.blocks[0].bounds.contains(read.pointer.unwrap()));
        assert!(
            read.endpoint(true).is_none(),
            "text outside the code viewport is clipped"
        );
        read.pointer = Some(Point::new(30.0, 25.0));
        assert!(read.endpoint(true).is_some());
    }
    #[test]
    fn logical_search_data_can_leave_the_ui_thread_without_presentation_state() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Catalog>();
        send_sync::<Vec<(u64, Arc<str>)>>();
        let content = markdown::Content::parse("Search **rendered** text.");
        let mut builder = Catalog::builder();
        builder.markdown("item", &[0], content.items());
        let catalog = builder.finish();
        let blocks = catalog.search_blocks();
        assert_eq!(blocks[0].1.as_ref(), "Search rendered text.");
        assert_eq!(
            catalog.retained_plain_bytes(),
            blocks.iter().map(|(_, text)| text.len()).sum::<usize>()
        );
    }

    #[test]
    fn logical_catalog_uses_the_existing_structure_aware_copy_oracle() {
        for body in [
            "A **bold** paragraph.\n\n```rust\nlet value = 42;\n```\n",
            "7. α first\n8. β second\n\n- [x] done\n  - nested\n- [ ] pending\n",
            "| α | β |\n| --- | --- |\n| first | second |\n| | |\n",
            "> A quoted paragraph.\n>\n> - one\n> - two\n",
        ] {
            let content = markdown::Content::parse(body);
            let mut builder = Catalog::builder();
            builder.markdown("message", &[0], content.items());
            let catalog = builder.finish();
            let (start, end) = catalog.endpoints().unwrap();
            let mut element = rendered_markdown(body, |_| {});
            let mut tree = Tree::new(&element);
            let read = read_rendered(&mut element, &mut tree);
            let last = read.blocks.len() - 1;
            let expected = copy_between(&read, 0, 0, last, read.blocks[last].plain.len());
            assert_eq!(catalog.copy(start, end).unwrap(), expected, "{body:?}");
            assert_eq!(catalog.copy(end, start).unwrap(), expected);
        }
    }

    #[test]
    fn logical_endpoints_survive_offscreen_eviction_and_catalog_rehydration() {
        let body = format!(
            "α first\n\n{}\n\nβ last",
            "offscreen paragraph\n\n".repeat(10000)
        );
        let build = || {
            let content = markdown::Content::parse(&body);
            let mut builder = Catalog::builder();
            builder.markdown("stable-item", &[0], content.items());
            builder.finish()
        };
        let first = build();
        let (start, end) = first.endpoints().unwrap();
        let bookmark = Bookmark {
            anchor: Some(start),
            end: Some(end),
            ..Default::default()
        };
        let expected = first.copy(start, end).unwrap();
        drop(first);
        let rehydrated = build();
        assert_eq!(
            rehydrated
                .copy(bookmark.anchor.unwrap(), bookmark.end.unwrap())
                .unwrap(),
            expected
        );
        // A sparse pointer frame has no widget for either endpoint. It can
        // still resolve the logical range without traversing hidden widgets.
        let sparse = Read::with_catalog(None, Some(rehydrated));
        let ranges = sparse
            .ranges(start, end)
            .expect("offscreen logical endpoints");
        assert!(ranges.blocks.is_empty());
        assert_eq!(sparse.copy(&ranges), expected);
    }
}
