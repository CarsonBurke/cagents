//! The workspace switcher: recently used workspaces, held open with the
//! modifier of Alt+Tab, and a live look at the highlighted one.
//!
//! Stepping through it changes nothing but the switcher. The window behind
//! it switches workspace once, when the modifier is released or Enter is
//! pressed, so a step costs a redraw of the list rather than a rebuilt
//! workspace. The highlighted workspace's focused pane is shown as it is,
//! from its own conversation or terminal, once the selection rests on it.

use super::*;
use crate::sidebar::AttentionSummary;
use cosmic::Theme;

/// How long the modifier is held before the switcher is drawn. A tap that
/// toggles between the two most recent workspaces ends sooner, and would
/// otherwise flash a list nobody reads.
pub(super) const DELAY: Duration = Duration::from_millis(120);
/// How long the selection rests on a workspace before its pane is brought up
/// for the preview, so stepping quickly past rows brings none of them up.
const PREVIEW_DWELL: Duration = Duration::from_millis(90);
const SCROLL_ID: &str = "workspace-switcher.rows";
/// Rows have one height so the list can be scrolled to a row by its index.
const ROW_HEIGHT: f32 = 52.0;
const ROW_GAP: f32 = space::XXS;
const LIST_PADDING: f32 = space::XS;
const LIST_WIDTH: f32 = 340.0;
/// Narrower than this beside the list, the preview would show too little to
/// be worth the room it takes from the list's names.
const DETAIL_MIN_WIDTH: f32 = 300.0;
const PANEL_MAX_WIDTH: f32 = 880.0;
const PANEL_MAX_HEIGHT: f32 = 720.0;
const PANEL_MIN_HEIGHT: f32 = 3.0 * ROW_HEIGHT + FOOTER_HEIGHT + 2.0 * LIST_PADDING;
const FOOTER_HEIGHT: f32 = 40.0;
/// The scrim left showing above and below the panel.
const WINDOW_MARGIN: f32 = 2.0 * design::HEADER_HEIGHT;
const TERMINAL_PREVIEW_FONT: f32 = 11.0;
const DIGITS: usize = 9;
/// The column a row's number key sits in.
const ROW_NUMBER_WIDTH: f32 = 20.0;

/// One destination in the workspace switcher, in the order it is listed.
/// What it says is read once, when the hold begins, so a row keeps its name
/// while the list is open; its status is read live.
#[derive(Debug)]
pub(super) struct Row {
    pub(super) destination: WorkspaceDestination,
    pub(super) action: SidebarAction,
    pub(super) project: Option<ProjectId>,
    pub(super) name: String,
    /// Where the workspace is: its project, and a channel's branch.
    pub(super) place: String,
}

/// The pane of the highlighted workspace its preview shows, read when the
/// selection moves.
#[derive(Debug, Default)]
pub(super) struct Panes {
    destination: Option<WorkspaceDestination>,
    focused: Option<CardId>,
    /// The channel `focused` belongs to, when the workspace shows a pane
    /// that works in it from a channel rather than one of its own.
    channel: Option<crate::model::ChannelId>,
}

/// The switcher's size, from the window it is drawn in.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Geometry {
    width: f32,
    height: f32,
    list_height: f32,
    detail: bool,
}

impl Geometry {
    fn new(window: iced::Size, rows: usize) -> Self {
        let width = (window.width - 2.0 * space::XL).clamp(0.0, PANEL_MAX_WIDTH);
        let detail = width >= LIST_WIDTH + DETAIL_MIN_WIDTH;
        let available = (window.height - 2.0 * WINDOW_MARGIN)
            .max(PANEL_MIN_HEIGHT)
            .min(PANEL_MAX_HEIGHT)
            .min((window.height - 2.0 * space::S).max(0.0));
        // Without a preview beside it, the list is only as tall as its rows.
        let height = if detail {
            available
        } else {
            (rows_height(rows) + 2.0 * LIST_PADDING + FOOTER_HEIGHT).min(available)
        };
        Self {
            width,
            height,
            list_height: (height - FOOTER_HEIGHT - 2.0 * LIST_PADDING).max(ROW_HEIGHT),
            detail,
        }
    }
}

fn rows_height(rows: usize) -> f32 {
    rows as f32 * (ROW_HEIGHT + ROW_GAP) - ROW_GAP.min(rows as f32 * ROW_GAP)
}

/// `over` painted on `under`, for a fade that must end in the exact colour
/// behind it.
fn composite(under: iced::Color, over: iced::Color) -> iced::Color {
    let alpha = over.a;
    iced::Color {
        r: under.r * (1.0 - alpha) + over.r * alpha,
        g: under.g * (1.0 - alpha) + over.g * alpha,
        b: under.b * (1.0 - alpha) + over.b * alpha,
        a: under.a.max(alpha),
    }
}

/// The panel's background, which a row's text fades into.
fn panel_color(theme: &Theme) -> iced::Color {
    theme.current_container().component.base.into()
}

/// A row's background: the panel's, under the sidebar's selected tint.
fn row_color(theme: &Theme, selected: bool) -> iced::Color {
    let base = panel_color(theme);
    if selected {
        let tint: iced::Color = theme.current_container().on.into();
        composite(base, tint.scale_alpha(sidebar::SELECTED_ROW_ALPHA))
    } else {
        base
    }
}

