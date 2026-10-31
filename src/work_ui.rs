//! The Work pane: the daemon's board of every worktree and branch in
//! progress, as columns of cards or as one list newest first, for every
//! project or for one.
//!
//! The daemon owns the board and pushes each changed one; this state only
//! filters and presents it. A pin is shown at once and confirmed by the next
//! board, so a card moves the moment it is moved.
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use cosmic::{
    Element, Theme,
    iced::{self, Alignment, Length, Padding},
    widget,
};

use crate::{
    agent::ActivityState,
    design::{self, Tone, space},
    model::TerminalId,
    ui_controls::{Icon, badge, icon_button, placeholder, popover, popover_item},
    work::{
        ActivitySource, Board, CheckOutcome, Column, Forge, Item, ItemKey, PullRequestState, Review,
    },
};

mod removal;
pub use removal::{
    Action as RemovalAction, BackendTarget, Effect as RemovalEffect, PreparedPlan,
    RemovalGeneration, RemovalOutcome, RemovalSubject,
};

/// A semantic color, read from the drawing theme.
type Toned = fn(&Theme) -> iced::Color;

/// The narrowest a board column may be before the board scrolls sideways.
const COLUMN_MIN_WIDTH: f32 = 248.0;
/// A column's width when the board scrolls sideways.
const COLUMN_WIDTH: f32 = 280.0;
const SCOPE_MENU_WIDTH: f32 = 280.0;
const CARD_MENU_WIDTH: f32 = 264.0;

/// One card on the board, by what identifies it to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CardRef {
    pub project: PathBuf,
    pub key: ItemKey,
}

impl CardRef {
    fn of(item: &Item) -> Self {
        Self {
            project: item.project.clone(),
            key: item.key.clone(),
        }
    }

    fn names(&self, item: &Item) -> bool {
        item.project == self.project && item.key == self.key
    }
}

/// Local callback ownership, not a backend ordering or mutation permit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinTicket {
    lifetime: uuid::Uuid,
    operation: uuid::Uuid,
    card: CardRef,
}

#[derive(Debug)]
struct Pins {
    lifetime: uuid::Uuid,
    latest: HashMap<CardRef, uuid::Uuid>,
    error: Option<String>,
}

