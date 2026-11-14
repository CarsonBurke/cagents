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
//!
//! The window binds source control and files to one worktree at a time, so
//! only one open inspector draws them: the focused pane's, or, while the
//! focused pane's inspector is closed, the last one that drew them. Every
//! other open inspector counts its own changes and offers to show them.

use super::*;

/// The narrowest a pane's content may become beside its docked inspector.
/// A pane narrower than this and the inspector together closes it.
pub(super) const PANE_CONTENT_MIN_WIDTH: f32 = 360.0;

/// The gap between the floating toggle and the corner of the pane's content.
const TOGGLE_INSET: f32 = space::XS;

/// One pane's inspector.
#[derive(Debug, Clone, Copy, Default)]
struct PaneState {
    /// Whether the user left the inspector open. This is what is saved.
    open: bool,
    /// Open, but closed for now because the pane is too narrow for it.
    narrowed: bool,
    transition: Option<InspectorTransition>,
    /// Opened while the pane was too narrow for it, so it covers the pane's
    /// content, and narrowness does not close it again.
    opened_narrow: bool,
}

impl PaneState {
    /// Whether the inspector is open on screen.
    fn shown(&self) -> bool {
        self.open && !self.narrowed
    }
}

/// Every pane's inspector, and which of them draws source control.
#[derive(Debug, Default)]
pub(super) struct PaneInspectors {
    panes: HashMap<CardId, PaneState>,
    /// Panes to open once this window sees them: those saved open, as a
    /// workspace restored after the window view brings them, and copies of
    /// panes whose inspectors were open.
    restored_open: HashSet<CardId>,
    /// A window saved before inspectors were per pane, with its one
    /// inspector open: the first pane focused opens it.
    legacy_open: bool,
    /// The pane whose inspector draws the window's source control and files.
    source_control: Option<CardId>,
}

impl PaneInspectors {
    /// What a window view saved: the panes whose inspectors were open, or,
    /// from a window saved before they were per pane, whether its one
    /// inspector was.
    pub(super) fn restore(open: Option<Vec<CardId>>, legacy_open: bool) -> Self {
        match open {
            Some(open) => Self {
                restored_open: open.into_iter().collect(),
                ..Self::default()
            },
            None => Self {
                legacy_open,
                ..Self::default()
            },
        }
    }

    /// The panes to save as open, in a stable order, among the cards `known`
    /// says still exist.
    pub(super) fn saved(&self, known: impl Fn(CardId) -> bool) -> Vec<CardId> {
        let mut open: Vec<CardId> = self
            .panes
            .iter()
            .filter(|(_, pane)| pane.open)
            .map(|(card, _)| *card)
            .chain(self.restored_open.iter().copied())
            .filter(|card| known(*card))
            .collect();
        open.sort_unstable();
        open.dedup();
        open
    }