/// A hairline between the panel's parts.
fn rule<'a>(width: Length, height: Length) -> Element<'a, Message> {
    widget::container(iced::widget::Space::new())
        .width(width)
        .height(height)
        .class(cosmic::theme::Container::custom(|theme| {
            widget::container::Style {
                background: Some(iced::Background::Color(design::component_rule(theme))),
                ..Default::default()
            }
        }))
        .into()
}

/// A key as the footer and the rows draw it: its name in a hairline box, on
/// a row whose background is `selected`'s.
fn key_outline<'a>(
    key: impl Into<Element<'a, Message>>,
    selected: bool,
) -> widget::Container<'a, Message, Theme> {
    widget::container(key)
        .padding([0.0, space::XS + 1.0])
        .class(cosmic::theme::Container::custom(move |theme| {
            widget::container::Style {
                border: iced::Border {
                    color: design::component_rule_on(theme, row_color(theme, selected)),
                    width: design::BORDER_WIDTH,
                    radius: design::RADIUS_XS.into(),
                },
                ..Default::default()
            }
        }))
}

fn keycap<'a>(key: &'a str, label: &'a str) -> Element<'a, Message> {
    widget::row::with_children(vec![
        key_outline(design::caption(key), false).into(),
        design::caption(label).into(),
    ])
    .spacing(space::XS)
    .align_y(Alignment::Center)
    .into()
}

/// The number key that opens row `index`, for the first [`DIGITS`] rows.
/// Later rows keep the room it takes, so every name starts at one edge.
fn row_number<'a>(index: usize, selected: bool) -> Element<'a, Message> {
    let key: Element<'a, Message> = if index < DIGITS {
        key_outline(design::caption((index + 1).to_string()), selected)
            .center_x(Length::Fill)
            .into()
    } else {
        iced::widget::Space::new().into()
    };
    widget::container(key).width(ROW_NUMBER_WIDTH).into()
}

impl Cagents {
    /// Preview workspaces in a frozen MRU order, committing on modifier release.
    pub(super) fn cycle_workspace(
        &mut self,
        direction: crate::workspace::FocusDirection,
        modifiers: keyboard::Modifiers,
    ) -> Task<Message> {
        if self.workspace_inputs_blocked() || self.focused_repository_is_saving() {
            return Task::none();
        }
        let Some(action) = self.next_workspace(direction) else {
            return Task::none();
        };
        if self
            .sidebar_entered_directory(&action)
            .is_some_and(|directory| self.refuse_in_deleting_worktree(&directory))
        {
            self.cancel_workspace_preview();
            return Task::none();
        }
        let first = self.chrome_ui.workspace_preview.replace(action).is_none();
        self.workspace_cycle_modifiers = modifiers
            & (keyboard::Modifiers::CTRL | keyboard::Modifiers::ALT | keyboard::Modifiers::LOGO);
        if self.workspace_cycle_modifiers.is_empty() {
            return self.commit_workspace_preview();
        }
        if !first {
            // A second step is someone looking for a workspace, not a tap.
            self.chrome_ui.workspace_switcher_shown = true;
            return Task::batch([
                self.workspace_selection_moved(),
                self.reveal_workspace_selection(),
            ]);
        }
        let since = Instant::now();
        self.chrome_ui.workspace_preview_since = Some(since);
        cosmic::task::future(async move {
            tokio::time::sleep(DELAY).await;
            Message::WorkspaceSwitcherDue(since)
        })
    }

    pub(super) fn commit_workspace_preview(&mut self) -> Task<Message> {
        let action = self.close_workspace_preview();
        // Coming back round to the workspace already open is a way of
        // cancelling. Selecting it again would give a workspace without a
        // terminal or conversation its default pane.
        let task = action
            .filter(|action| {
                let destination = WorkspaceDestination::from_action(action);
                destination != self.active_workspace_destination()
                    && sidebar::workspace_cycle(&self.sidebar_model, &self.sidebar_state)
                        .iter()
                        .any(|available| available == action)
            })
            .map_or_else(Task::none, |action| self.update(Message::Sidebar(action)));
        self.workspace_history
            .finish(self.active_workspace_destination());
        self.workspace_history
            .truncate_recent(WORKSPACE_HISTORY_LIMIT);
        self.persist();
        task
    }

    pub(super) fn cancel_workspace_preview(&mut self) {
        self.close_workspace_preview();
        self.workspace_history
            .finish(self.active_workspace_destination());
        self.workspace_history
            .truncate_recent(WORKSPACE_HISTORY_LIMIT);
    }

    /// Ends the hold. The pane brought up for the preview is put back by
    /// [`Self::settle_workspace_switcher_preview`] once the update is over,
    /// by which time a chosen workspace shows it, if it is on screen there.
    fn close_workspace_preview(&mut self) -> Option<SidebarAction> {
        self.workspace_cycle_modifiers = keyboard::Modifiers::empty();
        let state = &mut self.chrome_ui;
        state.workspace_preview_since = None;
        state.workspace_switcher_shown = false;
        state.workspace_switcher_scroll = 0.0;
        state.workspace_switcher_rows.clear();
        state.workspace_switcher_panes = Panes::default();
        state.workspace_switcher_rested = None;
        state.workspace_switcher_moved = None;
        state.workspace_preview.take()
    }

