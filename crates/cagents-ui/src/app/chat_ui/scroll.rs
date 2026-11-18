//! Layout-time anchoring around iced's native scrollable. Its own state remains
//! authoritative for wheel, thumb, keyboard and touch input.
mod block;
mod viewport;
pub(super) use block::{block, block_source};
pub(super) use viewport::{minimum_width, viewport};

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
#[cfg(test)]
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
        false,
        HashSet::new(),
        None,
        History::default(),
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Edge {
    Start,
    End,
    Latest,
}

// One accepted main-history fetch may outlive the reader intent that asked
// for it. Keep its source epoch, never its old source or native widgets.
#[derive(Clone, Copy)]
struct PreserveRefresh {
    request: u64,
    generation: u64,
    source: u64,
    arrived: bool,
    latest: bool,
}

struct ReaderState {
    memory: Memory,
    restore: bool,
    revealing: bool,
    needs_layout: bool,
}

impl Memory {
    pub(super) fn edge(&self, subagent: Option<&str>) -> Option<Edge> {
        self.0
            .borrow()
            .contexts
            .iter()
            .find(|context| context.subagent.as_deref() == subagent)
            .and_then(|context| context.edge)
    }
    pub(super) fn request_edge(&self, subagent: Option<&str>, edge: Edge) {
        let mut anchor = self.0.borrow_mut();
        let index = anchor.context_index(subagent);
        anchor.clock += 1;
        anchor.contexts[index].used = anchor.clock;
        anchor.contexts[index].edge = Some(edge);
    }
    pub(super) fn clear_latest(&self) {
        if let Some(context) = self
            .0
            .borrow_mut()
            .contexts
            .iter_mut()
            .find(|context| context.subagent.is_none() && context.edge == Some(Edge::Latest))
        {
            context.edge = None;
        }
    }
    pub(super) fn preserve_refresh(&self, request: u64, generation: u64) {
        let mut anchor = self.0.borrow_mut();
        let index = anchor.context_index(None);
        anchor.contexts[index].preserve = Some(PreserveRefresh {
            request,
            generation,
            source: generation,
            arrived: false,
            latest: false,
        });
    }
    pub(super) fn latest_source(
        &self,
        expected: Option<u64>,
        generation: u64,
        has_later: bool,
        loading: bool,
    ) {
        if let Some(context) = self
            .0
            .borrow_mut()
            .contexts
            .iter_mut()
            .find(|context| context.subagent.is_none())
            && let Some(preserve) = &mut context.preserve
            && expected.is_none_or(|expected| expected == preserve.generation)
            && generation > preserve.source
        {
            preserve.source = generation;
            preserve.arrived = true;
            preserve.latest = !has_later && !loading;
        }
    }
    pub(super) fn cancel_preserve(&self, request: u64) {
        if let Some(context) = self
            .0
            .borrow_mut()
            .contexts
            .iter_mut()
            .find(|context| context.subagent.is_none())
            && context
                .preserve
                .is_some_and(|preserve| preserve.request == request)
        {
            context.preserve = None;
        }
    }
    pub(super) fn preserve_arrived(&self, subagent: Option<&str>) -> bool {
        self.0
            .borrow()
            .contexts
            .iter()
            .find(|context| context.subagent.as_deref() == subagent)
            .is_some_and(|context| context.preserve.is_some_and(|preserve| preserve.arrived))
    }
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
    pub fn reader_width_for(
        &self,
        context: Option<&str>,
        identity: u64,
        source: &super::reader_source::Source,
    ) -> f32 {
        self.1
            .borrow()
            .entries
            .get(&(context.map(str::to_owned), identity))
            .filter(|saved| {
                saved
                    .source
                    .is_some_and(|proof| source.accepts(proof.digest))
            })
            .map_or(0.0, |saved| saved.content_width)
            .max(0.0)
    }
    pub(super) fn reader_proofs(
        &self,
        context: Option<&str>,
    ) -> Arc<HashMap<u64, reader_source::Proof>> {
        Arc::new(
            self.1
                .borrow()
                .entries
                .iter()
                .filter(|((saved_context, _), _)| saved_context.as_deref() == context)
                .filter_map(|((_, identity), saved)| saved.source.map(|proof| (*identity, proof)))
                .collect(),
        )
    }
    /// Advance only the exact bookmark that the worker verified. This changes
    /// no geometry, retains no bodies, and cannot bless another logical reader.
    pub(super) fn verified_readers(
        &self,
        context: Option<&str>,
        proofs: HashMap<u64, (reader_source::Proof, reader_source::Proof)>,
    ) {
        let mut positions = self.1.borrow_mut();
        for (identity, (current, previous)) in proofs {
            if let Some(saved) = positions
                .entries
                .get_mut(&(context.map(str::to_owned), identity))
                && saved.source == Some(previous)
            {
                saved.source = Some(current);
            }
        }
    }
    /// Only cold sources intersecting the saved reading window can delay its
    /// restoration. An unrelated newly preparing tail must not hold it hostage.
    pub(super) fn restoration_pending(
        &self,
        context: Option<&str>,
        evicted: bool,
        mut pending: impl FnMut(&str) -> bool,
    ) -> bool {
        let anchor = self.0.borrow();
        let Some(saved) = anchor
            .contexts
            .iter()
            .find(|saved| saved.subagent.as_deref() == context)
        else {
            return false;
        };
        if evicted && (saved.geometry.is_some() || saved.edge.is_some()) {
            return true;
        }
        saved.shown_rows().any(|row| pending(&row.id))
    }

    /// The rows the saved reading window showed, which a returning reader
    /// sees first. There are none without saved geometry, such as at the
    /// latest edge.
    pub(super) fn reading_rows(&self, context: Option<&str>) -> std::collections::HashSet<String> {
        self.0
            .borrow()
            .contexts
            .iter()
            .find(|saved| saved.subagent.as_deref() == context)
            .map(|saved| saved.shown_rows().map(|row| row.id.clone()).collect())
            .unwrap_or_default()
    }

