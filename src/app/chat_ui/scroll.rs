//! Layout-time anchoring around iced's native scrollable. Its own state remains
//! authoritative for wheel, thumb, keyboard and touch input.
mod block;
mod viewport;
pub(super) use block::block;
pub(super) use viewport::viewport;

use super::view::Scroll;
use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use iced::{Event, Rectangle, Size, Vector};
use std::{cell::Cell, rc::Rc};

/// `anchors` names direct children of the transcript column. Only their bounds
/// and stable identities are retained; the transcript's layout tree is not.
pub(super) fn transcript<'a>(
    content: impl Into<Element<'a, Message>>,
    target: Target,
    subagent: Option<&'a str>,
    anchors: Vec<(usize, &'a str)>,
    follow_end: bool,
    items: Vec<(Id, &'a str)>,
) -> Element<'a, Message> {
    transcript_impl(
        content.into(),
        target,
        subagent,
        anchors,
        follow_end,
        items,
        None,
        Memory::default(),
    )
}

pub(super) type Observations = Rc<std::cell::RefCell<HashMap<Option<String>, Scroll>>>;

/// Persist only reader geometry and stable row IDs across native tree eviction.
/// No source text, widgets, fonts or shaped paragraphs are retained here.
#[derive(Clone, Default)]
pub(super) struct Memory(
    Rc<std::cell::RefCell<Anchor>>,
    Rc<std::cell::RefCell<block::Positions>>,
);
struct ReaderState {
    memory: Memory,
    restore: bool,
}

impl Memory {
    pub fn clear(&self) {
        *self.0.borrow_mut() = Anchor::default();
        *self.1.borrow_mut() = block::Positions::default();
    }
    pub fn retain_children(&self, children: &[chat::Subagent]) {
        let mut state = self.0.borrow_mut();
        let active = state
            .active
            .and_then(|index| state.contexts.get(index))
            .map(|context| context.subagent.clone());
        state.contexts.retain(|context| {
            context
                .subagent
                .as_ref()
                .is_none_or(|id| children.iter().any(|child| &child.id == id))
        });
        state.active = active.and_then(|active| {
            state
                .contexts
                .iter()
                .position(|context| context.subagent == active)
        });
        self.1.borrow_mut().entries.retain(|(context, _), _| {
            context
                .as_ref()
                .is_none_or(|id| children.iter().any(|child| &child.id == id))
        });
        if state.reported.as_ref().is_some_and(|report| {
            report
                .subagent
                .as_ref()
                .is_some_and(|id| !children.iter().any(|child| &child.id == id))
        }) {
            state.reported = None;
        }
    }
    pub fn retained_rows(&self) -> usize {
        self.0
            .borrow()
            .contexts
            .iter()
            .map(|context| context.rows.len())
            .sum()
    }
}

pub(super) fn transcript_observed<'a>(
    content: impl Into<Element<'a, Message>>,
    target: Target,
    subagent: Option<&'a str>,
    anchors: Vec<(usize, &'a str)>,
    follow_end: bool,
    items: Vec<(Id, &'a str)>,
    observations: Observations,
    memory: Memory,
) -> Element<'a, Message> {
    transcript_impl(
        content.into(),
        target,
        subagent,
        anchors,
        follow_end,
        items,
        Some(observations),
        memory,
    )
}

fn transcript_impl<'a>(
    content: Element<'a, Message>,
    target: Target,
    subagent: Option<&'a str>,
    anchors: Vec<(usize, &'a str)>,
    follow_end: bool,
    items: Vec<(Id, &'a str)>,
    observations: Option<Observations>,
    memory: Memory,
) -> Element<'a, Message> {
    let captured = Rc::new(Cell::new(false));
    let content = widget::scrollable(Element::new(InputCapture {
        content: content.into(),
        captured: captured.clone(),
    }))
    .direction(iced::widget::scrollable::Direction::Vertical(
        design::scrollbar(),
    ))
    .id(card_control_id(target.card, "chat-transcript"))
    .anchor_bottom()
    .height(Length::Fill)
    .width(Length::Fill);
    Element::new(Anchored {
        content: content.into(),
        captured: Some(captured),
        target: Some(target),
        subagent,
        anchors,
        follow_end,
        items,
        observations,
        memory,
    })
}

/// Tool output follows its live end while preserving an explicitly scrolled
/// reading position. It does not participate in transcript pagination.
pub(super) fn tool_output<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    Element::new(Anchored {
        content: content.into(),
        target: None,
        captured: None,
        subagent: None,
        anchors: Vec::new(),
        follow_end: true,
        items: Vec::new(),
        observations: None,
        memory: Memory::default(),
    })
}

/// Measure a displayed message independently of its presentation group. A page
/// can keep an answer while evicting its prompt, changing the group's identity.
pub(super) fn item<'a>(id: &str, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    widget::container(content).id(item_id(id)).into()
}

pub(super) fn item_id(id: &str) -> Id {
    Id::new(format!("chat-scroll-item-{id}"))
}
#[derive(Clone, Copy, Hash)]
pub(super) enum ReaderAxis {
    Horizontal,
    Vertical,
}
pub(super) fn reader_key(logical: u64, axis: ReaderAxis) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut key = std::hash::DefaultHasher::new();
    ("reader-axis", logical, axis).hash(&mut key);
    key.finish()
}
pub(super) fn block_id(target: Target, context: Option<&str>, logical: u64) -> Id {
    use std::hash::{Hash, Hasher};
    let mut key = std::hash::DefaultHasher::new();
    (target.generation, context, logical).hash(&mut key);
    card_control_id(
        target.card,
        &format!("chat-block-scroll-{:016x}", key.finish()),
    )
}

pub(super) fn steps_id(id: &str) -> Id {
    Id::new(format!("chat-scroll-steps-{id}"))
}

/// One explicit Find navigation measurement. The caller accepts it only for
/// its current query/document/navigation generations, applies `offset`, waits
/// for the resulting layout, and retries until `exact` or its bounded limit.
/// Wheel/drag navigation must cancel that pending correction in the caller.
#[derive(Clone, Copy, Debug)]
pub(super) struct LogicalLocation {
    pub offset: f32,
    pub exact: bool,
    pub stage: u8,
}

