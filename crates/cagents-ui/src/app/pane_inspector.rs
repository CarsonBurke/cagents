//! Each pane's own inspector, docked on the pane's trailing edge.
//!
//! A pane opens and closes its inspector independently of every other pane.
//! Closed, the pane keeps only the toggle, floating over its content's
//! top-right corner, so it takes no row of its own. Open, the inspector docks
//! beside the pane's content, unless the pane is too narrow for both: such a
//! pane closes its inspector by itself, and one reopened there covers the
//! pane's content instead of squeezing it. A pane closed only for being too
//! narrow remembers that its inspector was open, so the inspector comes back
//! when the pane widens and a restart in a smaller window loses nothing.
//! Until then, the open inspector collapses to a rail along the pane's edge
//! that keeps its agent's status and work in view, and expands over the
//! pane's content when pressed; Escape collapses it again.
//!
//! The window binds source control and files to one worktree at a time, so
//! only one open inspector draws them: the focused pane's, or, while the
//! focused pane's inspector is closed, the last one that drew them. Every
//! other open inspector counts its own changes and offers to show them.
//!
//! Only a pane whose work belongs to a worktree has an inspector; see
//! [`offers_inspector`].

use super::*;

/// The narrowest a pane's content may become beside its docked inspector.
/// A pane narrower than this and the inspector together closes it.
pub(super) const PANE_CONTENT_MIN_WIDTH: f32 = 360.0;

/// The gap between the floating toggle and the corner of the pane's content.
pub(super) const TOGGLE_INSET: f32 = space::XS;

/// Content stops a standard gap before the floating toggle's leading edge.
pub(super) const TOGGLE_CLEARANCE: f32 = TOGGLE_INSET + design::PRIMARY_CONTROL_SIZE + space::S;

/// Whether a pane of `kind` can have an inspector.
///
/// The inspector shows a pane's work: its conversation's pull requests, the
/// worktree's changes and files, an editor's language-server results. A
/// terminal, chat or editor names its worktree, and a pull request pane its
/// repository. A browser names none: it shows the worktree a worktree's or
/// channel's workspace is scoped to, so it has an inspector only there; see
/// [`Cagents::inspector_availability`]. An application page - Settings,
/// Accounts, Automations, the Work board of every worktree - belongs to no
/// worktree, so its inspector could only show whichever worktree the window
/// last bound, which has nothing to do with the page.
pub(super) fn offers_inspector(kind: &CardKind) -> bool {
    match kind {
        CardKind::Terminal(_)
        | CardKind::Chat(_)
        | CardKind::Browser(_)
        | CardKind::PullRequests(_)
        | CardKind::Repository(_) => true,
        CardKind::Settings | CardKind::Accounts | CardKind::Automations | CardKind::Work => false,
    }
}

/// Whether a pane has an inspector where the window shows it now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Availability {
    /// The pane's kind has no inspector, so none is ever opened or restored.
    Never,
    /// The pane has an inspector, but nothing to show in it here, as a
    /// browser in the unscoped home strip: an inspector left open waits,
    /// hidden, for the pane to be somewhere it has something to show.
    Unavailable,
    Available,
}

/// One pane's inspector.
#[derive(Debug, Clone, Copy, Default)]
struct PaneState {
    /// Whether the inspector is open: as the user left it, or, until they
    /// open or close it, as the default says.
    open: bool,
    /// Whether the user opened or closed it, so `open` is their choice and
    /// the default no longer applies. This is what is saved.
    chosen: bool,
    /// Open, but closed for now because the pane is too narrow for it.
    narrowed: bool,
    /// Open, but hidden for now because it has nothing to show here.
    unavailable: bool,
    transition: Option<InspectorTransition>,
    /// Opened while the pane was too narrow for it, so it covers the pane's
    /// content, and narrowness does not close it again.
    opened_narrow: bool,
    /// Over the content because its rail was pressed, so Escape collapses
    /// it to the rail again rather than closing it.
    expanded: bool,
}

impl PaneState {
    /// Whether the inspector is open on screen.
    fn shown(&self) -> bool {
        self.open && !self.narrowed && !self.unavailable
    }

    /// Whether the pane docks the inspector's rail: open, in a pane too
    /// narrow to dock it, whether collapsed or expanded over the content.
    fn railed(&self) -> bool {
        self.open && !self.unavailable && (self.narrowed || self.opened_narrow)
    }
}

/// The panes whose inspectors the user opened or closed, as a window view
/// saves them. Every other pane follows the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(super) struct Choices {
    pub(super) opened: Vec<CardId>,
    pub(super) closed: Vec<CardId>,
}

/// Every pane's inspector, and which of them draws source control.
#[derive(Debug, Default)]
pub(super) struct PaneInspectors {
    panes: HashMap<CardId, PaneState>,
    /// What the user chose for panes this window has not seen yet: those
    /// saved, as a workspace restored after the window view brings them, and
    /// copies of panes whose inspectors the user opened or closed.
    restored: HashMap<CardId, bool>,
    /// Whether a pane the user has not opened or closed an inspector in has
    /// it open.
    open_by_default: bool,
    /// A window saved before inspectors were per pane, with its one
    /// inspector open: the first pane focused opens it.
    legacy_open: bool,
    /// The pane whose inspector draws the window's source control and files.
    source_control: Option<CardId>,
    /// Every pane that may be narrowed, hidden or opened narrow: at least
    /// each one with any of those set. Only these, the strip's panes and the
    /// panes whose kind is an exception can change those flags, so an
    /// incremental sync looks at them alone.
    constrained: HashSet<CardId>,
    /// The window's cards, in order, as the last incremental sync admitted
    /// them.
    known: Vec<CardId>,
    /// The cards whose inspectors are not simply available, as the last
    /// incremental sync found them: the few application pages and browsers.
    exceptions: Vec<(CardId, Availability)>,
    /// What the last incremental sync caught up with, until the inspectors
    /// themselves change.
    synced: Option<Synced>,
}

/// What an incremental sync of the inspectors depends on besides the durable
/// cards. Equal inputs over the same durable cards leave every pane's state
/// as it was; only the pane drawing source control is chosen again, from the
/// panes on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SyncInputs {
    pub(super) focused: Option<CardId>,
    /// Whether a browser has a worktree to show: the window is scoped.
    pub(super) browsers_available: bool,
    /// The strip's geometry, which says how wide each pane is.
    pub(super) geometry: u64,
}

#[derive(Debug)]
struct Synced {
    /// The durable workspace the cards came from. Copy-on-write gives every
    /// changed workspace a new address: while this weak reference lives,
    /// `Arc::make_mut` moves the value rather than changing it in place, and
    /// the old allocation cannot be reused. A change made through
    /// `Arc::get_mut` would fail while it lives instead.
    cards: std::sync::Weak<WorkspaceState>,
    inputs: SyncInputs,
}

impl PaneInspectors {
    /// Every pane's inspector state, in a stable order, for the projection
    /// check.
    #[cfg(any(test, feature = "development"))]
    pub(super) fn projection_view(&self) -> String {
        format!(
            "panes {} restored {} default {} legacy {} source control {:?}",
            super::projection_oracle::sorted_entries(&self.panes),
            super::projection_oracle::sorted_entries(&self.restored),
            self.open_by_default,
            self.legacy_open,
            self.source_control,
        )
    }

    /// What a window view saved before it kept [`Choices`]: the panes whose
    /// inspectors the user opened, or, from a window saved before they were
    /// per pane, whether its one inspector was. Inspectors were closed until
    /// opened then, so a pane missing from `open` was never closed by choice.
    pub(super) fn restore(open: Option<Vec<CardId>>, legacy_open: bool) -> Self {
        match open {
            Some(open) => Self {
                restored: open.into_iter().map(|card| (card, true)).collect(),
                ..Self::default()
            },
            None => Self {
                legacy_open,
                ..Self::default()
            },
        }
    }

    /// The inspectors the user opened or closed, as a window view saved them.
    pub(super) fn restore_choices(choices: Choices) -> Self {
        let closed = choices.closed.into_iter().map(|card| (card, false));
        Self {
            // A pane in both lists, which `choices` never writes, counts
            // as opened.
            restored: closed
                .chain(choices.opened.into_iter().map(|card| (card, true)))
                .collect(),
            ..Self::default()
        }
    }

    /// Opens the inspector of every pane the user has not opened or closed
    /// one in, or keeps them closed.
    pub(super) fn with_default(mut self, open: bool) -> Self {
        self.open_by_default = open;
        self
    }

    /// Opens or closes, at once, the inspector of every pane the user has not
    /// opened or closed one in, as `open` says, except in the panes
    /// `availability` says never have one.
    pub(super) fn set_open_by_default(
        &mut self,
        open: bool,
        availability: impl Fn(CardId) -> Availability,
    ) {
        if self.open_by_default == open {
            return;
        }
        self.open_by_default = open;
        for (card, pane) in &mut self.panes {
            if pane.chosen || availability(*card) == Availability::Never {
                continue;
            }
            *pane = PaneState {
                open,
                ..PaneState::default()
            };
        }
        self.synced = None;
    }

    /// The panes to save as open, in a stable order, among the cards `known`
    /// says still exist and can have an inspector: what a version that saves
    /// no [`Choices`] reads back. Only those the user opened: that version
    /// reads every pane listed as opened by choice.
    pub(super) fn saved(&self, known: impl Fn(CardId) -> bool) -> Vec<CardId> {
        let mut open: Vec<CardId> = self
            .panes
            .iter()
            .filter(|(_, pane)| pane.open && pane.chosen)
            .map(|(card, _)| *card)
            .chain(
                self.restored
                    .iter()
                    .filter(|(_, open)| **open)
                    .map(|(card, _)| *card),
            )
            .filter(|card| known(*card))
            .collect();
        open.sort_unstable();
        open.dedup();
        open
    }

    /// The inspectors the user opened or closed, in a stable order, among the
    /// cards `known` says still exist and can have an inspector.
    pub(super) fn choices(&self, known: impl Fn(CardId) -> bool) -> Choices {
        let mut choices = Choices::default();
        let chosen = self
            .panes
            .iter()
            .filter(|(_, pane)| pane.chosen)
            .map(|(card, pane)| (*card, pane.open))
            .chain(self.restored.iter().map(|(card, open)| (*card, *open)));
        for (card, open) in chosen {
            if known(card) {
                if open {
                    choices.opened.push(card);
                } else {
                    choices.closed.push(card);
                }
            }
        }
        for list in [&mut choices.opened, &mut choices.closed] {
            list.sort_unstable();
            list.dedup();
        }
        choices
    }

    /// Starts `copy`, a pane split from `source`, with `source`'s inspector
    /// as the user left it, or, if they never opened or closed it, as the
    /// default says.
    pub(super) fn inherit(&mut self, source: CardId, copy: CardId) {
        let choice = match self.panes.get(&source) {
            Some(pane) => pane.chosen.then_some(pane.open),
            None => self.restored.get(&source).copied(),
        };
        if let Some(open) = choice {
            self.restored.insert(copy, open);
            self.synced = None;
        }
    }

    /// Whether the pane's inspector is open on screen.
    pub(super) fn is_open(&self, card: CardId) -> bool {
        self.panes.get(&card).is_some_and(PaneState::shown)
    }

    /// Whether the pane draws its inspector: open, or closing.
    pub(super) fn is_visible(&self, card: CardId) -> bool {
        self.panes
            .get(&card)
            .is_some_and(|pane| pane.shown() || pane.transition.is_some())
    }