    /// Keys while the switcher is open. It owns the keyboard: a key that
    /// steers or dismisses it must not also reach the pane beneath, where
    /// Escape interrupts an agent and the arrows move through a review.
    pub(super) fn workspace_switcher_key(&mut self, event: &keyboard::Event) -> Task<Message> {
        use keyboard::key::Named;
        let keyboard::Event::KeyPressed {
            key,
            modifiers,
            physical_key,
            ..
        } = event
        else {
            return Task::none();
        };
        if let Some(index) = digit(key, physical_key) {
            // A number opens the row it labels, as a click would.
            return self.open_workspace_row(index);
        }
        match key {
            keyboard::Key::Named(Named::Escape) => {
                self.cancel_workspace_preview();
                Task::none()
            }
            keyboard::Key::Named(Named::Enter) => self.commit_workspace_preview(),
            keyboard::Key::Named(Named::Tab) => self.cycle_workspace(
                if modifiers.shift() {
                    crate::workspace::FocusDirection::Left
                } else {
                    crate::workspace::FocusDirection::Right
                },
                self.workspace_cycle_modifiers,
            ),
            keyboard::Key::Named(Named::Home) => self.select_workspace_preview(0),
            keyboard::Key::Named(Named::End) => self.select_workspace_preview(
                self.chrome_ui
                    .workspace_switcher_rows
                    .len()
                    .saturating_sub(1),
            ),
            keyboard::Key::Named(Named::ArrowDown | Named::ArrowRight) => self.cycle_workspace(
                crate::workspace::FocusDirection::Right,
                self.workspace_cycle_modifiers,
            ),
            keyboard::Key::Named(Named::ArrowUp | Named::ArrowLeft) => self.cycle_workspace(
                crate::workspace::FocusDirection::Left,
                self.workspace_cycle_modifiers,
            ),
            _ => match workspace_widget::global_shortcut(
                event,
                &self.settings.keyboard_shortcuts,
                self.focused_pane(),
            ) {
                Some(workspace_widget::GlobalShortcut::CycleWorkspace(direction)) => {
                    self.cycle_workspace(direction, *modifiers)
                }
                _ => Task::none(),
            },
        }
    }

    /// A row for each workspace the switcher can name, in the sidebar's
    /// order. Only these take part in the hold, so the rows and the frozen
    /// order index the same destinations.
    fn build_workspace_switcher_rows(&self, actions: Vec<SidebarAction>) -> Vec<Row> {
        let mut seen = HashSet::new();
        let worktrees: HashMap<_, _> = self
            .sidebar_model
            .projects
            .iter()
            .flat_map(|project| {
                project
                    .worktrees
                    .iter()
                    .map(move |worktree| (worktree.id, (project, worktree)))
            })
            .collect();
        let channels: HashMap<_, _> = self
            .sidebar_model
            .projects
            .iter()
            .flat_map(|project| {
                project
                    .channels
                    .iter()
                    .map(move |channel| (channel.id, (project, channel)))
            })
            .collect();
        actions
            .into_iter()
            .filter_map(|action| {
                let destination = WorkspaceDestination::from_action(&action)?;
                if !seen.insert(destination) {
                    return None;
                }
                let (project, name, place) = match &action {
                    SidebarAction::OpenRepository {
                        project_id,
                        repository,
                    } => {
                        let project = self.sidebar_model.project(*project_id);
                        (
                            project.map(|project| project.id),
                            project.map_or_else(
                                || display_path(repository).into_owned(),
                                |project| project.name.clone(),
                            ),
                            display_path(repository).into_owned(),
                        )
                    }
                    SidebarAction::OpenWorktree { worktree_id, .. } => {
                        let (project, worktree) = worktrees.get(worktree_id)?;
                        (
                            Some(project.id),
                            worktree.label().to_owned(),
                            project.name.clone(),
                        )
                    }
                    SidebarAction::OpenChannel { channel_id } => {
                        let (project, channel) = channels.get(channel_id)?;
                        let branch = self.channel_branch(*channel_id);
                        (
                            Some(project.id),
                            channel.title.clone(),
                            branch.map_or_else(
                                || project.name.clone(),
                                |branch| format!("{} · {branch}", project.name),
                            ),
                        )
                    }
                    _ => return None,
                };
                Some(Row {
                    destination,
                    action,
                    project,
                    name,
                    place,
                })
            })
            .collect()
    }

    /// The branch a channel works on: that of the worktree its focused pane
    /// runs in.
    fn channel_branch(&self, channel: crate::model::ChannelId) -> Option<&str> {
        let channel = self
            .durable
            .channels
            .iter()
            .find(|candidate| candidate.id == channel)?;
        let card = channel.focused.or_else(|| channel.cards.first().copied())?;
        let directory = worktree_ui::card_directory(self.card_state(card)?)?;
        let worktrees = || {
            self.sidebar_model
                .projects
                .iter()
                .flat_map(|project| &project.worktrees)
        };
        let root = worktree_ui::worktree_root(
            directory,
            worktrees().map(|worktree| worktree.path.as_path()),
        )?;
        worktrees()
            .find(|worktree| worktree.path == root)
            .map(sidebar::SidebarWorktree::label)
    }

    /// Scrolls the switcher's list just far enough to show the selected row.
    pub(super) fn reveal_workspace_selection(&mut self) -> Task<Message> {
        let selected = self.workspace_switcher_selection();
        let Some(index) = self.workspace_history.cycle_index() else {
            return Task::none();
        };
        if self
            .chrome_ui
            .workspace_switcher_rows
            .get(index)
            .map(|row| row.destination)
            != selected
        {
            return Task::none();
        }
        let top = LIST_PADDING + index as f32 * (ROW_HEIGHT + ROW_GAP);
        let bottom = top + ROW_HEIGHT + LIST_PADDING;
        let list = self.workspace_switcher_geometry().list_height + 2.0 * LIST_PADDING;
        let scroll = &mut self.chrome_ui.workspace_switcher_scroll;
        *scroll = scroll.max(bottom - list).min(top - LIST_PADDING).max(0.0);
        iced::widget::operation::scroll_to(
            widget::Id::new(SCROLL_ID),
            iced::widget::operation::AbsoluteOffset { x: 0.0, y: *scroll },
        )
    }