struct LocateLogical {
    scroll: Id,
    item: Id,
    identity: u64,
    byte: usize,
    content: Option<Rectangle>,
    viewport: Option<Rectangle>,
    found: Option<(Rectangle, u8)>,
}
impl LocateLogical {
    fn candidate(&mut self, bounds: Rectangle, stage: u8) {
        if self
            .found
            .as_ref()
            .is_none_or(|(_, previous)| stage >= *previous)
        {
            self.found = Some((bounds, stage));
        }
    }
}
impl Operation<Option<LogicalLocation>> for LocateLogical {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<Option<LogicalLocation>>)) {
        visit(self);
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        viewport: Rectangle,
        content: Rectangle,
        _: Vector,
        _: &mut dyn operation::Scrollable,
    ) {
        if id == Some(&self.scroll) {
            self.content = Some(content);
            self.viewport = Some(viewport);
        }
    }
    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if id == Some(&self.item) {
            self.candidate(bounds, 0);
        }
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some((bounds, stage)) =
            virtualized::logical_bounds(state, bounds, self.identity, self.byte)
        {
            self.candidate(bounds, stage);
        }
        if let Some(prose) = state.downcast_ref::<selectable_prose::State>()
            && let Some(local) = prose.logical_byte_bounds(self.identity, self.byte)
        {
            self.candidate(
                Rectangle {
                    x: bounds.x + local.x,
                    y: bounds.y + local.y,
                    ..local
                },
                3,
            );
        }
    }
    fn finish(&self) -> operation::Outcome<Option<LogicalLocation>> {
        operation::Outcome::Some(self.content.zip(self.viewport).zip(self.found).map(
            |((content, viewport), (found, stage))| {
                let travel = (content.height - viewport.height).max(0.0);
                let from_top = (found.y - content.y - (viewport.height - found.height) / 2.0)
                    .clamp(0.0, travel);
                LogicalLocation {
                    offset: travel - from_top,
                    exact: stage == 3,
                    stage,
                }
            },
        ))
    }
}

pub(super) fn locate_logical(
    target: Target,
    item: String,
    identity: u64,
    byte: usize,
) -> iced::Task<Option<LogicalLocation>> {
    iced::advanced::widget::operate(LocateLogical {
        scroll: card_control_id(target.card, "chat-transcript"),
        item: item_id(&item),
        identity,
        byte,
        content: None,
        viewport: None,
        found: None,
    })
}

struct ItemBounds<'a> {
    ids: std::collections::HashMap<Id, Vec<&'a str>>,
    rows: std::collections::HashMap<&'a str, Rectangle>,
    wanted: HashSet<u64>,
    virtual_rows: Vec<(String, Rectangle)>,
}
impl Operation for ItemBounds<'_> {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        self.virtual_rows.extend(
            virtualized::anchor_points(state, bounds, &self.wanted)
                .into_iter()
                .map(|(key, bounds)| (format!("virtual-chunk-{key:016x}"), bounds)),
        );
    }
    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if let Some(id) = id.and_then(|id| self.ids.get(id)) {
            for id in id {
                self.rows.insert(*id, bounds);
            }
        }
    }
}

