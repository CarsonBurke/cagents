use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::model::CardId;

const POSITION_EPSILON: f32 = 0.25;
const VELOCITY_EPSILON: f32 = 0.5;
/// Where the camera stops approaching its target and takes it.
///
/// A spring only ever nears its target, and every frame it is still moving
/// rebuilds the strip. Within half a logical pixel, and slow enough to cover
/// less than that again in a 60 Hz frame, what is left cannot be seen, so
/// finishing it there costs nothing visible. The speed bound is what keeps a
/// camera that is merely passing its target from being stopped on it.
const SETTLE_DISTANCE: f32 = 0.5;
const SETTLE_SPEED: f32 = 30.0;
const SCROLL_SYNC_EPSILON: f32 = 0.5;
const LAYOUT_DURATION: Duration = Duration::from_millis(220);

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
        match self {
            // Fractions describe the actual strip viewport, including the full
            // width preset. Drag bounds are enforced before storing a fraction.
            Self::Fraction(fraction) if viewport_width > 0.0 => viewport_width * fraction,
            Self::Fraction(_) => limits.minimum,
            Self::Fixed(logical_pixels) => logical_pixels.clamp(limits.minimum, limits.maximum),
        }
    }

    pub fn next_preset(self) -> Self {
        const PRESETS: [f32; 4] = [1.0 / 3.0, 0.5, 2.0 / 3.0, 1.0];
        let next = match self {
            Self::Fraction(fraction) => PRESETS
                .into_iter()
                .find(|preset| *preset > fraction + f32::EPSILON)
                .unwrap_or(PRESETS[0]),
            Self::Fixed(_) => PRESETS[0],
        };
        Self::Fraction(next)
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

pub use crate::names::{MAX_CARD_NAME_CHARS, normalize_card_name};

impl WorkspaceCard {
    pub fn new(
        id: CardId,
        accessible_name: impl Into<String>,
        width: CardWidth,
    ) -> Result<Self, WorkspaceError> {
        let accessible_name = normalize_card_name(&accessible_name.into())
            .ok_or(WorkspaceError::EmptyAccessibleName)?;

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
    pub animation_omega: f32,
    pub width_limits: WidthLimits,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            gap: 0.0,
            leading_padding: 0.0,
            trailing_padding: 0.0,
            overscan: 360.0,
            reveal_margin: 12.0,
            animation_omega: 30.0,
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

    fn width_within_boundaries(self) -> f32 {
        let width = self.width();
        // Layout widgets reconstruct the right edge as start + width. The
        // subtraction above can round upward, so its inverse addition may
        // exceed our canonical edge. Correct only that one-unit overshoot;
        // changing the disjointness tolerance would hide real overlaps.
        if width.is_finite() && width > 0.0 && self.start + width > self.end {
            width.next_down()
        } else {
            width
        }
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
        let mut cursor = config.leading_padding;
        let mut geometry = Vec::with_capacity(cards.len());
        let mut prefix_ends = Vec::with_capacity(cards.len());
        let available = (viewport_width
            - config.leading_padding
            - config.trailing_padding
            - config.gap * cards.len().saturating_sub(1) as f32)
            .max(0.0);
        let preferred_total = cards
            .iter()
            .map(|card| card.width.resolve(viewport_width, config.width_limits))
            .sum::<f32>();
        let expand = cards.len() > 1 && preferred_total > 0.0 && preferred_total < available;

        for (index, card) in cards.iter().enumerate() {
            let start = cursor;
            let width = if cards.len() == 1 && viewport_width > 0.0 {
                available
            } else if expand {
                // Fill spare space proportionally without changing the user's
                // preferences. Adding another pane can restore those widths.
                card.width.resolve(viewport_width, config.width_limits) / preferred_total
                    * available
            } else {
                card.width.resolve(viewport_width, config.width_limits)
            };
            let end = if expand && index + 1 == cards.len() {
                // Absorb rounding at the outer edge, avoiding a fractional
                // scroll range when the panes exactly fill the viewport.
                viewport_width - config.trailing_padding
            } else {
                start + width
            };
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
        // An empty strip still owns the viewport.
        let total_width = total_width.max(viewport_width);

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

    pub fn focus_offset(
        &self,
        id: CardId,
        viewport_width: f32,
        current_offset: f32,
    ) -> Option<f32> {
        let card = self.card(id)?;
        let target = if card.width() > viewport_width {
            card.start
        } else {
            // Keep a fully visible pane stationary; otherwise expose only its
            // hidden edge, preserving as much of its neighbor as possible.
            current_offset.clamp(card.end - viewport_width, card.start)
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
    /// The slot the card occupies in the strip right now.
    pub width: f32,
    /// The width its contents are laid out at. A pane that is still opening
    /// has its final width here while `width` grows towards it, so the strip
    /// reveals finished contents instead of reflowing cramped ones.
    pub content_width: f32,
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

    fn take_recognized_after_clamp(&mut self, offset: f32, maximum: f32) -> bool {
        if (offset - maximum).abs() > SCROLL_SYNC_EPSILON {
            return false;
        }
        let Some(index) = self.offsets[..self.len]
            .iter()
            .position(|issued| *issued > maximum)
        else {
            return false;
        };
        self.offsets[index] = f32::NAN;
        true
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
        let target = finite_nonnegative(target);
        self.animating = (target - self.offset).abs() > POSITION_EPSILON
            || self.velocity.abs() > VELOCITY_EPSILON;
        // Too near to animate is there already. A target left a hair away
        // would never be reached, and a card whose edge fell in between would
        // stay a destination.
        self.target = if self.animating { target } else { self.offset };
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

        if (self.offset - self.target).abs() <= SETTLE_DISTANCE
            && self.velocity.abs() <= SETTLE_SPEED
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

/// Transient presentation geometry. Durable cards and `geometry()` always hold
/// the target widths, so saving or switching worktrees cannot persist a frame.
#[derive(Debug, Clone)]
struct LayoutTransition {
    from_total_width: f32,
    from_bounds: Option<Vec<(f32, f32)>>,
    from_widths: Vec<f32>,
    geometry: StripGeometry,
    elapsed: Duration,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    cards: Vec<WorkspaceCard>,
    card_indices: Arc<HashMap<CardId, usize>>,
    focused: Option<CardId>,
    viewport_width: f32,
    geometry: StripGeometry,
    layout_transition: Option<LayoutTransition>,
    /// New panes whose slot is still growing. Their contents mount at once at
    /// the final width and are revealed through the slot. This survives
    /// retargeting so rapid insertions never lay contents out in a partial one.
    opening: HashSet<CardId>,
    camera: Camera,
    scroll_sync: Option<ScrollSync>,
    visibility: HashMap<CardId, VisibilityClass>,
    visibility_bands: Option<VisibilityBands>,
    pending_widget_focus: Option<CardId>,
    pending_close: Option<PendingClose>,
    resize_limits: Option<(CardId, WidthLimits)>,
    config: WorkspaceConfig,
}

impl Workspace {
    pub fn new(config: WorkspaceConfig) -> Result<Self, WorkspaceError> {
        let config = config.validate()?;
        Ok(Self {
            cards: Vec::new(),
            card_indices: Arc::default(),
            focused: None,
            viewport_width: 0.0,
            geometry: StripGeometry::build(&[], 0.0, config),
            layout_transition: None,
            opening: HashSet::new(),
            camera: Camera::new(config.animation_omega)?,
            scroll_sync: None,
            visibility: HashMap::new(),
            visibility_bands: None,
            pending_widget_focus: None,
            pending_close: None,
            resize_limits: None,
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
        self.cards.get(*self.card_indices.get(&id)?)
    }

    pub fn geometry(&self) -> &StripGeometry {
        &self.geometry
    }

    /// The card whose trailing edge currently sits on the viewport's own
    /// trailing edge, if any.
    ///
    /// That is where whatever is docked beside the strip draws its edge, so a
    /// rule of the card's own there would double it. It follows the camera:
    /// any card can be scrolled against the edge, and the last card of a strip
    /// that does not fill the viewport never is.
    pub fn card_at_viewport_edge(&self) -> Option<CardId> {
        let edge = self.camera.offset() + self.viewport_width;
        self.presentation_geometry()
            .cards()
            .iter()
            .find(|card| (card.end - edge).abs() < 0.5)
            .map(|card| card.id)
    }

    pub fn camera(&self) -> Camera {
        self.camera
    }

    pub fn is_animating(&self) -> bool {
        self.camera.is_animating() || self.layout_transition.is_some()
    }

    /// Whether the card's contents are laid out at the width they will keep.
    /// An opening card's are, while its slot is still growing.
    pub fn width_is_settled(&self, id: CardId) -> bool {
        self.layout_transition.is_none() || self.opening.contains(&id)
    }

    pub fn presentation_geometry(&self) -> &StripGeometry {
        self.layout_transition
            .as_ref()
            .map_or(&self.geometry, |transition| &transition.geometry)
    }

    pub fn viewport_width(&self) -> f32 {
        self.viewport_width
    }

    /// Convert a drag's rendered width back into a preference. Underfilled
    /// strips expand proportionally, so storing the visible width directly
    /// would expand it again and make the divider jump ahead of the pointer.
    pub fn preferred_width_for_resize(&self, id: CardId, visible_width: f32) -> f32 {
        if self.cards.len() < 2 {
            return visible_width;
        }
        let available = (self.viewport_width
            - self.config.leading_padding
            - self.config.trailing_padding
            - self.config.gap * self.cards.len().saturating_sub(1) as f32)
            .max(0.0);
        let others = self
            .cards
            .iter()
            .filter(|card| card.id != id)
            .map(|card| {
                card.width
                    .resolve(self.viewport_width, self.config.width_limits)
            })
            .sum::<f32>();
        if others > 0.0 && visible_width + others < available {
            visible_width * (others / (available - visible_width))
        } else {
            visible_width
        }
    }
    pub fn fractional_width_limits(&self) -> WidthLimits {
        let available = self.viewport_width;
        let maximum = if available > 0.0 {
            // Fractional panes can exceed the fixed-card maximum on wide
            // screens. Keep their rendered width reachable by the splitter.
            available
        } else {
            self.config.width_limits.minimum
        };
        WidthLimits {
            minimum: self.config.width_limits.minimum.min(maximum),
            maximum,
        }
    }

    /// Existing presets can render below the drag minimum on a compact
    /// viewport. Let a gesture grow from that size without jumping to the
    /// minimum, while refusing to shrink it further.
    pub fn card_resize_limits(&self, id: CardId) -> WidthLimits {
        if let Some((resizing, limits)) = self.resize_limits
            && resizing == id
        {
            return limits;
        }
        let limits = self.fractional_width_limits();
        self.geometry.card(id).map_or(limits, |card| WidthLimits {
            minimum: limits.minimum.min(card.width()),
            maximum: limits.maximum.max(card.width()),
        })
    }

    pub fn begin_card_resize(&mut self, id: CardId) -> WidthLimits {
        let limits = self.card_resize_limits(id);
        self.resize_limits = Some((id, limits));
        limits
    }

    pub fn finish_card_resize(&mut self, id: CardId) {
        if self
            .resize_limits
            .is_some_and(|(resizing, _)| resizing == id)
        {
            self.resize_limits = None;
        }
    }

    pub fn visibility(&self, id: CardId) -> Option<VisibilityClass> {
        self.card(id).map(|_| {
            self.visibility
                .get(&id)
                .copied()
                .unwrap_or(VisibilityClass::Offscreen)
        })
    }

    /// Whether a card on screen is one the camera is leaving or crossing on
    /// its way somewhere else, rather than one it is heading for.
    pub fn is_passing(&self, id: CardId) -> bool {
        if !self.camera.is_animating() {
            return false;
        }
        let (Some(bands), Some(index)) = (&self.visibility_bands, self.card_indices.get(&id))
        else {
            return false;
        };
        bands.visible.contains(index) && !bands.arriving.contains(index)
    }

    /// Only cards in the visible/overscan band, without scanning the strip.
    pub fn mounted_visibility(&self) -> impl Iterator<Item = (CardId, VisibilityClass)> + '_ {
        self.visibility.iter().map(|(&id, &class)| (id, class))
    }

    pub(crate) fn card_indices(&self) -> &Arc<HashMap<CardId, usize>> {
        &self.card_indices
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

        let animate = focus && !self.cards.is_empty() && self.viewport_width > 0.0;
        let previous = animate.then(|| self.presentation_geometry().clone());
        let id = card.id;
        self.cards.insert(
            index,
            WorkspaceCard::new(card.id, card.accessible_name, card.width)?,
        );
        self.rebuild_card_indices();
        if self.focused.is_none() || focus {
            self.focused = Some(id);
            self.pending_widget_focus = Some(id);
        }
        self.rebuild_geometry();
        if let Some(previous) = previous {
            self.begin_layout_transition(previous);
        }
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

    /// Width presets animate; direct drag updates use `set_card_width` so the
    /// boundary continues to follow the pointer without a trailing animation.
    pub fn animate_card_width(
        &mut self,
        id: CardId,
        width: CardWidth,
    ) -> Result<ViewportUpdate, WorkspaceError> {
        let index = self.index_of(id).ok_or(WorkspaceError::UnknownCard(id))?;
        let width = width.validate()?;
        if self.cards[index].width == width {
            return Ok(ViewportUpdate::unchanged());
        }
        let previous = self.presentation_geometry().clone();
        let camera = self.camera;
        self.cards[index].width = width;
        self.rebuild_geometry();
        self.camera = camera;
        self.begin_layout_transition(previous);
        self.reveal_focused();
        Ok(self.refresh_visibility(false))
    }

    pub fn rename_card(
        &mut self,
        id: CardId,
        accessible_name: impl Into<String>,
    ) -> Result<(), WorkspaceError> {
        let accessible_name = normalize_card_name(&accessible_name.into())
            .ok_or(WorkspaceError::EmptyAccessibleName)?;
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
            self.rebuild_card_indices();
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
        self.focus_card(id, true)
    }

    pub fn focus_without_scrolling(
        &mut self,
        id: CardId,
    ) -> Result<ViewportUpdate, WorkspaceError> {
        if self.focused == Some(id) {
            return Ok(ViewportUpdate::unchanged());
        }
        self.focus_card(id, false)
    }

    fn focus_card(&mut self, id: CardId, reveal: bool) -> Result<ViewportUpdate, WorkspaceError> {
        if self.index_of(id).is_none() {
            return Err(WorkspaceError::UnknownCard(id));
        }
        if self.pending_close.is_some_and(|pending| pending.id == id) {
            return Err(WorkspaceError::CardClosing(id));
        }

        // Retarget the running spring without restarting on held-key repeats.
        self.focused = Some(id);
        self.pending_widget_focus = Some(id);
        if reveal {
            self.reveal_focused();
        }
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
        let id = self.cards[next].id;
        if self.focused == Some(id) {
            // There is no neighbour past an end of the strip. The pane is
            // still brought back into view, as any navigation would, but the
            // keyboard stays where it is: asking for it again would pull it
            // out of a find bar and back to the pane's default control.
            self.reveal_focused();
            return Ok(self.refresh_visibility(false));
        }
        self.focus(id)
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
        self.finish_card_resize(id);
        let pending = self.pending_close.ok_or(WorkspaceError::NoClosePending)?;
        if pending.id != id {
            return Err(WorkspaceError::WrongPendingClose {
                expected: pending.id,
                received: id,
            });
        }

        let previous = self.presentation_geometry().clone();
        let index = self.index_of(id).ok_or(WorkspaceError::UnknownCard(id))?;
        let previous_camera = self.camera;
        self.cards.remove(index);
        self.rebuild_card_indices();
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
        if !self.cards.is_empty() && self.viewport_width > 0.0 {
            let mut transition = LayoutTransition {
                from_total_width: previous.total_width(),
                from_widths: self
                    .geometry
                    .cards()
                    .iter()
                    .map(|card| {
                        previous
                            .card(card.id)
                            .map_or(card.width(), CardGeometry::width)
                    })
                    .collect(),
                from_bounds: Some(
                    self.geometry
                        .cards()
                        .iter()
                        .map(|card| {
                            let old = previous.card(card.id).unwrap_or(*card);
                            (old.start, old.end)
                        })
                        .collect(),
                ),
                geometry: self.geometry.clone(),
                elapsed: Duration::ZERO,
            };
            Self::interpolate_layout(
                &mut transition,
                &self.geometry,
                self.viewport_width,
                self.config,
            );
            self.layout_transition = Some(transition);
            self.camera = previous_camera;
        }
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
        let width = finite_nonnegative(width);
        if self.viewport_width == width {
            return ViewportUpdate::unchanged();
        }
        self.reflow_viewport(width, None, false);
        self.refresh_visibility(false)
    }

    /// Geometry resizing is already animated by its owner (for example the
    /// docked inspector). Keep a settled camera anchored to the same point in
    /// the leading pane instead of adding a second, lagging scroll animation.
    /// Deliberate navigation and startup reveals keep their existing spring.
    fn reflow_viewport(&mut self, width: f32, acknowledged_offset: Option<f32>, user_input: bool) {
        let animate = self.camera.is_animating() || self.viewport_width <= 0.0 || user_input;
        let anchor = (!animate)
            .then(|| {
                let offset = self.camera.offset();
                let geometry = self.presentation_geometry();
                let index = geometry.intersecting_range(offset, offset + 1.0).start;
                geometry
                    .card_at(index)
                    .map(|card| (card.id, (offset - card.start) / card.width()))
            })
            .flatten();
        self.viewport_width = width;
        self.rebuild_geometry();
        if let Some((id, fraction)) = anchor
            && let Some(card) = self.geometry.card(id)
        {
            self.camera.jump_to(card.start + fraction * card.width());
        } else if let Some(offset) = acknowledged_offset {
            self.camera.jump_to(offset);
        }
        if animate {
            self.reveal_focused();
        } else if let Some(target) = self
            .focused
            .and_then(|id| self.geometry.focus_offset(id, width, self.camera.offset()))
        {
            self.camera.jump_to(target);
        }
        self.camera.clamp(self.geometry.max_offset(width));
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
        self.native_scroll_sample(offset, viewport_width, true)
    }

    /// Native layout/clamping acknowledges geometry, not a decision to scroll
    /// away from the focused pane. Only real input may interrupt its reveal.
    pub fn native_layout_scroll(&mut self, offset: f32, viewport_width: f32) -> ViewportUpdate {
        self.native_scroll_sample(offset, viewport_width, false)
    }

    fn native_scroll_sample(
        &mut self,
        offset: f32,
        viewport_width: f32,
        user_input: bool,
    ) -> ViewportUpdate {
        let old_offset = self.camera.offset();
        let offset = finite_nonnegative(offset);
        let viewport_width = finite_nonnegative(viewport_width);
        let width_changed = (self.viewport_width - viewport_width).abs() > f32::EPSILON;
        // The first layout uses the previous content geometry with the newly
        // measured viewport. Native scrolling may clamp an issued offset to
        // that temporary maximum before reporting either Scrolled or Resized.
        // Recognize this acknowledgement before replacing the old geometry.
        let previous_maximum = self.presentation_geometry().max_offset(viewport_width);
        let clamped_programmatic = width_changed
            && self
                .scroll_sync
                .as_mut()
                .is_some_and(|sync| sync.take_recognized_after_clamp(offset, previous_maximum));
        if width_changed {
            // Native layout feedback can report the measured width before
            // Resized. Retarget fractional panes here too; the later resize
            // sees an equal width and deliberately performs no further work.
            self.reflow_viewport(
                viewport_width,
                clamped_programmatic.then_some(offset),
                user_input,
            );
        }

        if clamped_programmatic {
            return self
                .refresh_visibility((self.camera.offset() - old_offset).abs() > f32::EPSILON);
        }

        let offset = offset.min(self.presentation_geometry().max_offset(self.viewport_width));
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

        // Iced also reports the current offset when content bounds change.
        // Layout feedback without movement must not cancel a focused-card reveal.
        if !user_input || (offset - self.camera.offset()).abs() <= f32::EPSILON {
            return self
                .refresh_visibility((self.camera.offset() - old_offset).abs() > f32::EPSILON);
        }

        self.scroll_sync = None;
        self.camera.interrupt();
        self.camera.jump_to(offset);
        self.refresh_visibility((self.camera.offset() - old_offset).abs() > f32::EPSILON)
    }

    pub fn tick(&mut self, elapsed: Duration) -> ViewportUpdate {
        if let Some(transition) = &mut self.layout_transition {
            transition.elapsed = transition.elapsed.saturating_add(elapsed);
            if transition.elapsed >= LAYOUT_DURATION {
                self.layout_transition = None;
            } else {
                Self::interpolate_layout(
                    transition,
                    &self.geometry,
                    self.viewport_width,
                    self.config,
                );
            }
            self.visibility_bands = None;
        }
        let changed = self.camera.step(elapsed);
        self.camera.clamp(
            self.presentation_geometry()
                .max_offset(self.viewport_width)
                .max(self.geometry.max_offset(self.viewport_width)),
        );
        self.refresh_visibility(changed)
    }

    pub fn projection(&self, viewport_width: f32) -> ViewportProjection {
        let viewport_width = finite_nonnegative(viewport_width);
        let rebuilt_geometry;
        let (geometry, target) = if (viewport_width - self.viewport_width).abs() <= f32::EPSILON {
            (self.presentation_geometry(), &self.geometry)
        } else {
            rebuilt_geometry = StripGeometry::build(&self.cards, viewport_width, self.config);
            (&rebuilt_geometry, &rebuilt_geometry)
        };
        let offset = self
            .camera
            .offset()
            .clamp(0.0, geometry.max_offset(viewport_width));
        let bands = self.bands(geometry, target, offset, viewport_width);
        let mounted = bands
            .mounted()
            .filter_map(|index| {
                let card = geometry.card_at(index)?;
                let opening = self.opening.contains(&card.id);
                let visibility = bands.class(index);
                let width = card.width_within_boundaries();
                let content_width = if opening {
                    self.geometry
                        .card_at(index)
                        .map_or(width, |target| target.width_within_boundaries().max(width))
                } else {
                    width
                };
                Some(MountedCard {
                    id: card.id,
                    start: card.start,
                    width,
                    content_width,
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
        self.card_indices.get(&id).copied()
    }

    fn rebuild_card_indices(&mut self) {
        self.card_indices = Arc::new(
            self.cards
                .iter()
                .enumerate()
                .map(|(index, card)| (card.id, index))
                .collect(),
        );
    }

    fn rebuild_geometry(&mut self) {
        self.layout_transition = None;
        self.geometry = StripGeometry::build(&self.cards, self.viewport_width, self.config);
        self.visibility_bands = None;
        self.camera
            .clamp(self.geometry.max_offset(self.viewport_width));
    }

    fn begin_layout_transition(&mut self, previous: StripGeometry) {
        if self.viewport_width <= 0.0 {
            return;
        }
        let widths: HashMap<_, _> = previous
            .cards()
            .iter()
            .map(|card| (card.id, card.width()))
            .collect();
        let from_widths: Vec<_> = self
            .geometry
            .cards()
            .iter()
            .map(|card| widths.get(&card.id).copied().unwrap_or(1.0))
            .collect();
        if self
            .geometry
            .cards()
            .iter()
            .zip(&from_widths)
            .all(|(card, width)| (card.width() - width).abs() <= POSITION_EPSILON)
        {
            return;
        }
        let mut transition = LayoutTransition {
            from_total_width: previous.total_width(),
            from_bounds: None,
            from_widths,
            geometry: self.geometry.clone(),
            elapsed: Duration::ZERO,
        };
        self.opening.extend(
            self.geometry
                .cards()
                .iter()
                .filter(|card| !widths.contains_key(&card.id))
                .map(|card| card.id),
        );
        Self::interpolate_layout(
            &mut transition,
            &self.geometry,
            self.viewport_width,
            self.config,
        );
        self.layout_transition = Some(transition);
        self.visibility_bands = None;
    }

    fn interpolate_layout(
        transition: &mut LayoutTransition,
        target: &StripGeometry,
        viewport_width: f32,
        config: WorkspaceConfig,
    ) {
        let progress =
            (transition.elapsed.as_secs_f32() / LAYOUT_DURATION.as_secs_f32()).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        let mut cursor = config.leading_padding;
        for (index, (((card, end), target), from)) in transition
            .geometry
            .cards
            .iter_mut()
            .zip(&mut transition.geometry.prefix_ends)
            .zip(target.cards())
            .zip(&transition.from_widths)
            .enumerate()
        {
            if let Some(bounds) = &transition.from_bounds {
                // Adjacent panes share their old and target boundaries. Use
                // identical arithmetic for those edges: deriving the right
                // edge from an independently interpolated width can overlap
                // the next pane by one f32 unit at fractional restored widths.
                let (start, end) = bounds[index];
                card.start = start + (target.start - start) * eased;
                card.end = end + (target.end - end) * eased;
            } else {
                card.start = cursor;
                card.end = cursor + from + (target.width() - from) * eased;
            }
            *end = card.end;
            cursor = card.end + config.gap;
        }
        let trailing = cursor - config.gap + config.trailing_padding;
        // Keep the destination reachable during a reveal. Shrinking content
        // still retains its current extent until the final frame.
        let extent = transition.from_total_width
            + (target.total_width() - transition.from_total_width) * eased;
        transition.geometry.total_width = trailing
            .max(viewport_width)
            .max(target.total_width())
            .max(extent);
    }

    fn reveal_focused(&mut self) {
        let Some(focused) = self.focused else {
            return;
        };
        if let Some(target) =
            self.geometry
                .focus_offset(focused, self.viewport_width, self.camera.target())
        {
            self.camera.animate_to(target);
        }
    }

    fn refresh_visibility(&mut self, offset_changed: bool) -> ViewportUpdate {
        if self.layout_transition.is_none() {
            self.opening.clear();
        } else {
            self.opening.retain(|id| self.card_indices.contains_key(id));
        }
        let bands = self.bands(
            self.presentation_geometry(),
            &self.geometry,
            self.camera.offset(),
            self.viewport_width,
        );
        if self.visibility_bands.as_ref() == Some(&bands) {
            return ViewportUpdate {
                offset_changed,
                transitions: Vec::new(),
            };
        }

        let mut transitions = Vec::new();
        self.visibility.retain(|id, class| {
            let mounted = self
                .card_indices
                .get(id)
                .is_some_and(|index| bands.is_mounted(*index));
            if !mounted {
                transitions.push(VisibilityTransition {
                    id: *id,
                    from: *class,
                    to: VisibilityClass::Offscreen,
                });
            }
            mounted
        });
        for index in bands.mounted() {
            let id = self.geometry.cards[index].id;
            let class = bands.class(index);
            let previous = self
                .visibility
                .insert(id, class)
                .unwrap_or(VisibilityClass::Offscreen);
            if previous != class {
                transitions.push(VisibilityTransition {
                    id,
                    from: previous,
                    to: class,
                });
            }
        }
        transitions.sort_unstable_by_key(|transition| {
            self.card_indices
                .get(&transition.id)
                .copied()
                .unwrap_or(usize::MAX)
        });
        self.visibility_bands = Some(bands);
        ViewportUpdate {
            offset_changed,
            transitions,
        }
    }

    /// Which cards are mounted, and as what, with the camera at `offset` over
    /// `geometry`.
    ///
    /// The cards that will be on screen once the camera reaches its target
    /// count as on screen already. Navigation scrolls to its destination at
    /// once, so classing a pane by where the camera is going lets it mount,
    /// hydrate and take the keyboard while the camera travels, instead of
    /// starting when it arrives. The destination is read from `target`, the
    /// geometry a layout transition is moving the presentation towards.
    fn bands(
        &self,
        geometry: &StripGeometry,
        target: &StripGeometry,
        offset: f32,
        viewport_width: f32,
    ) -> VisibilityBands {
        let viewport_width = finite_nonnegative(viewport_width);
        let viewport_start = finite_nonnegative(offset);
        let viewport_end = viewport_start + viewport_width;
        let overscan = finite_nonnegative(self.config.overscan);
        let destination = self
            .camera
            .target()
            .clamp(0.0, target.max_offset(viewport_width));
        VisibilityBands {
            visible: geometry.intersecting_range(viewport_start, viewport_end),
            arriving: target.intersecting_range(destination, destination + viewport_width),
            overscan: geometry.intersecting_range(
                (viewport_start - overscan).max(0.0),
                viewport_end + overscan,
            ),
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
    /// On screen now.
    visible: std::ops::Range<usize>,
    /// On screen once the camera reaches its target.
    arriving: std::ops::Range<usize>,
    /// Around what is on screen now, kept mounted so scrolling reveals
    /// finished panes.
    overscan: std::ops::Range<usize>,
}

impl VisibilityBands {
    fn is_mounted(&self, index: usize) -> bool {
        self.overscan.contains(&index) || self.arriving.contains(&index)
    }

    /// The mounted cards in strip order. A far destination is not adjacent to
    /// the overscan band, so the cards between the two stay unmounted.
    fn mounted(&self) -> impl Iterator<Item = usize> + use<> {
        let before = self.arriving.start..self.arriving.end.min(self.overscan.start);
        let after = self.arriving.start.max(self.overscan.end)..self.arriving.end;
        before.chain(self.overscan.clone()).chain(after)
    }

    fn class(&self, index: usize) -> VisibilityClass {
        if self.visible.contains(&index) || self.arriving.contains(&index) {
            VisibilityClass::Visible
        } else {
            VisibilityClass::Overscan
        }
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
    fn width_animation_preserves_targets_retargets_continuously_and_stops() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fraction(0.5); 2], 800.0);
        let id = workspace.focused().unwrap();
        workspace
            .animate_card_width(id, CardWidth::Fraction(1.0))
            .unwrap();
        assert_eq!(workspace.card(id).unwrap().width, CardWidth::Fraction(1.0));
        assert_eq!(workspace.geometry().card(id).unwrap().width(), 800.0);
        assert_eq!(
            workspace.presentation_geometry().card(id).unwrap().width(),
            400.0
        );
        workspace.tick(Duration::from_millis(80));
        let intermediate = workspace.presentation_geometry().card(id).unwrap().width();
        assert!(intermediate > 400.0 && intermediate < 800.0);
        assert!(workspace.projection(800.0).mounted_are_disjoint());
        workspace
            .animate_card_width(id, CardWidth::Fraction(1.0 / 3.0))
            .unwrap();
        assert_eq!(
            workspace.presentation_geometry().card(id).unwrap().width(),
            intermediate
        );
        workspace.tick(LAYOUT_DURATION);
        assert_eq!(workspace.presentation_geometry(), workspace.geometry());
        assert!(!workspace.is_animating());
        let settled = workspace.projection(800.0);
        assert_eq!(
            workspace.tick(Duration::from_secs(1)),
            ViewportUpdate::unchanged()
        );
        assert_eq!(workspace.projection(800.0), settled);
    }

    #[test]
    fn closing_a_pane_collapses_its_gap_without_teleporting_its_neighbors() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fraction(0.4); 3], 800.0);
        let removed = workspace.cards[1].id;
        let survivor = workspace.cards[2].id;
        let before = workspace
            .presentation_geometry()
            .card(survivor)
            .unwrap()
            .start;
        workspace.request_close(removed).unwrap();
        workspace.commit_close(removed).unwrap();
        let target = workspace.geometry().card(survivor).unwrap().start;
        assert!(target < before);
        assert_eq!(
            workspace
                .presentation_geometry()
                .card(survivor)
                .unwrap()
                .start,
            before
        );
        workspace.tick(Duration::from_millis(50));
        let moving = workspace
            .presentation_geometry()
            .card(survivor)
            .unwrap()
            .start;
        assert!(moving > target && moving < before);
        assert!(workspace.projection(800.0).mounted_are_disjoint());
        workspace.tick(LAYOUT_DURATION);
        assert_eq!(workspace.presentation_geometry(), workspace.geometry());
    }

    #[test]
    fn restored_fractional_panes_remain_disjoint_through_every_close_frame() {
        // Restored from the native ten-pane layout that panicked while closing
        // its second pane. Keep the f32 values serialized by the workspace.
        let fractions: Vec<f32> = serde_json::from_str(
            "[0.66666668653488159,0.5,0.5634920597076416,0.579365074634552,
              0.5,0.3333333432674408,0.3333333432674408,0.3333333432674408,0.5,0.5]",
        )
        .unwrap();
        let widths = fractions
            .into_iter()
            .map(CardWidth::Fraction)
            .collect::<Vec<_>>();
        for viewport in [799.75, 1200.5, 1640.0, 1641.0, 1642.0, 1643.0, 1902.25] {
            for closing in 0..widths.len() {
                let mut workspace = workspace_with_widths(&widths, viewport);
                let removed = workspace.order().nth(closing).unwrap();
                workspace.focus(removed).unwrap();
                workspace.tick(LAYOUT_DURATION);
                workspace.request_close(removed).unwrap();
                workspace.commit_close(removed).unwrap();
                for frame in 0..=220 {
                    let geometry = workspace.presentation_geometry();
                    for pair in geometry.cards().windows(2) {
                        assert!(
                            pair[0].end <= pair[1].start,
                            "viewport {viewport}, closing {closing}, frame {frame}: {} > {}",
                            pair[0].end,
                            pair[1].start
                        );
                    }
                    assert!(workspace.projection(viewport).mounted_are_disjoint());
                    workspace.tick(Duration::from_millis(1));
                }
                assert_eq!(workspace.presentation_geometry(), workspace.geometry());
                assert!(!workspace.order().any(|card| card == removed));
                workspace.check_invariants().unwrap();
            }
        }
    }

    #[test]
    fn closing_a_narrow_pane_preserves_shared_projection_boundaries() {
        let mut workspace = workspace_with_widths(
            &[
                CardWidth::Fraction(0.2),
                CardWidth::Fraction(1.2),
                CardWidth::Fraction(0.6),
            ],
            1000.0,
        );
        let removed = workspace.order().next().unwrap();
        workspace.request_close(removed).unwrap();
        workspace.commit_close(removed).unwrap();
        for _ in 0..=220 {
            assert!(workspace.projection(1000.0).mounted_are_disjoint());
            workspace.tick(Duration::from_millis(1));
        }
    }

    #[test]
    fn inserted_pane_moves_neighbors_without_overlapping_or_losing_focus() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(300.0); 2], 900.0);
        let second = workspace.order().nth(1).unwrap();
        let inserted = card(CardWidth::Fixed(300.0));
        let inserted_id = inserted.id;
        let update = workspace.add_card(inserted, 1, true).unwrap();
        assert!(update.transitions.contains(&VisibilityTransition {
            id: inserted_id,
            from: VisibilityClass::Offscreen,
            to: VisibilityClass::Visible,
        }));
        let initial = workspace
            .presentation_geometry()
            .card(second)
            .unwrap()
            .start;
        let target = workspace.geometry().card(second).unwrap().start;
        assert!(initial < target);
        assert_eq!(workspace.focused(), Some(inserted_id));
        assert_eq!(workspace.take_widget_focus_request(), Some(inserted_id));
        // Only the opening pane is at a width it will keep; its neighbour is
        // still being moved by the same transition.
        assert!(workspace.width_is_settled(inserted_id));
        assert!(!workspace.width_is_settled(second));
        assert_eq!(
            workspace.visibility(inserted_id),
            Some(VisibilityClass::Visible)
        );
        let final_width = workspace
            .geometry()
            .card(inserted_id)
            .unwrap()
            .width_within_boundaries();
        let opening = |workspace: &Workspace| {
            workspace
                .projection(900.0)
                .mounted
                .into_iter()
                .find(|pane| pane.id == inserted_id)
                .unwrap()
        };
        assert!(opening(&workspace).width < final_width);
        assert_eq!(opening(&workspace).content_width, final_width);
        workspace.tick(Duration::from_millis(80));
        let intermediate = workspace
            .presentation_geometry()
            .card(second)
            .unwrap()
            .start;
        assert!(intermediate > initial && intermediate < target);
        assert!(workspace.projection(900.0).mounted_are_disjoint());
        assert!(opening(&workspace).width < final_width);
        assert_eq!(opening(&workspace).content_width, final_width);
        workspace.tick(LAYOUT_DURATION);
        assert_eq!(opening(&workspace).width, final_width);
        assert_eq!(opening(&workspace).content_width, final_width);
        assert_eq!(workspace.take_widget_focus_request(), None);
        assert!(workspace.width_is_settled(second));
        assert_eq!(
            workspace
                .presentation_geometry()
                .card(second)
                .unwrap()
                .start,
            target
        );
        for _ in 0..120 {
            if !workspace.is_animating() {
                break;
            }
            workspace.tick(Duration::from_millis(16));
            assert!(workspace.projection(900.0).mounted_are_disjoint());
        }
        assert!(!workspace.is_animating());
        assert_eq!(workspace.focused(), Some(inserted_id));
    }

    #[test]
    fn opening_panes_mount_at_their_final_width_across_rapid_insertions_and_width_retargets() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(300.0)], 1200.0);
        let first = card(CardWidth::Fixed(300.0));
        let first_id = first.id;
        let update = workspace.add_card(first, 1, true).unwrap();
        assert!(
            update
                .transitions
                .iter()
                .any(|transition| transition.id == first_id && transition.to.is_mounted())
        );
        assert_eq!(workspace.take_widget_focus_request(), Some(first_id));
        workspace.tick(Duration::from_millis(80));
        let second = card(CardWidth::Fixed(300.0));
        let second_id = second.id;
        workspace.add_card(second, 2, true).unwrap();
        assert_eq!(workspace.take_widget_focus_request(), Some(second_id));
        workspace.tick(Duration::from_millis(80));
        workspace
            .animate_card_width(first_id, CardWidth::Fixed(350.0))
            .unwrap();
        let mounted = |workspace: &Workspace, id| {
            workspace
                .projection(1200.0)
                .mounted
                .into_iter()
                .find(|card| card.id == id)
                .expect("an opening pane is mounted")
        };
        let target = |workspace: &Workspace, id| {
            workspace
                .geometry()
                .card(id)
                .unwrap()
                .width_within_boundaries()
        };
        for _ in 0..13 {
            let update = workspace.tick(Duration::from_millis(16));
            assert!(workspace.projection(1200.0).mounted_are_disjoint());
            // Contents never reflow through the widths the slot passes.
            for id in [first_id, second_id] {
                let card = mounted(&workspace, id);
                assert!(card.width < target(&workspace, id));
                assert_eq!(card.content_width, target(&workspace, id));
            }
            assert!(update.transitions.is_empty());
        }
        workspace.tick(Duration::from_millis(12));
        for id in [first_id, second_id] {
            let card = mounted(&workspace, id);
            assert_eq!(card.width, target(&workspace, id));
            assert_eq!(card.content_width, target(&workspace, id));
        }
    }

    #[test]
    fn a_pane_opened_past_the_viewport_edge_is_visible_while_the_camera_travels() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fraction(0.5); 2], 800.0);
        let opened = card(CardWidth::Fraction(0.5));
        let id = opened.id;
        let update = workspace.add_card(opened, 2, true).unwrap();
        assert!(update.transitions.contains(&VisibilityTransition {
            id,
            from: VisibilityClass::Offscreen,
            to: VisibilityClass::Visible,
        }));
        let pane = workspace
            .projection(800.0)
            .mounted
            .into_iter()
            .find(|pane| pane.id == id)
            .unwrap();
        // Its slot has not been scrolled to yet.
        assert!(pane.start >= workspace.camera.offset() + 800.0);
        assert_eq!(pane.visibility, VisibilityClass::Visible);
        for _ in 0..40 {
            let update = workspace.tick(Duration::from_millis(16));
            assert!(
                !update
                    .transitions
                    .iter()
                    .any(|transition| transition.id == id)
            );
        }
        assert!(!workspace.is_animating());
        assert_eq!(workspace.visibility(id), Some(VisibilityClass::Visible));
    }

    #[test]
    fn canceled_opening_mounts_at_full_size_and_removed_openings_never_reappear() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(300.0)], 900.0);
        let opening = card(CardWidth::Fixed(300.0));
        let id = opening.id;
        workspace.add_card(opening, 1, true).unwrap();
        workspace.resize_viewport(1000.0);
        assert!(
            workspace
                .projection(1000.0)
                .mounted_ids()
                .any(|mounted| mounted == id)
        );
        assert_eq!(workspace.take_widget_focus_request(), Some(id));
        let removed = card(CardWidth::Fixed(300.0));
        let removed_id = removed.id;
        workspace.add_card(removed, 2, true).unwrap();
        workspace.request_close(removed_id).unwrap();
        workspace.commit_close(removed_id).unwrap();
        workspace.tick(LAYOUT_DURATION);
        assert!(workspace.card(removed_id).is_none());
        assert!(
            !workspace
                .projection(1000.0)
                .mounted_ids()
                .any(|mounted| mounted == removed_id)
        );
        assert!(workspace.opening.is_empty());
    }

    #[test]
    fn a_pane_the_camera_only_passes_is_told_apart_from_its_destination() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(300.0); 8], 600.0);
        let ids: Vec<_> = workspace.order().collect();
        assert!(!workspace.is_passing(ids[0]));
        workspace.focus(ids[7]).unwrap();
        // The pane being left is still on screen without being where the
        // camera is going. One in between is not on screen yet.
        assert!(workspace.is_passing(ids[0]));
        assert!(!workspace.is_passing(ids[4]));
        assert!(!workspace.is_passing(ids[7]));
        let mut crossed = false;
        for _ in 0..240 {
            workspace.tick(Duration::from_millis(16));
            crossed |= workspace.is_passing(ids[4]);
            assert!(!workspace.is_passing(ids[7]));
        }
        assert!(crossed);
        assert!(!workspace.is_animating());
        assert!(ids.iter().all(|id| !workspace.is_passing(*id)));
    }

    #[test]
    fn first_pane_has_its_space_immediately() {
        let mut workspace = Workspace::default();
        workspace.resize_viewport(900.0);
        let first = card(CardWidth::Fixed(300.0));
        let id = first.id;
        workspace.add_card(first, 0, true).unwrap();
        assert!(!workspace.is_animating());
        assert!(
            workspace
                .projection(900.0)
                .mounted_ids()
                .any(|mounted| mounted == id)
        );
        assert_eq!(workspace.take_widget_focus_request(), Some(id));
    }
    #[test]
    fn held_navigation_reaches_the_strip_edge_without_restarting_the_camera() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(300.0); 4], 400.0);
        for _ in 0..3 {
            workspace.focus_adjacent(FocusDirection::Right).unwrap();
            workspace.tick(Duration::from_millis(20));
        }
        let target = workspace.camera().target();
        for _ in 0..20 {
            workspace.focus_adjacent(FocusDirection::Right).unwrap();
            workspace.tick(Duration::from_millis(20));
        }
        assert_eq!(workspace.focused_index(), Some(3));
        assert!((workspace.camera().offset() - target).abs() < 1.0);

        workspace.native_scroll(0.0, 400.0);
        assert!(!workspace.camera().is_animating());
        // A boundary key must still reveal its focused column after manual
        // scrolling away, including a restored worktree's offscreen focus.
        workspace.focus_adjacent(FocusDirection::Right).unwrap();
        assert!(workspace.camera().is_animating());
        assert_eq!(workspace.camera().target(), target);
    }

    #[test]
    fn visibility_tracks_identity_when_reordering_inside_the_same_bands() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(500.0); 4], 600.0);
        let ids: Vec<_> = workspace.order().collect();
        assert_eq!(workspace.visibility(ids[0]), Some(VisibilityClass::Visible));
        assert_eq!(
            workspace.visibility(ids[3]),
            Some(VisibilityClass::Offscreen)
        );

        let update = workspace.reorder(ids[3], 0).unwrap();
        assert!(update.transitions.contains(&VisibilityTransition {
            id: ids[3],
            from: VisibilityClass::Offscreen,
            to: VisibilityClass::Visible,
        }));
        assert_eq!(workspace.card(ids[3]).unwrap().id, ids[3]);
        assert_eq!(workspace.focused_index(), Some(1));
        let projection = workspace.projection(600.0);
        for mounted in projection.mounted {
            assert_eq!(workspace.visibility(mounted.id), Some(mounted.visibility));
        }

        workspace.request_close(ids[3]).unwrap();
        workspace.commit_close(ids[3]).unwrap();
        assert_eq!(workspace.card(ids[3]), None);
        assert_eq!(workspace.visibility(ids[3]), None);
        assert_eq!(workspace.focused_index(), Some(0));
    }

    #[test]
    fn scrolling_within_a_band_emits_no_duplicate_exposure_changes() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(500.0); 6], 600.0);
        let initial: HashMap<_, _> = workspace.mounted_visibility().collect();
        for offset in [1.0, 2.0, 3.0] {
            let update = workspace.native_scroll(offset, 600.0);
            assert!(update.offset_changed);
            assert!(update.transitions.is_empty());
            assert_eq!(
                workspace.mounted_visibility().collect::<HashMap<_, _>>(),
                initial
            );
        }
        let update = workspace.native_scroll(2_000.0, 600.0);
        for (id, class) in initial {
            assert!(update.transitions.contains(&VisibilityTransition {
                id,
                from: class,
                to: VisibilityClass::Offscreen,
            }));
        }
        assert!(
            workspace
                .native_scroll(2_000.0, 600.0)
                .transitions
                .is_empty()
        );
    }

    #[test]
    fn unchanged_width_does_not_restart_reveal_after_manual_scrolling() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(500.0); 4], 600.0);
        workspace.native_scroll(700.0, 600.0);
        let camera = workspace.camera();
        assert_eq!(
            workspace.resize_viewport(600.0),
            ViewportUpdate::unchanged()
        );
        assert_eq!(workspace.camera(), camera);
        assert!(!workspace.camera().is_animating());
    }

    #[test]
    fn a_card_name_is_bounded_and_stripped_of_formatting() {
        // The card's process chooses this string, and it reaches the card
        // header, five accessibility strings, the header bar, the compositor
        // toplevel title and the state database.
        assert_eq!(normalize_card_name("  fish  "), Some("fish".into()));
        assert_eq!(
            normalize_card_name("two\nlines\u{2028}and\tmore"),
            Some("two lines and more".into())
        );
        // A bidi override could reorder the window title around the
        // application name and make one card impersonate another.
        assert_eq!(
            normalize_card_name("safe\u{202e}gnitiops"),
            Some("safegnitiops".into())
        );
        assert_eq!(normalize_card_name("\u{7}\u{1b}"), None);
        assert_eq!(normalize_card_name(""), None);
        assert_eq!(normalize_card_name("   \n\t "), None);

        let flood = "A".repeat(20_000_000);
        let normalized = normalize_card_name(&flood).expect("letters survive");
        assert_eq!(normalized.chars().count(), MAX_CARD_NAME_CHARS);
        // Truncation counts characters, so a multi-byte name cannot smuggle
        // more than the bound past it either.
        let wide = "→".repeat(1_000);
        assert_eq!(
            normalize_card_name(&wide)
                .expect("arrows survive")
                .chars()
                .count(),
            MAX_CARD_NAME_CHARS
        );
    }

    #[test]
    fn every_ingress_normalises_the_name_it_is_given() {
        // Names arrive from a reported title, a directory name and a restored
        // session. Normalising at both constructors is what makes the bound
        // hold for all of them, including the ones a restart replays.
        let hostile = format!("we\u{202e}b\n{}", "A".repeat(20_000_000));
        let card = WorkspaceCard::new(CardId::new(), hostile.clone(), CardWidth::Fixed(320.0))
            .expect("letters survive");
        assert_eq!(card.accessible_name.chars().count(), MAX_CARD_NAME_CHARS);
        assert!(!card.accessible_name.contains('\u{202e}'));

        let mut workspace = Workspace::default();
        workspace.resize_viewport(600.0);
        let id = card.id;
        workspace.add_card(card, 0, true).unwrap();
        workspace.rename_card(id, hostile).unwrap();
        let renamed = &workspace.cards()[0].accessible_name;
        assert_eq!(renamed.chars().count(), MAX_CARD_NAME_CHARS);
        assert!(!renamed.contains('\u{202e}'));

        // A name of nothing legible is refused, so a card keeps the one it had
        // rather than losing its header, its accessible name and the window
        // title at once.
        assert!(workspace.rename_card(id, "\u{202e}\u{7}").is_err());
        assert_eq!(
            workspace.cards()[0].accessible_name.chars().count(),
            MAX_CARD_NAME_CHARS
        );
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
                    assert!(cards.windows(2).all(|pair| pair[0].end <= pair[1].start));
                    assert_eq!(cards[1].start, cards[0].end + workspace.config.gap);
                    assert_eq!(cards[2].start, cards[1].end + workspace.config.gap);
                    assert_eq!(
                        workspace.geometry().total_width(),
                        (cards[2].end + workspace.config.trailing_padding).max(800.0)
                    );
                }
            }
        }
    }

    #[test]
    fn the_card_at_the_viewport_edge_follows_the_camera() {
        let mut filled = workspace_with_widths(&[CardWidth::Fraction(0.5); 4], 800.0);
        let cards: Vec<_> = filled
            .geometry()
            .cards()
            .iter()
            .map(|card| card.id)
            .collect();
        // Unscrolled, the second of four half-width cards meets the edge.
        assert_eq!(filled.card_at_viewport_edge(), Some(cards[1]));
        filled.camera.jump_to(200.0);
        assert_eq!(filled.card_at_viewport_edge(), None);
        filled.camera.jump_to(400.0);
        assert_eq!(filled.card_at_viewport_edge(), Some(cards[2]));

        let underfilled = workspace_with_widths(&[CardWidth::Fraction(0.3); 2], 800.0);
        assert_eq!(
            underfilled.card_at_viewport_edge(),
            Some(underfilled.cards()[1].id)
        );
        let lone = workspace_with_widths(&[CardWidth::Fraction(0.5)], 800.0);
        assert_eq!(lone.card_at_viewport_edge(), lone.focused());
    }

    #[test]
    fn focus_offset_moves_only_far_enough_to_expose_a_hidden_edge() {
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
        assert_eq!(geometry.focus_offset(ids[0], 700.0, 0.0), Some(0.0));
        assert_eq!(geometry.focus_offset(ids[1], 700.0, 0.0), Some(260.0));
        assert_eq!(geometry.focus_offset(ids[1], 700.0, 300.0), Some(300.0));
        assert_eq!(geometry.focus_offset(ids[1], 700.0, 700.0), Some(480.0));
        assert_eq!(geometry.focus_offset(ids[2], 700.0, 260.0), Some(740.0));
        assert_eq!(geometry.focus_offset(ids[0], 700.0, 260.0), Some(0.0));
    }

    #[test]
    fn the_camera_takes_its_target_once_the_rest_of_the_way_cannot_be_seen() {
        let frame = Duration::from_micros(16_667);
        let mut camera = Camera::new(WorkspaceConfig::default().animation_omega).unwrap();
        camera.animate_to(1_000.0);
        let mut elapsed = Duration::ZERO;
        let mut previous = camera.offset();
        while camera.is_animating() {
            previous = camera.offset();
            camera.step(frame);
            elapsed += frame;
            assert!(elapsed < Duration::from_secs(1), "the camera never settled");
        }
        assert_eq!(camera.offset(), 1_000.0);
        assert_eq!(camera.velocity(), 0.0);
        // The last frame is the one that closes the remaining half pixel, so
        // nothing jumps; and the strip stops rebuilding well before the old
        // quarter-pixel, half-pixel-per-second rule let it (some 460 ms).
        assert!(camera.offset() - previous < 1.0);
        assert!(elapsed < Duration::from_millis(360), "{elapsed:?}");
        assert!(!camera.step(frame));
    }

    #[test]
    fn a_camera_passing_its_target_at_speed_is_not_stopped_on_it() {
        let mut camera = Camera::new(WorkspaceConfig::default().animation_omega).unwrap();
        camera.animate_to(1_000.0);
        camera.step(Duration::from_millis(40));
        let passing = camera.offset();
        assert!(camera.velocity() > SETTLE_SPEED);

        // Retargeted to where it already is, the camera is on its target but
        // still carrying its speed, and has to be let run on and come back.
        camera.animate_to(passing);
        assert!(camera.is_animating());
        camera.step(Duration::from_millis(4));
        assert!(camera.is_animating());
        assert!(camera.offset() > passing);
        for _ in 0..120 {
            camera.step(Duration::from_micros(16_667));
        }
        assert!(!camera.is_animating());
        assert_eq!(camera.offset(), passing);
    }

    #[test]
    fn navigating_past_an_end_of_the_strip_leaves_the_keyboard_where_it_is() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(500.0); 4], 1_000.0);
        let ids = workspace.order().collect::<Vec<_>>();
        for (end, direction) in [
            (ids[0], FocusDirection::Left),
            (ids[3], FocusDirection::Right),
        ] {
            workspace.focus(end).unwrap();
            while workspace.is_animating() {
                workspace.tick(Duration::from_millis(16));
            }
            assert_eq!(workspace.take_widget_focus_request(), Some(end));

            assert_eq!(
                workspace.focus_adjacent(direction).unwrap(),
                ViewportUpdate::unchanged()
            );
            assert_eq!(workspace.focused(), Some(end));
            assert_eq!(workspace.take_widget_focus_request(), None);
            assert!(!workspace.is_animating());
        }

        // A pane the user scrolled away from is still brought back.
        workspace.native_scroll(0.0, 1_000.0);
        workspace.focus_adjacent(FocusDirection::Right).unwrap();
        assert_eq!(workspace.camera().target(), 1_000.0);
        assert_eq!(workspace.take_widget_focus_request(), None);

        // Anywhere else the neighbour takes the keyboard as before.
        workspace.focus_adjacent(FocusDirection::Left).unwrap();
        assert_eq!(workspace.take_widget_focus_request(), Some(ids[2]));
    }

    #[test]
    fn a_far_destination_mounts_and_takes_the_keyboard_before_the_camera_arrives() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(500.0); 12], 1_000.0);
        let ids = workspace.order().collect::<Vec<_>>();
        workspace.take_widget_focus_request();
        let mounted = |workspace: &Workspace| -> Vec<usize> {
            workspace
                .projection(1_000.0)
                .mounted_ids()
                .map(|id| ids.iter().position(|candidate| *candidate == id).unwrap())
                .collect()
        };
        // Two panes fill the viewport and the overscan reaches one more.
        assert_eq!(mounted(&workspace), [0, 1, 2]);

        let update = workspace.focus(ids[9]).unwrap();
        // The camera has not moved, yet the destination is already what the
        // strip mounts, reveals and hands the keyboard to. The panes it will
        // pass on the way are not mounted for it.
        assert_eq!(workspace.camera().offset(), 0.0);
        assert_eq!(workspace.camera().target(), 4_000.0);
        assert_eq!(workspace.take_widget_focus_request(), Some(ids[9]));
        assert_eq!(mounted(&workspace), [0, 1, 2, 8, 9]);
        for index in [0, 1, 8, 9] {
            assert_eq!(
                workspace.visibility(ids[index]),
                Some(VisibilityClass::Visible)
            );
        }
        assert_eq!(
            workspace.visibility(ids[2]),
            Some(VisibilityClass::Overscan)
        );
        assert_eq!(
            update.transitions,
            [8, 9].map(|index| VisibilityTransition {
                id: ids[index],
                from: VisibilityClass::Offscreen,
                to: VisibilityClass::Visible,
            })
        );
        let projection = workspace.projection(1_000.0);
        assert!(projection.mounted_are_disjoint());
        assert_eq!(
            projection
                .mounted
                .iter()
                .map(|card| card.visibility)
                .collect::<Vec<_>>(),
            [
                VisibilityClass::Visible,
                VisibilityClass::Visible,
                VisibilityClass::Overscan,
                VisibilityClass::Visible,
                VisibilityClass::Visible,
            ]
        );

        // Arriving changes nothing about the destination: it never leaves the
        // mounted set or drops out of the visible class on the way.
        while workspace.is_animating() {
            let update = workspace.tick(Duration::from_millis(16));
            assert!(
                update
                    .transitions
                    .iter()
                    .all(|transition| ![ids[8], ids[9]].contains(&transition.id))
            );
            assert!(workspace.projection(1_000.0).mounted_are_disjoint());
        }
        assert_eq!(mounted(&workspace), [7, 8, 9, 10]);
    }

    #[test]
    fn a_destination_is_released_when_navigation_retargets_or_the_user_scrolls_away() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(500.0); 12], 1_000.0);
        let ids = workspace.order().collect::<Vec<_>>();
        workspace.focus(ids[9]).unwrap();
        assert_eq!(workspace.visibility(ids[9]), Some(VisibilityClass::Visible));

        // Held navigation moves the destination on; the one it skipped holds
        // nothing.
        let update = workspace.focus(ids[5]).unwrap();
        assert_eq!(
            workspace.visibility(ids[9]),
            Some(VisibilityClass::Offscreen)
        );
        assert_eq!(workspace.visibility(ids[5]), Some(VisibilityClass::Visible));
        assert!(update.transitions.contains(&VisibilityTransition {
            id: ids[9],
            from: VisibilityClass::Visible,
            to: VisibilityClass::Offscreen,
        }));
        assert_eq!(workspace.take_widget_focus_request(), Some(ids[5]));

        // A scroll of the user's own cancels the journey, and with it the
        // claim of a destination the camera will no longer reach.
        workspace.tick(Duration::from_millis(16));
        let update = workspace.native_scroll(40.0, 1_000.0);
        assert!(!workspace.is_animating());
        assert_eq!(
            workspace.visibility(ids[5]),
            Some(VisibilityClass::Offscreen)
        );
        assert!(update.transitions.contains(&VisibilityTransition {
            id: ids[5],
            from: VisibilityClass::Visible,
            to: VisibilityClass::Offscreen,
        }));
        assert_eq!(
            workspace
                .projection(1_000.0)
                .mounted_ids()
                .collect::<Vec<_>>(),
            ids[..3]
        );
    }

    #[test]
    fn a_destination_is_not_mounted_before_the_viewport_is_measured() {
        let mut workspace = Workspace::default();
        for index in 0..12 {
            workspace
                .add_card(card(CardWidth::Fixed(500.0)), index, index == 0)
                .unwrap();
        }
        let ids = workspace.order().collect::<Vec<_>>();
        workspace.focus(ids[9]).unwrap();
        // Nothing is on screen in a viewport with no width, so the keyboard
        // waits for the measurement rather than going to an unmounted pane.
        assert_eq!(
            workspace.visibility(ids[9]),
            Some(VisibilityClass::Offscreen)
        );
        assert_eq!(workspace.take_widget_focus_request(), None);
        workspace.resize_viewport(1_000.0);
        assert_eq!(workspace.take_widget_focus_request(), Some(ids[9]));
    }

    #[test]
    fn hover_activation_preserves_the_camera_and_does_not_repeat_widget_focus() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fraction(0.5); 3], 1_000.0);
        let ids = workspace.order().collect::<Vec<_>>();
        workspace.native_scroll(125.0, 1_000.0);
        workspace.take_widget_focus_request();
        let camera = workspace.camera();
        workspace.focus_without_scrolling(ids[1]).unwrap();
        assert_eq!(workspace.camera(), camera);
        assert_eq!(workspace.focused(), Some(ids[1]));
        assert_eq!(workspace.take_widget_focus_request(), Some(ids[1]));
        workspace.focus_without_scrolling(ids[1]).unwrap();
        assert_eq!(workspace.take_widget_focus_request(), None);
        workspace.focus(ids[2]).unwrap();
        let moving = workspace.camera();
        workspace.focus_without_scrolling(ids[0]).unwrap();
        assert_eq!(workspace.camera(), moving);
    }

    #[test]
    fn cycled_fractional_columns_use_the_actual_viewport_without_peek_or_width_clamps() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fraction(0.61)], 320.0);
        let id = workspace.focused().unwrap();
        workspace
            .add_card(card(CardWidth::Fraction(1.0)), 1, false)
            .unwrap();
        for viewport in [320.0, 800.0, 3_840.0] {
            workspace.resize_viewport(viewport);
            workspace
                .set_card_width(id, CardWidth::Fraction(0.61))
                .unwrap();
            for fraction in [2.0 / 3.0, 1.0, 1.0 / 3.0, 0.5, 2.0 / 3.0] {
                let next = workspace.card(id).unwrap().width.next_preset();
                workspace.set_card_width(id, next).unwrap();
                assert!(
                    (workspace.geometry().card(id).unwrap().width() - viewport * fraction).abs()
                        < 0.001
                );
                assert_eq!(workspace.focused(), Some(id));
            }
        }
    }

    #[test]
    fn multiple_panes_fill_available_width_in_proportion_without_changing_preferences() {
        let config = WorkspaceConfig {
            leading_padding: 12.0,
            trailing_padding: 18.0,
            gap: 6.0,
            ..WorkspaceConfig::default()
        };
        let mut workspace = Workspace::new(config).unwrap();
        workspace.resize_viewport(1_200.0);
        for (index, fraction) in [0.2, 0.1, 0.1].into_iter().enumerate() {
            workspace
                .add_card(card(CardWidth::Fraction(fraction)), index, false)
                .unwrap();
        }
        let widths = workspace
            .geometry()
            .cards()
            .iter()
            .map(|card| card.width())
            .collect::<Vec<_>>();
        assert_eq!(widths, vec![579.0, 289.5, 289.5]);
        assert_eq!(workspace.geometry().cards()[0].start, 12.0);
        assert_eq!(workspace.geometry().cards()[2].end, 1_182.0);
        assert_eq!(workspace.geometry().max_offset(1_200.0), 0.0);
        assert_eq!(workspace.cards()[0].width, CardWidth::Fraction(0.2));
        workspace
            .add_card(card(CardWidth::Fraction(1.0)), 3, false)
            .unwrap();
        assert_eq!(workspace.geometry().cards()[0].width(), 240.0);
        assert!(workspace.geometry().max_offset(1_200.0) > 0.0);
        workspace.resize_viewport(0.0);
        assert!(
            workspace
                .geometry()
                .cards()
                .iter()
                .all(|card| card.width().is_finite() && card.width() > 0.0)
        );
    }

    #[test]
    fn dragging_expanded_panes_tracks_the_rendered_width_through_overflow() {
        let mut workspace = workspace_with_widths(
            &[CardWidth::Fraction(0.35), CardWidth::Fraction(0.35)],
            1_440.0,
        );
        let id = workspace.cards()[0].id;
        for visible in [720.0, 800.0, 900.0, 936.0, 1_000.0, 700.0] {
            let preferred = workspace.preferred_width_for_resize(id, visible);
            workspace
                .set_card_width(id, CardWidth::Fraction(preferred / 1_440.0))
                .unwrap();
            assert!((workspace.geometry().card(id).unwrap().width() - visible).abs() < 0.001);
            assert_eq!(workspace.cards()[1].width, CardWidth::Fraction(0.35));
        }
        workspace.resize_viewport(7_680.0);
        let rendered = workspace.geometry().card(id).unwrap().width();
        let limits = workspace.fractional_width_limits();
        assert!(rendered <= limits.maximum);
        let preferred = workspace.preferred_width_for_resize(id, rendered + 16.0);
        workspace
            .set_card_width(id, CardWidth::Fraction(preferred / 7_680.0))
            .unwrap();
        assert!((workspace.geometry().card(id).unwrap().width() - rendered - 16.0).abs() < 0.001);
    }

    #[test]
    fn resize_bounds_preserve_compact_and_full_width_gesture_origins() {
        for (fractions, viewport) in [([0.1, 0.2], 800.0), ([1.0, 0.5], 1_440.0)] {
            let mut workspace =
                workspace_with_widths(&fractions.map(CardWidth::Fraction), viewport);
            let id = workspace.cards()[0].id;
            let width = workspace.geometry().card(id).unwrap().width();
            let bounds = workspace.card_resize_limits(id);
            assert_eq!(width.clamp(bounds.minimum, bounds.maximum), width);
            let target = if width < 480.0 {
                width + 1.0
            } else {
                width - 1.0
            };
            let preferred = workspace
                .preferred_width_for_resize(id, target.clamp(bounds.minimum, bounds.maximum));
            workspace
                .set_card_width(id, CardWidth::Fraction(preferred / viewport))
                .unwrap();
            assert!((workspace.geometry().card(id).unwrap().width() - target).abs() < 0.001);
        }
    }

    #[test]
    fn compact_resize_can_backtrack_within_the_same_gesture() {
        let mut workspace =
            workspace_with_widths(&[CardWidth::Fraction(0.1), CardWidth::Fraction(0.2)], 800.0);
        let id = workspace.cards()[0].id;
        let origin = workspace.geometry().card(id).unwrap().width();
        for target in [300.0, 280.0, 500.0, 290.0, origin] {
            let bounds = workspace.begin_card_resize(id);
            assert_eq!(bounds.minimum, origin);
            let preferred = workspace
                .preferred_width_for_resize(id, target.clamp(bounds.minimum, bounds.maximum));
            workspace
                .set_card_width(id, CardWidth::Fraction(preferred / 800.0))
                .unwrap();
            assert!((workspace.geometry().card(id).unwrap().width() - target).abs() < 0.001);
        }
        workspace.finish_card_resize(id);
        assert!(workspace.resize_limits.is_none());
    }

    #[test]
    fn lone_pane_fills_every_viewport_without_losing_its_saved_width() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fraction(0.35)], 1_000.0);
        let first = workspace.focused().unwrap();
        for (width, expected) in [(960.0, 960.0), (1_440.0, 1_440.0), (800.0, 800.0)] {
            workspace.resize_viewport(width);
            let geometry = workspace.geometry();
            let card = geometry.card(first).unwrap();
            assert_eq!(card.start, workspace.config.leading_padding);
            assert!((card.width() - expected).abs() < 0.001);
            assert_eq!(geometry.max_offset(width), 0.0);
            assert_eq!(workspace.projection(width).mounted[0].start, card.start);
        }

        workspace.resize_viewport(1_440.0);
        workspace
            .add_card(card(CardWidth::Fraction(0.35)), 1, false)
            .unwrap();
        let geometry = workspace.geometry();
        assert_eq!(
            geometry.card(first).unwrap().start,
            workspace.config.leading_padding
        );
        assert!((geometry.card(first).unwrap().width() - 720.0).abs() < 0.001);
        assert_eq!(
            workspace.card(first).unwrap().width,
            CardWidth::Fraction(0.35)
        );
        assert_eq!(geometry.max_offset(workspace.viewport_width()), 0.0);
        assert_eq!(
            geometry.card_at(1).unwrap().start,
            geometry.card(first).unwrap().end + workspace.config.gap,
        );

        let second = geometry.card_at(1).unwrap().id;
        workspace.request_close(second).unwrap();
        workspace.commit_close(second).unwrap();
        assert!((workspace.geometry().card(first).unwrap().width() - 1_440.0).abs() < 0.001);

        workspace
            .set_card_width(first, CardWidth::Fraction(0.5))
            .unwrap();
        assert_eq!(workspace.geometry().card(first).unwrap().width(), 1_440.0);
        workspace.resize_viewport(800.0);
        assert_eq!(workspace.geometry().card(first).unwrap().width(), 800.0);
        workspace.resize_viewport(1_440.0);
        assert_eq!(workspace.geometry().card(first).unwrap().width(), 1_440.0);

        workspace.resize_viewport(959.0);
        let before = workspace.geometry().card(first).unwrap().width();
        workspace.resize_viewport(961.0);
        let after = workspace.geometry().card(first).unwrap().width();
        assert!((after - before).abs() < 3.0);

        let fixed = workspace_with_widths(&[CardWidth::Fixed(2_000.0)], 320.0);
        assert_eq!(fixed.geometry().card_at(0).unwrap().width(), 320.0);
        assert_eq!(fixed.geometry().max_offset(320.0), 0.0);

        let narrow = workspace_with_widths(&[CardWidth::Fraction(0.125)], 1_440.0);
        assert_eq!(narrow.geometry().card_at(0).unwrap().width(), 1_440.0);
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
            geometry.focus_offset(target.id, 700.0, 0.0),
            Some(target.start)
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
    fn stale_layout_scroll_preserves_focus_across_remounts_and_viewport_changes() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fraction(0.5); 7], 1_640.0);
        let focused = workspace.order().last().unwrap();
        workspace.focus(focused).unwrap();
        workspace.tick(Duration::from_secs(1));

        for width in [1_640.0, 800.0, 450.0, 900.0] {
            workspace.resize_viewport(width);
            let target = workspace.camera().target();
            // A remounted native scrollable starts at zero even though the
            // owner still holds the focused pane's camera position.
            workspace.native_layout_scroll(0.0, width);
            assert_eq!(workspace.camera().target(), target);
            workspace.tick(Duration::from_secs(1));
            let pane = workspace.geometry().card(focused).unwrap();
            let offset = workspace.camera().offset();
            assert!(pane.start >= offset - SCROLL_SYNC_EPSILON);
            assert!(pane.end <= offset + width + SCROLL_SYNC_EPSILON);
        }

        // Feedback can measure the new width before the resize message.
        workspace.native_layout_scroll(0.0, 600.0);
        let target = workspace.camera().target();
        workspace.resize_viewport(600.0);
        assert_eq!(workspace.camera().target(), target);
        assert!(target > 0.0);

        // An actual user scroll must still cancel the reveal immediately.
        workspace.native_scroll(0.0, 600.0);
        assert_eq!(workspace.camera().offset(), 0.0);
        assert_eq!(workspace.camera().target(), 0.0);
        assert!(!workspace.camera().is_animating());
    }

    #[test]
    fn viewport_reflow_keeps_fractional_panes_and_camera_in_step() {
        for layout_first in [false, true] {
            let mut workspace = workspace_with_widths(&[CardWidth::Fraction(1.0); 3], 800.0);
            let focused = workspace.order().nth(1).unwrap();
            workspace.focus(focused).unwrap();
            workspace.tick(Duration::from_secs(1));
            for width in [820.0, 880.0, 1_000.0, 1_180.0, 1_200.0, 1_100.0, 800.0] {
                if layout_first {
                    workspace.native_layout_scroll(workspace.camera().offset(), width);
                }
                workspace.resize_viewport(width);
                let pane = workspace.geometry().card(focused).unwrap();
                assert!((pane.start - workspace.camera().offset()).abs() < SCROLL_SYNC_EPSILON);
                assert!(
                    !workspace.camera().is_animating(),
                    "resize must not start a second animation"
                );
                let settled = workspace.camera().offset();
                workspace.tick(Duration::from_millis(16));
                assert_eq!(workspace.camera().offset(), settled);
            }
        }
    }

    #[test]
    fn viewport_reflow_anchors_settled_last_pane_despite_native_clamping() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fraction(1.0); 3], 800.0);
        let focused = workspace.order().last().unwrap();
        workspace.focus(focused).unwrap();
        workspace.tick(Duration::from_secs(1));
        let offset = workspace.camera().offset();
        workspace.record_programmatic_scroll(offset);
        let clamped = workspace.geometry().max_offset(900.0);
        workspace.native_layout_scroll(clamped, 900.0);
        assert_eq!(
            workspace.camera().offset(),
            workspace.geometry().card(focused).unwrap().start
        );
        assert!(!workspace.camera().is_animating());
    }

    #[test]
    fn viewport_reflow_keeps_fixed_pane_scroll_position() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(500.0); 5], 800.0);
        let focused = workspace.order().nth(2).unwrap();
        workspace.focus(focused).unwrap();
        workspace.tick(Duration::from_secs(1));
        let offset = workspace.camera().offset();
        workspace.resize_viewport(1_000.0);
        assert_eq!(workspace.camera().offset(), offset);
        assert!(!workspace.camera().is_animating());
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
    fn layout_feedback_before_first_frame_preserves_new_card_reveal() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fixed(500.0); 2], 600.0);
        workspace.native_scroll(100.0, 600.0);
        let added = card(CardWidth::Fixed(500.0));
        let added_id = added.id;
        workspace.add_card(added, 2, true).unwrap();
        let target = workspace.camera().target();
        assert!(target > workspace.camera().offset());

        workspace.native_scroll(workspace.camera().offset(), 600.0);
        workspace.resize_viewport(600.0);
        workspace.tick(Duration::from_millis(16));

        assert_eq!(workspace.focused(), Some(added_id));
        assert_eq!(workspace.camera().target(), target);
        assert!(workspace.camera().offset() > 100.0);
        assert!(workspace.camera().is_animating());
    }

    #[test]
    fn initial_scroll_feedback_retargets_restored_fractional_focus_for_measured_width() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fraction(0.5); 7], 1_340.0);
        let last = workspace.order().last().unwrap();
        workspace.focus(last).unwrap();
        let restored = workspace.camera().target();
        workspace.native_scroll(restored, 1_340.0);
        workspace.record_programmatic_scroll(restored);
        assert!(!workspace.is_animating());

        workspace.native_scroll(restored, 1_642.0);
        workspace.resize_viewport(1_642.0);
        let target = workspace
            .geometry()
            .focus_offset(last, 1_642.0, restored)
            .unwrap();
        assert!(target > restored);
        assert_eq!(workspace.camera().target(), target);
        assert!(workspace.is_animating());
        workspace.tick(Duration::from_millis(16));
        assert!(workspace.camera().offset() > restored);
    }

    #[test]
    fn initial_clamped_scroll_acknowledges_old_geometry_before_revealing_restored_focus() {
        let mut widths = [CardWidth::Fraction(0.5); 7];
        widths[4] = CardWidth::Fraction(2.0 / 3.0);
        let mut workspace = workspace_with_widths(&widths, 1_340.0);
        let last = workspace.order().last().unwrap();
        workspace.focus(last).unwrap();
        let restored = workspace.camera().target();
        assert!((restored - 3_573.333_5).abs() < SCROLL_SYNC_EPSILON);
        workspace.native_scroll(restored, 1_340.0);
        workspace.record_programmatic_scroll(restored);

        // Actual startup ordering: the native layout still has 1340px-based
        // pane widths, but its viewport is 1640px wide. The requested offset
        // is clamped before the model receives the measured width.
        let clamped = workspace.geometry().max_offset(1_640.0);
        assert!((clamped - 3_273.333_5).abs() < SCROLL_SYNC_EPSILON);
        workspace.native_scroll(clamped, 1_640.0);
        workspace.resize_viewport(1_640.0);
        let target = workspace
            .geometry()
            .focus_offset(last, 1_640.0, clamped)
            .unwrap();
        assert_eq!(workspace.camera().offset(), clamped);
        assert_eq!(workspace.camera().target(), target);
        assert!(target > restored);
        // A redraw can report the same native offset before the first frame.
        workspace.native_scroll(clamped, 1_640.0);
        assert_eq!(workspace.camera().target(), target);
        workspace.tick(Duration::from_millis(16));
        assert!(workspace.camera().offset() > clamped);

        // The acknowledgement was consumed, not a persistent permission to
        // disregard real user scrolling through the same coordinate later.
        workspace.native_scroll(clamped, 1_640.0);
        assert_eq!(workspace.camera().offset(), clamped);
        assert!(!workspace.is_animating());
    }

    #[test]
    fn width_change_does_not_disguise_unmatched_user_scroll_as_clamped_feedback() {
        let mut workspace = workspace_with_widths(&[CardWidth::Fraction(0.5); 7], 1_340.0);
        let last = workspace.order().last().unwrap();
        workspace.focus(last).unwrap();
        let restored = workspace.camera().target();
        workspace.native_scroll(restored, 1_340.0);
        workspace.record_programmatic_scroll(restored);

        workspace.native_scroll(1_000.0, 1_640.0);
        assert_eq!(workspace.camera().offset(), 1_000.0);
        assert_eq!(workspace.camera().target(), 1_000.0);
        assert!(!workspace.is_animating());
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
