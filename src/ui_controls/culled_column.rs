//! A vertical list that builds only the rows near its scroll viewport.
//!
//! A long list inside a [`scrollable`](cosmic::iced::widget::scrollable) pays
//! to build, lay out and diff every row on each rebuild, though only a
//! screenful is ever seen. [`culled_column`] takes a unique key per row and a
//! builder instead of the rows themselves. During layout it builds just the
//! rows that lie within an overscan of the last viewport it was drawn in, plus
//! the row holding keyboard focus and any rows the caller keeps built, and
//! stands in for the rest with their height.
//!
//! - Heights are measured as rows are laid out and kept per key. A row not
//!   yet measured at the current width counts as the mean of those that have
//!   been, or as `estimate` before any has. The mean is taken again only once
//!   it has drifted by more than [`DRIFT`], so rows of similar height keep the
//!   column's height, and its scrollbar, steady from the first layout, while
//!   rows that all grow, as when pull requests arrive, are soon accounted.
//! - Measuring a row can still move every row below it. So that nothing on
//!   screen moves, the column notes how far the row at the top of the
//!   viewport moved, after a new window, a width change or rows added above,
//!   and [`keep_in_place`] around the scrollable scrolls by as much before
//!   the next frame is drawn. Wrap every scrollable that holds a culled column
//!   with it.
//! - Row widget state is kept per key, as in a keyed list, so hover, focus
//!   and open tooltips stay with their row when the mounted window moves.
//! - When the viewport moves past what is mounted, the column asks for a new
//!   layout itself; no message reaches the application.
//! - The row with keyboard focus stays mounted, and so do the rows beside it,
//!   so moving focus row by row never runs off the end of what is built.
//!   Keyboard traversal that enters the column from outside reaches the first
//!   or last built row, not the first or last row.
//! - Rows are laid out at the column's width with unbounded height, so a row
//!   must not fill its height.
//! - Keys must be unique. Rows that repeat a key share one retained state
//!   between them, so each loses hover, focus and tooltip state on every
//!   layout.
//!
//! Rows outside the window have no widgets, so they are absent from the
//! accessibility tree and from pointer and keyboard operations until they come
//! near the viewport.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Id, Operation, Tree, operation, operation::scrollable::AbsoluteOffset, tree},
};
use cosmic::iced::{Element, Event, Length, Rectangle, Size, Vector};
use cosmic::{Renderer, Theme};

/// How far beyond the viewport, above and below, rows are kept built.
const OVERSCAN: f32 = 480.0;
/// The viewport assumed before the column has been drawn once.
const FIRST_VIEWPORT: f32 = 1200.0;
/// How far the mean measured height may drift, as a fraction, before rows
/// not yet measured are assumed to be the new mean.
const DRIFT: f32 = 0.02;
/// Rows kept built on each side of the focused row, so focus traversal always
/// finds its next stop mounted.
const FOCUS_NEIGHBOURS: usize = 2;

/// A culled vertical list of `keys.len()` rows; `build(index)` makes the row
/// at `index`, and is called only for rows near the viewport.
pub fn culled_column<'a, Message: 'a>(
    keys: Vec<u64>,
    build: impl Fn(usize) -> Element<'a, Message, Theme, Renderer> + 'a,
) -> CulledColumn<'a, Message> {
    CulledColumn {
        keys,
        build: Box::new(build),
        spacing: 0.0,
        estimate: 48.0,
        kept: Vec::new(),
        mounted: Vec::new(),
    }
}

pub struct CulledColumn<'a, Message> {
    keys: Vec<u64>,
    build: Box<dyn Fn(usize) -> Element<'a, Message, Theme, Renderer> + 'a>,
    spacing: f32,
    estimate: f32,
    /// Keys whose rows stay built wherever the viewport is.
    kept: Vec<u64>,
    /// The rows built in the last layout, by index, in order.
    mounted: Vec<(usize, Element<'a, Message, Theme, Renderer>)>,
}

impl<Message> CulledColumn<'_, Message> {
    /// The gap between rows.
    pub fn spacing(mut self, spacing: f32) -> Self {
        self.spacing = spacing;
        self
    }

    /// The height assumed for every row until one has been measured.
    pub fn estimate(mut self, estimate: f32) -> Self {
        self.estimate = estimate;
        self
    }

    /// Keeps the rows with these keys built wherever the viewport is, as for
    /// a row whose menu is open, so its overlay and state outlive scrolling.
    pub fn keep_built(mut self, keys: impl IntoIterator<Item = u64>) -> Self {
        self.kept.extend(keys);
        self
    }
}

#[derive(Default)]
struct State {
    /// Measured row heights, by key, at `width`.
    heights: HashMap<u64, f32>,
    /// The sum of `heights`, for their mean.
    measured: f32,
    /// The height assumed for rows not yet measured.
    assumed: Option<f32>,
    width: f32,
    /// The gap between rows, as the last layout placed them.
    spacing: f32,
    /// The keys whose trees are `tree.children`, in order.
    mounted: Vec<u64>,
    /// Where the last layout put each mounted row: its top and bottom.
    placed: Vec<(f32, f32)>,
    /// The row the user is reading, by key, and how far the viewport's top
    /// is below that row's top. It changes only when the viewport moves for
    /// another reason than following it, such as the wheel or the thumb, so
    /// a filter that hides the row and is then cleared returns to it.
    reading: std::cell::Cell<Option<(u64, f32)>>,
    /// Where, from the column's top, the viewport's top must be for the row
    /// being read to stay where it was drawn, when a layout since
    /// [`keep_in_place`] last scrolled has moved that row.
    follow: Option<f32>,
    /// The span of content, from the column's top, that the mounted rows
    /// cover; a viewport inside it needs no new layout.
    covered: Range<f32>,
    /// The visible span of the column, from its top, as last drawn.
    viewport: std::cell::Cell<Option<(f32, f32)>>,
    /// The height of the scrollable's viewport, which the visible span is
    /// shorter than when the column is.
    screen: std::cell::Cell<f32>,
    /// The key whose widget holds keyboard focus, as operations last found.
    focused: Option<u64>,
    /// The focused key the last layout built around.
    laid_out_focus: Option<u64>,
}

