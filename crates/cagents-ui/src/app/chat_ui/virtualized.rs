//! A transcript column whose widget trees exist only near the viewport.
//!
//! Row descriptors and measured heights are cheap. Shaped text, editors and
//! retained elements belong only to mounted rows and are dropped on eviction.
use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use iced::{Event, Point, Rectangle, Size, Vector};
use std::{cell::Cell, rc::Rc};

const OVERSCAN: f32 = 600.0;
#[cfg(test)]
thread_local! { static CONSTRUCTED: Cell<usize> = const { Cell::new(0) }; }
#[cfg(test)]
thread_local! { static PLACEHOLDER_REPORTS: Cell<usize> = const { Cell::new(0) }; }
#[cfg(test)]
pub(super) fn constructed_rows() -> usize {
    CONSTRUCTED.with(Cell::get)
}
#[cfg(test)]
pub(super) fn reset_placeholder_reports() {
    PLACEHOLDER_REPORTS.with(|count| count.set(0));
}
#[cfg(test)]
pub(super) fn placeholder_reports() -> usize {
    PLACEHOLDER_REPORTS.with(Cell::get)
}

static NEXT_COLUMN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Clone, Copy)]
pub(super) struct Viewport {
    pub top: f32,
    pub height: f32,
    pub bottom: bool,
}
pub(super) type Window = Rc<Cell<Option<Viewport>>>;

/// Offscreen aliases are expensive in a long transcript. Operations name the
/// aliases whose geometry they need; pointer, scroll and layout bookkeeping
/// never enumerate them.
#[derive(Default)]
pub(super) struct PlaceholderPolicy {
    ids: std::collections::HashSet<Id>,
    all: bool,
}
impl PlaceholderPolicy {
    pub(super) fn include(&mut self, id: Id) {
        self.ids.insert(id);
    }

    #[cfg(test)]
    fn include_all(&mut self) {
        self.all = true;
    }
}

#[derive(Clone, PartialEq)]
pub(super) struct LogicalRange {
    pub identity: u64,
    pub bytes: std::ops::Range<usize>,
    pub chunk: bool,
}

/// An immutable prepared outline can back many nested row descriptors. The
/// range/chunk view is body-free and avoids cloning every descendant per view.
#[derive(Clone, Default)]
pub(super) struct LogicalRanges {
    source: Option<std::sync::Arc<[LogicalRange]>>,
    range: std::ops::Range<usize>,
    chunk: bool,
}
impl LogicalRanges {
    pub(super) fn shared(
        source: std::sync::Arc<[LogicalRange]>,
        range: std::ops::Range<usize>,
        chunk: bool,
    ) -> Self {
        if range.start > range.end || range.end > source.len() {
            return Self::default();
        }
        Self {
            source: Some(source),
            range,
            chunk,
        }
    }
    /// Whether both name the same ranges of the same prepared outline.
    fn same(&self, other: &Self) -> bool {
        let source = match (&self.source, &other.source) {
            (Some(left), Some(right)) => std::sync::Arc::ptr_eq(left, right),
            (left, right) => left.is_none() && right.is_none(),
        };
        source && self.range == other.range && self.chunk == other.chunk
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = LogicalRange> + '_ {
        self.source.as_deref().unwrap_or(&[])[self.range.clone()]
            .iter()
            .cloned()
            .map(|mut logical| {
                logical.chunk |= self.chunk;
                logical
            })
    }
}
pub(super) struct Row<'a> {
    pub key: u64,
    pub revision: u64,
    pub estimate: f32,
    pub aliases: Vec<Id>,
    pub anchor: bool,
    pub logical: Option<LogicalRange>,
    pub descendants: LogicalRanges,
    source: Source<'a>,
    content: Option<Element<'a, Message>>,
}
/// What a row builds its element from.
enum Source<'a> {
    /// A builder that can run again after the row's tree is evicted.
    Rebuild(Box<dyn Fn() -> Element<'a, Message> + 'a>),
    /// A builder that runs once; the row keeps the element it built.
    Once(Option<Box<dyn FnOnce() -> Element<'a, Message> + 'a>>),
    /// Empty space of this height, which needs no builder of its own.
    Space(f32),
}
impl<'a> Row<'a> {
    pub fn once(
        key: u64,
        revision: u64,
        estimate: f32,
        id: Option<Id>,
        build: impl FnOnce() -> Element<'a, Message> + 'a,
    ) -> Self {
        Self {
            key,
            revision,
            estimate,
            aliases: id.into_iter().collect(),
            anchor: false,
            logical: None,
            descendants: LogicalRanges::default(),
            source: Source::Once(Some(Box::new(build))),
            content: None,
        }
    }
    pub fn new(
        key: u64,
        revision: u64,
        estimate: f32,
        id: Option<Id>,
        build: impl Fn() -> Element<'a, Message> + 'a,
    ) -> Self {
        Self {
            key,
            revision,
            estimate,
            aliases: id.into_iter().collect(),
            anchor: false,
            logical: None,
            descendants: LogicalRanges::default(),
            source: Source::Rebuild(Box::new(build)),
            content: None,
        }
    }
    /// A row of empty space, such as the gap between two other rows.
    pub fn space(key: u64, revision: u64, height: f32) -> Self {
        Self {
            key,
            revision,
            estimate: height,
            aliases: Vec::new(),
            anchor: false,
            logical: None,
            descendants: LogicalRanges::default(),
            source: Source::Space(height),
            content: None,
        }
    }
    fn element(&mut self) -> &mut Element<'a, Message> {
        if self.content.is_none() {
            #[cfg(test)]
            CONSTRUCTED.with(|count| count.set(count.get() + 1));
            self.content = Some(match &mut self.source {
                Source::Rebuild(build) => build(),
                Source::Once(build) => build.take().expect("a static row is built once")(),
                Source::Space(height) => widget::Space::new().height(*height).into(),
            });
        }
        self.content.as_mut().unwrap()
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Axis {
    #[default]
    Vertical,
    Horizontal,
}
impl Axis {
    fn point(self, main: f32) -> Point {
        match self {
            Self::Vertical => Point::new(0.0, main),
            Self::Horizontal => Point::new(main, 0.0),
        }
    }
    fn main(self, size: Size) -> f32 {
        match self {
            Self::Vertical => size.height,
            Self::Horizontal => size.width,
        }
    }
    fn cross(self, size: Size) -> f32 {
        match self {
            Self::Vertical => size.width,
            Self::Horizontal => size.height,
        }
    }
    fn size(self, main: f32, cross: f32) -> Size {
        match self {
            Self::Vertical => Size::new(cross, main),
            Self::Horizontal => Size::new(main, cross),
        }
    }
}

pub(super) fn column<'a>(rows: Vec<Row<'a>>, window: Window) -> Element<'a, Message> {
    Element::new(Column {
        rows,
        window,
        revision: NEXT_COLUMN.fetch_add(1, Ordering::Relaxed),
        axis: Axis::Vertical,
        cross_estimate: 0.0,
    })
}