impl Default for Pins {
    fn default() -> Self {
        Self {
            lifetime: uuid::Uuid::new_v4(),
            latest: HashMap::new(),
            error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Layout {
    /// Columns of cards, one per [`Column`].
    #[default]
    Board,
    /// Every card in one list, newest activity first.
    List,
}

/// What the board subscription reports.
#[derive(Debug, Clone)]
pub enum Watch {
    /// A board, and whether it is the first of its connection, whose
    /// revisions start where that daemon's do.
    Board {
        revision: u64,
        board: Arc<Board>,
        first: bool,
    },
    /// The subscription failed and is being retried.
    Lost(String),
}

#[derive(Debug, Clone)]
pub enum Action {
    Watched(Watch),
    /// The clock moved: relative times are redrawn.
    Tick,
    ScopeMenu(bool),
    /// Shows one project, by repository, or all of them.
    Scope(Option<PathBuf>),
    Query(String),
    Layout(Layout),
    /// Rescans every project and polls GitHub now.
    Refresh,
    Refreshed(Result<(), String>),
    /// The exact rendered destination, checked again against the current card.
    Activate(Activation),
    /// A rendered removal entry point, owned by this pane and watch.
    ActivateRemoval(RemovalActivation),
    Menu(Option<CardRef>),
    /// Pins a card to a column, or unpins it with `None`.
    Pin(CardRef, Option<Column>),
    Pinned {
        ticket: PinTicket,
        result: Result<(), String>,
    },
    Removal(Box<RemovalAction>),
}

impl Action {
    /// Whether the action arrives without the user doing anything, so it
    /// must not count as interaction with the window.
    pub fn background(&self) -> bool {
        matches!(
            self,
            Self::Watched(_) | Self::Tick | Self::Refreshed(_) | Self::Pinned { .. }
        ) || matches!(self, Self::Removal(action) if action.background())
    }
}

impl From<RemovalAction> for Action {
    fn from(action: RemovalAction) -> Self {
        Self::Removal(Box::new(action))
    }
}

/// A local menu callback, not a removal permit or filesystem identity proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalActivation {
    lifetime: ActivationLifetime,
    watch: uuid::Uuid,
    generation: RemovalGeneration,
    subject: RemovalSubject,
}

/// Immutable semantic ownership shared by queued callbacks and native buttons.
/// Paths identify UI destinations; this is not a filesystem identity proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    lifetime: ActivationLifetime,
    card: CardRef,
    destination: Destination,
    primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActivationLifetime(uuid::Uuid);

impl Default for ActivationLifetime {
    fn default() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Destination {
    Worktree(PathBuf),
    PullRequest { number: u64, url: String },
    Linear(String),
    Session(TerminalId),
    Branch(String),
}

impl Activation {
    fn new(item: &Item, destination: Destination, lifetime: ActivationLifetime) -> Self {
        Self {
            lifetime,
            card: CardRef::of(item),
            destination,
            primary: false,
        }
    }

    fn primary(item: &Item, lifetime: ActivationLifetime) -> Option<Self> {
        let destination = if let Some(worktree) = &item.worktree {
            Destination::Worktree(worktree.path.clone())
        } else {
            let request = item.pull_request.as_ref()?;
            Destination::PullRequest {
                number: request.number,
                url: request.url.clone(),
            }
        };
        Some(Self {
            primary: true,
            ..Self::new(item, destination, lifetime)
        })
    }

    fn matches(&self, item: &Item) -> bool {
        if !self.card.names(item) {
            return false;
        }
        if self.primary {
            return Self::primary(item, self.lifetime).as_ref() == Some(self);
        }
        match &self.destination {
            Destination::Worktree(path) => item.worktree.as_ref().is_some_and(|w| w.path == *path),
            Destination::PullRequest { number, url } => item
                .pull_request
                .as_ref()
                .is_some_and(|request| request.number == *number && request.url == *url),
            Destination::Linear(url) => {
                item.linear.as_ref().and_then(|ticket| ticket.url.as_ref()) == Some(url)
            }
            Destination::Session(id) => item.sessions.iter().any(|session| session.id == *id),
            Destination::Branch(branch) => item.branch.as_ref() == Some(branch),
        }
    }

    fn effect(self) -> Effect {
        match self.destination {
            Destination::Worktree(path) => Effect::OpenWorktree {
                project: self.card.project,
                path,
            },
            Destination::PullRequest { number, url } => Effect::OpenPullRequest {
                project: self.card.project,
                number,
                url,
            },
            Destination::Linear(url) => Effect::OpenUrl(url),
            Destination::Session(id) => Effect::FocusSession(id),
            Destination::Branch(branch) => Effect::Copy(branch),
        }
    }
}

/// What only the application can do for this pane.
#[derive(Debug, Clone)]
pub enum Effect {
    Removal(RemovalEffect),
    Refresh,
    Pin {
        ticket: PinTicket,
        project: PathBuf,
        item: String,
        column: Option<Column>,
    },
    OpenWorktree {
        project: PathBuf,
        path: PathBuf,
    },
    OpenPullRequest {
        project: PathBuf,
        number: u64,
        url: String,
    },
    OpenUrl(String),
    FocusSession(TerminalId),
    Copy(String),
}

#[derive(Debug, Default)]
pub struct State {
    /// Retires queued/native activations when this pane is reset, not on redraw.
    lifetime: ActivationLifetime,
    /// The board shown: the daemon's, with the pins not yet confirmed.
    board: Option<Arc<Board>>,
    /// The daemon's last board, from which one unconfirmed pin is restored.
    confirmed: Option<Arc<Board>>,
    /// Each card's searchable text, lowercase, in board order.
    search: Vec<String>,
    revision: u64,
    /// Why the subscription is down, while it is retried.
    lost: Option<String>,
    scope: Option<PathBuf>,
    scope_menu: bool,
    query: String,
    layout: Layout,
    menu: Option<CardRef>,
    refreshing: bool,
    /// The last refresh that failed, independent of pin feedback.
    error: Option<String>,
    pins: Pins,
    now_unix_ms: u64,
    removal: removal::State,
}

impl State {
    /// Forgets the board when the pane closes, so a reopened pane never
    /// shows a board from before it was closed.
    pub fn reset(&mut self) {
        let scope = self.scope.take();
        let layout = self.layout;
        // Closing a pane retires unsubmitted review input, not an accepted
        // request. Keep its exact host, token, receipt and in-flight generation.
        self.removal.retire_waiting_inspection();
        self.removal.retire_review_input();
        let removal = std::mem::take(&mut self.removal);
        *self = Self {
            scope,
            layout,
            removal,
            ..Self::default()
        };
    }

    pub fn update(&mut self, action: Action) -> Option<Effect> {
        match action {
            Action::ActivateRemoval(activation) => {
                if activation.lifetime != self.lifetime
                    || activation.watch != self.pins.lifetime
                    || activation.generation != self.removal.generation()
                    || self.lost.is_some()
                    || !self.removal.can_open_subject(&activation.subject)
                    || !self
                        .item(&activation.subject.card)
                        .is_some_and(|item| activation.subject.matches(item))
                {
                    return None;
                }
                self.menu = None;
                self.removal
                    .update(
                        RemovalAction::Open {
                            generation: activation.generation,
                            subject: activation.subject,
                        },
                        true,
                    )
                    .map(Effect::Removal)
            }
            Action::Removal(action) => {
                let subject = match action.as_ref() {
                    RemovalAction::Open { subject, .. } => Some(subject),
                    _ => self.removal.subject(),
                };
                let current = self.lost.is_none()
                    && subject.is_some_and(|subject| {
                        self.item(&subject.card)
                            .is_some_and(|item| subject.matches(item))
                    });
                self.menu = None;
                self.removal.update(*action, current).map(Effect::Removal)
            }
            Action::Watched(Watch::Board {
                revision,
                board,
                first,
            }) => {
                // A board pushed while the subscription was answered can
                // arrive before the answer; the newer one stands.
                if first || revision > self.revision {
                    if first {
                        self.removal.retire_waiting_inspection();
                        // A new watch connection owns a fresh callback lifetime.
                        // This neither cancels nor resends earlier server work.
                        self.pins = Pins::default();
                    }
                    if !self.pins.latest.is_empty() {
                        // A removed card can later reuse its branch key. Its
                        // earlier request must not acquire that new lifetime.
                        let present: HashSet<_> = board
                            .items
                            .iter()
                            .map(|item| (&item.project, &item.key))
                            .collect();
                        self.pins
                            .latest
                            .retain(|card, _| present.contains(&(&card.project, &card.key)));
                    }
                    self.revision = revision;
                    self.now_unix_ms = board.generated_at_unix_ms.max(now_unix_ms());
                    self.search = board.items.iter().map(search_text).collect();
                    if self
                        .scope
                        .as_ref()
                        .is_some_and(|scope| !board.projects.iter().any(|p| p.repository == *scope))
                    {
                        // The project closed.
                        self.scope = None;
                    }
                    if self
                        .menu
                        .as_ref()
                        .is_some_and(|card| !board.items.iter().any(|item| card.names(item)))
                    {
                        self.menu = None;
                    }
                    self.confirmed = Some(board.clone());
                    self.board = Some(board);
                    self.lost = None;
                }
                None
            }
            Action::Watched(Watch::Lost(error)) => {
                self.removal.retire_waiting_inspection();
                self.pins = Pins::default();
                self.lost = Some(error);
                None
            }
            Action::Tick => {
                self.now_unix_ms = now_unix_ms();
                None
            }
            Action::ScopeMenu(open) => {
                self.scope_menu = open;
                None
            }
            Action::Scope(scope) => {
                if let Some(repository) = &scope
                    && !self.board.as_ref().is_some_and(|board| {
                        board
                            .projects
                            .iter()
                            .any(|project| project.repository == *repository)
                    })
                {
                    return None;
                }
                self.scope = scope;
                self.scope_menu = false;
                None
            }
            Action::Query(query) => {
                self.query = query;
                None
            }
            Action::Layout(layout) => {
                self.layout = layout;
                None
            }
            Action::Refresh => {
                if self.refreshing {
                    return None;
                }
                self.refreshing = true;
                self.error = None;
                Some(Effect::Refresh)
            }
            Action::Refreshed(result) => {
                self.refreshing = false;
                self.error = result.err();
                None
            }
            Action::Activate(activation) => {
                if activation.lifetime != self.lifetime
                    || !activation.matches(self.item(&activation.card)?)
                {
                    return None;
                }
                self.menu = None;
                Some(activation.effect())
            }
            Action::Menu(menu) => {
                self.menu = menu;
                None
            }
            Action::Pin(card, column) => {
                self.menu = None;
                let board = Arc::make_mut(self.board.as_mut()?);
                let stale_after_days = board.stale_after_days;
                let item = board.items.iter_mut().find(|item| card.names(item))?;
                crate::work::place(item, column, self.now_unix_ms, stale_after_days);
                let ticket = PinTicket {
                    lifetime: self.pins.lifetime,
                    operation: uuid::Uuid::new_v4(),
                    card: card.clone(),
                };
                self.pins.latest.insert(card.clone(), ticket.operation);
                self.pins.error = None;
                Some(Effect::Pin {
                    ticket,
                    project: card.project,
                    item: card.key.storage_key(),
                    column,
                })
            }
            Action::Pinned { ticket, result } => {
                if ticket.lifetime != self.pins.lifetime
                    || self.pins.latest.get(&ticket.card) != Some(&ticket.operation)
                {
                    return None;
                }
                self.pins.latest.remove(&ticket.card);
                if let Err(error) = result {
                    // Transport loss need not mean the daemon changed nothing.
                    // Restore only this card's placement from the latest
                    // observation, preserving other pending pins and new facts.
                    let (column, derived, pinned) = self
                        .confirmed
                        .as_ref()?
                        .items
                        .iter()
                        .find(|item| ticket.card.names(item))
                        .map(|item| (item.column, item.derived, item.pinned))?;
                    let board = Arc::make_mut(self.board.as_mut()?);
                    let item = board
                        .items
                        .iter_mut()
                        .find(|item| ticket.card.names(item))?;
                    item.column = column;
                    item.derived = derived;
                    item.pinned = pinned;
                    self.pins.error =
                        Some(format!("Pin not confirmed for {}: {error}", item.label()));
                }
                None
            }
        }
    }

    fn item(&self, card: &CardRef) -> Option<&Item> {
        self.board
            .as_ref()?
            .items
            .iter()
            .find(|item| card.names(item))
    }

    /// The cards in scope that match the query, newest activity first.
    fn visible(&self) -> impl Iterator<Item = &Item> {
        let query = self.query.trim().to_lowercase();
        self.board
            .iter()
            .flat_map(|board| board.items.iter().zip(&self.search))
            .filter(move |(item, _)| {
                self.scope
                    .as_ref()
                    .is_none_or(|scope| item.project == *scope)
            })
            .filter(move |(_, text)| text.contains(query.as_str()))
            .map(|(item, _)| item)
    }

    pub fn view(&self) -> Element<'_, Action> {
        crate::ui_controls::responsive(move |size| self.layout_view(size.width)).into()
    }

    fn layout_view(&self, width: f32) -> Element<'_, Action> {
        let header = self.header();
        let body = match &self.board {
            None => self.waiting(),
            Some(board) if board.projects.is_empty() => placeholder("No Git projects are open")
                .icon(Icon::Branch.handle())
                .detail(
                    "Open a Git project and its worktrees, branches and pull requests appear here.",
                )
                .into(),
            Some(_) => match self.layout {
                Layout::Board => self.board_view(width),
                Layout::List => self.list_view(),
            },
        };
        let mut children = vec![header, design::horizontal_rule()];
        let current = self.lost.is_none()
            && self.removal.subject().is_some_and(|subject| {
                self.item(&subject.card)
                    .is_some_and(|item| subject.matches(item))
            });
        if let Some(review) = self.removal.view(current, self.lifetime) {
            children.push(review.map(Action::from));
            children.push(design::horizontal_rule());
        }
        children.push(body);
        widget::column::with_children(children)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn waiting(&self) -> Element<'_, Action> {
        match &self.lost {
            Some(error) => placeholder("The Work board is unavailable")
                .icon(Icon::Warning.handle())
                .detail(format!("{error}. Retrying…"))
                .into(),
            None => widget::container(
                widget::row::with_children(vec![
                    crate::ui_controls::activity_spinner(14.0, design::muted_color),
                    design::body("Reading your work…")
                        .class(cosmic::theme::Text::Custom(design::muted_text))
                        .into(),
                ])
                .spacing(space::S)
                .align_y(Alignment::Center),
            )
            .center(Length::Fill)
            .into(),
        }
    }

    fn header(&self) -> Element<'_, Action> {
        let layout = crate::ui_controls::segmented_controls(vec![
            icon_button(Icon::Board, "Board")
                .selected(self.layout == Layout::Board)
                .on_press(Action::Layout(Layout::Board))
                .into(),
            icon_button(Icon::List, "List, newest first")
                .selected(self.layout == Layout::List)
                .on_press(Action::Layout(Layout::List))
                .into(),
        ]);
        let refresh: Element<'_, Action> = if self.refreshing {
            crate::interaction::tooltip(
                widget::container(crate::ui_controls::activity_spinner(
                    14.0,
                    design::muted_color,
                ))
                .center(Length::Fixed(design::PRIMARY_CONTROL_SIZE)),
                "Rescanning projects and polling GitHub…",
            )
        } else {
            icon_button(Icon::Refresh, "Rescan projects and poll GitHub")
                .on_press_maybe(self.board.is_some().then_some(Action::Refresh))
                .into()
        };
        let mut search = crate::ui_controls::field::search("Filter cards", &self.query)
            .on_input(Action::Query)
            .width(Length::Fill);
        if !self.query.is_empty() {
            search = search.trailing_icon(crate::ui_controls::field::clear(Action::Query(
                String::new(),
            )));
        }
        let toolbar = widget::row::with_children(vec![
            design::title("Work").into(),
            self.scope_picker(),
            widget::container(search)
                .max_width(360)
                .width(Length::Fill)
                .into(),
            widget::Space::new().width(Length::Fill).into(),
            layout,
            refresh,
        ])
        .spacing(space::S)
        .align_y(Alignment::Center);
        let mut header = widget::column::with_children(vec![toolbar.into()]).spacing(space::XS);
        if let Some(status) = self.status() {
            header = header.push(status);
        }
        widget::container(header)
            .padding(Padding::from([space::S, space::L]))
            .width(Length::Fill)
            .into()
    }

    /// The scope's name, opening the list of projects with their card counts.
    fn scope_picker(&self) -> Element<'_, Action> {
        let projects = self
            .board
            .as_ref()
            .map(|board| board.projects.as_slice())
            .unwrap_or_default();
        let name = self
            .scope
            .as_ref()
            .and_then(|scope| projects.iter().find(|project| project.repository == *scope))
            .map_or("All projects", |project| project.name.as_str());
        let anchor = widget::button::custom(
            widget::row::with_children(vec![
                design::body(name)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
                widget::icon(Icon::ChevronDown.handle())
                    .size(design::COMPACT_ICON_SIZE)
                    .into(),
            ])
            .spacing(space::XS)
            .align_y(Alignment::Center),
        )
        .name("Choose project")
        .class(crate::ui_controls::control_style(self.scope_menu, None))
        .padding([space::XS, space::S])
        .on_press_maybe((!projects.is_empty()).then_some(Action::ScopeMenu(!self.scope_menu)));
        let content = self.scope_menu.then(|| {
            let cards_in_board = self
                .board
                .as_ref()
                .map(|board| board.items.as_slice())
                .unwrap_or_default();
            let mut counts: HashMap<&PathBuf, usize> = HashMap::new();
            for item in cards_in_board {
                *counts.entry(&item.project).or_default() += 1;
            }
            let mut items = vec![
                popover_item(
                    Icon::Grid.handle(),
                    "All projects",
                    cards(cards_in_board.len()),
                    Action::Scope(None),
                )
                .selected(self.scope.is_none())
                .into(),
            ];
            items.extend(projects.iter().map(|project| {
                popover_item(
                    Icon::Branch.handle(),
                    project.name.as_str(),
                    cards(counts.get(&project.repository).copied().unwrap_or_default()),
                    Action::Scope(Some(project.repository.clone())),
                )
                .selected(self.scope.as_ref() == Some(&project.repository))
                .into()
            }));
            widget::scrollable(widget::column::with_children(items).width(SCOPE_MENU_WIDTH))
                .height(Length::Shrink)
                .into()
        });
        popover(
            anchor,
            content,
            widget::popover::Position::Bottom,
            Action::ScopeMenu(false),
        )
    }

    /// How many cards are shown, and what the board could not read.
    fn status(&self) -> Option<Element<'_, Action>> {
        let board = self.board.as_ref()?;
        // The daemon sends a board only when it changes, so its age says
        // nothing about how current it is; a live subscription is current.
        let mut parts: Vec<Element<'_, Action>> = vec![
            design::caption(cards(self.visible().count()))
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .into(),
        ];
        if let Some(error) = &self.lost {
            parts.push(warning(format!("Disconnected: {error}")));
        }
        if let Some(error) = &self.error {
            parts.push(warning(error.clone()));
        }
        if let Some(error) = &self.pins.error {
            parts.push(warning(error.clone()));
        }
        let projects = board.projects.iter().filter(|project| {
            self.scope
                .as_ref()
                .is_none_or(|scope| project.repository == *scope)
        });
        for project in projects {
            if let Some(error) = &project.scan_error {
                parts.push(warning(format!(
                    "{} could not be read: {error}",
                    project.name
                )));
            }
            match &project.forge {
                Forge::Unavailable { reason } => {
                    parts.push(warning(format!(
                        "{}: GitHub unavailable: {reason}",
                        project.name
                    )));
                }
                Forge::Synced {
                    error: Some(error), ..
                } => parts.push(warning(format!(
                    "{}: pull requests may be out of date: {error}",
                    project.name
                ))),
                _ => {}
            }
        }
        if let Some(reason) = &board.linear_unavailable {
            parts.push(warning(format!(
                "Linear tickets could not be looked up: {reason}"
            )));
        }
        Some(
            widget::row::with_children(parts)
                .spacing(space::M)
                .align_y(Alignment::Center)
                .wrap()
                .into(),
        )
    }

    fn board_view(&self, width: f32) -> Element<'_, Action> {
        let mut columns: [Vec<&Item>; 5] = Default::default();
        for item in self.visible() {
            columns[column_index(item.column)].push(item);
        }
        let gutter = space::L * 2.0 + space::S * 4.0;
        let fits = width - gutter >= COLUMN_MIN_WIDTH * Column::ALL.len() as f32;
        let column_width = if fits {
            Length::FillPortion(1)
        } else {
            Length::Fixed(COLUMN_WIDTH)
        };
        let lanes = Column::ALL
            .into_iter()
            .zip(columns)
            .map(|(column, items)| self.lane(column, items, column_width))
            .collect::<Vec<_>>();
        let row = widget::row::with_children(lanes)
            .spacing(space::S)
            .height(Length::Fill)
            .padding(Padding::from([space::M, space::L]));
        if fits {
            row.width(Length::Fill).into()
        } else {
            crate::interaction::horizontal_wheel(
                widget::scrollable::horizontal(row)
                    .direction(iced::widget::scrollable::Direction::Horizontal(
                        design::scrollbar(),
                    ))
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
        }
    }

    fn lane<'a>(
        &'a self,
        column: Column,
        items: Vec<&'a Item>,
        width: Length,
    ) -> Element<'a, Action> {
        let header = widget::row::with_children(vec![
            widget::container(widget::Space::new().width(8).height(8))
                .class(dot(column_tone(column)))
                .into(),
            design::heading(column.label()).into(),
            design::caption(items.len().to_string())
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .into(),
        ])
        .spacing(space::S)
        .align_y(Alignment::Center)
        .padding([space::XS, space::S]);
        let body: Element<'a, Action> = if items.is_empty() {
            widget::container(
                design::caption(empty_lane(column))
                    .class(cosmic::theme::Text::Custom(design::muted_text)),
            )
            .padding(space::S)
            .width(Length::Fill)
            .into()
        } else {
            widget::scrollable(
                widget::column::with_children(
                    items
                        .into_iter()
                        .map(|item| self.card(item))
                        .collect::<Vec<_>>(),
                )
                .spacing(space::S)
                .padding(design::with_vertical_rail(Padding::from([0.0, space::XS]))),
            )
            .direction(iced::widget::scrollable::Direction::Vertical(
                design::scrollbar(),
            ))
            .height(Length::Fill)
            .into()
        };
        widget::container(
            widget::column::with_children(vec![header.into(), body])
                .spacing(space::XS)
                .height(Length::Fill),
        )
        .padding(Padding::from([space::XS, 0.0, space::XS, space::XS]))
        .width(width)
        .height(Length::Fill)
        .class(design::surface(
            Some(lane_fill),
            [design::RADIUS_M; 4],
            false,
        ))
        .into()
    }

    fn list_view(&self) -> Element<'_, Action> {
        let rows = self
            .visible()
            .map(|item| self.row(item))
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return self.no_match();
        }
        widget::scrollable(
            widget::column::with_children(rows)
                .spacing(space::XS)
                .padding(design::with_vertical_rail(Padding::from([
                    space::M,
                    space::L,
                ]))),
        )
        .direction(iced::widget::scrollable::Direction::Vertical(
            design::scrollbar(),
        ))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn no_match(&self) -> Element<'_, Action> {
        if self.query.trim().is_empty() {
            placeholder("No work in progress")
                .icon(Icon::Branch.handle())
                .detail("Worktrees, branches with commits and your open pull requests appear here.")
                .into()
        } else {
            placeholder("No cards match")
                .icon(Icon::Search.handle())
                .detail(format!(
                    "Nothing in scope mentions “{}”.",
                    self.query.trim()
                ))
                .into()
        }
    }

    /// A board card: branch, pull request, then what is true of it now.
    fn card<'a>(&'a self, item: &'a Item) -> Element<'a, Action> {
        let card = CardRef::of(item);
        let mut top = vec![
            status_glyph(item),
            design::heading(item.branch.as_deref().unwrap_or("Detached HEAD"))
                .wrapping(iced::widget::text::Wrapping::None)
                .ellipsize(iced::widget::text::Ellipsize::End(
                    iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill)
                .into(),
        ];
        if item.pinned {
            top.push(crate::interaction::tooltip(
                widget::icon(Icon::Lock.handle()).size(design::COMPACT_ICON_SIZE),
                format!("Pinned; its state says {}", item.derived.label()),
            ));
        }
        top.push(self.card_menu(item, &card));
        let mut lines = vec![
            widget::row::with_children(top)
                .spacing(space::XS)
                .align_y(Alignment::Center)
                .into(),
        ];
        if let Some(pull_request) = &item.pull_request {
            lines.push(
                design::body(format!("#{} {}", pull_request.number, pull_request.title))
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        iced::advanced::text::EllipsizeHeightLimit::Lines(2),
                    ))
                    .width(Length::Fill)
                    .into(),
            );
        }
        let facts = self.facts(item);
        if !facts.is_empty() {
            lines.push(
                widget::row::with_children(facts)
                    .spacing(space::XS)
                    .align_y(Alignment::Center)
                    .wrap()
                    .into(),
            );
        }
        lines.push(self.footer(item));
        let content = widget::column::with_children(lines).spacing(space::XS);
        let selected = self.menu.as_ref() == Some(&card);
        let primary = Activation::primary(item, self.lifetime);
        let button = widget::button::custom(content)
            .name(format!("Open {}", item.label()))
            .class(card_style(selected))
            .padding(space::S)
            .width(Length::Fill)
            .on_press_maybe(primary.clone().map(Action::Activate));
        crate::ui_controls::owned(
            (card.clone(), primary),
            widget::mouse_area(crate::interaction::hover(button))
                .on_right_press(Action::Menu(Some(card))),
        )
    }

    /// A list row: the same facts as a card, on one line where they fit.
    fn row<'a>(&'a self, item: &'a Item) -> Element<'a, Action> {
        let card = CardRef::of(item);
        let mut title = vec![
            status_glyph(item),
            design::heading(item.branch.as_deref().unwrap_or("Detached HEAD"))
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
        ];
        if let Some(pull_request) = &item.pull_request {
            title.push(
                design::body(format!("#{} {}", pull_request.number, pull_request.title))
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
            );
        } else {
            title.push(widget::Space::new().width(Length::Fill).into());
        }
        title.push(badge(
            item.column.label(),
            Tone::Themed(column_tone(item.column)),
        ));
        title.push(
            widget::container(
                design::caption(age(self.now_unix_ms, activity_at(item), false))
                    .class(cosmic::theme::Text::Custom(design::muted_text)),
            )
            .width(44)
            .align_x(iced::alignment::Horizontal::Right)
            .into(),
        );
        title.push(self.card_menu(item, &card));
        let mut facts = self.facts(item);
        if let Some(worktree) = &item.worktree {
            facts.push(
                design::caption(crate::app::display_path(&worktree.path))
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
            );
        }
        let mut lines = vec![
            widget::row::with_children(title)
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
        ];
        if !facts.is_empty() {
            lines.push(
                widget::container(
                    widget::row::with_children(facts)
                        .spacing(space::XS)
                        .align_y(Alignment::Center)
                        .wrap(),
                )
                .padding(Padding {
                    left: f32::from(design::ROW_ICON_SIZE) + space::S,
                    ..Padding::ZERO
                })
                .into(),
            );
        }
        let selected = self.menu.as_ref() == Some(&card);
        let primary = Activation::primary(item, self.lifetime);
        let button =
            widget::button::custom(widget::column::with_children(lines).spacing(space::XS))
                .name(format!("Open {}", item.label()))
                .class(card_style(selected))
                .padding([space::XS + space::XXS, space::S])
                .width(Length::Fill)
                .on_press_maybe(primary.clone().map(Action::Activate));
        crate::ui_controls::owned(
            (card.clone(), primary),
            widget::mouse_area(crate::interaction::hover(button))
                .on_right_press(Action::Menu(Some(card))),
        )
    }

    /// The pills that say what is true of a card now.
    fn facts<'a>(&'a self, item: &'a Item) -> Vec<Element<'a, Action>> {
        let mut facts = Vec::new();
        if self.scope.is_none() {
            facts.push(badge(
                item.project_name.as_str(),
                Tone::Themed(design::muted_color),
            ));
        }
        if let Some(pull_request) = &item.pull_request {
            let (label, tone): (&str, Toned) = match (pull_request.state, pull_request.draft) {
                (PullRequestState::Open, true) => ("Draft", design::muted_color),
                (PullRequestState::Open, false) => ("Open", design::success_color),
                (PullRequestState::Merged, _) => ("Merged", design::accent_color),
                (PullRequestState::Closed, _) => ("Closed", design::destructive_color),
            };
            facts.push(badge(label, Tone::Themed(tone)));
            if pull_request.state == PullRequestState::Open {
                if let Some((label, tone)) = pull_request.checks.and_then(|checks| {
                    checks.outcome().map(|outcome| -> (String, Toned) {
                        match outcome {
                            CheckOutcome::Passing => {
                                (format!("✓ {}", checks.passed), design::success_color)
                            }
                            CheckOutcome::Pending => (
                                format!("● {} running", checks.pending),
                                design::warning_color,
                            ),
                            CheckOutcome::Failing => (
                                format!("✕ {} failing", checks.failed),
                                design::destructive_color,
                            ),
                        }
                    })
                }) {
                    facts.push(badge(label, Tone::Themed(tone)));
                }
                if let Some(review) = pull_request.review {
                    let (label, tone): (&str, Toned) = match review {
                        Review::Approved => ("Approved", design::success_color),
                        Review::ChangesRequested => {
                            ("Changes requested", design::destructive_color)
                        }
                        Review::Required => ("Review required", design::muted_color),
                    };
                    facts.push(badge(label, Tone::Themed(tone)));
                }
                if !pull_request.contains_local_head && item.worktree.is_some() {
                    facts.push(crate::interaction::tooltip(
                        badge("Unpushed", Tone::Themed(design::warning_color)),
                        "The worktree has commits the pull request does not",
                    ));
                }
            }
        }
        if let Some(ticket) = &item.linear
            && let Some(identifier) = &ticket.identifier
        {
            let pill = badge(identifier.as_str(), Tone::Themed(design::accent_color));
            facts.push(match &ticket.title {
                Some(title) => crate::interaction::tooltip(pill, title.clone()),
                None => pill,
            });
        }
        if let Some(comparison) = &item.comparison
            && (comparison.ahead > 0 || comparison.behind > 0)
        {
            let counts = match (comparison.ahead, comparison.behind) {
                (ahead, 0) => format!("↑{ahead}"),
                (0, behind) => format!("↓{behind}"),
                (ahead, behind) => format!("↑{ahead} ↓{behind}"),
            };
            facts.push(crate::interaction::tooltip(
                design::caption(counts)
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .wrapping(iced::widget::text::Wrapping::None),
                format!(
                    "{} ahead of and {} behind {}",
                    commits(comparison.ahead),
                    commits(comparison.behind),
                    comparison.base
                ),
            ));
        }
        if let Some(dirty) = &item.dirty {
            if dirty.conflicted > 0 {
                facts.push(badge(
                    format!("{} conflicted", dirty.conflicted),
                    Tone::Themed(design::destructive_color),
                ));
            }
            if dirty.paths > 0 {
                facts.push(crate::interaction::tooltip(
                    badge(
                        format!("{} changed", dirty.paths),
                        Tone::Themed(design::warning_color),
                    ),
                    "Uncommitted changes in the worktree",
                ));
            }
        }
        if let Some(error) = &item.error {
            facts.push(crate::interaction::tooltip(
                badge("Unreadable", Tone::Themed(design::destructive_color)),
                error.clone(),
            ));
        }
        facts
    }

    /// Live sessions on the left, the last activity on the right.
    fn footer<'a>(&'a self, item: &'a Item) -> Element<'a, Action> {
        let mut parts = item
            .sessions
            .iter()
            .map(|session| {
                let working = matches!(session.state, ActivityState::Working);
                let glyph: Element<'a, Action> = if working {
                    crate::ui_controls::activity_spinner(12.0, design::accent_color)
                } else {
                    match session.provider {
                        Some(provider) => widget::icon(crate::ui_controls::provider_icon(provider))
                            .size(design::COMPACT_ICON_SIZE)
                            .into(),
                        None => widget::icon(Icon::Terminal.handle())
                            .size(design::COMPACT_ICON_SIZE)
                            .into(),
                    }
                };
                let label = session.title.clone().unwrap_or_else(|| {
                    session.provider.map_or_else(
                        || "Terminal".to_owned(),
                        |provider| provider.label().to_owned(),
                    )
                });
                let state = match session.state {
                    ActivityState::Working => "working",
                    ActivityState::Waiting => "waiting for you",
                    _ => "idle",
                };
                let activation =
                    Activation::new(item, Destination::Session(session.id), self.lifetime);
                crate::ui_controls::owned(
                    activation.clone(),
                    crate::interaction::tooltip(
                        widget::button::custom(glyph)
                            .name(format!("Focus {label}"))
                            .class(crate::ui_controls::control_style(false, None))
                            .padding(space::XXS)
                            .on_press(Action::Activate(activation)),
                        format!("{label}, {state}"),
                    ),
                )
            })
            .collect::<Vec<_>>();
        parts.push(widget::Space::new().width(Length::Fill).into());
        if let Some(activity) = &item.last_activity {
            parts.push(crate::interaction::tooltip(
                design::caption(format!(
                    "{} {}",
                    activity_label(activity.source),
                    age(self.now_unix_ms, activity.at_unix_ms, true)
                ))
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .wrapping(iced::widget::text::Wrapping::None),
                local_time(activity.at_unix_ms),
            ));
        }
        widget::row::with_children(parts)
            .spacing(space::XXS)
            .align_y(Alignment::Center)
            .into()
    }

    /// The card's actions: open what it names, and move it between columns.
    fn card_menu<'a>(&'a self, item: &'a Item, card: &CardRef) -> Element<'a, Action> {
        let open = self.menu.as_ref() == Some(card);
        let anchor = icon_button(Icon::More, "Card actions")
            .compact()
            .selected(open)
            .on_press(Action::Menu((!open).then(|| card.clone())));
        let content = open.then(|| {
            let mut items: Vec<Element<'a, Action>> = Vec::new();
            if let Some(worktree) = &item.worktree {
                let activation = Activation::new(
                    item,
                    Destination::Worktree(worktree.path.clone()),
                    self.lifetime,
                );
                items.push(crate::ui_controls::owned(
                    activation.clone(),
                    popover_item(
                        Icon::OpenFolder.handle(),
                        "Open worktree",
                        crate::app::display_path(&worktree.path),
                        Action::Activate(activation),
                    ),
                ));
            }
            if let Some(pull_request) = &item.pull_request {
                let activation = Activation::new(
                    item,
                    Destination::PullRequest {
                        number: pull_request.number,
                        url: pull_request.url.clone(),
                    },
                    self.lifetime,
                );
                items.push(crate::ui_controls::owned(
                    activation.clone(),
                    popover_item(
                        Icon::PullRequest.handle(),
                        "Open pull request",
                        format!("#{}", pull_request.number),
                        Action::Activate(activation),
                    ),
                ));
            }
            if let Some(ticket) = &item.linear
                && let Some(url) = &ticket.url
            {
                let activation =
                    Activation::new(item, Destination::Linear(url.clone()), self.lifetime);
                items.push(crate::ui_controls::owned(
                    activation.clone(),
                    popover_item(
                        Icon::WebBrowser.handle(),
                        "Open Linear issue",
                        ticket.identifier.clone().unwrap_or_default(),
                        Action::Activate(activation),
                    ),
                ));
            }
            if let Some(branch) = &item.branch {
                let activation =
                    Activation::new(item, Destination::Branch(branch.clone()), self.lifetime);
                items.push(crate::ui_controls::owned(
                    activation.clone(),
                    popover_item(
                        Icon::Copy.handle(),
                        "Copy branch name",
                        branch.as_str(),
                        Action::Activate(activation),
                    ),
                ));
            }
            if let Some(subject) = RemovalSubject::of(item) {
                let activation = RemovalActivation {
                    lifetime: self.lifetime,
                    watch: self.pins.lifetime,
                    generation: self.removal.generation(),
                    subject,
                };
                let enabled =
                    self.lost.is_none() && self.removal.can_open_subject(&activation.subject);
                items.push(crate::ui_controls::owned(
                    (activation.clone(), enabled),
                    popover_item(
                        Icon::Delete.handle(),
                        "Remove worktree…",
                        "Review what removal keeps and deletes",
                        Action::ActivateRemoval(activation.clone()),
                    )
                    .on_press_maybe(enabled.then_some(Action::ActivateRemoval(activation))),
                ));
            }
            items.push(design::horizontal_rule());
            for column in Column::ALL {
                let current = item.pinned && item.column == column;
                let description = if current {
                    "Pinned here"
                } else if column == item.derived {
                    "Where its state puts it"
                } else {
                    "Pin here"
                };
                items.push(crate::ui_controls::owned(
                    (
                        self.lifetime,
                        self.pins.lifetime,
                        card.clone(),
                        Some(column),
                        !current,
                    ),
                    popover_item(
                        if current {
                            Icon::Check.handle()
                        } else {
                            Icon::Board.handle()
                        },
                        column.label(),
                        description,
                        Action::Pin(card.clone(), Some(column)),
                    )
                    .on_press_maybe((!current).then(|| Action::Pin(card.clone(), Some(column)))),
                ));
            }
            if item.pinned {
                items.push(crate::ui_controls::owned(
                    (
                        self.lifetime,
                        self.pins.lifetime,
                        card.clone(),
                        None::<Column>,
                        true,
                    ),
                    popover_item(
                        Icon::Undo.handle(),
                        "Unpin",
                        format!("Return to {}", item.derived.label()),
                        Action::Pin(card.clone(), None),
                    ),
                ));
            }
            widget::column::with_children(items)
                .width(CARD_MENU_WIDTH)
                .into()
        });
        popover(
            anchor,
            content,
            widget::popover::Position::Bottom,
            Action::Menu(None),
        )
    }
}