impl State {
    /// Notes the visible span, from the column's top, of a column `height`
    /// tall, and what the user is now reading if they moved the viewport.
    fn observe(&self, span: (f32, f32), height: f32, screen: f32) {
        let (top, shown) = span;
        self.screen.set(screen);
        let last = self.viewport.replace(Some(span));
        // The scrollable draws at a whole-pixel offset, so a view that
        // followed its target is within half a pixel of it.
        let moved = last.is_none_or(|(was, _)| (was - top).abs() > 0.5);
        // Content that shrank under the viewport, as a filter makes it, pulls
        // the viewport up to its end; the user did not move.
        let clamped = last.is_some_and(|(was, _)| was > top) && top + shown >= height - 0.5;
        if moved && !clamped {
            // At the very top, rows added above should show.
            self.reading
                .set((top > 0.0).then(|| self.row_at(top)).flatten());
        }
    }

    /// The row `top` is in, or the one after the gap it is in, and how far
    /// `top` is below that row's top. None where the last layout built no
    /// row, as when the viewport has jumped; the next layout finds it.
    fn row_at(&self, top: f32) -> Option<(u64, f32)> {
        self.mounted
            .iter()
            .zip(&self.placed)
            .find(|(_, (_, bottom))| *bottom > top)
            .filter(|(_, (placed, _))| *placed <= top + self.spacing)
            .map(|(key, (placed, _))| (*key, top - placed))
    }

    fn height(&self, key: u64, estimate: f32) -> f32 {
        self.heights.get(&key).copied().unwrap_or(estimate)
    }

    /// The height to assume for an unmeasured row.
    fn unmeasured(&self, estimate: f32) -> f32 {
        self.assumed.unwrap_or(estimate)
    }

    fn measure(&mut self, key: u64, height: f32) {
        self.measured += height - self.heights.insert(key, height).unwrap_or(0.0);
    }

    /// Assumes the mean measured height for unmeasured rows, once it has
    /// drifted far enough from what they are assumed to be.
    fn reassume(&mut self) {
        if self.heights.is_empty() {
            return;
        }
        let mean = self.measured / self.heights.len() as f32;
        if self
            .assumed
            .is_none_or(|assumed| (mean - assumed).abs() > DRIFT * assumed)
        {
            self.assumed = Some(mean);
        }
    }

    fn forget_heights(&mut self) {
        self.heights.clear();
        self.measured = 0.0;
        self.assumed = None;
    }

    /// Drops the heights of rows that are gone, once they outnumber the rows
    /// there are, so a list whose rows keep changing doesn't grow forever.
    fn prune(&mut self, keys: &[u64]) {
        if self.heights.len() > 2 * keys.len() + 64 {
            let live: HashSet<u64> = keys.iter().copied().collect();
            self.heights.retain(|key, _| live.contains(key));
            self.measured = self.heights.values().sum();
        }
    }
}

/// The visible part of `bounds` within `viewport`, from the top of `bounds`.
fn visible_span(bounds: Rectangle, viewport: &Rectangle) -> Option<(f32, f32)> {
    let top = (viewport.y - bounds.y).max(0.0);
    let bottom = (viewport.y + viewport.height - bounds.y).min(bounds.height);
    (bottom > top || bounds.height == 0.0).then_some((top, (bottom - top).max(0.0)))
}

/// The span of content that shows `viewport` with `overscan` either side.
fn wanted(viewport: (f32, f32), overscan: f32) -> Range<f32> {
    let (top, height) = viewport;
    (top - overscan).max(0.0)..top + height + overscan
}

impl<Message> CulledColumn<'_, Message> {
    /// Every row's top from the measured or estimated heights, with the
    /// column's end last.
    fn tops(&self, state: &State) -> Vec<f32> {
        let mut tops = Vec::with_capacity(self.keys.len() + 1);
        let unmeasured = state.unmeasured(self.estimate);
        let mut y = 0.0;
        for key in &self.keys {
            tops.push(y);
            y += state.height(*key, unmeasured) + self.spacing;
        }
        tops.push(y);
        tops
    }

    /// Which rows to build: the contiguous run within the overscanned
    /// `viewport`, then the focused row and its neighbours and the kept rows,
    /// sorted.
    fn window(
        &self,
        state: &State,
        tops: &[f32],
        viewport: Option<(f32, f32)>,
    ) -> (Range<usize>, Vec<usize>) {
        let rows = &tops[..self.keys.len()];
        let span = wanted(viewport.unwrap_or((0.0, FIRST_VIEWPORT)), OVERSCAN);
        // The row whose top is above the span may still reach into it.
        let first = rows
            .partition_point(|top| *top <= span.start)
            .saturating_sub(1);
        let last = rows.partition_point(|top| *top < span.end).max(first);
        let mut built: Vec<usize> = (first..last).collect();
        if let Some(focused) = state.focused
            && let Some(index) = self.keys.iter().position(|key| *key == focused)
        {
            built.extend(
                index.saturating_sub(FOCUS_NEIGHBOURS)
                    ..(index + FOCUS_NEIGHBOURS + 1).min(self.keys.len()),
            );
        }
        if !self.kept.is_empty() {
            built.extend(
                self.keys
                    .iter()
                    .enumerate()
                    .filter(|(_, key)| self.kept.contains(key))
                    .map(|(index, _)| index),
            );
        }
        built.sort_unstable();
        built.dedup();
        (first..last, built)
    }

    fn children<'b>(
        &'b self,
        tree: &'b Tree,
        layout: Layout<'b>,
    ) -> impl Iterator<
        Item = (
            &'b Element<'b, Message, Theme, Renderer>,
            &'b Tree,
            Layout<'b>,
        ),
    >
    where
        Self: 'b,
    {
        let offset = layout.virtual_offset();
        self.mounted
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(move |(((_, row), tree), layout)| (row, tree, layout.with_virtual_offset(offset)))
    }
}