    #[cfg(test)]
    pub(super) fn visible(&self) -> impl Iterator<Item = CardId> + '_ {
        self.panes
            .iter()
            .filter(|(_, pane)| pane.shown() || pane.transition.is_some())
            .map(|(card, _)| *card)
    }

    /// Whether the pane docks its open inspector's rail; see
    /// [`PaneState::railed`]. Read from the state the last sync left, so a
    /// frame never measures a pane to draw it.
    pub(super) fn is_railed(&self, card: CardId) -> bool {
        self.panes.get(&card).is_some_and(PaneState::railed)
    }

    /// The panes whose inspectors show their work: drawn, or collapsed to a
    /// rail that counts it.
    pub(super) fn watching(&self) -> impl Iterator<Item = CardId> + '_ {
        self.panes
            .iter()
            .filter(|(_, pane)| pane.shown() || pane.transition.is_some() || pane.railed())
            .map(|(card, _)| *card)
    }

    /// Whether the pane's inspector covers its content because its rail
    /// was pressed, rather than because it was opened there.
    pub(super) fn is_expanded(&self, card: CardId) -> bool {
        self.panes.get(&card).is_some_and(|pane| pane.expanded)
    }

    pub(super) fn opened_narrow(&self, card: CardId) -> bool {
        self.panes.get(&card).is_some_and(|pane| pane.opened_narrow)
    }

    pub(super) fn is_animating(&self) -> bool {
        self.panes.values().any(|pane| pane.transition.is_some())
    }

    /// How far open the pane's inspector is drawn at `frame`.
    pub(super) fn progress(&self, card: CardId, frame: Option<Instant>) -> f32 {
        let Some(pane) = self.panes.get(&card) else {
            return 0.0;
        };
        pane.transition
            .map_or(if pane.shown() { 1.0 } else { 0.0 }, |transition| {
                transition.progress(frame.unwrap_or(transition.started_at))
            })
    }

    /// Opens or closes the pane's inspector, sliding from where it is drawn
    /// now. Returns whether anything changed.
    pub(super) fn set_open(
        &mut self,
        card: CardId,
        open: bool,
        narrow: bool,
        frame: Option<Instant>,
        now: Instant,
    ) -> bool {
        let from = self.progress(card, frame);
        let pane = self.panes.entry(card).or_default();
        self.restored.remove(&card);
        if pane.shown() == open {
            return false;
        }
        pane.open = open;
        pane.chosen = true;
        pane.narrowed = false;
        pane.expanded = false;
        if open {
            pane.opened_narrow = narrow;
        }
        self.constrained.insert(card);
        self.synced = None;
        // Closing keeps its mounting mode until the panel has slid away.
        pane.transition = Some(InspectorTransition {
            started_at: now,
            from,
            to: if open { 1.0 } else { 0.0 },
        });
        true
    }

    /// Expands a pane's collapsed rail over its content, sliding from where
    /// it is drawn now. The pane's open or closed choice stays as it was, so
    /// widening the pane docks the inspector as before. Returns whether
    /// anything changed.
    pub(super) fn expand(&mut self, card: CardId, frame: Option<Instant>, now: Instant) -> bool {
        let from = self.progress(card, frame);
        let Some(pane) = self.panes.get_mut(&card) else {
            return false;
        };
        if !pane.railed() || pane.shown() {
            return false;
        }
        pane.narrowed = false;
        pane.opened_narrow = true;
        pane.expanded = true;
        pane.transition = Some(InspectorTransition {
            started_at: now,
            from,
            to: 1.0,
        });
        self.constrained.insert(card);
        self.synced = None;
        true
    }

    /// Collapses an inspector its rail expanded over a narrow pane's
    /// content back to the rail, sliding from where it is drawn now. It
    /// stays open, as it was before the rail was pressed; one opened over
    /// the content from a closed pane is not collapsed, as Escape closes it
    /// again instead. Returns whether anything changed.
    pub(super) fn collapse(&mut self, card: CardId, frame: Option<Instant>, now: Instant) -> bool {
        let from = self.progress(card, frame);
        let Some(pane) = self.panes.get_mut(&card) else {
            return false;
        };
        if !pane.shown() || !pane.opened_narrow || !pane.expanded {
            return false;
        }
        // Covering the content until the slide ends, as a closing overlay
        // does; `advance` then lets it dock again once the pane widens.
        pane.narrowed = true;
        pane.transition = Some(InspectorTransition {
            started_at: now,
            from,
            to: 0.0,
        });
        self.constrained.insert(card);
        self.synced = None;
        true
    }

    /// Ends every slide that is complete at `now`; returns whether any did.
    pub(super) fn advance(&mut self, now: Instant) -> bool {
        let mut finished = false;
        for pane in self.panes.values_mut() {
            if pane
                .transition
                .is_some_and(|transition| transition.is_complete(now))
            {
                pane.transition = None;
                if !pane.open || pane.narrowed {
                    pane.opened_narrow = false;
                    pane.expanded = false;
                }
                finished = true;
            }
        }
        // Only the slides changed, and the pane drawing source control is
        // chosen on every sync, so the sync's inputs still hold.
        finished
    }

    pub(super) fn source_control(&self) -> Option<CardId> {
        self.source_control
    }

    /// Catches the inspectors up with the workspace: a pane seen for the
    /// first time opens as it was saved, or as the pane it was split from
    /// was; an open pane `narrow` says is too narrow closes until it widens,
    /// unless it was opened there; and source control follows the focused
    /// pane's open inspector,
    /// or stays with the one that last drew it, else goes to the first
    /// open one on screen. `cards` is every pane in the window, `focused`
    /// the focused one, `availability` says which have an inspector here,
    /// and `on_screen` says which the strip shows.
    ///
    /// A pane whose kind has no inspector restores none: whatever was saved
    /// open for it is dropped, and a legacy window focused on one forgets its
    /// one inspector rather than opening it on some later pane. An open
    /// inspector with nothing to show here is hidden at once, and comes back
    /// when it has.
    ///
    /// This visits every pane. [`Self::sync_incremental`] reaches the same
    /// state visiting only what its inputs say may have changed.
    #[cfg(test)]
    pub(super) fn sync(
        &mut self,
        cards: &[CardId],
        focused: Option<CardId>,
        availability: impl Fn(CardId) -> Availability,
        narrow: impl Fn(CardId) -> bool,
        on_screen: impl Fn(CardId) -> bool,
    ) {
        self.synced = None;
        self.admit(cards, focused, &availability);
        let panes: Vec<CardId> = self.panes.keys().copied().collect();
        self.constrain(panes, &availability, &narrow);
        self.forget_closed(cards);
        self.follow_focus(focused, &on_screen, |shown| {
            cards.iter().copied().find(|card| shown(*card))
        });
    }

    /// [`Self::sync`] for a window whose durable workspace is `source`,
    /// visiting only what may have changed since the last call.
    ///
    /// `cards` lists the window's cards and what each has, and is read only
    /// when `source` or whether browsers are available has changed. Every
    /// other input is in `inputs`: while they and `source` hold, no pane's
    /// state can change, and only the pane drawing source control is chosen
    /// again. When they change, only the panes `strip` lists, the
    /// exceptions among `cards` and the panes already constrained can be
    /// narrowed or hidden: a pane outside the strip has no width. `mounted`
    /// lists at least the panes `on_screen` accepts, and `index` gives a
    /// card's place among `cards`.
    pub(super) fn sync_incremental<I: IntoIterator<Item = CardId>>(
        &mut self,
        source: &Arc<WorkspaceState>,
        inputs: SyncInputs,
        cards: impl FnOnce() -> Vec<(CardId, Availability)>,
        strip: impl FnOnce() -> I,
        narrow: impl Fn(CardId) -> bool,
        mounted: impl Iterator<Item = CardId>,
        on_screen: impl Fn(CardId) -> bool,
        index: impl Fn(CardId) -> Option<usize>,
    ) {
        let synced = self.synced.as_ref();
        let cards_changed = synced.is_none_or(|synced| {
            synced.cards.as_ptr() != Arc::as_ptr(source)
                || synced.inputs.browsers_available != inputs.browsers_available
        });
        let changed = cards_changed || synced.is_none_or(|synced| synced.inputs != inputs);
        if changed {
            let mut admitted = Vec::new();
            if cards_changed {
                let cards = cards();
                self.exceptions.clear();
                self.exceptions.extend(
                    cards
                        .iter()
                        .copied()
                        .filter(|(_, availability)| *availability != Availability::Available),
                );
                if !self
                    .known
                    .iter()
                    .copied()
                    .eq(cards.iter().map(|(card, _)| *card))
                {
                    self.known.clear();
                    self.known.extend(cards.iter().map(|(card, _)| *card));
                    admitted.clone_from(&self.known);
                }
            }
            let exceptions = std::mem::take(&mut self.exceptions);
            let availability = |card: CardId| {
                exceptions
                    .iter()
                    .find(|(id, _)| *id == card)
                    .map_or(Availability::Available, |(_, availability)| *availability)
            };
            // Unchanged cards hold no pane this has not seen, so only the
            // legacy inspector's focus is settled.
            self.admit(&admitted, inputs.focused, &availability);
            let candidates: Vec<CardId> = strip()
                .into_iter()
                .chain(exceptions.iter().map(|(card, _)| *card))
                .chain(self.constrained.iter().copied())
                .collect();
            self.constrain(candidates, &availability, &narrow);
            if cards_changed {
                let known = std::mem::take(&mut self.known);
                self.forget_closed(&known);
                self.known = known;
            }
            self.exceptions = exceptions;
            self.synced = Some(Synced {
                cards: Arc::downgrade(source),
                inputs,
            });
        }
        // The panes on screen are few, and the first open one among the
        // window's cards is the one with the lowest place there.
        self.follow_focus(inputs.focused, &on_screen, |shown| {
            mounted
                .filter(|card| shown(*card))
                .filter_map(|card| Some((index(card)?, card)))
                .min()
                .map(|(_, card)| card)
        });
    }

    /// Opens each of `cards` this has not seen as it was saved, and settles
    /// a legacy window's one inspector on the focused pane.
    fn admit(
        &mut self,
        cards: &[CardId],
        focused: Option<CardId>,
        availability: &dyn Fn(CardId) -> Availability,
    ) {
        if self.legacy_open && focused.is_some_and(|card| availability(card) == Availability::Never)
        {
            self.legacy_open = false;
        }
        for card in cards {
            if self.panes.contains_key(card) {
                continue;
            }
            let legacy = self.legacy_open && Some(*card) == focused;
            if legacy {
                self.legacy_open = false;
            }
            let choice = self
                .restored
                .remove(card)
                .or(legacy.then_some(true))
                .filter(|_| availability(*card) != Availability::Never);
            self.panes.insert(
                *card,
                PaneState {
                    open: choice.unwrap_or(
                        self.open_by_default && availability(*card) != Availability::Never,
                    ),
                    chosen: choice.is_some(),
                    ..PaneState::default()
                },
            );
        }
        // A legacy window whose focused pane was already known.
        if self.legacy_open
            && let Some(focused) = focused
            && let Some(pane) = self.panes.get_mut(&focused)
        {
            pane.open = true;
            pane.chosen = true;
            self.legacy_open = false;
        }
    }

    /// Hides each open pane among `candidates` that has nothing to show here,
    /// and closes, until it widens, each that `narrow` says is too narrow,
    /// unless it was opened there. Only open inspectors care whether they
    /// have anything to show and how wide their panes are.
    fn constrain(
        &mut self,
        candidates: impl IntoIterator<Item = CardId>,
        availability: &dyn Fn(CardId) -> Availability,
        narrow: &dyn Fn(CardId) -> bool,
    ) {
        for card in candidates {
            let Some(pane) = self.panes.get_mut(&card) else {
                self.constrained.remove(&card);
                continue;
            };
            if !pane.open {
                continue;
            }
            let unavailable = availability(card) == Availability::Unavailable;
            if unavailable && !pane.unavailable {
                // At once, without a slide.
                pane.transition = None;
            }
            pane.unavailable = unavailable;
            if !narrow(card) {
                pane.narrowed = false;
                pane.opened_narrow = false;
                pane.expanded = false;
                // A rail collapsing as the pane widens docks the inspector
                // at once rather than sliding it shut and back open.
                if pane
                    .transition
                    .is_some_and(|transition| transition.to < 1.0)
                {
                    pane.transition = None;
                }
            } else if !pane.opened_narrow && !pane.narrowed {
                // At once, without a slide.
                pane.narrowed = true;
                pane.transition = None;
            }
            if pane.unavailable || pane.narrowed || pane.opened_narrow {
                self.constrained.insert(card);
            } else {
                self.constrained.remove(&card);
            }
        }
    }

    /// Drops the panes closed for good, here or in another window. Every
    /// pane in `cards` is known by now, so any more are gone.
    fn forget_closed(&mut self, cards: &[CardId]) {
        if !cards.is_empty() && self.panes.len() > cards.len() {
            let present: HashSet<CardId> = cards.iter().copied().collect();
            self.panes.retain(|card, _| present.contains(card));
            self.constrained.retain(|card| present.contains(card));
        }
    }

    /// Source control follows the focused pane's open inspector, or stays
    /// with the one that last drew it while that is on screen, else goes to
    /// the open inspector on screen that `first` finds among those `shown`.
    fn follow_focus(
        &mut self,
        focused: Option<CardId>,
        on_screen: &dyn Fn(CardId) -> bool,
        first: impl FnOnce(&dyn Fn(CardId) -> bool) -> Option<CardId>,
    ) {
        self.source_control = match focused {
            Some(focused) if self.is_visible(focused) => Some(focused),
            _ => {
                let shown = |card: CardId| self.is_visible(card) && on_screen(card);
                self.source_control
                    .filter(|card| shown(*card))
                    .or_else(|| first(&shown))
            }
        };
    }
}

