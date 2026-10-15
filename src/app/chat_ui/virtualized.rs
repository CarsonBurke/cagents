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
pub(super) fn constructed_rows() -> usize { CONSTRUCTED.with(Cell::get) }

static NEXT_COLUMN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Clone, Copy)]
pub(super) struct Viewport {
    pub top: f32,
    pub height: f32,
    pub bottom: bool,
}
pub(super) type Window = Rc<Cell<Option<Viewport>>>;

/// Sparse input operations opt out of placeholder enumeration. Offscreen
/// aliases exist for explicit navigation operations, never pointer hit tests.
pub(super) struct PlaceholderPolicy { pub visit: bool }

#[derive(Clone)]
pub(super) struct LogicalRange { pub identity: u64, pub bytes: std::ops::Range<usize>, pub chunk: bool }

pub(super) struct Row<'a> {
    pub key: u64,
    pub revision: u64,
    pub estimate: f32,
    pub aliases: Vec<Id>,
    pub anchor: bool,
    pub logical: Option<LogicalRange>,
    pub descendants: Vec<LogicalRange>,
    rebuild: Option<Box<dyn Fn() -> Element<'a, Message> + 'a>>,
    build: Option<Box<dyn FnOnce() -> Element<'a, Message> + 'a>>,
    content: Option<Element<'a, Message>>,
}
impl<'a> Row<'a> {
    pub fn once(
        key: u64,
        revision: u64,
        estimate: f32,
        id: Option<Id>,
        build: impl FnOnce() -> Element<'a, Message> + 'a,
    ) -> Self {
        Self { key, revision, estimate, aliases: id.into_iter().collect(), anchor: false, logical: None, descendants: vec![], rebuild: None, build: Some(Box::new(build)), content: None }
    }
    pub fn new(key: u64, revision: u64, estimate: f32, id: Option<Id>,
        build: impl Fn() -> Element<'a, Message> + 'a) -> Self {
        Self { key, revision, estimate, aliases: id.into_iter().collect(), anchor: false,
            logical: None, descendants: vec![], rebuild: Some(Box::new(build)), build: None, content: None }
    }
    fn element(&mut self) -> &mut Element<'a, Message> {
        if self.content.is_none() {
            #[cfg(test)]
            CONSTRUCTED.with(|count| count.set(count.get() + 1));
            self.content = Some(match &self.rebuild { Some(build) => build(), None => self.build.take().expect("a static row is built once")() });
        }
        self.content.as_mut().unwrap()
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Axis { #[default] Vertical, Horizontal }
impl Axis {
    fn point(self, main: f32) -> Point { match self { Self::Vertical => Point::new(0.0, main), Self::Horizontal => Point::new(main, 0.0) } }
    fn main(self, size: Size) -> f32 { match self { Self::Vertical => size.height, Self::Horizontal => size.width } }
    fn cross(self, size: Size) -> f32 { match self { Self::Vertical => size.width, Self::Horizontal => size.height } }
    fn size(self, main: f32, cross: f32) -> Size { match self { Self::Vertical => Size::new(cross, main), Self::Horizontal => Size::new(main, cross) } }
}

pub(super) fn column<'a>(rows: Vec<Row<'a>>, window: Window) -> Element<'a, Message> {
    Element::new(Column { rows, window, revision: NEXT_COLUMN.fetch_add(1, Ordering::Relaxed), axis: Axis::Vertical, cross_estimate: 0.0 })
}

/// Horizontal virtualization preserves a physical code line/table row while
/// only instantiating cells near its horizontal native scroll viewport.
pub(super) fn row<'a>(rows: Vec<Row<'a>>, height: f32, window: Window) -> Element<'a, Message> {
    Element::new(Column { rows, window, revision: NEXT_COLUMN.fetch_add(1, Ordering::Relaxed), axis: Axis::Horizontal, cross_estimate: height })
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
        if self.revision == row.revision { self.height }
        else { (self.height + row.estimate - self.estimate).max(0.0) }
    }
}
#[derive(Default)]
struct State {
    axis: Axis,
    width: f32,
    heights: HashMap<u64, Height>,
    cross_extents: std::collections::BTreeMap<u32, usize>,
    mounted: Vec<(usize, u64)>,
    offsets: PrefixSums,
    descriptors: u64,
    observed: Option<Viewport>,
    positions: HashMap<u64, usize>,
    anchors: HashSet<u64>,
    logical: HashMap<u64, Vec<(std::ops::Range<usize>, usize, bool)>>,
    aliased: Vec<usize>,
}