/// Finds the id of the focused widget, if any.
#[derive(Default)]
struct FocusedId(Option<Id>);
impl Operation for FocusedId {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn focusable(&mut self, id: Option<&Id>, _: Rectangle, state: &mut dyn operation::Focusable) {
        if state.is_focused() {
            self.0 = id.cloned();
        }
    }
}

impl<Message> Widget<Message, Theme, Renderer> for CulledColumn<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        Vec::new()
    }

    fn diff(&mut self, _tree: &mut Tree) {
        // Rows are built, and their trees matched by key, during layout.
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let width = limits.max().width;
        let state = tree.state.downcast_mut::<State>();
        // The row the user is reading stays put however the rows above it
        // change. While a filter hides it, nothing is followed and the
        // scrollable keeps its offset, as it would without culling.
        let anchor = state.reading.get().and_then(|(key, offset)| {
            let index = self.keys.iter().position(|row| *row == key)?;
            Some((index, offset))
        });
        // No further than the view can scroll: a row being read with too
        // little after it to reach the top stays as far up as it can, and
        // asks for nothing more until the user moves.
        let screen = state.screen.get();
        let target = |tops: &[f32]| {
            let end = (tops[tops.len() - 1] - self.spacing - screen).max(0.0);
            anchor.map(|(index, offset)| (tops[index] + offset).min(end))
        };
        if (state.width - width).abs() > 0.5 {
            state.forget_heights();
            state.width = width;
        }
        state.prune(&self.keys);
        let tops = self.tops(state);
        // Choose the window around where the viewport will be once it has
        // followed the row being read.
        let ahead = target(&tops);
        let viewport = state
            .viewport
            .get()
            .map(|(top, height)| (ahead.unwrap_or(top), height));
        let (run, rows) = self.window(state, &tops, viewport);

        // Match each row's retained tree by key, as a keyed list does.
        let mut retained: HashMap<u64, Tree> = std::mem::take(&mut state.mounted)
            .into_iter()
            .zip(std::mem::take(&mut tree.children))
            .collect();
        self.mounted = rows
            .iter()
            .map(|index| (*index, (self.build)(*index)))
            .collect();
        tree.children = self
            .mounted
            .iter_mut()
            .map(|(index, row)| {
                let mut child = retained
                    .remove(&self.keys[*index])
                    .unwrap_or_else(|| Tree::new(row.as_widget()));
                child.diff(row.as_widget_mut());
                child
            })
            .collect();
        // Rows that left the window drop their widget state here.
        drop(retained);

        let row_limits =
            layout::Limits::new(Size::new(width, 0.0), Size::new(width, f32::INFINITY));
        let nodes: Vec<layout::Node> = self
            .mounted
            .iter_mut()
            .zip(&mut tree.children)
            .map(|((_, row), child)| row.as_widget_mut().layout(child, renderer, &row_limits))
            .collect();
        let state = tree.state.downcast_mut::<State>();
        state.mounted = rows.iter().map(|index| self.keys[*index]).collect();
        state.laid_out_focus = state.focused;
        for (index, node) in rows.iter().zip(&nodes) {
            state.measure(self.keys[*index], node.size().height);
        }
        state.reassume();
        // Place rows by the measured heights, which may differ from the
        // estimates the window was chosen by.
        let tops = self.tops(state);
        // A target rather than a distance: the scrollable may have clamped
        // its offset to content that shrank, as under a filter, and a
        // distance from there would land elsewhere.
        if let (Some(top), Some((was, height))) = (target(&tops), state.viewport.get())
            && (top - was).abs() > 0.5
        {
            state.viewport.set(Some((top, height)));
            state.follow = Some(top);
        }
        state.placed = rows
            .iter()
            .zip(&nodes)
            .map(|(index, node)| (tops[*index], tops[*index] + node.size().height))
            .collect();
        state.spacing = self.spacing;
        // A viewport that moved past the rows built could not say which row
        // it shows until now.
        if state.reading.get().is_none()
            && let Some((top, _)) = state.viewport.get().filter(|(top, _)| *top > 0.0)
        {
            state.reading.set(state.row_at(top));
        }
        let children = rows
            .iter()
            .zip(nodes)
            .map(|(index, node)| node.move_to((0.0, tops[*index])))
            .collect();
        // What the contiguous run shows without another layout; the ends
        // of the list are covered however far they scroll.
        state.covered = if run.start == 0 {
            f32::NEG_INFINITY
        } else {
            tops[run.start]
        }..if run.end == self.keys.len() {
            f32::INFINITY
        } else {
            tops[run.end] - self.spacing
        };
        let total = (tops[self.keys.len()] - self.spacing).max(0.0);
        layout::Node::with_children(
            limits.resolve(Length::Fill, Length::Shrink, Size::new(width, total)),
            children,
        )
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let offset = layout.virtual_offset();
        for (((_, row), child), child_layout) in self
            .mounted
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            row.as_widget_mut().update(
                child,
                event,
                child_layout.with_virtual_offset(offset),
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
        // Lay out again before the viewport outruns what is built, while
        // half the overscan is still in hand.
        let state = tree.state.downcast_ref::<State>();
        if let Some(span) = visible_span(layout.bounds(), viewport) {
            state.observe(span, layout.bounds().height, viewport.height);
            let needs = wanted(span, OVERSCAN / 2.0);
            // Focus that moved to another row needs its neighbours built.
            if needs.start < state.covered.start
                || needs.end > state.covered.end
                || state.focused != state.laid_out_focus
            {
                shell.invalidate_layout();
                shell.request_redraw();
            }
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State>();
        // A frame drawn before keep_in_place has followed the row being read
        // shows where the scrollable still is; what is read is unchanged.
        if let Some(span) = visible_span(layout.bounds(), viewport)
            && state.follow.is_none()
        {
            state.observe(span, layout.bounds().height, viewport.height);
        }
        for (row, child, child_layout) in self.children(tree, layout) {
            if child_layout.bounds().intersects(viewport) {
                row.as_widget().draw(
                    child,
                    renderer,
                    theme,
                    style,
                    child_layout,
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
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.children(tree, layout)
            .map(|(row, child, child_layout)| {
                row.as_widget()
                    .mouse_interaction(child, child_layout, cursor, viewport, renderer)
            })
            .max()
            .unwrap_or_default()
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        // Lets [`keep_in_place`] find the column without visiting its rows.
        operation.custom(None, layout.bounds(), tree.state.downcast_mut::<State>());
        let offset = layout.virtual_offset();
        // An operation may skip this column's rows, as a search that has
        // already found its widget does; then nothing was learnt of focus.
        let mut focused = None;
        let mut visited = false;
        operation.traverse(&mut |operation| {
            visited = true;
            for (((index, row), child), child_layout) in self
                .mounted
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                row.as_widget_mut().operate(
                    child,
                    child_layout.with_virtual_offset(offset),
                    renderer,
                    operation,
                );
                let mut probe = FocusedId::default();
                row.as_widget_mut().operate(
                    child,
                    child_layout.with_virtual_offset(offset),
                    renderer,
                    &mut probe,
                );
                if probe.0.is_some() {
                    focused = Some(self.keys[*index]);
                }
            }
        });
        if visited {
            tree.state.downcast_mut::<State>().focused = focused;
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let offset = layout.virtual_offset();
        let children = self
            .mounted
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
            .filter_map(|(((_, row), child), child_layout)| {
                row.as_widget_mut().overlay(
                    child,
                    child_layout.with_virtual_offset(offset),
                    renderer,
                    viewport,
                    translation,
                )
            })
            .collect::<Vec<_>>();
        (!children.is_empty()).then(|| overlay::Group::with_children(children).overlay())
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        iced_accessibility::A11yTree::join(self.children(tree, layout).map(
            |(row, child, child_layout)| row.as_widget().a11y_nodes(child_layout, child, cursor),
        ))
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        for (row, child, child_layout) in self.children(tree, layout) {
            row.as_widget()
                .drag_destinations(child, child_layout, renderer, destinations);
        }
    }
}

impl<'a, Message: 'a> From<CulledColumn<'a, Message>> for Element<'a, Message, Theme, Renderer> {
    fn from(column: CulledColumn<'a, Message>) -> Self {
        Element::new(column)
    }
}

/// Wraps the scrollable around a [`culled_column`], and scrolls it to follow
/// the row at the top of its viewport whenever a layout moves that row, so
/// rows measured or added above it never move what is on screen.
///
/// While the scrollbar's thumb is dragged, the pointer decides the position,
/// so the scrollable is left where it is and the row is not followed. It
/// follows the first culled column inside the scrollable only.
pub fn keep_in_place<'a, Message: 'a>(
    scrollable: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> Element<'a, Message, Theme, Renderer> {
    Element::new(KeepInPlace {
        content: scrollable.into(),
    })
}

struct KeepInPlace<'a, Message> {
    content: Element<'a, Message, Theme, Renderer>,
}

/// Whether the left button went down on the scrollbar's rail, along the
/// scrollable's right edge, and is still held.
#[derive(Default)]
struct Held(bool);

/// Takes where the first culled column wants its viewport, without visiting
/// rows: the target from the column's top, and the column's own top.
#[derive(Default)]
struct TakeFollow {
    found: bool,
    follow: Option<(f32, f32)>,
}
impl Operation for TakeFollow {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        if !self.found {
            operate(self);
        }
    }
    fn custom(&mut self, _id: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if !self.found
            && let Some(state) = state.downcast_mut::<State>()
        {
            self.found = true;
            self.follow = state.follow.take().map(|top| (top, bounds.y));
        }
    }
}

/// Scrolls the outermost scrollable, and nothing inside it, so that the
/// column whose top is at `.1` shows its content from `.0` at the top.
struct ScrollTo(Option<(f32, f32)>);
impl Operation for ScrollTo {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        if self.0.is_some() {
            operate(self);
        }
    }
    fn scrollable(
        &mut self,
        _id: Option<&Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        _translation: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        if let Some((top, column)) = self.0.take() {
            let end = (content_bounds.height - bounds.height).max(0.0);
            let y = (column - content_bounds.y + top).clamp(0.0, end);
            state.scroll_to(AbsoluteOffset {
                x: None,
                y: Some(y),
            });
        }
    }
}

impl<Message> Widget<Message, Theme, Renderer> for KeepInPlace<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Held>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Held::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn id(&self) -> Option<Id> {
        self.content.as_widget().id()
    }

    fn set_id(&mut self, id: Id) {
        self.content.as_widget_mut().set_id(id);
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let held = &mut tree.state.downcast_mut::<Held>().0;
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let bounds = layout.bounds();
                let rail = Rectangle {
                    x: bounds.x + bounds.width - crate::design::SCROLLBAR_GUTTER,
                    width: crate::design::SCROLLBAR_GUTTER,
                    ..bounds
                };
                *held = cursor.is_over(rail);
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            | Event::Window(cosmic::iced::window::Event::Unfocused) => {
                *held = false;
            }
            _ => {}
        }
        let held = *held;
        // Follow the anchor before the event, so the column under it judges
        // the viewport where it will be drawn.
        let mut take = TakeFollow::default();
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, &mut take);
        if let Some(follow) = take.follow.filter(|_| !held) {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut ScrollTo(Some(follow)),
            );
            shell.request_redraw();
        }
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
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
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
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }

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

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            layout,
            renderer,
            destinations,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use cosmic::iced::advanced::widget::operation::{Outcome, focusable};
    use cosmic::iced::advanced::{clipboard, widget};
    use cosmic::iced::{Point, window};
    use cosmic::widget as cosmic_widget;

    use super::*;

    const ROW: f32 = 40.0;
    const SPACING: f32 = 4.0;
    const SCREEN: Size = Size::new(300.0, 400.0);

    /// A column of `rows` focusable rows of height `ROW`, logging which rows
    /// were built.
    fn column<'a>(rows: usize, built: Rc<RefCell<Vec<usize>>>) -> cosmic::Element<'a, ()> {
        column_of(rows, built, |_| ROW)
    }

    /// A column like [`column`] whose row at `index` is `height(index)` tall,
    /// though the column still assumes `ROW` before measuring any.
    fn column_of<'a>(
        rows: usize,
        built: Rc<RefCell<Vec<usize>>>,
        height: fn(usize) -> f32,
    ) -> cosmic::Element<'a, ()> {
        keyed(
            (0..rows as u64).collect(),
            built,
            height,
            std::iter::empty(),
        )
    }

    /// A column whose rows have `keys`, named and sized by their key, with
    /// the `kept` keys kept built.
    fn keyed<'a>(
        keys: Vec<u64>,
        built: Rc<RefCell<Vec<usize>>>,
        height: fn(usize) -> f32,
        kept: impl IntoIterator<Item = u64>,
    ) -> cosmic::Element<'a, ()> {
        let names = keys.clone();
        culled_column(keys, move |index| {
            built.borrow_mut().push(index);
            let key = names[index] as usize;
            cosmic_widget::button::custom(
                cosmic_widget::Space::new()
                    .width(Length::Fill)
                    .height(height(key)),
            )
            .id(widget::Id::new(format!("row-{key}")))
            .on_press(())
            .padding(0)
            .into()
        })
        .spacing(SPACING)
        .estimate(ROW)
        .keep_built(kept)
        .into()
    }

    struct Harness<'a> {
        element: cosmic::Element<'a, ()>,
        tree: Tree,
        node: layout::Node,
        renderer: Renderer,
    }

    impl<'a> Harness<'a> {
        fn new(mut element: cosmic::Element<'a, ()>) -> Self {
            let renderer = super::super::accessibility_probe::headless_renderer();
            let mut tree = Tree::new(element.as_widget());
            let node = layout_of(&mut element, &mut tree, &renderer);
            Self {
                element,
                tree,
                node,
                renderer,
            }
        }

        fn relayout(&mut self) {
            self.node = layout_of(&mut self.element, &mut self.tree, &self.renderer);
        }

        /// Lays out at `width` instead of the screen's.
        fn relayout_at(&mut self, width: f32) {
            self.node = self.element.as_widget_mut().layout(
                &mut self.tree,
                &self.renderer,
                &layout::Limits::new(Size::ZERO, Size::new(width, f32::INFINITY)),
            );
        }

        /// Takes where the column wants its viewport's top, as
        /// [`keep_in_place`] does.
        fn take_follow(&mut self) -> Option<f32> {
            let mut take = TakeFollow::default();
            self.element.as_widget_mut().operate(
                &mut self.tree,
                Layout::new(&self.node),
                &self.renderer,
                &mut take,
            );
            take.follow.map(|(top, _)| top)
        }

        /// Where the row with `key` was placed, from the column's top.
        fn top_of(&self, key: u64) -> Option<f32> {
            let state = self.tree.state.downcast_ref::<State>();
            let index = state.mounted.iter().position(|row| *row == key)?;
            Some(state.placed[index].0)
        }

        /// The first mounted row whose bottom is below `top`.
        fn row_at(&self, top: f32) -> u64 {
            let state = self.tree.state.downcast_ref::<State>();
            state
                .mounted
                .iter()
                .zip(&state.placed)
                .find(|(_, (_, bottom))| *bottom > top)
                .map(|(key, _)| *key)
                .unwrap()
        }

        /// Scrolls to `top`, lays out if the column asks to, and follows the
        /// row on screen as [`keep_in_place`] would; returns the new top.
        fn follow(&mut self, top: f32) -> f32 {
            if self.scroll_to(top) {
                self.relayout();
            }
            self.take_follow().unwrap_or(top)
        }

        /// Delivers a redraw with the column scrolled to `top`; returns
        /// whether the column asked for a new layout.
        fn scroll_to(&mut self, top: f32) -> bool {
            let viewport = Rectangle::new(Point::new(0.0, top), SCREEN);
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            self.element.as_widget_mut().update(
                &mut self.tree,
                &Event::Window(window::Event::RedrawRequested(
                    cosmic::iced::time::Instant::now(),
                )),
                Layout::new(&self.node),
                mouse::Cursor::Unavailable,
                &self.renderer,
                &mut clipboard::Null,
                &mut shell,
                &viewport,
            );
            shell.is_layout_invalid()
        }

        fn run(&mut self, operation: Box<dyn Operation>) {
            let mut operation = operation;
            loop {
                self.element.as_widget_mut().operate(
                    &mut self.tree,
                    Layout::new(&self.node),
                    &self.renderer,
                    operation.as_mut(),
                );
                match operation.finish() {
                    Outcome::Chain(next) => operation = next,
                    _ => break,
                }
            }
        }

        fn mounted(&self) -> Vec<u64> {
            self.tree.state.downcast_ref::<State>().mounted.clone()
        }

        fn focused(&self) -> Option<u64> {
            self.tree.state.downcast_ref::<State>().focused
        }
    }

    fn layout_of(
        element: &mut cosmic::Element<'_, ()>,
        tree: &mut Tree,
        renderer: &Renderer,
    ) -> layout::Node {
        element.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(Size::ZERO, Size::new(SCREEN.width, f32::INFINITY)),
        )
    }

    #[test]
    fn only_rows_near_the_viewport_are_built_and_the_rest_keep_their_height() {
        let built = Rc::new(RefCell::new(Vec::new()));
        let harness = Harness::new(column(3000, built.clone()));
        let stride = ROW + SPACING;
        assert_eq!(harness.node.size().height, 3000.0 * stride - SPACING);
        let rows = built.borrow().len();
        // The first viewport, before any draw, plus the overscan below it.
        assert!(
            rows <= ((FIRST_VIEWPORT + OVERSCAN) / stride) as usize + 2,
            "{rows} rows built of 3000"
        );
        let mounted = harness.mounted();
        assert_eq!(mounted.first(), Some(&0));
        // Built rows sit where every row before them puts them.
        for (child, key) in harness.node.children().iter().zip(&mounted) {
            assert_eq!(child.bounds().y, *key as f32 * stride);
        }
    }

    #[test]
    fn scrolling_past_the_built_rows_lays_out_again_around_the_viewport() {
        let built = Rc::new(RefCell::new(Vec::new()));
        let mut harness = Harness::new(column(3000, built.clone()));
        let stride = ROW + SPACING;
        assert!(!harness.scroll_to(0.0), "the first screen is already built");
        let top = 2000.0 * stride;
        assert!(harness.scroll_to(top));
        built.borrow_mut().clear();
        harness.relayout();
        let mounted = harness.mounted();
        assert!(
            mounted.contains(&2000) && mounted.contains(&2009),
            "{mounted:?}"
        );
        assert!(!mounted.contains(&0), "rows far above are dropped");
        assert!(built.borrow().len() <= ((SCREEN.height + 2.0 * OVERSCAN) / stride) as usize + 2);
        for (child, key) in harness.node.children().iter().zip(&mounted) {
            assert_eq!(child.bounds().y, *key as f32 * stride);
        }
        // Small moves inside the overscan need no new layout.
        assert!(!harness.scroll_to(top + OVERSCAN / 4.0));
    }

    #[test]
    fn focus_moves_row_by_row_past_the_built_rows_and_stays_built_when_scrolled_away() {
        let built = Rc::new(RefCell::new(Vec::new()));
        let mut harness = Harness::new(column(3000, built));
        harness.scroll_to(0.0);
        let last_built = *harness.mounted().last().unwrap();
        for _ in 0..=last_built + 3 {
            harness.run(Box::new(focusable::focus_next::<()>()));
            // The next event lays the column out around the new focus.
            if harness.scroll_to(0.0) {
                harness.relayout();
            }
        }
        // Focus has walked one row at a time past the first built window.
        assert_eq!(harness.focused(), Some(last_built + 3));
        assert!(harness.mounted().contains(&(last_built + 4)));
        // Scrolled far away, the focused row and its neighbours stay built.
        assert!(harness.scroll_to(2500.0 * (ROW + SPACING)));
        harness.relayout();
        let mounted = harness.mounted();
        assert!(mounted.contains(&(last_built + 3)), "{mounted:?}");
        assert!(mounted.contains(&2500));
        // An operation that skips the rows, as a finished search does,
        // learns nothing of focus and must not make the column forget it.
        struct Skips;
        impl Operation for Skips {
            fn traverse(&mut self, _operate: &mut dyn FnMut(&mut dyn Operation)) {}
        }
        harness.run(Box::new(Skips));
        assert!(!harness.scroll_to(2500.0 * (ROW + SPACING)));
        assert_eq!(harness.focused(), Some(last_built + 3));
    }

    #[test]
    fn a_row_keeps_its_widget_state_while_the_window_moves() {
        let built = Rc::new(RefCell::new(Vec::new()));
        let mut harness = Harness::new(column(3000, built));
        harness.scroll_to(0.0);
        // Focus row 1, then move the window so row 0 drops and rows shift.
        harness.run(Box::new(focusable::focus_next::<()>()));
        harness.run(Box::new(focusable::focus_next::<()>()));
        assert_eq!(harness.focused(), Some(1));
        harness.scroll_to(30.0 * (ROW + SPACING));
        harness.relayout();
        // Row 1 stays built beside the new window, with the rows between
        // dropped, so its tree has moved to another position.
        let mounted = harness.mounted();
        assert!(
            mounted.contains(&1) && !mounted.contains(&10),
            "{mounted:?}"
        );
        // Row 1's button still holds focus after its tree moved position.
        let mut probe = FocusedId::default();
        harness.element.as_widget_mut().operate(
            &mut harness.tree,
            Layout::new(&harness.node),
            &harness.renderer,
            &mut probe,
        );
        assert_eq!(probe.0, Some(widget::Id::new("row-1")));
    }

    #[test]
    fn the_column_keeps_its_height_while_scrolling_through_rows_taller_than_the_estimate() {
        // Cards come out taller than their estimate, and not all alike; the
        // scrollbar must not creep or jump as newly built rows are measured.
        let built = Rc::new(RefCell::new(Vec::new()));
        let mut harness = Harness::new(column_of(3000, built, |index| {
            if index % 3 == 0 { 70.0 } else { 55.0 }
        }));
        let first = harness.node.size().height;
        let exact = 1000.0 * 70.0 + 2000.0 * 55.0 + 2999.0 * SPACING;
        assert!(
            (first - exact).abs() / exact < 0.005,
            "{first} after the first layout, {exact} measured in full"
        );
        // Scroll through every row a screen at a time, as a fast wheel or a
        // dragged thumb does, laying out whenever the column asks.
        let mut top = 0.0;
        let mut previous = first;
        while top < harness.node.size().height {
            if harness.scroll_to(top) {
                harness.relayout();
            }
            let height = harness.node.size().height;
            // Each newly built row corrects the column by its own difference
            // from the assumed height, at most 15 px, and they cancel out.
            assert!(
                (height - previous).abs() <= 30.0 && (height - exact).abs() / exact < 0.005,
                "the column went from {previous} to {height} at {top}, of {exact}"
            );
            previous = height;
            top += SCREEN.height;
        }
        assert_eq!(harness.node.size().height, exact);
    }

    /// Rows of uneven height, none the estimated `ROW`.
    fn uneven(index: usize) -> f32 {
        [70.0, 55.0, 55.0, 90.0, 48.0][index % 5]
    }

    #[test]
    fn scrolling_up_through_rows_never_measured_leaves_what_is_on_screen_in_place() {
        let built = Rc::new(RefCell::new(Vec::new()));
        let mut harness = Harness::new(column_of(3000, built, uneven));
        // Jump to the middle, as a dragged thumb does, past every row measured.
        let mut top = harness.follow(1500.0 * (ROW + SPACING));
        // Then wheel up through rows never measured, a little at a time.
        for _ in 0..60 {
            let key = harness.row_at(top);
            let on_screen = harness.top_of(key).unwrap() - top;
            top = harness.follow(top - 100.0);
            let now = harness.top_of(key).unwrap() - top;
            assert!(
                (now - (on_screen + 100.0)).abs() < 0.01,
                "row {key} was {on_screen} px into the view and is now {now}"
            );
        }
    }

    #[test]
    fn a_width_change_keeps_the_row_at_the_top_of_the_view() {
        let built = Rc::new(RefCell::new(Vec::new()));
        let mut harness = Harness::new(column_of(3000, built, uneven));
        let mut top = 0.0;
        while top < 20_000.0 {
            top = harness.follow(top + SCREEN.height);
        }
        let key = harness.row_at(top);
        let on_screen = harness.top_of(key).unwrap() - top;
        // Every measured height is forgotten at a new width.
        harness.relayout_at(SCREEN.width - 20.0);
        let top = harness.take_follow().unwrap_or(top);
        assert_eq!(harness.top_of(key).unwrap() - top, on_screen);
    }

    #[test]
    fn rows_added_above_the_view_leave_what_is_on_screen_in_place() {
        let built = Rc::new(RefCell::new(Vec::new()));
        let keys: Vec<u64> = (100..3100).collect();
        let mut harness = Harness::new(keyed(keys.clone(), built.clone(), uneven, []));
        let top = harness.follow(0.0);
        let top = harness.follow(top + 2000.0);
        let key = harness.row_at(top);
        let on_screen = harness.top_of(key).unwrap() - top;
        // Ten new rows arrive at the head of the list.
        let more = (0..10).chain(keys).collect();
        harness.element = keyed(more, built, uneven, []);
        harness.tree.diff(harness.element.as_widget_mut());
        harness.relayout();
        let top = harness.take_follow().unwrap_or(top);
        assert_eq!(harness.top_of(key).unwrap() - top, on_screen);
        // At the very top, the new rows show instead.
        let mut harness = Harness::new(keyed((100..3100).collect(), Rc::default(), uneven, []));
        harness.follow(0.0);
        harness.element = keyed((0..3100).collect(), Rc::default(), uneven, []);
        harness.relayout();
        assert_eq!(harness.take_follow(), None);
        assert_eq!(harness.mounted().first(), Some(&0));
    }

    #[test]
    fn a_kept_row_stays_built_wherever_the_view_is() {
        let built = Rc::new(RefCell::new(Vec::new()));
        let mut harness = Harness::new(keyed((0..3000).collect(), built, |_| ROW, [5]));
        harness.follow(2500.0 * (ROW + SPACING));
        harness.relayout();
        let mounted = harness.mounted();
        assert!(
            mounted.contains(&5) && mounted.contains(&2500),
            "{mounted:?}"
        );
    }

    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};

    /// The vertical translation of the scrollable, and the content-space
    /// top of each named row.
    #[derive(Default)]
    struct Positions(f32, HashMap<String, f32>);
    impl Operation for Positions {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn scrollable(
            &mut self,
            _id: Option<&Id>,
            _bounds: Rectangle,
            _content_bounds: Rectangle,
            translation: Vector,
            _state: &mut dyn operation::Scrollable,
        ) {
            self.0 = translation.y;
        }
        fn focusable(
            &mut self,
            id: Option<&Id>,
            bounds: Rectangle,
            _state: &mut dyn operation::Focusable,
        ) {
            if let Some(id) = id {
                self.1.insert(format!("{id:?}"), bounds.y);
            }
        }
    }

    impl Positions {
        fn of(ui: &mut UserInterface<'_, (), Theme, Renderer>, renderer: &Renderer) -> Self {
            let mut positions = Self::default();
            ui.operate(renderer, &mut positions);
            positions
        }

        /// Where `row` is on screen, if it is built. Names are matched by
        /// the row's key, as each build of a row makes a new `Id`.
        fn on_screen(&self, row: &str) -> Option<f32> {
            self.1
                .iter()
                .find(|(name, _)| name.contains(&format!("\"{row}\"")))
                .map(|(_, top)| top - self.0)
        }

        /// The key of the row named `name`.
        fn key(name: &str) -> u64 {
            name.split("row-")
                .nth(1)
                .unwrap()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap()
        }
    }

    /// A lane as Work builds one: the column of uneven rows with `keys`, in
    /// a scrollable that keeps what is on screen in place.
    fn lane<'a>(keys: Vec<u64>) -> cosmic::Element<'a, ()> {
        keep_in_place(
            cosmic_widget::scrollable(keyed(keys, Rc::default(), uneven, std::iter::empty()))
                .id(Id::new("lane"))
                .height(Length::Fill),
        )
    }

    /// Redraws as the runtime does, again while layout keeps changing.
    fn settle(
        ui: &mut UserInterface<'_, (), Theme, Renderer>,
        renderer: &mut Renderer,
        cursor: mouse::Cursor,
    ) {
        let redraw = Event::Window(window::Event::RedrawRequested(
            cosmic::iced::time::Instant::now(),
        ));
        for _ in 0..3 {
            let (state, _) = ui.update(
                std::slice::from_ref(&redraw),
                cursor,
                renderer,
                &mut clipboard::Null,
                &mut Vec::new(),
            );
            if !state.has_layout_changed() {
                break;
            }
        }
    }

    #[test]
    fn keep_in_place_scrolls_to_follow_the_row_on_screen_through_the_runtime() {
        let mut renderer = super::super::accessibility_probe::headless_renderer();
        let id = Id::new("lane");
        let view = || lane((0..3000).collect());
        let mut ui = UserInterface::build(view(), SCREEN, Cache::new(), &mut renderer);
        settle(&mut ui, &mut renderer, mouse::Cursor::Unavailable);
        ui.operate(
            &renderer,
            &mut operation::scrollable::scroll_to(
                id,
                AbsoluteOffset {
                    x: None,
                    y: Some(1500.0 * (ROW + SPACING)),
                },
            ),
        );
        settle(&mut ui, &mut renderer, mouse::Cursor::Unavailable);
        let pointer = Point::new(150.0, 200.0);
        let cursor = mouse::Cursor::Available(pointer);
        let mut followed = 0;
        for _ in 0..30 {
            let before = Positions::of(&mut ui, &renderer);
            let _ = ui.update(
                &[
                    Event::Mouse(mouse::Event::CursorMoved { position: pointer }),
                    Event::Mouse(mouse::Event::WheelScrolled {
                        delta: mouse::ScrollDelta::Pixels { x: 0.0, y: 100.0 },
                    }),
                ],
                cursor,
                &mut renderer,
                &mut clipboard::Null,
                &mut Vec::new(),
            );
            settle(&mut ui, &mut renderer, cursor);
            let after = Positions::of(&mut ui, &renderer);
            // Every row on screen before and after moved down by the wheel's
            // 100 px, though rows above it were measured in between.
            let moved = ((before.0 - after.0) - 100.0).abs() > 0.5;
            for (name, top) in &before.1 {
                let Some(now) = after.1.get(name) else {
                    continue;
                };
                let (was, is) = (top - before.0, now - after.0);
                if (0.0..SCREEN.height).contains(&was) {
                    assert!(
                        (is - (was + 100.0)).abs() < 0.01,
                        "{name} was at {was}, is at {is}"
                    );
                }
            }
            followed += usize::from(moved);
        }
        assert!(followed > 0, "no layout moved the rows on screen");
    }

    /// Scrolls a lane of 3,000 uneven rows far down, then lists each of
    /// `filters` in turn, the last of them every row, as typing and clearing
    /// a filter does. Asserts that the row being read before the first is
    /// where it was whenever it is listed.
    fn filter_far_down(filters: &[&dyn Fn(u64, u64) -> bool]) {
        let mut renderer = super::super::accessibility_probe::headless_renderer();
        let all: Vec<u64> = (0..3000).collect();
        let mut ui = UserInterface::build(lane(all.clone()), SCREEN, Cache::new(), &mut renderer);
        settle(&mut ui, &mut renderer, mouse::Cursor::Unavailable);
        ui.operate(
            &renderer,
            &mut operation::scrollable::scroll_to(
                Id::new("lane"),
                AbsoluteOffset {
                    x: None,
                    y: Some(1500.0 * (ROW + SPACING)),
                },
            ),
        );
        settle(&mut ui, &mut renderer, mouse::Cursor::Unavailable);
        // The row the viewport's top is in.
        let before = Positions::of(&mut ui, &renderer);
        let (key, was) = before
            .1
            .iter()
            .map(|(name, top)| (Positions::key(name), top - before.0))
            .filter(|(key, top)| top + uneven(*key as usize) > 0.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        let row = format!("row-{key}");
        let everything: &dyn Fn(u64, u64) -> bool = &|_, _| true;
        for filter in filters.iter().chain([&everything]) {
            let keys: Vec<u64> = all.iter().copied().filter(|k| filter(*k, key)).collect();
            let listed = keys.contains(&key);
            let cache = ui.into_cache();
            ui = UserInterface::build(lane(keys), SCREEN, cache, &mut renderer);
            settle(&mut ui, &mut renderer, mouse::Cursor::Unavailable);
            if listed {
                let is = Positions::of(&mut ui, &renderer).on_screen(&row);
                // The scrollable draws at a whole-pixel offset.
                assert!(
                    is.is_some_and(|is| (is - was).abs() <= 0.5),
                    "{row} was at {was}, is at {is:?}"
                );
            }
        }
    }

    #[test]
    fn a_filter_that_keeps_the_row_being_read_leaves_it_in_place() {
        // One row in ten, the row being read among them.
        filter_far_down(&[&|k, key| k % 10 == key % 10]);
    }

    #[test]
    fn clearing_a_filter_that_hid_the_row_being_read_returns_to_it() {
        // Twenty rows from the top of the list, none of them built, though
        // together taller than the lane, so the lane shows their end.
        filter_far_down(&[&|k, _| k < 20]);
    }

    #[test]
    fn typing_a_filter_a_letter_at_a_time_and_clearing_it_returns_to_the_row_being_read() {
        // The first letters keep the row being read; later ones hide it.
        filter_far_down(&[
            &|k, key| k % 2 == key % 2,
            &|k, key| k % 10 == key % 10,
            &|k, _| k < 20,
            &|k, _| k < 5,
        ]);
    }

    #[test]
    fn a_filter_that_leaves_the_row_being_read_last_settles_at_the_end() {
        let mut renderer = super::super::accessibility_probe::headless_renderer();
        let mut ui = UserInterface::build(
            lane((0..3000).collect()),
            SCREEN,
            Cache::new(),
            &mut renderer,
        );
        settle(&mut ui, &mut renderer, mouse::Cursor::Unavailable);
        ui.operate(
            &renderer,
            &mut operation::scrollable::scroll_to(
                Id::new("lane"),
                AbsoluteOffset {
                    x: None,
                    y: Some(1500.0 * (ROW + SPACING)),
                },
            ),
        );
        settle(&mut ui, &mut renderer, mouse::Cursor::Unavailable);
        let before = Positions::of(&mut ui, &renderer);
        let (key, _) = before
            .1
            .iter()
            .map(|(name, top)| (Positions::key(name), top - before.0))
            .filter(|(key, top)| top + uneven(*key as usize) > 0.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        // The row being read is now the last listed, with nothing after it
        // to bring it back to the top of the view.
        let cache = ui.into_cache();
        ui = UserInterface::build(lane((0..=key).collect()), SCREEN, cache, &mut renderer);
        settle(&mut ui, &mut renderer, mouse::Cursor::Unavailable);
        let redraw = Event::Window(window::Event::RedrawRequested(
            cosmic::iced::time::Instant::now(),
        ));
        let relaid = (0..10)
            .filter(|_| {
                let (state, _) = ui.update(
                    std::slice::from_ref(&redraw),
                    mouse::Cursor::Unavailable,
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut Vec::new(),
                );
                state.has_layout_changed()
            })
            .count();
        assert_eq!(
            relaid, 0,
            "the lane laid out again on {relaid} of 10 redraws"
        );
        // It shows the end of the list, the row being read at the bottom.
        let is = Positions::of(&mut ui, &renderer)
            .on_screen(&format!("row-{key}"))
            .unwrap();
        assert!(
            (is + uneven(key as usize) - SCREEN.height).abs() <= 0.5,
            "row-{key} is at {is}"
        );
    }
}