    fn workspace_switcher_geometry(&self) -> Geometry {
        Geometry::new(
            iced::Size::new(self.window_width, self.window_height),
            self.chrome_ui.workspace_switcher_rows.len(),
        )
    }

    /// Whether row `index` lies wholly inside the list's scrolled view.
    #[cfg(test)]
    pub(super) fn workspace_switcher_shows_row(&self, index: usize) -> bool {
        let top = LIST_PADDING + index as f32 * (ROW_HEIGHT + ROW_GAP);
        let scroll = self.chrome_ui.workspace_switcher_scroll;
        let view = self.workspace_switcher_geometry().list_height + 2.0 * LIST_PADDING;
        top >= scroll && top + ROW_HEIGHT <= scroll + view
    }

    pub(super) fn workspace_switcher_selection(&self) -> Option<WorkspaceDestination> {
        self.chrome_ui
            .workspace_preview
            .as_ref()
            .and_then(WorkspaceDestination::from_action)
    }

    /// Record only actual navigation at the workspace's save boundary.
    /// Frames and preview steps never scan the project model or become visits.
    pub(super) fn capture_workspace_history(&mut self) {
        if self.chrome_ui.workspace_preview.is_none() {
            self.workspace_history
                .observe(self.active_workspace_destination());
        }
        self.workspace_history
            .truncate_recent(WORKSPACE_HISTORY_LIMIT);
        let history = self.workspace_history.recent().to_vec();
        if self.durable.workspace_history != history {
            Arc::make_mut(&mut self.durable).workspace_history = history;
        }
    }

    pub(super) fn select_workspace_preview(&mut self, index: usize) -> Task<Message> {
        if !self.select_workspace_row(index) {
            return Task::none();
        }
        Task::batch([
            self.workspace_selection_moved(),
            self.reveal_workspace_selection(),
        ])
    }

    /// Opens row `index` of the hold, as a click or its number does. Nothing
    /// is opened if the row cannot be selected.
    pub(super) fn open_workspace_row(&mut self, index: usize) -> Task<Message> {
        if !self.select_workspace_row(index) {
            return Task::none();
        }
        self.commit_workspace_preview()
    }

    fn select_workspace_row(&mut self, index: usize) -> bool {
        let Some(row) = self.chrome_ui.workspace_switcher_rows.get(index) else {
            return false;
        };
        let action = row.action.clone();
        let destination = row.destination;
        if self.workspace_history.select_index(index) != Some(destination) {
            return false;
        }
        self.chrome_ui.workspace_preview = Some(action);
        true
    }

    pub(super) fn next_workspace(
        &mut self,
        direction: crate::workspace::FocusDirection,
    ) -> Option<SidebarAction> {
        let backwards = direction == crate::workspace::FocusDirection::Left;
        if self.workspace_history.cycle_order().is_some() {
            self.workspace_history.advance(backwards)?;
        } else {
            let rows = self.build_workspace_switcher_rows(sidebar::workspace_cycle(
                &self.sidebar_model,
                &self.sidebar_state,
            ));
            let available: Vec<_> = rows.iter().map(|row| row.destination).collect();
            let live: HashSet<_> = available.iter().copied().collect();
            self.workspace_history.retain(|id| live.contains(&id));
            self.workspace_history.step(
                &available,
                self.active_workspace_destination(),
                backwards,
            )?;
            // The order holds each available destination once: recently
            // used first, then the rest in the sidebar's order.
            let mut rows: HashMap<_, _> =
                rows.into_iter().map(|row| (row.destination, row)).collect();
            self.chrome_ui.workspace_switcher_rows = self
                .workspace_history
                .cycle_order()?
                .iter()
                .filter_map(|destination| rows.remove(destination))
                .collect();
        }
        self.chrome_ui
            .workspace_switcher_rows
            .get(self.workspace_history.cycle_index()?)
            .map(|row| row.action.clone())
    }

    /// The selection moved while the switcher shows: read the panes of the
    /// workspace now highlighted, and bring its focused pane up for the
    /// preview once the selection has rested there for [`PREVIEW_DWELL`].
    ///
    /// One timer covers a run of steps, so stepping quickly wakes the window
    /// no more often than the dwell.
    fn workspace_selection_moved(&mut self) -> Task<Message> {
        let Some(destination) = self.workspace_switcher_selection() else {
            return Task::none();
        };
        if self.chrome_ui.workspace_switcher_panes.destination != Some(destination) {
            self.chrome_ui.workspace_switcher_panes = self.workspace_panes(destination);
        }
        if self.chrome_ui.workspace_switcher_rested == Some(destination) {
            return Task::none();
        }
        self.chrome_ui.workspace_switcher_rested = None;
        if self
            .chrome_ui
            .workspace_switcher_moved
            .replace(Instant::now())
            .is_some()
        {
            return Task::none();
        }
        self.workspace_switcher_rest_timer(PREVIEW_DWELL)
    }

    fn workspace_switcher_rest_timer(&self, delay: Duration) -> Task<Message> {
        let since = self.chrome_ui.workspace_preview_since;
        cosmic::task::future(async move {
            tokio::time::sleep(delay).await;
            Message::WorkspaceSwitcherRested(since)
        })
    }