    /// A reader left `context` with `rows` stacked in its viewport.
    #[cfg(test)]
    pub(super) fn save_window(&self, context: Option<&str>, rows: &[&str]) {
        const HEIGHT: f32 = 100.0;
        let mut anchor = self.0.borrow_mut();
        let index = anchor.context_index(context);
        let saved = &mut anchor.contexts[index];
        saved.geometry = Some(Geometry {
            viewport: Size::new(720.0, HEIGHT * rows.len() as f32),
            content: Size::new(720.0, HEIGHT * (rows.len() + 10) as f32),
        });
        saved.top = Some(0.0);
        saved.rows = rows
            .iter()
            .enumerate()
            .map(|(index, id)| Row {
                id: (*id).to_owned(),
                bounds: Rectangle::new(
                    iced::Point::new(0.0, HEIGHT * index as f32),
                    Size::new(720.0, HEIGHT),
                ),
                placeholder: false,
            })
            .collect();
    }

    #[cfg(test)]
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
    restoration_pending: bool,
    preparing: HashSet<&'a str>,
    find_intent: Option<find_navigation::Intent>,
    history: History,
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
        restoration_pending,
        preparing,
        find_intent,
        history,
    )
}

/// The history around the loaded window that the transcript stands in for.
#[derive(Clone, Copy, Default)]
pub(super) struct History {
    /// Whether there is history before and after the loaded window.
    pub pages: Pages,
    /// The item a page slides the window around.
    pub pivot: Option<Pivot>,
}

/// The item a page slides the window around, by its place among the
/// transcript's first `items` ids, which are its items in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Pivot {
    pub index: usize,
    pub items: usize,
}

/// Which pages of history beyond the loaded window there are to fetch.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) struct Pages {
    pub earlier: bool,
    pub later: bool,
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
    restoration_pending: bool,
    preparing: HashSet<&'a str>,
    find_intent: Option<find_navigation::Intent>,
    history: History,
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
    // The viewport ends in the open gap above the composer, and below the
    // title or the pane's top, so a line crossing either edge fades into the
    // pane rather than being sliced at a boundary nothing on screen draws.
    // The fade wraps the scrollable alone: the reader's anchoring sees the
    // scrollable's own layout through it.
    let content = crate::interaction::overflow_fade_y(content, design::base);
    Element::new(Anchored {
        content,
        captured: Some(captured),
        target: Some(target),
        subagent,
        anchors,
        follow_end,
        items,
        observations,
        memory,
        restoration_pending,
        preparing,
        find_intent,
        history,
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
        restoration_pending: false,
        preparing: HashSet::new(),
        find_intent: None,
        history: History::default(),
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
#[derive(Clone, Debug)]
pub struct LogicalLocation {
    pub offset: f32,
    pub exact: bool,
    pub stage: u8,
    pub(super) shape_pending: Option<flow_layout::PendingGeometry>,
    horizontal: Vec<(Id, f32)>,
}

impl LogicalLocation {
    fn finite(&self) -> bool {
        self.offset.is_finite() && self.horizontal.iter().all(|(_, offset)| offset.is_finite())
    }
}
fn finite_bounds(bounds: Rectangle) -> bool {
    [bounds.x, bounds.y, bounds.width, bounds.height]
        .into_iter()
        .all(f32::is_finite)
}

/// Widget operations visit every mounted card. Only descendants of the
/// requested transcript may supply geometry or receive reader corrections.
#[derive(Default)]
struct TranscriptScope {
    active: bool,
    enter: bool,
    authorized: bool,
}
impl TranscriptScope {
    fn authorize(&mut self, owner: Option<Target>, target: Target) {
        if !self.active {
            self.authorized = owner == Some(target);
        }
    }
    fn enter_root(&mut self) -> bool {
        if std::mem::take(&mut self.authorized) {
            self.enter = true;
            true
        } else {
            false
        }
    }
    fn begin(&mut self) -> bool {
        let before = self.active;
        self.active |= std::mem::take(&mut self.enter);
        before
    }
    fn restore(&mut self, before: bool) {
        self.active = before;
    }
}

struct LocateLogical {
    target: Target,
    scroll: Id,
    item: Id,
    identity: u64,
    byte: usize,
    content: Option<Rectangle>,
    viewport: Option<Rectangle>,
    found: Option<(Rectangle, u8)>,
    frames: Vec<(Id, Rectangle, Rectangle)>,
    pending: Option<(Id, Rectangle, Rectangle)>,
    horizontal: Vec<(Id, f32)>,
    shape_pending: Option<flow_layout::PendingGeometry>,
    scope: TranscriptScope,
}
impl LocateLogical {
    fn candidate(&mut self, bounds: Rectangle, stage: u8) {
        if !finite_bounds(bounds) {
            return;
        }
        if self
            .found
            .as_ref()
            .is_none_or(|(_, previous)| stage >= *previous)
        {
            self.found = Some((bounds, stage));
            self.horizontal = self
                .frames
                .iter()
                .filter_map(|(id, viewport, content)| {
                    let travel = (content.width - viewport.width).max(0.0);
                    (travel > 0.5).then(|| {
                        (
                            id.clone(),
                            (bounds.x
                                - content.x
                                - (viewport.width - bounds.width.min(viewport.width)) / 2.0)
                                .clamp(0.0, travel),
                        )
                    })
                })
                .collect();
        }
    }
}
impl Operation<Option<LogicalLocation>> for LocateLogical {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<Option<LogicalLocation>>)) {
        let before = self.scope.begin();
        let pushed = if let Some(frame) = self.pending.take() {
            self.frames.push(frame);
            true
        } else {
            false
        };
        visit(self);
        if pushed {
            self.frames.pop();
        }
        self.scope.restore(before);
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        viewport: Rectangle,
        content: Rectangle,
        _: Vector,
        _: &mut dyn operation::Scrollable,
    ) {
        if id == Some(&self.scroll) && self.scope.enter_root() {
            self.content = Some(content);
            self.viewport = Some(viewport);
        } else if self.scope.active
            && let Some(id) = id
        {
            self.pending = Some((id.clone(), viewport, content));
        }
    }
    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if self.scope.active && id == Some(&self.item) {
            self.candidate(bounds, 0);
        }
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(marker) = state.downcast_ref::<FindCorrection>() {
            self.scope.authorize(marker.target, self.target);
            return;
        }
        if !self.scope.active {
            return;
        }
        if let Some((bounds, stage)) =
            virtualized::logical_bounds(state, bounds, self.identity, self.byte)
        {
            self.candidate(bounds, stage);
        }
        if let Some(prose) = state.downcast_ref::<selectable_prose::State>() {
            self.shape_pending = prose
                .logical_geometry_pending(self.target, self.identity, self.byte)
                .or(self.shape_pending);
            if let Some(local) = prose.logical_byte_bounds(self.identity, self.byte) {
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
    }
    fn finish(&self) -> operation::Outcome<Option<LogicalLocation>> {
        operation::Outcome::Some(self.content.zip(self.viewport).zip(self.found).and_then(
            |((content, viewport), (found, stage))| {
                if !finite_bounds(content) || !finite_bounds(viewport) {
                    return None;
                }
                let travel = (content.height - viewport.height).max(0.0);
                let from_top = (found.y - content.y - (viewport.height - found.height) / 2.0)
                    .clamp(0.0, travel);
                let location = LogicalLocation {
                    offset: travel - from_top,
                    exact: stage == 3 && self.shape_pending.is_none(),
                    stage,
                    shape_pending: self.shape_pending,
                    horizontal: self.horizontal.clone(),
                };
                location.finite().then_some(location)
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
        target,
        scroll: card_control_id(target.card, "chat-transcript"),
        item: item_id(&item),
        identity,
        byte,
        content: None,
        viewport: None,
        found: None,
        frames: Vec::new(),
        pending: None,
        horizontal: Vec::new(),
        shape_pending: None,
        scope: TranscriptScope::default(),
    })
}

/// Exact-target marker for typed transcript operations. Only Find marks a
/// correction, so ordinary keyboard/history operations keep input semantics.
struct FindCorrection {
    target: Option<Target>,
    corrected: bool,
}
struct ApplyEdge {
    target: Target,
    scope: TranscriptScope,
    edge: Edge,
    intent: find_navigation::Intent,
    document: find_navigation::DocumentEpoch,
}
impl ApplyEdge {
    fn current(&self) -> bool {
        self.intent.document_current(self.document)
    }
}
impl<T: 'static> Operation<T> for ApplyEdge {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<T>)) {
        if self.current() {
            let before = self.scope.begin();
            visit(self);
            self.scope.restore(before);
        }
    }
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if self.current()
            && let Some(marker) = state.downcast_ref::<FindCorrection>()
        {
            self.scope.authorize(marker.target, self.target);
        }
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        _: Rectangle,
        _: Rectangle,
        _: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        if self.current()
            && id == Some(&card_control_id(self.target.card, "chat-transcript"))
            && self.scope.enter_root()
        {
            state.snap_to(iced::widget::scrollable::RelativeOffset {
                x: None,
                y: Some(if self.edge == Edge::Start { 1.0 } else { 0.0 }),
            });
        }
    }
}
pub(super) fn snap_to_edge(
    target: Target,
    edge: Edge,
    intent: find_navigation::Intent,
    document: find_navigation::DocumentEpoch,
) -> iced::Task<Message> {
    iced::advanced::widget::operate(ApplyEdge {
        target,
        scope: TranscriptScope::default(),
        edge,
        intent,
        document,
    })
}