struct Anchored<'a> {
    captured: Option<Rc<Cell<bool>>>,
    target: Option<Target>,
    content: Element<'a, Message>,
    subagent: Option<&'a str>,
    anchors: Vec<(usize, &'a str)>,
    follow_end: bool,
    items: Vec<(Id, &'a str)>,
    observations: Option<Observations>,
    memory: Memory,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Geometry {
    viewport: Size,
    content: Size,
}

impl Geometry {
    fn travel(self) -> f32 {
        (self.content.height - self.viewport.height).max(0.0)
    }

    fn node(self) -> layout::Node {
        layout::Node::with_children(self.viewport, vec![layout::Node::new(self.content)])
    }
}

struct Row {
    id: String,
    bounds: Rectangle,
}

#[derive(Default)]
struct Context {
    subagent: Option<String>,
    geometry: Option<Geometry>,
    rows: Vec<Row>,
    top: Option<f32>,
    used: u64,
    follow_end: bool,
}

impl Context {
    fn position(&self, next: Geometry, rows: &[(&str, Rectangle)], follow_end: bool) -> f32 {
        let Some((previous, top)) = self.geometry.zip(self.top) else {
            return if self.subagent.is_some() {
                0.0
            } else {
                next.travel()
            };
        };
        // Fitting live content normally follows its end. But an earlier page
        // can prepend surviving rows to that same short transcript: preserve
        // the exchange the reader was looking at instead of jumping past it.
        let prepended = self.rows.first().is_some_and(|old| {
            rows.iter()
                .any(|(id, bounds)| *id == old.id && bounds.y > old.bounds.y + 0.5)
        });
        if !prepended
            && self.follow_end
            && follow_end
            && previous.travel() - top <= 0.5
            && (previous.travel() > 0.0 || self.subagent.is_none())
        {
            return next.travel();
        }
        // A recreated widget can restore authoritative native geometry from
        // the Runtime before it has any measured row anchors.
        if self.rows.is_empty() {
            return top.clamp(0.0, next.travel());
        }
        // Most relayouts only refresh a timer or hover state. Avoid building
        // a lookup table when every measured row is unchanged.
        if self.rows.len() == rows.len()
            && self
                .rows
                .iter()
                .zip(rows)
                .all(|(old, (id, bounds))| old.id == *id && old.bounds == *bounds)
        {
            return top.clamp(0.0, next.travel());
        }
        let next_rows: std::collections::HashMap<_, _> = rows.iter().copied().collect();
        // Preserve the first visible surviving exchange, not the transcript's
        // total height: a page may arrive while the live answer also grows.
        let visible = || {
            self.rows.iter().filter(|row| {
                row.bounds.y + row.bounds.height > top
                    && row.bounds.y < top + previous.viewport.height
            })
        };
        let overlap = visible()
            .filter(|row| row.id.starts_with("virtual-chunk-"))
            .filter_map(|row| next_rows.get(row.id.as_str()).map(|bounds| (row, bounds)))
            .next()
            .or_else(|| {
                visible()
                    .filter_map(|row| next_rows.get(row.id.as_str()).map(|bounds| (row, bounds)))
                    .next()
            })
            .or_else(|| {
                // Front eviction can remove every visible exchange. Keep the
                // nearest surviving boundary instead of jumping to live output.
                self.rows
                    .iter()
                    .filter_map(|row| next_rows.get(row.id.as_str()).map(|bounds| (row, bounds)))
                    .min_by(|(a, _), (b, _)| {
                        (a.bounds.y - top)
                            .abs()
                            .total_cmp(&(b.bounds.y - top).abs())
                    })
            });
        overlap.map_or(next.travel(), |(old, new)| {
            let within = top - old.bounds.y;
            // A disclosure or reflow may shorten the anchored exchange itself.
            // Do not carry an offset past its new end into an unrelated turn.
            let within = if within > 0.0 {
                within.min(new.height.max(0.0))
            } else {
                within
            };
            (new.y + within).clamp(0.0, next.travel())
        })
    }

    fn remember(&mut self, next: Geometry, rows: &[(&str, Rectangle)], top: f32) {
        for (index, (id, bounds)) in rows.iter().enumerate() {
            if let Some(row) = self.rows.get_mut(index) {
                if row.id != *id {
                    row.id = (*id).to_owned();
                }
                row.bounds = *bounds;
            } else {
                self.rows.push(Row {
                    id: (*id).to_owned(),
                    bounds: *bounds,
                });
            }
        }
        self.rows.truncate(rows.len());
        self.geometry = Some(next);
        self.top = Some(top);
    }
}

#[derive(PartialEq)]
struct Report {
    subagent: Option<String>,
    top: f32,
    geometry: Geometry,
}

#[derive(Default)]
struct Anchor {
    contexts: Vec<Context>,
    active: Option<usize>,
    clock: u64,
    reported: Option<Report>,
    pending_input: bool,
    modifiers: iced::keyboard::Modifiers,
    seed: Option<Scroll>,
}

impl Anchor {
    fn geometry(&self) -> Option<Geometry> {
        self.active.and_then(|index| self.contexts[index].geometry)
    }

    #[cfg(test)]
    fn layout(
        &mut self,
        subagent: Option<&str>,
        rows: &[(&str, Rectangle)],
        next: Geometry,
        current_top: Option<f32>,
    ) -> Option<f32> {
        self.layout_following(subagent, rows, next, current_top, true)
    }

    fn layout_following(
        &mut self,
        subagent: Option<&str>,
        rows: &[(&str, Rectangle)],
        next: Geometry,
        current_top: Option<f32>,
        follow_end: bool,
    ) -> Option<f32> {
        if let Some(index) = self.active
            && let Some(top) = current_top
        {
            self.contexts[index].top = Some(top);
        }
        let previous_active = self.active;
        let index = self
            .contexts
            .iter()
            .position(|context| context.subagent.as_deref() == subagent)
            .unwrap_or_else(|| {
                // Main is never evicted. Bound retained subagent positions even
                // when a long-lived session launches hundreds of workers.
                if self
                    .contexts
                    .iter()
                    .filter(|context| context.subagent.is_some())
                    .count()
                    >= 32
                {
                    let oldest = self
                        .contexts
                        .iter()
                        .enumerate()
                        .filter(|(_, context)| context.subagent.is_some())
                        .min_by_key(|(_, context)| context.used)
                        .unwrap()
                        .0;
                    self.contexts[oldest] = Context {
                        subagent: subagent.map(str::to_owned),
                        ..Context::default()
                    };
                    oldest
                } else {
                    self.contexts.push(Context {
                        subagent: subagent.map(str::to_owned),
                        ..Context::default()
                    });
                    self.contexts.len() - 1
                }
            });
        self.clock += 1;
        let seed = self.seed.take();
        let context = &mut self.contexts[index];
        if context.geometry.is_none()
            && let Some(seed) = seed
        {
            context.geometry = Some(Geometry {
                viewport: Size::new(next.viewport.width, seed.window_height()),
                content: Size::new(next.content.width, seed.content()),
            });
            context.top = Some(seed.from_top());
            context.follow_end = seed.at_bottom() && follow_end;
        }
        let switched = previous_active != Some(index) || context.geometry.is_none();
        let old_bottom = context
            .geometry
            .zip(context.top)
            .map(|(geometry, top)| (geometry.travel() - top).max(0.0));
        // Relative offsets (notably Home's Relative(1)) must become absolute
        // after reflow even when the desired bottom distance is unchanged.
        let freeze_reading = old_bottom.is_some_and(|bottom| bottom > 0.5)
            && context.geometry.is_some_and(|previous| previous != next);
        let top = context.position(next, rows, follow_end);
        context.follow_end = follow_end;
        let bottom = next.travel() - top;
        context.used = self.clock;
        context.remember(next, rows, top);
        self.active = Some(index);
        if switched {
            self.pending_input = false;
        }
        (switched || freeze_reading || old_bottom.is_none_or(|old| (old - bottom).abs() > 0.5))
            .then_some(bottom)
    }
}

/// Wheel direction also matters when a notch moves *away* from a nearby
/// threshold, or when it cannot move at a pinned edge after a failed request.
fn navigation_input(
    before: Option<f32>,
    top: f32,
    scroll: Scroll,
    wheel: Option<f32>,
    child_captured: bool,
) -> bool {
    match wheel {
        Some(y) => {
            !child_captured
                && ((y > 0.0 && scroll.at_top() && before.is_none_or(|old| top <= old))
                    || (y < 0.0 && scroll.at_bottom() && before.is_none_or(|old| top >= old)))
        }
        None => before.is_some_and(|old| {
            old != top && (!scroll.at_top() || top < old) && (!scroll.at_bottom() || top > old)
        }),
    }
}

/// The native scrollable updates its content first. Record whether the
/// content consumed the event before the outer scrollable sees it, so nested
/// tool output never triggers conversation pagination.
struct InputCapture<'a> {
    content: Element<'a, Message>,
    captured: Rc<Cell<bool>>,
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for InputCapture<'_> {
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content.as_widget().a11y_nodes(layout, tree, cursor)
    }

    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }
    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }
    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }
    fn diff(&mut self, tree: &mut Tree) {
        self.content.as_widget_mut().diff(tree);
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
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
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
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
        self.captured.set(shell.is_event_captured());
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
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, cosmic::Theme, cosmic::Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }
}

#[derive(Default)]
struct ReadTop(Option<f32>);

impl Operation for ReadTop {
    fn traverse(&mut self, _: &mut dyn FnMut(&mut dyn Operation)) {}

    fn scrollable(
        &mut self,
        _: Option<&Id>,
        _: Rectangle,
        _: Rectangle,
        translation: Vector,
        _: &mut dyn operation::Scrollable,
    ) {
        self.0 = Some(translation.y);
    }
}

struct SetBottom(f32);

impl Operation for SetBottom {
    fn traverse(&mut self, _: &mut dyn FnMut(&mut dyn Operation)) {}