    pub(super) fn workspace_switcher_rested(&mut self, since: Option<Instant>) -> Task<Message> {
        if since.is_none() || self.chrome_ui.workspace_preview_since != since {
            return Task::none();
        }
        let Some(moved) = self.chrome_ui.workspace_switcher_moved else {
            return Task::none();
        };
        let elapsed = moved.elapsed();
        if elapsed < PREVIEW_DWELL {
            return self.workspace_switcher_rest_timer(PREVIEW_DWELL - elapsed);
        }
        self.chrome_ui.workspace_switcher_moved = None;
        self.chrome_ui.workspace_switcher_rested = self.workspace_switcher_selection();
        Task::none()
    }

    /// The switcher is drawn for the first time. The selection has rested
    /// for as long as the hold took to show it.
    pub(super) fn show_workspace_switcher(&mut self) -> Task<Message> {
        self.chrome_ui.workspace_switcher_shown = true;
        if let Some(destination) = self.workspace_switcher_selection() {
            self.chrome_ui.workspace_switcher_panes = self.workspace_panes(destination);
            self.chrome_ui.workspace_switcher_rested = Some(destination);
        }
        self.reveal_workspace_selection()
    }

    /// The pane `destination` would focus.
    fn workspace_panes(&self, destination: WorkspaceDestination) -> Panes {
        if Some(destination) == self.active_workspace_destination() {
            return Panes {
                destination: Some(destination),
                focused: self.workspace.focused(),
                channel: None,
            };
        }
        let open = |id: &CardId| {
            self.card_state(*id)
                .is_some_and(|card| card.existence.is_open())
        };
        let mut channeled = Vec::new();
        let (cards, preferred): (Vec<CardId>, Option<CardId>) = match destination {
            WorkspaceDestination::Channel(id) => {
                let Some(channel) = self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == id)
                else {
                    return Panes::default();
                };
                (
                    channel.cards.iter().copied().filter(open).collect(),
                    channel.focused,
                )
            }
            WorkspaceDestination::Worktree(_) | WorkspaceDestination::Folder(_) => {
                let Some(scope) = self
                    .chrome_ui
                    .workspace_switcher_rows
                    .iter()
                    .find(|row| row.destination == destination)
                    .and_then(|row| self.sidebar_entered_directory(&row.action))
                else {
                    return Panes::default();
                };
                let in_channels: HashSet<_> = self
                    .durable
                    .channels
                    .iter()
                    .flat_map(|channel| channel.cards.iter().copied())
                    .collect();
                let roots = self
                    .known_worktree_roots
                    .iter()
                    .map(PathBuf::as_path)
                    .collect::<Vec<_>>();
                let (own, elsewhere): (Vec<CardId>, Vec<CardId>) = self
                    .durable
                    .cards
                    .iter()
                    .filter(|card| {
                        card.existence.is_open()
                            && worktree_ui::belongs_to_scope(
                                card,
                                &scope,
                                &roots,
                                &self.worktree_owners,
                            )
                    })
                    .map(|card| card.id)
                    .partition(|card| !in_channels.contains(card));
                channeled = elsewhere;
                (own, self.worktree_focus.get(&scope).copied())
            }
        };
        // With no panes of its own, a worktree whose status counts agents
        // working in it from channels shows one of those at work.
        if let Some(focused) = worktree_ui::scope_focus(
            cards.iter().filter_map(|id| self.card_state(*id)),
            preferred,
        ) {
            return Panes {
                destination: Some(destination),
                focused: Some(focused),
                channel: None,
            };
        }
        let focused = channeled
            .iter()
            .copied()
            .find(|&card| self.sidebar_state.card_attention(card).state().is_some())
            .or_else(|| channeled.first().copied());
        Panes {
            destination: Some(destination),
            focused,
            channel: focused.and_then(|card| {
                self.durable
                    .channels
                    .iter()
                    .find(|channel| channel.cards.contains(&card))
                    .map(|channel| channel.id)
            }),
        }
    }

    /// The pane the switcher shows live: the highlighted workspace's focused
    /// pane, once the selection rests there. The open workspace's panes are
    /// on screen behind the switcher already, and a widget mounted twice
    /// would have both copies claim the same scroll and find anchors.
    fn wanted_workspace_preview_card(&self) -> Option<CardId> {
        if !self.chrome_ui.workspace_switcher_shown || !self.workspace_switcher_geometry().detail {
            return None;
        }
        let selected = self.workspace_switcher_selection()?;
        if self.chrome_ui.workspace_switcher_rested != Some(selected)
            || Some(selected) == self.active_workspace_destination()
        {
            return None;
        }
        let panes = &self.chrome_ui.workspace_switcher_panes;
        // A pane the open workspace also shows is drawn there already.
        let card = panes
            .focused
            .filter(|_| panes.destination == Some(selected))
            .filter(|&card| self.workspace.visibility(card).is_none())?;
        matches!(
            self.cards.get(&card),
            Some(CardRuntime::Chat(_) | CardRuntime::Terminal(_))
        )
        .then_some(card)
    }

    /// Brings the pane the switcher previews up, and puts back the one it no
    /// longer does. Runs after every update, so whatever closed the switcher
    /// or moved its selection, the pane follows; a workspace opened from it
    /// has shown its panes by then, and a previewed pane on screen there is
    /// left as it is.
    pub(super) fn settle_workspace_switcher_preview(&mut self) -> Task<Message> {
        let wanted = self.wanted_workspace_preview_card();
        let current = self.chrome_ui.workspace_switcher_preview;
        if wanted == current {
            return Task::none();
        }
        if let Some(card) = current {
            let on_screen = self.workspace.visibility(card) == Some(VisibilityClass::Visible)
                && self.workspace_surfaces_visible();
            if !on_screen {
                self.set_exposure(
                    card,
                    if self.workspace.visibility(card) == Some(VisibilityClass::Overscan) {
                        Exposure::Prefetch
                    } else {
                        Exposure::Offscreen
                    },
                );
                match self.cards.get_mut(&card) {
                    Some(CardRuntime::Terminal(handle)) => {
                        let _ = handle.hide();
                    }
                    Some(CardRuntime::Chat(runtime)) => {
                        let _ = runtime.set_visible(false);
                    }
                    _ => {}
                }
            }
        }
        if let Some(card) = wanted {
            let hot = matches!(self.residency(card), Residency::Hot);
            self.set_exposure(card, Exposure::Visible);
            match self.cards.get_mut(&card) {
                // A terminal that is not hot is restored, visible, by the
                // sweep below, which counts this card as on screen.
                Some(CardRuntime::Terminal(handle)) if hot => {
                    let _ = handle.reveal();
                }
                Some(CardRuntime::Chat(runtime)) => {
                    let _ = runtime.set_visible(true);
                }
                _ => {}
            }
        }
        self.chrome_ui.workspace_switcher_preview = wanted;
        self.trim_warm_chats();
        self.lifecycle_sweep()
    }

    pub(super) fn workspace_switcher(&self) -> Option<Element<'_, Message>> {
        let selected = self.workspace_switcher_selection()?;
        if !self.chrome_ui.workspace_switcher_shown {
            // Not drawn yet, but already holding the keyboard.
            return Some(crate::interaction::keyboard_modal(
                iced::widget::Space::new(),
            ));
        }
        let since = self.chrome_ui.workspace_preview_since?;
        let geometry = self.workspace_switcher_geometry();
        let active = self.active_workspace_destination();
        let rows = self
            .chrome_ui
            .workspace_switcher_rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                self.workspace_switcher_row(index, row, row.destination == selected, since)
            })
            .collect::<Vec<Element<'_, Message>>>();
        let list = widget::scrollable(
            widget::column::with_children(rows)
                .spacing(ROW_GAP)
                .padding(design::with_vertical_rail(iced::Padding::new(LIST_PADDING))),
        )
        .id(widget::Id::new(SCROLL_ID))
        .direction(iced::widget::scrollable::Direction::Vertical(
            design::scrollbar(),
        ))
        .class(cosmic::style::iced::Scrollable::Minimal)
        // Where the wheel left it, so the next step scrolls from there.
        .on_scroll(move |viewport| Message::WorkspaceSwitcherScrolled {
            since,
            offset: viewport.absolute_offset().y,
        })
        .height(Length::Fill);
        let list = widget::container(list)
            .width(if geometry.detail {
                Length::Fixed(LIST_WIDTH)
            } else {
                Length::Fill
            })
            .height(Length::Fill);
        let body: Element<'_, Message> = if geometry.detail {
            widget::row::with_children(vec![
                list.into(),
                rule(Length::Fixed(design::BORDER_WIDTH), Length::Fill),
                self.workspace_switcher_detail(selected, active),
            ])
            .height(Length::Fill)
            .into()
        } else {
            list.into()
        };
        let footer = widget::container(crate::interaction::fade_overflow(
            widget::row::with_children(vec![
                keycap("Tab", "next"),
                keycap("Shift Tab", "previous"),
                keycap("1–9", "open that row"),
                keycap("Release", "open"),
                keycap("Esc", "cancel"),
            ])
            .spacing(space::L)
            .align_y(Alignment::Center),
            panel_color,
        ))
        .padding([0.0, space::M])
        .height(FOOTER_HEIGHT - design::BORDER_WIDTH)
        .width(Length::Fill)
        .align_y(Alignment::Center);
        let panel = widget::container(
            widget::column::with_children(vec![
                widget::container(body).height(Length::Fill).into(),
                rule(Length::Fill, Length::Fixed(design::BORDER_WIDTH)),
                footer.into(),
            ])
            .width(Length::Fixed(geometry.width))
            .height(Length::Fixed(geometry.height)),
        )
        .class(design::elevated());
        let scrim = crate::interaction::opaque(
            widget::mouse_area(
                widget::container(iced::widget::Space::new())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .class(cosmic::theme::Container::custom(|theme| {
                        widget::container::Style {
                            background: Some(iced::Background::Color(design::modal_scrim(theme))),
                            ..Default::default()
                        }
                    })),
            )
            .on_press(Message::WorkspaceSwitcherDismissed(since)),
        );
        let centered = widget::container(crate::interaction::opaque(panel))
            .center_x(Length::Fill)
            .center_y(Length::Fill);
        Some(crate::interaction::keyboard_modal(
            crate::interaction::layer(scrim, centered.into()),
        ))
    }

    fn workspace_switcher_row<'a>(
        &'a self,
        index: usize,
        row: &'a Row,
        selected: bool,
        since: Instant,
    ) -> Element<'a, Message> {
        let project = row
            .project
            .and_then(|project| self.sidebar_model.project(project));
        let chip = project.map_or_else(
            || {
                widget::icon::from_name("folder-symbolic")
                    .size(design::ROW_ICON_SIZE)
                    .into()
            },
            sidebar::project_chip,
        );
        let fade = move |theme: &Theme| row_color(theme, selected);
        let text = widget::column::with_children(vec![
            crate::interaction::fade_overflow(
                design::body(row.name.as_str()).wrapping(iced::widget::text::Wrapping::None),
                fade,
            ),
            crate::interaction::fade_overflow(
                design::caption(row.place.as_str()).wrapping(iced::widget::text::Wrapping::None),
                fade,
            ),
        ])
        .spacing(space::XXS)
        .width(Length::Fill);
        let attention = self.destination_attention(row);
        let mut children = vec![row_number(index, selected), chip, text.into()];
        children.extend(sidebar::attention_indicator(attention));
        let content = widget::container(
            widget::row::with_children(children)
                .spacing(space::S)
                .align_y(Alignment::Center),
        )
        .padding([0.0, space::S])
        .height(ROW_HEIGHT)
        .width(Length::Fill)
        .align_y(Alignment::Center);
        let status = attention.description();
        let button = widget::button::custom(content)
            .name(format!(
                "{} workspace: {}, {}{}{}",
                if selected { "Selected" } else { "Open" },
                row.name,
                row.place,
                if status.is_empty() { "" } else { ", " },
                status,
            ))
            .selected(selected)
            .class(sidebar::navigation_row_style(selected))
            .padding(0)
            .width(Length::Fill)
            .on_press(Message::WorkspaceSwitcherActivate { since, index });
        widget::mouse_area(button)
            .on_enter(Message::WorkspaceSwitcherSelected { since, index })
            .into()
    }

    /// What needs attention in a row's workspace, as its sidebar row shows
    /// it. Looked up through the row's project, for a view that asks once
    /// per row.
    fn destination_attention(&self, row: &Row) -> AttentionSummary {
        match (row.destination, row.project) {
            (WorkspaceDestination::Worktree(worktree), Some(project)) => {
                self.sidebar_state.worktree_attention(project, worktree)
            }
            (WorkspaceDestination::Folder(project), _) => {
                self.sidebar_state.project_attention(project)
            }
            (WorkspaceDestination::Channel(channel), Some(project)) => self
                .sidebar_model
                .project(project)
                .and_then(|project| {
                    project
                        .channels
                        .iter()
                        .find(|candidate| candidate.id == channel)
                })
                .map(|channel| channel.attention)
                .unwrap_or_default(),
            _ => AttentionSummary::default(),
        }
    }

    /// The highlighted workspace as it is now: its focused pane, live and
    /// edge to edge. The row beside it names the workspace already, so the
    /// only words here are ones the row lacks: what the pane is doing.
    fn workspace_switcher_detail(
        &self,
        selected: WorkspaceDestination,
        active: Option<WorkspaceDestination>,
    ) -> Element<'_, Message> {
        let panes = &self.chrome_ui.workspace_switcher_panes;
        if panes.destination != Some(selected) {
            return iced::widget::Space::new().into();
        }
        let notice = |text: &'static str| -> Element<'_, Message> {
            widget::container(design::caption(text))
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into()
        };
        if Some(selected) == active {
            // Its panes are behind the switcher, as they will be on release.
            return notice("Current workspace");
        }
        let Some(card) = panes.focused else {
            return notice("Nothing open yet");
        };
        let live = self.chrome_ui.workspace_switcher_preview == Some(card);
        let body: Option<Element<'_, Message>> = match self.cards.get(&card).filter(|_| live) {
            Some(CardRuntime::Chat(runtime)) => runtime.transcript_preview().map(|transcript| {
                widget::container(crate::interaction::inert(transcript))
                    .padding([space::XS, space::M])
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_y(Alignment::End)
                    .clip(true)
                    .into()
            }),
            Some(CardRuntime::Terminal(handle)) => Some(
                widget::container(
                    crate::terminal::SnapshotPreview::new(handle)
                        .font_size(TERMINAL_PREVIEW_FONT)
                        .font_family(&self.settings.terminal_font_family)
                        .scale_factor(effective_surface_scale(
                            self.window_scale_factor,
                            self.settings.ui_scale,
                        ))
                        .padding(space::S),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .clip(true)
                .into(),
            ),
            _ => None,
        };
        let Some(body) = body else {
            // Until the pane is up, and for one with nothing to draw, it is
            // named instead.
            return widget::container(self.workspace_switcher_pane_heading(card))
                .padding(space::M)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into();
        };
        let mut children = Vec::with_capacity(3);
        // A conversation ends with what its agent is doing. A terminal's
        // screen does not say what it runs, and neither says that the pane is
        // a channel's, which the workspace only shows.
        let channel = panes.channel.and_then(|channel| {
            self.chrome_ui
                .workspace_switcher_rows
                .iter()
                .find(|row| row.destination == WorkspaceDestination::Channel(channel))
                .map(|row| row.name.as_str())
        });
        let caption = match (self.terminal_running(card), channel) {
            (Some(running), Some(channel)) => Some(format!("{running} · from {channel}")),
            (Some(running), None) => Some(running),
            (None, Some(channel)) => Some(format!("From {channel}")),
            (None, None) => None,
        };
        if let Some(caption) = caption {
            children.push(
                widget::container(crate::interaction::fade_overflow(
                    design::caption(caption).wrapping(iced::widget::text::Wrapping::None),
                    panel_color,
                ))
                .padding([space::XS, space::M])
                .width(Length::Fill)
                .into(),
            );
            children.push(rule(Length::Fill, Length::Fixed(design::BORDER_WIDTH)));
        }
        children.push(body);
        widget::column::with_children(children)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// What pane `card` is doing, when that is known: an agent's activity, or
    /// what a terminal runs.
    fn workspace_switcher_pane_doing(&self, card: CardId) -> Option<String> {
        match self.cards.get(&card)? {
            CardRuntime::Chat(runtime) => runtime
                .timeline_activity(false, None)
                .map(|(_, label)| label),
            CardRuntime::Terminal(_) => self.terminal_running(card),
            _ => None,
        }
    }

    /// A pane that is not drawn live: its icon, name and what it is doing.
    fn workspace_switcher_pane_heading(&self, card: CardId) -> Element<'_, Message> {
        let state = self.card_state(card);
        let title = self.card_chrome.get(&card).map_or_else(
            || state.map_or("Pane", |state| self.card_display_title(state)),
            |chrome| chrome.title.as_str(),
        );
        let mut text = vec![design::body(title).into()];
        if let Some(doing) = self.workspace_switcher_pane_doing(card) {
            text.push(design::caption(doing).into());
        }
        widget::row::with_children(vec![
            self.card_icon(card)
                .icon()
                .size(design::ROW_ICON_SIZE)
                .into(),
            widget::column::with_children(text)
                .spacing(space::XXS)
                .into(),
        ])
        .spacing(space::S)
        .align_y(Alignment::Center)
        .into()
    }

    /// What terminal `card` is running, as its title reports it: the title
    /// its program set, or else its foreground command and directory.
    fn terminal_running(&self, card: CardId) -> Option<String> {
        let state = self.card_state(card)?;
        let CardKind::Terminal(session) = &state.kind else {
            return None;
        };
        let reported = crate::agent::codex_title_without_status(&state.title).trim();
        if !reported.is_empty() {
            return Some(reported.to_owned());
        }
        // Not reported yet: the program it was started with.
        Path::new(&session.launch.program)
            .file_name()
            .map(|program| program.to_string_lossy().into_owned())
    }
}