impl Cagents {
    /// The width a pane gives its inspector: the one width every inspector
    /// shares when docked, narrowed to leave room for content. An overlay
    /// instead occupies the whole pane, including through its closing slide.
    pub(super) fn pane_inspector_width(&self, card: CardId) -> f32 {
        if self.pane_inspector_layout(card) == Some(InspectorLayout::Overlay)
            && let Some(width) = pane_width(&self.workspace, card)
        {
            return width;
        }
        let width = self
            .settings
            .inspector_width
            .clamp(design::INSPECTOR_MIN_WIDTH, design::INSPECTOR_MAX_WIDTH);
        pane_width(&self.workspace, card).map_or(width, |pane| {
            width
                .min(pane - PANE_CONTENT_MIN_WIDTH)
                .max(design::INSPECTOR_MIN_WIDTH)
                .min(pane)
        })
    }

    /// Whether the pane is too narrow for its content and a docked
    /// inspector together.
    pub(super) fn pane_too_narrow(&self, card: CardId) -> bool {
        pane_too_narrow(&self.workspace, card)
    }

    /// How the pane draws its open inspector: beside its content, or, in a
    /// pane too narrow for both, over it.
    pub(super) fn pane_inspector_layout(&self, card: CardId) -> Option<InspectorLayout> {
        if !self.pane_inspectors.is_visible(card) {
            return None;
        }
        Some(
            if self.pane_inspectors.opened_narrow(card) && self.pane_too_narrow(card) {
                InspectorLayout::Overlay
            } else {
                InspectorLayout::Docked
            },
        )
    }

    pub(super) fn pane_inspector_progress(&self, card: CardId) -> f32 {
        self.pane_inspectors
            .progress(card, self.frame_clock.frame())
    }

    /// The focused pane, whose inspector the keyboard shortcut and the
    /// commands that show their results there open, if it has one.
    pub(super) fn focused_inspector_pane(&self) -> Option<CardId> {
        self.workspace
            .focused()
            .filter(|card| self.pane_has_inspector(*card))
    }

    /// Whether the pane has an inspector where the window shows it now.
    pub(super) fn inspector_availability(&self, card: CardId) -> Availability {
        self.card_state(card).map_or(Availability::Never, |card| {
            self.kind_availability(&card.kind)
        })
    }

    fn kind_availability(&self, kind: &CardKind) -> Availability {
        kind_availability(kind, self.active_workspace.is_some())
    }

    /// Whether the pane has an inspector to open and show now.
    pub(super) fn pane_has_inspector(&self, card: CardId) -> bool {
        self.inspector_availability(card) == Availability::Available
    }

    /// The panes whose inspectors a window view saves as open: any the user
    /// left open, including one hidden for now, but never an application
    /// page's.
    pub(super) fn saved_open_inspectors(&self) -> Vec<CardId> {
        self.pane_inspectors.saved(|card| {
            self.card_state(card)
                .is_some_and(|card| offers_inspector(&card.kind))
        })
    }

    /// The inspectors the user opened or closed, for a window view to save.
    pub(super) fn saved_inspector_choices(&self) -> Choices {
        self.pane_inspectors.choices(|card| {
            self.card_state(card)
                .is_some_and(|card| offers_inspector(&card.kind))
        })
    }

    /// Opens or closes the pane's inspector. A pane without one never opens
    /// it.
    pub(super) fn set_pane_inspector_open(&mut self, card: CardId, open: bool) -> Task<Message> {
        if open && !self.pane_has_inspector(card) {
            return Task::none();
        }
        let closing_overlay =
            !open && self.pane_inspector_layout(card) == Some(InspectorLayout::Overlay);
        let narrow = self.pane_too_narrow(card);
        if !self.pane_inspectors.set_open(
            card,
            open,
            narrow,
            self.frame_clock.frame(),
            Instant::now(),
        ) {
            return Task::none();
        }
        self.frame_clock.reset();
        self.sync_pane_inspectors();
        let binding = self.rebind_source_control();
        // An inspector opened over the pane's content takes the keyboard
        // from it, so typing never lands in content it hides.
        let focus = if closing_overlay
            || (open && self.pane_inspector_layout(card) == Some(InspectorLayout::Overlay))
        {
            iced::widget::operation::focus(self.pane_inspector_toggle_id(card))
                .map(cosmic::Action::App)
        } else {
            Task::none()
        };
        Task::batch([binding, self.save_settings_task(false), focus])
    }

    /// Opens or closes the pane's inspector. A rail is an open inspector
    /// collapsed for want of room, so toggling it shows the inspector over
    /// the pane rather than closing it.
    pub(super) fn toggle_pane_inspector(&mut self, card: CardId) -> Task<Message> {
        if self.pane_inspectors.is_railed(card) && !self.pane_inspectors.is_open(card) {
            return self.expand_pane_inspector(card);
        }
        let open = !self.pane_inspectors.is_open(card);
        self.set_pane_inspector_open(card, open)
    }

    /// Expands the pane's rail over its content, without changing whether
    /// the pane keeps its inspector open, and gives the inspector the
    /// keyboard, as one opened over the content takes it.
    pub(super) fn expand_pane_inspector(&mut self, card: CardId) -> Task<Message> {
        if !self.pane_has_inspector(card)
            || !self
                .pane_inspectors
                .expand(card, self.frame_clock.frame(), Instant::now())
        {
            return Task::none();
        }
        self.frame_clock.reset();
        self.sync_pane_inspectors();
        let binding = self.rebind_source_control();
        let focus = iced::widget::operation::focus(self.pane_inspector_toggle_id(card))
            .map(cosmic::Action::App);
        Task::batch([binding, focus])
    }

    /// Returns the inspector covering a narrow pane's content to what it
    /// was before it covered it: one its rail expanded collapses to the
    /// rail, which takes the keyboard back and keeps the inspector open, so
    /// widening the pane docks it again; one opened there from a closed
    /// pane closes again. Escape never leaves a closed inspector open.
    pub(super) fn close_pane_inspector_overlay(&mut self, card: CardId) -> Task<Message> {
        if !self.pane_inspectors.is_open(card)
            || self.pane_inspector_layout(card) != Some(InspectorLayout::Overlay)
        {
            return Task::none();
        }
        // Escape belongs to the inspector whose descendants hold native
        // focus, even if the pointer last activated another pane.
        let update = self
            .workspace
            .focus_without_scrolling(card)
            .unwrap_or_else(|_| ViewportUpdate::unchanged());
        self.workspace.take_widget_focus_request();
        let viewport = self.apply_viewport(update, false, DurableChange::None);
        if !self.pane_inspectors.is_expanded(card) {
            return Task::batch([viewport, self.set_pane_inspector_open(card, false)]);
        }
        if !self
            .pane_inspectors
            .collapse(card, self.frame_clock.frame(), Instant::now())
        {
            return viewport;
        }
        self.frame_clock.reset();
        self.sync_pane_inspectors();
        // The rail beneath takes the keyboard once the panel has slid away,
        // as the toggle does after a close.
        Task::batch([viewport, self.rebind_source_control()])
    }

    pub(super) fn advance_pane_inspector_transitions(&mut self, now: Instant) -> Task<Message> {
        // The floating toggle, or the rail of an inspector that stays open,
        // replaces the outgoing panel's toggle once the slide ends. Focus
        // that control, unless the user has moved to another pane or opened
        // a modal in the meantime.
        // One that collapses to its rail is no longer shown either.
        let closing_overlay = self.workspace.focused().filter(|card| {
            !self.modal_is_open()
                && !self.pane_inspectors.is_open(*card)
                && self.pane_inspector_layout(*card) == Some(InspectorLayout::Overlay)
        });
        if !self.pane_inspectors.advance(now) {
            return Task::none();
        }
        self.sync_pane_inspectors();
        // A frame skips the rebind, and a closed inspector no longer pins
        // the panels to the work it showed.
        let focus = closing_overlay
            .filter(|card| self.pane_inspector_layout(*card).is_none())
            .map_or_else(Task::none, |card| {
                let id = if self.pane_inspectors.is_railed(card) {
                    self.pane_inspector_rail_id(card)
                } else {
                    self.pane_inspector_toggle_id(card)
                };
                workspace_widget::focus_if_unheld(id).map(cosmic::Action::App)
            });
        Task::batch([self.rebind_source_control(), focus])
    }

    /// Runs at the tail of every update, before the source-control rebind;
    /// see [`PaneInspectors::sync`].
    /// Opens or closes every inspector the user has not opened or closed,
    /// as the open-by-default setting now says.
    pub(super) fn set_inspectors_open_by_default(&mut self, open: bool) {
        let browsers_available = self.active_workspace.is_some();
        let durable = &self.durable;
        let card_indices = &self.card_indices;
        self.pane_inspectors.set_open_by_default(open, |card| {
            card_indices
                .get(&card)
                .and_then(|index| durable.cards.get(*index))
                .filter(|state| state.id == card)
                .or_else(|| durable.cards.iter().find(|state| state.id == card))
                .map_or(Availability::Never, |state| {
                    kind_availability(&state.kind, browsers_available)
                })
        });
    }

    pub(super) fn sync_pane_inspectors(&mut self) {
        let browsers_available = self.active_workspace.is_some();
        let inputs = SyncInputs {
            focused: self.workspace.focused(),
            browsers_available,
            geometry: self.workspace.geometry_revision(),
        };
        let durable = &self.durable;
        let workspace = &self.workspace;
        let card_indices = &self.card_indices;
        self.pane_inspectors.sync_incremental(
            durable,
            inputs,
            || {
                durable
                    .cards
                    .iter()
                    .map(|card| (card.id, kind_availability(&card.kind, browsers_available)))
                    .collect()
            },
            || workspace.order(),
            |card| pane_too_narrow(workspace, card),
            workspace.mounted_visibility().map(|(card, _)| card),
            |card| {
                workspace
                    .visibility(card)
                    .is_some_and(crate::workspace::VisibilityClass::is_mounted)
            },
            |card| {
                // The index is rebuilt as the projection begins; a stale
                // entry falls back to a scan, as `card_index` does.
                card_indices
                    .get(&card)
                    .copied()
                    .filter(|index| durable.cards.get(*index).is_some_and(|c| c.id == card))
                    .or_else(|| durable.cards.iter().position(|c| c.id == card))
            },
        );
    }