/// Mutable measured heights with logarithmic corrections and prefix lookup.
/// Ordinary wheel/layout passes never rebuild an all-history offset vector.
#[derive(Default)]
struct PrefixSums { values: Vec<f32>, tree: Vec<f32> }
impl PrefixSums {
    fn new(values: Vec<f32>) -> Self {
        let mut index = Self { tree: vec![0.0; values.len() + 1], values };
        for at in 1..index.tree.len() {
            index.tree[at] += index.values[at - 1];
            let next = at + (at & at.wrapping_neg());
            if next < index.tree.len() { index.tree[next] += index.tree[at]; }
        }
        index
    }
    fn prefix(&self, mut end: usize) -> f32 {
        let mut sum = 0.0;
        while end > 0 { sum += self.tree[end]; end &= end - 1; }
        sum
    }
    fn total(&self) -> f32 { self.prefix(self.values.len()) }
    fn set(&mut self, index: usize, value: f32) {
        let delta = value - self.values[index];
        self.values[index] = value;
        let mut at = index + 1;
        while at < self.tree.len() { self.tree[at] += delta; at += at & at.wrapping_neg(); }
    }
    fn partition_point(&self, predicate: impl Fn(f32) -> bool) -> usize {
        let mut low = 0; let mut high = self.values.len() + 1;
        while low < high {
            let middle = low + (high - low) / 2;
            if predicate(self.prefix(middle)) { low = middle + 1; } else { high = middle; }
        }
        low
    }
}

