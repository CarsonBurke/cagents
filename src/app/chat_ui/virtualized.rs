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

pub(super) struct Row<'a> {
    pub key: u64,
    pub revision: u64,
    pub estimate: f32,
    pub aliases: Vec<Id>,
    pub anchor: bool,
    build: Option<Box<dyn FnOnce() -> Element<'a, Message> + 'a>>,
    content: Option<Element<'a, Message>>,
}
impl<'a> Row<'a> {
    pub fn new(
        key: u64,
        revision: u64,
        estimate: f32,
        id: Option<Id>,
        build: impl FnOnce() -> Element<'a, Message> + 'a,
    ) -> Self {
        Self { key, revision, estimate, aliases: id.into_iter().collect(), anchor: false, build: Some(Box::new(build)), content: None }
    }
    fn element(&mut self) -> &mut Element<'a, Message> {
        if self.content.is_none() {
            self.content = Some(self.build.take().expect("a row is built once")());
        }
        self.content.as_mut().unwrap()
    }
}

pub(super) fn column<'a>(rows: Vec<Row<'a>>, window: Window) -> Element<'a, Message> {
    Element::new(Column { rows, window })
}

struct Column<'a> {
    rows: Vec<Row<'a>>,
    window: Window,
}
#[derive(Clone, Copy)]
struct Height {
    revision: u64,
    height: f32,
    estimate: f32,
}
impl Height {
    fn for_row(&self, row: &Row<'_>) -> f32 {
        if self.revision == row.revision { self.height }
        else { (self.height + row.estimate - self.estimate).max(0.0) }
    }
}
#[derive(Default)]
struct State {
    width: f32,
    heights: HashMap<u64, Height>,
    mounted: Vec<(usize, u64)>,
    offsets: Vec<f32>,
    observed: Option<Viewport>,
    positions: HashMap<u64, usize>,
    anchors: HashSet<u64>,
}

fn visible(offsets: &[f32], top: f32, height: f32) -> std::ops::Range<usize> {
    let count = offsets.len().saturating_sub(1);
    let first = offsets.partition_point(|offset| *offset < top - OVERSCAN).saturating_sub(1).min(count);
    let end = offsets.partition_point(|offset| *offset <= top + height + OVERSCAN).min(count);
    first..end.max(first)
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Column<'_> {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<State>() }
    fn state(&self) -> tree::State { tree::State::new(State::default()) }
    fn diff(&mut self, _: &mut Tree) {}
    fn size(&self) -> Size<Length> { Size::new(Length::Fill, Length::Shrink) }
    fn layout(&mut self, tree: &mut Tree, renderer: &cosmic::Renderer, limits: &layout::Limits) -> layout::Node {
        let state = tree.state.downcast_mut::<State>();
        let width = if limits.max().width.is_finite() { limits.max().width } else { design::READABLE_MEASURE };
        if state.width != width {
            state.heights.clear();
            state.width = width;
        }
        state.positions = self.rows.iter().enumerate().map(|(index, row)| (row.key, index)).collect();
        state.anchors = self.rows.iter().filter(|row| row.anchor).map(|row| row.key).collect();
        // Retain only descriptors still loaded by the transcript window.
        let keys: HashSet<_> = self.rows.iter().map(|row| row.key).collect();
        state.heights.retain(|key, _| keys.contains(key));
        let mut nodes = Vec::new();
        for _ in 0..3 {
            state.offsets.clear();
            state.offsets.push(0.0);
            for row in &self.rows {
                let height = state.heights.get(&row.key)
                    .map_or(row.estimate, |height| height.for_row(row)).max(0.0);
                state.offsets.push(state.offsets.last().copied().unwrap() + height);
            }
            let total = state.offsets.last().copied().unwrap_or_default();
            let viewport = state.observed.or(self.window.get()).unwrap_or(Viewport {
                top: 0.0, height: 800.0, bottom: true,
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
                let mut node = element.as_widget_mut().layout(&mut child, renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(width, f32::INFINITY)));
                let height = node.size().height;
                changed |= (height - (state.offsets[index + 1] - state.offsets[index])).abs() > 0.5;
                state.heights.insert(key, Height { revision, height, estimate });
                node.move_to_mut(Point::new(0.0, state.offsets[index]));
                nodes.push(node);
                tree.children.push(child);
                state.mounted.push((index, key));
            }
            if !changed { break; }
        }
        // Update positions after measurements, retaining the viewport's anchor
        // through the outer transcript's layout-time anchoring operation.
        let mut total = 0.0;
        for (index, row) in self.rows.iter().enumerate() {
            state.offsets[index] = total;
            total += state.heights.get(&row.key)
                .map_or(row.estimate, |height| height.for_row(row)).max(0.0);
        }
        if let Some(last) = state.offsets.last_mut() { *last = total; }
        for ((index, _), node) in state.mounted.iter().zip(&mut nodes) {
            node.move_to_mut(Point::new(0.0, state.offsets[*index]));
        }
        layout::Node::with_children(Size::new(width, total), nodes)
    }
    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &cosmic::Renderer, operation: &mut dyn Operation) {
        let state = tree.state.downcast_mut::<State>();
        operation.custom(None, layout.bounds(), state);
        // Placeholder bounds make unloaded items addressable by find/history
        // anchoring without constructing their text or widget trees.
        let mut placeholders = PlaceholderPolicy { visit: true };
        operation.custom(None, layout.bounds(), &mut placeholders);
        if placeholders.visit { for (index, row) in self.rows.iter().enumerate() {
            for id in &row.aliases {
                operation.container(Some(id), Rectangle {
                    x: layout.bounds().x, y: layout.bounds().y + state.offsets[index],
                    width: layout.bounds().width, height: state.offsets[index + 1] - state.offsets[index],
                });
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

/// Fine-grained anchors include the visible window and previously anchored
/// rows even when corrected estimates moved them outside the current window.
/// No hidden row is realized to answer this geometry request.
pub(super) fn anchor_points(state: &mut dyn std::any::Any, bounds: Rectangle, wanted: &HashSet<u64>) -> Vec<(u64, Rectangle)> {
    let Some(state) = state.downcast_ref::<State>() else { return vec![]; };
    let mut keys: HashSet<_> = state.mounted.iter().map(|(_, key)| *key).filter(|key| state.anchors.contains(key)).collect();
    keys.extend(wanted.iter().filter(|key| state.anchors.contains(key)).copied());
    keys.into_iter().filter_map(|key| {
        let index = *state.positions.get(&key)?;
        Some((key, Rectangle { x: bounds.x, y: bounds.y + state.offsets[index], width: bounds.width,
            height: state.offsets[index + 1] - state.offsets[index] }))
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
        if let (Some(state), Some(viewport)) = (state.downcast_mut::<State>(), self.clip) {
            let top = (viewport.y + self.translation.y - bounds.y).max(0.0);
            let wanted = visible(&state.offsets, top, viewport.height);
            let mounted = state.mounted.first().map_or(0, |(index, _)| *index)
                ..state.mounted.last().map_or(0, |(index, _)| *index + 1);
            self.changed |= wanted != mounted;
            state.observed = Some(Viewport { top, height: viewport.height, bottom: false });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

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