    /// Whether this pane's inspector draws the window's source control and
    /// files.
    pub(super) fn pane_shows_source_control(&self, card: CardId) -> bool {
        self.pane_inspectors.source_control() == Some(card)
    }

    fn pane_inspector_toggle_id(&self, card: CardId) -> widget::Id {
        self.card_chrome.get(&card).map_or_else(
            || card_control_id(card, "inspector.toggle"),
            |chrome| chrome.inspector_toggle_id.clone(),
        )
    }

    pub(super) fn pane_inspector_rail_id(&self, card: CardId) -> widget::Id {
        self.card_chrome.get(&card).map_or_else(
            || card_control_id(card, "inspector.rail"),
            |chrome| chrome.inspector_rail_id.clone(),
        )
    }

    /// The toggle of the pane's inspector. It is mounted once per pane, so
    /// it keeps its identity, its focus and its tooltip whether it floats
    /// over the pane's content or sits in the open inspector.
    pub(super) fn pane_inspector_toggle(&self, card: CardId, open: bool) -> Element<'_, Message> {
        let label = workspace_widget::shortcut_tooltip(
            workspace_widget::GlobalShortcut::ToggleInspector,
            &self.settings.keyboard_shortcuts,
        );
        let id = self.pane_inspector_toggle_id(card);
        let mut toggle = crate::ui_controls::icon_button(Icon::SplitPanel, label)
            .selected(open)
            .on_press_maybe((!self.modal_is_open()).then_some(Message::TogglePaneInspector(card)))
            .id(id);
        if open {
            toggle = toggle.tone(design::Tone::Themed(design::accent_color));
        }
        toggle.into()
    }

    /// The pane's content with its inspector: the toggle floating over the
    /// content's top-right corner while the inspector is closed, unless
    /// `toolbar_hosts_toggle` says the content's own toolbar ends with it,
    /// and the inspector beside or over the content while it is open. An
    /// open inspector in a pane too narrow to dock it docks its rail
    /// instead, which stays beneath the inspector expanded over the content,
    /// so expanding it never reflows the content.
    ///
    /// The tree keeps one shape whatever the inspector does, so opening or
    /// closing it never remounts the pane's content and loses its scroll
    /// offset or focus.
    pub(super) fn with_pane_inspector<'a>(
        &'a self,
        card: CardId,
        content: Element<'a, Message>,
        toolbar_hosts_toggle: bool,
    ) -> Element<'a, Message> {
        let layout = self.pane_inspector_layout(card);
        let width = self.pane_inspector_width(card);
        let progress = self.pane_inspector_progress(card);
        let open = self.pane_inspectors.is_open(card);
        let railed = self.pane_inspectors.is_railed(card);
        let space = || -> Element<'a, Message> { iced::widget::Space::new().into() };
        // The splitter's layer sizes the layer in front of it, so where
        // there is no splitter it still spans the pane.
        let no_splitter = || -> Element<'a, Message> {
            iced::widget::Space::new()
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        };
        let (docked, splitter, front): (
            Element<'a, Message>,
            Element<'a, Message>,
            Element<'a, Message>,
        ) = match layout {
            None if railed => (self.inspector_rail(card), no_splitter(), space()),
            None if toolbar_hosts_toggle => (space(), no_splitter(), space()),
            None => {
                let toggle = widget::container(
                    widget::container(self.pane_inspector_toggle(card, false))
                        .class(cosmic::theme::Container::custom(floating_toggle_style)),
                )
                .padding(TOGGLE_INSET)
                .align_x(Alignment::End)
                .align_y(Alignment::Start)
                .width(Length::Fill)
                .height(Length::Shrink);
                (space(), no_splitter(), toggle.into())
            }
            Some(InspectorLayout::Docked) => {
                let visible = width * progress;
                let panel = crate::interaction::reveal_width(
                    self.pane_inspector_panel(card, width, open),
                    width,
                    progress,
                );
                let splitter = widget::container(self.pane_inspector_splitter(card, width))
                    .padding(Padding::ZERO.right(visible))
                    .align_x(Alignment::End)
                    .width(Length::Fill)
                    .height(Length::Fill);
                (panel, splitter.into(), space())
            }
            Some(InspectorLayout::Overlay) => {
                let panel = crate::interaction::reveal_width(
                    self.pane_inspector_panel(card, width, open),
                    width,
                    progress,
                );
                let panel = widget::container(crate::interaction::opaque(panel))
                    .align_x(Alignment::End)
                    .width(Length::Fill)
                    .height(Length::Fill);
                let rail = if railed {
                    self.inspector_rail(card)
                } else {
                    space()
                };
                (rail, no_splitter(), panel.into())
            }
        };
        let row = widget::row::with_children(vec![content, docked])
            .width(Length::Fill)
            .height(Length::Fill);
        let front = crate::interaction::layer(splitter, front);
        if layout == Some(InspectorLayout::Overlay) {
            crate::interaction::exclusive_layer(row.into(), Some(front))
        } else {
            crate::interaction::layer(row.into(), front)
        }
    }

    fn pane_inspector_splitter(&self, card: CardId, width: f32) -> Element<'_, Message> {
        let id = self.card_chrome.get(&card).map_or_else(
            || card_control_id(card, "inspector.resize"),
            |chrome| chrome.inspector_resize_id.clone(),
        );
        // Dragged no wider than this pane leaves room for, so the width
        // every inspector shares is one this pane can show.
        let maximum =
            pane_width(&self.workspace, card).map_or(design::INSPECTOR_MAX_WIDTH, |pane| {
                (pane - PANE_CONTENT_MIN_WIDTH)
                    .clamp(design::INSPECTOR_MIN_WIDTH, design::INSPECTOR_MAX_WIDTH)
            });
        Splitter::new(
            id,
            inspector::RESIZE_HANDLE_NAME,
            width,
            design::INSPECTOR_MIN_WIDTH,
            maximum,
            SplitterEdge::Leading,
            self.modal_is_open(),
            |width| Message::ContextInspector(InspectorAction::Resize { width }),
            || Message::ContextInspector(InspectorAction::ResizeFinished),
        )
        .default_width(design::INSPECTOR_DEFAULT_WIDTH)
        .into()
    }

    /// The open inspector: a rule against the pane's content, then the
    /// pane's sections, the first of which carries the toggle.
    fn pane_inspector_panel(&self, card: CardId, width: f32, open: bool) -> Element<'_, Message> {
        let sections =
            self.context_sections_view(card, width, self.pane_inspector_toggle(card, open));
        let overlay = open
            && self.pane_inspector_layout(card) == Some(InspectorLayout::Overlay)
            && !self.modal_is_open();
        let sections = crate::interaction::focused_key(sections, move |event| {
            (overlay
                && matches!(
                    event,
                    keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(keyboard::key::Named::Escape),
                        ..
                    }
                ))
            .then_some(Message::ClosePaneInspector(card))
        });
        widget::container(
            widget::row::with_children(vec![
                design::vertical_rule(),
                widget::container(sections)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into(),
            ])
            .width(Length::Fill)
            .height(Length::Fill),
        )
        .class(cosmic::theme::Container::Background)
        .id(card_control_id(card, "inspector.panel"))
        .width(width)
        .height(Length::Fill)
        .into()
    }
}

/// Whether a pane of `kind` has an inspector where the window shows it now.
/// `browsers_available` says whether the window is scoped to a worktree.
fn kind_availability(kind: &CardKind, browsers_available: bool) -> Availability {
    match kind {
        _ if !offers_inspector(kind) => Availability::Never,
        // A browser names no worktree; it shows the one a worktree's or
        // channel's workspace is scoped to, as `sidebar_focus` binds it.
        // Only the unscoped home strip leaves it nothing to show: a browser
        // a strip owns is never on screen there.
        CardKind::Browser(_) if !browsers_available => Availability::Unavailable,
        _ => Availability::Available,
    }
}

/// The width the strip gives the pane, once it has measured it.
fn pane_width(workspace: &crate::workspace::Workspace, card: CardId) -> Option<f32> {
    workspace
        .card_geometry(card)
        .map(|geometry| geometry.width())
        .filter(|width| *width > 0.0)
}

/// Whether the pane is too narrow for its content beside the narrowest
/// inspector. A wider inspector narrows itself to fit instead, so dragging
/// one wider never closes another.
fn pane_too_narrow(workspace: &crate::workspace::Workspace, card: CardId) -> bool {
    pane_width(workspace, card)
        .is_some_and(|pane| pane < design::INSPECTOR_MIN_WIDTH + PANE_CONTENT_MIN_WIDTH)
}

/// The floating toggle's surface: the pane's background, so the content it
/// covers does not show through, with a quiet edge.
fn floating_toggle_style(theme: &cosmic::Theme) -> iced::widget::container::Style {
    iced::widget::container::Style {
        background: Some(iced::Background::Color(theme.cosmic().bg_color().into())),
        border: iced::Border {
            color: design::muted_icon_color(theme).scale_alpha(0.35),
            width: design::BORDER_WIDTH,
            radius: design::RADIUS_S.into(),
        },
        ..Default::default()
    }
}

#[cfg(test)]
impl Cagents {
    /// Opens `card`'s inspector and lets its slide finish.
    pub(super) fn open_pane_inspector_now(&mut self, card: CardId) {
        drop(self.set_pane_inspector_open(card, true));
        self.settle_pane_inspectors();
    }

    /// Finishes every inspector's slide.
    pub(super) fn settle_pane_inspectors(&mut self) {
        drop(self.advance_pane_inspector_transitions(Instant::now() + DIALOG_TRANSITION_DURATION));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a card is, for [`an_incremental_sync_reaches_what_a_full_one_does`].
    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        Page,
        Browser,
        Pane,
    }

    /// The window an incremental and a full sync both catch up with.
    struct Window {
        cards: Vec<(CardId, Kind)>,
        strip: Vec<CardId>,
        narrow: HashSet<CardId>,
        on_screen: HashSet<CardId>,
        focused: Option<CardId>,
        browsers_available: bool,
        source: Arc<WorkspaceState>,
        geometry: u64,
    }

    impl Window {
        fn availability(&self, card: CardId) -> Availability {
            match self.cards.iter().find(|(id, _)| *id == card) {
                Some((_, Kind::Page)) => Availability::Never,
                Some((_, Kind::Browser)) if !self.browsers_available => Availability::Unavailable,
                _ => Availability::Available,
            }
        }

        fn sync_incremental(&self, inspectors: &mut PaneInspectors) {
            inspectors.sync_incremental(
                &self.source,
                SyncInputs {
                    focused: self.focused,
                    browsers_available: self.browsers_available,
                    geometry: self.geometry,
                },
                || {
                    self.cards
                        .iter()
                        .map(|(card, _)| (*card, self.availability(*card)))
                        .collect()
                },
                || self.strip.iter().copied(),
                |card| self.narrow.contains(&card),
                self.on_screen.iter().copied(),
                |card| self.on_screen.contains(&card),
                |card| self.cards.iter().position(|(id, _)| *id == card),
            );
        }