/// The row a number key opens, counting from one at the top of the list.
fn digit(key: &keyboard::Key, physical: &keyboard::key::Physical) -> Option<usize> {
    use keyboard::key::Code;
    let from_key = match key {
        keyboard::Key::Character(text) => text
            .chars()
            .next()
            .filter(|_| text.chars().count() == 1)
            .and_then(|ch| ch.to_digit(10)),
        _ => None,
    };
    let from_code = || match physical {
        keyboard::key::Physical::Code(code) => match code {
            Code::Digit1 | Code::Numpad1 => Some(1),
            Code::Digit2 | Code::Numpad2 => Some(2),
            Code::Digit3 | Code::Numpad3 => Some(3),
            Code::Digit4 | Code::Numpad4 => Some(4),
            Code::Digit5 | Code::Numpad5 => Some(5),
            Code::Digit6 | Code::Numpad6 => Some(6),
            Code::Digit7 | Code::Numpad7 => Some(7),
            Code::Digit8 | Code::Numpad8 => Some(8),
            Code::Digit9 | Code::Numpad9 => Some(9),
            _ => None,
        },
        _ => None,
    };
    // The physical key stands in only for a character, so a digit typed
    // with Shift or on another layout still counts, while the numpad's
    // Home, End and arrows with Num Lock off keep steering.
    let digit = match key {
        keyboard::Key::Character(_) => from_key.or_else(from_code)?,
        _ => from_key?,
    };
    (1..=DIGITS as u32)
        .contains(&digit)
        .then(|| digit as usize - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_follows_the_window_and_drops_its_preview_when_narrow() {
        let wide = Geometry::new(iced::Size::new(1_600.0, 1_000.0), 40);
        assert_eq!(wide.width, PANEL_MAX_WIDTH);
        assert_eq!(wide.height, PANEL_MAX_HEIGHT);
        assert!(wide.detail);
        let short = Geometry::new(iced::Size::new(1_600.0, 500.0), 40);
        assert_eq!(short.height, 500.0 - 2.0 * WINDOW_MARGIN);
        assert!(short.list_height < short.height);
        let narrow = Geometry::new(iced::Size::new(560.0, 1_000.0), 3);
        assert!(!narrow.detail);
        assert_eq!(narrow.width, 560.0 - 2.0 * space::XL);
        // Without a preview, a short list is as tall as its rows.
        assert_eq!(
            narrow.height,
            rows_height(3) + 2.0 * LIST_PADDING + FOOTER_HEIGHT
        );
        let tiny = Geometry::new(iced::Size::new(300.0, 200.0), 40);
        assert!(tiny.height <= 200.0);
    }

    #[test]
    fn number_keys_name_rows_from_one() {
        use keyboard::key::{Code, Physical};
        let key = |text: &str| keyboard::Key::Character(text.into());
        assert_eq!(digit(&key("1"), &Physical::Code(Code::Digit1)), Some(0));
        assert_eq!(digit(&key("9"), &Physical::Code(Code::Digit9)), Some(8));
        assert_eq!(digit(&key("0"), &Physical::Code(Code::Digit0)), None);
        // A layout that types something else on the number row still opens
        // the row by the key's position.
        assert_eq!(digit(&key("&"), &Physical::Code(Code::Digit1)), Some(0));
        assert_eq!(digit(&key("a"), &Physical::Code(Code::KeyA)), None);
        // The numpad's End with Num Lock off steers rather than opening row 1.
        assert_eq!(
            digit(
                &keyboard::Key::Named(keyboard::key::Named::End),
                &Physical::Code(Code::Numpad1)
            ),
            None
        );
    }
}
