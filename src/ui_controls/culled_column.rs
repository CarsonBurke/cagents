//! A vertical list that builds only the rows near its scroll viewport.
//!
//! A long list inside a [`scrollable`](cosmic::iced::widget::scrollable) pays
//! to build, lay out and diff every row on each rebuild, though only a
//! screenful is ever seen. [`culled_column`] takes a key per row and a builder
//! instead of the rows themselves. During layout it builds just the rows that
//! lie within an overscan of the last viewport it was drawn in, plus the row
//! holding keyboard focus, and stands in for the rest with their height.
//!
//! - Heights are measured as rows are laid out and kept per key, so content
//!   above and below the mounted rows keeps its place while scrolling. A row
//!   never measured at the current width counts as `estimate` until it is.
//! - Row widget state is kept per key, as in a keyed list, so hover, focus
//!   and open tooltips stay with their row when the mounted window moves.
//! - When the viewport moves past what is mounted, the column asks for a new
//!   layout itself; no message reaches the application.
//! - The row with keyboard focus stays mounted, and so do the rows beside it,
//!   so moving focus row by row never runs off the end of what is built.
//!
//! Rows outside the window have no widgets, so they are absent from the
//! accessibility tree and from pointer and keyboard operations until they come
//! near the viewport.

use std::collections::HashMap;
use std::ops::Range;

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use cosmic::iced::{Element, Event, Length, Rectangle, Size, Vector};
use cosmic::{Renderer, Theme};

/// How far beyond the viewport, above and below, rows are kept built.
const OVERSCAN: f32 = 480.0;
/// The viewport assumed before the column has been drawn once.
const FIRST_VIEWPORT: f32 = 1200.0;
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
        mounted: Vec::new(),
    }
}

pub struct CulledColumn<'a, Message> {
    keys: Vec<u64>,
    build: Box<dyn Fn(usize) -> Element<'a, Message, Theme, Renderer> + 'a>,
    spacing: f32,
    estimate: f32,
    /// The rows built in the last layout, by index, in order.
    mounted: Vec<(usize, Element<'a, Message, Theme, Renderer>)>,
}

impl<Message> CulledColumn<'_, Message> {
    /// The gap between rows.
    pub fn spacing(mut self, spacing: f32) -> Self {
        self.spacing = spacing;
        self
    }

    /// The height assumed for a row until it has been measured.
    pub fn estimate(mut self, estimate: f32) -> Self {
        self.estimate = estimate;
        self
    }
}

#[derive(Default)]
struct State {
    /// Measured row heights, by key, at `width`.
    heights: HashMap<u64, f32>,
    width: f32,
    /// The keys whose trees are `tree.children`, in order.
    mounted: Vec<u64>,
    /// The span of content, from the column's top, that the mounted rows
    /// cover; a viewport inside it needs no new layout.
    covered: Range<f32>,
    /// The visible span of the column, from its top, as last drawn.
    viewport: std::cell::Cell<Option<(f32, f32)>>,
    /// The key whose widget holds keyboard focus, as operations last found.
    focused: Option<u64>,
    /// The focused key the last layout built around.
    laid_out_focus: Option<u64>,
}

impl State {
    fn height(&self, key: u64, estimate: f32) -> f32 {
        self.heights.get(&key).copied().unwrap_or(estimate)
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
        let mut y = 0.0;
        for key in &self.keys {
            tops.push(y);
            y += state.height(*key, self.estimate) + self.spacing;
        }
        tops.push(y);
        tops
    }

    /// Which rows to build: the contiguous run within the overscanned
    /// viewport, then the focused row and its neighbours, sorted.
    fn window(&self, state: &State, tops: &[f32]) -> (Range<usize>, Vec<usize>) {
        let rows = &tops[..self.keys.len()];
        let span = wanted(
            state.viewport.get().unwrap_or((0.0, FIRST_VIEWPORT)),
            OVERSCAN,
        );
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
            built.sort_unstable();
            built.dedup();
        }
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
        if (state.width - width).abs() > 0.5 {
            state.heights.clear();
            state.width = width;
        }
        let tops = self.tops(state);
        let (run, rows) = self.window(state, &tops);

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
            state.heights.insert(self.keys[*index], node.size().height);
        }
        // Place rows by the measured heights, which may differ from the
        // estimates the window was chosen by.
        let tops = self.tops(state);
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
            state.viewport.set(Some(span));
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
        if let Some(span) = visible_span(layout.bounds(), viewport) {
            tree.state.downcast_ref::<State>().viewport.set(Some(span));
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
        culled_column((0..rows as u64).collect(), move |index| {
            built.borrow_mut().push(index);
            cosmic_widget::button::custom(
                cosmic_widget::Space::new().width(Length::Fill).height(ROW),
            )
            .id(widget::Id::new(format!("row-{index}")))
            .on_press(())
            .padding(0)
            .into()
        })
        .spacing(SPACING)
        .estimate(ROW)
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
}