        fn sync(&self, inspectors: &mut PaneInspectors) {
            let cards: Vec<CardId> = self.cards.iter().map(|(card, _)| *card).collect();
            inspectors.sync(
                &cards,
                self.focused,
                |card| self.availability(card),
                |card| self.narrow.contains(&card),
                |card| self.on_screen.contains(&card),
            );
        }

        /// The durable cards changed, as copy-on-write gives them a new
        /// address.
        fn cards_changed(&mut self) {
            self.source = Arc::new(WorkspaceState::default());
        }
    }

    #[test]
    fn an_incremental_sync_reaches_what_a_full_one_does() {
        // SplitMix64, so a failing sequence replays from its seed.
        let mut seed: u64 = 0x5eed_1a5e;
        let mut next = move |bound: usize| {
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            ((z ^ (z >> 31)) % bound as u64) as usize
        };
        for run in 0..40 {
            let saved = cards(6);
            let kinds = [Kind::Pane, Kind::Pane, Kind::Browser, Kind::Page];
            let mut window = Window {
                cards: saved.iter().map(|card| (*card, kinds[next(4)])).collect(),
                strip: Vec::new(),
                narrow: HashSet::new(),
                on_screen: HashSet::new(),
                focused: None,
                browsers_available: next(2) == 0,
                source: Arc::new(WorkspaceState::default()),
                geometry: 0,
            };
            window.strip = window.cards.iter().map(|(card, _)| *card).take(4).collect();
            window.focused = window.strip.first().copied();
            let (open, legacy) = match run % 3 {
                0 => (Some(saved[..3].to_vec()), false),
                1 => (None, true),
                _ => (None, false),
            };
            let by_default = run % 2 == 0;
            let mut incremental =
                PaneInspectors::restore(open.clone(), legacy).with_default(by_default);
            let mut full = PaneInspectors::restore(open, legacy).with_default(by_default);
            let now = Instant::now();
            for step in 0..200 {
                match next(14) {
                    0 => {
                        let card = CardId::new();
                        let source = window.cards[next(window.cards.len())].0;
                        incremental.inherit(source, card);
                        full.inherit(source, card);
                        window.cards.push((card, kinds[next(4)]));
                        if next(2) == 0 {
                            window.strip.push(card);
                            window.geometry += 1;
                        }
                        window.cards_changed();
                    }
                    1 if window.cards.len() > 2 => {
                        let (card, _) = window.cards.remove(next(window.cards.len()));
                        if let Some(index) = window.strip.iter().position(|id| *id == card) {
                            window.strip.remove(index);
                            window.geometry += 1;
                        }
                        window.narrow.remove(&card);
                        window.on_screen.remove(&card);
                        if window.focused == Some(card) {
                            window.focused = window.strip.first().copied();
                        }
                        window.cards_changed();
                    }
                    2 => {
                        window.focused = (next(5) > 0)
                            .then(|| window.cards[next(window.cards.len())].0)
                            .filter(|card| window.strip.contains(card));
                    }
                    3 if !window.strip.is_empty() => {
                        let card = window.strip[next(window.strip.len())];
                        if !window.narrow.remove(&card) {
                            window.narrow.insert(card);
                        }
                        window.geometry += 1;
                    }
                    4 if !window.strip.is_empty() => {
                        let card = window.strip[next(window.strip.len())];
                        if !window.on_screen.remove(&card) {
                            window.on_screen.insert(card);
                        }
                    }
                    5 => window.browsers_available = !window.browsers_available,
                    6 | 7 => {
                        let card = window.cards[next(window.cards.len())].0;
                        if window.availability(card) == Availability::Available {
                            let open = next(3) > 0;
                            let narrow = window.narrow.contains(&card);
                            let at = now + Duration::from_millis(step);
                            incremental.set_open(card, open, narrow, None, at);
                            full.set_open(card, open, narrow, None, at);
                        }
                    }
                    8 => {
                        let at = now + Duration::from_millis(step) + DIALOG_TRANSITION_DURATION;
                        incremental.advance(at);
                        full.advance(at);
                    }
                    // A pane leaves the strip, as a scope switch takes it
                    // off screen, or comes back. Outside the strip it has no
                    // width, so it is never narrow: only `constrained`
                    // brings a narrowed pane that left back into the sync.
                    9 => {
                        let card = window.cards[next(window.cards.len())].0;
                        if let Some(index) = window.strip.iter().position(|id| *id == card) {
                            window.strip.remove(index);
                            window.narrow.remove(&card);
                            window.on_screen.remove(&card);
                            if window.focused == Some(card) {
                                window.focused = window.strip.first().copied();
                            }
                        } else {
                            window.strip.push(card);
                        }
                        window.geometry += 1;
                    }
                    // The default flips, as Settings does.
                    11 => {
                        let open = !incremental.open_by_default;
                        incremental.set_open_by_default(open, |card| window.availability(card));
                        full.set_open_by_default(open, |card| window.availability(card));
                    }
                    // A rail is pressed, or Escape collapses what it
                    // expanded.
                    12 => {
                        let card = window.cards[next(window.cards.len())].0;
                        let at = now + Duration::from_millis(step);
                        if next(2) == 0 {
                            incremental.expand(card, None, at);
                            full.expand(card, None, at);
                        } else {
                            incremental.collapse(card, None, at);
                            full.collapse(card, None, at);
                        }
                    }
                    // A card changes kind under the same card list.
                    10 => {
                        let index = next(window.cards.len());
                        window.cards[index].1 = kinds[next(4)];
                        window.cards_changed();
                    }
                    _ => window.cards_changed(),
                }
                window.sync_incremental(&mut incremental);
                window.sync(&mut full);
                assert_eq!(
                    incremental.projection_view(),
                    full.projection_view(),
                    "run {run}, step {step}",
                );
            }
        }
    }

    fn cards(count: usize) -> Vec<CardId> {
        (0..count).map(|_| CardId::new()).collect()
    }

    fn open(inspectors: &mut PaneInspectors, card: CardId) {
        let now = Instant::now();
        assert!(inspectors.set_open(card, true, false, None, now));
        inspectors.advance(now + DIALOG_TRANSITION_DURATION);
    }

    fn close(inspectors: &mut PaneInspectors, card: CardId) {
        let now = Instant::now();
        assert!(inspectors.set_open(card, false, false, None, now));
        inspectors.advance(now + DIALOG_TRANSITION_DURATION);
    }