struct ApplyFind {
    target: Target,
    scope: TranscriptScope,
    scroll: Id,
    location: LogicalLocation,
    intent: find_navigation::Intent,
    document: Option<find_navigation::DocumentEpoch>,
}
impl ApplyFind {
    fn current(&self) -> bool {
        self.location.finite()
            && self.document.map_or_else(
                || self.intent.current(),
                |epoch| self.intent.document_current(epoch),
            )
    }
}
impl<T: 'static> Operation<T> for ApplyFind {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<T>)) {
        if self.current() {
            let before = self.scope.begin();
            visit(self);
            self.scope.restore(before);
        }
    }
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if self.current()
            && let Some(marker) = state.downcast_mut::<FindCorrection>()
        {
            self.scope.authorize(marker.target, self.target);
            if marker.target == Some(self.target) {
                marker.corrected = true;
            }
        }
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        _: Rectangle,
        _: Rectangle,
        _: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        if !self.current() {
            return;
        }
        if id == Some(&self.scroll) && self.scope.enter_root() {
            state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                x: None,
                y: Some(self.location.offset),
            });
        } else if self.scope.active
            && let Some((_, x)) = self
                .location
                .horizontal
                .iter()
                .find(|(wanted, _)| Some(wanted) == id)
        {
            state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                x: Some(*x),
                y: None,
            });
        }
    }
}
pub(super) fn apply_find_location(
    target: Target,
    location: LogicalLocation,
    intent: find_navigation::Intent,
    document: Option<find_navigation::DocumentEpoch>,
) -> iced::Task<Message> {
    iced::advanced::widget::operate(ApplyFind {
        target,
        scope: TranscriptScope::default(),
        scroll: card_control_id(target.card, "chat-transcript"),
        location,
        intent,
        document,
    })
}
pub(super) fn find_anchor(
    target: Target,
    anchor: Id,
    intent: find_navigation::Intent,
) -> iced::Task<Message> {
    struct Anchor {
        target: Target,
        scope: TranscriptScope,
        scroll: Id,
        anchor: Id,
        intent: find_navigation::Intent,
        content: Option<Rectangle>,
        found: Option<Rectangle>,
        viewport: Option<Rectangle>,
    }
    impl<T: 'static> Operation<T> for Anchor {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<T>)) {
            if self.intent.current() {
                let before = self.scope.begin();
                visit(self);
                self.scope.restore(before);
            }
        }
        fn scrollable(
            &mut self,
            id: Option<&Id>,
            viewport: Rectangle,
            content: Rectangle,
            _: Vector,
            _: &mut dyn operation::Scrollable,
        ) {
            if id == Some(&self.scroll) && self.scope.enter_root() {
                self.content = Some(content);
                self.viewport = Some(viewport);
            }
        }
        fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
            if let Some(marker) = state.downcast_ref::<FindCorrection>() {
                self.scope.authorize(marker.target, self.target);
            }
        }
        fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
            if self.scope.active && id == Some(&self.anchor) {
                self.found = Some(bounds);
            }
        }
        fn finish(&self) -> operation::Outcome<T> {
            if !self.intent.current() {
                return operation::Outcome::None;
            }
            let Some(((content, viewport), found)) =
                self.content.zip(self.viewport).zip(self.found)
            else {
                return operation::Outcome::None;
            };
            if !finite_bounds(content) || !finite_bounds(viewport) || !finite_bounds(found) {
                return operation::Outcome::None;
            }
            let travel = (content.height - viewport.height).max(0.0);
            operation::Outcome::Chain(Box::new(ApplyFind {
                target: self.target,
                scope: TranscriptScope::default(),
                scroll: self.scroll.clone(),
                intent: self.intent.clone(),
                document: None,
                location: LogicalLocation {
                    // The anchor lands a fade's depth below the top, so the
                    // line jumped to is not the one dissolving into the edge.
                    offset: travel
                        - (found.y - content.y - crate::interaction::OVERFLOW_FADE)
                            .clamp(0.0, travel),
                    exact: false,
                    stage: 0,
                    shape_pending: None,
                    horizontal: Vec::new(),
                },
            }))
        }
    }
    iced::advanced::widget::operate(Anchor {
        target,
        scope: TranscriptScope::default(),
        scroll: card_control_id(target.card, "chat-transcript"),
        anchor,
        intent,
        content: None,
        found: None,
        viewport: None,
    })
}

