use std::{
    collections::{HashMap, HashSet},
    fmt,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::model::CardId;

const POSITION_EPSILON: f32 = 0.25;
const VELOCITY_EPSILON: f32 = 0.5;
const SCROLL_SYNC_EPSILON: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CardWidth {
    Fraction(f32),
    Fixed(f32),
}

impl CardWidth {
    pub fn fraction(fraction: f32) -> Result<Self, WorkspaceError> {
        validate_positive_finite(fraction, "width fraction")?;
        Ok(Self::Fraction(fraction))
    }

    pub fn fixed(logical_pixels: f32) -> Result<Self, WorkspaceError> {
        validate_positive_finite(logical_pixels, "fixed width")?;
        Ok(Self::Fixed(logical_pixels))
    }

    pub fn resolve(self, viewport_width: f32, limits: WidthLimits) -> f32 {
        let requested = match self {
            Self::Fraction(fraction) => viewport_width * fraction,
            Self::Fixed(logical_pixels) => logical_pixels,
        };

        requested.clamp(limits.minimum, limits.maximum)
    }

    fn resolve_for_strip(
        self,
        viewport_width: f32,
        limits: WidthLimits,
        maximum_fraction_width: f32,
    ) -> f32 {
        let resolved = self.resolve(viewport_width, limits);
        match self {
            // A fractional column should not consume the final gap and reveal
            // edge. Fixed columns are an explicit request and may overflow.
            Self::Fraction(_) if maximum_fraction_width > 0.0 => {
                resolved.min(maximum_fraction_width)
            }
            Self::Fraction(_) | Self::Fixed(_) => resolved,
        }
    }

    fn validate(self) -> Result<Self, WorkspaceError> {
        match self {
            Self::Fraction(value) => Self::fraction(value),
            Self::Fixed(value) => Self::fixed(value),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WidthLimits {
    pub minimum: f32,
    pub maximum: f32,
}

impl WidthLimits {
    pub fn new(minimum: f32, maximum: f32) -> Result<Self, WorkspaceError> {
        validate_positive_finite(minimum, "minimum card width")?;
        validate_positive_finite(maximum, "maximum card width")?;
        if minimum > maximum {
            return Err(WorkspaceError::InvalidWidthLimits { minimum, maximum });
        }
        Ok(Self { minimum, maximum })
    }
}

impl Default for WidthLimits {
    fn default() -> Self {
        Self {
            minimum: 480.0,
            maximum: 1_920.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceCard {
    pub id: CardId,
    pub accessible_name: String,
    pub width: CardWidth,
}

impl WorkspaceCard {
    pub fn new(
        id: CardId,
        accessible_name: impl Into<String>,
        width: CardWidth,
    ) -> Result<Self, WorkspaceError> {
        let accessible_name = accessible_name.into();
        if accessible_name.trim().is_empty() {
            return Err(WorkspaceError::EmptyAccessibleName);
        }

        Ok(Self {
            id,
            accessible_name,
            width: width.validate()?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub gap: f32,
    pub leading_padding: f32,
    pub trailing_padding: f32,
    pub overscan: f32,
    pub reveal_margin: f32,
    pub line_scroll_pixels: f32,
    pub animation_omega: f32,
    pub width_limits: WidthLimits,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            gap: 8.0,
            leading_padding: 12.0,
            trailing_padding: 12.0,
            overscan: 360.0,
            reveal_margin: 12.0,
            line_scroll_pixels: 48.0,
            animation_omega: 18.0,
            width_limits: WidthLimits::default(),
        }
    }
}

impl WorkspaceConfig {
    pub fn validate(self) -> Result<Self, WorkspaceError> {
        validate_nonnegative_finite(self.gap, "card gap")?;
        validate_nonnegative_finite(self.leading_padding, "leading padding")?;
        validate_nonnegative_finite(self.trailing_padding, "trailing padding")?;
        validate_nonnegative_finite(self.overscan, "overscan")?;
        validate_nonnegative_finite(self.reveal_margin, "reveal margin")?;
        validate_positive_finite(self.line_scroll_pixels, "line scroll distance")?;
        validate_positive_finite(self.animation_omega, "animation frequency")?;
        WidthLimits::new(self.width_limits.minimum, self.width_limits.maximum)?;
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardGeometry {
    pub id: CardId,
    pub index: usize,
    pub start: f32,
    pub end: f32,
}

impl CardGeometry {
    pub fn width(self) -> f32 {
        self.end - self.start
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StripGeometry {
    cards: Vec<CardGeometry>,
    prefix_ends: Vec<f32>,
    total_width: f32,
}

impl StripGeometry {
    pub fn build(cards: &[WorkspaceCard], viewport_width: f32, config: WorkspaceConfig) -> Self {
        let viewport_width = finite_nonnegative(viewport_width);
        let maximum_fraction_width =
            viewport_width - config.leading_padding - config.gap - config.reveal_margin;
        let mut cursor = config.leading_padding;
        let mut geometry = Vec::with_capacity(cards.len());
        let mut prefix_ends = Vec::with_capacity(cards.len());

        for (index, card) in cards.iter().enumerate() {
            let start = cursor;
            let width = card.width.resolve_for_strip(
                viewport_width,
                config.width_limits,
                maximum_fraction_width,
            );
            let end = start + width;
            geometry.push(CardGeometry {
                id: card.id,
                index,
                start,
                end,
            });
            prefix_ends.push(end);
            cursor = end;
            if index + 1 < cards.len() {
                cursor += config.gap;
            }
        }

        let total_width = if cards.is_empty() {
            config.leading_padding + config.trailing_padding
        } else {
            cursor + config.trailing_padding
        };
        let total_width = if !geometry.is_empty() && total_width < viewport_width {
            let centering_offset = (viewport_width - total_width) / 2.0;
            for card in &mut geometry {
                card.start += centering_offset;
                card.end += centering_offset;
            }
            for end in &mut prefix_ends {
                *end += centering_offset;
            }
            viewport_width
        } else {
            total_width
        };

        Self {
            cards: geometry,
            prefix_ends,
            total_width,
        }
    }

    pub fn cards(&self) -> &[CardGeometry] {
        &self.cards
    }

    pub fn total_width(&self) -> f32 {
        self.total_width
    }

    pub fn card(&self, id: CardId) -> Option<CardGeometry> {
        self.cards.iter().copied().find(|card| card.id == id)
    }

    pub fn card_at(&self, index: usize) -> Option<CardGeometry> {
        self.cards.get(index).copied()
    }

    pub fn intersecting_range(&self, start: f32, end: f32) -> std::ops::Range<usize> {
        if self.cards.is_empty() || end <= start {
            return 0..0;
        }

        let first = self
            .prefix_ends
            .partition_point(|card_end| *card_end <= start);
        let count = self.cards[first..].partition_point(|card| card.start < end);
        first..first + count
    }

    pub fn max_offset(&self, viewport_width: f32) -> f32 {
        (self.total_width - finite_nonnegative(viewport_width)).max(0.0)
    }

    pub fn focus_offset(&self, id: CardId, viewport_width: f32, margin: f32) -> Option<f32> {
        let card = self.card(id)?;
        let viewport_width = finite_nonnegative(viewport_width);
        let margin = finite_nonnegative(margin).min(viewport_width / 2.0);
        let fitting_width = (viewport_width - margin * 2.0).max(0.0);
        let target = if card.width() > fitting_width {
            card.start - margin
        } else {
            card.start + card.width() / 2.0 - viewport_width / 2.0
        };

        Some(target.clamp(0.0, self.max_offset(viewport_width)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VisibilityClass {
    Visible,
    Overscan,
    Offscreen,
}

impl VisibilityClass {
    pub fn is_mounted(self) -> bool {
        matches!(self, Self::Visible | Self::Overscan)
    }

    pub fn keeps_visual_resources_hot(self) -> bool {
        self.is_mounted()
    }

    pub fn hibernation_directive(self) -> HibernationDirective {
        match self {
            Self::Visible => HibernationDirective::KeepInteractive,
            Self::Overscan => HibernationDirective::KeepWarm,
            Self::Offscreen => HibernationDirective::MayHibernate,
        }
    }
}

impl From<VisibilityClass> for crate::model::Exposure {
    fn from(visibility: VisibilityClass) -> Self {
        match visibility {
            VisibilityClass::Visible => Self::Visible,
            VisibilityClass::Overscan => Self::Prefetch,
            VisibilityClass::Offscreen => Self::Offscreen,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HibernationDirective {
    KeepInteractive,
    KeepWarm,
    MayHibernate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibilityTransition {
    pub id: CardId,
    pub from: VisibilityClass,
    pub to: VisibilityClass,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MountedCard {
    pub id: CardId,
    pub start: f32,
    pub width: f32,
    pub visibility: VisibilityClass,
}

impl MountedCard {
    pub fn end(self) -> f32 {
        self.start + self.width
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewportProjection {
    pub viewport_width: f32,
    pub offset: f32,
    pub total_width: f32,
    pub mounted: Vec<MountedCard>,
}

impl ViewportProjection {
    pub fn mounted_ids(&self) -> impl Iterator<Item = CardId> + '_ {
        self.mounted.iter().map(|card| card.id)
    }

    pub fn mounted_are_disjoint(&self) -> bool {
        self.mounted.iter().all(|card| {
            card.start.is_finite()
                && card.width.is_finite()
                && card.start >= 0.0
                && card.width > 0.0
                && card.end() <= self.total_width
        }) && self
            .mounted
            .windows(2)
            .all(|pair| pair[0].end() <= pair[1].start)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDirection {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusTarget {
    Card(CardId),
    WorkspaceToolbar,
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusHandoff {
    pub unmounting: CardId,
    pub target: FocusTarget,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WheelDelta {
    Lines { x: f32, y: f32 },
    Pixels { x: f32, y: f32 },
}

impl WheelDelta {
    pub fn logical_x(self, line_pixels: f32) -> f32 {
        match self {
            Self::Lines { x, .. } => x * line_pixels,
            Self::Pixels { x, .. } => x,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelevantInput {
    KeyboardNavigation,
    WheelOrTrackpad,
    PointerPan,
    Scrollbar,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PanState {
    pointer_start: f32,
    offset_start: f32,
}

/// Allocation-free ring of exact programmatic scroll offsets that the native
/// scrollable may report after a later camera frame has already been issued.
/// Exact samples avoid mistaking a real scrollbar drag through the animated
/// range for delayed programmatic feedback.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ScrollSync {
    offsets: [f32; 32],
    len: usize,
    next: usize,
    latest: f32,
}

impl ScrollSync {
    fn new(offset: f32) -> Self {
        let mut sync = Self {
            offsets: [f32::NAN; 32],
            len: 0,
            next: 0,
            latest: offset,
        };
        sync.record(offset);
        sync
    }

    fn record(&mut self, offset: f32) {
        self.offsets[self.next] = offset;
        self.next = (self.next + 1) % self.offsets.len();
        self.len = (self.len + 1).min(self.offsets.len());
        self.latest = offset;
    }

    fn take_recognized(&mut self, offset: f32) -> bool {
        let Some(index) = self.offsets[..self.len]
            .iter()
            .position(|issued| (offset - issued).abs() <= SCROLL_SYNC_EPSILON)
        else {
            return false;
        };
        self.offsets[index] = f32::NAN;
        true
    }

    fn is_latest(self, offset: f32) -> bool {
        (offset - self.latest).abs() <= SCROLL_SYNC_EPSILON
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    offset: f32,
    target: f32,
    velocity: f32,
    omega: f32,
    animating: bool,
}

impl Camera {
    pub fn new(omega: f32) -> Result<Self, WorkspaceError> {
        validate_positive_finite(omega, "animation frequency")?;
        Ok(Self {
            offset: 0.0,
            target: 0.0,
            velocity: 0.0,
            omega,
            animating: false,
        })
    }

    pub fn offset(self) -> f32 {
        self.offset
    }

    pub fn target(self) -> f32 {
        self.target
    }

    pub fn velocity(self) -> f32 {
        self.velocity
    }

    pub fn is_animating(self) -> bool {
        self.animating
    }

    pub fn jump_to(&mut self, offset: f32) {
        self.offset = finite_nonnegative(offset);
        self.target = self.offset;
        self.velocity = 0.0;
        self.animating = false;
    }

    pub fn animate_to(&mut self, target: f32) {
        self.target = finite_nonnegative(target);
        self.animating = (self.target - self.offset).abs() > POSITION_EPSILON
            || self.velocity.abs() > VELOCITY_EPSILON;
    }

    pub fn interrupt(&mut self) {
        self.target = self.offset;
        self.velocity = 0.0;
        self.animating = false;
    }

    pub fn clamp(&mut self, maximum: f32) {
        let maximum = finite_nonnegative(maximum);
        self.offset = self.offset.clamp(0.0, maximum);
        self.target = self.target.clamp(0.0, maximum);
        if self.offset == self.target && self.velocity.abs() <= VELOCITY_EPSILON {
            self.velocity = 0.0;
            self.animating = false;
        }
    }

    pub fn step(&mut self, elapsed: Duration) -> bool {
        if !self.animating {
            return false;
        }

        let before = self.offset;
        let seconds = elapsed.as_secs_f32().max(0.0);
        if !seconds.is_finite() || seconds == 0.0 {
            return false;
        }

        let displacement = self.offset - self.target;
        let coefficient = self.velocity + self.omega * displacement;
        let decay = (-self.omega * seconds).exp();
        self.offset = self.target + (displacement + coefficient * seconds) * decay;
        self.velocity = (self.velocity - self.omega * coefficient * seconds) * decay;

        if (self.offset - self.target).abs() <= POSITION_EPSILON
            && self.velocity.abs() <= VELOCITY_EPSILON
        {
            self.offset = self.target;
            self.velocity = 0.0;
            self.animating = false;
        }

        (self.offset - before).abs() > f32::EPSILON
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewportUpdate {
    pub offset_changed: bool,
    pub transitions: Vec<VisibilityTransition>,
}

impl ViewportUpdate {
    pub fn unchanged() -> Self {
        Self {
            offset_changed: false,
            transitions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingClose {
    id: CardId,
    handoff: FocusHandoff,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    cards: Vec<WorkspaceCard>,
    focused: Option<CardId>,
    viewport_width: f32,
    geometry: StripGeometry,
    camera: Camera,
    pan: Option<PanState>,
    scroll_sync: Option<ScrollSync>,
    visibility: HashMap<CardId, VisibilityClass>,
    pending_widget_focus: Option<CardId>,
    pending_close: Option<PendingClose>,
    config: WorkspaceConfig,
}

impl Workspace {
    pub fn new(config: WorkspaceConfig) -> Result<Self, WorkspaceError> {
        let config = config.validate()?;
        Ok(Self {
            cards: Vec::new(),
            focused: None,
            viewport_width: 0.0,
            geometry: StripGeometry::build(&[], 0.0, config),
            camera: Camera::new(config.animation_omega)?,
            pan: None,
            scroll_sync: None,
            visibility: HashMap::new(),
            pending_widget_focus: None,
            pending_close: None,
            config,
        })
    }

    pub fn cards(&self) -> &[WorkspaceCard] {
        &self.cards
    }

    pub fn order(&self) -> impl ExactSizeIterator<Item = CardId> + '_ {
        self.cards.iter().map(|card| card.id)
    }

    pub fn focused(&self) -> Option<CardId> {
        self.focused
    }

    pub fn focused_index(&self) -> Option<usize> {
        let focused = self.focused?;
        self.index_of(focused)
    }

    pub fn card(&self, id: CardId) -> Option<&WorkspaceCard> {
        self.cards.iter().find(|card| card.id == id)
    }

    pub fn geometry(&self) -> &StripGeometry {
        &self.geometry
    }

    pub fn camera(&self) -> Camera {
        self.camera
    }

    pub fn viewport_width(&self) -> f32 {
        self.viewport_width
    }

    pub fn visibility(&self, id: CardId) -> Option<VisibilityClass> {
        self.card(id).map(|_| {
            self.visibility
                .get(&id)
                .copied()
                .unwrap_or(VisibilityClass::Offscreen)
        })
    }

    pub fn is_panning(&self) -> bool {
        self.pan.is_some()
    }

    pub fn pending_close(&self) -> Option<FocusHandoff> {
        self.pending_close.map(|pending| pending.handoff)
    }

    pub fn add_card(
        &mut self,
        card: WorkspaceCard,
        index: usize,
        focus: bool,
    ) -> Result<ViewportUpdate, WorkspaceError> {
        if self.index_of(card.id).is_some() {
            return Err(WorkspaceError::DuplicateCard(card.id));
        }
        if index > self.cards.len() {
            return Err(WorkspaceError::IndexOutOfBounds {
                index,
                len: self.cards.len(),
            });
        }

        let id = card.id;
        self.cards.insert(
            index,
            WorkspaceCard::new(card.id, card.accessible_name, card.width)?,
        );
        if self.focused.is_none() || focus {
            self.focused = Some(id);
            self.pending_widget_focus = Some(id);
        }
        self.rebuild_geometry();
        if focus {
            self.reveal_focused();
        }
        Ok(self.refresh_visibility(false))
    }

    pub fn set_card_width(
        &mut self,
        id: CardId,
        width: CardWidth,
    ) -> Result<ViewportUpdate, WorkspaceError> {
        let index = self.index_of(id).ok_or(WorkspaceError::UnknownCard(id))?;
        self.cards[index].width = width.validate()?;
        self.rebuild_geometry();
        self.reveal_focused();
        Ok(self.refresh_visibility(false))
    }

    pub fn rename_card(
        &mut self,
        id: CardId,
        accessible_name: impl Into<String>,
    ) -> Result<(), WorkspaceError> {
        let accessible_name = accessible_name.into();
        if accessible_name.trim().is_empty() {
            return Err(WorkspaceError::EmptyAccessibleName);
        }
        let index = self.index_of(id).ok_or(WorkspaceError::UnknownCard(id))?;
        self.cards[index].accessible_name = accessible_name;
        Ok(())
    }

    pub fn reorder(
        &mut self,
        id: CardId,
        destination: usize,
    ) -> Result<ViewportUpdate, WorkspaceError> {
        let source = self.index_of(id).ok_or(WorkspaceError::UnknownCard(id))?;
        if destination >= self.cards.len() {
            return Err(WorkspaceError::IndexOutOfBounds {
                index: destination,
                len: self.cards.len(),
            });
        }
        if source != destination {
            let card = self.cards.remove(source);
            self.cards.insert(destination, card);
            self.rebuild_geometry();
            self.reveal_focused();
        }
        Ok(self.refresh_visibility(false))
    }

    pub fn move_focused(
        &mut self,
        direction: FocusDirection,
    ) -> Result<ViewportUpdate, WorkspaceError> {
        let focused = self.focused.ok_or(WorkspaceError::NoFocusedCard)?;
        let source = self
            .index_of(focused)
            .ok_or(WorkspaceError::UnknownCard(focused))?;
        let destination = match direction {
            FocusDirection::Left => source.saturating_sub(1),
            FocusDirection::Right => (source + 1).min(self.cards.len().saturating_sub(1)),
        };
        self.reorder(focused, destination)
    }

    pub fn focus(&mut self, id: CardId) -> Result<ViewportUpdate, WorkspaceError> {
        if self.index_of(id).is_none() {
            return Err(WorkspaceError::UnknownCard(id));
        }
        if self.pending_close.is_some_and(|pending| pending.id == id) {
            return Err(WorkspaceError::CardClosing(id));
        }

        self.interrupt_for(RelevantInput::KeyboardNavigation);
        self.focused = Some(id);
        self.pending_widget_focus = Some(id);
        self.reveal_focused();
        Ok(self.refresh_visibility(false))
    }

    pub fn focus_adjacent(
        &mut self,
        direction: FocusDirection,
    ) -> Result<ViewportUpdate, WorkspaceError> {
        if self.cards.is_empty() {
            self.focused = None;
            return Ok(ViewportUpdate::unchanged());
        }

        let current = self.focused_index().unwrap_or(0);
        let next = match direction {
            FocusDirection::Left => current.saturating_sub(1),
            FocusDirection::Right => (current + 1).min(self.cards.len() - 1),
        };
        self.focus(self.cards[next].id)
    }

    pub fn take_widget_focus_request(&mut self) -> Option<CardId> {
        let id = self.pending_widget_focus?;
        if self.visibility(id).is_some_and(VisibilityClass::is_mounted) {
            self.pending_widget_focus = None;
            Some(id)
        } else {
            None
        }
    }

    pub fn request_close(&mut self, id: CardId) -> Result<FocusHandoff, WorkspaceError> {
        if self.pending_close.is_some() {
            return Err(WorkspaceError::CloseAlreadyPending);
        }
        let index = self.index_of(id).ok_or(WorkspaceError::UnknownCard(id))?;
        let target = if self.focused == Some(id) {
            self.cards
                .get(index + 1)
                .or_else(|| {
                    index
                        .checked_sub(1)
                        .and_then(|previous| self.cards.get(previous))
                })
                .filter(|card| {
                    self.visibility(card.id)
                        .is_some_and(VisibilityClass::is_mounted)
                })
                .map(|card| FocusTarget::Card(card.id))
                .unwrap_or(FocusTarget::WorkspaceToolbar)
        } else {
            FocusTarget::Unchanged
        };
        let handoff = FocusHandoff {
            unmounting: id,
            target,
        };
        self.pending_close = Some(PendingClose { id, handoff });
        Ok(handoff)
    }

    pub fn commit_close(&mut self, id: CardId) -> Result<ViewportUpdate, WorkspaceError> {
        let pending = self.pending_close.ok_or(WorkspaceError::NoClosePending)?;
        if pending.id != id {
            return Err(WorkspaceError::WrongPendingClose {
                expected: pending.id,
                received: id,
            });
        }

        let index = self.index_of(id).ok_or(WorkspaceError::UnknownCard(id))?;
        self.cards.remove(index);
        self.pending_close = None;
        self.visibility.remove(&id);
        if self.pending_widget_focus == Some(id) {
            self.pending_widget_focus = None;
        }
        if self.focused == Some(id) {
            self.focused = match pending.handoff.target {
                FocusTarget::Card(next) if self.index_of(next).is_some() => Some(next),
                FocusTarget::Unchanged => self.focused,
                _ => self
                    .cards
                    .get(index)
                    .or_else(|| {
                        index
                            .checked_sub(1)
                            .and_then(|previous| self.cards.get(previous))
                    })
                    .map(|card| card.id),
            };
            if !matches!(pending.handoff.target, FocusTarget::Card(_)) {
                self.pending_widget_focus = self.focused;
            }
        }
        if self.cards.is_empty() {
            self.focused = None;
            self.pending_widget_focus = None;
        }
        self.rebuild_geometry();
        self.reveal_focused();
        Ok(self.refresh_visibility(false))
    }

    pub fn cancel_close(&mut self, id: CardId) -> Result<(), WorkspaceError> {
        let pending = self.pending_close.ok_or(WorkspaceError::NoClosePending)?;
        if pending.id != id {
            return Err(WorkspaceError::WrongPendingClose {
                expected: pending.id,
                received: id,
            });
        }
        self.pending_close = None;
        Ok(())
    }

    pub fn resize_viewport(&mut self, width: f32) -> ViewportUpdate {
        self.viewport_width = finite_nonnegative(width);
        self.rebuild_geometry();
        self.reveal_focused();
        self.camera
            .clamp(self.geometry.max_offset(self.viewport_width));
        self.refresh_visibility(false)
    }

    /// Records an exact offset sent to the native scrollable. A bounded ring
    /// recognizes delayed `on_scroll` feedback without allocating per frame.
    pub fn record_programmatic_scroll(&mut self, offset: f32) {
        let offset = finite_nonnegative(offset);
        match &mut self.scroll_sync {
            Some(sync) => sync.record(offset),
            None => self.scroll_sync = Some(ScrollSync::new(offset)),
        }
    }

    pub fn native_scroll(&mut self, offset: f32, viewport_width: f32) -> ViewportUpdate {
        let old_offset = self.camera.offset();
        let viewport_width = finite_nonnegative(viewport_width);
        if (self.viewport_width - viewport_width).abs() > f32::EPSILON {
            self.viewport_width = viewport_width;
            self.rebuild_geometry();
            self.camera
                .clamp(self.geometry.max_offset(self.viewport_width));
        }

        let offset = finite_nonnegative(offset).min(self.geometry.max_offset(self.viewport_width));
        let recognized = self
            .scroll_sync
            .as_mut()
            .is_some_and(|sync| sync.take_recognized(offset));
        if recognized {
            if self
                .scroll_sync
                .is_some_and(|sync| sync.is_latest(offset) && !self.camera.is_animating())
            {
                self.scroll_sync = None;
            }
            return self
                .refresh_visibility((self.camera.offset() - old_offset).abs() > f32::EPSILON);
        }

        self.scroll_sync = None;
        self.interrupt_for(RelevantInput::Scrollbar);
        self.camera.jump_to(offset);
        self.refresh_visibility((self.camera.offset() - old_offset).abs() > f32::EPSILON)
    }

    pub fn scroll_wheel(&mut self, delta: WheelDelta) -> ViewportUpdate {
        let old_offset = self.camera.offset();
        self.interrupt_for(RelevantInput::WheelOrTrackpad);
        let requested = self.camera.offset() - delta.logical_x(self.config.line_scroll_pixels);
        self.camera
            .jump_to(requested.clamp(0.0, self.geometry.max_offset(self.viewport_width)));
        self.refresh_visibility((self.camera.offset() - old_offset).abs() > f32::EPSILON)
    }

    pub fn begin_pan(&mut self, pointer_x: f32) {
        self.interrupt_for(RelevantInput::PointerPan);
        self.pan = Some(PanState {
            pointer_start: pointer_x,
            offset_start: self.camera.offset(),
        });
    }

    pub fn update_pan(&mut self, pointer_x: f32) -> ViewportUpdate {
        let Some(pan) = self.pan else {
            return ViewportUpdate::unchanged();
        };
        let old_offset = self.camera.offset();
        let maximum = self.geometry.max_offset(self.viewport_width);
        let requested = pan.offset_start + pan.pointer_start - pointer_x;
        self.camera.jump_to(requested.clamp(0.0, maximum));
        self.refresh_visibility((self.camera.offset() - old_offset).abs() > f32::EPSILON)
    }

    pub fn end_pan(&mut self) {
        self.pan = None;
    }

    pub fn interrupt_for(&mut self, input: RelevantInput) {
        match input {
            RelevantInput::KeyboardNavigation => {
                self.camera.interrupt();
            }
            RelevantInput::WheelOrTrackpad | RelevantInput::PointerPan => {
                self.scroll_sync = None;
                self.camera.interrupt();
            }
            RelevantInput::Scrollbar => {
                self.camera.interrupt();
            }
        }
    }

    pub fn tick(&mut self, elapsed: Duration) -> ViewportUpdate {
        let changed = self.camera.step(elapsed);
        self.camera
            .clamp(self.geometry.max_offset(self.viewport_width));
        self.refresh_visibility(changed)
    }

    pub fn projection(&self, viewport_width: f32) -> ViewportProjection {
        let viewport_width = finite_nonnegative(viewport_width);
        let rebuilt_geometry;
        let geometry = if (viewport_width - self.viewport_width).abs() <= f32::EPSILON {
            &self.geometry
        } else {
            rebuilt_geometry = StripGeometry::build(&self.cards, viewport_width, self.config);
            &rebuilt_geometry
        };
        let offset = self
            .camera
            .offset()
            .clamp(0.0, geometry.max_offset(viewport_width));
        let bands = visibility_bands(geometry, offset, viewport_width, self.config.overscan);
        let mounted = bands
            .mounted
            .clone()
            .filter_map(|index| {
                let card = geometry.card_at(index)?;
                let visibility = if bands.visible.contains(&index) {
                    VisibilityClass::Visible
                } else {
                    VisibilityClass::Overscan
                };
                Some(MountedCard {
                    id: card.id,
                    start: card.start,
                    width: card.width(),
                    visibility,
                })
            })
            .collect();

        let projection = ViewportProjection {
            viewport_width,
            offset,
            total_width: geometry.total_width(),
            mounted,
        };
        debug_assert!(projection.mounted_are_disjoint());
        projection
    }

    pub fn check_invariants(&self) -> Result<(), WorkspaceError> {
        let mut seen = std::collections::HashSet::with_capacity(self.cards.len());
        for card in &self.cards {
            if !seen.insert(card.id) {
                return Err(WorkspaceError::DuplicateCard(card.id));
            }
            if card.accessible_name.trim().is_empty() {
                return Err(WorkspaceError::EmptyAccessibleName);
            }
            card.width.validate()?;
        }

        if let Some(focused) = self.focused
            && self.index_of(focused).is_none()
        {
            return Err(WorkspaceError::UnknownCard(focused));
        }
        if self.cards.is_empty() && self.focused.is_some() {
            return Err(WorkspaceError::FocusInvariant);
        }
        if !self.cards.is_empty() && self.focused.is_none() {
            return Err(WorkspaceError::FocusInvariant);
        }
        Ok(())
    }

    fn index_of(&self, id: CardId) -> Option<usize> {
        self.cards.iter().position(|card| card.id == id)
    }

    fn rebuild_geometry(&mut self) {
        self.geometry = StripGeometry::build(&self.cards, self.viewport_width, self.config);
        self.camera
            .clamp(self.geometry.max_offset(self.viewport_width));
    }

    fn reveal_focused(&mut self) {
        let Some(focused) = self.focused else {
            return;
        };
        if let Some(target) =
            self.geometry
                .focus_offset(focused, self.viewport_width, self.config.reveal_margin)
        {
            self.camera.animate_to(target);
        }
    }

    fn refresh_visibility(&mut self, offset_changed: bool) -> ViewportUpdate {
        let bands = visibility_bands(
            &self.geometry,
            self.camera.offset(),
            self.viewport_width,
            self.config.overscan,
        );
        let mut next = HashMap::with_capacity(bands.mounted.len());
        for index in bands.mounted {
            let card = self.geometry.cards[index];
            let class = if bands.visible.contains(&index) {
                VisibilityClass::Visible
            } else {
                VisibilityClass::Overscan
            };
            next.insert(card.id, class);
        }

        let mut affected: HashSet<_> = self.visibility.keys().copied().collect();
        affected.extend(next.keys().copied());
        let mut affected: Vec<_> = affected.into_iter().collect();
        affected.sort_by_key(|id| self.index_of(*id).unwrap_or(usize::MAX));
        let mut transitions = Vec::new();
        for id in affected {
            let old = self
                .visibility
                .get(&id)
                .copied()
                .unwrap_or(VisibilityClass::Offscreen);
            let new = next.get(&id).copied().unwrap_or(VisibilityClass::Offscreen);
            if old != new {
                transitions.push(VisibilityTransition {
                    id,
                    from: old,
                    to: new,
                });
            }
        }
        self.visibility = next;
        ViewportUpdate {
            offset_changed,
            transitions,
        }
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new(WorkspaceConfig::default()).expect("default workspace configuration is valid")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkspaceError {
    DuplicateCard(CardId),
    UnknownCard(CardId),
    CardClosing(CardId),
    EmptyAccessibleName,
    InvalidNumber { field: &'static str, value: f32 },
    InvalidWidthLimits { minimum: f32, maximum: f32 },
    IndexOutOfBounds { index: usize, len: usize },
    NoFocusedCard,
    CloseAlreadyPending,
    NoClosePending,
    WrongPendingClose { expected: CardId, received: CardId },
    FocusInvariant,
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateCard(id) => write!(formatter, "card {id:?} is already in the workspace"),
            Self::UnknownCard(id) => write!(formatter, "card {id:?} is not in the workspace"),
            Self::CardClosing(id) => write!(formatter, "card {id:?} is pending removal"),
            Self::EmptyAccessibleName => write!(formatter, "card accessible name cannot be empty"),
            Self::InvalidNumber { field, value } => {
                write!(formatter, "{field} must be finite and valid, got {value}")
            }
            Self::InvalidWidthLimits { minimum, maximum } => write!(
                formatter,
                "minimum card width {minimum} exceeds maximum card width {maximum}"
            ),
            Self::IndexOutOfBounds { index, len } => {
                write!(formatter, "index {index} is outside workspace length {len}")
            }
            Self::NoFocusedCard => write!(formatter, "workspace has no focused card"),
            Self::CloseAlreadyPending => write!(formatter, "another card removal is pending"),
            Self::NoClosePending => write!(formatter, "no card removal is pending"),
            Self::WrongPendingClose { expected, received } => write!(
                formatter,
                "card {received:?} cannot be removed while {expected:?} is pending"
            ),
            Self::FocusInvariant => write!(formatter, "workspace focus invariant is broken"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VisibilityBands {
    visible: std::ops::Range<usize>,
    mounted: std::ops::Range<usize>,
}

fn visibility_bands(
    geometry: &StripGeometry,
    offset: f32,
    viewport_width: f32,
    overscan: f32,
) -> VisibilityBands {
    let viewport_start = finite_nonnegative(offset);
    let viewport_end = viewport_start + finite_nonnegative(viewport_width);
    let overscan = finite_nonnegative(overscan);
    let overscan_start = (viewport_start - overscan).max(0.0);
    let overscan_end = viewport_end + overscan;
    VisibilityBands {
        visible: geometry.intersecting_range(viewport_start, viewport_end),
        mounted: geometry.intersecting_range(overscan_start, overscan_end),
    }
}

fn finite_nonnegative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn validate_positive_finite(value: f32, field: &'static str) -> Result<(), WorkspaceError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(WorkspaceError::InvalidNumber { field, value })
    }
}

fn validate_nonnegative_finite(value: f32, field: &'static str) -> Result<(), WorkspaceError> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(WorkspaceError::InvalidNumber { field, value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(width: CardWidth) -> WorkspaceCard {
        WorkspaceCard::new(CardId::new(), "Test card", width).unwrap()
    }

    fn workspace_with_widths(widths: &[CardWidth], viewport: f32) -> Workspace {
        let mut workspace = Workspace::default();
        workspace.resize_viewport(viewport);
        for (index, width) in widths.iter().copied().enumerate() {
            workspace.add_card(card(width), index, index == 0).unwrap();
        }
        workspace
    }

    #[test]
    fn prefix_geometry_is_monotonic_and_exact_for_many_width_combinations() {
        let choices = [
            CardWidth::Fixed(240.0),
            CardWidth::Fixed(417.0),
            CardWidth::Fraction(0.5),
            CardWidth::Fraction(1.25),
        ];
        for a in choices {
            for b in choices {
                for c in choices {
                    let workspace = workspace_with_widths(&[a, b, c], 800.0);
                    let cards = workspace.geometry().cards();
                    assert_eq!(cards.len(), 3);
                    assert!(cards.windows(2).all(|pair| pair[0].end < pair[1].start));
                    assert_eq!(cards[1].start, cards[0].end + workspace.config.gap);
                    assert_eq!(cards[2].start, cards[1].end + workspace.config.gap);
                    assert_eq!(
                        workspace.geometry().total_width(),
                        cards[2].end + workspace.config.trailing_padding
                    );
                }
            }
        }
    }

    #[test]
    fn focus_offset_centers_fitting_columns_and_clamps_strip_edges() {
        let workspace = workspace_with_widths(
            &[
                CardWidth::Fixed(480.0),
                CardWidth::Fixed(480.0),
                CardWidth::Fixed(480.0),
            ],
            700.0,
        );
        let geometry = workspace.geometry();
        let ids: Vec<_> = workspace.order().collect();
        assert_eq!(geometry.focus_offset(ids[0], 700.0, 12.0), Some(0.0));
        let middle = geometry.card(ids[1]).unwrap();
        assert_eq!(
            geometry.focus_offset(ids[1], 700.0, 12.0),
            Some(middle.start + middle.width() / 2.0 - 350.0)
        );
        assert_eq!(
            geometry.focus_offset(ids[2], 700.0, 12.0),
            Some(geometry.max_offset(700.0))
        );
    }

    #[test]
    fn fractional_columns_reserve_the_gap_and_next_column_peek_at_every_width() {
        for viewport in [320.0, 800.0, 1_920.0] {
            let workspace = workspace_with_widths(&[CardWidth::Fraction(1.0); 3], viewport);
            let cards = workspace.geometry().cards();
            let expected_width = viewport
                - workspace.config.leading_padding
                - workspace.config.gap
                - workspace.config.reveal_margin;
            assert_eq!(cards[0].width(), expected_width);
            assert_eq!(cards[1].start, viewport - workspace.config.reveal_margin);
            assert_eq!(viewport - cards[1].start, workspace.config.reveal_margin);
        }
    }

    #[test]
    fn underfilled_strip_centers_columns_without_changing_their_widths() {
        let workspace = workspace_with_widths(&[CardWidth::Fraction(0.7)], 1_000.0);
        let geometry = workspace.geometry();
        let card = geometry.card_at(0).unwrap();

        assert_eq!(card.width(), 700.0);
        assert_eq!(card.start, 150.0);
        assert_eq!(card.end, 850.0);
        assert_eq!(geometry.total_width(), 1_000.0);
        assert_eq!(geometry.max_offset(1_000.0), 0.0);
    }

    #[test]
    fn overflowing_strip_preserves_leading_gutter_and_neighbor_peek() {
        let workspace = workspace_with_widths(
            &[CardWidth::Fraction(0.7), CardWidth::Fraction(0.7)],
            1_000.0,
        );
        let geometry = workspace.geometry();
        let first = geometry.card_at(0).unwrap();
        let second = geometry.card_at(1).unwrap();

        assert_eq!(first.start, workspace.config.leading_padding);
        assert_eq!(second.start, first.end + workspace.config.gap);
        assert!(geometry.total_width() > 1_000.0);
        assert!(geometry.max_offset(1_000.0) > 0.0);
    }

    #[test]
    fn every_projection_has_ordered_disjoint_mounted_columns() {
        let mut workspace = workspace_with_widths(
            &[
                CardWidth::Fraction(1.0),
                CardWidth::Fixed(900.0),
                CardWidth::Fraction(0.45),
                CardWidth::Fixed(480.0),
            ],
            700.0,
        );
        for width in [320.0, 700.0, 1_440.0] {
            workspace.resize_viewport(width);
            let maximum = workspace.geometry().max_offset(width);
            for offset in [0.0, maximum / 2.0, maximum] {
                workspace.native_scroll(offset, width);
                assert!(workspace.projection(width).mounted_are_disjoint());
            }
        }
    }

    #[test]
    fn focus_offset_leading_aligns_an_oversized_column() {
        let workspace =
            workspace_with_widths(&[CardWidth::Fixed(900.0), CardWidth::Fixed(900.0)], 700.0);
        let geometry = workspace.geometry();
        let target = geometry.card_at(1).unwrap();
        assert_eq!(
            geometry.focus_offset(target.id, 700.0, 12.0),
            Some(target.start - 12.0)
        );
    }

    #[test]
    fn reorder_preserves_ids_focus_and_every_permutation() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(280.0); 5], 640.0);
        let original: Vec<_> = workspace.order().collect();
        workspace.focus(original[2]).unwrap();
        for destination in 0..original.len() {
            workspace.reorder(original[2], destination).unwrap();
            assert_eq!(workspace.focused(), Some(original[2]));
            let mut after: Vec<_> = workspace.order().collect();
            after.sort_by_key(|id| format!("{id:?}"));
            let mut expected = original.clone();
            expected.sort_by_key(|id| format!("{id:?}"));
            assert_eq!(after, expected);
            workspace.check_invariants().unwrap();
        }
    }

    #[test]
    fn focus_navigation_stops_at_edges_and_close_keeps_focus_valid() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fraction(0.6); 3], 700.0);
        let ids: Vec<_> = workspace.order().collect();
        workspace.focus_adjacent(FocusDirection::Left).unwrap();
        assert_eq!(workspace.focused(), Some(ids[0]));
        workspace.focus_adjacent(FocusDirection::Right).unwrap();
        assert_eq!(workspace.focused(), Some(ids[1]));
        let handoff = workspace.request_close(ids[1]).unwrap();
        assert_eq!(handoff.target, FocusTarget::Card(ids[2]));
        assert!(workspace.card(ids[1]).is_some());
        workspace.commit_close(ids[1]).unwrap();
        assert_eq!(workspace.focused(), Some(ids[2]));
        workspace.check_invariants().unwrap();
    }

    #[test]
    fn closing_an_unfocused_card_does_not_steal_focus() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(300.0); 3], 700.0);
        let ids: Vec<_> = workspace.order().collect();
        let handoff = workspace.request_close(ids[2]).unwrap();
        assert_eq!(handoff.target, FocusTarget::Unchanged);
        assert_eq!(workspace.focused(), Some(ids[0]));
        workspace.commit_close(ids[2]).unwrap();
        assert_eq!(workspace.focused(), Some(ids[0]));
        workspace.check_invariants().unwrap();
    }

    #[test]
    fn virtualization_uses_strict_half_open_boundaries() {
        let config = WorkspaceConfig {
            gap: 0.0,
            leading_padding: 0.0,
            trailing_padding: 0.0,
            overscan: 100.0,
            reveal_margin: 0.0,
            width_limits: WidthLimits::new(1.0, 10_000.0).unwrap(),
            ..WorkspaceConfig::default()
        };
        let mut workspace = Workspace::new(config).unwrap();
        workspace.resize_viewport(100.0);
        for index in 0..4 {
            workspace
                .add_card(card(CardWidth::Fixed(100.0)), index, false)
                .unwrap();
        }
        workspace.native_scroll(100.0, 100.0);
        let projection = workspace.projection(100.0);
        let classes: Vec<_> = workspace
            .order()
            .map(|id| workspace.visibility(id).unwrap())
            .collect();
        assert_eq!(
            classes,
            vec![
                VisibilityClass::Overscan,
                VisibilityClass::Visible,
                VisibilityClass::Overscan,
                VisibilityClass::Offscreen,
            ]
        );
        assert_eq!(projection.mounted.len(), 3);
        assert_eq!(
            VisibilityClass::Offscreen.hibernation_directive(),
            HibernationDirective::MayHibernate
        );
        assert_eq!(
            crate::model::Exposure::from(VisibilityClass::Overscan),
            crate::model::Exposure::Prefetch
        );
    }

    #[test]
    fn critically_damped_animation_converges_without_overshoot() {
        let mut camera = Camera::new(18.0).unwrap();
        camera.animate_to(900.0);
        let mut previous = camera.offset();
        for _ in 0..240 {
            camera.step(Duration::from_secs_f32(1.0 / 120.0));
            assert!(camera.offset() >= previous);
            assert!(camera.offset() <= 900.0);
            previous = camera.offset();
        }
        assert_eq!(camera.offset(), 900.0);
        assert_eq!(camera.velocity(), 0.0);
        assert!(!camera.is_animating());
    }

    #[test]
    fn visibility_transitions_are_explicit_and_do_not_repeat() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(400.0); 4], 400.0);
        let first = workspace.resize_viewport(400.0);
        assert!(first.transitions.is_empty());
        let moved = workspace.native_scroll(1_248.0, 400.0);
        assert!(moved.transitions.iter().any(|transition| {
            transition.from == VisibilityClass::Visible
                && transition.to == VisibilityClass::Offscreen
        }));
        let unchanged = workspace.native_scroll(1_248.0, 400.0);
        assert!(unchanged.transitions.is_empty());
    }

    #[test]
    fn wheel_and_pan_interrupt_animation_and_stay_bounded() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(500.0); 4], 600.0);
        let last = workspace.order().last().unwrap();
        workspace.focus(last).unwrap();
        assert!(workspace.camera().is_animating());
        workspace.scroll_wheel(WheelDelta::Pixels { x: -80.0, y: 0.0 });
        assert!(!workspace.camera().is_animating());
        workspace.begin_pan(200.0);
        workspace.update_pan(-10_000.0);
        assert_eq!(
            workspace.camera().offset(),
            workspace.geometry().max_offset(workspace.viewport_width())
        );
    }

    #[test]
    fn viewport_resize_preserves_restored_focus_and_camera_animation() {
        let mut workspace = Workspace::default();
        let mut restored_focus = None;
        for index in 0..4 {
            let card = card(CardWidth::Fixed(500.0));
            let id = card.id;
            workspace.add_card(card, index, index == 3).unwrap();
            if index == 3 {
                restored_focus = Some(id);
            }
        }
        let restored_focus = restored_focus.unwrap();

        assert!(workspace.camera().is_animating());
        workspace.resize_viewport(600.0);
        assert_eq!(workspace.focused(), Some(restored_focus));
        assert_eq!(workspace.pending_widget_focus, Some(restored_focus));
        assert!(workspace.camera().is_animating());

        workspace.tick(Duration::from_millis(16));
        let animated_offset = workspace.camera().offset();
        workspace.resize_viewport(700.0);
        assert_eq!(workspace.camera().offset(), animated_offset);
        assert!(workspace.camera().is_animating());
        assert_eq!(workspace.pending_widget_focus, Some(restored_focus));
    }

    #[test]
    fn delayed_scroll_feedback_does_not_interrupt_camera_but_manual_scroll_does() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(500.0); 4], 600.0);
        let last = workspace.order().last().unwrap();
        workspace.focus(last).unwrap();
        workspace.tick(Duration::from_millis(16));
        let delayed_offset = workspace.camera().offset();
        workspace.record_programmatic_scroll(delayed_offset);

        workspace.tick(Duration::from_millis(16));
        let advanced_offset = workspace.camera().offset();
        let target = workspace.camera().target();
        workspace.record_programmatic_scroll(advanced_offset);
        workspace.native_scroll(delayed_offset, 600.0);

        assert!(workspace.camera().is_animating());
        assert_eq!(workspace.camera().offset(), advanced_offset);
        assert_eq!(workspace.camera().target(), target);

        let manual_offset = (delayed_offset + advanced_offset) / 2.0;
        workspace.native_scroll(manual_offset, 600.0);
        assert!(!workspace.camera().is_animating());
        assert_eq!(workspace.camera().offset(), manual_offset);
    }
}