    #[test]
    fn each_pane_opens_and_closes_its_own_inspector() {
        let panes = cards(2);
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(!inspectors.is_open(panes[0]) && !inspectors.is_open(panes[1]));
        open(&mut inspectors, panes[0]);
        inspectors.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(inspectors.is_open(panes[0]));
        assert!(!inspectors.is_open(panes[1]), "the other pane stays closed");
        // Focus moving to the other pane leaves both as they were.
        inspectors.sync(
            &panes,
            Some(panes[1]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(inspectors.is_open(panes[0]) && !inspectors.is_open(panes[1]));
    }

    #[test]
    fn a_pane_opened_from_another_copies_its_inspector() {
        let panes = cards(1);
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        open(&mut inspectors, panes[0]);
        inspectors.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        let split = CardId::new();
        inspectors.inherit(panes[0], split);
        // Saved before the workspace brings it, it is saved open.
        assert_eq!(inspectors.saved(|_| true).len(), 2);
        let both = [panes[0], split];
        inspectors.sync(
            &both,
            Some(split),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(
            inspectors.is_open(split),
            "copied from the pane it came from"
        );

        // From a pane with its inspector closed, the new pane's is closed,
        // and a pane merely opened beside an open one is closed too.
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        let split = CardId::new();
        inspectors.inherit(panes[0], split);
        inspectors.sync(
            &[panes[0], split],
            Some(split),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(!inspectors.is_open(split));
        open(&mut inspectors, panes[0]);
        let beside = CardId::new();
        inspectors.sync(
            &[panes[0], split, beside],
            Some(beside),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(!inspectors.is_open(beside));
    }

    #[test]
    fn open_inspectors_survive_a_restart_and_a_legacy_window_keeps_its_one() {
        let panes = cards(3);
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        open(&mut inspectors, panes[2]);
        let saved = inspectors.saved(|_| true);
        assert_eq!(saved, vec![panes[2]]);

        // The workspace may arrive after the window view.
        let mut restored = PaneInspectors::restore(Some(saved), false);
        restored.sync(&[], None, |_| Availability::Available, |_| false, |_| true);
        restored.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(restored.is_open(panes[2]));
        assert!(!restored.is_open(panes[0]) && !restored.is_open(panes[1]));
        // A pane saved open but not yet seen is still saved open.
        let unseen = PaneInspectors::restore(Some(vec![panes[1]]), false);
        assert_eq!(unseen.saved(|_| true), vec![panes[1]]);

        // A window saved with its one inspector open opens the focused
        // pane's; one saved closed opens none.
        let mut legacy = PaneInspectors::restore(None, true);
        legacy.sync(
            &panes,
            Some(panes[1]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(legacy.is_open(panes[1]));
        assert!(!legacy.is_open(panes[0]) && !legacy.is_open(panes[2]));
        let mut closed = PaneInspectors::restore(None, false);
        closed.sync(
            &panes,
            Some(panes[1]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(panes.iter().all(|pane| !closed.is_open(*pane)));
    }

    #[test]
    fn inspectors_open_by_default_until_the_user_closes_one_and_that_choice_is_kept() {
        let panes = cards(3);
        let sync = |inspectors: &mut PaneInspectors, cards: &[CardId]| {
            inspectors.sync(
                cards,
                cards.first().copied(),
                |_| Availability::Available,
                |_| false,
                |_| true,
            );
        };
        let mut inspectors = PaneInspectors::default().with_default(true);
        sync(&mut inspectors, &panes);
        assert!(panes.iter().all(|pane| inspectors.is_open(*pane)));
        assert_eq!(
            inspectors.choices(|_| true),
            Choices::default(),
            "following the default is not a choice"
        );
        close(&mut inspectors, panes[1]);
        sync(&mut inspectors, &panes);
        assert!(!inspectors.is_open(panes[1]));
        let choices = inspectors.choices(|_| true);
        assert_eq!(choices.closed, vec![panes[1]]);
        assert!(choices.opened.is_empty());

        // After a restart the closed pane stays closed and the rest follow
        // the default, whether the workspace arrives before or after.
        let mut restored = PaneInspectors::restore_choices(choices.clone()).with_default(true);
        sync(&mut restored, &[]);
        sync(&mut restored, &panes);
        assert!(!restored.is_open(panes[1]));
        assert!(restored.is_open(panes[0]) && restored.is_open(panes[2]));
        assert_eq!(restored.choices(|_| true), choices);

        // Turning the default off closes only the panes it opened, and a
        // pane split from one that follows it follows it too.
        let mut chosen = PaneInspectors::default().with_default(true);
        sync(&mut chosen, &panes);
        // Closed, then opened again: the user's choice now.
        close(&mut chosen, panes[0]);
        open(&mut chosen, panes[0]);
        chosen.set_open_by_default(false, |_| Availability::Available);
        sync(&mut chosen, &panes);
        assert!(chosen.is_open(panes[0]), "opened by the user");
        assert!(!chosen.is_open(panes[1]) && !chosen.is_open(panes[2]));
        let split = CardId::new();
        chosen.inherit(panes[2], split);
        sync(&mut chosen, &[panes[0], panes[1], panes[2], split]);
        assert!(!chosen.is_open(split));
        chosen.set_open_by_default(true, |_| Availability::Available);
        sync(&mut chosen, &[panes[0], panes[1], panes[2], split]);
        assert!(chosen.is_open(split) && chosen.is_open(panes[2]));
    }

    #[test]
    fn a_window_saved_before_inspectors_opened_by_default_closes_none_by_choice() {
        let panes = cards(3);
        // Inspectors were closed until opened, so the panes missing from an
        // older save were never closed by the user: they follow the default.
        let mut restored = PaneInspectors::restore(Some(vec![panes[2]]), false).with_default(true);
        restored.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(panes.iter().all(|pane| restored.is_open(*pane)));
        assert_eq!(
            restored.choices(|_| true),
            Choices {
                opened: vec![panes[2]],
                closed: Vec::new(),
            }
        );
        // As does a window saved with its one inspector closed.
        let mut legacy = PaneInspectors::restore(None, false).with_default(true);
        legacy.sync(
            &panes,
            Some(panes[1]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(panes.iter().all(|pane| legacy.is_open(*pane)));
        // A page never has one, whatever the default.
        let mut pages = PaneInspectors::default().with_default(true);
        pages.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Never,
            |_| false,
            |_| true,
        );
        assert!(panes.iter().all(|pane| !pages.is_open(*pane)));
    }

    #[test]
    fn a_pane_too_narrow_closes_its_inspector_but_may_reopen_it() {
        let panes = cards(1);
        let pane = panes[0];
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(
            &panes,
            Some(pane),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        open(&mut inspectors, pane);
        inspectors.sync(
            &panes,
            Some(pane),
            |_| Availability::Available,
            |_| true,
            |_| true,
        );
        assert!(!inspectors.is_open(pane), "narrowing closes it");
        assert!(!inspectors.is_visible(pane), "at once, without a slide");
        assert_eq!(
            inspectors.saved(|_| true),
            vec![pane],
            "a restart in a wider window brings it back"
        );
        // Widening brings it back.
        inspectors.sync(
            &panes,
            Some(pane),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(inspectors.is_open(pane));
        inspectors.sync(
            &panes,
            Some(pane),
            |_| Availability::Available,
            |_| true,
            |_| true,
        );
        assert!(!inspectors.is_open(pane));

        // Reopened while narrow, it stays open, over the content.
        assert!(inspectors.set_open(pane, true, true, None, Instant::now()));
        inspectors.sync(
            &panes,
            Some(pane),
            |_| Availability::Available,
            |_| true,
            |_| true,
        );
        assert!(inspectors.is_open(pane) && inspectors.opened_narrow(pane));
        // Once the pane is wide again it docks, and narrowing closes it.
        inspectors.sync(
            &panes,
            Some(pane),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(inspectors.is_open(pane) && !inspectors.opened_narrow(pane));
        inspectors.sync(
            &panes,
            Some(pane),
            |_| Availability::Available,
            |_| true,
            |_| true,
        );
        assert!(!inspectors.is_open(pane));

        // Closed by the user, it stays closed when the pane widens.
        let now = Instant::now();
        assert!(inspectors.set_open(pane, true, true, None, now));
        assert!(inspectors.set_open(pane, false, true, None, now));
        inspectors.sync(
            &panes,
            Some(pane),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(!inspectors.is_open(pane));
        assert!(inspectors.saved(|_| true).is_empty());
    }

    #[test]
    fn a_narrow_pane_collapses_its_open_inspector_to_a_rail_that_expands_over_it() {
        let panes = cards(2);
        let (pane, closed) = (panes[0], panes[1]);
        // Open by default, so nothing the user chose is saved, and nothing
        // the rail does may make it a choice.
        let mut inspectors = PaneInspectors::default().with_default(true);
        let sync = |inspectors: &mut PaneInspectors, narrow: bool| {
            inspectors.sync(
                &panes,
                Some(pane),
                |_| Availability::Available,
                |_| narrow,
                |_| true,
            );
        };
        sync(&mut inspectors, false);
        close(&mut inspectors, closed);
        let choices = inspectors.choices(|_| true);
        assert!(inspectors.is_open(pane) && !inspectors.is_railed(pane));

        // Too narrow to dock, the open inspector becomes a rail; the one
        // the user closed stays closed, with no rail.
        sync(&mut inspectors, true);
        assert!(!inspectors.is_open(pane) && inspectors.is_railed(pane));
        assert!(!inspectors.is_visible(pane), "at once, without a slide");
        assert!(!inspectors.is_railed(closed));
        assert!(
            inspectors.watching().any(|card| card == pane),
            "the rail still counts its work"
        );
        assert!(!inspectors.expand(closed, None, Instant::now()));

        // Pressed, it expands over the content, keeping its rail.
        let now = Instant::now();
        assert!(inspectors.expand(pane, None, now));
        assert!(!inspectors.expand(pane, None, now), "already expanded");
        sync(&mut inspectors, true);
        assert!(inspectors.is_open(pane) && inspectors.opened_narrow(pane));
        assert!(inspectors.is_railed(pane));
        inspectors.advance(now + DIALOG_TRANSITION_DURATION);
        assert_eq!(inspectors.choices(|_| true), choices);

        // Escape collapses it back to the rail, sliding away over the
        // content first.
        let now = Instant::now();
        assert!(inspectors.collapse(pane, None, now));
        sync(&mut inspectors, true);
        assert!(!inspectors.is_open(pane) && inspectors.is_visible(pane));
        assert!(inspectors.opened_narrow(pane), "it covers the content");
        assert!(inspectors.is_railed(pane));
        inspectors.advance(now + DIALOG_TRANSITION_DURATION);
        assert!(!inspectors.is_visible(pane) && !inspectors.opened_narrow(pane));
        assert!(inspectors.is_railed(pane));
        assert!(!inspectors.collapse(pane, None, now), "already collapsed");
        assert_eq!(inspectors.choices(|_| true), choices);

        // Widening docks it as it was, with nothing chosen.
        sync(&mut inspectors, false);
        assert!(inspectors.is_open(pane) && !inspectors.is_railed(pane));
        assert_eq!(inspectors.progress(pane, None), 1.0);
        assert_eq!(inspectors.choices(|_| true), choices);

        // Widened while collapsing, it docks at once rather than sliding
        // shut and open again.
        sync(&mut inspectors, true);
        let now = Instant::now();
        assert!(inspectors.expand(pane, None, now));
        inspectors.advance(now + DIALOG_TRANSITION_DURATION);
        assert!(inspectors.collapse(pane, None, now));
        sync(&mut inspectors, false);
        assert!(inspectors.is_open(pane) && !inspectors.is_animating());
        assert_eq!(inspectors.progress(pane, None), 1.0);

        // Closed by choice from over the content, the rail goes too.
        sync(&mut inspectors, true);
        assert!(inspectors.expand(pane, None, now));
        assert!(inspectors.set_open(pane, false, true, None, now));
        assert!(!inspectors.is_railed(pane));
        inspectors.advance(now + DIALOG_TRANSITION_DURATION);
        assert!(!inspectors.is_railed(pane) && !inspectors.opened_narrow(pane));

        // Opened over the content from closed, it was never the rail's, so
        // Escape's collapse leaves it for closing instead.
        assert!(inspectors.set_open(pane, true, true, None, now));
        assert!(inspectors.is_open(pane) && !inspectors.is_expanded(pane));
        assert!(!inspectors.collapse(pane, None, now));
        assert!(inspectors.is_open(pane));

        // An inspector with nothing to show here has no rail either.
        let mut inspectors = PaneInspectors::default().with_default(true);
        inspectors.sync(
            &panes,
            Some(pane),
            |_| Availability::Unavailable,
            |_| true,
            |_| true,
        );
        assert!(!inspectors.is_railed(pane));
        assert!(!inspectors.expand(pane, None, Instant::now()));
    }

    #[test]
    fn a_wider_inspector_narrows_itself_to_its_pane_rather_than_closing() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        drop(app.workspace.resize_viewport(800.0));
        app.open_pane_inspector_now(card);
        // Another pane's inspector dragged to the widest.
        app.settings.inspector_width = design::INSPECTOR_MAX_WIDTH;
        app.sync_pane_inspectors();
        assert!(app.pane_inspectors.is_open(card));
        assert_eq!(
            app.pane_inspector_width(card),
            800.0 - PANE_CONTENT_MIN_WIDTH
        );
        assert_eq!(
            app.pane_inspector_layout(card),
            Some(InspectorLayout::Docked)
        );

        // Too narrow for even the narrowest, it closes, and comes back as
        // the pane widens.
        drop(app.workspace.resize_viewport(600.0));
        app.sync_pane_inspectors();
        assert!(!app.pane_inspectors.is_open(card));
        drop(app.workspace.resize_viewport(1200.0));
        app.sync_pane_inspectors();
        assert!(app.pane_inspectors.is_open(card));
        assert_eq!(app.pane_inspector_width(card), design::INSPECTOR_MAX_WIDTH);
    }

    /// Where the pane's view lays out its inspector toggle.
    fn toggle_bounds(app: &Cagents, card: CardId, size: iced::Size) -> iced::Rectangle {
        mounted_toggle(app, card, size).expect("the pane mounts its inspector toggle")
    }

    /// Where the pane's view lays out its inspector toggle, if it mounts one.
    fn mounted_toggle(app: &Cagents, card: CardId, size: iced::Size) -> Option<iced::Rectangle> {
        mounted_control(app, card, size, app.pane_inspector_toggle_id(card))
    }

    /// Where the pane's view lays out its inspector rail, if it mounts one.
    fn mounted_rail(app: &Cagents, card: CardId, size: iced::Size) -> Option<iced::Rectangle> {
        mounted_control(app, card, size, app.pane_inspector_rail_id(card))
    }

    /// Where the pane's view lays out the focusable control `id`, if it
    /// mounts one.
    fn mounted_control(
        app: &Cagents,
        card: CardId,
        size: iced::Size,
        id: widget::Id,
    ) -> Option<iced::Rectangle> {
        use cosmic::iced::advanced::widget::{Operation, Tree, operation::Focusable};
        use cosmic::iced::advanced::{Layout, layout};

        struct Find(widget::Id, Option<iced::Rectangle>);
        impl Operation for Find {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: &mut dyn Focusable,
            ) {
                if id == Some(&self.0) {
                    self.1 = Some(bounds);
                }
            }
        }

        let renderer = super::super::tests::headless_renderer();
        let mut element = app.card_view(card, true, false);
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, size),
        );
        let mut find = Find(id, None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut find);
        find.1
    }

    /// Runs `task`'s focus changes against the pane's mounted view, then
    /// delivers `events` to it, as the native runtime does. Returns what the
    /// view published and the control left with focus.
    fn pane_keyboard(
        app: &Cagents,
        card: CardId,
        size: iced::Size,
        task: Task<Message>,
        events: &[iced::Event],
    ) -> (Vec<Message>, Option<widget::Id>) {
        use cosmic::iced::advanced::widget::{Operation, operation};
        use cosmic::iced::futures::StreamExt;
        use cosmic::iced::runtime::user_interface::{Cache, UserInterface};

        struct Focused(Option<widget::Id>);
        impl Operation for Focused {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                if state.is_focused() {
                    self.0 = id.cloned();
                }
            }
        }

        let mut renderer = super::super::tests::headless_renderer();
        let mut ui = UserInterface::build(
            app.card_view(card, true, false),
            size,
            Cache::new(),
            &mut renderer,
        );
        iced::futures::executor::block_on(async {
            let Some(mut stream) = iced::runtime::task::into_stream(task) else {
                return;
            };
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Widget(mut operation) = action {
                    loop {
                        ui.operate(&renderer, operation.as_mut());
                        match operation.finish() {
                            operation::Outcome::Chain(next) => operation = next,
                            _ => break,
                        }
                    }
                }
            }
        });
        let mut messages = Vec::new();
        let mut clipboard = iced::advanced::clipboard::Null;
        drop(ui.update(
            events,
            iced::mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard,
            &mut messages,
        ));
        let mut focused = Focused(None);
        ui.operate(&renderer, &mut focused);
        (messages, focused.0)
    }

    #[test]
    fn escape_returns_focus_to_the_rail_which_enter_and_space_press() {
        use cosmic::iced::keyboard::{self, Key, Modifiers, key::Named};
        let narrow = iced::Size::new(600.0, 800.0);
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let resize = |app: &mut Cagents, width: f32| {
            drop(app.update(Message::Viewport {
                generation: app.workspace_view_generation,
                event: WorkspaceViewportEvent::Resized(width),
            }));
        };
        // Open where it docks, then collapsed to its rail by a narrower pane.
        resize(&mut app, 1200.0);
        app.open_pane_inspector_now(card);
        resize(&mut app, narrow.width);
        assert!(app.pane_inspectors.is_railed(card));
        drop(app.update(Message::ExpandPaneInspector(card)));
        app.settle_pane_inspectors();
        assert!(app.pane_inspectors.is_expanded(card));

        // Escape slides the panel away, and the rail it leaves takes the
        // keyboard, as the toggle does once a panel closes.
        drop(app.close_pane_inspector_overlay(card));
        let settled =
            app.advance_pane_inspector_transitions(Instant::now() + DIALOG_TRANSITION_DURATION);
        assert!(app.pane_inspectors.is_railed(card));
        assert_eq!(app.pane_inspector_layout(card), None);
        let (_, focused) = pane_keyboard(&app, card, narrow, settled, &[]);
        assert_eq!(focused, Some(app.pane_inspector_rail_id(card)));

        let press = |key: Key, modifiers: Modifiers, repeat: bool| {
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                modified_key: key.clone(),
                key,
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers,
                text: None,
                repeat,
            })
        };
        let space = || Key::Character(" ".into());
        let focus_rail = || widget::button::focus(app.pane_inspector_rail_id(card));
        for (event, expands) in [
            (
                press(Key::Named(Named::Enter), Modifiers::empty(), false),
                true,
            ),
            (press(space(), Modifiers::empty(), false), true),
            (press(space(), Modifiers::SHIFT, false), false),
            (press(space(), Modifiers::CTRL, false), false),
            (press(space(), Modifiers::empty(), true), false),
        ] {
            let (messages, _) = pane_keyboard(&app, card, narrow, focus_rail(), &[event.clone()]);
            let pressed = messages
                .iter()
                .filter(
                    |message| matches!(message, Message::ExpandPaneInspector(id) if *id == card),
                )
                .count();
            assert_eq!(pressed, usize::from(expands), "{event:?}");
        }
        // Unfocused, the rail leaves Space to whatever has the keyboard.
        let (messages, _) = pane_keyboard(
            &app,
            card,
            narrow,
            Task::none(),
            &[press(space(), Modifiers::empty(), false)],
        );
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, Message::ExpandPaneInspector(_)))
        );
    }

    #[test]
    fn the_toggle_sits_in_the_panes_top_right_corner_however_the_inspector_is_drawn() {
        // Within the inset of the floating toggle or the header's gutter.
        const CORNER: f32 = 2.0 * space::S;
        let size = iced::Size::new(600.0, 800.0);
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        drop(app.workspace.resize_viewport(size.width));
        assert!(app.pane_too_narrow(card));
        let in_corner = |bounds: iced::Rectangle, state: &str| {
            assert!(
                bounds.width > 0.0 && bounds.height > 0.0,
                "{state}: the toggle is drawn: {bounds:?}"
            );
            assert!(
                size.width - bounds.x - bounds.width <= CORNER,
                "{state}: at the pane's right edge: {bounds:?}"
            );
            assert!(bounds.y <= CORNER, "{state}: at its top: {bounds:?}");
        };
        in_corner(toggle_bounds(&app, card, size), "closed");
        // Reopened in the narrow pane, over its content.
        app.open_pane_inspector_now(card);
        assert_eq!(
            app.pane_inspector_layout(card),
            Some(InspectorLayout::Overlay)
        );
        in_corner(toggle_bounds(&app, card, size), "over the content");
        // Docked beside the content of a wider pane.
        let size = iced::Size::new(1200.0, 800.0);
        drop(app.workspace.resize_viewport(size.width));
        app.sync_pane_inspectors();
        assert_eq!(
            app.pane_inspector_layout(card),
            Some(InspectorLayout::Docked)
        );
        in_corner(toggle_bounds(&app, card, size), "docked");
    }

    #[test]
    fn the_rail_stands_in_for_an_open_inspector_exactly_where_it_cannot_dock() {
        let narrow = iced::Size::new(600.0, 800.0);
        let wide = iced::Size::new(1200.0, 800.0);
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let resize = |app: &mut Cagents, size: iced::Size| {
            drop(app.update(Message::Viewport {
                generation: app.workspace_view_generation,
                event: WorkspaceViewportEvent::Resized(size.width),
            }));
        };
        resize(&mut app, wide);
        app.open_pane_inspector_now(card);
        let choices = app.saved_inspector_choices();
        assert!(mounted_rail(&app, card, wide).is_none(), "docked");

        // Too narrow to dock: the rail runs down the pane's right edge, in
        // place of the floating toggle.
        resize(&mut app, narrow);
        assert!(app.pane_inspectors.is_railed(card));
        let rail = mounted_rail(&app, card, narrow).expect("the rail is drawn");
        assert!(
            (narrow.width - (rail.x + rail.width)) <= space::XXS + 0.5,
            "at the pane's right edge: {rail:?}"
        );
        assert!(rail.width <= inspector_rail::RAIL_WIDTH, "{rail:?}");
        assert!(
            rail.height > narrow.height / 2.0,
            "the pane's height: {rail:?}"
        );
        assert!(mounted_toggle(&app, card, narrow).is_none());
        assert_eq!(app.pane_inspector_layout(card), None);
        assert!(!app.pane_content_inputs_blocked(card));

        // Pressed, it expands the inspector over the content, which owns
        // the keyboard while the rail stays laid out beneath it.
        drop(app.update(Message::ExpandPaneInspector(card)));
        app.settle_pane_inspectors();
        assert_eq!(
            app.pane_inspector_layout(card),
            Some(InspectorLayout::Overlay)
        );
        assert!(mounted_toggle(&app, card, narrow).is_some());
        assert!(
            mounted_rail(&app, card, narrow).is_none(),
            "covered, as the content is, the rail takes no focus"
        );
        assert_eq!(app.saved_inspector_choices(), choices);

        // Escape collapses it to the rail again.
        drop(app.update(Message::ClosePaneInspector(card)));
        app.settle_pane_inspectors();
        assert_eq!(app.pane_inspector_layout(card), None);
        assert!(mounted_rail(&app, card, narrow).is_some());
        assert!(mounted_toggle(&app, card, narrow).is_none());
        assert_eq!(app.saved_inspector_choices(), choices);

        // The inspector shortcut expands it too, as the rail is an open
        // inspector.
        drop(app.update(Message::ToggleContextInspector));
        app.settle_pane_inspectors();
        assert_eq!(
            app.pane_inspector_layout(card),
            Some(InspectorLayout::Overlay)
        );
        drop(app.update(Message::ClosePaneInspector(card)));
        app.settle_pane_inspectors();

        // Widening docks the inspector as it was.
        resize(&mut app, wide);
        assert_eq!(
            app.pane_inspector_layout(card),
            Some(InspectorLayout::Docked)
        );
        assert!(mounted_rail(&app, card, wide).is_none());
        assert_eq!(app.saved_inspector_choices(), choices);

        // Closed from over the content, it is closed: the floating toggle
        // returns, and nothing docks once the pane widens.
        resize(&mut app, narrow);
        drop(app.update(Message::ExpandPaneInspector(card)));
        app.settle_pane_inspectors();
        drop(app.update(Message::TogglePaneInspector(card)));
        app.settle_pane_inspectors();
        assert!(!app.pane_inspectors.is_railed(card));
        assert!(mounted_rail(&app, card, narrow).is_none());
        assert!(mounted_toggle(&app, card, narrow).is_some());

        // Opened over the content from closed, Escape closes it again, as
        // it was: no rail, and still closed once the pane widens.
        drop(app.update(Message::TogglePaneInspector(card)));
        app.settle_pane_inspectors();
        assert_eq!(
            app.pane_inspector_layout(card),
            Some(InspectorLayout::Overlay)
        );
        drop(app.update(Message::ClosePaneInspector(card)));
        app.settle_pane_inspectors();
        assert!(!app.pane_inspectors.is_open(card));
        assert!(!app.pane_inspectors.is_railed(card));
        assert!(mounted_rail(&app, card, narrow).is_none());
        assert!(mounted_toggle(&app, card, narrow).is_some());
        assert!(app.saved_inspector_choices().closed.contains(&card));
        resize(&mut app, wide);
        assert_eq!(app.pane_inspector_layout(card), None);
    }

    #[test]
    fn source_control_follows_the_focused_open_inspector_or_stays_where_it_was() {
        let panes = cards(3);
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert_eq!(inspectors.source_control(), None);
        open(&mut inspectors, panes[0]);
        inspectors.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert_eq!(inspectors.source_control(), Some(panes[0]));
        // Focusing a pane with its inspector closed keeps it where it is.
        inspectors.sync(
            &panes,
            Some(panes[2]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert_eq!(inspectors.source_control(), Some(panes[0]));
        // Focusing another open one moves it there.
        open(&mut inspectors, panes[1]);
        inspectors.sync(
            &panes,
            Some(panes[1]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert_eq!(inspectors.source_control(), Some(panes[1]));
        // Closing that one hands it to the inspector still open.
        let now = Instant::now();
        inspectors.set_open(panes[1], false, false, None, now);
        inspectors.advance(now + DIALOG_TRANSITION_DURATION);
        inspectors.sync(
            &panes,
            Some(panes[1]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert_eq!(inspectors.source_control(), Some(panes[0]));
        // With none open, none draws it.
        inspectors.set_open(panes[0], false, false, None, now);
        inspectors.advance(now + DIALOG_TRANSITION_DURATION);
        inspectors.sync(
            &panes,
            Some(panes[1]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert_eq!(inspectors.source_control(), None);
    }

    #[test]
    fn source_control_leaves_an_inspector_the_strip_scrolled_away() {
        let panes = cards(3);
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        open(&mut inspectors, panes[0]);
        open(&mut inspectors, panes[2]);
        inspectors.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert_eq!(inspectors.source_control(), Some(panes[0]));
        // Focus moves to a closed pane and the strip scrolls the owner away:
        // the open inspector still in view draws it.
        let on_screen = |card| card != panes[0];
        inspectors.sync(
            &panes,
            Some(panes[1]),
            |_| Availability::Available,
            |_| false,
            on_screen,
        );
        assert_eq!(inspectors.source_control(), Some(panes[2]));
        assert!(inspectors.is_open(panes[0]), "scrolled away, still open");
        // With no open inspector in view, none draws it.
        inspectors.sync(
            &panes,
            Some(panes[1]),
            |_| Availability::Available,
            |_| false,
            |card| card == panes[1],
        );
        assert_eq!(inspectors.source_control(), None);
    }

    /// The application pages, which belong to no worktree.
    const PAGES: [CardKind; 4] = [
        CardKind::Settings,
        CardKind::Accounts,
        CardKind::Automations,
        CardKind::Work,
    ];

    #[test]
    fn only_panes_whose_work_belongs_to_a_worktree_offer_an_inspector() {
        let directory = PathBuf::from("/work/project");
        let recipe = crate::model::LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: directory.clone(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let url = Url::parse("http://localhost:8080/").unwrap();
        let with_work = [
            ("terminal", super::super::tests::terminal_card_state().kind),
            (
                "chat",
                CardKind::Chat(crate::chat::Session {
                    id: crate::model::TerminalId::new(),
                    launch: crate::chat::Launch {
                        provider: crate::agent::Provider::Omp,
                        recipe,
                        session_id: None,
                    },
                    proof: None,
                    terminal: None,
                    owned_codex_listener: None,
                    native_continuity: Default::default(),
                }),
            ),
            (
                "browser",
                CardKind::Browser(crate::model::BrowserSession {
                    id: crate::model::BrowserId::new(),
                    current_url: url.clone(),
                    history: vec![url],
                    history_index: 0,
                    scroll_x: 0.0,
                    scroll_y: 0.0,
                    fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
                }),
            ),
            (
                "pull requests",
                CardKind::PullRequests(crate::model::PullRequestView {
                    repository: directory.clone(),
                    selected_number: Some(1),
                    file_panel_width: None,
                    file_panel_collapsed: None,
                }),
            ),
            (
                "repository",
                CardKind::Repository(crate::model::RepositoryView {
                    project: ProjectId::new(),
                    repository: directory.clone(),
                    worktree: directory.clone(),
                    selected_file: None,
                }),
            ),
            (
                "editor",
                CardKind::Repository(crate::model::RepositoryView {
                    project: ProjectId::new(),
                    repository: directory.clone(),
                    worktree: directory,
                    selected_file: Some("src/main.rs".into()),
                }),
            ),
        ];
        for (name, kind) in &with_work {
            assert!(offers_inspector(kind), "a {name} pane has an inspector");
        }
        for page in &PAGES {
            assert!(!offers_inspector(page), "{page:?} has no inspector");
        }
    }

    /// A listed worktree of the sidebar's test model, and its primary one.
    const FIX: &str = "/repo/fix-sidebar";
    const MAIN: &str = "/repo/main";

    /// An app whose focused terminal works in [`FIX`], a worktree Projects
    /// lists.
    fn worktree_app() -> (Cagents, CardId) {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.sidebar_model = crate::sidebar::tests::panel_model();
        let terminal = app.workspace.focused().unwrap();
        let CardKind::Terminal(session) = &mut app.card_state_mut(terminal).unwrap().kind else {
            unreachable!("the test app's pane is a terminal")
        };
        session.launch.working_directory = FIX.into();
        drop(app.rebind_source_control());
        assert_eq!(active_worktree(&app).as_deref(), Some(Path::new(FIX)));
        (app, terminal)
    }

    /// The worktree the window's source control and files are bound to.
    fn active_worktree(app: &Cagents) -> Option<PathBuf> {
        app.sidebar_state
            .active_worktree()
            .map(|key| key.canonical_path.clone())
    }

    /// A terminal app with `page` opened beside its terminal and focused.
    fn page_app(page: &CardKind) -> (Cagents, CardId, CardId) {
        let (mut app, terminal) = worktree_app();
        drop(app.open_utility_card(page.clone()));
        let page = app.workspace.focused().unwrap();
        assert_ne!(page, terminal, "the page opens as its own pane");
        drop(app.workspace.resize_viewport(1200.0));
        app.sync_pane_inspectors();
        (app, terminal, page)
    }

    #[test]
    fn an_application_page_has_no_toggle_and_nothing_opens_its_inspector() {
        let size = iced::Size::new(1200.0, 800.0);
        for kind in &PAGES {
            let (mut app, terminal, page) = page_app(kind);
            assert!(!app.pane_has_inspector(page), "{kind:?}");
            assert_eq!(
                mounted_toggle(&app, page, size),
                None,
                "{kind:?} mounts no toggle, so it takes no room or focus stop"
            );
            assert_eq!(app.focused_inspector_pane(), None, "{kind:?}");

            // Neither the shortcut, the toggle's message nor a command that
            // shows its result in the inspector opens one.
            drop(app.update(Message::ToggleContextInspector));
            drop(app.update(Message::TogglePaneInspector(page)));
            drop(app.set_pane_inspector_open(page, true));
            app.settle_pane_inspectors();
            assert!(!app.pane_inspectors.is_visible(page), "{kind:?}");
            assert_eq!(app.pane_inspector_layout(page), None, "{kind:?}");
            assert_eq!(app.pane_inspectors.source_control(), None, "{kind:?}");
            assert!(app.saved_open_inspectors().is_empty(), "{kind:?}");

            // The terminal beside it keeps its toggle and its inspector,
            // which keeps source control and the panels' worktree while the
            // page has focus.
            assert!(mounted_toggle(&app, terminal, size).is_some(), "{kind:?}");
            app.open_pane_inspector_now(terminal);
            assert!(app.pane_inspectors.is_open(terminal), "{kind:?}");
            drop(app.workspace.focus(page));
            app.sync_pane_inspectors();
            drop(app.rebind_source_control());
            assert_eq!(
                app.pane_inspectors.source_control(),
                Some(terminal),
                "{kind:?} never takes source control from the open inspector"
            );
            assert_eq!(
                app.inspector_panel_pin().as_deref(),
                Some(Path::new(FIX)),
                "{kind:?}: the panels stay pinned to the terminal's worktree"
            );
            assert_eq!(
                active_worktree(&app).as_deref(),
                Some(Path::new(FIX)),
                "{kind:?}"
            );
            drop(app.update(Message::ToggleContextInspector));
            assert!(
                app.pane_inspectors.is_open(terminal),
                "{kind:?}: the shortcut on the page leaves another pane's inspector alone"
            );
        }
    }

    #[test]
    fn a_page_saved_open_restores_closed_and_leaves_other_panes_saved_open() {
        let panes = cards(3);
        let page = panes[1];
        let availability = |card: CardId| {
            if card == page {
                Availability::Never
            } else {
                Availability::Available
            }
        };
        // `saved` lists its panes sorted.
        let mut others = vec![panes[0], panes[2]];
        others.sort_unstable();
        let mut restored = PaneInspectors::restore(Some(panes.clone()), false);
        restored.sync(&panes, Some(page), availability, |_| false, |_| true);
        assert!(!restored.is_open(page) && !restored.is_visible(page));
        assert!(restored.is_open(panes[0]) && restored.is_open(panes[2]));
        assert_eq!(restored.source_control(), Some(panes[0]));
        assert_eq!(
            restored.saved(|_| true),
            others,
            "the page's saved state is dropped, the others' kept"
        );

        // A legacy window restored focused on a page forgets its one
        // inspector, rather than opening it whenever another pane is focused.
        let mut legacy = PaneInspectors::restore(None, true);
        legacy.sync(&panes, Some(page), availability, |_| false, |_| true);
        legacy.sync(&panes, Some(panes[2]), availability, |_| false, |_| true);
        assert!(panes.iter().all(|card| !legacy.is_open(*card)));
        // Restored focused on a pane with one, it opens there.
        let mut legacy = PaneInspectors::restore(None, true);
        legacy.sync(&panes, Some(panes[2]), availability, |_| false, |_| true);
        assert!(legacy.is_open(panes[2]));
    }

    /// The window view saves what the user left open, but never a page's,
    /// even before the workspace brings the page.
    #[test]
    fn the_window_view_never_saves_a_pages_inspector_open() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let terminal = app.workspace.focused().unwrap();
        drop(app.open_utility_card(CardKind::Settings));
        let page = app.workspace.focused().unwrap();
        drop(app.workspace.resize_viewport(1200.0));
        app.pane_inspectors = PaneInspectors::restore(Some(vec![terminal, page]), false);
        assert_eq!(app.saved_open_inspectors(), vec![terminal]);
        app.sync_pane_inspectors();
        assert!(app.pane_inspectors.is_open(terminal));
        assert!(!app.pane_inspectors.is_visible(page));
        assert_eq!(app.saved_open_inspectors(), vec![terminal]);
    }

    /// A browser names no worktree: it shows the one the window is scoped
    /// to, so it has an inspector only in a worktree's workspace. Outside
    /// one, an inspector left open is hidden, still saved, and comes back.
    #[test]
    fn a_browser_has_an_inspector_only_in_a_worktrees_workspace() {
        let (mut app, browser) = worktree_app();
        let url = Url::parse("http://localhost:8080/").unwrap();
        app.card_state_mut(browser).unwrap().kind =
            CardKind::Browser(crate::model::BrowserSession {
                id: crate::model::BrowserId::new(),
                current_url: url.clone(),
                history: vec![url],
                history_index: 0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
            });
        drop(app.workspace.resize_viewport(1200.0));
        app.sync_pane_inspectors();

        let size = iced::Size::new(1200.0, 800.0);

        // In the unscoped home strip.
        assert!(mounted_toggle(&app, browser, size).is_none());
        assert_eq!(
            app.inspector_availability(browser),
            Availability::Unavailable
        );
        assert_eq!(
            app.focused_pane(),
            workspace_widget::FocusedPane::NoInspector
        );
        drop(app.update(Message::ToggleContextInspector));
        drop(app.update(Message::TogglePaneInspector(browser)));
        app.settle_pane_inspectors();
        assert!(!app.pane_inspectors.is_visible(browser));

        // In a worktree's or channel's workspace, it shows that worktree.
        app.active_workspace = Some(MAIN.into());
        assert!(app.pane_has_inspector(browser));
        assert!(mounted_toggle(&app, browser, size).is_some());
        assert_eq!(app.focused_pane(), workspace_widget::FocusedPane::Other);
        drop(app.update(Message::ToggleContextInspector));
        app.settle_pane_inspectors();
        assert!(app.pane_inspectors.is_open(browser));
        assert_eq!(app.pane_inspectors.source_control(), Some(browser));
        assert_eq!(active_worktree(&app).as_deref(), Some(Path::new(MAIN)));

        // Back outside one, it hides at once but stays saved open.
        app.active_workspace = None;
        app.sync_pane_inspectors();
        assert!(!app.pane_inspectors.is_visible(browser));
        assert_eq!(app.pane_inspectors.source_control(), None);
        assert_eq!(app.saved_open_inspectors(), vec![browser]);
        app.active_workspace = Some(MAIN.into());
        app.sync_pane_inspectors();
        assert!(app.pane_inspectors.is_open(browser));
    }

    /// A pull request pane's inspector shows the repository whose pull
    /// requests it lists, not whichever worktree was focused before it. In a
    /// worktree's or channel's workspace, the scope wins, as for every pane.
    #[test]
    fn a_pull_request_panes_inspector_shows_its_own_repository() {
        let (mut app, card) = worktree_app();
        app.card_state_mut(card).unwrap().kind =
            CardKind::PullRequests(crate::model::PullRequestView {
                repository: MAIN.into(),
                selected_number: None,
                file_panel_width: None,
                file_panel_collapsed: None,
            });
        app.sync_pane_inspectors();
        assert!(app.pane_has_inspector(card));
        app.open_pane_inspector_now(card);
        assert_eq!(app.pane_inspectors.source_control(), Some(card));
        assert_eq!(active_worktree(&app).as_deref(), Some(Path::new(MAIN)));

        app.active_workspace = Some(FIX.into());
        app.sync_pane_inspectors();
        drop(app.rebind_source_control());
        assert_eq!(active_worktree(&app).as_deref(), Some(Path::new(FIX)));
    }

    #[test]
    fn closing_a_pane_forgets_its_inspector() {
        let panes = cards(2);
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(
            &panes,
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        open(&mut inspectors, panes[1]);
        inspectors.sync(
            &panes[..1],
            Some(panes[0]),
            |_| Availability::Available,
            |_| false,
            |_| true,
        );
        assert!(!inspectors.is_visible(panes[1]));
        assert!(inspectors.saved(|_| true).is_empty());
    }
}