    /// Starts `copy`, a pane split from `source`, with `source`'s inspector
    /// open or closed.
    pub(super) fn inherit(&mut self, source: CardId, copy: CardId) {
        if self.is_open(source) {
            self.restored_open.insert(copy);
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

    pub(super) fn visible(&self) -> impl Iterator<Item = CardId> + '_ {
        self.panes
            .iter()
            .filter(|(_, pane)| pane.shown() || pane.transition.is_some())
            .map(|(card, _)| *card)
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
        self.restored_open.remove(&card);
        if pane.shown() == open {
            return false;
        }
        pane.open = open;
        pane.narrowed = false;
        if open {
            pane.opened_narrow = narrow;
        }
        // Closing keeps its mounting mode until the panel has slid away.
        pane.transition = Some(InspectorTransition {
            started_at: now,
            from,
            to: if open { 1.0 } else { 0.0 },
        });
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
                if !pane.open {
                    pane.opened_narrow = false;
                }
                finished = true;
            }
        }
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
    /// the focused one, and `on_screen` says which the strip shows.
    pub(super) fn sync(
        &mut self,
        cards: &[CardId],
        focused: Option<CardId>,
        narrow: impl Fn(CardId) -> bool,
        on_screen: impl Fn(CardId) -> bool,
    ) {
        for card in cards {
            if self.panes.contains_key(card) {
                continue;
            }
            let open =
                self.restored_open.remove(card) || (self.legacy_open && Some(*card) == focused);
            if self.legacy_open && Some(*card) == focused {
                self.legacy_open = false;
            }
            self.panes.insert(
                *card,
                PaneState {
                    open,
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
            self.legacy_open = false;
        }
        // Only open inspectors care how wide their panes are.
        for (card, pane) in &mut self.panes {
            if !pane.open {
                continue;
            }
            if !narrow(*card) {
                pane.narrowed = false;
                pane.opened_narrow = false;
            } else if !pane.opened_narrow && !pane.narrowed {
                // At once, without a slide.
                pane.narrowed = true;
                pane.transition = None;
            }
        }
        // Panes closed for good, here or in another window. Every pane in
        // `cards` is known by now, so any more are gone.
        if !cards.is_empty() && self.panes.len() > cards.len() {
            let present: HashSet<CardId> = cards.iter().copied().collect();
            self.panes.retain(|card, _| present.contains(card));
        }
        self.source_control = match focused {
            Some(focused) if self.is_visible(focused) => Some(focused),
            _ => {
                let shown = |card: &CardId| self.is_visible(*card) && on_screen(*card);
                self.source_control
                    .filter(shown)
                    .or_else(|| cards.iter().copied().find(shown))
            }
        };
    }
}

impl Cagents {
    /// The width a pane gives its inspector: the one width every inspector
    /// shares, narrowed as far as the inspector's minimum to leave the
    /// pane's content its own, and never more than the pane.
    pub(super) fn pane_inspector_width(&self, card: CardId) -> f32 {
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
    /// commands that show their results there open.
    pub(super) fn focused_inspector_pane(&self) -> Option<CardId> {
        self.workspace.focused()
    }

    pub(super) fn set_pane_inspector_open(&mut self, card: CardId, open: bool) -> Task<Message> {
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
        let focus = if open && self.pane_inspector_layout(card) == Some(InspectorLayout::Overlay) {
            iced::widget::operation::focus(self.pane_inspector_toggle_id(card))
                .map(cosmic::Action::App)
        } else {
            Task::none()
        };
        Task::batch([binding, self.save_settings_task(false), focus])
    }

    pub(super) fn toggle_pane_inspector(&mut self, card: CardId) -> Task<Message> {
        let open = !self.pane_inspectors.is_open(card);
        self.set_pane_inspector_open(card, open)
    }

    pub(super) fn advance_pane_inspector_transitions(&mut self, now: Instant) -> Task<Message> {
        if !self.pane_inspectors.advance(now) {
            return Task::none();
        }
        self.sync_pane_inspectors();
        // A frame skips the rebind, and a closed inspector no longer pins
        // the panels to the work it showed.
        self.rebind_source_control()
    }

    /// Runs at the tail of every update, before the source-control rebind;
    /// see [`PaneInspectors::sync`].
    pub(super) fn sync_pane_inspectors(&mut self) {
        let cards: Vec<CardId> = self.durable.cards.iter().map(|card| card.id).collect();
        let focused = self.workspace.focused();
        let workspace = &self.workspace;
        self.pane_inspectors.sync(
            &cards,
            focused,
            |card| pane_too_narrow(workspace, card),
            |card| {
                workspace
                    .visibility(card)
                    .is_some_and(crate::workspace::VisibilityClass::is_mounted)
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
    /// and the inspector beside or over the content while it is open.
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
                (space(), no_splitter(), panel.into())
            }
        };
        let row = widget::row::with_children(vec![content, docked])
            .width(Length::Fill)
            .height(Length::Fill);
        crate::interaction::layer(row.into(), crate::interaction::layer(splitter, front))
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
        .width(width)
        .height(Length::Fill)
        .into()
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
mod tests {
    use super::*;

    fn cards(count: usize) -> Vec<CardId> {
        (0..count).map(|_| CardId::new()).collect()
    }

    fn open(inspectors: &mut PaneInspectors, card: CardId) {
        let now = Instant::now();
        assert!(inspectors.set_open(card, true, false, None, now));
        inspectors.advance(now + DIALOG_TRANSITION_DURATION);
    }

    #[test]
    fn each_pane_opens_and_closes_its_own_inspector() {
        let panes = cards(2);
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(&panes, Some(panes[0]), |_| false, |_| true);
        assert!(!inspectors.is_open(panes[0]) && !inspectors.is_open(panes[1]));
        open(&mut inspectors, panes[0]);
        inspectors.sync(&panes, Some(panes[0]), |_| false, |_| true);
        assert!(inspectors.is_open(panes[0]));
        assert!(!inspectors.is_open(panes[1]), "the other pane stays closed");
        // Focus moving to the other pane leaves both as they were.
        inspectors.sync(&panes, Some(panes[1]), |_| false, |_| true);
        assert!(inspectors.is_open(panes[0]) && !inspectors.is_open(panes[1]));
    }

    #[test]
    fn a_pane_opened_from_another_copies_its_inspector() {
        let panes = cards(1);
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(&panes, Some(panes[0]), |_| false, |_| true);
        open(&mut inspectors, panes[0]);
        inspectors.sync(&panes, Some(panes[0]), |_| false, |_| true);
        let split = CardId::new();
        inspectors.inherit(panes[0], split);
        // Saved before the workspace brings it, it is saved open.
        assert_eq!(inspectors.saved(|_| true).len(), 2);
        let both = [panes[0], split];
        inspectors.sync(&both, Some(split), |_| false, |_| true);
        assert!(
            inspectors.is_open(split),
            "copied from the pane it came from"
        );

        // From a pane with its inspector closed, the new pane's is closed,
        // and a pane merely opened beside an open one is closed too.
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(&panes, Some(panes[0]), |_| false, |_| true);
        let split = CardId::new();
        inspectors.inherit(panes[0], split);
        inspectors.sync(&[panes[0], split], Some(split), |_| false, |_| true);
        assert!(!inspectors.is_open(split));
        open(&mut inspectors, panes[0]);
        let beside = CardId::new();
        inspectors.sync(
            &[panes[0], split, beside],
            Some(beside),
            |_| false,
            |_| true,
        );
        assert!(!inspectors.is_open(beside));
    }

    #[test]
    fn open_inspectors_survive_a_restart_and_a_legacy_window_keeps_its_one() {
        let panes = cards(3);
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(&panes, Some(panes[0]), |_| false, |_| true);
        open(&mut inspectors, panes[2]);
        let saved = inspectors.saved(|_| true);
        assert_eq!(saved, vec![panes[2]]);

        // The workspace may arrive after the window view.
        let mut restored = PaneInspectors::restore(Some(saved), false);
        restored.sync(&[], None, |_| false, |_| true);
        restored.sync(&panes, Some(panes[0]), |_| false, |_| true);
        assert!(restored.is_open(panes[2]));
        assert!(!restored.is_open(panes[0]) && !restored.is_open(panes[1]));
        // A pane saved open but not yet seen is still saved open.
        let unseen = PaneInspectors::restore(Some(vec![panes[1]]), false);
        assert_eq!(unseen.saved(|_| true), vec![panes[1]]);

        // A window saved with its one inspector open opens the focused
        // pane's; one saved closed opens none.
        let mut legacy = PaneInspectors::restore(None, true);
        legacy.sync(&panes, Some(panes[1]), |_| false, |_| true);
        assert!(legacy.is_open(panes[1]));
        assert!(!legacy.is_open(panes[0]) && !legacy.is_open(panes[2]));
        let mut closed = PaneInspectors::restore(None, false);
        closed.sync(&panes, Some(panes[1]), |_| false, |_| true);
        assert!(panes.iter().all(|pane| !closed.is_open(*pane)));
    }

    #[test]
    fn a_pane_too_narrow_closes_its_inspector_but_may_reopen_it() {
        let panes = cards(1);
        let pane = panes[0];
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(&panes, Some(pane), |_| false, |_| true);
        open(&mut inspectors, pane);
        inspectors.sync(&panes, Some(pane), |_| true, |_| true);
        assert!(!inspectors.is_open(pane), "narrowing closes it");
        assert!(!inspectors.is_visible(pane), "at once, without a slide");
        assert_eq!(
            inspectors.saved(|_| true),
            vec![pane],
            "a restart in a wider window brings it back"
        );
        // Widening brings it back.
        inspectors.sync(&panes, Some(pane), |_| false, |_| true);
        assert!(inspectors.is_open(pane));
        inspectors.sync(&panes, Some(pane), |_| true, |_| true);
        assert!(!inspectors.is_open(pane));

        // Reopened while narrow, it stays open, over the content.
        assert!(inspectors.set_open(pane, true, true, None, Instant::now()));
        inspectors.sync(&panes, Some(pane), |_| true, |_| true);
        assert!(inspectors.is_open(pane) && inspectors.opened_narrow(pane));
        // Once the pane is wide again it docks, and narrowing closes it.
        inspectors.sync(&panes, Some(pane), |_| false, |_| true);
        assert!(inspectors.is_open(pane) && !inspectors.opened_narrow(pane));
        inspectors.sync(&panes, Some(pane), |_| true, |_| true);
        assert!(!inspectors.is_open(pane));

        // Closed by the user, it stays closed when the pane widens.
        let now = Instant::now();
        assert!(inspectors.set_open(pane, true, true, None, now));
        assert!(inspectors.set_open(pane, false, true, None, now));
        inspectors.sync(&panes, Some(pane), |_| false, |_| true);
        assert!(!inspectors.is_open(pane));
        assert!(inspectors.saved(|_| true).is_empty());
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
        let mut find = Find(app.pane_inspector_toggle_id(card), None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut find);
        find.1.expect("the pane mounts its inspector toggle")
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
    fn source_control_follows_the_focused_open_inspector_or_stays_where_it_was() {
        let panes = cards(3);
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(&panes, Some(panes[0]), |_| false, |_| true);
        assert_eq!(inspectors.source_control(), None);
        open(&mut inspectors, panes[0]);
        inspectors.sync(&panes, Some(panes[0]), |_| false, |_| true);
        assert_eq!(inspectors.source_control(), Some(panes[0]));
        // Focusing a pane with its inspector closed keeps it where it is.
        inspectors.sync(&panes, Some(panes[2]), |_| false, |_| true);
        assert_eq!(inspectors.source_control(), Some(panes[0]));
        // Focusing another open one moves it there.
        open(&mut inspectors, panes[1]);
        inspectors.sync(&panes, Some(panes[1]), |_| false, |_| true);
        assert_eq!(inspectors.source_control(), Some(panes[1]));
        // Closing that one hands it to the inspector still open.
        let now = Instant::now();
        inspectors.set_open(panes[1], false, false, None, now);
        inspectors.advance(now + DIALOG_TRANSITION_DURATION);
        inspectors.sync(&panes, Some(panes[1]), |_| false, |_| true);
        assert_eq!(inspectors.source_control(), Some(panes[0]));
        // With none open, none draws it.
        inspectors.set_open(panes[0], false, false, None, now);
        inspectors.advance(now + DIALOG_TRANSITION_DURATION);
        inspectors.sync(&panes, Some(panes[1]), |_| false, |_| true);
        assert_eq!(inspectors.source_control(), None);
    }

    #[test]
    fn source_control_leaves_an_inspector_the_strip_scrolled_away() {
        let panes = cards(3);
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(&panes, Some(panes[0]), |_| false, |_| true);
        open(&mut inspectors, panes[0]);
        open(&mut inspectors, panes[2]);
        inspectors.sync(&panes, Some(panes[0]), |_| false, |_| true);
        assert_eq!(inspectors.source_control(), Some(panes[0]));
        // Focus moves to a closed pane and the strip scrolls the owner away:
        // the open inspector still in view draws it.
        let on_screen = |card| card != panes[0];
        inspectors.sync(&panes, Some(panes[1]), |_| false, on_screen);
        assert_eq!(inspectors.source_control(), Some(panes[2]));
        assert!(inspectors.is_open(panes[0]), "scrolled away, still open");
        // With no open inspector in view, none draws it.
        inspectors.sync(&panes, Some(panes[1]), |_| false, |card| card == panes[1]);
        assert_eq!(inspectors.source_control(), None);
    }

    #[test]
    fn closing_a_pane_forgets_its_inspector() {
        let panes = cards(2);
        let mut inspectors = PaneInspectors::default();
        inspectors.sync(&panes, Some(panes[0]), |_| false, |_| true);
        open(&mut inspectors, panes[1]);
        inspectors.sync(&panes[..1], Some(panes[0]), |_| false, |_| true);
        assert!(!inspectors.is_visible(panes[1]));
        assert!(inspectors.saved(|_| true).is_empty());
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