fn visible(offsets: &PrefixSums, top: f32, height: f32) -> std::ops::Range<usize> {
    let count = offsets.values.len();
    let first = offsets.partition_point(|offset| offset < top - OVERSCAN).saturating_sub(1).min(count);
    let end = offsets.partition_point(|offset| offset <= top + height + OVERSCAN).min(count);
    first..end.max(first)
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Column<'_> {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<State>() }
    fn state(&self) -> tree::State { tree::State::new(State::default()) }
    fn diff(&mut self, _: &mut Tree) {}
    fn size(&self) -> Size<Length> { match self.axis { Axis::Vertical => Size::new(Length::Fill, Length::Shrink), Axis::Horizontal => Size::new(Length::Shrink, Length::Shrink) } }
    fn layout(&mut self, tree: &mut Tree, renderer: &cosmic::Renderer, limits: &layout::Limits) -> layout::Node {
        let state = tree.state.downcast_mut::<State>();
        let width = match self.axis {
            Axis::Vertical => if limits.max().width.is_finite() { limits.max().width } else { design::READABLE_MEASURE },
            Axis::Horizontal => self.cross_estimate,
        };
        let width_changed = state.width != width || state.axis != self.axis;
        state.axis = self.axis;
        if width_changed {
            state.heights.clear();
            state.width = width;
        }
        if width_changed || state.descriptors != self.revision {
        state.positions = self.rows.iter().enumerate().map(|(index, row)| (row.key, index)).collect();
        state.anchors = self.rows.iter().filter(|row| row.anchor).map(|row| row.key).collect();
        state.aliased = self.rows.iter().enumerate().filter_map(|(index, row)| (!row.aliases.is_empty()).then_some(index)).collect();
        state.logical.clear();
        for (index, row) in self.rows.iter().enumerate() {
            for logical in row.logical.iter().chain(&row.descendants) {
                state.logical.entry(logical.identity).or_default().push((logical.bytes.clone(), index, logical.chunk));
            }
        }
        // Retain only descriptors still loaded by the transcript window.
        let keys: HashSet<_> = self.rows.iter().map(|row| row.key).collect();
        state.heights.retain(|key, _| keys.contains(key));
        state.cross_extents.clear();
        for measured in state.heights.values() {
            *state.cross_extents.entry(measured.cross.to_bits()).or_default() += 1;
        }
            state.offsets = PrefixSums::new(self.rows.iter().map(|row| {
                state.heights.get(&row.key).map_or(row.estimate, |height| height.for_row(row)).max(0.0)
            }).collect());
            state.descriptors = self.revision;
        }
        let mut nodes = Vec::new();
        for _ in 0..3 {
            let total = state.offsets.total();
            let viewport = state.observed.or(self.window.get()).unwrap_or(Viewport {
                top: 0.0, height: 800.0, bottom: self.axis == Axis::Vertical,
            });
            let top = if viewport.bottom { (total - viewport.height).max(0.0) } else { viewport.top };
            let range = visible(&state.offsets, top, viewport.height);
            let mut previous: HashMap<_, _> = state.mounted.drain(..).zip(std::mem::take(&mut tree.children))
                .map(|((_, key), tree)| (key, tree)).collect();
            nodes.clear();
            let mut changed = false;
            for index in range {
                let row = &mut self.rows[index];
                let key = row.key;
                let revision = row.revision;
                let estimate = row.estimate;
                let element = row.element();
                let mut child = previous.remove(&key).unwrap_or_else(|| Tree::new(&*element));
                child.diff(element.as_widget_mut());
                let child_max = match self.axis { Axis::Vertical => Size::new(width, f32::INFINITY),
                    Axis::Horizontal => Size::new(f32::INFINITY, f32::INFINITY) };
                let mut node = element.as_widget_mut().layout(&mut child, renderer,
                    &layout::Limits::new(Size::ZERO, child_max));
                let height = self.axis.main(node.size());
                changed |= (height - state.offsets.values[index]).abs() > 0.5;
                let cross = self.axis.cross(node.size()).max(0.0);
                if let Some(previous) = state.heights.insert(key, Height { revision, height, estimate, cross }) {
                    if let Some(count) = state.cross_extents.get_mut(&previous.cross.to_bits()) {
                        *count -= 1;
                        if *count == 0 { state.cross_extents.remove(&previous.cross.to_bits()); }
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
                    if row.rebuild.is_some() { row.content = None; }
                }
            }
            if !changed { break; }
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
        let measured_cross = state.cross_extents.last_key_value().map_or(0.0, |(bits, _)| f32::from_bits(*bits));
        let cross = match self.axis {
            Axis::Vertical if limits.max().width.is_finite() => width,
            Axis::Vertical => measured_cross,
            Axis::Horizontal => measured_cross.max(width),
        };
        layout::Node::with_children(self.axis.size(total, cross), nodes)
    }
    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &cosmic::Renderer, operation: &mut dyn Operation) {
        let state = tree.state.downcast_mut::<State>();
        operation.custom(None, layout.bounds(), state);
        // Placeholder bounds make unloaded items addressable by find/history
        // anchoring without constructing their text or widget trees.
        let mut placeholders = PlaceholderPolicy { visit: true };
        operation.custom(None, layout.bounds(), &mut placeholders);
        if placeholders.visit { for &index in &state.aliased {
            let row = &self.rows[index];
            for id in &row.aliases {
                let start = state.offsets.prefix(index);
                let size = self.axis.size(state.offsets.values[index], self.axis.cross(layout.bounds().size()));
                operation.container(Some(id), Rectangle::new(layout.bounds().position() + match self.axis { Axis::Vertical => Vector::new(0.0, start), Axis::Horizontal => Vector::new(start, 0.0) }, size));
            }
        }
        }
        operation.traverse(&mut |operation| {
            for (((index, _), child), child_layout) in state.mounted.iter().zip(&mut tree.children).zip(layout.children()) {
                self.rows[*index].content.as_mut().unwrap().as_widget_mut().operate(child,
                    child_layout.with_virtual_offset(layout.virtual_offset()), renderer, operation);
            }
        });
    }
    fn update(&mut self, tree: &mut Tree, event: &Event, layout: Layout<'_>, cursor: mouse::Cursor,
        renderer: &cosmic::Renderer, clipboard: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, viewport: &Rectangle) {
        let state = tree.state.downcast_mut::<State>();
        for (((index, _), child), child_layout) in state.mounted.iter().zip(&mut tree.children).zip(layout.children()) {
            if child_layout.bounds().intersection(viewport).is_some() {
                self.rows[*index].content.as_mut().unwrap().as_widget_mut().update(child, event,
                    child_layout.with_virtual_offset(layout.virtual_offset()), cursor, renderer, clipboard, shell, viewport);
            }
        }
    }
    fn draw(&self, tree: &Tree, renderer: &mut cosmic::Renderer, theme: &cosmic::Theme, style: &renderer::Style,
        layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle) {
        let state = tree.state.downcast_ref::<State>();
        for (((index, _), child), child_layout) in state.mounted.iter().zip(&tree.children).zip(layout.children()) {
            if child_layout.bounds().intersection(viewport).is_some() {
                self.rows[*index].content.as_ref().unwrap().as_widget().draw(child, renderer, theme, style,
                    child_layout.with_virtual_offset(layout.virtual_offset()), cursor, viewport);
            }
        }
    }
    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle,
        renderer: &cosmic::Renderer) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State>();
        state.mounted.iter().zip(&tree.children).zip(layout.children()).filter_map(|(((index, _), child), child_layout)| {
            child_layout.bounds().intersection(viewport)?;
            Some(self.rows[*index].content.as_ref().unwrap().as_widget().mouse_interaction(child,
                child_layout.with_virtual_offset(layout.virtual_offset()), cursor, viewport, renderer))
        }).max().unwrap_or_default()
    }
    fn overlay<'b>(&'b mut self, tree: &'b mut Tree, layout: Layout<'b>, renderer: &cosmic::Renderer,
        viewport: &Rectangle, translation: Vector) -> Option<overlay::Element<'b, Message, cosmic::Theme, cosmic::Renderer>> {
        let state = tree.state.downcast_ref::<State>();
        let mut overlays = Vec::new();
        // Rows and trees are disjoint, in the same monotonically increasing order.
        let mut remaining = self.rows.as_mut_slice();
        let mut offset = 0;
        for (((index, _), child), child_layout) in state.mounted.iter().zip(&mut tree.children).zip(layout.children()) {
            let (_, rest) = remaining.split_at_mut(*index - offset);
            let (row, rest) = rest.split_first_mut().unwrap();
            remaining = rest;
            offset = *index + 1;
            if let Some(overlay) = row.content.as_mut().unwrap().as_widget_mut().overlay(child,
                child_layout.with_virtual_offset(layout.virtual_offset()), renderer, viewport, translation) {
                overlays.push(overlay);
            }
        }
        (!overlays.is_empty()).then(|| overlay::Group::with_children(overlays).overlay())
    }
    fn a11y_nodes(&self, layout: Layout<'_>, tree: &Tree, cursor: mouse::Cursor) -> iced_accessibility::A11yTree {
        let state = tree.state.downcast_ref::<State>();
        iced_accessibility::A11yTree::join(state.mounted.iter().zip(&tree.children).zip(layout.children()).map(|(((index, _), child), child_layout)| {
            self.rows[*index].content.as_ref().unwrap().as_widget().a11y_nodes(child_layout.with_virtual_offset(layout.virtual_offset()), child, cursor)
        }))
    }
}