/// Whether `query`, already lowercase, appears in anything the card shows.
/// What a query is matched against: the branch, project, worktree path,
/// pull request number and title, and Linear ticket, one per line so no
/// match spans two of them.
fn search_text(item: &Item) -> String {
    let mut fields = vec![item.project_name.clone()];
    fields.extend(item.branch.clone());
    fields.extend(
        item.worktree
            .as_ref()
            .map(|worktree| worktree.path.to_string_lossy().into_owned()),
    );
    if let Some(pull_request) = &item.pull_request {
        fields.push(format!("#{}", pull_request.number));
        fields.push(pull_request.title.clone());
    }
    if let Some(ticket) = &item.linear {
        fields.extend(ticket.identifier.clone());
        fields.extend(ticket.title.clone());
    }
    fields.join("\n").to_lowercase()
}

fn column_index(column: Column) -> usize {
    Column::ALL
        .iter()
        .position(|candidate| *candidate == column)
        .expect("every column is listed")
}

fn column_tone(column: Column) -> Toned {
    match column {
        Column::Active => design::success_color,
        Column::InProgress => design::warning_color,
        Column::InReview => design::accent_color,
        Column::Landed => merged_color,
        Column::Stale => design::muted_color,
    }
}

/// Merged work reads as done, apart from work under review.
fn merged_color(theme: &Theme) -> iced::Color {
    design::mix(
        design::accent_color(theme),
        design::success_color(theme),
        0.5,
    )
}

fn empty_lane(column: Column) -> &'static str {
    match column {
        Column::Active => "Nothing changed in the last few hours",
        Column::InProgress => "No local work without a review",
        Column::InReview => "No pull requests waiting for review",
        Column::Landed => "Nothing merged is left to clean up",
        Column::Stale => "Nothing has gone quiet",
    }
}

fn lane_fill(theme: &Theme) -> iced::Color {
    design::sunken(theme)
}

fn dot(tone: Toned) -> cosmic::theme::Container<'static> {
    cosmic::theme::Container::custom(move |theme| widget::container::Style {
        background: Some(iced::Background::Color(tone(theme))),
        border: iced::Border {
            radius: 4.0.into(),
            ..Default::default()
        },
        ..Default::default()
    })
}

/// What the card's leading glyph says at a glance: a working agent, else the
/// pull request's state, else a branch.
fn status_glyph<'a>(item: &Item) -> Element<'a, Action> {
    if item
        .sessions
        .iter()
        .any(|session| session.state == ActivityState::Working)
    {
        return widget::container(crate::ui_controls::activity_spinner(
            12.0,
            design::accent_color,
        ))
        .center(Length::Fixed(f32::from(design::ROW_ICON_SIZE)))
        .into();
    }
    if item
        .sessions
        .iter()
        .any(|session| session.state == ActivityState::Waiting)
    {
        return glyph(Icon::AgentWaiting, design::warning_color);
    }
    match &item.pull_request {
        Some(pull_request) => match pull_request.state {
            PullRequestState::Merged => glyph(Icon::MergedPullRequest, merged_color),
            PullRequestState::Closed => glyph(Icon::ClosedPullRequest, design::destructive_color),
            PullRequestState::Open if pull_request.draft => {
                glyph(Icon::PullRequest, design::muted_color)
            }
            PullRequestState::Open => glyph(Icon::PullRequest, design::success_color),
        },
        None => glyph(Icon::Branch, design::muted_color),
    }
}

fn glyph<'a>(icon: Icon, tone: Toned) -> Element<'a, Action> {
    widget::icon(icon.handle())
        .size(design::ROW_ICON_SIZE)
        .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(move |theme| {
            iced::widget::svg::Style {
                color: Some(tone(theme)),
            }
        })))
        .into()
}

fn warning<'a>(text: String) -> Element<'a, Action> {
    widget::row::with_children(vec![
        widget::icon(Icon::Warning.handle())
            .size(design::COMPACT_ICON_SIZE)
            .into(),
        design::caption(text)
            .class(cosmic::theme::Text::Custom(design::warning_text))
            .into(),
    ])
    .spacing(space::XS)
    .align_y(Alignment::Center)
    .into()
}

/// A raised card that lifts on hover and stays lifted while its menu is open.
fn card_style(selected: bool) -> cosmic::theme::Button {
    fn appearance(theme: &Theme, lifted: bool, selected: bool) -> widget::button::Style {
        let mut fill = design::raised(theme);
        if lifted || selected {
            let on: iced::Color = theme.current_container().on.into();
            fill = design::mix(fill, on, if selected { 0.08 } else { 0.04 });
        }
        widget::button::Style {
            background: Some(iced::Background::Color(fill)),
            border_radius: design::RADIUS_S.into(),
            border_width: design::BORDER_WIDTH,
            border_color: if selected {
                design::accent_color(theme)
            } else {
                design::hairline(theme)
            },
            text_color: Some(theme.current_container().on.into()),
            ..design::button_style()
        }
    }
    if selected {
        cosmic::theme::Button::Custom {
            active: Box::new(|_, theme| appearance(theme, false, true)),
            disabled: Box::new(|theme| appearance(theme, false, true)),
            hovered: Box::new(|_, theme| appearance(theme, true, true)),
            pressed: Box::new(|_, theme| appearance(theme, true, true)),
        }
    } else {
        cosmic::theme::Button::Custom {
            active: Box::new(|focused, theme| appearance(theme, focused, false)),
            disabled: Box::new(|theme| appearance(theme, false, false)),
            hovered: Box::new(|_, theme| appearance(theme, true, false)),
            pressed: Box::new(|_, theme| appearance(theme, true, false)),
        }
    }
}

fn activity_at(item: &Item) -> u64 {
    item.last_activity.map_or(0, |activity| activity.at_unix_ms)
}

fn activity_label(source: ActivitySource) -> &'static str {
    match source {
        ActivitySource::Commit => "Committed",
        ActivitySource::Files => "Edited",
        ActivitySource::Session => "Agent active",
        ActivitySource::Created => "Created",
        ActivitySource::PullRequest => "PR updated",
    }
}

fn cards(count: usize) -> String {
    match count {
        1 => "1 card".to_owned(),
        count => format!("{count} cards"),
    }
}

fn commits(count: u32) -> String {
    match count {
        1 => "1 commit".to_owned(),
        count => format!("{count} commits"),
    }
}

/// How long ago `at` was, compactly: `now`, `5m`, `3h`, `2d`, `3w`, `4mo`,
/// `2y`, followed by ` ago` when `suffix` is set.
pub fn age(now_unix_ms: u64, at_unix_ms: u64, suffix: bool) -> String {
    if at_unix_ms == 0 {
        return "—".to_owned();
    }
    let seconds = now_unix_ms.saturating_sub(at_unix_ms) / 1000;
    let compact = match seconds {
        0..60 => {
            return if suffix {
                "just now".to_owned()
            } else {
                "now".to_owned()
            };
        }
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        86_400..1_209_600 => format!("{}d", seconds / 86_400),
        1_209_600..5_184_000 => format!("{}w", seconds / 604_800),
        5_184_000..31_536_000 => format!("{}mo", seconds / 2_592_000),
        _ => format!("{}y", seconds / 31_536_000),
    };
    if suffix {
        format!("{compact} ago")
    } else {
        compact
    }
}

