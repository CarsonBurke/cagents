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
        anchor.contexts[index].reflow = None;
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
    pub fn reader_width(&self, context: Option<&str>, identity: u64) -> f32 {
        self.1
            .borrow()
            .entries
            .get(&(context.map(str::to_owned), identity))
            .map_or(0.0, |saved| saved.content_width)
            .max(0.0)
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
        let reference = saved.reflow.as_ref();
        let Some((geometry, top)) = reference
            .map(|saved| (saved.geometry, saved.top))
            .or_else(|| saved.geometry.zip(saved.top))
        else {
            return false;
        };
        let rows = reference.map_or(saved.rows.as_slice(), |saved| saved.rows.as_slice());
        rows.iter().any(|row| {
            row.bounds.y + row.bounds.height > top
                && row.bounds.y < top + geometry.viewport.height
                && pending(&row.id)
        })
    }

    pub fn retained_rows(&self) -> usize {
        self.0
            .borrow()
            .contexts
            .iter()
            .map(|context| {
                context.rows.len() + context.reflow.as_ref().map_or(0, |saved| saved.rows.len())
            })
            .sum()
    }

    /// Observe geometry produced by the real transcript layout, including
    /// while the modal Inspector intentionally withholds widget operations.
    #[cfg(test)]
    pub(super) fn inspector_fixture_geometry(
        &self,
        subagent: Option<&str>,
    ) -> Option<(Scroll, Size, Vec<(String, Rectangle)>)> {
        let anchor = self.0.borrow();
        let context = anchor
            .contexts
            .iter()
            .find(|context| context.subagent.as_deref() == subagent)?;
        Some((
            Scroll::new(
                context.top?,
                context.geometry?.content.height,
                context.geometry?.viewport.height,
            ),
            context.geometry?.viewport,
            context
                .rows
                .iter()
                .map(|row| (row.id.clone(), row.bounds))
                .collect(),
        ))
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
    restoration_pending: bool,
    preparing: HashSet<&'a str>,
    find_intent: Option<find_navigation::Intent>,
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
        restoration_pending,
        preparing,
        find_intent,
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
pub(super) struct LogicalLocation {
    pub offset: f32,
    pub exact: bool,
    pub stage: u8,
    pub shape_pending: Option<flow_layout::PendingGeometry>,
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
                    offset: travel - (found.y - content.y).clamp(0.0, travel),
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

struct ItemBounds<'a> {
    ids: std::collections::HashMap<Id, Vec<&'a str>>,
    rows: std::collections::HashMap<&'a str, Rectangle>,
    wanted: HashSet<u64>,
    virtual_rows: Vec<(String, Rectangle)>,
    mounted: HashSet<String>,
    wanted_guards: HashSet<u64>,
    guards: HashMap<u64, bool>,
    path: Vec<u64>,
    paths: HashMap<String, Vec<u64>>,
}
impl Operation for ItemBounds<'_> {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(scope) = state.downcast_mut::<virtualized::RowScope>() {
            match scope {
                virtualized::RowScope::Probe(enabled) => *enabled = true,
                virtualized::RowScope::Enter(key) => self.path.push(*key),
                virtualized::RowScope::Exit => {
                    self.path.pop();
                }
            }
            return;
        }
        self.guards
            .extend(virtualized::row_status(state, &self.wanted_guards));
        self.virtual_rows.extend(
            virtualized::anchor_points(state, bounds, &self.wanted)
                .into_iter()
                .map(|(key, bounds, mounted)| {
                    let id = format!("virtual-chunk-{key:016x}");
                    self.paths.insert(id.clone(), self.path.clone());
                    if mounted {
                        self.mounted.insert(id.clone());
                    }
                    (id, bounds)
                }),
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

struct MountedItems<'a, 'b> {
    ids: &'b std::collections::HashMap<Id, Vec<&'a str>>,
    items: HashSet<&'a str>,
    target: Option<Target>,
    root: Option<Id>,
    root_translation: Option<Vector>,
    pending: Vec<Rectangle>,
    translation: Vector,
    clip: Option<Rectangle>,
    pending_clip: Option<(Rectangle, Vector)>,
}

impl MountedItems<'_, '_> {
    fn pending_geometry(&mut self, bounds: Rectangle) {
        if self.clip.is_some_and(|clip| {
            clip.intersection(&Rectangle {
                x: bounds.x - self.translation.x,
                y: bounds.y - self.translation.y,
                ..bounds
            })
            .is_some()
        }) {
            let nested = self.translation - self.root_translation.unwrap_or_default();
            self.pending.push(Rectangle {
                x: bounds.x - nested.x,
                y: bounds.y - nested.y,
                ..bounds
            });
        }
    }
}

impl Operation for MountedItems<'_, '_> {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        let translation = self.translation;
        let clip = self.clip;
        if let Some((bounds, offset)) = self.pending_clip.take() {
            let bounds = Rectangle {
                x: bounds.x - self.translation.x,
                y: bounds.y - self.translation.y,
                ..bounds
            };
            self.clip = Some(self.clip.map_or(bounds, |clip| {
                clip.intersection(&bounds).unwrap_or_default()
            }));
            self.translation += offset;
        }
        visit(self);
        self.translation = translation;
        self.clip = clip;
    }

    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        _: Rectangle,
        translation: Vector,
        _: &mut dyn operation::Scrollable,
    ) {
        if self.root_translation.is_none() && (self.root.is_none() || self.root.as_ref() == id) {
            self.root_translation = Some(translation);
        }
        self.pending_clip = Some((bounds, translation));
    }

    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(policy) = state.downcast_mut::<virtualized::PlaceholderPolicy>() {
            policy.visit = false;
        }
        if let Some(prose) = state.downcast_ref::<selectable_prose::State>()
            && let (Some(target), Some(logical)) =
                (self.target.or(prose.target), prose.logical.as_ref())
            && prose
                .logical_geometry_pending(target, logical.identity, logical.start)
                .is_some()
        {
            self.pending_geometry(bounds);
        }
    }

    fn container(&mut self, id: Option<&Id>, _: Rectangle) {
        if let Some(ids) = id.and_then(|id| self.ids.get(id)) {
            self.items.extend(ids.iter().copied());
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

#[derive(Clone)]
struct Row {
    id: String,
    bounds: Rectangle,
}

struct Reflow {
    geometry: Geometry,
    rows: Vec<Row>,
    top: f32,
    follow_end: bool,
    preserve: bool,
    path: Vec<u64>,
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
    reflow: Option<Reflow>,
    paths: HashMap<String, Vec<u64>>,
}

impl Context {
    fn position(&self, next: Geometry, rows: &[(&str, Rectangle)], follow_end: bool) -> f32 {
        let reference = self.reflow.as_ref();
        let previous_top = reference
            .map(|saved| (saved.geometry, saved.top))
            .or_else(|| self.geometry.zip(self.top));
        let Some((previous, top)) = previous_top else {
            return if self.subagent.is_some() {
                0.0
            } else {
                next.travel()
            };
        };
        let old_rows = reference.map_or(self.rows.as_slice(), |saved| saved.rows.as_slice());
        let was_following = reference.map_or(self.follow_end, |saved| saved.follow_end);
        let preserving = reference.is_some_and(|saved| saved.preserve)
            || self.preserve.is_some_and(|preserve| preserve.arrived);
        // Fitting live content normally follows its end. But an earlier page
        // can prepend surviving rows to that same short transcript: preserve
        // the exchange the reader was looking at instead of jumping past it.
        let prepended = old_rows.first().is_some_and(|old| {
            rows.iter()
                .any(|(id, bounds)| *id == old.id && bounds.y > old.bounds.y + 0.5)
        });
        if !prepended
            && was_following
            && !preserving
            && follow_end
            && previous.travel() - top <= 0.5
            && (previous.travel() > 0.0 || self.subagent.is_none())
        {
            return next.travel();
        }
        // A recreated widget can restore authoritative native geometry from
        // the Runtime before it has any measured row anchors.
        if old_rows.is_empty() {
            return top.clamp(0.0, next.travel());
        }
        // Most relayouts only refresh a timer or hover state. Avoid building
        // a lookup table when every measured row is unchanged.
        if old_rows.len() == rows.len()
            && old_rows
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
            old_rows.iter().filter(|row| {
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
                old_rows
                    .iter()
                    .filter_map(|row| next_rows.get(row.id.as_str()).map(|bounds| (row, bounds)))
                    .min_by(|(a, _), (b, _)| {
                        (a.bounds.y - top)
                            .abs()
                            .total_cmp(&(b.bounds.y - top).abs())
                    })
            });
        let fallback = if preserving {
            top.clamp(0.0, next.travel())
        } else {
            next.travel()
        };
        overlap.map_or(fallback, |(old, new)| {
            let within = top - old.bounds.y;
            // A disclosure or reflow may shorten the anchored exchange itself.
            // Do not carry an offset past its new end into an unrelated turn.
            let within = if within > 0.0 {
                // During reflow keep the source intersecting the viewport.
                // Landing exactly at an estimated end would exclude the row
                // both from realization and its redraw-driven Flow request.
                let inset = if reference.is_some() { 1.0 } else { 0.0 };
                within.min((new.height - inset).max(0.0))
            } else {
                within
            };
            (new.y + within).clamp(0.0, next.travel())
        })
    }

    fn finish_reflow(
        &mut self,
        rows: &[(&str, Rectangle)],
        mounted: &HashSet<String>,
        pending: &[Rectangle],
        mounted_items: &HashSet<&str>,
        guards: &HashMap<u64, bool>,
    ) {
        let Some(saved) = &self.reflow else {
            return;
        };
        let visible = |row: &&Row| {
            row.bounds.y + row.bounds.height > saved.top
                && row.bounds.y < saved.top + saved.geometry.viewport.height
        };
        let fine = saved
            .rows
            .iter()
            .filter(visible)
            .find(|row| row.id.starts_with("virtual-chunk-"));
        let prepended = saved.rows.first().is_some_and(|old| {
            rows.iter()
                .any(|(id, bounds)| *id == old.id && bounds.y > old.bounds.y + 0.5)
        });
        let following_end = !prepended
            && saved.follow_end
            && self.follow_end
            && !saved.preserve
            && !self.preserve.is_some_and(|preserve| preserve.arrived)
            && saved.geometry.travel() - saved.top <= 0.5
            && (saved.geometry.travel() > 0.0 || self.subagent.is_none());
        let viewport_ready = || self.top.is_some() && pending.is_empty();
        let ready = if following_end {
            viewport_ready()
        } else if let Some(fine) = fine {
            let parent = |row: &&Row| {
                !row.id.starts_with("virtual-chunk-")
                    && row.bounds.y <= fine.bounds.y
                    && row.bounds.y + row.bounds.height >= fine.bounds.y + fine.bounds.height - 0.5
            };
            let still_loaded = saved
                .rows
                .iter()
                .filter(parent)
                .any(|row| rows.iter().any(|(id, _)| *id == row.id));
            let mut route_ready = true;
            for key in &saved.path {
                match guards.get(key) {
                    Some(true) => {}
                    Some(false) => {
                        route_ready = false;
                        break;
                    }
                    // Every preceding descriptor is mounted. The first
                    // absent child belongs to a replaced native source path.
                    None => break,
                }
            }
            let parent_ready = route_ready
                && viewport_ready()
                && saved.rows.iter().filter(visible).any(|row| {
                    !row.id.starts_with("virtual-chunk-")
                        && row.bounds.y <= fine.bounds.y
                        && row.bounds.y + row.bounds.height
                            >= fine.bounds.y + fine.bounds.height - 0.5
                        && mounted_items.contains(row.id.as_str())
                        && rows.iter().any(|(id, _)| *id == row.id)
                });
            rows.iter()
                .find(|(id, _)| *id == fine.id)
                .is_some_and(|(id, _)| mounted.contains(*id) && viewport_ready())
                || (!rows.iter().any(|(id, _)| *id == fine.id) && (!still_loaded || parent_ready))
        } else {
            viewport_ready()
        };
        if ready {
            self.reflow = None;
        }
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
        if context.reflow.is_none()
            && context.edge.is_none()
            && let Some((previous, top)) = context.geometry.zip(context.top)
            && previous.viewport.width != next.viewport.width
        {
            // Keep the measured source identity through width estimates and
            // asynchronous shaping. A temporary item alias can realize the
            // original chunk, but must not replace its reading bookmark.
            context.reflow = Some(Reflow {
                geometry: previous,
                rows: context.rows.clone(),
                top,
                follow_end: context.follow_end,
                preserve: context.preserve.is_some_and(|preserve| preserve.arrived),
                path: context
                    .rows
                    .iter()
                    .find(|row| {
                        row.id.starts_with("virtual-chunk-")
                            && row.bounds.y + row.bounds.height > top
                            && row.bounds.y < top + previous.viewport.height
                    })
                    .and_then(|row| context.paths.get(&row.id))
                    .cloned()
                    .unwrap_or_default(),
            });
        }
        if let Some(saved) = &mut context.reflow {
            saved.preserve |= context.preserve.is_some_and(|preserve| preserve.arrived);
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
        let top = match edge {
            Some(Edge::Start) => 0.0,
            Some(Edge::End | Edge::Latest) => next.travel(),
            None => context.position(next, rows, follow_end),
        };
        // Either explicit edge remains deliberate while a nonempty prepared
        // source fits, including a child that would otherwise start on growth.
        // A genuine empty reset consumes it.
        if next.travel() > 0.0 || rows.is_empty() {
            context.edge = None;
        }
        context.follow_end = follow_end && (rows.is_empty() || edge != Some(Edge::Start));
        let bottom = next.travel() - top;
        context.used = self.clock;
        context.remember(next, rows, top);
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
            self.pending_input = false;
        }
        (edge.is_some()
            || switched
            || freeze_reading
            || old_bottom.is_none_or(|old| (old - bottom).abs() > 0.5))
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
        for pass in 0..4 {
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
                .flat_map(|index| {
                    let context = &anchor.contexts[index];
                    context
                        .rows
                        .iter()
                        .chain(context.reflow.iter().flat_map(|saved| saved.rows.iter()))
                })
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
                mounted: HashSet::new(),
                wanted_guards: anchor
                    .active
                    .into_iter()
                    .flat_map(|index| {
                        let context = &anchor.contexts[index];
                        context.paths.values().flatten().copied().chain(
                            context
                                .reflow
                                .iter()
                                .flat_map(|saved| saved.path.iter().copied()),
                        )
                    })
                    .collect(),
                guards: HashMap::new(),
                path: vec![],
                paths: HashMap::new(),
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
                .map(|context| context.position(next, &rows, self.follow_end))
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
            if let Some(index) = anchor.active {
                anchor.contexts[index].paths = std::mem::take(&mut measured.paths);
            }
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
                if let Some(index) = anchor.active {
                    // A projected alias cannot prove that a fine source was
                    // removed. Only a settled, mounted parent without pending
                    // shaping can establish that its old chunk was replaced.
                    let mut mounted_items = MountedItems {
                        ids: &measured.ids,
                        items: HashSet::new(),
                        target: self.target,
                        root: self
                            .target
                            .map(|target| card_control_id(target.card, "chat-transcript")),
                        root_translation: None,
                        pending: vec![],
                        translation: Vector::ZERO,
                        clip: None,
                        pending_clip: None,
                    };
                    if anchor.contexts[index].reflow.is_some() {
                        self.content.as_widget_mut().operate(
                            child,
                            Layout::new(&node),
                            renderer,
                            &mut mounted_items,
                        );
                    }
                    anchor.contexts[index].finish_reflow(
                        &rows,
                        &measured.mounted,
                        &mounted_items.pending,
                        &mounted_items.items,
                        &measured.guards,
                    );
                }
                state.revealing &= self.restoration_pending;
                state.needs_layout = false;
                break;
            }
            // Keep every returned layout paired with the row geometry and
            // scroll correction measured above. An extra child layout after
            // the final pass would return new estimates without anchoring
            // them, and the next update could save that provisional offset.
            // Warm readers need the same bounded continuation as cold ones.
            state.needs_layout = !self.restoration_pending;
            if pass == 3 {
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
                        context.reflow = None;
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
                    context.reflow = None;
                    let top = (saved_top + top - before.0.unwrap_or(top))
                        .clamp(0.0, saved_geometry.travel());
                    context.top = Some(top);
                    let scroll = Scroll::new(
                        top,
                        saved_geometry.content.height,
                        saved_geometry.viewport.height,
                    );
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
                    let user_input = navigation_input(Some(saved_top), top, scroll, wheel, false)
                        || anchor.pending_input;
                    anchor.pending_input = false;
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
                        .filter(|_| self.observations.is_none() || user_input)
                    {
                        shell.publish(action(Action::Scrolled(
                            target,
                            self.subagent.map(str::to_owned),
                            scroll,
                            user_input,
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
                    if wheel_intent || !matches!(event, Event::Window(_)) {
                        context.reflow = None;
                    }
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
            let scroll = Scroll::new(top, geometry.content.height, geometry.viewport.height);
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

    // The draw counter observes actual widget traversal. It does not supply
    // viewport answers or keep any virtual row/tree alive.
    struct PresentationProbe {
        label: &'static str,
        height: f32,
        draws: Rc<std::cell::RefCell<Vec<&'static str>>>,
        clicks: Rc<Cell<usize>>,
    }
    impl Widget<Message, cosmic::Theme, cosmic::Renderer> for PresentationProbe {
        fn tag(&self) -> tree::Tag {
            tree::Tag::of::<iced_accessibility::Id>()
        }
        fn state(&self) -> tree::State {
            tree::State::new(iced_accessibility::Id::unique())
        }
        fn size(&self) -> Size<Length> {
            Size::new(Length::Fill, Length::Fixed(self.height))
        }
        fn layout(
            &mut self,
            _: &mut Tree,
            _: &cosmic::Renderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            layout::Node::new(Size::new(limits.max().width, self.height))
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
            self.draws.borrow_mut().push(self.label);
        }
        fn update(
            &mut self,
            _: &mut Tree,
            event: &Event,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &cosmic::Renderer,
            _: &mut dyn Clipboard,
            _: &mut Shell<'_, Message>,
            _: &Rectangle,
        ) {
            if matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_))) {
                self.clicks.set(self.clicks.get() + 1);
            }
        }
        fn mouse_interaction(
            &self,
            _: &Tree,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &Rectangle,
            _: &cosmic::Renderer,
        ) -> mouse::Interaction {
            mouse::Interaction::Pointer
        }
        fn a11y_nodes(
            &self,
            _: Layout<'_>,
            tree: &Tree,
            _: mouse::Cursor,
        ) -> iced_accessibility::A11yTree {
            let mut node = iced_accessibility::accesskit::Node::new(
                iced_accessibility::accesskit::Role::Button,
            );
            node.set_label(self.label);
            iced_accessibility::A11yTree::leaf(
                node,
                tree.state.downcast_ref::<iced_accessibility::Id>().clone(),
            )
        }
    }

    fn presentation_probe(
        label: &'static str,
        height: f32,
        draws: &Rc<std::cell::RefCell<Vec<&'static str>>>,
        clicks: &Rc<Cell<usize>>,
    ) -> Element<'static, Message> {
        Element::new(PresentationProbe {
            label,
            height,
            draws: draws.clone(),
            clicks: clicks.clone(),
        })
    }

    #[test]
    fn cold_reader_draw_waits_for_more_than_four_actual_virtual_viewport_passes() {
        fn nested(
            depth: u64,
            draws: Rc<std::cell::RefCell<Vec<&'static str>>>,
            clicks: Rc<Cell<usize>>,
        ) -> Element<'static, Message> {
            if depth == 0 {
                return presentation_probe("restored beginning", 400.0, &draws, &clicks);
            }
            let nested_draws = draws.clone();
            let nested_clicks = clicks.clone();
            let mut first = virtualized::Row::new(depth * 2, 0, 12_000.0, None, move || {
                nested(depth - 1, nested_draws.clone(), nested_clicks.clone())
            });
            first.anchor = true;
            virtualized::column(
                vec![
                    first,
                    virtualized::Row::new(depth * 2 + 1, 0, 6_000.0, None, move || {
                        presentation_probe("provisional end", 6_000.0, &draws, &clicks)
                    }),
                ],
                Default::default(),
            )
        }
        let mut renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let memory = Memory::default();
        memory.request_edge(None, Edge::Start);
        let draws = Rc::new(std::cell::RefCell::new(Vec::new()));
        let clicks = Rc::new(Cell::new(0));
        let mut element = transcript_observed(
            nested(10, draws.clone(), clicks.clone()),
            target,
            None,
            vec![],
            true,
            vec![],
            Observations::default(),
            memory,
            false,
            HashSet::new(),
            None,
        );
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(720.0, 400.0));
        let limits = layout::Limits::new(Size::ZERO, viewport.size());
        let mut node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(tree.state.downcast_ref::<ReaderState>().revealing);
        assert!(
            tree.state.downcast_ref::<ReaderState>().needs_layout,
            "real nested windows exceed the unchanged four-pass layout bound"
        );
        for _ in 0..8 {
            element.as_widget().draw(
                &tree,
                &mut renderer,
                &cosmic::Theme::dark(),
                &renderer::Style::default(),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &viewport,
            );
            assert!(
                draws.borrow().is_empty(),
                "provisional windows must not paint"
            );
            assert!(
                element
                    .as_widget()
                    .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable)
                    .root()
                    .is_empty()
            );
            assert_eq!(
                element.as_widget().mouse_interaction(
                    &tree,
                    Layout::new(&node),
                    mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
                    &viewport,
                    &renderer
                ),
                mouse::Interaction::None
            );
            assert!(
                element
                    .as_widget_mut()
                    .overlay(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &viewport,
                        Vector::ZERO
                    )
                    .is_none()
            );
            let mut messages = vec![];
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Layout::new(&node),
                mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut shell,
                &viewport,
            );
            assert_eq!(clicks.get(), 0, "unpainted text is not clickable");
            element.as_widget_mut().update(
                &mut tree,
                &Event::Window(iced::window::Event::RedrawRequested(
                    iced::time::Instant::now(),
                )),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut shell,
                &viewport,
            );
            assert!(
                shell.is_layout_invalid(),
                "cap exhaustion must make native progress"
            );
            assert_eq!(
                shell.redraw_request(),
                iced::window::RedrawRequest::NextFrame
            );
            node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            if !tree.state.downcast_ref::<ReaderState>().revealing {
                break;
            }
        }
        assert!(
            !tree.state.downcast_ref::<ReaderState>().revealing,
            "actual nested windows must eventually converge"
        );
        assert!(!tree.state.downcast_ref::<ReaderState>().needs_layout);
        let mut top = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut top);
        assert_eq!(top.0, Some(0.0));
        element.as_widget().draw(
            &tree,
            &mut renderer,
            &cosmic::Theme::dark(),
            &renderer::Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &viewport,
        );
        assert_eq!(&*draws.borrow(), &["restored beginning"]);
        assert_eq!(
            element.as_widget().mouse_interaction(
                &tree,
                Layout::new(&node),
                mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
                &viewport,
                &renderer
            ),
            mouse::Interaction::Pointer
        );
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut vec![]),
            &viewport,
        );
        assert!(
            clicks.get() > 0,
            "coherent content becomes interactive again"
        );
        assert!(
            !element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable)
                .root()
                .is_empty()
        );
    }

    #[test]
    fn warm_virtual_reader_keeps_returned_geometry_coherent_and_finishes_redraws() {
        fn nested(
            depth: u64,
            draws: Rc<std::cell::RefCell<Vec<&'static str>>>,
            clicks: Rc<Cell<usize>>,
        ) -> Element<'static, Message> {
            if depth == 0 {
                return presentation_probe("reading beginning", 400.0, &draws, &clicks);
            }
            let nested_draws = draws.clone();
            let nested_clicks = clicks.clone();
            let mut first = virtualized::Row::new(depth * 2, 0, 12_000.0, None, move || {
                nested(depth - 1, nested_draws.clone(), nested_clicks.clone())
            });
            first.anchor = true;
            virtualized::column(
                vec![
                    first,
                    virtualized::Row::new(depth * 2 + 1, 0, 6_000.0, None, move || {
                        presentation_probe("later content", 6_000.0, &draws, &clicks)
                    }),
                ],
                Default::default(),
            )
        }

        fn assert_native_geometry(
            element: &mut Element<'_, Message>,
            tree: &mut Tree,
            node: &layout::Node,
            renderer: &cosmic::Renderer,
            memory: &Memory,
        ) {
            // Read the bookmark before any operation or redraw can overwrite
            // it. Its geometry must describe the frame actually returned.
            let (remembered, remembered_top) = {
                let anchor = memory.0.borrow();
                let context = &anchor.contexts[anchor.active.unwrap()];
                (context.geometry.unwrap(), context.top.unwrap())
            };
            assert_eq!(remembered.viewport, node.size());
            assert_eq!(
                remembered.content,
                node.children()[0].size(),
                "the returned native frame must not outrun its retained reader geometry"
            );
            let mut actual = ReadTop::default();
            element
                .as_widget_mut()
                .operate(tree, Layout::new(node), renderer, &mut actual);
            assert!(
                (actual.0.unwrap() - remembered_top).abs() <= 0.5,
                "the returned native translation and retained reader must agree"
            );
        }

        let mut renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let memory = Memory::default();
        let observations = Observations::default();
        let draws = Rc::new(std::cell::RefCell::new(Vec::new()));
        let clicks = Rc::new(Cell::new(0));
        let make = |body| {
            transcript_observed(
                item("reader", body),
                target,
                None,
                vec![],
                true,
                vec![(item_id("reader"), "reader")],
                observations.clone(),
                memory.clone(),
                false,
                HashSet::new(),
                None,
            )
        };
        memory.request_edge(None, Edge::Start);
        let mut element = make(
            widget::column::with_children(vec![
                presentation_probe("reading beginning", 400.0, &draws, &clicks),
                widget::Space::new().height(6_000.0).into(),
            ])
            .into(),
        );
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(720.0, 400.0));
        let limits = layout::Limits::new(Size::ZERO, viewport.size());
        let initial = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget().draw(
            &tree,
            &mut renderer,
            &cosmic::Theme::dark(),
            &renderer::Style::default(),
            Layout::new(&initial),
            mouse::Cursor::Unavailable,
            &viewport,
        );
        assert_eq!(&*draws.borrow(), &["reading beginning"]);
        assert!(
            !tree.state.downcast_ref::<ReaderState>().revealing,
            "the changed reader has already been presented"
        );
        draws.borrow_mut().clear();

        // Real nested virtual windows replace the warm body in the same
        // native tree. Their estimates reveal more windows as they resolve.
        element = make(nested(10, draws.clone(), clicks));
        tree.diff(element.as_widget_mut());
        let mut node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut continued = 0;
        let mut settled = false;
        for _ in 0..32 {
            assert_native_geometry(&mut element, &mut tree, &node, &renderer, &memory);
            let mut pending = virtualized::UpdateViewport::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut pending);
            let mut messages = vec![];
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &Event::Window(iced::window::Event::RedrawRequested(
                    iced::time::Instant::now(),
                )),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut shell,
                &viewport,
            );
            if pending.changed {
                assert!(
                    shell.is_layout_invalid(),
                    "a warm reader with unresolved native windows must continue layout"
                );
                assert_eq!(
                    shell.redraw_request(),
                    iced::window::RedrawRequest::NextFrame,
                    "a warm reader must schedule the frame needed to finish its windows"
                );
                continued += 1;
            }
            if !shell.is_layout_invalid() {
                settled = true;
                break;
            }
            node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
        }
        assert!(continued > 0, "the fixture must require warm continuation");
        assert!(settled, "actual warm virtual windows must converge");
        assert_native_geometry(&mut element, &mut tree, &node, &renderer, &memory);
        let mut actual = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        assert_eq!(actual.0, Some(0.0), "the reading beginning stays in view");
        element.as_widget().draw(
            &tree,
            &mut renderer,
            &cosmic::Theme::dark(),
            &renderer::Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &viewport,
        );
        assert_eq!(&*draws.borrow(), &["reading beginning"]);
    }

    #[test]
    fn cold_pending_reader_is_unpainted_without_spinning_but_find_and_empty_settle() {
        let mut renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let memory = Memory::default();
        let draws = Rc::new(std::cell::RefCell::new(Vec::new()));
        let clicks = Rc::new(Cell::new(0));
        let navigation = find_navigation::State::default();
        let guard = navigation.guard();
        let make = |pending, height| {
            transcript_observed(
                item("body", presentation_probe("body", height, &draws, &clicks)),
                target,
                None,
                vec![],
                true,
                vec![(item_id("body"), "body")],
                Observations::default(),
                memory.clone(),
                pending,
                HashSet::new(),
                Some(guard.clone()),
            )
        };
        let viewport = Rectangle::with_size(Size::new(720.0, 400.0));
        let limits = layout::Limits::new(Size::ZERO, viewport.size());
        {
            let mut ready = make(false, 2_000.0);
            let mut tree = Tree::new(&ready);
            let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
            ready.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut SetBottom(500.0),
            );
        }
        let mut pending = make(true, 2_000.0);
        let mut tree = Tree::new(&pending);
        let node = pending
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(tree.state.downcast_ref::<ReaderState>().revealing);
        pending.as_widget().draw(
            &tree,
            &mut renderer,
            &cosmic::Theme::dark(),
            &renderer::Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &viewport,
        );
        assert!(draws.borrow().is_empty());
        let mut messages = vec![];
        let mut shell = Shell::new(&mut messages);
        pending.as_widget_mut().update(
            &mut tree,
            &Event::Window(iced::window::Event::RedrawRequested(
                iced::time::Instant::now(),
            )),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut shell,
            &viewport,
        );
        assert!(
            !shell.is_layout_invalid(),
            "source-pending alone cannot spin native layouts"
        );
        assert_eq!(shell.redraw_request(), iced::window::RedrawRequest::Wait);
        let mut find = ApplyFind {
            target,
            scope: TranscriptScope::default(),
            scroll: card_control_id(target.card, "chat-transcript"),
            location: LogicalLocation {
                offset: 800.0,
                exact: true,
                stage: 3,
                shape_pending: None,
                horizontal: vec![],
            },
            intent: guard.clone(),
            document: Some(navigation.document_epoch()),
        };
        pending
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut find);
        assert!(guard.current());
        assert!(
            !tree.state.downcast_ref::<ReaderState>().restore,
            "Find operations remain active while presentation is hidden"
        );
        // Find can accept a native correction before source readiness. Its
        // next layout must not turn that correction into presentation readiness.
        let mut still_pending = make(true, 2_000.0);
        tree.diff(still_pending.as_widget_mut());
        let pending_node = still_pending
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(tree.state.downcast_ref::<ReaderState>().revealing);
        still_pending.as_widget().draw(
            &tree,
            &mut renderer,
            &cosmic::Theme::dark(),
            &renderer::Style::default(),
            Layout::new(&pending_node),
            mouse::Cursor::Unavailable,
            &viewport,
        );
        assert!(draws.borrow().is_empty());
        assert!(
            still_pending
                .as_widget()
                .a11y_nodes(
                    Layout::new(&pending_node),
                    &tree,
                    mouse::Cursor::Unavailable
                )
                .root()
                .is_empty()
        );
        let mut shell = Shell::new(&mut messages);
        still_pending.as_widget_mut().update(
            &mut tree,
            &Event::Window(iced::window::Event::RedrawRequested(
                iced::time::Instant::now(),
            )),
            Layout::new(&pending_node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut shell,
            &viewport,
        );
        assert!(!shell.is_layout_invalid());
        assert_ne!(
            shell.redraw_request(),
            iced::window::RedrawRequest::NextFrame
        );
        let mut ready = make(false, 2_000.0);
        tree.diff(ready.as_widget_mut());
        let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
        assert!(!tree.state.downcast_ref::<ReaderState>().revealing);
        let mut top = ReadTop::default();
        ready
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut top);
        assert_eq!(top.0, Some(800.0));
        ready.as_widget().draw(
            &tree,
            &mut renderer,
            &cosmic::Theme::dark(),
            &renderer::Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &viewport,
        );
        assert_eq!(&*draws.borrow(), &["body"]);
        // A pending explicit edge in an already visible context must not blank it.
        memory.request_edge(None, Edge::Latest);
        let mut visible_pending = make(true, 2_000.0);
        tree.diff(visible_pending.as_widget_mut());
        visible_pending
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(!tree.state.downcast_ref::<ReaderState>().revealing);
        // A new, genuinely empty reader has no geometry to wait for.
        let mut empty = transcript_observed(
            widget::column::with_children(Vec::<Element<'_, Message>>::new()),
            target,
            None,
            vec![],
            true,
            vec![],
            Observations::default(),
            memory.clone(),
            false,
            HashSet::new(),
            None,
        );
        let mut empty_tree = Tree::new(&empty);
        empty
            .as_widget_mut()
            .layout(&mut empty_tree, &renderer, &limits);
        assert!(!empty_tree.state.downcast_ref::<ReaderState>().revealing);
        assert!(!empty_tree.state.downcast_ref::<ReaderState>().needs_layout);
        assert_eq!(memory.edge(None), None);
        assert!(memory.0.borrow().contexts[0].rows.is_empty());
    }

    #[test]
    fn wheel_during_cold_reveal_moves_from_saved_reader_not_provisional_end() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let viewport = Rectangle::with_size(Size::new(720.0, 400.0));
        let limits = layout::Limits::new(Size::ZERO, viewport.size());
        let mut final_tops = vec![];
        for pending in [false, true] {
            let memory = Memory::default();
            let make = |pending| {
                transcript_observed(
                    item(
                        "body",
                        widget::Space::new().height(2_000).width(Length::Fill),
                    ),
                    target,
                    None,
                    vec![],
                    true,
                    vec![(item_id("body"), "body")],
                    Observations::default(),
                    memory.clone(),
                    pending,
                    HashSet::new(),
                    None,
                )
            };
            {
                let mut initial = make(false);
                let mut tree = Tree::new(&initial);
                let node = initial
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
                initial.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut SetBottom(500.0),
                );
            }
            let mut reader = make(pending);
            let mut tree = Tree::new(&reader);
            let node = reader.as_widget_mut().layout(&mut tree, &renderer, &limits);
            let mut before = ReadTop::default();
            reader
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut before);
            assert_eq!(
                before.0,
                Some(if pending { 1_600.0 } else { 1_100.0 }),
                "cold native default is deliberately different from the saved middle"
            );
            reader.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
                }),
                Layout::new(&node),
                mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut vec![]),
                &viewport,
            );
            let mut ready = make(false);
            tree.diff(ready.as_widget_mut());
            let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
            let mut after = ReadTop::default();
            ready
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
            final_tops.push(after.0.unwrap());
        }
        assert!(
            final_tops[0] > 1_100.0 && final_tops[0] < 1_600.0,
            "real native wheel actually moves the visible control toward the end"
        );
        assert_eq!(
            final_tops[0], final_tops[1],
            "pending return preserves exactly the same reader-relative wheel movement"
        );
    }

    #[test]
    fn cold_reader_wheel_reports_saved_geometry_and_dispatches_real_history_edges() {
        let renderer = crate::app::tests::headless_renderer();
        let viewport = Rectangle::with_size(Size::new(720.0, 400.0));
        let limits = layout::Limits::new(Size::ZERO, viewport.size());
        // The third case's conflicting axes require the modifier cached while
        // source-pending; using the old unshifted y would miss Earlier.
        for (bottom, shifted) in [(false, false), (true, false), (false, true)] {
            if shifted && cfg!(target_os = "macos") {
                continue;
            }
            let (mut live, handle) = view::prepared_runtime_for_test(&[]);
            let mut app = crate::app::tests::context_inspector_test_app(true);
            let mut target = live.target;
            target.card = app.workspace.focused().unwrap();
            live.target = target;
            app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.has_earlier = true;
            snapshot.has_later = true;
            handle.replace_snapshot_for_test(snapshot);
            live.refresh();
            let memory = live.scroll_memory.clone();
            let observations = live.observed_scroll.clone();
            app.cards
                .insert(target.card, CardRuntime::Chat(Box::new(live)));
            let make = |pending, height| {
                transcript_observed(
                    item(
                        "body",
                        widget::Space::new().height(height).width(Length::Fill),
                    ),
                    target,
                    None,
                    vec![],
                    true,
                    vec![(item_id("body"), "body")],
                    observations.clone(),
                    memory.clone(),
                    pending,
                    HashSet::new(),
                    None,
                )
            };
            {
                let mut ready = make(false, 2_000.0);
                let mut tree = Tree::new(&ready);
                let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
                ready.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut SetBottom(if bottom { 0.0 } else { 1_600.0 }),
                );
            }
            // A fitting cold placeholder cannot be mistaken for authoritative
            // fitting history: outward paging uses retained 2000/400 geometry.
            let mut pending = make(true, 20.0);
            let mut tree = Tree::new(&pending);
            let node = pending
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut messages = vec![];
            if shifted {
                pending.as_widget_mut().update(
                    &mut tree,
                    &Event::Keyboard(iced::keyboard::Event::ModifiersChanged(
                        iced::keyboard::Modifiers::SHIFT,
                    )),
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut iced::advanced::clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
                assert!(memory.0.borrow().modifiers.shift());
            }
            pending.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Lines {
                        x: if shifted { 1.0 } else { 0.0 },
                        y: if bottom || shifted { -1.0 } else { 1.0 },
                    },
                }),
                Layout::new(&node),
                mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            let reports: Vec<_> = messages.into_iter().filter(|message| matches!(message,
                Message::ChatUi(Action::Scrolled(actual, None, _, true)) if *actual == target
            )).collect();
            assert_eq!(
                reports.len(),
                1,
                "outward wheel must publish real reader intent"
            );
            let Message::ChatUi(Action::Scrolled(_, _, scroll, _)) = &reports[0] else {
                unreachable!()
            };
            assert_eq!(scroll.content(), 2_000.0);
            assert_eq!(scroll.window_height(), 400.0);
            assert_eq!(scroll.from_top(), if bottom { 1_600.0 } else { 0.0 });
            assert_eq!(observations.borrow()[&None], *scroll);
            drop(pending);
            drop(app.update(reports.into_iter().next().unwrap()));
            let runtime = app.chat_runtime_mut(target).unwrap();
            assert_eq!(
                runtime.pending.as_ref().and_then(|pending| pending.page),
                Some(if bottom {
                    view::Page::Newer
                } else {
                    view::Page::Earlier
                }),
                "actual App consumes the emitted wheel intent and starts the matching provider page"
            );
        }
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
                false,
                HashSet::new(),
                None,
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
    fn cold_reader_restore_ignores_empty_and_same_item_placeholder_layouts() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let memory = Memory::default();
        let observations = Observations::default();
        let make = |pending, height| {
            let ids = if height == 0 {
                vec![]
            } else {
                vec!["first", "middle", "last"]
            };
            transcript_observed(
                widget::column::with_children(
                    ids.iter()
                        .map(|id| item(id, widget::Space::new().height(height).width(Length::Fill)))
                        .collect::<Vec<_>>(),
                ),
                target,
                None,
                vec![],
                true,
                ids.into_iter().map(|id| (item_id(id), id)).collect(),
                observations.clone(),
                memory.clone(),
                pending,
                HashSet::new(),
                None,
            )
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
        {
            let mut element = make(false, 500);
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
            // Deliberately drop before another layout: operations must commit
            // actual native geometry when it changes, just as wheel input does.
        }
        assert_eq!(memory.0.borrow().contexts[0].top, Some(600.0));
        let mut pending = make(true, 0);
        let mut tree = Tree::new(&pending);
        for height in [0, 20, 20] {
            pending = make(true, height);
            tree.diff(pending.as_widget_mut());
            let node = pending
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            pending.as_widget_mut().update(
                &mut tree,
                &Event::Window(iced::window::Event::RedrawRequested(
                    iced::time::Instant::now(),
                )),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut vec![]),
                &Rectangle::with_size(Size::new(720.0, 400.0)),
            );
            assert_eq!(memory.0.borrow().contexts[0].top, Some(600.0));
            assert_eq!(
                memory.0.borrow().contexts[0]
                    .geometry
                    .unwrap()
                    .content
                    .height,
                1500.0
            );
            assert!(tree.state.downcast_ref::<ReaderState>().restore);
            assert!(
                observations.borrow().is_empty(),
                "temporary geometry cannot replace runtime observations"
            );
        }
        let mut ready = make(false, 500);
        tree.diff(ready.as_widget_mut());
        let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let mut actual = ReadTop::default();
        ready
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        assert_eq!(actual.0, Some(600.0));
        // An authoritative empty replacement is different from hydration wait.
        let mut empty = make(false, 0);
        tree.diff(empty.as_widget_mut());
        empty.as_widget_mut().layout(&mut tree, &renderer, &limits);
        assert_eq!(memory.0.borrow().contexts[0].top, Some(0.0));
        assert!(memory.0.borrow().contexts[0].rows.is_empty());
        let mut replacement = make(false, 500);
        tree.diff(replacement.as_widget_mut());
        let node = replacement
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut actual = ReadTop::default();
        replacement
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        assert_eq!(
            actual.0,
            Some(1100.0),
            "a genuinely empty transcript resets end-follow"
        );
    }

    #[test]
    fn actual_native_wheel_survives_tree_drop_before_the_next_layout() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let memory = Memory::default();
        let observations = Observations::default();
        let make = || {
            transcript_observed(
                item(
                    "body",
                    widget::Space::new().height(2000).width(Length::Fill),
                ),
                target,
                None,
                vec![],
                true,
                vec![(item_id("body"), "body")],
                observations.clone(),
                memory.clone(),
                false,
                HashSet::new(),
                None,
            )
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
        {
            let mut element = make();
            let mut tree = Tree::new(&element);
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Lines { x: 0.0, y: 5.0 },
                }),
                Layout::new(&node),
                mouse::Cursor::Available(iced::Point::new(100.0, 100.0)),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut vec![]),
                &Rectangle::with_size(Size::new(720.0, 400.0)),
            );
        }
        let expected = observations.borrow()[&None].from_top();
        assert!(
            expected < 1600.0 && expected > 0.0,
            "the real native wheel must move the reader"
        );
        let mut restored = make();
        let mut tree = Tree::new(&restored);
        let node = restored
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut actual = ReadTop::default();
        restored
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        assert_eq!(actual.0, Some(expected));
    }

    #[test]
    fn pending_child_switch_and_current_find_preserve_their_own_native_reader_context() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 3,
        };
        let memory = Memory::default();
        let navigation = find_navigation::State::default();
        let make = |context: Option<&'static str>, pending, height| {
            transcript_observed(
                widget::column::with_children(
                    ["first", "middle", "last"]
                        .into_iter()
                        .map(|id| item(id, widget::Space::new().height(height).width(Length::Fill)))
                        .collect::<Vec<_>>(),
                ),
                target,
                context,
                vec![],
                true,
                ["first", "middle", "last"]
                    .into_iter()
                    .map(|id| (item_id(id), id))
                    .collect(),
                Observations::default(),
                memory.clone(),
                pending,
                HashSet::new(),
                Some(navigation.guard()),
            )
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
        let mut element = make(None, false, 500);
        let mut tree = Tree::new(&element);
        for (context, bottom) in [(None, 500.0), (Some("child"), 300.0)] {
            element = make(context, false, 500);
            tree.diff(element.as_widget_mut());
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut SetBottom(bottom),
            );
        }
        assert!(
            !memory.restoration_pending(None, false, |id| id == "last"),
            "an unrelated cold tail cannot block the saved window"
        );
        assert!(memory.restoration_pending(None, false, |id| id == "middle"));
        // This is a context switch within a retained native Tree, not merely
        // restoration after creating a fresh Tree.
        element = make(None, true, 20);
        tree.diff(element.as_widget_mut());
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert_eq!(
            memory
                .0
                .borrow()
                .contexts
                .iter()
                .find(|c| c.subagent.is_none())
                .unwrap()
                .top,
            Some(600.0)
        );
        assert_eq!(
            memory
                .0
                .borrow()
                .contexts
                .iter()
                .find(|c| c.subagent.as_deref() == Some("child"))
                .unwrap()
                .top,
            Some(800.0)
        );
        element = make(None, false, 500);
        tree.diff(element.as_widget_mut());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut actual = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        assert_eq!(actual.0, Some(600.0));
        drop(tree);
        memory.request_edge(None, Edge::End);
        let mut pending = make(None, true, 500);
        let mut tree = Tree::new(&pending);
        let node = pending
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let stale = navigation.document_epoch();
        navigation.changed_document();
        let apply = |document| ApplyFind {
            target,
            scope: TranscriptScope::default(),
            scroll: card_control_id(target.card, "chat-transcript"),
            location: LogicalLocation {
                offset: 200.0,
                exact: true,
                stage: 0,
                shape_pending: None,
                horizontal: vec![],
            },
            intent: navigation.guard(),
            document: Some(document),
        };
        pending.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut apply(stale),
        );
        assert!(
            tree.state.downcast_ref::<ReaderState>().restore,
            "stale Find cannot replace the pending saved reader"
        );
        assert_eq!(memory.edge(None), Some(Edge::End));
        pending.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut apply(navigation.document_epoch()),
        );
        assert!(
            !tree.state.downcast_ref::<ReaderState>().restore,
            "an accepted current Find owns the new native position"
        );
        assert_eq!(
            memory.edge(None),
            None,
            "only a current accepted Find supersedes the pending edge"
        );
        let mut ready = make(None, false, 500);
        tree.diff(ready.as_widget_mut());
        let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let mut actual = ReadTop::default();
        ready
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        assert_eq!(
            actual.0,
            Some(900.0),
            "cold completion must not undo the accepted Find correction"
        );
    }

    #[test]
    fn superseded_latest_refresh_keeps_native_find_and_overlap_without_waiting_for_unrelated_cold_rows()
     {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 3,
        };
        let navigation = find_navigation::State::default();
        let memory = Memory::default();
        let make = |context, ids: Vec<&'static str>, height, preparing| {
            transcript_observed(
                widget::column::with_children(
                    ids.iter()
                        .map(|id| item(id, widget::Space::new().height(height).width(Length::Fill)))
                        .collect::<Vec<_>>(),
                ),
                target,
                context,
                vec![],
                true,
                ids.into_iter().map(|id| (item_id(id), id)).collect(),
                Observations::default(),
                memory.clone(),
                false,
                preparing,
                Some(navigation.guard()),
            )
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
        let old = vec!["first", "middle", "last"];
        let mut element = make(None, old.clone(), 500, HashSet::new());
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
        memory.preserve_refresh(42, 10);
        let document = navigation.document_epoch();
        navigation.changed_document();
        let apply = |document| ApplyFind {
            target,
            scope: TranscriptScope::default(),
            scroll: card_control_id(target.card, "chat-transcript"),
            location: LogicalLocation {
                offset: 200.0,
                exact: true,
                stage: 0,
                shape_pending: None,
                horizontal: vec![],
            },
            intent: navigation.guard(),
            document: Some(document),
        };
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut apply(document),
        );
        let mut actual = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        assert_eq!(actual.0, Some(600.0), "stale Find cannot change reader");
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut apply(navigation.document_epoch()),
        );
        let mut actual = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        assert_eq!(actual.0, Some(900.0), "current accepted native Find wins");
        memory.latest_source(Some(9), 11, false, false);
        assert!(
            !memory.preserve_arrived(None),
            "wrong refresh epoch cannot authorize replacement fallback"
        );
        memory.cancel_preserve(41);
        memory.latest_source(Some(10), 11, true, false);
        assert!(
            memory.preserve_arrived(None),
            "intermediate replacement also preserves reader"
        );
        memory.latest_source(None, 12, false, false);
        assert!(memory.preserve_arrived(None));
        assert!(!memory.preserve_arrived(Some("child")));
        element = make(Some("child"), old.clone(), 500, HashSet::new());
        tree.diff(element.as_widget_mut());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(300.0),
        );
        let replacement = vec![
            "replacement-first",
            "replacement-middle",
            "replacement-last",
        ];
        element = make(
            None,
            replacement.clone(),
            20,
            replacement.iter().copied().collect(),
        );
        tree.diff(element.as_widget_mut());
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(memory.preserve_arrived(None));
        assert_eq!(
            memory
                .0
                .borrow()
                .contexts
                .iter()
                .find(|context| context.subagent.is_none())
                .unwrap()
                .top,
            Some(900.0),
            "cold replacement cannot overwrite the accepted Find geometry"
        );
        element = make(None, replacement, 500, HashSet::from(["replacement-first"]));
        tree.diff(element.as_widget_mut());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut actual = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        assert_eq!(
            actual.0,
            Some(900.0),
            "unrelated cold first row cannot block Ready replacement viewport"
        );
        assert!(
            !memory.preserve_arrived(None),
            "one arrived replacement consumes the bounded marker"
        );
        // With overlapping IDs, row anchoring takes precedence over numeric fallback.
        memory.clear();
        element = make(None, old.clone(), 500, HashSet::new());
        tree.diff(element.as_widget_mut());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(500.0),
        );
        memory.preserve_refresh(43, 11);
        memory.latest_source(Some(11), 12, false, false);
        element = make(
            None,
            vec!["prefix", "first", "middle", "last"],
            500,
            HashSet::new(),
        );
        tree.diff(element.as_widget_mut());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut actual = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        assert_eq!(
            actual.0,
            Some(1100.0),
            "surviving row moves by prepend delta, not saved numeric top"
        );
        memory.preserve_refresh(44, 12);
        memory.cancel_preserve(44);
        memory.latest_source(Some(12), 13, false, false);
        assert!(
            !memory.preserve_arrived(None),
            "failed exact refresh cannot authorize later unrelated source"
        );
    }

    #[test]
    fn cold_fitting_native_home_end_and_wheel_keep_explicit_context_intent() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 9,
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
        for context in [None, Some("child")] {
            for (edge, wheel_cancels) in
                [(Edge::Start, false), (Edge::End, false), (Edge::End, true)]
            {
                let memory = Memory::default();
                let make = |pending, height| {
                    transcript_observed(
                        widget::column::with_children(
                            ["first", "middle", "last"]
                                .into_iter()
                                .map(|id| {
                                    item(
                                        id,
                                        widget::Space::new().height(height).width(Length::Fill),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        ),
                        target,
                        context,
                        vec![],
                        true,
                        ["first", "middle", "last"]
                            .into_iter()
                            .map(|id| (item_id(id), id))
                            .collect(),
                        Observations::default(),
                        memory.clone(),
                        pending,
                        HashSet::new(),
                        None,
                    )
                };
                {
                    let mut element = make(false, 500);
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
                }
                memory.request_edge(context, edge);
                let mut pending = make(true, 20);
                let mut tree = Tree::new(&pending);
                let node = pending
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
                let mut snap = operation::scrollable::snap_to::<()>(
                    card_control_id(target.card, "chat-transcript"),
                    iced::widget::scrollable::RelativeOffset {
                        x: None,
                        y: Some(if edge == Edge::Start { 1.0 } else { 0.0 }),
                    },
                );
                let mut before = ReadTop::default();
                pending.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut before,
                );
                pending.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut snap,
                );
                let mut after = ReadTop::default();
                pending.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut after,
                );
                assert_eq!(
                    (before.0, after.0),
                    (Some(0.0), Some(0.0)),
                    "fitting native operation has no translation change"
                );
                assert_eq!(memory.edge(context), Some(edge));
                assert!(tree.state.downcast_ref::<ReaderState>().revealing);
                if wheel_cancels {
                    pending.as_widget_mut().update(
                        &mut tree,
                        &Event::Mouse(mouse::Event::WheelScrolled {
                            delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
                        }),
                        Layout::new(&node),
                        mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
                        &renderer,
                        &mut iced::advanced::clipboard::Null,
                        &mut Shell::new(&mut vec![]),
                        &Rectangle::with_size(Size::new(720.0, 400.0)),
                    );
                    assert_eq!(
                        memory.edge(context),
                        None,
                        "a later deliberate native wheel supersedes the pending edge"
                    );
                }
                if !wheel_cancels {
                    let mut fitting_ready = make(false, 20);
                    tree.diff(fitting_ready.as_widget_mut());
                    fitting_ready
                        .as_widget_mut()
                        .layout(&mut tree, &renderer, &limits);
                    assert_eq!(
                        memory.edge(context),
                        Some(edge),
                        "authoritative fitting child/parent Ready keeps either explicit edge through later growth"
                    );
                }
                let mut ready = make(false, 500);
                tree.diff(ready.as_widget_mut());
                let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
                let mut actual = ReadTop::default();
                ready.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut actual,
                );
                assert_eq!(
                    actual.0,
                    Some(if wheel_cancels {
                        600.0
                    } else if edge == Edge::Start {
                        0.0
                    } else {
                        1100.0
                    })
                );
                assert_eq!(memory.edge(context), None);
                memory.request_edge(context, Edge::End);
                let mut empty = transcript_observed(
                    widget::Space::new().height(0).width(Length::Fill),
                    target,
                    context,
                    vec![],
                    true,
                    vec![],
                    Observations::default(),
                    memory.clone(),
                    false,
                    HashSet::new(),
                    None,
                );
                tree.diff(empty.as_widget_mut());
                empty.as_widget_mut().layout(&mut tree, &renderer, &limits);
                assert_eq!(
                    memory.edge(context),
                    None,
                    "an authoritative truly empty transcript consumes the pending edge"
                );
                memory.request_edge(Some("child"), Edge::Start);
                memory.request_edge(None, Edge::End);
                assert_eq!(memory.edge(Some("child")), Some(Edge::Start));
                memory.clear();
                assert_eq!(memory.edge(Some("child")), None);
                assert_eq!(memory.edge(context), None);
            }
        }
    }

    #[test]
    fn queued_edge_operation_refuses_new_generation_and_source_document() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 2,
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
        for changed_generation in [false, true] {
            let navigation = find_navigation::State::default();
            let mut queued = ApplyEdge {
                target,
                scope: TranscriptScope::default(),
                edge: Edge::Start,
                intent: navigation.guard(),
                document: navigation.document_epoch(),
            };
            let current = if changed_generation {
                Target {
                    generation: target.generation + 1,
                    ..target
                }
            } else {
                navigation.changed_document();
                target
            };
            let mut element = transcript_observed(
                item(
                    "body",
                    widget::Space::new().height(2000).width(Length::Fill),
                ),
                current,
                None,
                vec![],
                true,
                vec![(item_id("body"), "body")],
                Observations::default(),
                Memory::default(),
                false,
                HashSet::new(),
                None,
            );
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
            let mut before = ReadTop::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut before);
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut queued);
            let mut after = ReadTop::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
            assert_eq!(before.0, Some(1100.0));
            assert_eq!(
                after.0, before.0,
                "a held typed edge task cannot scroll a newer target or source document"
            );
        }
    }

    fn native_logical_location(
        element: &mut Element<'_, Message>,
        tree: &mut Tree,
        node: &layout::Node,
        renderer: &cosmic::Renderer,
        target: Target,
        identity: u64,
        byte: usize,
    ) -> Option<LogicalLocation> {
        let result = Arc::new(Mutex::new(None));
        let sink = result.clone();
        let mut locate = operation::map(
            LocateLogical {
                target,
                scroll: card_control_id(target.card, "chat-transcript"),
                item: item_id("opaque-item"),
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
            },
            move |location| {
                *sink.lock().unwrap() = location;
            },
        );
        element
            .as_widget_mut()
            .operate(tree, Layout::new(node), renderer, &mut locate);
        let _ = locate.finish();
        result.lock().unwrap().clone()
    }

    #[test]
    fn find_geometry_and_corrections_stay_in_the_exact_mounted_transcript() {
        #[derive(Default)]
        struct Offsets(HashMap<Id, Vector>);
        impl Operation for Offsets {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                id: Option<&Id>,
                _: Rectangle,
                _: Rectangle,
                translation: Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                if let Some(id) = id {
                    self.0.insert(id.clone(), translation);
                }
            }
        }
        let renderer = crate::app::tests::headless_renderer();
        let wanted = Target {
            card: CardId::new(),
            generation: 1,
        };
        let neighbor = Target {
            card: CardId::new(),
            generation: 1,
        };
        let identity = selection::logical_identity("opaque-item", &[0, 0]);
        let byte = "wanted line\n".len() * 1500;
        let wanted_state = find_navigation::State::default();
        let neighbor_state = find_navigation::State::default();
        let neighbor_guard = neighbor_state.guard();
        let horizontal_id = block_id(neighbor, None, identity);
        let wanted_body = prose::code_text(
            Arc::from("wanted line\n".repeat(5000)),
            Arc::from([]),
            iced::Pixels(14.0),
            iced::Font::MONOSPACE,
            wanted,
            None,
            Some(selectable_prose::Logical { identity, start: 0 }),
            None,
            None,
        );
        let owned_horizontal_id = block_id(wanted, None, identity);
        let wanted_body: Element<'_, Message> =
            widget::scrollable(widget::row::with_children(vec![
                widget::Space::new().width(1500).into(),
                wanted_body,
            ]))
            .direction(iced::widget::scrollable::Direction::Horizontal(
                design::scrollbar(),
            ))
            .id(owned_horizontal_id.clone())
            .width(Length::Fill)
            .into();
        let neighbor_body = widget::scrollable(widget::row::with_children(vec![
            widget::Space::new().width(3000).into(),
            selectable_prose::logical_text(
                vec![iced::widget::text::Span::new("neighbor glyph".to_owned())].into(),
                iced::Pixels(14.0),
                iced::Font::MONOSPACE,
                neighbor,
                None,
                selectable_prose::Logical {
                    identity,
                    start: byte,
                },
            ),
        ]))
        .direction(iced::widget::scrollable::Direction::Horizontal(
            design::scrollbar(),
        ))
        .id(horizontal_id.clone());
        let pane = |body, target, guard| {
            widget::container(transcript_observed(
                item("opaque-item", body),
                target,
                None,
                vec![],
                true,
                vec![],
                Observations::default(),
                Memory::default(),
                false,
                HashSet::new(),
                Some(guard),
            ))
            .width(400)
            .height(400)
            .into()
        };
        let mut element: Element<'_, Message> = widget::row::with_children(vec![
            pane(wanted_body, wanted, wanted_state.guard()),
            pane(neighbor_body.into(), neighbor, neighbor_guard.clone()),
        ])
        .into();
        let limits = layout::Limits::new(Size::ZERO, Size::new(800.0, 400.0));
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let location = native_logical_location(
            &mut element,
            &mut tree,
            &node,
            &renderer,
            wanted,
            identity,
            byte,
        )
        .expect("wanted actual transcript geometry");
        assert!(
            !location.exact && location.stage == 1,
            "an unrelated mounted exact glyph cannot replace the wanted virtual descriptor"
        );
        assert!(
            location
                .horizontal
                .iter()
                .all(|(id, _)| *id != horizontal_id),
            "neighbor reader IDs never enter the wanted result"
        );
        let mut before = Offsets::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut before);
        let mut defense = location.clone();
        defense.horizontal.push((horizontal_id.clone(), 1500.0));
        let mut apply = ApplyFind {
            target: wanted,
            scope: TranscriptScope::default(),
            scroll: card_control_id(wanted.card, "chat-transcript"),
            location: defense,
            intent: wanted_state.guard(),
            document: Some(wanted_state.document_epoch()),
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut apply);
        let mut after = Offsets::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
        assert_ne!(
            before.0[&card_control_id(wanted.card, "chat-transcript")].y,
            after.0[&card_control_id(wanted.card, "chat-transcript")].y,
            "positive control changes the wanted native reader"
        );
        assert_eq!(
            before.0[&horizontal_id], after.0[&horizontal_id],
            "even a captured foreign reader ID cannot scroll a neighboring card"
        );
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &Event::Window(iced::window::Event::RedrawRequested(
                iced::time::Instant::now(),
            )),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(Size::new(800.0, 400.0)),
        );
        assert!(
            neighbor_guard.current(),
            "neighboring Find intent is unchanged"
        );
        assert!(
            !messages.iter().any(|message| matches!(message,
            Message::ChatUi(Action::Scrolled(target, _, _, true)) if *target == neighbor)),
            "a correction cannot mark neighboring navigation as user input"
        );
        let mut own_marker = FindCorrection {
            target: Some(wanted),
            corrected: false,
        };
        let mut foreign_marker = FindCorrection {
            target: Some(neighbor),
            corrected: false,
        };
        <ApplyFind as Operation<()>>::custom(
            &mut apply,
            None,
            Rectangle::default(),
            &mut own_marker,
        );
        <ApplyFind as Operation<()>>::custom(
            &mut apply,
            None,
            Rectangle::default(),
            &mut foreign_marker,
        );
        assert!(
            own_marker.corrected && !foreign_marker.corrected,
            "the pre-transcript correction marker is bound to its exact Target"
        );
        let stale_target = Target {
            generation: wanted.generation + 1,
            ..wanted
        };
        assert!(
            native_logical_location(
                &mut element,
                &mut tree,
                &node,
                &renderer,
                stale_target,
                identity,
                byte
            )
            .is_none(),
            "a stale-generation tree supplies no coarse, virtual or exact Find geometry"
        );
        let mut stale = ApplyFind {
            target: stale_target,
            scope: TranscriptScope::default(),
            scroll: card_control_id(wanted.card, "chat-transcript"),
            location: LogicalLocation {
                offset: 0.0,
                ..location
            },
            intent: wanted_state.guard(),
            document: Some(wanted_state.document_epoch()),
        };
        let mut before = Offsets::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut before);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut stale);
        let mut after = Offsets::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
        assert_eq!(
            before.0, after.0,
            "a stale-generation operation cannot mutate native readers"
        );
        let mut marker = FindCorrection {
            target: Some(wanted),
            corrected: false,
        };
        <ApplyFind as Operation<()>>::custom(&mut stale, None, Rectangle::default(), &mut marker);
        assert!(
            !marker.corrected,
            "a stale-generation operation cannot mark a correction"
        );
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        use iced::futures::StreamExt;
        let mut task = iced::runtime::task::into_stream(find_anchor(
            stale_target,
            item_id("opaque-item"),
            wanted_state.guard(),
        ))
        .unwrap();
        let effect = executor.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), task.next())
                .await
                .unwrap()
                .unwrap()
        });
        let iced::runtime::Action::Widget(mut coarse) = effect else {
            panic!("native anchor operation");
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, coarse.as_mut());
        assert!(
            matches!(coarse.finish(), operation::Outcome::None),
            "a stale-generation coarse anchor cannot queue a scroll correction"
        );
        executor.shutdown_background();
        for (offset, horizontal) in [
            (f32::INFINITY, None),
            (f32::NEG_INFINITY, None),
            (f32::NAN, None),
            (0.0, Some(f32::INFINITY)),
            (0.0, Some(f32::NEG_INFINITY)),
            (0.0, Some(f32::NAN)),
        ] {
            let mut invalid = ApplyFind {
                target: wanted,
                scope: TranscriptScope::default(),
                scroll: card_control_id(wanted.card, "chat-transcript"),
                location: LogicalLocation {
                    offset,
                    exact: false,
                    stage: 0,
                    shape_pending: None,
                    horizontal: horizontal
                        .map(|x| vec![(owned_horizontal_id.clone(), x)])
                        .unwrap_or_default(),
                },
                intent: wanted_state.guard(),
                document: Some(wanted_state.document_epoch()),
            };
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut invalid);
            let mut rejected = Offsets::default();
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut rejected,
            );
            assert_eq!(
                after.0, rejected.0,
                "nonfinite Find offsets cannot mutate a native reader"
            );
            let mut marker = FindCorrection {
                target: Some(wanted),
                corrected: false,
            };
            <ApplyFind as Operation<()>>::custom(
                &mut invalid,
                None,
                Rectangle::default(),
                &mut marker,
            );
            assert!(!marker.corrected);
        }
    }

    #[test]
    fn appended_flow_prefix_is_not_exact_find_geometry_before_redraw_requests_current_source() {
        fn paragraph(content: &markdown::Content) -> markdown::Text {
            let markdown::Item::Paragraph(text) = &content.items()[0] else {
                panic!("paragraph");
            };
            text.clone()
        }
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 1,
        };
        let identity = selection::logical_identity("opaque-item", &[0, 0]);
        let style = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE).style;
        let mut content = markdown::Content::parse(&"prefix needle words ".repeat(300));
        let cache = flow_layout::Cache::default();
        let entry = cache.markdown_entry(identity, paragraph(&content), style);
        let make = |entry| {
            transcript(
                item(
                    "opaque-item",
                    selectable_prose::flow_source(
                        iced::Pixels(14.0),
                        iced::Font::default(),
                        target,
                        None,
                        selectable_prose::Logical { identity, start: 0 },
                        entry,
                    ),
                ),
                target,
                None,
                vec![],
                true,
                vec![],
            )
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
        let mut element = make(entry.clone());
        let mut tree = Tree::new(&element);
        let _ = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(
            cache.complete_fixture(),
            "initial real geometry fixture completed"
        );
        element = make(entry.clone());
        tree.diff(element.as_widget_mut());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let ready = native_logical_location(
            &mut element,
            &mut tree,
            &node,
            &renderer,
            target,
            identity,
            7,
        )
        .expect("actual transcript geometry");
        assert!(ready.exact && ready.shape_pending.is_none());
        content.push_str(" appended words reflow the current source");
        let current = cache.markdown_entry(identity, paragraph(&content), style);
        assert!(Rc::ptr_eq(&entry, &current));
        element = make(current.clone());
        tree.diff(element.as_widget_mut());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        // No Redraw/update has requested the new revision. The old prefix glyph
        // is genuinely mounted, but it cannot finish the current Find intent.
        let pending = native_logical_location(
            &mut element,
            &mut tree,
            &node,
            &renderer,
            target,
            identity,
            7,
        )
        .expect("actual transcript geometry");
        assert_eq!(
            pending.stage, 3,
            "old prefix glyph remains available for display"
        );
        assert!(
            pending.shape_pending.is_some() && !pending.exact,
            "pre-redraw old prefix geometry cannot certify the current source"
        );
        assert!(
            cache.complete_fixture(),
            "the current revision still needs a real request"
        );
        element = make(current);
        tree.diff(element.as_widget_mut());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let exact = native_logical_location(
            &mut element,
            &mut tree,
            &node,
            &renderer,
            target,
            identity,
            7,
        )
        .expect("actual transcript geometry");
        assert!(exact.exact && exact.shape_pending.is_none());
    }

    #[test]
    fn queued_find_operations_check_live_intent_and_document_before_native_scroll() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 1,
        };
        let state = find_navigation::State::default();
        let guard = state.guard();
        let epoch = state.document_epoch();
        let location = LogicalLocation {
            offset: 600.0,
            exact: true,
            stage: 3,
            shape_pending: None,
            horizontal: Vec::new(),
        };
        let mut element = transcript(
            widget::Space::new().height(5000).width(Length::Fill),
            target,
            None,
            vec![],
            true,
            vec![],
        );
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut apply = ApplyFind {
            target,
            scope: TranscriptScope::default(),
            scroll: card_control_id(target.card, "chat-transcript"),
            location: location.clone(),
            intent: guard.clone(),
            document: Some(epoch),
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut apply);
        let mut before = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut before);
        assert!(
            before.0.is_some_and(|top| top > 0.0),
            "positive control actually changed native scroll"
        );
        state.changed_document();
        let mut stale = ApplyFind {
            target,
            scope: TranscriptScope::default(),
            scroll: card_control_id(target.card, "chat-transcript"),
            location: LogicalLocation {
                offset: 0.0,
                ..location.clone()
            },
            intent: guard.clone(),
            document: Some(epoch),
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut stale);
        let mut after = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
        assert_eq!(
            before.0, after.0,
            "document changed after task creation rejects actual mutation"
        );
        guard.cancel();
        let mut canceled = ApplyFind {
            target,
            scope: TranscriptScope::default(),
            scroll: card_control_id(target.card, "chat-transcript"),
            location: LogicalLocation {
                offset: 0.0,
                ..location
            },
            intent: guard,
            document: None,
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut canceled);
        let mut after = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
        assert_eq!(
            before.0, after.0,
            "even a coarse queued operation cannot override newer input"
        );
    }

    #[test]
    fn find_corrections_preserve_their_intent_but_native_nested_wheels_cancel_it() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 1,
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
        let viewport = Rectangle::with_size(Size::new(400.0, 400.0));
        for nested in [false, true] {
            let state = find_navigation::State::default();
            let guard = state.guard();
            let content: Element<'_, Message> = if nested {
                widget::scrollable(widget::Space::new().height(10_000).width(400))
                    .height(200)
                    .into()
            } else {
                widget::Space::new().height(10_000).width(400).into()
            };
            let mut element = transcript_observed(
                content,
                target,
                None,
                vec![],
                true,
                vec![],
                Observations::default(),
                Memory::default(),
                false,
                HashSet::new(),
                Some(guard.clone()),
            );
            let mut tree = Tree::new(&element);
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut apply = ApplyFind {
                target,
                scope: TranscriptScope::default(),
                scroll: card_control_id(target.card, "chat-transcript"),
                location: LogicalLocation {
                    offset: 600.0,
                    exact: true,
                    stage: 3,
                    shape_pending: None,
                    horizontal: Vec::new(),
                },
                intent: guard.clone(),
                document: Some(state.document_epoch()),
            };
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut apply);
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &Event::Window(iced::window::Event::RedrawRequested(
                    iced::time::Instant::now(),
                )),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            assert!(guard.current());
            assert!(
                !messages.iter().any(|message| matches!(
                    message,
                    Message::ChatUi(Action::Scrolled(_, _, _, true))
                )),
                "Find's own correction must not claim user input"
            );
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
                }),
                Layout::new(&node),
                mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            assert!(
                !guard.current(),
                "a wheel cancels before either outer or nested reader consumes it"
            );
        }
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
            None,
            None,
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
                        target,
                        scroll: card_control_id(target.card, "chat-transcript"),
                        item: item_id("answer"),
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
                scroll: Id,
                identity: u64,
                byte: usize,
                translation: Vector,
                pending: Option<Vector>,
                viewport: Option<Rectangle>,
                glyph: Option<Rectangle>,
            }
            impl Operation for Visible {
                fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                    let before = self.translation;
                    if let Some(translation) = self.pending.take() {
                        self.translation += translation;
                    }
                    visit(self);
                    self.translation = before;
                }
                fn scrollable(
                    &mut self,
                    id: Option<&Id>,
                    viewport: Rectangle,
                    _: Rectangle,
                    translation: Vector,
                    _: &mut dyn operation::Scrollable,
                ) {
                    if id == Some(&self.scroll) {
                        self.viewport = Some(viewport);
                    }
                    self.pending = Some(translation);
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
                scroll: card_control_id(target.card, "chat-transcript"),
                identity,
                byte,
                translation: Vector::ZERO,
                pending: None,
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
                false,
                HashSet::new(),
                None,
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
            messages.is_empty(),
            "leaving the end updates direct observations without paging"
        );
        assert!(
            !observations.borrow()[&None].at_bottom(),
            "the real wheel moved away from the end"
        );
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
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
            "wheel navigation toward the history edge still drives paging"
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

    const REFLOW_FINE: &str = "virtual-chunk-000000000000002a";

    fn width_reflow_geometry(width: f32, height: f32) -> Geometry {
        Geometry {
            viewport: Size::new(width, 400.0),
            content: Size::new(width, height),
        }
    }

    fn width_reflow_reader(top: f32, follow_end: bool) -> Anchor {
        let mut anchor = Anchor::default();
        let measured = rows(&[
            ("answer", 100.0, 2000.0),
            (REFLOW_FINE, 1100.0, 400.0),
            ("next", 2100.0, 900.0),
        ]);
        let geometry = width_reflow_geometry(720.0, 3000.0);
        anchor.layout_following(None, &measured, geometry, None, follow_end);
        anchor.layout_following(None, &measured, geometry, Some(top), follow_end);
        assert_eq!(anchor.contexts[0].top, Some(top));
        anchor
    }

    #[test]
    fn width_reflow_keeps_fine_displacement_through_alias_estimate_and_pending_flow() {
        let mut anchor = width_reflow_reader(1250.0, false);
        let cold_parent = rows(&[("answer", 100.0, 100.0), ("next", 200.0, 1800.0)]);
        anchor.layout_following(
            None,
            &cold_parent,
            width_reflow_geometry(400.0, 2000.0),
            Some(1250.0),
            false,
        );
        let route = anchor.contexts[0].top.unwrap();
        assert!(
            route >= cold_parent[0].1.y && route < cold_parent[0].1.y + cold_parent[0].1.height,
            "the coarse route must intersect its parent so the fine row can mount"
        );
        anchor.contexts[0].finish_reflow(
            &cold_parent,
            &HashSet::new(),
            &[],
            &HashSet::new(),
            &HashMap::new(),
        );
        assert!(anchor.contexts[0].reflow.is_some());

        let estimated = rows(&[("answer", 100.0, 1700.0), (REFLOW_FINE, 850.0, 100.0)]);
        anchor.layout_following(
            None,
            &estimated,
            width_reflow_geometry(400.0, 1800.0),
            Some(route),
            false,
        );
        assert_eq!(anchor.contexts[0].top, Some(949.0));
        assert!(
            anchor.contexts[0].top.unwrap() < estimated[1].1.y + estimated[1].1.height,
            "the estimated fine row must remain eligible to request its visible Flow"
        );
        anchor.contexts[0].finish_reflow(
            &estimated,
            &HashSet::new(),
            &[],
            &HashSet::new(),
            &HashMap::new(),
        );
        assert!(anchor.contexts[0].reflow.is_some());

        let mounted = rows(&[("answer", 100.0, 2200.0), (REFLOW_FINE, 900.0, 300.0)]);
        let mounted_ids = HashSet::from([REFLOW_FINE.to_owned()]);
        anchor.layout_following(
            None,
            &mounted,
            width_reflow_geometry(400.0, 2300.0),
            Some(949.0),
            false,
        );
        assert_eq!(anchor.contexts[0].top, Some(1050.0));
        anchor.contexts[0].finish_reflow(
            &mounted,
            &mounted_ids,
            &[mounted[1].1],
            &HashSet::from(["answer"]),
            &HashMap::new(),
        );
        assert!(
            anchor.contexts[0].reflow.is_some(),
            "mounting the row does not make its pending Flow geometry exact"
        );

        let exact = rows(&[
            ("answer", 100.0, 2400.0),
            (REFLOW_FINE, 1450.0, 600.0),
            ("next", 2500.0, 900.0),
        ]);
        let geometry = width_reflow_geometry(400.0, 3400.0);
        anchor.layout_following(None, &exact, geometry, Some(1050.0), false);
        assert_eq!(
            anchor.contexts[0].top.unwrap() - exact[1].1.y,
            150.0,
            "neither the coarse alias nor the shortened estimate replaces the original fine displacement"
        );
        anchor.contexts[0].finish_reflow(
            &exact,
            &mounted_ids,
            &[],
            &HashSet::from(["answer"]),
            &HashMap::new(),
        );
        assert!(anchor.contexts[0].reflow.is_none());
        anchor.layout_following(None, &exact, geometry, Some(1600.0), false);
        assert_eq!(anchor.contexts[0].top, Some(1600.0));
    }

    #[test]
    fn width_reflow_releases_disappeared_source_and_uses_replacement_boundary() {
        let mut anchor = width_reflow_reader(1250.0, false);
        let alias = rows(&[("answer", 0.0, 2000.0)]);
        anchor.layout_following(
            None,
            &alias,
            width_reflow_geometry(400.0, 2000.0),
            Some(1250.0),
            false,
        );
        anchor.contexts[0].finish_reflow(
            &alias,
            &HashSet::new(),
            &[],
            &HashSet::new(),
            &HashMap::new(),
        );
        assert!(anchor.contexts[0].reflow.is_some());

        let replacement = rows(&[("replacement", 0.0, 900.0)]);
        anchor.layout_following(
            None,
            &replacement,
            width_reflow_geometry(400.0, 900.0),
            anchor.contexts[0].top,
            false,
        );
        assert_eq!(anchor.contexts[0].top, Some(500.0));
        anchor.contexts[0].finish_reflow(
            &replacement,
            &HashSet::new(),
            &[],
            &HashSet::new(),
            &HashMap::new(),
        );
        assert!(anchor.contexts[0].reflow.is_none());
        let grown = rows(&[("replacement", 0.0, 1900.0)]);
        anchor.layout_following(
            None,
            &grown,
            width_reflow_geometry(400.0, 1900.0),
            Some(500.0),
            false,
        );
        assert_eq!(anchor.contexts[0].top, Some(500.0));
    }

    #[test]
    fn width_reflow_keeps_true_end_follow_through_fitting_estimates() {
        let mut anchor = Anchor::default();
        let original = rows(&[("answer", 0.0, 3000.0), (REFLOW_FINE, 2400.0, 600.0)]);
        anchor.layout(None, &original, width_reflow_geometry(720.0, 3000.0), None);
        assert_eq!(anchor.contexts[0].top, Some(2600.0));
        let fitting = rows(&[("answer", 0.0, 200.0)]);
        anchor.layout(
            None,
            &fitting,
            width_reflow_geometry(400.0, 200.0),
            Some(2600.0),
        );
        assert_eq!(anchor.contexts[0].top, Some(0.0));
        anchor.contexts[0].finish_reflow(
            &fitting,
            &HashSet::new(),
            &[],
            &HashSet::new(),
            &HashMap::new(),
        );
        assert!(anchor.contexts[0].follow_end);

        let estimated = rows(&[("answer", 0.0, 600.0), (REFLOW_FINE, 200.0, 400.0)]);
        anchor.layout(
            None,
            &estimated,
            width_reflow_geometry(400.0, 600.0),
            Some(0.0),
        );
        assert_eq!(anchor.contexts[0].top, Some(200.0));
        anchor.contexts[0].finish_reflow(
            &estimated,
            &HashSet::new(),
            &[],
            &HashSet::new(),
            &HashMap::new(),
        );
        assert!(anchor.contexts[0].follow_end);
        let exact = rows(&[("answer", 0.0, 4000.0), (REFLOW_FINE, 3400.0, 600.0)]);
        let mounted = HashSet::from([REFLOW_FINE.to_owned()]);
        anchor.layout(
            None,
            &exact,
            width_reflow_geometry(400.0, 4000.0),
            Some(200.0),
        );
        assert_eq!(anchor.contexts[0].top, Some(3600.0));
        anchor.contexts[0].finish_reflow(
            &exact,
            &mounted,
            &[exact[1].1],
            &HashSet::from(["answer"]),
            &HashMap::new(),
        );
        assert_eq!(anchor.contexts[0].top, Some(3600.0));
        anchor.contexts[0].finish_reflow(
            &exact,
            &mounted,
            &[],
            &HashSet::from(["answer"]),
            &HashMap::new(),
        );
        assert!(anchor.contexts[0].follow_end);
        let grown = rows(&[("answer", 0.0, 4400.0), (REFLOW_FINE, 3400.0, 1000.0)]);
        anchor.layout(
            None,
            &grown,
            width_reflow_geometry(400.0, 4400.0),
            Some(3600.0),
        );
        assert_eq!(anchor.contexts[0].top, Some(4000.0));
    }

    #[test]
    fn width_reflow_clamping_historical_reader_at_provisional_end_does_not_follow() {
        let mut anchor = width_reflow_reader(1250.0, true);
        let provisional = rows(&[("answer", 0.0, 600.0)]);
        let geometry = width_reflow_geometry(400.0, 600.0);
        anchor.layout(None, &provisional, geometry, Some(1250.0));
        assert_eq!(anchor.contexts[0].top, Some(geometry.travel()));
        anchor.contexts[0].finish_reflow(
            &provisional,
            &HashSet::new(),
            &[],
            &HashSet::new(),
            &HashMap::new(),
        );
        assert!(anchor.contexts[0].reflow.is_some());
        let exact = rows(&[
            ("answer", 100.0, 2400.0),
            (REFLOW_FINE, 1450.0, 600.0),
            ("next", 2500.0, 900.0),
        ]);
        let geometry = width_reflow_geometry(400.0, 3400.0);
        anchor.layout(None, &exact, geometry, Some(200.0));
        assert_eq!(anchor.contexts[0].top, Some(1600.0));
        assert_ne!(anchor.contexts[0].top, Some(geometry.travel()));
        anchor.contexts[0].finish_reflow(
            &exact,
            &HashSet::from([REFLOW_FINE.to_owned()]),
            &[],
            &HashSet::from(["answer"]),
            &HashMap::new(),
        );
        assert!(anchor.contexts[0].reflow.is_none());
    }

    #[test]
    fn width_reflow_explicit_edges_supersede_the_retained_fine_reader() {
        for edge in [Edge::Start, Edge::End, Edge::Latest] {
            let memory = Memory::default();
            let mut anchor = width_reflow_reader(1250.0, false);
            let provisional = rows(&[("answer", 100.0, 100.0)]);
            anchor.layout_following(
                None,
                &provisional,
                width_reflow_geometry(400.0, 2000.0),
                Some(1250.0),
                false,
            );
            assert!(anchor.contexts[0].reflow.is_some());
            *memory.0.borrow_mut() = anchor;
            memory.request_edge(None, edge);
            let mut anchor = memory.0.borrow_mut();
            assert!(anchor.contexts[0].reflow.is_none());
            let exact = rows(&[("answer", 0.0, 3400.0), (REFLOW_FINE, 1450.0, 600.0)]);
            let geometry = width_reflow_geometry(400.0, 3400.0);
            let previous_top = anchor.contexts[0].top;
            anchor.layout(None, &exact, geometry, previous_top);
            assert_eq!(
                anchor.contexts[0].top,
                Some(if edge == Edge::Start {
                    0.0
                } else {
                    geometry.travel()
                })
            );
            assert!(anchor.contexts[0].reflow.is_none());
        }
    }

    #[test]
    fn width_reflow_same_parent_rewrite_waits_for_mounted_ready_source() {
        const REWRITTEN_FINE: &str = "virtual-chunk-0000000000000099";
        for pending_flow in [false, true] {
            let mut anchor = width_reflow_reader(1250.0, false);
            let rewritten = rows(&[
                ("answer", 100.0, 2400.0),
                (REWRITTEN_FINE, 1450.0, 600.0),
                ("next", 2500.0, 900.0),
            ]);
            anchor.layout_following(
                None,
                &rewritten,
                width_reflow_geometry(400.0, 3400.0),
                Some(1250.0),
                false,
            );
            let mounted = HashSet::from([REWRITTEN_FINE.to_owned()]);
            anchor.contexts[0].finish_reflow(
                &rewritten,
                &mounted,
                &[],
                &HashSet::new(),
                &HashMap::new(),
            );
            assert!(
                anchor.contexts[0].reflow.is_some(),
                "a coarse alias alone cannot prove the old fine source was rewritten"
            );
            let pending = if pending_flow {
                vec![rewritten[1].1]
            } else {
                vec![]
            };
            anchor.contexts[0].finish_reflow(
                &rewritten,
                &mounted,
                &pending,
                &HashSet::from(["answer"]),
                &HashMap::new(),
            );
            assert_eq!(anchor.contexts[0].reflow.is_some(), pending_flow);
            if pending_flow {
                anchor.contexts[0].finish_reflow(
                    &rewritten,
                    &mounted,
                    &[],
                    &HashSet::from(["answer"]),
                    &HashMap::new(),
                );
            }
            assert!(anchor.contexts[0].reflow.is_none());

            // The replacement owns subsequent geometry. No retired fine key
            // or old parent displacement may reassert itself on another pass.
            let shifted = rows(&[
                ("answer", 400.0, 2400.0),
                (REWRITTEN_FINE, 2050.0, 600.0),
                ("next", 2800.0, 900.0),
            ]);
            let previous_top = anchor.contexts[0].top;
            anchor.layout_following(
                None,
                &shifted,
                width_reflow_geometry(400.0, 3700.0),
                previous_top,
                false,
            );
            assert_eq!(anchor.contexts[0].top, Some(1850.0));
        }
    }

    #[test]
    fn width_reflow_changed_native_scroll_operation_supersedes_retained_reader() {
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let memory = Memory::default();
        let make = |fine_y| {
            transcript_observed(
                item(
                    "answer",
                    widget::column::with_children(vec![
                        widget::Space::new().height(fine_y).into(),
                        item(REFLOW_FINE, widget::Space::new().height(400.0)),
                        widget::Space::new().height(2600.0 - fine_y).into(),
                    ])
                    .width(Length::Fill),
                ),
                target,
                None,
                vec![],
                false,
                vec![
                    (item_id("answer"), "answer"),
                    (item_id(REFLOW_FINE), REFLOW_FINE),
                ],
                Observations::default(),
                memory.clone(),
                false,
                HashSet::new(),
                None,
            )
        };
        let mut element = make(1100.0);
        let mut tree = Tree::new(&element);
        let wide = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
        let node = element.as_widget_mut().layout(&mut tree, &renderer, &wide);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(1350.0),
        );
        assert_eq!(memory.0.borrow().contexts[0].top, Some(1250.0));
        let narrow = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &narrow);
        assert!(memory.0.borrow().contexts[0].reflow.is_some());
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(2200.0),
        );
        assert!(memory.0.borrow().contexts[0].reflow.is_none());
        assert_eq!(memory.0.borrow().contexts[0].top, Some(400.0));
        element = make(1800.0);
        tree.diff(element.as_widget_mut());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &narrow);
        let mut actual = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        assert_eq!(actual.0, Some(400.0));
        assert_eq!(memory.0.borrow().contexts[0].top, Some(400.0));
    }

    #[test]
    fn width_reflow_prepend_at_old_end_keeps_original_exchange_until_fine_ready() {
        let mut anchor = Anchor::default();
        let original = rows(&[("answer", 0.0, 3000.0), (REFLOW_FINE, 2400.0, 600.0)]);
        anchor.layout(None, &original, width_reflow_geometry(720.0, 3000.0), None);
        assert_eq!(anchor.contexts[0].top, Some(2600.0));
        let estimated = rows(&[
            ("older", 0.0, 500.0),
            ("answer", 500.0, 1800.0),
            (REFLOW_FINE, 1800.0, 100.0),
            ("newer", 2300.0, 1200.0),
        ]);
        let geometry = width_reflow_geometry(400.0, 3500.0);
        anchor.layout(None, &estimated, geometry, Some(2600.0));
        assert_eq!(anchor.contexts[0].top, Some(1899.0));
        assert_ne!(anchor.contexts[0].top, Some(geometry.travel()));
        anchor.contexts[0].finish_reflow(
            &estimated,
            &HashSet::new(),
            &[],
            &HashSet::from(["answer"]),
            &HashMap::new(),
        );
        assert!(
            anchor.contexts[0].reflow.is_some(),
            "the old end became a history exchange when an earlier page arrived"
        );
        let exact = rows(&[
            ("older", 0.0, 500.0),
            ("answer", 500.0, 3500.0),
            (REFLOW_FINE, 2900.0, 1000.0),
            ("newer", 4000.0, 1000.0),
        ]);
        let geometry = width_reflow_geometry(400.0, 5000.0);
        anchor.layout(None, &exact, geometry, Some(1899.0));
        assert_eq!(anchor.contexts[0].top.unwrap() - exact[2].1.y, 200.0);
        assert_ne!(anchor.contexts[0].top, Some(geometry.travel()));
        anchor.contexts[0].finish_reflow(
            &exact,
            &HashSet::from([REFLOW_FINE.to_owned()]),
            &[],
            &HashSet::from(["answer"]),
            &HashMap::new(),
        );
        assert!(anchor.contexts[0].reflow.is_none());
    }

    #[test]
    fn width_reflow_late_preserved_refresh_arrival_survives_consumed_estimates() {
        for replaced_source in [false, true] {
            let memory = Memory::default();
            let original = rows(&[("answer", 0.0, 3000.0), (REFLOW_FINE, 2400.0, 600.0)]);
            memory.0.borrow_mut().layout(
                None,
                &original,
                width_reflow_geometry(720.0, 3000.0),
                None,
            );
            memory.preserve_refresh(42, 10);
            let estimated = rows(&[("answer", 0.0, 600.0), (REFLOW_FINE, 200.0, 100.0)]);
            {
                let mut anchor = memory.0.borrow_mut();
                anchor.layout(
                    None,
                    &estimated,
                    width_reflow_geometry(400.0, 600.0),
                    Some(2600.0),
                );
                anchor.contexts[0].finish_reflow(
                    &estimated,
                    &HashSet::new(),
                    &[estimated[1].1],
                    &HashSet::from(["answer"]),
                    &HashMap::new(),
                );
                assert!(anchor.contexts[0].reflow.is_some());
            }
            // The accepted history fetch can answer after width restoration
            // already started; its one-shot arrival is consumed next layout.
            memory.latest_source(Some(10), 11, true, false);
            assert!(memory.preserve_arrived(None));
            {
                let mut anchor = memory.0.borrow_mut();
                anchor.layout(
                    None,
                    &estimated,
                    width_reflow_geometry(400.0, 600.0),
                    Some(200.0),
                );
                anchor.contexts[0].finish_reflow(
                    &estimated,
                    &HashSet::new(),
                    &[],
                    &HashSet::from(["answer"]),
                    &HashMap::new(),
                );
                assert!(
                    anchor.contexts[0].reflow.is_some(),
                    "consuming arrival cannot promote an estimated historical row to live end-follow"
                );
            }
            assert!(!memory.preserve_arrived(None));
            let exact = if replaced_source {
                rows(&[("replacement", 0.0, 6000.0)])
            } else {
                rows(&[("answer", 0.0, 6000.0), (REFLOW_FINE, 3400.0, 1000.0)])
            };
            let geometry = width_reflow_geometry(400.0, 6000.0);
            let mut anchor = memory.0.borrow_mut();
            anchor.layout(None, &exact, geometry, Some(200.0));
            assert_eq!(
                anchor.contexts[0].top,
                Some(if replaced_source { 2600.0 } else { 3600.0 }),
                "preserved refresh keeps original geometry even if overlap disappears"
            );
            assert_ne!(anchor.contexts[0].top, Some(geometry.travel()));
            anchor.contexts[0].finish_reflow(
                &exact,
                &HashSet::from([REFLOW_FINE.to_owned()]),
                &[],
                &HashSet::from(["answer"]),
                &HashMap::new(),
            );
            assert!(anchor.contexts[0].reflow.is_none());
        }
    }

    #[test]
    fn width_reflow_partial_eviction_releases_removed_parent_despite_visible_survivor() {
        let mut anchor = Anchor::default();
        let original = rows(&[
            ("answer", 100.0, 1400.0),
            (REFLOW_FINE, 1100.0, 400.0),
            ("survivor", 1500.0, 1500.0),
        ]);
        let geometry = width_reflow_geometry(720.0, 3000.0);
        anchor.layout_following(None, &original, geometry, None, false);
        anchor.layout_following(None, &original, geometry, Some(1250.0), false);
        let evicted = rows(&[("survivor", 0.0, 2000.0)]);
        anchor.layout_following(
            None,
            &evicted,
            width_reflow_geometry(400.0, 2000.0),
            Some(1250.0),
            false,
        );
        assert_eq!(anchor.contexts[0].top, Some(0.0));
        anchor.contexts[0].finish_reflow(
            &evicted,
            &HashSet::new(),
            &[],
            &HashSet::from(["survivor"]),
            &HashMap::new(),
        );
        assert!(
            anchor.contexts[0].reflow.is_none(),
            "a neighboring visible survivor cannot retain a different source's removed fine bookmark"
        );
        let shifted = rows(&[("survivor", 200.0, 2000.0)]);
        anchor.layout_following(
            None,
            &shifted,
            width_reflow_geometry(400.0, 2200.0),
            Some(0.0),
            false,
        );
        assert_eq!(anchor.contexts[0].top, Some(200.0));
    }

    #[test]
    fn width_reflow_overscan_flow_does_not_hold_ready_replacement_or_bypass_unmounted_source() {
        const REWRITTEN_FINE: &str = "virtual-chunk-0000000000000099";
        for rewritten in [false, true] {
            let mut anchor = width_reflow_reader(1250.0, false);
            let fine = if rewritten {
                REWRITTEN_FINE
            } else {
                REFLOW_FINE
            };
            let current = rows(&[
                ("answer", 100.0, 2400.0),
                (fine, 1450.0, 600.0),
                ("next", 2500.0, 900.0),
            ]);
            let geometry = width_reflow_geometry(400.0, 3400.0);
            anchor.layout_following(None, &current, geometry, Some(1250.0), false);
            let overscan = Rectangle {
                y: 2001.0,
                height: 40.0,
                ..current[1].1
            };
            assert!(overscan.intersection(&current[1].1).is_some());
            assert!(overscan.y > anchor.contexts[0].top.unwrap() + geometry.viewport.height);
            let top = anchor.contexts[0].top.unwrap();
            let parent_id = item_id("answer");
            let ids = HashMap::from([(parent_id.clone(), vec!["answer"])]);
            let nested_translation = Vector::new(120.0, 80.0);
            let mut mounted_items = MountedItems {
                ids: &ids,
                items: HashSet::new(),
                target: None,
                root: None,
                root_translation: Some(Vector::new(0.0, top)),
                pending: vec![],
                translation: Vector::new(120.0, top + 80.0),
                clip: Some(Rectangle::with_size(Size::new(400.0, 400.0))),
                pending_clip: None,
            };
            mounted_items.container(Some(&parent_id), current[0].1);
            mounted_items.pending_geometry(Rectangle {
                x: overscan.x + nested_translation.x,
                y: overscan.y + nested_translation.y,
                ..overscan
            });
            assert!(
                mounted_items.pending.is_empty(),
                "nested translation cannot make a clipped overscan Flow eligible"
            );
            let visible = Rectangle {
                x: 50.0,
                y: top + 100.0,
                width: 200.0,
                height: 40.0,
            };
            mounted_items.pending_geometry(Rectangle {
                x: visible.x + nested_translation.x,
                y: visible.y + nested_translation.y,
                ..visible
            });
            assert_eq!(mounted_items.pending, [visible]);
            anchor.contexts[0].finish_reflow(
                &current,
                &HashSet::from([fine.to_owned()]),
                &mounted_items.pending,
                &mounted_items.items,
                &HashMap::new(),
            );
            assert!(
                anchor.contexts[0].reflow.is_some(),
                "eligible visible Flow must finish even after its parent and fine row mount"
            );
            mounted_items.pending.clear();
            anchor.contexts[0].finish_reflow(
                &current,
                &HashSet::new(),
                &mounted_items.pending,
                &mounted_items.items,
                &HashMap::new(),
            );
            assert_eq!(
                anchor.contexts[0].reflow.is_none(),
                rewritten,
                "overscan Flow never starts, but the original fine source still requires actual mounting"
            );
            if !rewritten {
                assert_eq!(anchor.contexts[0].top, Some(1600.0));
                anchor.contexts[0].finish_reflow(
                    &current,
                    &HashSet::from([REFLOW_FINE.to_owned()]),
                    &mounted_items.pending,
                    &mounted_items.items,
                    &HashMap::new(),
                );
                assert!(anchor.contexts[0].reflow.is_none());
            }
        }
    }

    #[test]
    fn width_reflow_nested_source_route_waits_for_non_anchor_ancestors_in_order() {
        const REWRITTEN_FINE: &str = "virtual-chunk-0000000000000099";
        let measured_bounds = Rectangle::with_size(Size::new(400.0, 400.0));
        let mut measured = ItemBounds {
            ids: HashMap::new(),
            rows: HashMap::new(),
            wanted: HashSet::new(),
            virtual_rows: vec![],
            mounted: HashSet::new(),
            wanted_guards: HashSet::from([1, 2]),
            guards: HashMap::new(),
            path: vec![],
            paths: HashMap::new(),
        };
        let mut probe = virtualized::RowScope::Probe(false);
        measured.custom(None, measured_bounds, &mut probe);
        assert!(matches!(probe, virtualized::RowScope::Probe(true)));
        measured.custom(None, measured_bounds, &mut virtualized::RowScope::Enter(1));
        // A horizontal cell still guards the native source path even though
        // it contributes no selectable fine anchor of its own.
        measured.custom(None, measured_bounds, &mut virtualized::RowScope::Enter(2));
        assert!(measured.virtual_rows.is_empty());
        assert_eq!(measured.path, [1, 2]);
        let source_path = measured.path.clone();
        measured.custom(None, measured_bounds, &mut virtualized::RowScope::Exit);
        measured.custom(None, measured_bounds, &mut virtualized::RowScope::Exit);
        assert!(measured.path.is_empty());

        for (guards, ready) in [
            (HashMap::from([(1, true), (2, false)]), false),
            (HashMap::from([(1, true), (2, true)]), true),
            (HashMap::from([(1, true)]), true),
            (HashMap::from([(1, false)]), false),
        ] {
            let mut anchor = width_reflow_reader(1250.0, false);
            anchor.contexts[0]
                .paths
                .insert(REFLOW_FINE.into(), source_path.clone());
            let rewritten = rows(&[
                ("answer", 100.0, 2400.0),
                (REWRITTEN_FINE, 1450.0, 600.0),
                ("next", 2500.0, 900.0),
            ]);
            let geometry = width_reflow_geometry(400.0, 3400.0);
            anchor.layout_following(None, &rewritten, geometry, Some(1250.0), false);
            assert_eq!(
                anchor.contexts[0].reflow.as_ref().unwrap().path,
                source_path
            );
            anchor.contexts[0].finish_reflow(
                &rewritten,
                &HashSet::from([REWRITTEN_FINE.to_owned()]),
                &[],
                &HashSet::from(["answer"]),
                &guards,
            );
            assert_eq!(anchor.contexts[0].reflow.is_none(), ready);
            if !ready {
                let original_fine = rows(&[
                    ("answer", 100.0, 2400.0),
                    (REFLOW_FINE, 1450.0, 600.0),
                    ("next", 2500.0, 900.0),
                ]);
                anchor.layout_following(None, &original_fine, geometry, Some(1250.0), false);
                assert_eq!(anchor.contexts[0].top, Some(1600.0));
                anchor.contexts[0].finish_reflow(
                    &original_fine,
                    &HashSet::from([REFLOW_FINE.to_owned()]),
                    &[],
                    &HashSet::from(["answer"]),
                    &HashMap::from([(1, true), (2, true)]),
                );
                assert!(anchor.contexts[0].reflow.is_none());
            }
        }
    }
}