/// Geometry from cheap row descriptions. Exact glyph geometry, when mounted,
/// supersedes this estimate in scroll::locate_logical. No hidden builder runs.
pub(super) fn logical_bounds(state: &dyn std::any::Any, bounds: Rectangle, identity: u64, byte: usize) -> Option<(Rectangle, u8)> {
    let state = state.downcast_ref::<State>()?;
    let ranges = state.logical.get(&identity)?;
    let at = ranges.partition_point(|(range, _, _)| range.start <= byte).saturating_sub(1);
    let (range, index, chunk) = ranges.get(at)?;
    if byte > range.end { return None; }
    let top = state.offsets.prefix(*index);
    let height = state.offsets.prefix(*index + 1) - top;
    let fraction = byte.saturating_sub(range.start) as f32 / range.len().max(1) as f32;
    let rectangle = match state.axis {
        Axis::Vertical => Rectangle { x: bounds.x, y: bounds.y + top + height * fraction, width: bounds.width, height: 1.0 },
        Axis::Horizontal => Rectangle { x: bounds.x + top + height * fraction, y: bounds.y, width: 1.0, height: bounds.height },
    };
    Some((rectangle, if *chunk { 2 } else { 1 }))
}

/// Fine-grained anchors include the visible window and previously anchored
/// rows even when corrected estimates moved them outside the current window.
/// No hidden row is realized to answer this geometry request.
pub(super) fn anchor_points(state: &mut dyn std::any::Any, bounds: Rectangle, wanted: &HashSet<u64>) -> Vec<(u64, Rectangle)> {
    let Some(state) = state.downcast_ref::<State>() else { return vec![]; };
    let mut keys: HashSet<_> = state.mounted.iter().map(|(_, key)| *key).filter(|key| state.anchors.contains(key)).collect();
    keys.extend(wanted.iter().filter(|key| state.anchors.contains(key)).copied());
    keys.into_iter().filter_map(|key| {
        let index = *state.positions.get(&key)?;
        let top = state.offsets.prefix(index);
        let rectangle = match state.axis {
            Axis::Vertical => Rectangle { x: bounds.x, y: bounds.y + top, width: bounds.width, height: state.offsets.values[index] },
            Axis::Horizontal => Rectangle { x: bounds.x + top, y: bounds.y, width: state.offsets.values[index], height: bounds.height },
        };
        Some((key, rectangle))
    }).collect()
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
            let bounds = Rectangle { x: bounds.x - self.translation.x, y: bounds.y - self.translation.y, ..bounds };
            self.clip = Some(self.clip.map_or(bounds, |clip| clip.intersection(&bounds).unwrap_or_default()));
            self.translation += translation;
        }
        visit(self);
        self.translation = previous;
        self.clip = clip;
    }
    fn scrollable(&mut self, _: Option<&Id>, bounds: Rectangle, _: Rectangle,
        translation: Vector, _: &mut dyn operation::Scrollable) {
        self.pending = Some((bounds, translation));
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(policy) = state.downcast_mut::<PlaceholderPolicy>() { policy.visit = false; return; }
        if let Some(invalidation) = state.downcast_mut::<retained::LayoutInvalidation>() {
            match invalidation {
                retained::LayoutInvalidation::Enter => self.exchanges.push(self.touches),
                retained::LayoutInvalidation::Exit { changed } => *changed = self.exchanges.pop().is_some_and(|before| before != self.touches),
            }
            return;
        }
        if let (Some(state), Some(viewport)) = (state.downcast_mut::<State>(), self.clip) {
            let (top, height) = match state.axis {
                Axis::Vertical => ((viewport.y + self.translation.y - bounds.y).max(0.0), viewport.height),
                Axis::Horizontal => ((viewport.x + self.translation.x - bounds.x).max(0.0), viewport.width),
            };
            let wanted = visible(&state.offsets, top, height);
            let mounted = state.mounted.first().map_or(0, |(index, _)| *index)
                ..state.mounted.last().map_or(0, |(index, _)| *index + 1);
            if wanted != mounted { self.changed = true; self.touches += 1; }
            state.observed = Some(Viewport { top, height, bottom: false });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn measured_height_index_matches_flat_offsets_after_sparse_corrections() {
        let mut values: Vec<_> = (0..10000).map(|at| (at % 17) as f32).collect();
        let mut index = PrefixSums::new(values.clone());
        for at in (0..values.len()).step_by(97) {
            values[at] += 19.5;
            index.set(at, values[at]);
        }
        let mut offsets = vec![0.0];
        for height in values { offsets.push(offsets.last().unwrap() + height); }
        for (at, expected) in offsets.iter().enumerate() { assert_eq!(index.prefix(at), *expected); }
        for top in [0.0, 1000.0, 10000.0, index.total()] {
            assert_eq!(index.partition_point(|offset| offset < top), offsets.partition_point(|offset| *offset < top));
            assert_eq!(index.partition_point(|offset| offset <= top), offsets.partition_point(|offset| *offset <= top));
        }
    }

    #[test]
    fn logical_navigation_uses_only_descriptor_offsets_until_a_chunk_is_mounted() {
        let mut state = State { offsets: PrefixSums::new(vec![1000.0, 500.0]), ..Default::default() };
        state.logical.insert(42, vec![(0..100, 0, true), (100..200, 1, true)]);
        let bounds = Rectangle::new(Point::new(10.0, 20.0), Size::new(720.0, 1500.0));
        let (first, stage) = logical_bounds(&state, bounds, 42, 50).unwrap();
        assert_eq!((first.y, stage), (520.0, 2));
        assert_eq!(logical_bounds(&state, bounds, 42, 100).unwrap().0.y, 1020.0);
        assert_eq!(logical_bounds(&state, bounds, 42, 200).unwrap().0.y, 1520.0);
        assert!(logical_bounds(&state, bounds, 42, 201).is_none());
        assert!(logical_bounds(&state, bounds, 43, 50).is_none());
        assert!(state.mounted.is_empty(), "no offscreen row was instantiated");
    }

    #[test]
    fn evicted_reusable_rows_release_element_owners_and_can_mount_again() {
        struct Lease(Arc<()>);
        impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Lease {
            fn size(&self) -> Size<Length> { Size::new(Length::Fill, Length::Fixed(20.0)) }
            fn layout(&mut self, _: &mut Tree, _: &cosmic::Renderer, limits: &layout::Limits) -> layout::Node {
                layout::Node::new(Size::new(limits.max().width, 20.0))
            }
            fn draw(&self, _: &Tree, _: &mut cosmic::Renderer, _: &cosmic::Theme,
                _: &renderer::Style, _: Layout<'_>, _: mouse::Cursor, _: &Rectangle) {}
        }
        let renderer = crate::app::tests::headless_renderer();
        let owners = Rc::new(std::cell::RefCell::new(Vec::new()));
        let window = Rc::new(Cell::new(Some(Viewport { top: 0.0, height: 600.0, bottom: false })));
        let mut element = column((0..10000).map(|index| {
            let owners = owners.clone();
            Row::new(index, 0, 20.0, None, move || {
                let owner = Arc::new(());
                owners.borrow_mut().push(Arc::downgrade(&owner));
                Element::new(Lease(owner))
            })
        }).collect(), window.clone());
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, f32::INFINITY));
        element.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let first = owners.borrow().clone();
        assert!(first.iter().all(|owner| owner.upgrade().is_some()));
        window.set(Some(Viewport { top: 100000.0, height: 600.0, bottom: false }));
        element.as_widget_mut().layout(&mut tree, &renderer, &limits);
        assert!(first.iter().all(|owner| owner.upgrade().is_none()), "eviction must drop Element as well as Tree");
        window.set(Some(Viewport { top: 0.0, height: 600.0, bottom: false }));
        element.as_widget_mut().layout(&mut tree, &renderer, &limits);
        assert!(owners.borrow().iter().filter(|owner| owner.upgrade().is_some()).count() <= 92);
    }

    #[test]
    fn only_viewport_rows_are_instantiated_and_evicted_trees_are_released() {
        let renderer = crate::app::tests::headless_renderer();
        let built = Rc::new(Cell::new(0));
        let window = Rc::new(Cell::new(Some(Viewport { top: 40000.0, height: 600.0, bottom: false })));
        let mut element = column((0..10000).map(|index| {
            let built = built.clone();
            Row::new(index, 0, 20.0, None, move || {
                built.set(built.get() + 1);
                widget::Space::new().height(20).width(Length::Fill).into()
            })
        }).collect(), window.clone());
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, f32::INFINITY));
        let node = element.as_widget_mut().layout(&mut tree, &renderer, &limits);
        assert_eq!(node.size().height, 200000.0);
        assert!(built.get() <= 92, "only viewport + overscan rows may be built");
        assert!(tree.children.len() <= 92);
        let before = built.get();
        window.set(Some(Viewport { top: 100000.0, height: 600.0, bottom: false }));
        element.as_widget_mut().layout(&mut tree, &renderer, &limits);
        assert!(built.get() - before <= 92);
        assert!(tree.children.len() <= 92, "old shaping/widget state is evicted");
    }
}