fn local_time(at_unix_ms: u64) -> String {
    i64::try_from(at_unix_ms)
        .ok()
        .and_then(|at| jiff::Timestamp::from_millisecond(at).ok())
        .map(|at| {
            at.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%a %b %-d, %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{Comparison, Dirty, Project, PullRequest};

    fn item(branch: &str, project: &str) -> Item {
        Item {
            project: PathBuf::from(format!("/repos/{project}/.git")),
            project_name: project.to_owned(),
            key: ItemKey::Branch(branch.to_owned()),
            branch: Some(branch.to_owned()),
            worktree: None,
            head: None,
            comparison: Some(Comparison {
                base: "origin/main".into(),
                ahead: 1,
                behind: 0,
            }),
            dirty: Some(Dirty::default()),
            pull_request: None,
            linear: None,
            sessions: Vec::new(),
            last_activity: None,
            error: None,
            column: Column::InProgress,
            derived: Column::InProgress,
            pinned: false,
        }
    }

    fn board(items: Vec<Item>) -> Arc<Board> {
        let mut projects = items
            .iter()
            .map(|item| Project {
                repository: item.project.clone(),
                name: item.project_name.clone(),
                root: None,
                base: None,
                scanned_at_unix_ms: None,
                scan_error: None,
                forge: Forge::NotGitHub,
            })
            .collect::<Vec<_>>();
        projects.dedup();
        Arc::new(Board {
            generated_at_unix_ms: 1_000,
            stale_after_days: 7,
            projects,
            items,
            linear_unavailable: None,
        })
    }

    fn watched(state: &mut State, revision: u64, board: Arc<Board>) {
        state.update(Action::Watched(Watch::Board {
            revision,
            board,
            first: false,
        }));
    }

    fn connected(state: &mut State, revision: u64, board: Arc<Board>) {
        state.update(Action::Watched(Watch::Board {
            revision,
            board,
            first: true,
        }));
    }

    #[test]
    fn an_older_board_never_replaces_a_newer_one() {
        let mut state = State::default();
        connected(&mut state, 2, board(vec![item("new", "cagents")]));
        watched(&mut state, 1, board(vec![item("old", "cagents")]));
        assert_eq!(
            state.visible().map(Item::label).collect::<Vec<_>>(),
            ["new"]
        );
        // A new daemon starts its revisions over, whether or not this pane
        // saw the old connection fail.
        connected(&mut state, 1, board(vec![item("restarted", "cagents")]));
        assert_eq!(
            state.visible().map(Item::label).collect::<Vec<_>>(),
            ["restarted"]
        );
    }

    #[test]
    fn scope_and_query_filter_cards_and_a_closed_project_leaves_scope() {
        let mut state = State::default();
        let mut reviewed = item("fix/login", "redact");
        reviewed.pull_request = Some(PullRequest {
            number: 42,
            title: "Fix login redirect".into(),
            url: "https://github.com/o/redact/pull/42".into(),
            state: PullRequestState::Open,
            draft: false,
            head_oid: "a".into(),
            contains_local_head: true,
            checks: None,
            review: None,
            updated_at: String::new(),
        });
        watched(
            &mut state,
            1,
            board(vec![item("feat/board", "cagents"), reviewed]),
        );
        state.update(Action::Query("REDIRECT".into()));
        assert_eq!(
            state.visible().map(Item::label).collect::<Vec<_>>(),
            ["fix/login"]
        );
        state.update(Action::Query("#42".into()));
        assert_eq!(state.visible().count(), 1);
        state.update(Action::Query(String::new()));
        state.update(Action::Scope(Some(PathBuf::from("/repos/cagents/.git"))));
        assert_eq!(
            state.visible().map(Item::label).collect::<Vec<_>>(),
            ["feat/board"]
        );
        watched(&mut state, 2, board(vec![item("fix/login", "redact")]));
        assert_eq!(state.scope, None);
    }

    fn scope_menu_text(state: &State) -> Option<Vec<String>> {
        use cosmic::iced::{
            Rectangle, Size, Vector,
            advanced::{
                Layout, layout,
                widget::{Operation, Tree},
            },
        };
        #[derive(Default)]
        struct Text(Vec<String>);
        impl Operation for Text {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, _: Rectangle, text: &str) {
                self.0.push(text.into());
            }
        }
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut element = state.scope_picker();
        let mut tree = Tree::new(element.as_widget());
        let viewport = Rectangle::with_size(Size::new(800.0, 700.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let mut overlay = element.as_widget_mut().overlay(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &viewport,
            Vector::ZERO,
        )?;
        let popup = overlay.as_overlay_mut().layout(&renderer, viewport.size());
        let mut text = Text::default();
        overlay
            .as_overlay_mut()
            .operate(Layout::new(&popup), &renderer, &mut text);
        Some(text.0)
    }

    #[test]
    fn scope_menu_counts_all_cards_by_exact_project_and_tracks_accepted_boards() {
        let mut initial = board(vec![
            item("first", "sample"),
            item("second", "sample"),
            item("third", "sample-extra"),
            item("unlisted", "orphan"),
        ]);
        let data = Arc::make_mut(&mut initial);
        data.projects.retain(|project| project.name != "orphan");
        for project in &mut data.projects {
            project.name = "Same name".into();
        }
        let mut empty = data.projects[0].clone();
        empty.repository = "/repos/empty/.git".into();
        empty.name = "Empty project".into();
        data.projects.push(empty.clone());
        let mut state = State::default();
        connected(&mut state, 2, initial.clone());
        assert!(
            scope_menu_text(&state).is_none(),
            "closed menu has no popup"
        );
        state.update(Action::ScopeMenu(true));
        let expected = [
            "All projects",
            "4 cards",
            "Same name",
            "2 cards",
            "Same name",
            "1 card",
            "Empty project",
            "0 cards",
        ];
        assert_eq!(scope_menu_text(&state).unwrap(), expected);
        state.update(Action::Query("no matching cards".into()));
        state.update(Action::Scope(Some("/repos/sample/.git".into())));
        assert_eq!(state.visible().count(), 0);
        state.update(Action::ScopeMenu(true));
        assert_eq!(
            scope_menu_text(&state).unwrap(),
            expected,
            "picker counts ignore current search and scope"
        );

        let mut updated = board(vec![item("new", "sample-extra")]);
        let data = Arc::make_mut(&mut updated);
        data.projects[0].name = "Same name".into();
        data.projects.push(empty);
        watched(&mut state, 3, updated);
        assert!(
            state.scope.is_none(),
            "removed selected project leaves scope"
        );
        let updated_counts = [
            "All projects",
            "1 card",
            "Same name",
            "1 card",
            "Empty project",
            "0 cards",
        ];
        assert_eq!(scope_menu_text(&state).unwrap(), updated_counts);
        watched(&mut state, 2, initial);
        assert_eq!(
            scope_menu_text(&state).unwrap(),
            updated_counts,
            "an older board must not restore obsolete counts"
        );

        state.reset();
        state.update(Action::ScopeMenu(true));
        assert_eq!(
            scope_menu_text(&state).unwrap(),
            ["All projects", "0 cards"]
        );
        connected(&mut state, 1, board(Vec::new()));
        assert_eq!(
            scope_menu_text(&state).unwrap(),
            ["All projects", "0 cards"]
        );
    }

    #[test]
    fn queued_project_picker_choice_cannot_restore_a_removed_scope() {
        use iced::runtime::user_interface::Cache;

        fn build<'a>(
            state: &'a State,
            cache: Cache,
            renderer: &mut cosmic::Renderer,
        ) -> iced::runtime::UserInterface<'a, Action, Theme, cosmic::Renderer> {
            let mut ui = iced::runtime::UserInterface::build(
                state.layout_view(1600.0),
                iced::Size::new(1600.0, 900.0),
                cache,
                renderer,
            );
            draw_work_ui(&mut ui, renderer);
            ui
        }

        fn choose(state: &mut State, renderer: &mut cosmic::Renderer, label: &str) -> Action {
            let cache = if !state.scope_menu {
                let mut ui = build(state, Cache::new(), renderer);
                let point = first_open_bounds(&ui, "Choose project").center();
                let actions = click_work(&mut ui, renderer, point);
                let cache = ui.into_cache();
                assert!(matches!(actions.as_slice(), [Action::ScopeMenu(true)]));
                state.update(actions.into_iter().next().unwrap());
                cache
            } else {
                Cache::new()
            };
            let mut ui = build(state, cache, renderer);
            // The selected project's text also appears in the anchor. Only
            // choose a real overlay row below it, never that same-label anchor.
            #[derive(Default)]
            struct Rows {
                text: Vec<(String, iced::Rectangle)>,
                buttons: Vec<iced::Rectangle>,
            }
            impl iced::advanced::widget::Operation for Rows {
                fn traverse(
                    &mut self,
                    visit: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation),
                ) {
                    visit(self);
                }
                fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
                    self.text.push((text.into(), bounds));
                }
                fn focusable(
                    &mut self,
                    _: Option<&widget::Id>,
                    bounds: iced::Rectangle,
                    _: &mut dyn iced::advanced::widget::operation::Focusable,
                ) {
                    self.buttons.push(bounds);
                }
            }
            let anchor = first_open_bounds(&ui, "Choose project");
            let mut rows = Rows::default();
            ui.operate(renderer, &mut rows);
            let bounds = rows
                .buttons
                .iter()
                .filter(|bounds| {
                    bounds.y >= anchor.y + anchor.height
                        && rows.text.iter().any(|(text, text_bounds)| {
                            text == label && bounds.contains(text_bounds.center())
                        })
                })
                .min_by(|a, b| (a.width * a.height).total_cmp(&(b.width * b.height)))
                .copied()
                .unwrap_or_else(|| panic!("missing actual scope row {label}"));
            assert!(bounds.width > 0.0 && bounds.height > 0.0);
            let point = bounds.center();
            let actions = click_work(&mut ui, renderer, point);
            assert!(
                matches!(actions.as_slice(), [Action::Scope(_)]),
                "{actions:?}"
            );
            actions.into_iter().next().unwrap()
        }

        for select_b in [false, true] {
            let mut state = State::default();
            let a = native_work_item("branch-a", "project-a");
            let b = native_work_item("branch-b", "project-b");
            let other_b = native_work_item("other-b", "project-b");
            connected(
                &mut state,
                1,
                board(vec![a.clone(), b.clone(), other_b.clone()]),
            );
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let old_a = choose(&mut state, &mut renderer, "project-a");
            assert!(matches!(&old_a, Action::Scope(Some(path)) if *path == a.project));
            let current = choose(
                &mut state,
                &mut renderer,
                if select_b {
                    "project-b"
                } else {
                    "All projects"
                },
            );
            state.update(current);
            let scope = state.scope.clone();
            assert_eq!(scope, select_b.then(|| b.project.clone()));
            watched(&mut state, 2, board(vec![b.clone(), other_b]));

            assert!(state.update(old_a).is_none());
            assert_eq!(
                state.scope, scope,
                "late A must preserve the valid current scope"
            );
            assert!(!state.scope_menu);
            assert_eq!(
                state.visible().map(Item::label).collect::<Vec<_>>(),
                ["branch-b", "other-b"]
            );
            // Inspect the rendered header/status/body, not just the scope field.
            #[derive(Default)]
            struct Text(Vec<String>);
            impl iced::advanced::widget::Operation for Text {
                fn traverse(
                    &mut self,
                    visit: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation),
                ) {
                    visit(self);
                }
                fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
                    self.0.push(text.into());
                }
            }
            let mut ui = build(&state, Cache::new(), &mut renderer);
            let mut text = Text::default();
            ui.operate(&renderer, &mut text);
            assert!(text.0.iter().any(|text| text
                == if select_b {
                    "project-b"
                } else {
                    "All projects"
                }));
            assert!(text.0.iter().any(|text| text == "2 cards"));
            assert!(!text.0.iter().any(|text| text == "No cards match"));
            assert!(first_open_bounds(&ui, "Open branch-b").width > 0.0);
            drop(ui);

            // Current project and All projects callbacks remain accepted.
            let current_b = choose(&mut state, &mut renderer, "project-b");
            state.update(current_b);
            assert_eq!(state.scope, Some(b.project));
            assert!(!state.scope_menu);
            let all = choose(&mut state, &mut renderer, "All projects");
            state.update(all);
            assert!(state.scope.is_none());
            assert!(!state.scope_menu);
        }
    }

    #[test]
    fn a_pin_moves_the_card_at_once_and_names_it_to_the_daemon() {
        let mut state = State::default();
        watched(&mut state, 1, board(vec![item("feat/board", "cagents")]));
        let card = CardRef::of(&state.board.as_ref().unwrap().items[0]);
        let effect = state.update(Action::Pin(card.clone(), Some(Column::Stale)));
        assert!(matches!(
            effect,
            Some(Effect::Pin { ref item, column: Some(Column::Stale), .. }) if item == "branch:feat/board"
        ));
        let shown = state.item(&card).unwrap();
        assert_eq!((shown.column, shown.pinned), (Column::Stale, true));
        state.update(Action::Pin(card.clone(), None));
        let shown = state.item(&card).unwrap();
        assert_eq!((shown.column, shown.pinned), (shown.derived, false));
    }

    #[test]
    fn a_refused_pin_returns_the_card() {
        let mut state = State::default();
        watched(&mut state, 1, board(vec![item("feat/board", "cagents")]));
        let card = CardRef::of(&state.board.as_ref().unwrap().items[0]);
        let ticket = pin(&mut state, &card, Some(Column::Stale));
        state.update(Action::Pinned {
            ticket,
            result: Err("the Work board store is unavailable".into()),
        });
        let shown = state.item(&card).unwrap();
        assert_eq!((shown.column, shown.pinned), (Column::InProgress, false));
        assert!(state.pins.error.is_some());
    }

    fn pin(state: &mut State, card: &CardRef, column: Option<Column>) -> PinTicket {
        let Some(Effect::Pin {
            ticket,
            project,
            item,
            column: actual,
        }) = state.update(Action::Pin(card.clone(), column))
        else {
            panic!("the current card must emit a pin request")
        };
        assert_eq!(project, card.project);
        assert_eq!(item, card.key.storage_key());
        assert_eq!(actual, column);
        ticket
    }

    fn pin_board() -> (State, CardRef, CardRef) {
        let a = item("first", "cagents");
        let b = item("second", "cagents");
        let cards = (CardRef::of(&a), CardRef::of(&b));
        let mut state = State::default();
        connected(&mut state, 1, board(vec![a, b]));
        (state, cards.0, cards.1)
    }

    fn complete_pin(state: &mut State, ticket: PinTicket, result: Result<(), String>) {
        let action = Action::Pinned { ticket, result };
        assert!(
            action.background(),
            "a modal cannot strand an issued completion"
        );
        assert!(
            state.update(action).is_none(),
            "completion must not issue or retry a request"
        );
    }

    #[test]
    fn a_pin_failure_preserves_other_cards_even_when_their_success_arrives_first() {
        for success_first in [false, true] {
            let (mut state, a, b) = pin_board();
            state.update(Action::Refreshed(Err("current refresh failure".into())));
            let first = pin(&mut state, &a, Some(Column::Stale));
            let second = pin(&mut state, &b, Some(Column::Active));
            if success_first {
                complete_pin(&mut state, second.clone(), Ok(()));
            }
            complete_pin(&mut state, first, Err("first pin unavailable".into()));
            let shown_a = state.item(&a).unwrap();
            assert_eq!(
                (shown_a.column, shown_a.pinned),
                (Column::InProgress, false)
            );
            let shown_b = state.item(&b).unwrap();
            assert_eq!((shown_b.column, shown_b.pinned), (Column::Active, true));
            if !success_first {
                complete_pin(&mut state, second, Ok(()));
            }
            assert_eq!(state.item(&b).unwrap().column, Column::Active);
            assert!(state.pins.error.as_deref().unwrap().contains("first"));
            assert_eq!(state.error.as_deref(), Some("current refresh failure"));
            let next = pin(&mut state, &a, Some(Column::Landed));
            assert!(
                state.pins.error.is_none(),
                "a new pin clears only old pin feedback"
            );
            assert_eq!(state.error.as_deref(), Some("current refresh failure"));
            complete_pin(&mut state, next, Ok(()));
        }
    }

    #[test]
    fn a_pin_failure_uses_current_placement_and_keeps_new_board_facts() {
        let (mut state, a, b) = pin_board();
        let first = pin(&mut state, &a, Some(Column::Stale));
        let mut newer_a = item("first", "cagents");
        newer_a.project_name = "Fresh project name".into();
        newer_a.comparison.as_mut().unwrap().ahead = 9;
        newer_a.column = Column::InReview;
        newer_a.derived = Column::Active;
        newer_a.pinned = true;
        let authoritative = board(vec![newer_a.clone(), item("second", "cagents")]);
        watched(&mut state, 2, authoritative.clone());
        assert_eq!(
            state.board.as_deref(),
            Some(authoritative.as_ref()),
            "watcher state wins without replaying old optimistic intent"
        );
        let second = pin(&mut state, &b, Some(Column::Landed));
        complete_pin(&mut state, first, Err("response missing".into()));
        assert_eq!(
            state.item(&a),
            Some(&newer_a),
            "restore current placement without copying old facts"
        );
        assert_eq!(state.item(&b).unwrap().column, Column::Landed);
        assert_eq!(state.confirmed.as_deref(), Some(authoritative.as_ref()));
        let new_pin = pin(&mut state, &a, Some(Column::Stale));
        assert_ne!(state.item(&a).unwrap().derived, newer_a.derived);
        complete_pin(&mut state, new_pin, Err("new response missing".into()));
        assert_eq!(
            state.item(&a),
            Some(&newer_a),
            "all three placement fields return to the current observation"
        );
        complete_pin(&mut state, second, Err("second response missing".into()));
        assert_eq!(state.board.as_deref(), Some(authoritative.as_ref()));
    }

    #[test]
    fn old_pin_completions_do_not_cross_reset_loss_or_watch_connection_lifetimes() {
        for boundary in ["reset", "lost", "reconnect"] {
            let (mut state, a, b) = pin_board();
            let old = pin(&mut state, &a, Some(Column::Stale));
            match boundary {
                "reset" => state.reset(),
                "lost" => {
                    state.update(Action::Watched(Watch::Lost("disconnected".into())));
                }
                "reconnect" => connected(
                    &mut state,
                    1,
                    board(vec![item("first", "cagents"), item("second", "cagents")]),
                ),
                _ => unreachable!(),
            }
            complete_pin(&mut state, old.clone(), Err("old request failure".into()));
            assert!(state.pins.error.is_none());
            if boundary == "reset" {
                assert!(
                    state.board.is_none(),
                    "completion cannot reopen the closed board"
                );
            }
            if boundary != "reconnect" {
                connected(
                    &mut state,
                    1,
                    board(vec![item("first", "cagents"), item("second", "cagents")]),
                );
            }
            state.update(Action::Refreshed(Err("new refresh failure".into())));
            let current = pin(&mut state, &b, Some(Column::Active));
            complete_pin(&mut state, old.clone(), Err("late old failure".into()));
            complete_pin(&mut state, old, Ok(()));
            assert_eq!(state.item(&b).unwrap().column, Column::Active);
            assert!(state.pins.error.is_none());
            assert_eq!(state.error.as_deref(), Some("new refresh failure"));
            complete_pin(&mut state, current, Err("current request missing".into()));
            assert!(
                state
                    .pins
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("current request missing"),
                "the current lifetime still accepts its matching callback"
            );
            connected(
                &mut state,
                1,
                board(vec![item("first", "cagents"), item("second", "cagents")]),
            );
            assert!(
                state.pins.error.is_none(),
                "new watch lifetime clears only pin feedback"
            );
            assert_eq!(state.error.as_deref(), Some("new refresh failure"));
        }
    }

    #[test]
    fn superseded_pin_completions_cannot_undo_or_retire_the_latest_same_card_request() {
        for latest_succeeds in [false, true] {
            let (mut state, a, _) = pin_board();
            let old = pin(&mut state, &a, Some(Column::Stale));
            let latest = pin(&mut state, &a, Some(Column::Active));
            complete_pin(&mut state, old.clone(), Err("superseded failure".into()));
            complete_pin(&mut state, old.clone(), Ok(()));
            assert_eq!(state.item(&a).unwrap().column, Column::Active);
            assert!(state.pins.error.is_none());
            complete_pin(
                &mut state,
                latest,
                if latest_succeeds {
                    Ok(())
                } else {
                    Err("latest failure".into())
                },
            );
            let expected = if latest_succeeds {
                Column::Active
            } else {
                Column::InProgress
            };
            assert_eq!(state.item(&a).unwrap().column, expected);
            let feedback = state.pins.error.clone();
            complete_pin(
                &mut state,
                old,
                Err("superseded failure after latest".into()),
            );
            assert_eq!(state.item(&a).unwrap().column, expected);
            assert_eq!(state.pins.error, feedback);
        }
    }

    #[test]
    fn a_missing_pinned_card_is_not_resurrected_by_its_late_failure() {
        for reappears in [false, true] {
            let (mut state, a, b) = pin_board();
            let removed = pin(&mut state, &a, Some(Column::Stale));
            watched(&mut state, 2, board(vec![item("second", "cagents")]));
            if reappears {
                let mut replacement = item("first", "cagents");
                replacement.head = Some(crate::work::Head {
                    oid: "replacement-head".into(),
                    committed_at_unix_ms: None,
                });
                watched(
                    &mut state,
                    3,
                    board(vec![replacement, item("second", "cagents")]),
                );
            }
            let before_a = state.item(&a).cloned();
            let retained = pin(&mut state, &b, Some(Column::Active));
            complete_pin(&mut state, removed, Err("removed card failure".into()));
            assert_eq!(
                state.item(&a),
                before_a.as_ref(),
                "no resurrection or adoption by a reappeared card"
            );
            assert_eq!(state.item(&b).unwrap().column, Column::Active);
            assert!(state.pins.error.is_none());
            complete_pin(&mut state, retained, Err("retained card failure".into()));
            assert_eq!(state.item(&b).unwrap().column, Column::InProgress);
            assert!(
                state
                    .pins
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("retained card failure")
            );
        }
    }

    #[test]
    fn a_card_menu_closes_when_its_card_leaves_the_board() {
        let mut state = State::default();
        watched(&mut state, 1, board(vec![item("feat/board", "cagents")]));
        let card = CardRef::of(&state.board.as_ref().unwrap().items[0]);
        state.update(Action::Menu(Some(card.clone())));
        watched(&mut state, 2, board(vec![item("feat/board", "cagents")]));
        assert_eq!(state.menu.as_ref(), Some(&card));
        watched(&mut state, 3, board(vec![item("feat/other", "cagents")]));
        assert_eq!(state.menu, None);
        // A card that comes back does not reopen its menu.
        watched(&mut state, 4, board(vec![item("feat/board", "cagents")]));
        assert_eq!(state.menu, None);
    }

    // Keep the actual UserInterface cache across accepted watch updates. A
    // new Tree for each click cannot expose a held press moving with a slot.
    fn cached_work_ui<'a>(
        state: &'a State,
        cache: iced::runtime::user_interface::Cache,
        renderer: &mut cosmic::Renderer,
    ) -> iced::runtime::UserInterface<'a, Action, Theme, cosmic::Renderer> {
        let element = match state.layout {
            Layout::Board => state.board_view(1600.0),
            Layout::List => state.list_view(),
        };
        let mut ui = iced::runtime::UserInterface::build(
            element,
            iced::Size::new(1600.0, 900.0),
            cache,
            renderer,
        );
        draw_work_ui(&mut ui, renderer);
        ui
    }

    fn draw_work_ui(
        ui: &mut iced::runtime::UserInterface<'_, Action, Theme, cosmic::Renderer>,
        renderer: &mut cosmic::Renderer,
    ) {
        ui.draw(
            renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            iced::mouse::Cursor::Unavailable,
        );
    }

    fn first_open_bounds(
        ui: &iced::runtime::UserInterface<'_, Action, Theme, cosmic::Renderer>,
        label: &str,
    ) -> iced::Rectangle {
        let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
        let mut bounds = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter(|node| node.node().label() == Some(label))
            .map(|node| {
                let b = node.node().bounds().unwrap();
                iced::Rectangle {
                    x: b.x0 as f32,
                    y: b.y0 as f32,
                    width: (b.x1 - b.x0) as f32,
                    height: (b.y1 - b.y0) as f32,
                }
            })
            .collect::<Vec<_>>();
        bounds.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
        let bounds = *bounds
            .first()
            .unwrap_or_else(|| panic!("missing actual card button {label}"));
        assert!(
            bounds.width > 0.0 && bounds.height > 0.0,
            "{label}: {bounds:?}"
        );
        bounds
    }

    fn retained_work_events(
        ui: &mut iced::runtime::UserInterface<'_, Action, Theme, cosmic::Renderer>,
        renderer: &mut cosmic::Renderer,
        point: iced::Point,
        buttons: &[iced::mouse::Event],
    ) -> Vec<Action> {
        let events = std::iter::once(iced::Event::Mouse(iced::mouse::Event::CursorMoved {
            position: point,
        }))
        .chain(buttons.iter().cloned().map(iced::Event::Mouse))
        .collect::<Vec<_>>();
        let mut messages = Vec::new();
        let _ = ui.update(
            &events,
            iced::mouse::Cursor::Available(point),
            renderer,
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        draw_work_ui(ui, renderer);
        messages
    }

    fn native_work_item(branch: &str, project: &str) -> Item {
        let mut value = item(branch, project);
        value.worktree = Some(crate::work::Worktree {
            path: PathBuf::from(format!("/fixture/{project}/{branch}")),
            primary: false,
            locked: false,
            created_at_unix_ms: None,
        });
        value
    }

    fn assert_native_open(state: &mut State, messages: Vec<Action>, expected: &Item) {
        assert!(
            matches!(messages.as_slice(), [Action::Activate(activation)] if Some(activation) == Activation::primary(expected, state.lifetime).as_ref()),
            "expected current card, received {messages:?}"
        );
        let effect = state.update(messages.into_iter().next().unwrap());
        assert!(
            matches!(effect, Some(Effect::OpenWorktree{project,path}) if project == expected.project && path == expected.worktree.as_ref().unwrap().path)
        );
        // Inspect the reducer effect only; never open a real project or terminal.
    }

    #[test]
    fn reordered_board_and_list_cards_cannot_adopt_another_cards_native_press() {
        use iced::mouse::{Button, Event};
        use iced::runtime::user_interface::Cache;
        for layout in [Layout::Board, Layout::List] {
            let a = native_work_item("branch-a", "workspace");
            let b = native_work_item("branch-b", "workspace");
            let mut state = State::default();
            state.update(Action::Layout(layout));
            watched(&mut state, 1, board(vec![a.clone(), b.clone()]));
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let mut ui = cached_work_ui(&state, Cache::new(), &mut renderer);
            let point = first_open_bounds(&ui, "Open branch-a").center();
            assert!(
                retained_work_events(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonPressed(Button::Left)]
                )
                .is_empty()
            );
            let cache = ui.into_cache();
            watched(&mut state, 2, board(vec![b.clone(), a]));
            assert_eq!(state.visible().next().unwrap().key, b.key);
            let mut ui = cached_work_ui(&state, cache, &mut renderer);
            assert!(
                first_open_bounds(&ui, "Open branch-b").contains(point),
                "replacement must occupy the held hit area in {layout:?}"
            );
            let release = retained_work_events(
                &mut ui,
                &mut renderer,
                point,
                &[Event::ButtonReleased(Button::Left)],
            );
            assert!(
                release.is_empty(),
                "held A must not open B in {layout:?}: {release:?}"
            );
            let fresh = retained_work_events(
                &mut ui,
                &mut renderer,
                point,
                &[
                    Event::ButtonPressed(Button::Left),
                    Event::ButtonReleased(Button::Left),
                ],
            );
            drop(ui);
            assert_native_open(&mut state, fresh, &b);
        }
    }

    #[test]
    fn stable_board_and_list_card_identity_keeps_its_native_press_on_refresh() {
        use iced::mouse::{Button, Event};
        use iced::runtime::user_interface::Cache;
        for layout in [Layout::Board, Layout::List] {
            let a = native_work_item("branch-a", "workspace");
            let b = native_work_item("branch-b", "workspace");
            let mut refreshed = a.clone();
            refreshed.comparison.as_mut().unwrap().ahead = 2;
            let mut state = State::default();
            state.update(Action::Layout(layout));
            watched(&mut state, 1, board(vec![a.clone(), b.clone()]));
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let mut ui = cached_work_ui(&state, Cache::new(), &mut renderer);
            let point = first_open_bounds(&ui, "Open branch-a").center();
            assert!(
                retained_work_events(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonPressed(Button::Left)]
                )
                .is_empty()
            );
            let cache = ui.into_cache();
            watched(&mut state, 2, board(vec![refreshed.clone(), b]));
            let mut ui = cached_work_ui(&state, cache, &mut renderer);
            assert!(first_open_bounds(&ui, "Open branch-a").contains(point));
            let release = retained_work_events(
                &mut ui,
                &mut renderer,
                point,
                &[Event::ButtonReleased(Button::Left)],
            );
            drop(ui);
            assert_native_open(&mut state, release, &refreshed);
        }
    }

    #[test]
    fn equal_work_labels_in_different_projects_do_not_share_a_native_press() {
        use iced::mouse::{Button, Event};
        use iced::runtime::user_interface::Cache;
        for layout in [Layout::Board, Layout::List] {
            let mut a = native_work_item("shared", "project-a");
            let mut b = native_work_item("shared", "project-b");
            a.project_name = "Same name".into();
            b.project_name = a.project_name.clone();
            assert_eq!(a.key, b.key);
            assert_eq!(a.label(), b.label());
            assert_ne!(CardRef::of(&a), CardRef::of(&b));
            let mut state = State::default();
            state.update(Action::Layout(layout));
            watched(&mut state, 1, board(vec![a.clone(), b.clone()]));
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let mut ui = cached_work_ui(&state, Cache::new(), &mut renderer);
            let point = first_open_bounds(&ui, "Open shared").center();
            assert!(
                retained_work_events(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonPressed(Button::Left)]
                )
                .is_empty()
            );
            let cache = ui.into_cache();
            watched(&mut state, 2, board(vec![b.clone(), a]));
            let mut ui = cached_work_ui(&state, cache, &mut renderer);
            assert!(first_open_bounds(&ui, "Open shared").contains(point));
            let release = retained_work_events(
                &mut ui,
                &mut renderer,
                point,
                &[Event::ButtonReleased(Button::Left)],
            );
            assert!(
                release.is_empty(),
                "same labels must not transfer A's press to B in {layout:?}: {release:?}"
            );
            let fresh = retained_work_events(
                &mut ui,
                &mut renderer,
                point,
                &[
                    Event::ButtonPressed(Button::Left),
                    Event::ButtonReleased(Button::Left),
                ],
            );
            drop(ui);
            assert_native_open(&mut state, fresh, &b);
        }
    }

    fn request(number: u64) -> PullRequest {
        PullRequest {
            number,
            title: "Same title".into(),
            url: format!("https://github.com/fixture/work/pull/{number}"),
            state: PullRequestState::Open,
            draft: false,
            head_oid: "head".into(),
            contains_local_head: true,
            checks: None,
            review: None,
            updated_at: String::new(),
        }
    }

    fn click_work(
        ui: &mut iced::runtime::UserInterface<'_, Action, Theme, cosmic::Renderer>,
        renderer: &mut cosmic::Renderer,
        point: iced::Point,
    ) -> Vec<Action> {
        use iced::mouse::{Button, Event};
        retained_work_events(
            ui,
            renderer,
            point,
            &[
                Event::ButtonPressed(Button::Left),
                Event::ButtonReleased(Button::Left),
            ],
        )
    }

    // a11y_nodes visits only the popover anchor. Operate traverses the actual
    // overlay tree and exposes each real button's bounds after its text.
    fn popup_button_bounds(
        ui: &mut iced::runtime::UserInterface<'_, Action, Theme, cosmic::Renderer>,
        renderer: &cosmic::Renderer,
        label: &str,
    ) -> iced::Rectangle {
        use iced::advanced::widget::{Operation, operation};
        #[derive(Default)]
        struct Outline {
            text: Vec<(String, iced::Rectangle)>,
            buttons: Vec<iced::Rectangle>,
        }
        impl Operation for Outline {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
                self.text.push((text.into(), bounds));
            }
            fn focusable(
                &mut self,
                _: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: &mut dyn operation::Focusable,
            ) {
                self.buttons.push(bounds);
            }
        }
        let mut outline = Outline::default();
        ui.operate(renderer, &mut outline);
        let texts = outline
            .text
            .iter()
            .filter(|(text, _)| text == label)
            .map(|(_, bounds)| *bounds)
            .collect::<Vec<_>>();
        assert!(
            !texts.is_empty(),
            "missing actual popup text {label}: {:?}",
            outline.text
        );
        let bounds = outline
            .buttons
            .iter()
            .filter(|bounds| texts.iter().any(|text| bounds.contains(text.center())))
            .min_by(|a, b| (a.width * a.height).total_cmp(&(b.width * b.height)))
            .copied()
            .unwrap_or_else(|| panic!("no actual popup button for {label}: {texts:?}"));
        assert!(
            bounds.width > 0.0 && bounds.height > 0.0,
            "{label}: {bounds:?}"
        );
        bounds
    }

    fn open_native_menu(
        state: &mut State,
        renderer: &mut cosmic::Renderer,
    ) -> iced::runtime::user_interface::Cache {
        use iced::runtime::user_interface::Cache;
        let mut ui = cached_work_ui(state, Cache::new(), renderer);
        let point = first_open_bounds(&ui, "Card actions").center();
        let actions = click_work(&mut ui, renderer, point);
        let cache = ui.into_cache();
        assert!(
            matches!(actions.as_slice(), [Action::Menu(Some(_))]),
            "menu anchor: {actions:?}"
        );
        state.update(actions.into_iter().next().unwrap());
        cache
    }

    #[test]
    fn same_card_changed_primary_destination_retires_board_and_list_native_press() {
        use iced::mouse::{Button, Event};
        use iced::runtime::user_interface::Cache;
        for layout in [Layout::Board, Layout::List] {
            for primary_pull_request in [false, true] {
                let mut a = native_work_item("same-branch", "workspace");
                if primary_pull_request {
                    a.worktree = None;
                    a.pull_request = Some(request(42));
                }
                let mut b = a.clone();
                if primary_pull_request {
                    b.pull_request = Some(request(43));
                } else {
                    b.worktree.as_mut().unwrap().path = "/fixture/workspace/new-target".into();
                }
                assert_eq!(CardRef::of(&a), CardRef::of(&b));
                let mut state = State::default();
                state.update(Action::Layout(layout));
                watched(&mut state, 1, board(vec![a]));
                let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
                let mut ui = cached_work_ui(&state, Cache::new(), &mut renderer);
                let point = first_open_bounds(&ui, "Open same-branch").center();
                assert!(
                    retained_work_events(
                        &mut ui,
                        &mut renderer,
                        point,
                        &[Event::ButtonPressed(Button::Left)]
                    )
                    .is_empty()
                );
                let cache = ui.into_cache();
                watched(&mut state, 2, board(vec![b.clone()]));
                let mut ui = cached_work_ui(&state, cache, &mut renderer);
                assert!(first_open_bounds(&ui, "Open same-branch").contains(point));
                let release = retained_work_events(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonReleased(Button::Left)],
                );
                assert!(
                    release.is_empty(),
                    "same card retarget in {layout:?}: {release:?}"
                );
                let fresh = click_work(&mut ui, &mut renderer, point);
                drop(ui);
                assert!(
                    matches!(fresh.as_slice(), [Action::Activate(a)] if Some(a) == Activation::primary(&b, state.lifetime).as_ref()),
                    "{fresh:?}"
                );
                let effect = state.update(fresh.into_iter().next().unwrap());
                if primary_pull_request {
                    assert!(
                        matches!(effect, Some(Effect::OpenPullRequest { number: 43, ref url, .. }) if url == &b.pull_request.as_ref().unwrap().url)
                    );
                } else {
                    assert!(
                        matches!(effect, Some(Effect::OpenWorktree { ref path, .. }) if path == &b.worktree.as_ref().unwrap().path)
                    );
                }
            }
        }
    }

    #[test]
    fn changed_secondary_menu_destinations_cannot_adopt_native_presses_or_queued_actions() {
        use iced::mouse::{Button, Event};
        for label in [
            "Open worktree",
            "Open pull request",
            "Open Linear issue",
            "Copy branch name",
        ] {
            let mut a = native_work_item("same-branch", "workspace");
            a.pull_request = Some(request(42));
            a.linear = Some(crate::work::LinearTicket {
                identifier: Some("FIX-1".into()),
                title: None,
                url: Some("https://linear.app/fixture/issue/FIX-1".into()),
                source: crate::work::TicketSource::Link,
            });
            let mut b = a.clone();
            match label {
                "Open worktree" => {
                    b.worktree.as_mut().unwrap().path = "/fixture/workspace/other-target".into()
                }
                "Open pull request" => b.pull_request = Some(request(43)),
                "Open Linear issue" => {
                    b.linear.as_mut().unwrap().url =
                        Some("https://linear.app/fixture/issue/FIX-2".into())
                }
                _ => b.branch = Some("new-branch-name".into()),
            }
            let lifetime = ActivationLifetime::default();
            if label != "Open worktree" {
                assert_eq!(
                    Activation::primary(&a, lifetime),
                    Activation::primary(&b, lifetime),
                    "secondary change must not reset outer button"
                );
            }
            let mut state = State::default();
            watched(&mut state, 1, board(vec![a]));
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let cache = open_native_menu(&mut state, &mut renderer);
            let mut ui = cached_work_ui(&state, cache, &mut renderer);
            let point = popup_button_bounds(&mut ui, &renderer, label).center();
            // Capture a real emitted action, but delay delivery to the reducer.
            let queued = click_work(&mut ui, &mut renderer, point);
            assert!(
                matches!(queued.as_slice(), [Action::Activate(_)]),
                "{label}: {queued:?}"
            );
            assert!(
                retained_work_events(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonPressed(Button::Left)]
                )
                .is_empty()
            );
            let cache = ui.into_cache();
            watched(&mut state, 2, board(vec![b.clone()]));
            let mut ui = cached_work_ui(&state, cache, &mut renderer);
            assert!(popup_button_bounds(&mut ui, &renderer, label).contains(point));
            let release = retained_work_events(
                &mut ui,
                &mut renderer,
                point,
                &[Event::ButtonReleased(Button::Left)],
            );
            assert!(
                release.is_empty(),
                "{label} replacement adopted a press: {release:?}"
            );
            let fresh = click_work(&mut ui, &mut renderer, point);
            drop(ui);
            let menu = state.menu.clone();
            assert!(state.update(queued.into_iter().next().unwrap()).is_none());
            assert_eq!(
                state.menu, menu,
                "stale action must not close current popup"
            );
            assert!(
                matches!(fresh.as_slice(), [Action::Activate(_)]),
                "{label}: {fresh:?}"
            );
            let effect = state.update(fresh.into_iter().next().unwrap());
            match label {
                "Open worktree" => assert!(
                    matches!(effect, Some(Effect::OpenWorktree {ref path,..}) if path == &b.worktree.as_ref().unwrap().path)
                ),
                "Open pull request" => assert!(matches!(
                    effect,
                    Some(Effect::OpenPullRequest { number: 43, .. })
                )),
                "Open Linear issue" => assert!(
                    matches!(effect, Some(Effect::OpenUrl(ref url)) if Some(url) == b.linear.as_ref().unwrap().url.as_ref())
                ),
                _ => assert!(
                    matches!(effect, Some(Effect::Copy(ref branch)) if Some(branch) == b.branch.as_ref())
                ),
            }
        }
    }

    #[test]
    fn reordered_footer_sessions_keep_native_press_and_queued_focus_with_exact_session() {
        use iced::mouse::{Button, Event};
        use iced::runtime::user_interface::Cache;
        let mut a = native_work_item("same-branch", "workspace");
        let one = TerminalId::new();
        let two = TerminalId::new();
        a.sessions = [one, two]
            .into_iter()
            .enumerate()
            .map(|(i, id)| crate::work::Session {
                id,
                provider: None,
                title: Some(format!("Session {}", i + 1)),
                state: ActivityState::Waiting,
            })
            .collect();
        let mut b = a.clone();
        b.sessions.swap(0, 1);
        let mut state = State::default();
        watched(&mut state, 1, board(vec![a.clone()]));
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut ui = cached_work_ui(&state, Cache::new(), &mut renderer);
        let point = first_open_bounds(&ui, "Focus Session 1").center();
        let queued = click_work(&mut ui, &mut renderer, point);
        assert!(
            matches!(queued.as_slice(), [Action::Activate(a)] if a.destination == Destination::Session(one)),
            "footer must not bubble Open: {queued:?}"
        );
        assert!(
            retained_work_events(
                &mut ui,
                &mut renderer,
                point,
                &[Event::ButtonPressed(Button::Left)]
            )
            .is_empty()
        );
        let cache = ui.into_cache();
        watched(&mut state, 2, board(vec![b.clone()]));
        let mut ui = cached_work_ui(&state, cache, &mut renderer);
        assert!(first_open_bounds(&ui, "Focus Session 2").contains(point));
        let release = retained_work_events(
            &mut ui,
            &mut renderer,
            point,
            &[Event::ButtonReleased(Button::Left)],
        );
        assert!(
            release.is_empty(),
            "footer replacement adopted press/bubbled Open: {release:?}"
        );
        let fresh = click_work(&mut ui, &mut renderer, point);
        drop(ui);
        assert!(
            matches!(fresh.as_slice(), [Action::Activate(a)] if a.destination == Destination::Session(two)),
            "fresh exact session: {fresh:?}"
        );
        assert!(
            matches!(state.update(fresh.into_iter().next().unwrap()), Some(Effect::FocusSession(id)) if id == two)
        );
        // Reorder preserves original membership; the queued S1 still names S1.
        assert!(
            matches!(state.update(queued[0].clone()), Some(Effect::FocusSession(id)) if id == one)
        );
        b.sessions.retain(|session| session.id != one);
        watched(&mut state, 3, board(vec![b]));
        assert!(state.update(queued.into_iter().next().unwrap()).is_none());
    }

    #[test]
    fn queued_primary_actions_refuse_missing_or_changed_destinations_without_fallback() {
        use iced::runtime::user_interface::Cache;
        for kind in [0, 1, 2, 3, 4] {
            let mut a = native_work_item("same-branch", "workspace");
            if kind >= 2 {
                a.worktree = None;
                a.pull_request = Some(request(42));
            }
            let mut state = State::default();
            watched(&mut state, 1, board(vec![a.clone()]));
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let mut ui = cached_work_ui(&state, Cache::new(), &mut renderer);
            let point = first_open_bounds(&ui, "Open same-branch").center();
            let queued = click_work(&mut ui, &mut renderer, point);
            drop(ui);
            assert!(
                matches!(queued.as_slice(), [Action::Activate(_)]),
                "{queued:?}"
            );
            let mut b = a.clone();
            match kind {
                0 => b.worktree.as_mut().unwrap().path = "/fixture/retarget".into(),
                1 => {} // card removed below
                2 => b
                    .pull_request
                    .as_mut()
                    .unwrap()
                    .url
                    .push_str("?different-target"),
                3 => b.worktree = native_work_item("other-target", "workspace").worktree,
                _ => {
                    b.comparison.as_mut().unwrap().ahead += 1;
                    b.pull_request
                        .as_mut()
                        .unwrap()
                        .title
                        .push_str(" refreshed");
                }
            }
            watched(
                &mut state,
                2,
                board(if kind == 1 { vec![] } else { vec![b] }),
            );
            let effect = state.update(queued.into_iter().next().unwrap());
            if kind == 4 {
                assert!(
                    matches!(effect, Some(Effect::OpenPullRequest { number: 42, .. })),
                    "same target must survive unrelated metadata"
                );
            } else {
                assert!(
                    effect.is_none(),
                    "queued kind {kind} redirected: {effect:?}"
                );
            }
        }
    }

    #[test]
    fn queued_activations_cannot_cross_a_reset_and_identical_board_reopen() {
        use iced::runtime::user_interface::Cache;
        let a = native_work_item("same-branch", "workspace");
        let mut state = State::default();
        watched(&mut state, 1, board(vec![a.clone()]));
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut ui = cached_work_ui(&state, Cache::new(), &mut renderer);
        let point = first_open_bounds(&ui, "Open same-branch").center();
        let old = click_work(&mut ui, &mut renderer, point);
        let cache = ui.into_cache();
        assert!(matches!(old.as_slice(), [Action::Activate(_)]), "{old:?}");
        let lifetime = state.lifetime;
        state.reset();
        watched(&mut state, 1, board(vec![a.clone()]));
        assert_ne!(state.lifetime, lifetime);
        assert!(state.update(old.into_iter().next().unwrap()).is_none());
        let mut ui = cached_work_ui(&state, cache, &mut renderer);
        let point = first_open_bounds(&ui, "Open same-branch").center();
        let fresh = click_work(&mut ui, &mut renderer, point);
        drop(ui);
        assert_native_open(&mut state, fresh, &a);
    }

    #[test]
    fn unchanged_secondary_destinations_keep_native_presses_through_metadata_updates() {
        use iced::mouse::{Button, Event};
        use iced::runtime::user_interface::Cache;
        for popup in [false, true] {
            let mut a = native_work_item("same-branch", "workspace");
            a.pull_request = Some(request(42));
            let id = TerminalId::new();
            a.sessions.push(crate::work::Session {
                id,
                provider: None,
                title: Some("Session".into()),
                state: ActivityState::Waiting,
            });
            let mut b = a.clone();
            b.pull_request
                .as_mut()
                .unwrap()
                .title
                .push_str(" refreshed");
            b.sessions[0].title = Some("Renamed session".into());
            b.sessions[0].state = ActivityState::Idle;
            b.comparison.as_mut().unwrap().ahead += 1;
            let mut state = State::default();
            watched(&mut state, 1, board(vec![a]));
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let cache = if popup {
                open_native_menu(&mut state, &mut renderer)
            } else {
                Cache::new()
            };
            let mut ui = cached_work_ui(&state, cache, &mut renderer);
            let point = if popup {
                popup_button_bounds(&mut ui, &renderer, "Open pull request").center()
            } else {
                first_open_bounds(&ui, "Focus Session").center()
            };
            assert!(
                retained_work_events(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonPressed(Button::Left)]
                )
                .is_empty()
            );
            let cache = ui.into_cache();
            watched(&mut state, 2, board(vec![b]));
            let mut ui = cached_work_ui(&state, cache, &mut renderer);
            let bounds = if popup {
                popup_button_bounds(&mut ui, &renderer, "Open pull request")
            } else {
                first_open_bounds(&ui, "Focus Renamed session")
            };
            assert!(bounds.contains(point));
            let release = retained_work_events(
                &mut ui,
                &mut renderer,
                point,
                &[Event::ButtonReleased(Button::Left)],
            );
            drop(ui);
            assert!(
                matches!(release.as_slice(), [Action::Activate(_)]),
                "same target native state must survive: {release:?}"
            );
            let effect = state.update(release.into_iter().next().unwrap());
            if popup {
                assert!(matches!(
                    effect,
                    Some(Effect::OpenPullRequest { number: 42, .. })
                ));
            } else {
                assert!(matches!(effect, Some(Effect::FocusSession(actual)) if actual == id));
            }
        }
    }

    #[test]
    fn inserted_menu_rows_cannot_transfer_a_native_pin_to_another_column() {
        use iced::mouse::{Button, Event};
        for change in ["insert row", "reconnect", "lost", "metadata"] {
            let a = native_work_item("same-branch", "workspace");
            let mut b = a.clone();
            if change == "insert row" {
                b.pull_request = Some(request(42));
            }
            if change == "metadata" {
                b.comparison.as_mut().unwrap().ahead += 1;
            }
            let mut state = State::default();
            state.update(Action::Layout(Layout::List));
            watched(&mut state, 1, board(vec![a]));
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let cache = open_native_menu(&mut state, &mut renderer);
            let mut ui = cached_work_ui(&state, cache, &mut renderer);
            let point = popup_button_bounds(&mut ui, &renderer, "In review").center();
            assert!(
                retained_work_events(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonPressed(Button::Left)]
                )
                .is_empty()
            );
            let cache = ui.into_cache();
            let activation_lifetime = state.lifetime;
            let pin_lifetime = state.pins.lifetime;
            if change == "lost" {
                state.update(Action::Watched(Watch::Lost("fixture watch lost".into())));
            }
            if change == "reconnect" {
                connected(&mut state, 1, board(vec![b.clone()]));
            } else {
                watched(&mut state, 2, board(vec![b.clone()]));
            }
            assert_eq!(
                state.lifetime, activation_lifetime,
                "watch changes do not retire destination activations"
            );
            if matches!(change, "reconnect" | "lost") {
                assert_ne!(state.pins.lifetime, pin_lifetime);
            } else {
                assert_eq!(state.pins.lifetime, pin_lifetime);
            }
            let mut ui = cached_work_ui(&state, cache, &mut renderer);
            let replacement_column = if change == "insert row" {
                Column::InProgress
            } else {
                Column::InReview
            };
            let replacement = popup_button_bounds(&mut ui, &renderer, replacement_column.label());
            assert!(
                replacement.contains(point),
                "{change}: actual replacement must remain at oldpoint: {replacement:?} {point:?}"
            );
            let release = retained_work_events(
                &mut ui,
                &mut renderer,
                point,
                &[Event::ButtonReleased(Button::Left)],
            );
            if change == "metadata" {
                assert!(
                    matches!(release.as_slice(), [Action::Pin(card, Some(Column::InReview))] if card == &CardRef::of(&b)),
                    "ordinary same-owner metadata Watch must keep held positive: {release:?}"
                );
            } else {
                assert!(
                    release.is_empty(),
                    "{change}: retired pin native owner adopted press: {release:?}"
                );
            }
            let fresh_replacement = click_work(&mut ui, &mut renderer, point);
            assert!(
                matches!(fresh_replacement.as_slice(), [Action::Pin(card, Some(column))] if card == &CardRef::of(&b) && *column == replacement_column),
                "{fresh_replacement:?}"
            );
            let intended = popup_button_bounds(&mut ui, &renderer, "In review").center();
            let fresh = click_work(&mut ui, &mut renderer, intended);
            drop(ui);
            assert!(
                matches!(fresh.as_slice(), [Action::Pin(card, Some(Column::InReview))] if card == &CardRef::of(&b)),
                "{fresh:?}"
            );
            assert!(matches!(
                state.update(fresh.into_iter().next().unwrap()),
                Some(Effect::Pin {
                    column: Some(Column::InReview),
                    ..
                })
            ));
            // Existing Pin callbacks/reducer are unchanged. This checks local
            // native ownership only, not queued Pin or server request ordering.
        }
    }

    #[test]
    fn ages_are_compact() {
        let minute = 60_000;
        assert_eq!(age(10 * minute, 10 * minute - 1_000, false), "now");
        assert_eq!(age(10 * minute, 5 * minute, true), "5m ago");
        assert_eq!(age(3 * 60 * minute, 0, false), "—");
        assert_eq!(
            age(30 * 24 * 60 * minute, 27 * 24 * 60 * minute, false),
            "3d"
        );
    }

    fn removable(path: &str) -> Item {
        let mut item = item("feature", "cagents");
        item.worktree = Some(crate::work::Worktree {
            path: path.into(),
            primary: false,
            locked: false,
            created_at_unix_ms: None,
        });
        item
    }

    fn prepared_removal(subject: &RemovalSubject) -> PreparedPlan {
        use crate::model::{BootId, HostIncarnation, HostSessionId};
        PreparedPlan {
            backend: BackendTarget {
                socket: "/run/test/control.sock".into(),
                proof: crate::model::HostSessionProof {
                    host_session_id: HostSessionId::new("host"),
                    boot_id: BootId::new("boot"),
                    host_incarnation: HostIncarnation::new("incarnation"),
                },
            },
            plan: crate::work::RemovalPlan {
                project: subject.card.project.clone(),
                project_name: "cagents".into(),
                item: "feature".into(),
                merged_pull_request: None,
                inspection: crate::worktree_content::Inspection {
                    path: subject.path.clone(),
                    branch: subject.branch.clone(),
                    head: Some("head".into()),
                    verdict: crate::worktree_content::Verdict::Landed,
                    landed_in: Some("origin/main".into()),
                    staged: 0,
                    modified: 0,
                    untracked: 0,
                    untracked_bytes: 0,
                    filtered: vec![],
                    nested: vec![],
                    ignored: vec![],
                    ignored_total: 0,
                    token: Some("reviewed".into()),
                },
            },
        }
    }

    fn removal_review(state: &mut State, item: &Item) -> RemovalSubject {
        let subject = RemovalSubject::of(item).unwrap();
        let Some(Effect::Removal(RemovalEffect::Plan { generation, .. })) = state.update(
            RemovalAction::Open {
                generation: state.removal.generation(),
                subject: subject.clone(),
            }
            .into(),
        ) else {
            panic!("plan only")
        };
        state.update(
            RemovalAction::Planned {
                generation,
                result: Ok(prepared_removal(&subject)),
            }
            .into(),
        );
        subject
    }

    #[test]
    fn branch_card_recreation_cannot_retarget_review_or_a_rendered_open_callback() {
        let mut state = State::default();
        let original = removable("/work/original");
        connected(&mut state, 1, board(vec![original.clone()]));
        let subject = removal_review(&mut state, &original);
        let generation = state.removal.generation();
        watched(&mut state, 2, board(vec![removable("/work/replacement")]));
        state.update(
            RemovalAction::DeleteBranch {
                generation,
                delete: true,
            }
            .into(),
        );
        assert_eq!(
            state.removal.generation(),
            generation,
            "a stale toggle must be ignored too"
        );
        assert!(
            state
                .update(RemovalAction::Confirm { generation }.into())
                .is_none()
        );
        assert!(
            state
                .update(
                    RemovalAction::Open {
                        generation,
                        subject: subject.clone()
                    }
                    .into()
                )
                .is_none()
        );
        assert_eq!(state.removal.subject(), Some(&subject));
        assert_eq!(
            state
                .item(&subject.card)
                .unwrap()
                .worktree
                .as_ref()
                .unwrap()
                .path,
            PathBuf::from("/work/replacement")
        );
    }

    #[test]
    fn a_planning_reply_survives_reset_with_its_original_request_generation() {
        let mut state = State::default();
        let original = removable("/work/original");
        connected(&mut state, 1, board(vec![original.clone()]));
        let subject = RemovalSubject::of(&original).unwrap();
        let Some(Effect::Removal(RemovalEffect::Plan { generation, .. })) = state.update(
            RemovalAction::Open {
                generation: state.removal.generation(),
                subject: subject.clone(),
            }
            .into(),
        ) else {
            panic!("initial inspection")
        };
        state.reset();
        assert_eq!(state.removal.generation(), generation);
        connected(&mut state, 1, board(vec![original]));
        assert!(
            state
                .update(
                    RemovalAction::Planned {
                        generation,
                        result: Ok(prepared_removal(&subject)),
                    }
                    .into()
                )
                .is_none()
        );
        assert!(matches!(
            state.update(RemovalAction::Confirm {
                generation: state.removal.generation(),
            }.into()),
            Some(Effect::Removal(RemovalEffect::Remove { subject: removed, .. })) if removed == subject
        ));
    }

    #[test]
    fn deferred_inspection_requires_the_current_pane_watch_and_exact_subject() {
        for transition in ["refresh", "reset", "lost", "reconnect", "replaced", "error"] {
            let mut state = State::default();
            let first = removable("/work/a");
            let mut latest = removable("/work/b");
            latest.key = ItemKey::Worktree("/work/b".into());
            connected(&mut state, 1, board(vec![first.clone(), latest.clone()]));
            let first_subject = RemovalSubject::of(&first).unwrap();
            let desired = RemovalSubject::of(&latest).unwrap();
            let Some(Effect::Removal(RemovalEffect::Plan {
                generation: physical,
                ..
            })) = state.update(
                RemovalAction::Open {
                    generation: state.removal.generation(),
                    subject: first_subject.clone(),
                }
                .into(),
            )
            else {
                panic!("one actual first inspection")
            };
            assert!(
                state
                    .update(
                        RemovalAction::Open {
                            generation: state.removal.generation(),
                            subject: desired.clone(),
                        }
                        .into()
                    )
                    .is_none()
            );
            let waiting = state.removal.generation();
            match transition {
                "refresh" | "error" => watched(&mut state, 2, board(vec![first, latest.clone()])),
                "reset" => {
                    state.reset();
                    connected(&mut state, 1, board(vec![latest.clone()]));
                }
                "lost" => {
                    state.update(Action::Watched(Watch::Lost("disconnected".into())));
                }
                "reconnect" => connected(&mut state, 1, board(vec![latest.clone()])),
                "replaced" => {
                    let mut replacement = latest.clone();
                    replacement.worktree.as_mut().unwrap().path = "/work/recreated".into();
                    watched(&mut state, 2, board(vec![replacement]));
                }
                _ => unreachable!(),
            }
            let completion: Action = RemovalAction::Planned {
                generation: physical,
                result: if transition == "error" {
                    Err("client inspection deadline expired".into())
                } else {
                    Ok(prepared_removal(&first_subject))
                },
            }
            .into();
            assert!(completion.background());
            let effect = state.update(completion);
            if transition == "refresh" {
                assert!(matches!(effect, Some(Effect::Removal(RemovalEffect::Plan {
                    generation, subject, backend: None,
                })) if generation == waiting && subject == desired));
                assert!(
                    state
                        .update(
                            RemovalAction::Planned {
                                generation: physical,
                                result: Err("duplicate".into()),
                            }
                            .into()
                        )
                        .is_none(),
                    "a stale completion cannot release the new physical read"
                );
                assert!(
                    state
                        .update(
                            RemovalAction::Planned {
                                generation: waiting,
                                result: Ok(prepared_removal(&desired)),
                            }
                            .into()
                        )
                        .is_none()
                );
                assert!(matches!(state.update(RemovalAction::Confirm {
                    generation: state.removal.generation(),
                }.into()), Some(Effect::Removal(RemovalEffect::Remove { subject, .. })) if subject == desired));
            } else {
                assert!(effect.is_none(), "no hidden inspection after {transition}");
                assert!(
                    state
                        .update(
                            RemovalAction::Confirm {
                                generation: state.removal.generation(),
                            }
                            .into()
                        )
                        .is_none()
                );
                assert_eq!(state.removal.subject(), Some(&desired));
                connected(&mut state, 3, board(vec![latest]));
                assert!(
                    matches!(state.update(RemovalAction::Reinspect {
                    generation: state.removal.generation(),
                }.into()), Some(Effect::Removal(RemovalEffect::Plan {
                    subject, backend: None, ..
                })) if subject == desired),
                    "a fresh explicit read remains available"
                );
            }
        }
    }

    #[test]
    fn pending_and_uncertain_removal_survive_reset_and_completions_are_background() {
        let mut state = State::default();
        let original = removable("/work/original");
        connected(&mut state, 1, board(vec![original.clone()]));
        let subject = removal_review(&mut state, &original);
        let generation = state.removal.generation();
        assert!(matches!(
            state.update(RemovalAction::Confirm { generation }.into()),
            Some(Effect::Removal(RemovalEffect::Remove { .. }))
        ));
        state.reset();
        assert_eq!(state.removal.subject(), Some(&subject));
        assert_eq!(state.removal.generation(), generation);
        assert!(
            state
                .update(RemovalAction::Confirm { generation }.into())
                .is_none()
        );
        let completion: Action = RemovalAction::Finished {
            generation,
            result: RemovalOutcome::Uncertain("EOF after submission".into()),
        }
        .into();
        assert!(
            completion.background(),
            "another modal must not drop this completion"
        );
        state.update(completion);
        state.reset();
        connected(&mut state, 1, board(vec![removable("/work/replacement")]));
        let generation = state.removal.generation();
        assert!(
            state
                .update(RemovalAction::Dismiss { generation }.into())
                .is_none()
        );
        assert_eq!(state.removal.subject(), Some(&subject));
        let Some(Effect::Removal(RemovalEffect::Plan {
            subject: inspected,
            backend: Some(backend),
            generation,
        })) = state.update(RemovalAction::Reinspect { generation }.into())
        else {
            panic!("explicit pinned reinspection")
        };
        assert_eq!(inspected, subject);
        assert_eq!(backend, prepared_removal(&subject).backend);
        state.reset();
        assert_eq!(state.removal.generation(), generation);
        assert_eq!(state.removal.subject(), Some(&subject));
        let completion: Action = RemovalAction::Planned {
            generation,
            result: Err("not reachable".into()),
        }
        .into();
        assert!(completion.background());
        state.update(completion);
        assert!(
            state
                .update(
                    RemovalAction::Confirm {
                        generation: state.removal.generation()
                    }
                    .into()
                )
                .is_none()
        );
    }

    #[test]
    fn primary_worktree_never_gets_a_removal_subject() {
        let mut item = removable("/repo");
        item.worktree.as_mut().unwrap().primary = true;
        assert!(RemovalSubject::of(&item).is_none());
    }

    #[test]
    fn removal_menu_native_and_queued_callbacks_keep_exact_subject_pane_and_watch() {
        use iced::mouse::{Button, Event};
        for change in ["target", "branch", "watch", "reset", "metadata"] {
            let mut original = removable("/work/original");
            if change == "branch" {
                // A second checkout may keep its path key while switching
                // branch. Its primary Open destination is unchanged.
                original.key = ItemKey::Worktree("/work/original".into());
            }
            let mut current = original.clone();
            let mut state = State::default();
            state.update(Action::Layout(Layout::List));
            let primary = Activation::primary(&original, state.lifetime);
            connected(&mut state, 1, board(vec![original]));
            let generation = state.removal.generation();
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let cache = open_native_menu(&mut state, &mut renderer);
            let mut ui = cached_work_ui(&state, cache, &mut renderer);
            let point = popup_button_bounds(&mut ui, &renderer, "Remove worktree…").center();
            let queued = click_work(&mut ui, &mut renderer, point);
            assert!(
                matches!(queued.as_slice(), [Action::ActivateRemoval(_)]),
                "{change}: {queued:?}"
            );
            let press = retained_work_events(
                &mut ui,
                &mut renderer,
                point,
                &[Event::ButtonPressed(Button::Left)],
            );
            assert!(press.is_empty(), "{change}: {press:?}");
            let cache = ui.into_cache();
            match change {
                "target" => {
                    current.worktree.as_mut().unwrap().path = "/work/replacement".into();
                    watched(&mut state, 2, board(vec![current.clone()]));
                }
                "branch" => {
                    current.branch = Some("replacement-branch".into());
                    assert_eq!(Activation::primary(&current, state.lifetime), primary);
                    watched(&mut state, 2, board(vec![current.clone()]));
                }
                "watch" => {
                    state.update(Action::Watched(Watch::Lost("disconnected".into())));
                    connected(&mut state, 1, board(vec![current.clone()]));
                }
                "reset" => {
                    state.reset();
                    connected(&mut state, 1, board(vec![current.clone()]));
                    // Open the replacement pane's actual menu, then rebuild
                    // with the old held cache to expose a same-slot ABA.
                    let _ = open_native_menu(&mut state, &mut renderer);
                }
                _ => {
                    current.comparison.as_mut().unwrap().ahead += 1;
                    watched(&mut state, 2, board(vec![current.clone()]));
                }
            }
            assert_eq!(state.removal.generation(), generation);
            let mut ui = cached_work_ui(&state, cache, &mut renderer);
            assert!(popup_button_bounds(&mut ui, &renderer, "Remove worktree…").contains(point));
            let release = retained_work_events(
                &mut ui,
                &mut renderer,
                point,
                &[Event::ButtonReleased(Button::Left)],
            );
            if change == "metadata" {
                assert!(
                    matches!(release.as_slice(), [Action::ActivateRemoval(_)]),
                    "stable subject: {release:?}"
                );
            } else {
                assert!(
                    release.is_empty(),
                    "{change} adopted an old press: {release:?}"
                );
            }
            let fresh = click_work(&mut ui, &mut renderer, point);
            drop(ui);
            let menu = state.menu.clone();
            if change != "metadata" {
                assert!(state.update(queued.into_iter().next().unwrap()).is_none());
                assert_eq!(state.menu, menu, "refusal must not close the current menu");
                assert_eq!(state.removal.generation(), generation);
                assert!(state.removal.subject().is_none());
            }
            assert!(
                matches!(fresh.as_slice(), [Action::ActivateRemoval(_)]),
                "{change}: {fresh:?}"
            );
            let Some(Effect::Removal(RemovalEffect::Plan {
                subject,
                backend: None,
                ..
            })) = state.update(fresh.into_iter().next().unwrap())
            else {
                panic!("fresh {change} must request only an inspection")
            };
            assert_eq!(subject, RemovalSubject::of(&current).unwrap());
        }
    }

    fn full_removal_ui<'a>(
        state: &'a State,
        cache: iced::runtime::user_interface::Cache,
        renderer: &mut cosmic::Renderer,
    ) -> iced::runtime::UserInterface<'a, Action, Theme, cosmic::Renderer> {
        full_removal_ui_at_width(state, 1600.0, cache, renderer)
    }

    fn full_removal_ui_at_width<'a>(
        state: &'a State,
        width: f32,
        cache: iced::runtime::user_interface::Cache,
        renderer: &mut cosmic::Renderer,
    ) -> iced::runtime::UserInterface<'a, Action, Theme, cosmic::Renderer> {
        let mut ui = iced::runtime::UserInterface::build(
            state.layout_view(width),
            iced::Size::new(width, 900.0),
            cache,
            renderer,
        );
        draw_work_ui(&mut ui, renderer);
        ui
    }

    fn removal_review_geometry(
        ui: &mut iced::runtime::UserInterface<'_, Action, Theme, cosmic::Renderer>,
        renderer: &cosmic::Renderer,
    ) -> (
        iced::Rectangle,
        iced::Rectangle,
        iced::Vector,
        iced::Rectangle,
    ) {
        use iced::advanced::widget::{Operation, operation};
        #[derive(Default)]
        struct Geometry {
            scrolls: Vec<(iced::Rectangle, iced::Rectangle, iced::Vector)>,
            checkbox: Option<iced::Rectangle>,
        }
        impl Operation for Geometry {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                _: Option<&widget::Id>,
                bounds: iced::Rectangle,
                content: iced::Rectangle,
                translation: iced::Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                if bounds.height <= 360.0 && content.height > bounds.height + 1.0 {
                    self.scrolls.push((bounds, content, translation));
                }
            }
            fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
                if text == "Also delete the landed branch" {
                    self.checkbox = Some(bounds);
                }
            }
        }
        let mut geometry = Geometry::default();
        ui.operate(renderer, &mut geometry);
        assert_eq!(
            geometry.scrolls.len(),
            1,
            "one actually overflowing inline review"
        );
        let (viewport, content, translation) = geometry.scrolls[0];
        (
            viewport,
            content,
            translation,
            geometry.checkbox.expect("actual checkbox layout"),
        )
    }

    fn projected_review_bounds(
        mut bounds: iced::Rectangle,
        translation: iced::Vector,
    ) -> iced::Rectangle {
        bounds.x -= translation.x;
        bounds.y -= translation.y;
        bounds
    }

    // Exercise the actual full Work tree/cache and its scroll translation,
    // rather than manufacturing new retained-list callbacks.
    fn unresolved_control_point(
        ui: &mut iced::runtime::UserInterface<'_, Action, Theme, cosmic::Renderer>,
        renderer: &mut cosmic::Renderer,
        label: &str,
    ) -> iced::Point {
        use iced::advanced::widget::{Operation, operation};
        #[derive(Default)]
        struct Scroll(Option<(iced::Rectangle, iced::Vector)>);
        impl Operation for Scroll {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                _: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: iced::Rectangle,
                translation: iced::Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                if bounds.height <= 360.0 {
                    self.0 = Some((bounds, translation));
                }
            }
        }
        let mut scroll = Scroll::default();
        ui.operate(renderer, &mut scroll);
        let (viewport, _) = scroll.0.expect("inline removal scrollable");
        assert!(
            retained_work_events(
                ui,
                renderer,
                viewport.center(),
                &[iced::mouse::Event::WheelScrolled {
                    delta: iced::mouse::ScrollDelta::Pixels {
                        x: 0.0,
                        y: -100_000.0
                    },
                }]
            )
            .is_empty()
        );
        let mut scroll = Scroll::default();
        ui.operate(renderer, &mut scroll);
        let (viewport, translation) = scroll.0.unwrap();
        let bounds = projected_review_bounds(popup_button_bounds(ui, renderer, label), translation);
        assert!(
            viewport.contains(bounds.center()),
            "{label} visible after ordinary scroll: {bounds:?} in {viewport:?}"
        );
        bounds.center()
    }

    #[test]
    fn unresolved_native_keep_return_and_back_retire_reset_input_without_losing_original() {
        use iced::mouse::{Button, Event};
        use iced::runtime::user_interface::Cache;
        for width in [1600.0, 420.0] {
            let original = removable("/work/original");
            let mut other = removable("/work/other");
            other.key = crate::work::ItemKey::Worktree("/work/other".into());
            other.branch = Some("unrelated".into());
            let mut state = State::default();
            connected(&mut state, 1, board(vec![original.clone(), other.clone()]));
            let subject = removal_review(&mut state, &original);
            let generation = state.removal.generation();
            assert!(matches!(
                state.update(RemovalAction::Confirm { generation }.into()),
                Some(Effect::Removal(RemovalEffect::Remove { .. }))
            ));
            state.update(
                RemovalAction::Finished {
                    generation,
                    result: RemovalOutcome::Uncertain("EOF after original submission".into()),
                }
                .into(),
            );
            // A lost success reply can arrive after the board has already
            // stopped listing the removed worktree. Keeping must work without
            // treating that absence as a successful receipt.
            watched(&mut state, 2, board(vec![other.clone()]));
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let mut cache = Cache::new();
            let mut old_return = None;
            for label in ["Keep unresolved; review other work", "Review original"] {
                let mut ui = iced::runtime::UserInterface::build(
                    state.layout_view(width),
                    iced::Size::new(width, 1000.0),
                    cache,
                    &mut renderer,
                );
                draw_work_ui(&mut ui, &mut renderer);
                let point = unresolved_control_point(&mut ui, &mut renderer, label);
                let queued = click_work(&mut ui, &mut renderer, point);
                assert!(
                    matches!(queued.as_slice(), [Action::Removal(action)] if match (label, action.as_ref()) {
                        ("Review original", RemovalAction::ReturnUnresolved { .. }) => true,
                        ("Keep unresolved; review other work", RemovalAction::KeepUnresolved { .. }) => true,
                        _ => false,
                    }),
                    "actual callback: {queued:?}"
                );
                assert!(
                    retained_work_events(
                        &mut ui,
                        &mut renderer,
                        point,
                        &[Event::ButtonPressed(Button::Left)]
                    )
                    .is_empty()
                );
                cache = ui.into_cache();
                state.reset();
                connected(&mut state, 1, board(vec![other.clone()]));
                for action in queued.clone() {
                    assert!(state.update(action).is_none());
                }
                if label == "Review original" {
                    assert!(
                        state.removal.subject().is_none(),
                        "old queued return retired by pane owner"
                    );
                    old_return = Some(queued[0].clone());
                } else {
                    assert_eq!(
                        state.removal.subject(),
                        Some(&subject),
                        "old queued Keep cannot hide current unknown"
                    );
                }
                let mut ui = iced::runtime::UserInterface::build(
                    state.layout_view(width),
                    iced::Size::new(width, 1000.0),
                    cache,
                    &mut renderer,
                );
                draw_work_ui(&mut ui, &mut renderer);
                let new_point = unresolved_control_point(&mut ui, &mut renderer, label);
                assert!(
                    retained_work_events(
                        &mut ui,
                        &mut renderer,
                        new_point,
                        &[Event::ButtonReleased(Button::Left)]
                    )
                    .is_empty(),
                    "old held input cannot borrow fresh callback"
                );
                let fresh = click_work(&mut ui, &mut renderer, new_point);
                assert!(matches!(fresh.as_slice(), [Action::Removal(_)]));
                cache = ui.into_cache();
                for action in fresh {
                    assert!(state.update(action).is_none());
                }
            }
            assert_eq!(state.removal.subject(), Some(&subject));
            let mut ui = iced::runtime::UserInterface::build(
                state.layout_view(width),
                iced::Size::new(width, 1000.0),
                cache,
                &mut renderer,
            );
            draw_work_ui(&mut ui, &mut renderer);
            let point = unresolved_control_point(&mut ui, &mut renderer, "Inspect again");
            let inspected = click_work(&mut ui, &mut renderer, point);
            assert!(
                matches!(inspected.as_slice(), [Action::Removal(action)] if matches!(action.as_ref(), RemovalAction::Reinspect { .. }))
            );
            cache = ui.into_cache();
            let Some(Effect::Removal(RemovalEffect::Plan {
                generation,
                subject: inspected_subject,
                backend: Some(backend),
            })) = state.update(inspected.into_iter().next().unwrap())
            else {
                panic!("original-host readonly inspection")
            };
            assert_eq!(inspected_subject, subject);
            assert_eq!(backend, prepared_removal(&subject).backend);
            state.update(
                RemovalAction::Planned {
                    generation,
                    result: Err("original worktree missing after unknown result".into()),
                }
                .into(),
            );
            let mut ui = iced::runtime::UserInterface::build(
                state.layout_view(width),
                iced::Size::new(width, 1000.0),
                cache,
                &mut renderer,
            );
            draw_work_ui(&mut ui, &mut renderer);
            let point =
                unresolved_control_point(&mut ui, &mut renderer, "Back to unresolved removals");
            let back = click_work(&mut ui, &mut renderer, point);
            assert!(
                matches!(back.as_slice(), [Action::Removal(action)] if matches!(action.as_ref(), RemovalAction::CloseUnresolved { .. }))
            );
            drop(ui);
            for action in back {
                assert!(state.update(action).is_none());
            }
            assert!(state.removal.subject().is_none());
            assert!(
                !state.removal.can_open_subject(&subject),
                "kept original stays fenced after Back/reset/read failure"
            );
            let other_subject = removal_review(&mut state, &other);
            assert!(state.update(old_return.unwrap()).is_none());
            assert_eq!(state.removal.subject(), Some(&other_subject));
            assert!(
                matches!(state.update(RemovalAction::Confirm { generation: state.removal.generation() }.into()), Some(Effect::Removal(RemovalEffect::Remove { subject, .. })) if subject == other_subject)
            );
        }
    }

    #[test]
    fn removal_disclosure_retires_cached_confirmation_presses() {
        use iced::mouse::{Button, Event};
        use iced::runtime::user_interface::Cache;

        for width in [760.0, 1600.0] {
            // The unchanged case proves this cache and hit target can retain
            // a held press; opening and closing must retire that same input.
            for (initial_open, next_open) in [(false, false), (false, true), (true, false)] {
                let path = format!("/work/{}target", "common-segment/".repeat(18));
                let original = removable(&path);
                let mut state = State::default();
                connected(&mut state, 1, board(vec![original.clone()]));
                let subject = removal_review(&mut state, &original);
                let generation = state.removal.generation();
                if initial_open {
                    assert!(
                        state
                            .update(
                                RemovalAction::Details {
                                    generation,
                                    open: true
                                }
                                .into()
                            )
                            .is_none()
                    );
                }
                let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
                let mut ui = full_removal_ui_at_width(&state, width, Cache::new(), &mut renderer);
                let point = unresolved_control_point(&mut ui, &mut renderer, "Remove worktree");
                assert!(
                    retained_work_events(
                        &mut ui,
                        &mut renderer,
                        point,
                        &[Event::ButtonPressed(Button::Left)],
                    )
                    .is_empty()
                );
                let cache = ui.into_cache();
                assert!(
                    state
                        .update(
                            RemovalAction::Details {
                                generation,
                                open: next_open
                            }
                            .into()
                        )
                        .is_none(),
                    "disclosure cannot dispatch work"
                );
                assert_eq!(state.removal.generation(), generation);
                assert_eq!(state.removal.subject(), Some(&subject));
                let mut ui = full_removal_ui_at_width(&state, width, cache, &mut renderer);
                // Release over the replacement's actual visible location. A
                // release at the old location could pass only because it misses.
                let point = unresolved_control_point(&mut ui, &mut renderer, "Remove worktree");
                let release = retained_work_events(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonReleased(Button::Left)],
                );
                let accepted = if initial_open != next_open {
                    assert!(
                        release.is_empty(),
                        "held confirmation crossed {initial_open} -> {next_open} at {width}: {release:?}"
                    );
                    click_work(&mut ui, &mut renderer, point)
                } else {
                    release
                };
                drop(ui);
                assert!(
                    matches!(accepted.as_slice(), [Action::Removal(action)] if matches!(action.as_ref(), RemovalAction::Confirm { generation: g } if *g == generation)),
                    "fresh confirmation after {initial_open} -> {next_open} at {width}: {accepted:?}"
                );
                let Some(Effect::Removal(RemovalEffect::Remove {
                    subject: removed,
                    backend,
                    request,
                    ..
                })) = state.update(accepted.into_iter().next().unwrap())
                else {
                    panic!("capture the removal effect without executing it")
                };
                assert_eq!(removed, subject);
                assert_eq!(backend, prepared_removal(&subject).backend);
                assert_eq!(
                    request,
                    crate::work::Removal {
                        scope: crate::work::Scope::Project(subject.card.project.clone()),
                        item: path,
                        token: "reviewed".into(),
                        keep: crate::work::Keep::Nothing,
                        delete_branch: false,
                    },
                    "disclosure preserves the exact reviewed consent"
                );
            }
        }
    }

    #[test]
    fn removal_disclosure_keeps_keyboard_focus_across_cached_expansion_and_collapse() {
        use iced::advanced::widget::{Operation, operation};
        use iced::keyboard::{self, key};
        use iced::runtime::user_interface::Cache;

        fn disclosure_focus(
            ui: &mut iced::runtime::UserInterface<'_, Action, Theme, cosmic::Renderer>,
            renderer: &cosmic::Renderer,
            label: &str,
            set_focus: bool,
        ) -> bool {
            struct Focus {
                bounds: iced::Rectangle,
                set_focus: bool,
                matched: usize,
                focused: bool,
            }
            impl Operation for Focus {
                fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                    visit(self);
                }
                fn focusable(
                    &mut self,
                    _: Option<&widget::Id>,
                    bounds: iced::Rectangle,
                    state: &mut dyn operation::Focusable,
                ) {
                    if bounds == self.bounds {
                        self.matched += 1;
                        if self.set_focus {
                            state.focus();
                        }
                        self.focused = state.is_focused();
                    } else if self.set_focus {
                        state.unfocus();
                    }
                }
            }
            let mut focus = Focus {
                bounds: popup_button_bounds(ui, renderer, label),
                set_focus,
                matched: 0,
                focused: false,
            };
            ui.operate(renderer, &mut focus);
            assert_eq!(focus.matched, 1, "one actual disclosure button");
            focus.focused
        }

        for width in [760.0, 1600.0] {
            let original = removable(&format!("/work/{}target", "common-segment/".repeat(18)));
            let mut state = State::default();
            connected(&mut state, 1, board(vec![original.clone()]));
            let subject = removal_review(&mut state, &original);
            let generation = state.removal.generation();
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let mut ui = full_removal_ui_at_width(&state, width, Cache::new(), &mut renderer);
            assert!(disclosure_focus(
                &mut ui,
                &renderer,
                "Show full details",
                true
            ));
            let mut cache = ui.into_cache();
            for open in [true, false] {
                let mut ui = full_removal_ui_at_width(&state, width, cache, &mut renderer);
                let label = if open {
                    "Show full details"
                } else {
                    "Hide full details"
                };
                assert!(
                    disclosure_focus(&mut ui, &renderer, label, false),
                    "disclosure must retain keyboard focus before Enter at {width}"
                );
                let mut messages = Vec::new();
                let _ = ui.update(
                    &[
                        iced::Event::Keyboard(keyboard::Event::KeyPressed {
                            key: keyboard::Key::Named(key::Named::Enter),
                            modified_key: keyboard::Key::Named(key::Named::Enter),
                            physical_key: key::Physical::Code(key::Code::Enter),
                            location: keyboard::Location::Standard,
                            modifiers: keyboard::Modifiers::default(),
                            text: None,
                            repeat: false,
                        }),
                        iced::Event::Keyboard(keyboard::Event::KeyReleased {
                            key: keyboard::Key::Named(key::Named::Enter),
                            modified_key: keyboard::Key::Named(key::Named::Enter),
                            physical_key: key::Physical::Code(key::Code::Enter),
                            location: keyboard::Location::Standard,
                            modifiers: keyboard::Modifiers::default(),
                        }),
                    ],
                    iced::mouse::Cursor::Unavailable,
                    &mut renderer,
                    &mut iced::advanced::clipboard::Null,
                    &mut messages,
                );
                cache = ui.into_cache();
                assert!(
                    matches!(messages.as_slice(), [Action::Removal(action)] if matches!(action.as_ref(), RemovalAction::Details { generation: g, open: value } if *g == generation && *value == open)),
                    "Enter must only toggle disclosure at {width}: {messages:?}"
                );
                assert!(state.update(messages.into_iter().next().unwrap()).is_none());
                assert_eq!(state.removal.generation(), generation);
                assert_eq!(state.removal.subject(), Some(&subject));
            }
            let mut ui = full_removal_ui_at_width(&state, width, cache, &mut renderer);
            assert!(
                disclosure_focus(&mut ui, &renderer, "Show full details", false),
                "collapse must also retain disclosure keyboard focus at {width}"
            );
        }
    }

    #[test]
    fn removal_confirm_native_owner_tracks_consent_not_watch_reconnection() {
        use iced::mouse::{Button, Event};
        use iced::runtime::user_interface::Cache;
        for change in ["choice", "watch", "reset"] {
            let change_choice = change == "choice";
            let original = removable("/work/original");
            let mut state = State::default();
            connected(&mut state, 1, board(vec![original.clone()]));
            let subject = RemovalSubject::of(&original).unwrap();
            let Some(Effect::Removal(RemovalEffect::Plan { generation, .. })) = state.update(
                RemovalAction::Open {
                    generation: state.removal.generation(),
                    subject: subject.clone(),
                }
                .into(),
            ) else {
                panic!("initial inspection only")
            };
            let mut prepared = prepared_removal(&subject);
            prepared.plan.inspection.ignored = (0..6)
                .map(|i| crate::worktree_content::IgnoredEntry {
                    path: format!("ignored-directory-{i}"),
                    directory: true,
                })
                .collect();
            prepared.plan.inspection.ignored_total = 6;
            state.update(
                RemovalAction::Planned {
                    generation,
                    result: Ok(prepared),
                }
                .into(),
            );
            let generation = state.removal.generation();
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let mut ui = full_removal_ui(&state, Cache::new(), &mut renderer);
            let (viewport, content, before_scroll, _) = removal_review_geometry(&mut ui, &renderer);
            assert_eq!(before_scroll, iced::Vector::ZERO);
            let scroll = retained_work_events(
                &mut ui,
                &mut renderer,
                viewport.center(),
                &[Event::WheelScrolled {
                    delta: iced::mouse::ScrollDelta::Pixels {
                        x: 0.0,
                        y: -content.height,
                    },
                }],
            );
            assert!(
                scroll.is_empty(),
                "wheel must not choose a removal action: {scroll:?}"
            );
            let (viewport, _, translation, checkbox) = removal_review_geometry(&mut ui, &renderer);
            assert!(translation.y > 0.0, "the actual review must scroll");
            let bounds = projected_review_bounds(
                popup_button_bounds(&mut ui, &renderer, "Remove worktree"),
                translation,
            );
            assert!(
                viewport.contains(bounds.position())
                    && viewport.contains(iced::Point::new(
                        bounds.x + bounds.width,
                        bounds.y + bounds.height
                    )),
                "confirm must be fully visible: {bounds:?}, {viewport:?}"
            );
            let point = bounds.center();
            let mut queued = click_work(&mut ui, &mut renderer, point);
            assert!(
                matches!(queued.as_slice(), [Action::Removal(action)] if matches!(action.as_ref(), RemovalAction::Confirm { generation: g } if *g == generation)),
                "{queued:?}"
            );
            let choice = if change_choice {
                let checkbox = projected_review_bounds(checkbox, translation);
                assert!(viewport.contains(checkbox.center()));
                let messages = click_work(&mut ui, &mut renderer, checkbox.center());
                assert!(
                    matches!(messages.as_slice(), [Action::Removal(action)] if matches!(action.as_ref(), RemovalAction::DeleteBranch { generation: g, delete: true } if *g == generation)),
                    "{messages:?}"
                );
                Some(messages.into_iter().next().unwrap())
            } else {
                None
            };
            let press = retained_work_events(
                &mut ui,
                &mut renderer,
                point,
                &[Event::ButtonPressed(Button::Left)],
            );
            assert!(press.is_empty(), "{press:?}");
            let cache = ui.into_cache();
            if let Some(choice) = choice {
                assert!(state.update(choice).is_none());
                assert_ne!(state.removal.generation(), generation);
                assert!(state.update(queued.remove(0)).is_none());
            }
            // A first Watch owns new Pin callbacks, but the accepted review
            // still owns its original inspection, host and receipt generation.
            if change == "reset" {
                state.reset();
            }
            connected(&mut state, 1, board(vec![original]));
            assert_eq!(state.removal.subject(), Some(&subject));
            let accepted_generation = state.removal.generation();
            if change == "reset" {
                assert_ne!(accepted_generation, generation);
                assert!(
                    state.update(queued.remove(0)).is_none(),
                    "a real queued Confirm from the closed pane must not remove the identical reconnected subject"
                );
            } else if !change_choice {
                assert_eq!(accepted_generation, generation);
            }
            let mut ui = full_removal_ui(&state, cache, &mut renderer);
            let (_, _, retained_translation, _) = removal_review_geometry(&mut ui, &renderer);
            assert_eq!(
                retained_translation, translation,
                "consent changes must preserve the actual scroll offset"
            );
            assert!(
                projected_review_bounds(
                    popup_button_bounds(&mut ui, &renderer, "Remove worktree"),
                    retained_translation
                )
                .contains(point)
            );
            let release = retained_work_events(
                &mut ui,
                &mut renderer,
                point,
                &[Event::ButtonReleased(Button::Left)],
            );
            let accepted = if change != "watch" {
                assert!(
                    release.is_empty(),
                    "old consent crossed {change}: {release:?}"
                );
                click_work(&mut ui, &mut renderer, point)
            } else {
                release
            };
            drop(ui);
            assert!(
                matches!(accepted.as_slice(), [Action::Removal(action)] if matches!(action.as_ref(), RemovalAction::Confirm { generation: g } if *g == accepted_generation)),
                "{accepted:?}"
            );
            let Some(Effect::Removal(RemovalEffect::Remove {
                subject: removed,
                backend,
                request,
                ..
            })) = state.update(accepted.into_iter().next().unwrap())
            else {
                panic!("only inspect the captured removal effect, never execute it")
            };
            assert_eq!(removed, subject);
            assert_eq!(backend, prepared_removal(&subject).backend);
            assert_eq!(request.item, subject.path.to_str().unwrap());
            assert_eq!(request.token, "reviewed");
            assert_eq!(request.delete_branch, change_choice);
            assert_eq!(request.keep, crate::work::Keep::Nothing);
        }
    }
}