/// Horizontal virtualization preserves a physical code line/table row while
/// only instantiating cells near its horizontal native scroll viewport.
pub(super) fn row<'a>(rows: Vec<Row<'a>>, height: f32, window: Window) -> Element<'a, Message> {
    Element::new(Column {
        rows,
        window,
        revision: NEXT_COLUMN.fetch_add(1, Ordering::Relaxed),
        axis: Axis::Horizontal,
        cross_estimate: height,
    })
}

struct Column<'a> {
    axis: Axis,
    cross_estimate: f32,
    revision: u64,
    rows: Vec<Row<'a>>,
    window: Window,
}
#[derive(Clone, Copy)]
struct Height {
    revision: u64,
    height: f32,
    estimate: f32,
    cross: f32,
}
impl Height {
    fn for_row(&self, row: &Row<'_>) -> f32 {
        if self.revision == row.revision {
            self.height
        } else {
            (self.height + row.estimate - self.estimate).max(0.0)
        }
    }
}
#[cfg(test)]
thread_local! {
    /// How many times this thread's columns rebuilt every index.
    static REINDEXES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// What a row last gave the column's indexes, so a column rebuilt with the
/// same rows corrects only the offsets of rows that changed.
struct Shape {
    key: u64,
    anchor: bool,
    aliases: Vec<Id>,
    logical: Option<LogicalRange>,
    descendants: LogicalRanges,
    revision: u64,
    estimate: f32,
}
impl Shape {
    fn of(row: &Row<'_>) -> Self {
        Self {
            key: row.key,
            anchor: row.anchor,
            aliases: row.aliases.clone(),
            logical: row.logical.clone(),
            descendants: row.descendants.clone(),
            revision: row.revision,
            estimate: row.estimate,
        }
    }
    /// Whether `row` holds the same place in every index but the offsets.
    fn indexes(&self, row: &Row<'_>) -> bool {
        self.key == row.key
            && self.anchor == row.anchor
            && self.aliases == row.aliases
            && self.logical == row.logical
            && self.descendants.same(&row.descendants)
    }
}

#[derive(Default)]
struct State {
    axis: Axis,
    width: f32,
    heights: HashMap<u64, Height>,
    cross_extents: std::collections::BTreeMap<u32, usize>,
    mounted: Vec<(usize, u64)>,
    // The exact last layout window, including an empty insertion point.
    realized: std::ops::Range<usize>,
    offsets: PrefixSums,
    descriptors: u64,
    shapes: Vec<Shape>,
    observed: Option<Viewport>,
    positions: HashMap<u64, usize>,
    anchors: HashSet<u64>,
    logical: HashMap<u64, Vec<(std::ops::Range<usize>, usize, bool)>>,
    aliases: HashMap<Id, Vec<usize>>,
}

/// Mutable measured heights with logarithmic corrections and prefix lookup.
/// Ordinary wheel/layout passes never rebuild an all-history offset vector.
#[derive(Default)]
struct PrefixSums {
    values: Vec<f32>,
    tree: Vec<f32>,
}
impl PrefixSums {
    fn new(values: Vec<f32>) -> Self {
        let mut index = Self {
            tree: vec![0.0; values.len() + 1],
            values,
        };
        for at in 1..index.tree.len() {
            index.tree[at] += index.values[at - 1];
            let next = at + (at & at.wrapping_neg());
            if next < index.tree.len() {
                index.tree[next] += index.tree[at];
            }
        }
        index
    }
    fn prefix(&self, mut end: usize) -> f32 {
        let mut sum = 0.0;
        while end > 0 {
            sum += self.tree[end];
            end &= end - 1;
        }
        sum
    }
    fn total(&self) -> f32 {
        self.prefix(self.values.len())
    }
    fn set(&mut self, index: usize, value: f32) {
        let delta = value - self.values[index];
        self.values[index] = value;
        let mut at = index + 1;
        while at < self.tree.len() {
            self.tree[at] += delta;
            at += at & at.wrapping_neg();
        }
    }
    fn partition_point(&self, predicate: impl Fn(f32) -> bool) -> usize {
        let mut low = 0;
        let mut high = self.values.len() + 1;
        while low < high {
            let middle = low + (high - low) / 2;
            if predicate(self.prefix(middle)) {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }
}

fn visible(offsets: &PrefixSums, top: f32, height: f32) -> std::ops::Range<usize> {
    let count = offsets.values.len();
    let first = offsets
        .partition_point(|offset| offset < top - OVERSCAN)
        .saturating_sub(1)
        .min(count);
    let end = offsets
        .partition_point(|offset| offset <= top + height + OVERSCAN)
        .min(count);
    first..end.max(first)
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Column<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }
    fn diff(&mut self, _: &mut Tree) {}
    fn size(&self) -> Size<Length> {
        match self.axis {
            Axis::Vertical => Size::new(Length::Fill, Length::Shrink),
            Axis::Horizontal => Size::new(Length::Shrink, Length::Shrink),
        }
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<State>();
        let width = match self.axis {
            Axis::Vertical => {
                if limits.max().width.is_finite() {
                    limits.max().width
                } else {
                    design::READABLE_MEASURE
                }
            }
            Axis::Horizontal => self.cross_estimate,
        };
        // An unbounded horizontal parent needs each physical code line's
        // natural width. The finite estimate is only for viewport estimates;
        // it must not cap the child's measured horizontal scroll extent.
        let constraint = match self.axis {
            Axis::Vertical => limits.max().width,
            Axis::Horizontal => self.cross_estimate,
        };
        let width_changed = state.width != constraint || state.axis != self.axis;
        state.axis = self.axis;
        if width_changed {
            state.heights.clear();
            state.width = constraint;
        }
        // A transcript builds its column again for every frame, mostly with
        // the same rows: a streaming answer changes one row's revision.
        // Rebuilding every index for that is work in the length of the
        // transcript on every frame, so only a changed row is corrected.
        let reshaped = state.descriptors != self.revision
            && (state.shapes.len() != self.rows.len()
                || !self
                    .rows
                    .iter()
                    .zip(&state.shapes)
                    .all(|(row, shape)| shape.indexes(row)));
        if !width_changed && !reshaped && state.descriptors != self.revision {
            for (index, (row, shape)) in self.rows.iter().zip(&mut state.shapes).enumerate() {
                if shape.revision != row.revision || shape.estimate != row.estimate {
                    shape.revision = row.revision;
                    shape.estimate = row.estimate;
                    state.offsets.set(
                        index,
                        state
                            .heights
                            .get(&row.key)
                            .map_or(row.estimate, |height| height.for_row(row))
                            .max(0.0),
                    );
                }
            }
            state.descriptors = self.revision;
        }
        if width_changed || reshaped {
            #[cfg(test)]
            REINDEXES.with(|count| count.set(count.get() + 1));
            state.shapes = self.rows.iter().map(Shape::of).collect();
            state.positions = self
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| (row.key, index))
                .collect();
            state.anchors = self
                .rows
                .iter()
                .filter(|row| row.anchor)
                .map(|row| row.key)
                .collect();
            state.aliases.clear();
            for (index, row) in self.rows.iter().enumerate() {
                for id in &row.aliases {
                    state.aliases.entry(id.clone()).or_default().push(index);
                }
            }
            state.logical.clear();
            for (index, row) in self.rows.iter().enumerate() {
                for logical in row.logical.iter().cloned().chain(row.descendants.iter()) {
                    state.logical.entry(logical.identity).or_default().push((
                        logical.bytes.clone(),
                        index,
                        logical.chunk,
                    ));
                }
            }
            // Retain only descriptors still loaded by the transcript window.
            let keys: HashSet<_> = self.rows.iter().map(|row| row.key).collect();
            state.heights.retain(|key, _| keys.contains(key));
            state.cross_extents.clear();
            for measured in state.heights.values() {
                *state
                    .cross_extents
                    .entry(measured.cross.to_bits())
                    .or_default() += 1;
            }
            state.offsets = PrefixSums::new(
                self.rows
                    .iter()
                    .map(|row| {
                        state
                            .heights
                            .get(&row.key)
                            .map_or(row.estimate, |height| height.for_row(row))
                            .max(0.0)
                    })
                    .collect(),
            );
            state.descriptors = self.revision;
        }
        let mut nodes = Vec::new();
        for _ in 0..3 {
            let total = state.offsets.total();
            let viewport = state.observed.or(self.window.get()).unwrap_or(Viewport {
                top: 0.0,
                height: 800.0,
                bottom: self.axis == Axis::Vertical,
            });
            let top = if viewport.bottom {
                (total - viewport.height).max(0.0)
            } else {
                viewport.top
            };
            let range = visible(&state.offsets, top, viewport.height);
            // A measurement in this layout can shrink the mounted source
            // below the old observed viewport. Keep this pass's native owners
            // until the parent refreshes that viewport; dropping them here can
            // discard the ready geometry that produced the shrink. An empty
            // window on the first pass still evicts genuinely offscreen rows.
            if range.is_empty() && !nodes.is_empty() {
                break;
            }
            state.realized = range.clone();
            let mut previous: HashMap<_, _> = state
                .mounted
                .drain(..)
                .zip(std::mem::take(&mut tree.children))
                .map(|((_, key), tree)| (key, tree))
                .collect();
            nodes.clear();
            let mut changed = false;
            for index in range {
                let row = &mut self.rows[index];
                let key = row.key;
                let revision = row.revision;
                let estimate = row.estimate;
                let element = row.element();
                let mut child = previous
                    .remove(&key)
                    .unwrap_or_else(|| Tree::new(&*element));
                child.diff(element.as_widget_mut());
                let child_max = match self.axis {
                    Axis::Vertical => Size::new(limits.max().width, f32::INFINITY),
                    Axis::Horizontal => Size::new(f32::INFINITY, f32::INFINITY),
                };
                let mut node = element.as_widget_mut().layout(
                    &mut child,
                    renderer,
                    // Match an intrinsic iced Column/Row: fluid descendants
                    // shrink on unbounded axes. Activity rails inherit Fill
                    // height, so discarding this compression makes them grow
                    // to infinity when a Find-expanded tool has no height cap.
                    &layout::Limits::with_compression(
                        Size::ZERO,
                        child_max,
                        Size::new(
                            limits.compression().width || !child_max.width.is_finite(),
                            true,
                        ),
                    ),
                );
                let height = self.axis.main(node.size());
                changed |= (height - state.offsets.values[index]).abs() > 0.5;
                let cross = self.axis.cross(node.size()).max(0.0);
                if let Some(previous) = state.heights.insert(
                    key,
                    Height {
                        revision,
                        height,
                        estimate,
                        cross,
                    },
                ) && let Some(count) = state.cross_extents.get_mut(&previous.cross.to_bits())
                {
                    *count -= 1;
                    if *count == 0 {
                        state.cross_extents.remove(&previous.cross.to_bits());
                    }
                }
                *state.cross_extents.entry(cross.to_bits()).or_default() += 1;
                state.offsets.set(index, height);
                node.move_to_mut(self.axis.point(state.offsets.prefix(index)));
                nodes.push(node);
                tree.children.push(child);
                state.mounted.push((index, key));
            }
            // Reusable descriptors retain logical sources, never visited
            // widget elements or styled fragments once their tree is evicted.
            for key in previous.keys() {
                if let Some(index) = state.positions.get(key) {
                    let row = &mut self.rows[*index];
                    if !matches!(row.source, Source::Once(_)) {
                        row.content = None;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        // Update positions after measurements, retaining the viewport's anchor
        // through the outer transcript's layout-time anchoring operation.
        let total = state.offsets.total();
        for ((index, _), node) in state.mounted.iter().zip(&mut nodes) {
            node.move_to_mut(self.axis.point(state.offsets.prefix(*index)));
        }
        // An evicted row keeps only cheap measured geometry. Losing its
        // cross extent would collapse a horizontal scrollbar while scrolling
        // past short physical lines (or shrink a tall table row sideways).
        let measured_cross = state
            .cross_extents
            .last_key_value()
            .map_or(0.0, |(bits, _)| f32::from_bits(*bits));
        let cross = match self.axis {
            Axis::Vertical if limits.max().width.is_finite() => width,
            Axis::Vertical => measured_cross,
            Axis::Horizontal => measured_cross.max(width),
        };
        layout::Node::with_children(self.axis.size(total, cross), nodes)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        let state = tree.state.downcast_mut::<State>();
        operation.custom(None, layout.bounds(), state);
        // Placeholder bounds make unloaded items addressable without
        // constructing their text or widget trees. The operation names the
        // aliases it needs, so this remains proportional to its saved reading
        // window rather than every alias in a loaded history page.
        let mut placeholders = PlaceholderPolicy::default();
        operation.custom(None, layout.bounds(), &mut placeholders);
        let mut report = |id: &Id, indexes: &[usize]| {
            for &index in indexes {
                let start = state.offsets.prefix(index);
                let size = self.axis.size(
                    state.offsets.values[index],
                    self.axis.cross(layout.bounds().size()),
                );
                operation.container(
                    Some(id),
                    Rectangle::new(
                        layout.bounds().position()
                            + match self.axis {
                                Axis::Vertical => Vector::new(0.0, start),
                                Axis::Horizontal => Vector::new(start, 0.0),
                            },
                        size,
                    ),
                );
                #[cfg(test)]
                PLACEHOLDER_REPORTS.with(|count| count.set(count.get() + 1));
            }
        };
        if placeholders.all {
            for (id, indexes) in &state.aliases {
                report(id, indexes);
            }
        } else {
            for id in &placeholders.ids {
                if let Some((id, indexes)) = state.aliases.get_key_value(id) {
                    report(id, indexes);
                }
            }
        }
        operation.traverse(&mut |operation| {
            for (((index, _), child), child_layout) in state
                .mounted
                .iter()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                self.rows[*index]
                    .content
                    .as_mut()
                    .unwrap()
                    .as_widget_mut()
                    .operate(
                        child,
                        child_layout.with_virtual_offset(layout.virtual_offset()),
                        renderer,
                        operation,
                    );
            }
        });
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
        for (((index, _), child), child_layout) in state
            .mounted
            .iter()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            if child_layout.bounds().intersection(viewport).is_some() {
                self.rows[*index]
                    .content
                    .as_mut()
                    .unwrap()
                    .as_widget_mut()
                    .update(
                        child,
                        event,
                        child_layout.with_virtual_offset(layout.virtual_offset()),
                        cursor,
                        renderer,
                        clipboard,
                        shell,
                        viewport,
                    );
            }
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
        let state = tree.state.downcast_ref::<State>();
        for (((index, _), child), child_layout) in state
            .mounted
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            if child_layout.bounds().intersection(viewport).is_some() {
                self.rows[*index]
                    .content
                    .as_ref()
                    .unwrap()
                    .as_widget()
                    .draw(
                        child,
                        renderer,
                        theme,
                        style,
                        child_layout.with_virtual_offset(layout.virtual_offset()),
                        cursor,
                        viewport,
                    );
            }
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State>();
        state
            .mounted
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .filter_map(|(((index, _), child), child_layout)| {
                child_layout.bounds().intersection(viewport)?;
                Some(
                    self.rows[*index]
                        .content
                        .as_ref()
                        .unwrap()
                        .as_widget()
                        .mouse_interaction(
                            child,
                            child_layout.with_virtual_offset(layout.virtual_offset()),
                            cursor,
                            viewport,
                            renderer,
                        ),
                )
            })
            .max()
            .unwrap_or_default()
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, cosmic::Theme, cosmic::Renderer>> {
        let state = tree.state.downcast_ref::<State>();
        let mut overlays = Vec::new();
        // Rows and trees are disjoint, in the same monotonically increasing order.
        let mut remaining = self.rows.as_mut_slice();
        let mut offset = 0;
        for (((index, _), child), child_layout) in state
            .mounted
            .iter()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            let (_, rest) = remaining.split_at_mut(*index - offset);
            let (row, rest) = rest.split_first_mut().unwrap();
            remaining = rest;
            offset = *index + 1;
            if let Some(overlay) = row.content.as_mut().unwrap().as_widget_mut().overlay(
                child,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                renderer,
                viewport,
                translation,
            ) {
                overlays.push(overlay);
            }
        }
        (!overlays.is_empty()).then(|| overlay::Group::with_children(overlays).overlay())
    }
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        let state = tree.state.downcast_ref::<State>();
        iced_accessibility::A11yTree::join(
            state
                .mounted
                .iter()
                .zip(&tree.children)
                .zip(layout.children())
                .map(|(((index, _), child), child_layout)| {
                    self.rows[*index]
                        .content
                        .as_ref()
                        .unwrap()
                        .as_widget()
                        .a11y_nodes(
                            child_layout.with_virtual_offset(layout.virtual_offset()),
                            child,
                            cursor,
                        )
                }),
        )
    }
}

/// Geometry from cheap row descriptions. Exact glyph geometry, when mounted,
/// supersedes this estimate in scroll::locate_logical. No hidden builder runs.
pub(super) fn logical_bounds(
    state: &dyn std::any::Any,
    bounds: Rectangle,
    identity: u64,
    byte: usize,
) -> Option<(Rectangle, u8)> {
    let state = state.downcast_ref::<State>()?;
    let ranges = state.logical.get(&identity)?;
    let at = ranges
        .partition_point(|(range, _, _)| range.start <= byte)
        .saturating_sub(1);
    let (range, index, chunk) = ranges.get(at)?;
    if byte > range.end {
        return None;
    }
    let top = state.offsets.prefix(*index);
    let height = state.offsets.prefix(*index + 1) - top;
    let fraction = byte.saturating_sub(range.start) as f32 / range.len().max(1) as f32;
    let rectangle = match state.axis {
        Axis::Vertical => Rectangle {
            x: bounds.x,
            y: bounds.y + top + height * fraction,
            width: bounds.width,
            height: 1.0,
        },
        Axis::Horizontal => Rectangle {
            x: bounds.x + top + height * fraction,
            y: bounds.y,
            width: 1.0,
            height: bounds.height,
        },
    };
    Some((rectangle, if *chunk { 2 } else { 1 }))
}

/// Fine-grained anchors include the visible window and previously anchored
/// rows even when corrected estimates moved them outside the current window.
/// No hidden row is realized to answer this geometry request, so each anchor
/// says whether it is mounted: an unmounted one has only its estimate.
pub(super) fn anchor_points(
    state: &mut dyn std::any::Any,
    bounds: Rectangle,
    wanted: &HashSet<u64>,
) -> Vec<(u64, Rectangle, bool)> {
    let Some(state) = state.downcast_ref::<State>() else {
        return vec![];
    };
    let mut keys: HashSet<_> = state
        .mounted
        .iter()
        .map(|(_, key)| *key)
        .filter(|key| state.anchors.contains(key))
        .collect();
    keys.extend(
        wanted
            .iter()
            .filter(|key| state.anchors.contains(key))
            .copied(),
    );
    keys.into_iter()
        .filter_map(|key| {
            let index = *state.positions.get(&key)?;
            let top = state.offsets.prefix(index);
            let rectangle = match state.axis {
                Axis::Vertical => Rectangle {
                    x: bounds.x,
                    y: bounds.y + top,
                    width: bounds.width,
                    height: state.offsets.values[index],
                },
                Axis::Horizontal => Rectangle {
                    x: bounds.x + top,
                    y: bounds.y,
                    width: state.offsets.values[index],
                    height: bounds.height,
                },
            };
            let mounted = state.mounted.iter().any(|(_, mounted)| *mounted == key);
            Some((key, rectangle, mounted))
        })
        .collect()
}

/// The outer native scrollable supplies its actual translation after layout
/// and input. Nested virtual columns share that coordinate space; horizontal
/// code scrollables do not replace the outer vertical viewport.
#[derive(Default)]
pub(super) struct UpdateViewport {
    translation: Vector,
    clip: Option<Rectangle>,
    pending: Option<(Rectangle, Vector)>,
    pub changed: bool,
    touches: usize,
    exchanges: Vec<usize>,
}
impl Operation for UpdateViewport {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        let previous = self.translation;
        let clip = self.clip;
        if let Some((bounds, translation)) = self.pending.take() {
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
        visit(self);
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
        self.pending = Some((bounds, translation));
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(invalidation) = state.downcast_mut::<retained::LayoutInvalidation>() {
            match invalidation {
                retained::LayoutInvalidation::Enter => self.exchanges.push(self.touches),
                retained::LayoutInvalidation::Exit { changed } => {
                    *changed = self
                        .exchanges
                        .pop()
                        .is_some_and(|before| before != self.touches)
                }
            }
            return;
        }
        if let (Some(state), Some(viewport)) = (state.downcast_mut::<State>(), self.clip) {
            let (top, height) = match state.axis {
                Axis::Vertical => (
                    (viewport.y + self.translation.y - bounds.y).max(0.0),
                    viewport.height,
                ),
                Axis::Horizontal => (
                    (viewport.x + self.translation.x - bounds.x).max(0.0),
                    viewport.width,
                ),
            };
            let wanted = visible(&state.offsets, top, height);
            if wanted != state.realized {
                self.changed = true;
                self.touches += 1;
            }
            state.observed = Some(Viewport {
                top,
                height,
                bottom: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn measured_shrink_keeps_mounted_flow_until_parent_refresh_and_offscreen_evicts() {
        use std::cell::RefCell;

        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 1,
        };
        let cache = flow_layout::Cache::default();
        let owner = Rc::new(RefCell::new(std::rc::Weak::new()));
        let spans: Arc<[iced::widget::text::Span<'static, markdown::Uri>]> =
            vec![iced::widget::text::Span::new(
                "geometry source ".repeat(256),
            )]
            .into();
        let window = Rc::new(Cell::new(Some(Viewport {
            top: 9_600.0,
            height: 400.0,
            bottom: false,
        })));
        let reader = |height: f32| {
            let cache = cache.clone();
            let owner = owner.clone();
            let spans = spans.clone();
            column(
                vec![Row::new(
                    7,
                    u64::from(height.to_bits()),
                    10_000.0,
                    None,
                    move || {
                        let flow = cache.entry(7, spans.clone());
                        *owner.borrow_mut() = Rc::downgrade(&flow);
                        widget::container(selectable_prose::flow_source(
                            iced::Pixels(14.0),
                            iced::Font::default(),
                            target,
                            None,
                            selectable_prose::Logical {
                                identity: 7,
                                start: 0,
                            },
                            flow,
                        ))
                        .height(height)
                        .into()
                    },
                )],
                window.clone(),
            )
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(600.0, f32::INFINITY));
        let mut element = reader(10_000.0);
        let mut tree = Tree::new(&element);
        let initial = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert_eq!(initial.size().height, 10_000.0);
        let original = owner.borrow().clone();
        assert!(
            original.upgrade().is_some(),
            "the actual mounted Flow owns its geometry"
        );

        // Model an asynchronous measurement replacing a larger estimate while
        // the parent still observes its old end window. The real native tree
        // and weak Flow cache must keep the same owner through this layout.
        element = reader(1_000.0);
        tree.diff(element.as_widget_mut());
        let shrunk = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert_eq!(shrunk.size().height, 1_000.0);
        assert!(
            original.upgrade().is_some(),
            "own measurement must not evict ready Flow before its parent refreshes"
        );
        assert_eq!(tree.children.len(), 1);

        window.set(Some(Viewport {
            top: 600.0,
            height: 400.0,
            bottom: false,
        }));
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(
            original.upgrade().is_some(),
            "the refreshed intersecting viewport reuses its existing owner"
        );

        // A later authoritative viewport really is past this whole column.
        // It must release both the native tree and the row's element, rather
        // than clamp every local viewport into its own content and pin it.
        window.set(Some(Viewport {
            top: 10_000.0,
            height: 400.0,
            bottom: false,
        }));
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(tree.children.is_empty());
        assert!(
            original.upgrade().is_none(),
            "genuinely offscreen Flow must retire"
        );

        window.set(Some(Viewport {
            top: 0.0,
            height: 400.0,
            bottom: false,
        }));
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(
            owner.borrow().upgrade().is_some(),
            "normal reentry rebuilds the evicted owner"
        );
        assert!(original.upgrade().is_none());
    }

    #[test]
    fn empty_virtual_windows_settle_and_reenter_after_native_scroll() {
        fn reader(axis: Axis, count: usize, reverse: bool, id: Id) -> Element<'static, Message> {
            let rows = (0..count)
                .map(|at| {
                    let index = if reverse { count - at - 1 } else { at };
                    Row::new(index as u64 + 1, 1, 20.0, None, move || {
                        widget::Space::new().width(20.0).height(20.0).into()
                    })
                })
                .collect();
            let (body, direction): (Element<'static, Message>, _) = match axis {
                Axis::Vertical => (
                    widget::column::with_children(vec![
                        column(rows, Default::default()),
                        widget::Space::new().height(100_000.0).into(),
                    ])
                    .into(),
                    iced::widget::scrollable::Direction::Vertical(Default::default()),
                ),
                Axis::Horizontal => (
                    widget::row::with_children(vec![
                        row(rows, 20.0, Default::default()),
                        widget::Space::new().width(100_000.0).into(),
                    ])
                    .into(),
                    iced::widget::scrollable::Direction::Horizontal(Default::default()),
                ),
            };
            widget::scrollable(body)
                .id(id)
                .direction(direction)
                .anchor_bottom()
                .anchor_right()
                .width(600.0)
                .height(400.0)
                .into()
        }
        #[derive(Default)]
        struct Probe(Option<(std::ops::Range<usize>, usize, usize, Option<usize>)>);
        impl Operation for Probe {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn custom(&mut self, _: Option<&Id>, _: Rectangle, value: &mut dyn std::any::Any) {
                if let Some(state) = value.downcast_ref::<State>() {
                    self.0 = Some((
                        state.realized.clone(),
                        state.mounted.len(),
                        state.positions.len(),
                        state
                            .positions
                            .get(&(state.positions.len() as u64))
                            .copied(),
                    ));
                }
            }
        }
        let renderer = crate::app::tests::headless_renderer();
        for axis in [Axis::Vertical, Axis::Horizontal] {
            let id = Id::unique();
            let mut element = reader(axis, 300, false, id.clone());
            let mut tree = Tree::new(&element);
            let mut limits = layout::Limits::new(Size::ZERO, Size::new(600.0, 400.0));
            let mut node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut initial = UpdateViewport::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut initial);
            assert!(
                initial.changed,
                "the actual native end viewport first evicts the old mounted rows"
            );
            node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            for _ in 0..8 {
                let mut observation = UpdateViewport::default();
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut observation,
                );
                assert!(
                    !observation.changed && observation.touches == 0,
                    "an empty realized window does not request another layout"
                );
                let mut probe = Probe::default();
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut probe,
                );
                assert_eq!(probe.0, Some((300..300, 0, 300, Some(299))));
            }
            // Native reentry must still request and mount the newly visible rows.
            let mut home = operation::scrollable::scroll_to::<()>(
                id.clone(),
                operation::scrollable::AbsoluteOffset {
                    x: Some(f32::MAX),
                    y: Some(f32::MAX),
                },
            );
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut home);
            let mut reenter = UpdateViewport::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut reenter);
            assert!(
                reenter.changed,
                "native reentry realizes a different row window"
            );
            node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut probe = Probe::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
            assert!(
                probe
                    .0
                    .is_some_and(|(range, mounted, count, _)| range.start == 0
                        && mounted > 0
                        && count == 300)
            );
            // A new descriptor count/order and width must invalidate the old indices.
            element = reader(axis, 301, true, id);
            tree.diff(element.as_widget_mut());
            limits = layout::Limits::new(Size::ZERO, Size::new(500.0, 400.0));
            node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut probe = Probe::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
            assert!(probe.0.is_some_and(|(_, mounted, count, last)| mounted > 0
                && count == 301
                && last == Some(0)));
        }
    }

    /// A transcript builds its column again for every frame. Rebuilt with the
    /// same rows, it corrects the offsets of the rows that changed; with
    /// different rows, it indexes them again. Either way an offscreen row's
    /// placeholder sits where the rows above it end.
    #[test]
    fn a_column_rebuilt_each_frame_keeps_offsets_exact() {
        #[derive(Default)]
        struct Placeholders(HashMap<Id, Rectangle>);
        impl Operation for Placeholders {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
                if let Some(policy) = state.downcast_mut::<PlaceholderPolicy>() {
                    policy.include_all();
                }
            }
            fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
                if let Some(id) = id {
                    self.0.insert(id.clone(), bounds);
                }
            }
        }
        let renderer = crate::app::tests::headless_renderer();
        let window = Rc::new(Cell::new(Some(Viewport {
            top: 0.0,
            height: 100.0,
            bottom: false,
        })));
        let build = |rows: &[(u64, f32)]| {
            column(
                rows.iter()
                    .map(|&(key, height)| {
                        Row::new(
                            key,
                            u64::from(height.to_bits()),
                            height,
                            Some(Id::from(format!("row-{key}"))),
                            move || widget::Space::new().height(height).into(),
                        )
                    })
                    .collect(),
                window.clone(),
            )
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(600.0, f32::INFINITY));
        let mut rows: Vec<(u64, f32)> = (1..=200)
            .map(|key| (key, 20.0 + (key % 3) as f32))
            .collect();
        let mut tree = Tree::new(build(&rows));
        let reindexes = || REINDEXES.with(Cell::get);
        let mut check = |rows: &[(u64, f32)]| {
            let mut element = build(rows);
            tree.diff(element.as_widget_mut());
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut placeholders = Placeholders::default();
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut placeholders,
            );
            let mut top = 0.0;
            for &(key, height) in rows {
                let bounds = placeholders.0[&Id::from(format!("row-{key}"))];
                assert_eq!((bounds.y, bounds.height), (top, height), "row {key}");
                top += height;
            }
            assert_eq!(node.size().height, top);
        };
        check(&rows);
        let indexed = reindexes();
        check(&rows);
        // An offscreen row grows, then a mounted one, and the column only
        // corrects their offsets.
        rows[150].1 = 64.0;
        check(&rows);
        rows[1].1 = 48.0;
        check(&rows);
        assert_eq!(reindexes(), indexed, "the same rows keep their indexes");
        // Earlier history arrives above, and a row leaves.
        rows.insert(0, (999, 33.0));
        rows.remove(120);
        check(&rows);
        assert_eq!(reindexes(), indexed + 1, "new rows are indexed again");
    }

    /// A row measured while mounted, then changed while offscreen, keeps
    /// its measured height adjusted by how its estimate changed, as a full
    /// reindex would place it.
    #[test]
    fn a_measured_row_changed_offscreen_keeps_its_measured_height() {
        #[derive(Default)]
        struct Placeholders(HashMap<Id, Rectangle>);
        impl Operation for Placeholders {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
                if let Some(policy) = state.downcast_mut::<PlaceholderPolicy>() {
                    policy.include_all();
                }
            }
            fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
                if let Some(id) = id {
                    self.0.insert(id.clone(), bounds);
                }
            }
        }
        let renderer = crate::app::tests::headless_renderer();
        let window = Rc::new(Cell::new(Some(Viewport {
            top: 0.0,
            height: 100.0,
            bottom: false,
        })));
        // Row 3 is drawn 50 tall against an estimate of 20.
        let build = |revision: u64, estimate: f32| {
            column(
                (1..=200u64)
                    .map(|key| {
                        let (revision, estimate, drawn) = match key {
                            3 => (revision, estimate, 50.0),
                            _ => (0, 20.0, 20.0),
                        };
                        Row::new(
                            key,
                            revision,
                            estimate,
                            Some(Id::from(format!("row-{key}"))),
                            move || widget::Space::new().height(drawn).into(),
                        )
                    })
                    .collect(),
                window.clone(),
            )
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(600.0, f32::INFINITY));
        let mut tree = Tree::new(build(1, 20.0));
        let mut placed = |revision: u64, estimate: f32| {
            let mut element = build(revision, estimate);
            tree.diff(element.as_widget_mut());
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut placeholders = Placeholders::default();
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut placeholders,
            );
            let bounds = |key: u64| placeholders.0[&Id::from(format!("row-{key}"))];
            (bounds(3).height, bounds(4).y, node.size().height)
        };
        assert_eq!(placed(1, 20.0), (50.0, 90.0, 199.0 * 20.0 + 50.0));
        window.set(Some(Viewport {
            top: 3_000.0,
            height: 100.0,
            bottom: false,
        }));
        assert_eq!(placed(1, 20.0), (50.0, 90.0, 199.0 * 20.0 + 50.0));
        let indexed = REINDEXES.with(Cell::get);
        assert_eq!(placed(2, 26.0), (56.0, 96.0, 199.0 * 20.0 + 56.0));
        assert_eq!(REINDEXES.with(Cell::get), indexed);
    }

    #[test]
    fn offscreen_aliases_are_selected_by_id() {
        struct Count {
            aliases: usize,
            requested: Option<Id>,
        }
        impl Operation for Count {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
                if let Some(id) = &self.requested
                    && let Some(policy) = state.downcast_mut::<PlaceholderPolicy>()
                {
                    policy.include(id.clone());
                }
            }
            fn container(&mut self, _: Option<&Id>, _: Rectangle) {
                self.aliases += 1;
            }
        }

        const ROWS: u64 = 10_000;
        let renderer = crate::app::tests::headless_renderer();
        let window = Rc::new(Cell::new(Some(Viewport {
            top: 100_000.0,
            height: 600.0,
            bottom: false,
        })));
        let mut element = column(
            (0..ROWS)
                .map(|key| {
                    Row::new(key, 0, 20.0, Some(Id::from(format!("row-{key}"))), || {
                        widget::Space::new().height(20).into()
                    })
                })
                .collect(),
            window,
        );
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, f32::INFINITY));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);

        let mut ordinary = Count {
            aliases: 0,
            requested: None,
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut ordinary);
        assert_eq!(
            ordinary.aliases, 0,
            "ordinary operations skip all history aliases"
        );

        let mut anchored = Count {
            aliases: 0,
            requested: Some(Id::from("row-7654")),
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut anchored);
        assert_eq!(anchored.aliases, 1);
    }

    /// Records the UI-thread cost of an operation over a long transcript.
    ///
    /// Run with `--ignored --nocapture`; the count makes it clear whether the
    /// operation enumerated offscreen aliases, independent of host speed.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn generic_operation_cost_for_ten_thousand_history_rows() {
        struct Count(usize);
        impl Operation for Count {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn container(&mut self, _: Option<&Id>, _: Rectangle) {
                self.0 += 1;
            }
        }

        const ROWS: u64 = 10_000;
        const OPERATIONS: usize = 120;
        let renderer = crate::app::tests::headless_renderer();
        let window = Rc::new(Cell::new(Some(Viewport {
            top: 100_000.0,
            height: 600.0,
            bottom: false,
        })));
        let mut element = column(
            (0..ROWS)
                .map(|key| {
                    Row::new(key, 0, 20.0, Some(Id::from(format!("row-{key}"))), || {
                        widget::Space::new().height(20).into()
                    })
                })
                .collect(),
            window,
        );
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, f32::INFINITY));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);

        let started = std::time::Instant::now();
        let aliases = (0..OPERATIONS)
            .map(|_| {
                let mut operation = Count(0);
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut operation,
                );
                operation.0
            })
            .sum::<usize>();
        println!(
            "{ROWS} rows, {OPERATIONS} generic operations: {aliases} aliases in {:.3} ms",
            started.elapsed().as_secs_f64() * 1e3
        );
    }

    #[test]
    fn measured_height_index_matches_flat_offsets_after_sparse_corrections() {
        let mut values: Vec<_> = (0..10000).map(|at| (at % 17) as f32).collect();
        let mut index = PrefixSums::new(values.clone());
        for at in (0..values.len()).step_by(97) {
            values[at] += 19.5;
            index.set(at, values[at]);
        }
        let mut offsets = vec![0.0];
        for height in values {
            offsets.push(offsets.last().unwrap() + height);
        }
        for (at, expected) in offsets.iter().enumerate() {
            assert_eq!(index.prefix(at), *expected);
        }
        for top in [0.0, 1000.0, 10000.0, index.total()] {
            assert_eq!(
                index.partition_point(|offset| offset < top),
                offsets.partition_point(|offset| *offset < top)
            );
            assert_eq!(
                index.partition_point(|offset| offset <= top),
                offsets.partition_point(|offset| *offset <= top)
            );
        }
    }

    #[test]
    fn intrinsic_rows_keep_fill_height_activity_rails_finite_in_an_unbounded_parent() {
        fn activity() -> Element<'static, Message> {
            widget::row::with_children(vec![
                widget::container(widget::Space::new().height(Length::Fill))
                    .width(1)
                    .height(Length::Fill)
                    .into(),
                widget::column::with_children(vec![
                    widget::Space::new().height(25).width(100).into(),
                    widget::Space::new().height(40).width(100).into(),
                ])
                .into(),
            ])
            .into()
        }
        let renderer = crate::app::tests::headless_renderer();
        let limits = layout::Limits::new(Size::ZERO, Size::new(600.0, f32::INFINITY));
        let mut ordinary: Element<'_, Message> = widget::column::with_children(vec![activity()])
            .width(Length::Fill)
            .height(Length::Shrink)
            .into();
        let mut tree = Tree::new(&ordinary);
        let expected = ordinary
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(expected.size().height.is_finite());
        let mut virtualized = column(
            vec![Row::new(1, 1, 65.0, None, activity)],
            Default::default(),
        );
        let mut tree = Tree::new(&virtualized);
        let actual = virtualized
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(
            actual.size().height.is_finite(),
            "intrinsic virtual rows must not expand activity rails to infinite height"
        );
        assert_eq!(
            actual.size(),
            expected.size(),
            "same finite native intrinsic layout"
        );
    }

    #[test]
    fn logical_navigation_uses_only_descriptor_offsets_until_a_chunk_is_mounted() {
        let mut state = State {
            offsets: PrefixSums::new(vec![1000.0, 500.0]),
            ..Default::default()
        };
        state
            .logical
            .insert(42, vec![(0..100, 0, true), (100..200, 1, true)]);
        let bounds = Rectangle::new(Point::new(10.0, 20.0), Size::new(720.0, 1500.0));
        let (first, stage) = logical_bounds(&state, bounds, 42, 50).unwrap();
        assert_eq!((first.y, stage), (520.0, 2));
        assert_eq!(logical_bounds(&state, bounds, 42, 100).unwrap().0.y, 1020.0);
        assert_eq!(logical_bounds(&state, bounds, 42, 200).unwrap().0.y, 1520.0);
        assert!(logical_bounds(&state, bounds, 42, 201).is_none());
        assert!(logical_bounds(&state, bounds, 43, 50).is_none());
        assert!(
            state.mounted.is_empty(),
            "no offscreen row was instantiated"
        );
    }

    #[test]
    fn evicted_reusable_rows_release_element_owners_and_can_mount_again() {
        struct Lease {
            _owner: Arc<()>,
        }
        impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Lease {
            fn size(&self) -> Size<Length> {
                Size::new(Length::Fill, Length::Fixed(20.0))
            }
            fn layout(
                &mut self,
                _: &mut Tree,
                _: &cosmic::Renderer,
                limits: &layout::Limits,
            ) -> layout::Node {
                layout::Node::new(Size::new(limits.max().width, 20.0))
            }
            fn draw(
                &self,
                _: &Tree,
                _: &mut cosmic::Renderer,
                _: &cosmic::Theme,
                _: &renderer::Style,
                _: Layout<'_>,
                _: mouse::Cursor,
                _: &Rectangle,
            ) {
            }
        }
        let renderer = crate::app::tests::headless_renderer();
        let owners = Rc::new(std::cell::RefCell::new(Vec::new()));
        let window = Rc::new(Cell::new(Some(Viewport {
            top: 0.0,
            height: 600.0,
            bottom: false,
        })));
        let mut element = column(
            (0..10000)
                .map(|index| {
                    let owners = owners.clone();
                    Row::new(index, 0, 20.0, None, move || {
                        let owner = Arc::new(());
                        owners.borrow_mut().push(Arc::downgrade(&owner));
                        Element::new(Lease { _owner: owner })
                    })
                })
                .collect(),
            window.clone(),
        );
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, f32::INFINITY));
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let first = owners.borrow().clone();
        assert!(first.iter().all(|owner| owner.upgrade().is_some()));
        window.set(Some(Viewport {
            top: 100000.0,
            height: 600.0,
            bottom: false,
        }));
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(
            first.iter().all(|owner| owner.upgrade().is_none()),
            "eviction must drop Element as well as Tree"
        );
        window.set(Some(Viewport {
            top: 0.0,
            height: 600.0,
            bottom: false,
        }));
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(
            owners
                .borrow()
                .iter()
                .filter(|owner| owner.upgrade().is_some())
                .count()
                <= 92
        );
    }

    #[test]
    fn only_viewport_rows_are_instantiated_and_evicted_trees_are_released() {
        let renderer = crate::app::tests::headless_renderer();
        let built = Rc::new(Cell::new(0));
        let window = Rc::new(Cell::new(Some(Viewport {
            top: 40000.0,
            height: 600.0,
            bottom: false,
        })));
        let mut element = column(
            (0..10000)
                .map(|index| {
                    let built = built.clone();
                    Row::new(index, 0, 20.0, None, move || {
                        built.set(built.get() + 1);
                        widget::Space::new().height(20).width(Length::Fill).into()
                    })
                })
                .collect(),
            window.clone(),
        );
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, f32::INFINITY));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert_eq!(node.size().height, 200000.0);
        assert!(
            built.get() <= 92,
            "only viewport + overscan rows may be built"
        );
        assert!(tree.children.len() <= 92);
        let before = built.get();
        window.set(Some(Viewport {
            top: 100000.0,
            height: 600.0,
            bottom: false,
        }));
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(built.get() - before <= 92);
        assert!(
            tree.children.len() <= 92,
            "old shaping/widget state is evicted"
        );
    }
}