    fn scrollable(
        &mut self,
        _: Option<&Id>,
        _: Rectangle,
        _: Rectangle,
        _: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
            x: None,
            y: Some(self.0),
        });
    }
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Anchored<'_> {
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
        tree::Tag::of::<ReaderState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(ReaderState {
            memory: self.memory.clone(),
            restore: true,
        })
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

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<ReaderState>();
        let mut restore = std::mem::take(&mut state.restore);
        let mut anchor = state.memory.0.borrow_mut();
        if restore {
            anchor.pending_input = false;
        }
        if !anchor
            .contexts
            .iter()
            .any(|context| context.subagent.as_deref() == self.subagent)
        {
            anchor.seed = self.observations.as_ref().and_then(|observations| {
                observations
                    .borrow()
                    .get(&self.subagent.map(str::to_owned))
                    .copied()
            });
        }
        let child = &mut tree.children[0];
        let mut top = ReadTop::default();
        if let Some(previous) = anchor.geometry().filter(|_| !restore) {
            self.content.as_widget_mut().operate(
                child,
                Layout::new(&previous.node()),
                renderer,
                &mut top,
            );
        }
        let mut node = self.content.as_widget_mut().layout(child, renderer, limits);
        for _ in 0..4 {
            let next = Geometry {
                viewport: node.size(),
                content: node.children()[0].size(),
            };
            let mut rows: Vec<_> = self
                .anchors
                .iter()
                .filter_map(|(index, id)| {
                    node.children()[0]
                        .children()
                        .get(*index)
                        .map(|child| (*id, child.bounds()))
                })
                .collect();
            let mut ids: std::collections::HashMap<Id, Vec<&str>> = Default::default();
            for (id, item) in &self.items {
                ids.entry(id.clone()).or_default().push(item);
            }
            let wanted = anchor
                .active
                .into_iter()
                .flat_map(|index| anchor.contexts[index].rows.iter())
                .filter_map(|row| {
                    row.id
                        .strip_prefix("virtual-chunk-")
                        .and_then(|key| u64::from_str_radix(key, 16).ok())
                })
                .collect();
            let mut measured = ItemBounds {
                ids,
                rows: Default::default(),
                wanted,
                virtual_rows: vec![],
            };
            self.content.as_widget_mut().operate(
                child,
                Layout::new(&node),
                renderer,
                &mut measured,
            );
            rows.extend(measured.rows);
            rows.extend(
                measured
                    .virtual_rows
                    .iter()
                    .map(|(key, bounds)| (key.as_str(), *bounds)),
            );
            rows.sort_by(|a, b| a.1.y.total_cmp(&b.1.y));
            let offset = anchor
                .layout_following(self.subagent, &rows, next, top.0, self.follow_end)
                .or_else(|| {
                    restore.then(|| {
                        next.travel()
                            - anchor
                                .active
                                .and_then(|index| anchor.contexts[index].top)
                                .unwrap_or_default()
                    })
                });
            if let Some(offset) = offset {
                self.content.as_widget_mut().operate(
                    child,
                    Layout::new(&node),
                    renderer,
                    &mut SetBottom(offset),
                );
                top.0 = Some((next.travel() - offset).max(0.0));
            }
            restore = false;
            let mut virtual_viewport = virtualized::UpdateViewport::default();
            self.content.as_widget_mut().operate(
                child,
                Layout::new(&node),
                renderer,
                &mut virtual_viewport,
            );
            if !virtual_viewport.changed {
                break;
            }
            node = self.content.as_widget_mut().layout(child, renderer, limits);
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
        let mut before = ReadTop::default();
        if self.target.is_some() {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut before,
            );
        }
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
        if self.target.is_some() {
            let mut after = ReadTop::default();
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut after,
            );
            let mut anchor = tree
                .state
                .downcast_ref::<ReaderState>()
                .memory
                .0
                .borrow_mut();
            if let Some((top, geometry)) = after.0.zip(anchor.geometry()) {
                anchor.pending_input |= navigation_input(
                    before.0,
                    top,
                    Scroll::new(top, geometry.content.height, geometry.viewport.height),
                    None,
                    false,
                );
            }
        }
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
        let mut before = ReadTop::default();
        if self.target.is_some() {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut before,
            );
        }
        if let Some(captured) = &self.captured {
            captured.set(false);
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
        let mut virtual_viewport = virtualized::UpdateViewport::default();
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout,
            renderer,
            &mut virtual_viewport,
        );
        if virtual_viewport.changed {
            shell.invalidate_layout();
        }
        if self.target.is_none() {
            return;
        }
        let mut top = ReadTop::default();
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, &mut top);
        if let Some(top) = top.0 {
            let geometry = Geometry {
                viewport: layout.bounds().size(),
                content: layout
                    .children()
                    .next()
                    .expect("transcript content")
                    .bounds()
                    .size(),
            };
            let mut anchor = tree
                .state
                .downcast_ref::<ReaderState>()
                .memory
                .0
                .borrow_mut();
            let changed = anchor.reported.as_ref().is_none_or(|previous| {
                previous.subagent.as_deref() != self.subagent
                    || previous.top != top
                    || previous.geometry != geometry
            });
            let scroll = Scroll::new(top, geometry.content.height, geometry.viewport.height);
            if let Event::Keyboard(iced::keyboard::Event::ModifiersChanged(modifiers)) = event {
                anchor.modifiers = *modifiers;
            }
            // Only navigation toward an edge can request its page. Content
            // scrollables own their gestures, including retries at their ends.
            let wheel = match event {
                Event::Mouse(mouse::Event::WheelScrolled { delta })
                    if cursor.is_over(layout.bounds()) && cursor.is_over(*viewport) =>
                {
                    Some(match *delta {
                        mouse::ScrollDelta::Lines { x, y } => {
                            if anchor.modifiers.shift() && !cfg!(target_os = "macos") {
                                x
                            } else {
                                y
                            }
                        }
                        mouse::ScrollDelta::Pixels { y, .. } => y,
                    })
                }
                _ => None,
            };
            let child_captured = self
                .captured
                .as_ref()
                .is_some_and(|captured| captured.get());
            let user_input = navigation_input(before.0, top, scroll, wheel, child_captured)
                || anchor.pending_input;
            if changed || user_input {
                anchor.pending_input = false;
                anchor.reported = Some(Report {
                    subagent: self.subagent.map(str::to_owned),
                    top,
                    geometry,
                });
                if let Some(observations) = &self.observations {
                    observations
                        .borrow_mut()
                        .insert(self.subagent.map(str::to_owned), scroll);
                }
                // Native geometry is shared directly with the Runtime. Only
                // navigation needs a model message; a streamed height change
                // must not cause another complete update/view/layout cycle.
                if let Some(target) = self
                    .target
                    .filter(|_| self.observations.is_none() || user_input)
                {
                    shell.publish(action(Action::Scrolled(
                        target,
                        self.subagent.map(str::to_owned),
                        Scroll::new(top, geometry.content.height, geometry.viewport.height),
                        user_input,
                    )));
                }
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
    #[test]
    fn input_capture_and_anchor_preserve_accessible_children() {
        let captured = Rc::new(Cell::new(false));
        let child = Element::new(InputCapture {
            content: Element::new(crate::ui_controls::accessibility_probe::Probe),
            captured,
        });
        let element = tool_output(child);
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

    fn geometry(height: f32) -> Geometry {
        Geometry {
            viewport: Size::new(400.0, 400.0),
            content: Size::new(400.0, height),
        }
    }
    fn rows<'a>(entries: &[(&'a str, f32, f32)]) -> Vec<(&'a str, Rectangle)> {
        entries
            .iter()
            .map(|(id, y, height)| {
                (
                    *id,
                    Rectangle {
                        x: 0.0,
                        y: *y,
                        width: 400.0,
                        height: *height,
                    },
                )
            })
            .collect()
    }
    fn reading() -> Anchor {
        let mut anchor = Anchor::default();
        assert_eq!(
            anchor.layout(
                None,
                &rows(&[
                    ("a", 0.0, 500.0),
                    ("b", 500.0, 500.0),
                    ("c", 1000.0, 1000.0)
                ]),
                geometry(2000.0),
                None
            ),
            Some(0.0)
        );
        anchor
    }

    #[test]
    fn dropping_and_recreating_native_trees_preserves_only_the_reader_anchor_memory() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let memory = Memory::default();
        let observations = Observations::default();
        let make = || {
            transcript_observed(
                widget::column::with_children(
                    ["first", "middle", "last"]
                        .into_iter()
                        .map(|id| item(id, widget::Space::new().height(500).width(Length::Fill)))
                        .collect::<Vec<_>>(),
                ),
                target,
                None,
                vec![],
                true,
                ["first", "middle", "last"]
                    .into_iter()
                    .map(|id| (item_id(id), id))
                    .collect(),
                observations.clone(),
                memory.clone(),
            )
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
        {
            let mut element = make();
            let mut tree = Tree::new(&element);
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut SetBottom(500.0),
            );
            element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
        }
        assert!(
            memory.retained_rows() > 0,
            "only stable row IDs and geometry survive tree drop"
        );
        let mut recreated = make();
        let mut tree = Tree::new(&recreated);
        let node = recreated
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut actual = ReadTop::default();
        recreated
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        assert_eq!(
            actual.0,
            Some(600.0),
            "revealing a hidden transcript does not jump to its end"
        );
    }

    #[test]
    fn logical_find_realizes_a_distant_chunk_and_centers_its_mounted_glyph() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let identity = 9876;
        let line = "measured source line αβ\n";
        let source: Arc<str> = line.repeat(5000).into();
        let byte = line.len() * 1500;
        let text = prose::code_text(
            source,
            Arc::from([]),
            design::TEXT_BODY.into(),
            iced::Font::MONOSPACE,
            target,
            None,
            Some(selectable_prose::Logical { identity, start: 0 }),
        );
        let mut cases = vec![(text, identity, byte)];
        let list = format!(
            "{}{}",
            "prefix\n\n".repeat(4),
            (0..10000)
                .map(|index| format!("- bullet {index} target\n"))
                .collect::<String>()
        );
        let table = format!(
            "{}| First | Second |\n| --- | --- |\n{}",
            "prefix\n\n".repeat(4),
            (0..10000)
                .map(|index| format!("| row {index} | target {index} |\n"))
                .collect::<String>()
        );
        for (source, path) in [
            (list, vec![0, 4, 9999, 0]),
            (table, vec![0, 4, 10000, 1, 0]),
        ] {
            let cached = CachedItem::new(
                &ItemContent::Text {
                    text: source,
                    thinking: false,
                    phase: chat::Phase::Final,
                },
                crate::settings::EditorSyntaxTheme::Auto,
            );
            let identity =
                selection::logical_identity(&format!("unbound-{}", cached.revision), &path);
            cases.push((
                prose::view(
                    &cached,
                    design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE),
                    target,
                    None,
                ),
                identity,
                0,
            ));
        }
        for (text, identity, byte) in cases {
            let before = virtualized::constructed_rows();
            let mut element = transcript(
                widget::column::with_children(vec![
                    widget::Space::new().height(10000).into(),
                    item("answer", text),
                    widget::Space::new().height(10000).into(),
                ]),
                target,
                None,
                vec![],
                true,
                vec![],
            );
            let mut tree = Tree::new(&element);
            let viewport = Rectangle::with_size(Size::new(720.0, 400.0));
            let limits = layout::Limits::new(Size::ZERO, viewport.size());
            let mut node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut SetBottom(f32::MAX),
            );
            node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut exact = false;
            for _ in 0..6 {
                let found = Arc::new(std::sync::Mutex::new(None));
                let sink = found.clone();
                let mut locate = operation::map(
                    LocateLogical {
                        scroll: card_control_id(target.card, "chat-transcript"),
                        item: item_id("answer"),
                        identity,
                        byte,
                        content: None,
                        viewport: None,
                        found: None,
                    },
                    move |location| {
                        *sink.lock().unwrap() = location;
                    },
                );
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut locate,
                );
                let _ = locate.finish();
                let location = found
                    .lock()
                    .unwrap()
                    .take()
                    .expect("offscreen descriptors remain navigable");
                exact = location.exact;
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut SetBottom(location.offset),
                );
                node = element
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
                if exact {
                    break;
                }
            }
            assert!(
                exact,
                "estimate navigation converges to actual mounted glyph geometry"
            );
            struct Visible {
                identity: u64,
                byte: usize,
                translation: Vector,
                viewport: Option<Rectangle>,
                glyph: Option<Rectangle>,
            }
            impl Operation for Visible {
                fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                    visit(self);
                }
                fn scrollable(
                    &mut self,
                    _: Option<&Id>,
                    viewport: Rectangle,
                    _: Rectangle,
                    translation: Vector,
                    _: &mut dyn operation::Scrollable,
                ) {
                    self.translation = translation;
                    self.viewport = Some(viewport);
                }
                fn custom(
                    &mut self,
                    _: Option<&Id>,
                    bounds: Rectangle,
                    state: &mut dyn std::any::Any,
                ) {
                    if let Some(prose) = state.downcast_ref::<selectable_prose::State>()
                        && let Some(local) = prose.logical_byte_bounds(self.identity, self.byte)
                    {
                        self.glyph = Some(Rectangle {
                            x: bounds.x + local.x - self.translation.x,
                            y: bounds.y + local.y - self.translation.y,
                            ..local
                        });
                    }
                }
            }
            let mut visible = Visible {
                identity,
                byte,
                translation: Vector::ZERO,
                viewport: None,
                glyph: None,
            };
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut visible);
            let glyph = visible.glyph.expect("the requested chunk is mounted");
            let viewport = visible.viewport.unwrap();
            assert!(
                glyph.y >= viewport.y && glyph.y + glyph.height <= viewport.y + viewport.height
            );
            assert!(
                (glyph.y + glyph.height / 2.0 - viewport.y - viewport.height / 2.0).abs()
                    <= glyph.height + 1.0,
                "mounted match glyph is centered: {glyph:?}, {viewport:?}"
            );
            assert!(
                virtualized::constructed_rows() - before < 1500,
                "staged navigation must not instantiate ten thousand hidden rows"
            );
        }
    }

    #[test]
    fn native_geometry_updates_the_reader_without_echoing_a_model_message() {
        let renderer = crate::app::tests::headless_renderer();
        let observations: Observations = Default::default();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let make = |height| {
            transcript_observed(
                widget::Space::new().height(height).width(Length::Fill),
                target,
                None,
                vec![],
                true,
                vec![],
                observations.clone(),
                Memory::default(),
            )
        };
        let bounds = Rectangle::with_size(Size::new(400.0, 400.0));
        let limits = layout::Limits::new(Size::ZERO, bounds.size());
        let mut element = make(100.0);
        let mut tree = Tree::new(&element);
        let mut messages = Vec::new();
        for height in [100.0, 2000.0, 4000.0] {
            element = make(height);
            tree.diff(element.as_widget_mut());
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &bounds,
            );
            assert!(
                messages.is_empty(),
                "streamed geometry does not enqueue another model/view cycle"
            );
            let scroll = observations.borrow()[&None];
            assert_eq!(scroll.content(), height);
            assert!(
                scroll.at_bottom(),
                "the direct observation keeps authoritative native geometry"
            );
        }
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            }),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            messages.iter().any(|message| matches!(
                message,
                Message::ChatUi(Action::Scrolled(_, None, _, true))
            )),
            "user navigation still drives paging"
        );
    }

    #[test]
    fn native_layout_reports_fitting_content_and_anchors_real_column_children() {
        use iced::advanced::renderer::Headless;
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cosmic::Renderer::new(
                iced::Font::default(),
                iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let element = |entries: Vec<(&'static str, f32)>| {
            let anchors = entries
                .iter()
                .enumerate()
                .map(|(index, (id, _))| (index, *id))
                .collect();
            let children: Vec<Element<'static, Message>> = entries
                .into_iter()
                .map(|(_, height)| {
                    widget::Space::new()
                        .width(Length::Fill)
                        .height(height)
                        .into()
                })
                .collect();
            transcript(
                widget::column::with_children(children).width(Length::Fill),
                target,
                None,
                anchors,
                true,
                Vec::new(),
            )
        };
        let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(400.0, 400.0));
        let limits = layout::Limits::new(Size::ZERO, bounds.size());
        let mut fitting = element(vec![("short", 100.0)]);
        let mut tree = Tree::new(fitting.as_widget());
        let node = fitting
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut messages = Vec::new();
        let event = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
        fitting.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(messages.as_slice(), [Message::ChatUi(Action::Scrolled(actual, None, scroll, false))]
            if *actual == target && *scroll == Scroll::new(0.0, 100.0, 400.0))
        );

        messages.clear();
        fitting.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            messages.is_empty(),
            "unchanged geometry must not trigger automatic paging loops"
        );
        fitting.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            }),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(
                messages.as_slice(),
                [Message::ChatUi(Action::Scrolled(_, None, _, true))]
            ),
            "a wheel gesture at a pinned edge must permit retry"
        );

        let mut initial = element(vec![("a", 500.0), ("b", 500.0), ("c", 1000.0)]);
        let mut tree = Tree::new(initial.as_widget());
        let node = initial
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        initial.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(1000.0),
        );
        initial.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        messages.clear();
        let mut updated = element(vec![
            ("older", 300.0),
            ("a", 500.0),
            ("b", 500.0),
            ("c", 1200.0),
        ]);
        tree.diff(updated.as_widget_mut());
        let node = updated
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut measured = ReadTop::default();
        updated
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut measured);
        assert_eq!(measured.0, Some(900.0));
        messages.clear();
        updated.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(messages.as_slice(), [Message::ChatUi(Action::Scrolled(_, None, scroll, false))]
            if *scroll == Scroll::new(900.0, 2500.0, 400.0))
        );

        // Equal-height replacement changes the offset through layout, not
        // through a gesture. Treating it as input bounces older pages to live.
        let mut replaced = element(vec![("d", 500.0), ("e", 500.0), ("f", 1500.0)]);
        tree.diff(replaced.as_widget_mut());
        let node = replaced
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        messages.clear();
        replaced.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(messages.as_slice(), [Message::ChatUi(Action::Scrolled(_, None, scroll, false))]
            if *scroll == Scroll::new(2100.0, 2500.0, 400.0))
        );
        let mut home = operation::scrollable::snap_to::<()>(
            card_control_id(target.card, "chat-transcript"),
            iced::widget::scrollable::RelativeOffset {
                x: None,
                y: Some(1.0),
            },
        );
        replaced
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut home);
        messages.clear();
        replaced.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(messages.as_slice(), [Message::ChatUi(Action::Scrolled(_, None, scroll, true))]
            if *scroll == Scroll::new(0.0, 2500.0, 400.0)),
            "Home's external operation is input"
        );
        messages.clear();
        replaced.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(messages.is_empty(), "input signal must be consumed once");
        replaced.as_widget_mut().update(
            &mut tree,
            &Event::Keyboard(iced::keyboard::Event::ModifiersChanged(
                iced::keyboard::Modifiers::SHIFT,
            )),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        replaced.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            }),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            messages.is_empty(),
            "Shift-wheel remaps to horizontal input and must not page"
        );
    }

    #[test]
    fn nested_output_consumes_wheel_without_paging_transcript() {
        use iced::advanced::renderer::Headless;
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cosmic::Renderer::new(
                iced::Font::default(),
                iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let nested = tool_output(
            widget::scrollable(widget::Space::new().height(1000.0))
                .height(200.0)
                .width(Length::Fill)
                .anchor_bottom(),
        );
        let mut element = transcript(
            widget::column::with_children(vec![nested, widget::Space::new().height(1400.0).into()])
                .width(Length::Fill),
            target,
            None,
            vec![],
            true,
            Vec::new(),
        );
        let mut tree = Tree::new(element.as_widget());
        let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(400.0, 400.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(1200.0),
        );
        let mut messages = Vec::new();
        let mut clipboard = iced::advanced::clipboard::Null;
        // Consume the deliberate outer navigation before wheeling its child.
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::CursorMoved {
                position: iced::Point::new(50.0, 50.0),
            }),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
            &renderer,
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        messages.clear();
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            }),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
            &renderer,
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            messages.is_empty(),
            "output scroll must not become outer pagination: {messages:?}"
        );
        let mut measured = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut measured);
        assert_eq!(measured.0, Some(0.0));
        // The same upward gesture over plain transcript is a retry at its edge.
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            }),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(50.0, 300.0)),
            &renderer,
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(matches!(
            messages.as_slice(),
            [Message::ChatUi(Action::Scrolled(_, None, _, true))]
        ));
    }

    #[test]
    fn wheel_navigation_respects_direction_and_nested_capture() {
        let top = Scroll::new(10.0, 1000.0, 400.0);
        assert!(navigation_input(Some(20.0), 10.0, top, Some(1.0), false));
        assert!(!navigation_input(Some(0.0), 10.0, top, Some(-1.0), false));
        assert!(!navigation_input(Some(10.0), 10.0, top, Some(1.0), true));
        let bottom = Scroll::new(590.0, 1000.0, 400.0);
        assert!(navigation_input(
            Some(580.0),
            590.0,
            bottom,
            Some(-1.0),
            false
        ));
        assert!(!navigation_input(
            Some(600.0),
            590.0,
            bottom,
            Some(1.0),
            false
        ));
        assert!(!navigation_input(
            Some(590.0),
            590.0,
            bottom,
            Some(-1.0),
            true
        ));
        assert!(navigation_input(Some(100.0), 200.0, bottom, None, false));
        assert!(!navigation_input(Some(10.0), 10.0, top, Some(0.0), false));
    }

    #[test]
    fn thumb_touch_and_keyboard_moves_away_from_edges_do_not_page() {
        let top = Scroll::new(10.0, 1000.0, 400.0);
        assert!(!navigation_input(Some(0.0), 10.0, top, None, false));
        assert!(navigation_input(Some(20.0), 10.0, top, None, false));
        let bottom = Scroll::new(590.0, 1000.0, 400.0);
        assert!(!navigation_input(Some(600.0), 590.0, bottom, None, false));
        assert!(navigation_input(Some(580.0), 590.0, bottom, None, false));
    }

    #[test]
    fn fitting_page_preserves_exchange_when_both_ends_grow() {
        let mut anchor = Anchor::default();
        anchor.layout(None, &rows(&[("short", 0.0, 300.0)]), geometry(300.0), None);
        let next = rows(&[
            ("older", 0.0, 500.0),
            ("short", 500.0, 300.0),
            ("newer", 800.0, 600.0),
        ]);
        assert_eq!(
            anchor.layout(None, &next, geometry(1400.0), Some(0.0)),
            Some(500.0)
        );
        assert_eq!(anchor.contexts[0].top, Some(500.0));
    }

    #[test]
    fn simultaneous_prepend_and_streaming_append_preserve_visible_exchange() {
        let next = rows(&[
            ("older", 0.0, 300.0),
            ("a", 300.0, 500.0),
            ("b", 800.0, 500.0),
            ("c", 1300.0, 1200.0),
        ]);
        assert_eq!(
            reading().layout(None, &next, geometry(2500.0), Some(600.0)),
            Some(1200.0)
        );
    }

    #[test]
    fn prepend_at_home_freezes_the_previous_first_exchange() {
        let mut anchor = reading();
        let next = rows(&[
            ("older", 0.0, 300.0),
            ("a", 300.0, 500.0),
            ("b", 800.0, 500.0),
            ("c", 1300.0, 1000.0),
        ]);
        assert_eq!(
            anchor.layout(None, &next, geometry(2300.0), Some(0.0)),
            Some(1600.0)
        );
        assert_eq!(
            anchor.layout(None, &next, geometry(2300.0), Some(300.0)),
            None
        );
    }

    #[test]
    fn growth_above_and_front_eviction_preserve_surviving_rows() {
        let next = rows(&[
            ("a", 0.0, 800.0),
            ("b", 800.0, 500.0),
            ("c", 1300.0, 1000.0),
        ]);
        assert_eq!(
            reading().layout(None, &next, geometry(2300.0), Some(600.0)),
            Some(1000.0)
        );
        let next = rows(&[("b", 0.0, 500.0), ("c", 500.0, 1000.0)]);
        assert_eq!(
            reading().layout(None, &next, geometry(1500.0), Some(600.0)),
            Some(1000.0)
        );
        assert_eq!(
            reading().layout(None, &next, geometry(1500.0), Some(100.0)),
            Some(1100.0)
        );
    }

    #[test]
    fn viewport_resize_and_live_end_use_the_current_native_offset() {
        let next = rows(&[
            ("a", 0.0, 500.0),
            ("b", 500.0, 500.0),
            ("c", 1000.0, 1200.0),
        ]);
        let mut resized = geometry(2200.0);
        resized.viewport = Size::new(300.0, 600.0);
        assert_eq!(
            reading().layout(None, &next, resized, Some(900.0)),
            Some(700.0)
        );
        assert_eq!(reading().layout(None, &next, resized, Some(1600.0)), None);
    }

    #[test]
    fn shortened_exchange_clamps_its_internal_offset() {
        let next = rows(&[("a", 0.0, 500.0), ("b", 500.0, 50.0), ("c", 550.0, 1000.0)]);
        assert_eq!(
            reading().layout(None, &next, geometry(1550.0), Some(800.0)),
            Some(600.0)
        );
    }

    #[test]
    fn separate_transcripts_restore_positions_and_have_deliberate_initial_edges() {
        let mut anchor = reading();
        let child = rows(&[("child", 0.0, 1600.0)]);
        assert_eq!(
            anchor.layout(Some("worker"), &child, geometry(1600.0), Some(600.0)),
            Some(1200.0)
        );
        let main = rows(&[
            ("a", 0.0, 500.0),
            ("b", 500.0, 500.0),
            ("c", 1000.0, 1200.0),
        ]);
        assert_eq!(
            anchor.layout(None, &main, geometry(2200.0), Some(300.0)),
            Some(1200.0)
        );
        assert_eq!(
            anchor.layout(Some("worker"), &child, geometry(1600.0), Some(600.0)),
            Some(900.0)
        );
    }

    #[test]
    fn short_subagent_content_preserves_its_initial_top_when_it_grows() {
        let mut anchor = Anchor::default();
        let initial = rows(&[("child", 0.0, 100.0)]);
        assert_eq!(
            anchor.layout(Some("worker"), &initial, geometry(100.0), None),
            Some(0.0)
        );
        let grown = rows(&[("child", 0.0, 1600.0)]);
        assert_eq!(
            anchor.layout(Some("worker"), &grown, geometry(1600.0), Some(0.0)),
            Some(1200.0)
        );
        assert_eq!(
            anchor.layout(
                Some("worker"),
                &rows(&[("child", 0.0, 1800.0)]),
                geometry(1800.0),
                Some(1200.0)
            ),
            None,
            "End explicitly follows the live end after content becomes scrollable"
        );
    }

    #[test]
    fn bounded_contexts_keep_main_even_after_many_workers() {
        let mut anchor = reading();
        let child = rows(&[("child", 0.0, 1600.0)]);
        for i in 0..64 {
            anchor.layout(
                Some(&format!("worker-{i}")),
                &child,
                geometry(1600.0),
                Some(600.0),
            );
        }
        assert_eq!(anchor.contexts.len(), 33);
        let main = rows(&[
            ("a", 0.0, 500.0),
            ("b", 500.0, 500.0),
            ("c", 1000.0, 1000.0),
        ]);
        assert_eq!(
            anchor.layout(None, &main, geometry(2000.0), Some(300.0)),
            Some(1000.0)
        );
    }

    #[test]
    fn tool_output_without_message_anchors_preserves_reading_offset() {
        let mut anchor = Anchor::default();
        assert_eq!(anchor.layout(None, &[], geometry(2000.0), None), Some(0.0));
        assert_eq!(
            anchor.layout(None, &[], geometry(2200.0), Some(600.0)),
            Some(1200.0)
        );
        assert_eq!(
            anchor.layout(None, &[], geometry(2400.0), Some(1800.0)),
            None
        );
    }

    #[test]
    fn rebuilt_named_anchors_match_the_active_widget_id_hash_map() {
        // This uses Iced's selected ID implementation, including its a11y
        // reexport, rather than an unused feature-gated copy of the type.
        let first = item_id("persisted-answer");
        let rebuilt = item_id("persisted-answer");
        assert_eq!(first, rebuilt);
        let mut anchors = std::collections::HashMap::new();
        anchors.insert(first, 300.0);
        assert_eq!(anchors.get(&rebuilt), Some(&300.0));
    }

    #[test]
    fn regrouped_answer_and_tool_keep_their_measured_reading_position() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let element = |entries: Vec<(&'static str, f32)>, group| {
            let ids = entries.iter().map(|(id, _)| (item_id(id), *id)).collect();
            let children: Vec<Element<'static, Message>> = entries
                .into_iter()
                .map(|(id, height)| {
                    item(id, widget::Space::new().width(Length::Fill).height(height))
                })
                .collect();
            transcript(
                widget::container(widget::column::with_children(children)).id(Id::new(group)),
                target,
                None,
                Vec::new(),
                false,
                ids,
            )
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
        for top in [450.0, 850.0] {
            let mut initial = element(
                vec![("prompt", 300.0), ("answer", 500.0), ("tool", 600.0)],
                "old-group",
            );
            let mut tree = Tree::new(&initial);
            let node = initial
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            initial.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut SetBottom(1000.0 - top),
            );
            // A second layout captures the native reading position before the
            // prompt is evicted and the answer becomes the group's first row.
            initial
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let memory = tree.state.downcast_ref::<ReaderState>().memory.0.borrow();
            let context = &memory.contexts[0];
            assert_eq!(
                context
                    .rows
                    .iter()
                    .find(|row| row.id == "answer")
                    .map(|row| row.bounds.y),
                Some(300.0)
            );
            drop(memory);
            let mut next = element(
                vec![("answer", 500.0), ("tool", 600.0), ("newer", 1000.0)],
                "new-group",
            );
            tree.diff(next.as_widget_mut());
            let node = next.as_widget_mut().layout(&mut tree, &renderer, &limits);
            let mut actual = ReadTop::default();
            next.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
            let memory = tree.state.downcast_ref::<ReaderState>().memory.0.borrow();
            let context = &memory.contexts[0];
            assert_eq!(
                actual.0,
                Some(top - 300.0),
                "rows={:?}, geometry={:?}, remembered={:?}",
                context
                    .rows
                    .iter()
                    .map(|row| (&row.id, row.bounds))
                    .collect::<Vec<_>>(),
                context.geometry,
                context.top
            );
        }
    }

    #[test]
    fn forward_history_preserves_boundary_instead_of_following_new_page_end() {
        let mut anchor = Anchor::default();
        let previous = rows(&[("older", 0.0, 800.0), ("boundary", 800.0, 500.0)]);
        anchor.layout_following(None, &previous, geometry(1300.0), None, false);
        let next = rows(&[("boundary", 0.0, 500.0), ("newer", 500.0, 1200.0)]);
        assert_eq!(
            anchor.layout_following(None, &next, geometry(1700.0), Some(900.0), true),
            Some(1200.0)
        );
        assert_eq!(anchor.contexts[0].top, Some(100.0));
    }

    #[test]
    fn backward_history_from_live_bottom_preserves_overlapping_exchange() {
        let mut anchor = Anchor::default();
        let previous = rows(&[("boundary", 0.0, 500.0), ("live", 500.0, 700.0)]);
        anchor.layout_following(None, &previous, geometry(1200.0), None, true);
        let next = rows(&[("older", 0.0, 700.0), ("boundary", 700.0, 500.0)]);
        assert_eq!(
            anchor.layout_following(None, &next, geometry(1200.0), Some(0.0), false),
            Some(100.0)
        );
        assert_eq!(anchor.contexts[0].top, Some(700.0));
    }

    #[test]
    fn entirely_replaced_window_starts_at_its_newest_boundary() {
        assert_eq!(
            reading().layout(
                None,
                &rows(&[("older", 0.0, 900.0)]),
                geometry(900.0),
                Some(0.0)
            ),
            Some(0.0)
        );
    }
}