/// Row bounds for anchoring, and which of them only a placeholder reported.
struct ItemBounds<'a> {
    ids: std::collections::HashMap<Id, Vec<&'a str>>,
    rows: std::collections::HashMap<&'a str, Rectangle>,
    wanted: HashSet<u64>,
    /// Each fine-grained anchor, and whether its row is mounted.
    virtual_rows: Vec<(String, Rectangle, bool)>,
    /// Items whose bounds came from a virtual column's placeholder rather
    /// than their laid-out widget: an unmounted row's estimate, or the whole
    /// virtual row for each item it aliases.
    placeholders: HashSet<&'a str>,
    /// A virtual column reports its placeholder aliases between its
    /// [`virtualized::PlaceholderPolicy`] and its traversal of mounted rows.
    in_placeholders: bool,
}
impl Operation for ItemBounds<'_> {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        self.in_placeholders = false;
        visit(self);
        self.in_placeholders = false;
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if state.is::<virtualized::PlaceholderPolicy>() {
            self.in_placeholders = true;
            return;
        }
        self.virtual_rows.extend(
            virtualized::anchor_points(state, bounds, &self.wanted)
                .into_iter()
                .map(|(key, bounds, mounted)| {
                    (format!("virtual-chunk-{key:016x}"), bounds, mounted)
                }),
        );
    }
    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if let Some(id) = id.and_then(|id| self.ids.get(id)) {
            for id in id {
                self.rows.insert(*id, bounds);
                if self.in_placeholders {
                    self.placeholders.insert(id);
                } else {
                    self.placeholders.remove(id);
                }
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
    restoration_pending: bool,
    preparing: HashSet<&'a str>,
    find_intent: Option<find_navigation::Intent>,
    history: History,
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
    /// Only a virtual column's placeholder reported these bounds.
    placeholder: bool,
}

/// Where one layout pass puts the reader's top.
struct Position {
    top: f32,
    /// The row this was anchored to has only placeholder bounds in the new
    /// layout, while it was measured where the reader left it. Once the
    /// viewport moves there, the row is mounted and the place can be exact.
    estimated: bool,
}

impl Position {
    fn exact(top: f32) -> Self {
        Self {
            top,
            estimated: false,
        }
    }
}

/// What a layout pass asks of the native offset, as a distance from the end.
enum Placement {
    /// The reader's place is decided, and the offset moves only if given.
    Settled(Option<f32>),
    /// The reader's saved place is kept, and the offset moves to an estimate
    /// of it so the rows it is anchored to are mounted for the next pass.
    Estimated(f32),
}

#[derive(Default)]
struct Context {
    subagent: Option<String>,
    geometry: Option<Geometry>,
    rows: Vec<Row>,
    top: Option<f32>,
    used: u64,
    follow_end: bool,
    edge: Option<Edge>,
    preserve: Option<PreserveRefresh>,
}
impl Context {
    /// The saved rows that intersected the saved viewport.
    fn shown_rows(&self) -> impl Iterator<Item = &Row> {
        let window = self.geometry.zip(self.top);
        self.rows.iter().filter(move |row| {
            window.is_some_and(|(geometry, top)| {
                row.bounds.y + row.bounds.height > top
                    && row.bounds.y < top + geometry.viewport.height
            })
        })
    }
}

impl Context {
    fn position(
        &self,
        next: Geometry,
        rows: &[(&str, Rectangle)],
        placeholders: &HashSet<&str>,
        follow_end: bool,
    ) -> Position {
        let Some((previous, top)) = self.geometry.zip(self.top) else {
            return Position::exact(if self.subagent.is_some() {
                0.0
            } else {
                next.travel()
            });
        };
        // Fitting live content normally follows its end. But an earlier page
        // can prepend surviving rows to that same short transcript: preserve
        // the exchange the reader was looking at instead of jumping past it.
        // Only a transcript that fit had every row measured. In a longer one
        // an unmounted row reports its group's placeholder bounds, and its
        // measured position later lies below them: as the live window evicts
        // the start of a long answer, that is not new history above the
        // reader, who stays at the end.
        let prepended = previous.travel() <= 0.5
            && self.rows.first().is_some_and(|old| {
                rows.iter()
                    .any(|(id, bounds)| *id == old.id && bounds.y > old.bounds.y + 0.5)
            });
        if !prepended
            && self.follow_end
            && !self.preserve.is_some_and(|preserve| preserve.arrived)
            && follow_end
            && previous.travel() - top <= 0.5
            && (previous.travel() > 0.0 || self.subagent.is_none())
        {
            return Position::exact(next.travel());
        }
        // A recreated widget can restore authoritative native geometry from
        // the Runtime before it has any measured row anchors.
        if self.rows.is_empty() {
            return Position::exact(top.clamp(0.0, next.travel()));
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
            return Position::exact(top.clamp(0.0, next.travel()));
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
        // A row measured where the reader left it, but only a placeholder
        // now, gives an estimated place.
        fn surviving<'r>(
            row: &'r Row,
            next_rows: &std::collections::HashMap<&str, Rectangle>,
            placeholders: &HashSet<&str>,
        ) -> Option<(&'r Row, Rectangle, bool)> {
            let bounds = *next_rows.get(row.id.as_str())?;
            Some((
                row,
                bounds,
                !row.placeholder && placeholders.contains(row.id.as_str()),
            ))
        }
        let surviving = |row| surviving(row, &next_rows, placeholders);
        let chunk = |row: &&Row| row.id.starts_with("virtual-chunk-");
        // A placeholder's bounds are an estimate, or the whole virtual row for
        // each item it aliases, so an offset within them lands somewhere else.
        // Anchor to a row measured in both layouts when one is visible.
        let measured = |row: &&Row| !row.placeholder;
        let overlap = visible()
            .filter(chunk)
            .filter(measured)
            .filter_map(surviving)
            .find(|(.., estimated)| !estimated)
            .or_else(|| {
                visible()
                    .filter(measured)
                    .filter_map(surviving)
                    .find(|(.., estimated)| !estimated)
            })
            .or_else(|| visible().filter(chunk).filter_map(surviving).next())
            .or_else(|| visible().filter_map(surviving).next())
            .or_else(|| {
                // Front eviction can remove every visible exchange. Keep the
                // nearest surviving boundary instead of jumping to live output.
                // Its row was not in view, so mounting it would not refine
                // the reader's place.
                self.rows
                    .iter()
                    .filter_map(surviving)
                    .min_by(|(a, ..), (b, ..)| {
                        (a.bounds.y - top)
                            .abs()
                            .total_cmp(&(b.bounds.y - top).abs())
                    })
                    .map(|(row, bounds, _)| (row, bounds, false))
            });
        let fallback = if self.preserve.is_some_and(|preserve| preserve.arrived) {
            top.clamp(0.0, next.travel())
        } else {
            next.travel()
        };
        overlap.map_or(Position::exact(fallback), |(old, new, estimated)| {
            let within = top - old.bounds.y;
            // A disclosure or reflow may shorten the anchored exchange itself.
            // Do not carry an offset past its new end into an unrelated turn.
            let within = if within > 0.0 {
                within.min(new.height.max(0.0))
            } else {
                within
            };
            Position {
                top: (new.y + within).clamp(0.0, next.travel()),
                estimated,
            }
        })
    }

    fn remember(
        &mut self,
        next: Geometry,
        rows: &[(&str, Rectangle)],
        placeholders: &HashSet<&str>,
        top: f32,
    ) {
        for (index, (id, bounds)) in rows.iter().enumerate() {
            let placeholder = placeholders.contains(*id);
            if let Some(row) = self.rows.get_mut(index) {
                if row.id != *id {
                    row.id = (*id).to_owned();
                }
                row.bounds = *bounds;
                row.placeholder = placeholder;
            } else {
                self.rows.push(Row {
                    id: (*id).to_owned(),
                    bounds: *bounds,
                    placeholder,
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
    pending_input: Option<view::Page>,
    modifiers: iced::keyboard::Modifiers,
    seed: Option<Scroll>,
    /// The page a wheel last asked for ahead of the reader, and the item
    /// the window it asked from was to slide around.
    prefetched: Option<(view::Page, String)>,
}

impl Anchor {
    /// Where the halves a page around `pivot` keeps end, from the rows
    /// `subagent`'s transcript last laid out. `items` is the transcript's
    /// ids, its items in order first. A row can hold several items, such as
    /// an exchange's steps, and an exchange draws its steps above its prose
    /// whatever their order, so each half ends at the nearest row holding
    /// any item of the other.
    fn kept(
        &self,
        subagent: Option<&str>,
        items: &[(Id, &str)],
        pivot: Pivot,
    ) -> Option<view::Kept> {
        let context = self
            .contexts
            .iter()
            .find(|context| context.subagent.as_deref() == subagent)?;
        let order: std::collections::HashMap<&str, usize> = items
            .iter()
            .take(pivot.items)
            .enumerate()
            .map(|(index, (_, id))| (*id, index))
            .collect();
        let mut older_end = None::<f32>;
        let mut newer_start = None::<f32>;
        for row in &context.rows {
            let Some(&index) = order.get(row.id.as_str()) else {
                continue;
            };
            if index > pivot.index {
                older_end = Some(older_end.map_or(row.bounds.y, |end| end.min(row.bounds.y)));
            } else if index < pivot.index {
                let bottom = row.bounds.y + row.bounds.height;
                newer_start = Some(newer_start.map_or(bottom, |start| start.max(bottom)));
            }
        }
        Some(view::Kept {
            older_end: older_end?,
            newer_start: newer_start?,
        })
    }

    /// A wheel heading for an edge asks for its page once: further notches
    /// toward the same edge, before the page moves the window, would only
    /// cost the model an update each. A reader who turns back, or a window
    /// around another item, asks again.
    fn ahead_once(
        &mut self,
        direction: Option<view::Page>,
        wheel: bool,
        scroll: Scroll,
        pivot: Option<&str>,
    ) -> Option<view::Page> {
        if !wheel || scroll.at_top() || scroll.at_bottom() {
            return direction;
        }
        let Some(page) = direction else {
            self.prefetched = None;
            return None;
        };
        let pivot = pivot.unwrap_or_default();
        if self
            .prefetched
            .as_ref()
            .is_some_and(|(asked, from)| *asked == page && from == pivot)
        {
            return None;
        }
        self.prefetched = Some((page, pivot.to_owned()));
        direction
    }

    fn context_index(&mut self, subagent: Option<&str>) -> usize {
        self.contexts
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
            })
    }

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
        self.layout_measured(subagent, rows, next, current_top, true)
    }

    /// A pass over `rows` that were all measured.
    #[cfg(test)]
    fn layout_measured(
        &mut self,
        subagent: Option<&str>,
        rows: &[(&str, Rectangle)],
        next: Geometry,
        current_top: Option<f32>,
        follow_end: bool,
    ) -> Option<f32> {
        match self.layout_following(
            subagent,
            rows,
            &HashSet::new(),
            next,
            current_top,
            follow_end,
            true,
        ) {
            Placement::Settled(bottom) => bottom,
            Placement::Estimated(_) => unreachable!("a settling pass decides"),
        }
    }

    /// `placeholders` names the `rows` only a placeholder reported. Unless
    /// `settle` is set, a pass that could anchor the reader only to such a
    /// row keeps the saved place and returns [`Placement::Estimated`].
    #[allow(clippy::too_many_arguments)]
    fn layout_following(
        &mut self,
        subagent: Option<&str>,
        rows: &[(&str, Rectangle)],
        placeholders: &HashSet<&str>,
        next: Geometry,
        current_top: Option<f32>,
        follow_end: bool,
        settle: bool,
    ) -> Placement {
        if let Some(index) = self.active
            && let Some(top) = current_top
        {
            self.contexts[index].top = Some(top);
        }
        let previous_active = self.active;
        let index = self.context_index(subagent);
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
        let edge = context.edge;
        let position = match edge {
            Some(Edge::Start) => Position::exact(0.0),
            Some(Edge::End | Edge::Latest) => Position::exact(next.travel()),
            None => context.position(next, rows, placeholders, follow_end),
        };
        if position.estimated && !settle {
            // Remembering this pass would replace the measured place with the
            // estimate, and the next pass would faithfully keep the estimate.
            self.active = Some(index);
            if switched {
                self.pending_input = None;
            }
            return Placement::Estimated(next.travel() - position.top);
        }
        let top = position.top;
        // Either explicit edge remains deliberate while a nonempty prepared
        // source fits, including a child that would otherwise start on growth.
        // A genuine empty reset consumes it.
        if next.travel() > 0.0 || rows.is_empty() {
            context.edge = None;
        }
        context.follow_end = follow_end && (rows.is_empty() || edge != Some(Edge::Start));
        let bottom = next.travel() - top;
        context.used = self.clock;
        context.remember(next, rows, placeholders, top);
        if let Some(preserve) = &mut context.preserve
            && preserve.arrived
        {
            if preserve.latest {
                context.preserve = None;
            } else {
                preserve.arrived = false;
            }
        }
        self.active = Some(index);
        if switched {
            self.pending_input = None;
        }
        Placement::Settled(
            (edge.is_some()
                || switched
                || freeze_reading
                || old_bottom.is_none_or(|old| (old - bottom).abs() > 0.5))
            .then_some(bottom),
        )
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
    pages: Pages,
) -> Option<view::Page> {
    match wheel {
        Some(y) if !child_captured => {
            if y > 0.0 && scroll.at_top() && before.is_none_or(|old| top <= old) {
                Some(view::Page::Earlier)
            } else if y < 0.0 && scroll.at_bottom() && before.is_none_or(|old| top >= old) {
                Some(view::Page::Newer)
            } else {
                // A wheel within reach of a loaded edge is a reader heading
                // there: the page they are heading for is fetched ahead of
                // them. Elsewhere a notch is only reading, and asks nothing
                // of the model.
                before.and_then(|old| {
                    if pages.earlier && top < old && scroll.prefetch_earlier() {
                        Some(view::Page::Earlier)
                    } else if pages.later && top > old && scroll.prefetch_later() {
                        Some(view::Page::Newer)
                    } else {
                        None
                    }
                })
            }
        }
        Some(_) => None,
        None => before.and_then(|old| {
            (old != top && (!scroll.at_top() || top < old) && (!scroll.at_bottom() || top > old))
                .then_some(if top < old {
                    view::Page::Earlier
                } else {
                    view::Page::Newer
                })
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

impl Anchored<'_> {
    /// The item a page slides this transcript's window around.
    fn pivot_id(&self) -> Option<&str> {
        let pivot = self.history.pivot?;
        self.items
            .get(..pivot.items)?
            .get(pivot.index)
            .map(|(_, id)| *id)
    }

    /// What of the window a page keeps, see [`Anchor::kept`].
    fn kept(&self, anchor: &Anchor) -> Option<view::Kept> {
        anchor.kept(self.subagent, &self.items, self.history.pivot?)
    }
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Anchored<'_> {
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        if tree.state.downcast_ref::<ReaderState>().revealing {
            return iced_accessibility::A11yTree::default();
        }
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
            revealing: self.target.is_some(),
            needs_layout: false,
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
        let source_pending = self.restoration_pending;
        let state = tree.state.downcast_mut::<ReaderState>();
        if state.memory.edge(self.subagent).is_some()
            || state.memory.preserve_arrived(self.subagent)
        {
            state.restore = true;
        }
        let switching = state.memory.0.borrow().active.is_none_or(|index| {
            state.memory.0.borrow().contexts[index].subagent.as_deref() != self.subagent
        });
        state.revealing |= self.target.is_some() && switching;
        state.needs_layout = false;
        if (state.restore || switching) && self.restoration_pending {
            state.restore = true;
            // An empty hydrated-body wait or a cold source placeholder is not
            // new reader geometry. Keep only the saved IDs/offsets until that
            // exact selected source is prepared; never retain its old widgets.
            return self
                .content
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, limits);
        }
        let mut restore = std::mem::take(&mut state.restore);
        let mut anchor = state.memory.0.borrow_mut();
        if restore {
            anchor.pending_input = None;
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
        const PASSES: usize = 4;
        let mut settle = false;
        for pass in 0..PASSES {
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
                placeholders: HashSet::new(),
                in_placeholders: false,
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
                    .map(|(key, bounds, _)| (key.as_str(), *bounds)),
            );
            let mut placeholders = measured.placeholders;
            placeholders.extend(
                measured
                    .virtual_rows
                    .iter()
                    .filter(|(.., mounted)| !mounted)
                    .map(|(key, ..)| key.as_str()),
            );
            rows.sort_by(|a, b| a.1.y.total_cmp(&b.1.y));
            // A superseded Latest fetch may replace every old row. Wait for
            // only cold aliases touching the desired replacement viewport;
            // unrelated history preparation cannot hold the reader here.
            let pending_window_top = anchor
                .contexts
                .iter()
                .find(|context| context.subagent.as_deref() == self.subagent)
                .filter(|context| {
                    context.edge.is_none()
                        && context.preserve.is_some_and(|preserve| preserve.arrived)
                })
                .map(|context| {
                    context
                        .position(next, &rows, &placeholders, self.follow_end)
                        .top
                })
                .filter(|top| {
                    rows.iter().any(|(id, bounds)| {
                        self.preparing.contains(id)
                            && bounds.y + bounds.height > *top
                            && bounds.y < *top + next.viewport.height
                    })
                });
            if let Some(top) = pending_window_top {
                self.restoration_pending = true;
                state.restore = true;
                state.needs_layout = false;
                self.content.as_widget_mut().operate(
                    child,
                    Layout::new(&node),
                    renderer,
                    &mut SetBottom(next.travel() - top),
                );
                let mut viewport = virtualized::UpdateViewport::default();
                self.content.as_widget_mut().operate(
                    child,
                    Layout::new(&node),
                    renderer,
                    &mut viewport,
                );
                if !viewport.changed {
                    break;
                }
                node = self.content.as_widget_mut().layout(child, renderer, limits);
                continue;
            }
            self.restoration_pending = source_pending;
            state.restore = false;
            // A returning tree mounts only rows near the end, so the rows the
            // reader left are placeholders in its first pass. That pass only
            // moves the native offset toward them; the next one, with them
            // mounted, places the reader from the saved measurements.
            let placement = anchor.layout_following(
                self.subagent,
                &rows,
                &placeholders,
                next,
                top.0,
                self.follow_end,
                settle || pass + 1 == PASSES,
            );
            let estimated = matches!(placement, Placement::Estimated(_));
            let offset = match placement {
                Placement::Estimated(bottom) => Some(bottom),
                Placement::Settled(bottom) => bottom.or_else(|| {
                    restore.then(|| {
                        next.travel()
                            - anchor
                                .active
                                .and_then(|index| anchor.contexts[index].top)
                                .unwrap_or_default()
                    })
                }),
            };
            if let Some(offset) = offset {
                self.content.as_widget_mut().operate(
                    child,
                    Layout::new(&node),
                    renderer,
                    &mut SetBottom(offset),
                );
                // An estimate is not where the reader is: the next pass starts
                // from the saved place, and must apply whatever it settles on.
                top.0 = (!estimated).then(|| (next.travel() - offset).max(0.0));
            } else if top.0.is_some() {
                // The native offset kept its distance from the end, which is
                // where this pass decided the reader is. A later pass in this
                // layout must start there, not at the top read before growth:
                // otherwise a measurement that corrects this pass's estimate
                // makes a reader following the end look scrolled away.
                top.0 = anchor.active.and_then(|index| anchor.contexts[index].top);
            }
            restore = estimated;
            let mut virtual_viewport = virtualized::UpdateViewport::default();
            self.content.as_widget_mut().operate(
                child,
                Layout::new(&node),
                renderer,
                &mut virtual_viewport,
            );
            if !virtual_viewport.changed {
                if estimated {
                    // Moving there mounts nothing more, so these rows are as
                    // measured as this layout gets: settle on them.
                    settle = true;
                    continue;
                }
                state.revealing &= self.restoration_pending;
                state.needs_layout = false;
                break;
            }
            // A cold reader is not presentable while its native virtual
            // windows still disagree with the restored scroll position. Keep
            // the four-pass bound and continue in the next native update.
            state.needs_layout = state.revealing && !self.restoration_pending;
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
        let mut correction = FindCorrection {
            target: self.target,
            corrected: false,
        };
        operation.custom(None, layout.bounds(), &mut correction);
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
            let state = tree.state.downcast_mut::<ReaderState>();
            let mut anchor = state.memory.0.borrow_mut();
            if let Some(top) = after.0 {
                let changed = before.0.is_some_and(|before| before != top);
                let retain_edge = self.restoration_pending
                    && !correction.corrected
                    && anchor.contexts.iter().any(|context| {
                        context.subagent.as_deref() == self.subagent && context.edge.is_some()
                    });
                if (changed || correction.corrected) && !retain_edge {
                    state.restore = false;
                    if let Some(index) = anchor
                        .contexts
                        .iter()
                        .position(|context| context.subagent.as_deref() == self.subagent)
                    {
                        anchor.active = Some(index);
                        let context = &mut anchor.contexts[index];
                        context.edge = None;
                        context.top = Some(top);
                        if self.restoration_pending {
                            context.geometry = Some(Geometry {
                                viewport: layout.bounds().size(),
                                content: layout
                                    .children()
                                    .next()
                                    .expect("transcript content")
                                    .bounds()
                                    .size(),
                            });
                            context.rows.clear();
                        }
                    }
                }
            }
            if !correction.corrected
                && let Some((top, geometry)) = after.0.zip(anchor.geometry())
            {
                anchor.pending_input = navigation_input(
                    before.0,
                    top,
                    Scroll::new(top, geometry.content.height, geometry.viewport.height),
                    None,
                    false,
                    self.history.pages,
                )
                .or(anchor.pending_input);
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
        let revealing = tree.state.downcast_ref::<ReaderState>().revealing;
        if let Event::Keyboard(iced::keyboard::Event::ModifiersChanged(modifiers)) = event {
            tree.state
                .downcast_ref::<ReaderState>()
                .memory
                .0
                .borrow_mut()
                .modifiers = *modifiers;
        }
        if let Some(intent) = &self.find_intent
            && (!revealing
                || matches!(
                    event,
                    Event::Mouse(mouse::Event::WheelScrolled { .. })
                        | Event::Window(iced::window::Event::Resized(_))
                ))
            && layout.bounds().intersection(viewport).is_some()
            && matches!(
                event,
                Event::Mouse(mouse::Event::WheelScrolled { .. })
                    | Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                    | Event::Touch(_)
                    | Event::Window(iced::window::Event::Resized(_))
            )
            && (matches!(
                event,
                Event::Touch(_) | Event::Window(iced::window::Event::Resized(_))
            ) || cursor.is_over(layout.bounds().intersection(viewport).unwrap()))
        {
            intent.cancel();
        }
        // A returning native tree can still be at its default end. Apply a
        // wheel relative to the saved reader, never that provisional offset.
        let provisional_wheel = (revealing
            && matches!(event, Event::Mouse(mouse::Event::WheelScrolled { .. }))
            && cursor.is_over(layout.bounds())
            && cursor.is_over(*viewport))
        .then(|| {
            let state = tree.state.downcast_ref::<ReaderState>();
            let anchor = state.memory.0.borrow();
            let context = anchor
                .contexts
                .iter()
                .find(|context| context.subagent.as_deref() == self.subagent)?;
            Some((context.top?, context.geometry?))
        })
        .flatten();
        if let Some((top, _)) = provisional_wheel {
            let travel = (layout
                .children()
                .next()
                .expect("transcript content")
                .bounds()
                .height
                - layout.bounds().height)
                .max(0.0);
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut SetBottom(travel - top.clamp(0.0, travel)),
            );
        }
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
        // Explicit Find/Home/End widget operations still traverse this reader,
        // and wheel intent may supersede its saved position. Provisional text
        // cannot receive clicks, touch, typing or queued accessibility actions.
        if !revealing
            || matches!(
                event,
                Event::Window(_)
                    | Event::Mouse(mouse::Event::WheelScrolled { .. })
                    | Event::Keyboard(iced::keyboard::Event::ModifiersChanged(_))
            )
        {
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
        let mut virtual_viewport = virtualized::UpdateViewport::default();
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout,
            renderer,
            &mut virtual_viewport,
        );
        if virtual_viewport.changed && (!revealing || !self.restoration_pending) {
            shell.invalidate_layout();
        }
        if tree.state.downcast_ref::<ReaderState>().needs_layout {
            shell.invalidate_layout();
            shell.request_redraw();
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
            let state = tree.state.downcast_mut::<ReaderState>();
            let mut anchor = state.memory.0.borrow_mut();
            if let Some((saved_top, saved_geometry)) = provisional_wheel {
                if !self
                    .captured
                    .as_ref()
                    .is_some_and(|captured| captured.get())
                    && let Some(context) = anchor
                        .contexts
                        .iter_mut()
                        .find(|context| context.subagent.as_deref() == self.subagent)
                {
                    context.edge = None;
                    let top = (saved_top + top - before.0.unwrap_or(top))
                        .clamp(0.0, saved_geometry.travel());
                    context.top = Some(top);
                    let scroll = Scroll::new(
                        top,
                        saved_geometry.content.height,
                        saved_geometry.viewport.height,
                    )
                    .with_kept(self.kept(&anchor));
                    let wheel = match event {
                        Event::Mouse(mouse::Event::WheelScrolled { delta }) => Some(match *delta {
                            mouse::ScrollDelta::Lines { x, y } => {
                                if anchor.modifiers.shift() && !cfg!(target_os = "macos") {
                                    x
                                } else {
                                    y
                                }
                            }
                            mouse::ScrollDelta::Pixels { y, .. } => y,
                        }),
                        _ => None,
                    };
                    let direction = navigation_input(
                        Some(saved_top),
                        top,
                        scroll,
                        wheel,
                        false,
                        self.history.pages,
                    );
                    let direction = anchor
                        .ahead_once(direction, wheel.is_some(), scroll, self.pivot_id())
                        .or(anchor.pending_input);
                    anchor.pending_input = None;
                    anchor.reported = Some(Report {
                        subagent: self.subagent.map(str::to_owned),
                        top,
                        geometry: saved_geometry,
                    });
                    if let Some(observations) = &self.observations {
                        observations
                            .borrow_mut()
                            .insert(self.subagent.map(str::to_owned), scroll);
                    }
                    if let Some(target) = self
                        .target
                        .filter(|_| self.observations.is_none() || direction.is_some())
                    {
                        shell.publish(action(Action::Scrolled(
                            target,
                            self.subagent.map(str::to_owned),
                            scroll,
                            direction,
                        )));
                    }
                }
                // Preserve row-relative restoration across the remaining cold
                // measurements, including a fitting placeholder's zero delta.
                state.restore = true;
                return;
            }
            let moved = before.0.is_some_and(|before| before != top);
            let wheel_intent = matches!(event, Event::Mouse(mouse::Event::WheelScrolled { .. }))
                && cursor.is_over(layout.bounds())
                && cursor.is_over(*viewport)
                && !self
                    .captured
                    .as_ref()
                    .is_some_and(|captured| captured.get());
            if moved || wheel_intent {
                if let Some(context) = anchor
                    .contexts
                    .iter_mut()
                    .find(|context| context.subagent.as_deref() == self.subagent)
                {
                    context.edge = None;
                }
            }
            if state.restore && self.restoration_pending && !moved {
                return;
            }
            if moved {
                state.restore = false;
                if let Some(index) = anchor
                    .contexts
                    .iter()
                    .position(|context| context.subagent.as_deref() == self.subagent)
                {
                    anchor.active = Some(index);
                    let context = &mut anchor.contexts[index];
                    context.top = Some(top);
                    if self.restoration_pending {
                        context.geometry = Some(geometry);
                        context.rows.clear();
                    }
                }
            }
            let changed = anchor.reported.as_ref().is_none_or(|previous| {
                previous.subagent.as_deref() != self.subagent
                    || previous.top != top
                    || previous.geometry != geometry
            });
            let scroll = Scroll::new(top, geometry.content.height, geometry.viewport.height)
                .with_kept(self.kept(&anchor));
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
            let direction = navigation_input(
                before.0,
                top,
                scroll,
                wheel,
                child_captured,
                self.history.pages,
            );
            let direction = anchor
                .ahead_once(direction, wheel.is_some(), scroll, self.pivot_id())
                .or(anchor.pending_input);
            if changed || direction.is_some() {
                anchor.pending_input = None;
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
                    .filter(|_| self.observations.is_none() || direction.is_some())
                {
                    shell.publish(action(Action::Scrolled(
                        target,
                        self.subagent.map(str::to_owned),
                        scroll,
                        direction,
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
        if tree.state.downcast_ref::<ReaderState>().revealing {
            return;
        }
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
        if tree.state.downcast_ref::<ReaderState>().revealing {
            return mouse::Interaction::None;
        }
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
        if tree.state.downcast_ref::<ReaderState>().revealing {
            return None;
        }
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
mod tests;
