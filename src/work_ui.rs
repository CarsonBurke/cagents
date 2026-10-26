//! The Work pane: the daemon's board of every worktree and branch in
//! progress, as columns of cards or as one list newest first, for every
//! project or for one.
//!
//! The daemon owns the board and pushes each changed one; this state only
//! filters and presents it. A pin is shown at once and confirmed by the next
//! board, so a card moves the moment it is moved.
use std::{path::PathBuf, sync::Arc};

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

/// A semantic color, read from the drawing theme.
type Toned = fn(&Theme) -> iced::Color;

/// The narrowest a board column may be before the board scrolls sideways.
const COLUMN_MIN_WIDTH: f32 = 248.0;
/// A column's width when the board scrolls sideways.
const COLUMN_WIDTH: f32 = 280.0;
const SCOPE_MENU_WIDTH: f32 = 280.0;
const CARD_MENU_WIDTH: f32 = 264.0;

mod board_scroll;

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
        epoch: u64,
        revision: u64,
        board: Arc<Board>,
        first: bool,
    },
    /// The subscription failed and is being retried.
    Lost { epoch: u64, error: String },
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
    /// The card's main action: its worktree, else its pull request.
    Open(CardRef),
    Menu(Option<CardRef>),
    CloseMenu(CardRef),
    PlacementMenu(CardRef, bool),
    Details(CardRef),
    OpenWorktree(CardRef),
    OpenPullRequest(CardRef),
    OpenUrl(String),
    FocusSession(TerminalId),
    Copy(String),
    /// Pins a card to a column, or unpins it with `None`.
    Pin(CardRef, Option<Column>),
    Pinned(Result<(), String>),
}

impl Action {
    /// Whether the action arrives without the user doing anything, so it
    /// must not count as interaction with the window.
    pub fn background(&self) -> bool {
        matches!(
            self,
            Self::Watched(_) | Self::Tick | Self::Refreshed(_) | Self::Pinned(_)
        )
    }
}

/// What only the application can do for this pane.
#[derive(Debug, Clone)]
pub enum Effect {
    Refresh,
    Pin {
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
    /// The board shown: the daemon's, with the pins not yet confirmed.
    board: Option<Arc<Board>>,
    /// The daemon's last board, which a refused pin reverts to.
    confirmed: Option<Arc<Board>>,
    /// Each card's searchable text, lowercase, in board order.
    search: Vec<String>,
    revision: u64,
    /// Never reused, including across closing/reopening this pane.
    watch_epoch: u64,
    listening: bool,
    /// Retained rows are historical until this listener receives a board.
    awaiting_current: bool,
    /// Why the subscription is down, while it is retried.
    lost: Option<String>,
    scope: Option<PathBuf>,
    scope_menu: bool,
    query: String,
    layout: Layout,
    menu: Option<CardRef>,
    placement_menu: bool,
    details: Option<CardRef>,
    refreshing: bool,
    /// The last refresh or pin that failed.
    error: Option<String>,
    now_unix_ms: u64,
}

impl State {
    /// Forgets the board when the pane closes, so a reopened pane never
    /// shows a board from before it was closed.
    pub fn reset(&mut self) {
        let scope = self.scope.take();
        let layout = self.layout;
        let watch_epoch = self.watch_epoch;
        *self = Self {
            scope,
            layout,
            watch_epoch,
            ..Self::default()
        };
    }

    /// Synchronize actual visible demand before the first subscription and
    /// after every application update. Suspending never cancels explicit work.
    pub fn set_listening(&mut self, enabled: bool) {
        if self.listening == enabled {
            return;
        }
        self.listening = false;
        self.awaiting_current = false;
        self.lost = None;
        if enabled && let Some(epoch) = self.watch_epoch.checked_add(1) {
            self.watch_epoch = epoch;
            self.listening = true;
            self.awaiting_current = true;
        }
    }

    pub fn listening_epoch(&self) -> Option<u64> {
        self.listening.then_some(self.watch_epoch)
    }

    pub fn update(&mut self, action: Action) -> Option<Effect> {
        match action {
            Action::Watched(Watch::Board {
                epoch,
                revision,
                board,
                first,
            }) => {
                if self.listening_epoch() != Some(epoch) {
                    return None;
                }
                // A board pushed while the subscription was answered can
                // arrive before the answer; the newer one stands.
                if first || revision > self.revision {
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
                    if self
                        .details
                        .as_ref()
                        .is_some_and(|card| !board.items.iter().any(|item| card.names(item)))
                    {
                        self.details = None;
                    }
                    self.confirmed = Some(board.clone());
                    self.board = Some(board);
                    self.lost = None;
                    self.awaiting_current = false;
                }
                None
            }
            Action::Watched(Watch::Lost { epoch, error }) => {
                if self.listening_epoch() != Some(epoch) {
                    return None;
                }
                self.lost = Some(error);
                self.awaiting_current = true;
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
            Action::Open(card) => {
                let item = self.item(&card)?;
                if item.worktree.is_some() {
                    self.update(Action::OpenWorktree(card))
                } else {
                    self.update(Action::OpenPullRequest(card))
                }
            }
            Action::Menu(menu) => {
                if let Some(card) = &menu {
                    self.item(card)?;
                }
                self.menu = menu;
                self.placement_menu = false;
                None
            }
            Action::CloseMenu(card) => {
                if self.menu.as_ref() == Some(&card) {
                    self.menu = None;
                    self.placement_menu = false;
                }
                None
            }
            Action::PlacementMenu(card, open) => {
                self.item(&card)?;
                if self.menu.as_ref() == Some(&card) {
                    self.placement_menu = open;
                }
                None
            }
            Action::Details(card) => {
                self.item(&card)?;
                self.menu = None;
                self.details = (self.details.as_ref() != Some(&card)).then_some(card);
                None
            }
            Action::OpenWorktree(card) => {
                self.menu = None;
                let item = self.item(&card)?;
                Some(Effect::OpenWorktree {
                    project: item.project.clone(),
                    path: item.worktree.as_ref()?.path.clone(),
                })
            }
            Action::OpenPullRequest(card) => {
                self.menu = None;
                let item = self.item(&card)?;
                let pull_request = item.pull_request.as_ref()?;
                Some(Effect::OpenPullRequest {
                    project: item.project.clone(),
                    number: pull_request.number,
                    url: pull_request.url.clone(),
                })
            }
            Action::OpenUrl(url) => {
                self.menu = None;
                Some(Effect::OpenUrl(url))
            }
            Action::FocusSession(id) => {
                self.menu = None;
                Some(Effect::FocusSession(id))
            }
            Action::Copy(text) => {
                self.menu = None;
                Some(Effect::Copy(text))
            }
            Action::Pin(card, column) => {
                self.menu = None;
                let board = Arc::make_mut(self.board.as_mut()?);
                let stale_after_days = board.stale_after_days;
                let item = board.items.iter_mut().find(|item| card.names(item))?;
                crate::work::place(item, column, self.now_unix_ms, stale_after_days);
                Some(Effect::Pin {
                    project: card.project,
                    item: card.key.storage_key(),
                    column,
                })
            }
            Action::Pinned(result) => {
                // A pin the daemon took arrives with its next board; one it
                // refused changed nothing there, so no board will undo it.
                if let Err(error) = result {
                    self.board = self.confirmed.clone();
                    self.error = Some(error);
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
                .icon(Icon::Folder.handle())
                .detail(
                    "Open a Git project and its worktrees, branches and pull requests appear here.",
                )
                .into(),
            Some(_) => match self.layout {
                Layout::Board => self.board_view(width),
                Layout::List => self.list_view(),
            },
        };
        widget::column::with_children(vec![header, design::horizontal_rule(), body])
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn waiting(&self) -> Element<'_, Action> {
        if !self.listening && self.watch_epoch == u64::MAX {
            return placeholder("Work updates cannot restart")
                .icon(Icon::Warning.handle())
                .detail("Reopen this window to watch the Work board again.")
                .into();
        }
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
            let count = |scope: Option<&PathBuf>| {
                self.board.as_ref().map_or(0, |board| {
                    board
                        .items
                        .iter()
                        .filter(|item| scope.is_none_or(|scope| item.project == *scope))
                        .count()
                })
            };
            let mut items = vec![
                popover_item(
                    Icon::Grid.handle(),
                    "All projects",
                    cards(count(None)),
                    Action::Scope(None),
                )
                .selected(self.scope.is_none())
                .into(),
            ];
            items.extend(projects.iter().map(|project| {
                popover_item(
                    Icon::Folder.handle(),
                    project.name.as_str(),
                    cards(count(Some(&project.repository))),
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
        if self.awaiting_current {
            parts.push(design::caption("Updating Work…").into());
        }
        if !self.listening && self.watch_epoch == u64::MAX {
            parts.push(warning(
                "Work updates cannot restart; reopen this window.".to_owned(),
            ));
        }
        if let Some(error) = &self.lost {
            parts.push(warning(format!("Disconnected: {error}")));
        }
        if let Some(error) = &self.error {
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
        if board.linear_unavailable.is_some() && self.visible().any(|item| item.linear.is_some()) {
            parts.push(warning("Linked issue status unavailable".to_owned()));
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
            board_scroll::view(
                widget::scrollable::horizontal(row)
                    .id(board_scroll::ID)
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
                .icon(Icon::Grid.handle())
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

    fn card<'a>(&'a self, item: &'a Item) -> Element<'a, Action> {
        self.item_view(item, false)
    }

    fn row<'a>(&'a self, item: &'a Item) -> Element<'a, Action> {
        self.item_view(item, true)
    }

    /// The human title leads in both layouts; technical facts are disclosed
    /// deliberately instead of competing with the title and lifecycle.
    fn item_view<'a>(&'a self, item: &'a Item, compact: bool) -> Element<'a, Action> {
        let card = CardRef::of(item);
        let title = item_title(item);
        let top = widget::row::with_children(vec![
            status_glyph(item),
            crate::interaction::tooltip(
                design::heading(title)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        iced::advanced::text::EllipsizeHeightLimit::Lines(2),
                    ))
                    .width(Length::Fill),
                title.to_owned(),
            ),
        ])
        .spacing(space::S)
        .align_y(Alignment::Center);
        let facts = widget::row::with_children(self.facts(item))
            .spacing(space::XS)
            .align_y(Alignment::Center)
            .wrap();
        let button = widget::button::custom(
            widget::column::with_children(vec![top.into(), facts.into()]).spacing(space::XS),
        )
        .name(format!("Open {title}"))
        .class(card_style(self.menu.as_ref() == Some(&card)))
        .padding(if compact {
            Padding::from([space::XS + space::XXS, space::S])
        } else {
            Padding::from(space::S)
        })
        .width(Length::Fill)
        .on_press(Action::Open(card.clone()));
        // The menu is a sibling, so its keyboard/pointer activation can never
        // bubble into the main Open action.
        let primary = widget::row::with_children(vec![button.into(), self.card_menu(item, &card)])
            .spacing(space::XXS)
            .align_y(Alignment::Center);
        let mut content: Vec<Element<'a, Action>> = vec![primary.into()];
        if self.details.as_ref() == Some(&card) {
            content.push(self.item_details(item));
        }
        crate::ui_controls::owned(
            card.clone(),
            widget::mouse_area(widget::column::with_children(content).spacing(space::XXS))
                .on_right_press(Action::Menu(Some(card))),
        )
    }

    /// One lifecycle summary, with only useful context and actionable facts.
    /// Board placement and PR state remain independent in the underlying data.
    fn facts<'a>(&'a self, item: &'a Item) -> Vec<Element<'a, Action>> {
        let (label, tone) = lifecycle(item);
        let mut facts = vec![badge(label, Tone::Themed(tone))];
        if self.scope.is_none() {
            facts.push(design::caption(item.project_name.as_str()).into());
        }
        if let Some(pr) = &item.pull_request {
            facts.push(design::caption(format!("#{}", pr.number)).into());
            // Agent activity, draft status and CI/review are independent. Keep
            // actionable failures visible even when another state leads.
            if pr.state == PullRequestState::Open {
                if pr.checks.and_then(|checks| checks.outcome()) == Some(CheckOutcome::Failing)
                    && label != "Checks failed"
                {
                    facts.push(badge(
                        "Checks failed",
                        Tone::Themed(design::destructive_color),
                    ));
                }
                if pr.review == Some(Review::ChangesRequested) && label != "Changes requested" {
                    facts.push(badge(
                        "Changes requested",
                        Tone::Themed(design::warning_color),
                    ));
                }
            }
        }
        if item.pinned {
            facts.push(crate::interaction::tooltip(
                widget::icon(Icon::Board.handle()).size(design::COMPACT_ICON_SIZE),
                format!("Manually placed in {}", item.column.label()),
            ));
        }
        if let Some(dirty) = &item.dirty {
            if dirty.conflicted > 0 {
                facts.push(badge(
                    format!("{} conflicted", dirty.conflicted),
                    Tone::Themed(design::destructive_color),
                ));
            } else if dirty.paths > 0 {
                facts.push(crate::interaction::tooltip(
                    badge(
                        format!("{} changed", dirty.paths),
                        Tone::Themed(design::warning_color),
                    ),
                    "Uncommitted changes in the worktree",
                ));
            }
        }
        facts
    }

    fn item_details<'a>(&'a self, item: &'a Item) -> Element<'a, Action> {
        let line = |label: &str, value: String| -> Element<'a, Action> {
            design::body(format!("{label}: {value}"))
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .width(Length::Fill)
                .into()
        };
        let mut rows = vec![
            design::body(item_title(item))
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .width(Length::Fill)
                .into(),
        ];
        if let Some(pr) = &item.pull_request {
            let state = match (pr.state, pr.draft) {
                (PullRequestState::Open, true) => "Draft",
                (PullRequestState::Open, false) => "Open",
                (PullRequestState::Merged, _) => "Merged",
                (PullRequestState::Closed, _) => "Closed",
            };
            rows.push(line("Pull request", format!("#{} · {state}", pr.number)));
            if pr.state == PullRequestState::Merged && item.derived != Column::Landed {
                let explanation = if item.has_live_agent() {
                    "The pull request is merged; a session is still active in this worktree."
                } else if item.dirty.as_ref().is_some_and(|dirty| dirty.paths > 0) {
                    "The pull request is merged; uncommitted local changes remain."
                } else if !pr.contains_local_head {
                    "The pull request is merged; its merged history has not been confirmed to include this worktree's current head."
                } else {
                    "Pull request state and local work status are tracked independently."
                };
                rows.push(
                    design::body(explanation)
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                        .width(Length::Fill)
                        .into(),
                );
            }
            if let Some(checks) = pr.checks {
                rows.push(line(
                    "Checks",
                    format!(
                        "{} passed · {} running · {} failed",
                        checks.passed, checks.pending, checks.failed
                    ),
                ));
            }
            if let Some(review) = pr.review {
                rows.push(line(
                    "Review",
                    match review {
                        Review::Approved => "Approved",
                        Review::ChangesRequested => "Changes requested",
                        Review::Required => "Review required",
                    }
                    .to_owned(),
                ));
            }
        }
        rows.push(line("Local work", item.derived.label().to_owned()));
        if item.pinned {
            rows.push(line(
                "Pinned board position",
                item.column.label().to_owned(),
            ));
        }
        if let Some(branch) = &item.branch {
            rows.push(line("Branch", branch.clone()));
        }
        if let Some(worktree) = &item.worktree {
            rows.push(line(
                "Worktree",
                crate::app::display_path(&worktree.path).into_owned(),
            ));
        }
        if let Some(comparison) = &item.comparison {
            rows.push(line(
                "Compared with base",
                format!(
                    "{} ahead and {} behind {}",
                    commits(comparison.ahead),
                    commits(comparison.behind),
                    comparison.base,
                ),
            ));
        }
        if let Some(ticket) = &item.linear {
            let title = ticket.title.as_deref().unwrap_or_default();
            rows.push(line(
                "Issue",
                match &ticket.identifier {
                    Some(identifier) => format!("{identifier} {title}").trim().to_owned(),
                    None => title.to_owned(),
                },
            ));
            if let Some(reason) = self
                .board
                .as_ref()
                .and_then(|board| board.linear_unavailable.as_ref())
            {
                rows.push(line("Issue connection", reason.clone()));
            }
        }
        if let Some(error) = &item.error {
            rows.push(line("Could not refresh", error.clone()));
        }
        rows.push(self.footer(item));
        widget::container(widget::column::with_children(rows).spacing(space::XS))
            .padding(space::S)
            .width(Length::Fill)
            .into()
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
                crate::interaction::tooltip(
                    widget::button::custom(glyph)
                        .name(format!("Focus {label}"))
                        .class(crate::ui_controls::control_style(false, None))
                        .padding(space::XXS)
                        .on_press(Action::FocusSession(session.id)),
                    format!("{label}, {state}"),
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
        let anchor = icon_button(Icon::More, format!("Actions for {}", item_title(item)))
            .compact()
            .id(card_actions_id(card))
            .selected(open)
            .on_press(if open {
                Action::CloseMenu(card.clone())
            } else {
                Action::Menu(Some(card.clone()))
            });
        let content = open.then(|| {
            if self.placement_menu {
                return self.placement_options(item, card);
            }
            let mut items: Vec<Element<'a, Action>> = vec![
                popover_item(
                    Icon::Info.handle(),
                    if self.details.as_ref() == Some(card) {
                        "Hide details"
                    } else {
                        "Show details"
                    },
                    "Branch, local status and activity",
                    Action::Details(card.clone()),
                )
                .into(),
            ];
            if let Some(worktree) = &item.worktree {
                items.push(
                    popover_item(
                        Icon::OpenFolder.handle(),
                        "Open worktree",
                        crate::app::display_path(&worktree.path),
                        Action::OpenWorktree(card.clone()),
                    )
                    .into(),
                );
            }
            if let Some(pull_request) = &item.pull_request {
                items.push(
                    popover_item(
                        Icon::PullRequest.handle(),
                        "Open pull request",
                        format!("#{}", pull_request.number),
                        Action::OpenPullRequest(card.clone()),
                    )
                    .into(),
                );
            }
            if let Some(ticket) = &item.linear
                && let Some(url) = &ticket.url
            {
                items.push(
                    popover_item(
                        Icon::WebBrowser.handle(),
                        "Open Linear issue",
                        ticket.identifier.clone().unwrap_or_default(),
                        Action::OpenUrl(url.clone()),
                    )
                    .into(),
                );
            }
            if let Some(branch) = &item.branch {
                items.push(
                    popover_item(
                        Icon::Copy.handle(),
                        "Copy branch name",
                        branch.as_str(),
                        Action::Copy(branch.clone()),
                    )
                    .into(),
                );
            }
            items.push(design::horizontal_rule());
            items.push(
                popover_item(
                    Icon::Board.handle(),
                    "Board placement…",
                    if item.pinned {
                        format!("Manually placed in {}", item.column.label())
                    } else {
                        "Follows activity and pull request status".to_owned()
                    },
                    Action::PlacementMenu(card.clone(), true),
                )
                .into(),
            );
            widget::scrollable(widget::column::with_children(items).width(CARD_MENU_WIDTH))
                .height(Length::Shrink)
                .into()
        });
        crate::interaction::composer_popover(anchor, content, Action::CloseMenu(card.clone()))
    }

    fn placement_options<'a>(&'a self, item: &'a Item, card: &CardRef) -> Element<'a, Action> {
        let mut choices: Vec<Element<'a, Action>> = vec![
            popover_item(
                Icon::ChevronLeft.handle(),
                "Back to actions",
                "",
                Action::PlacementMenu(card.clone(), false),
            )
            .into(),
        ];
        choices.push(
            design::caption("Manual placement does not change pull request or session status.")
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .width(Length::Fill)
                .into(),
        );
        choices.push(
            popover_item(
                Icon::Undo.handle(),
                "Follow activity",
                format!("Automatic: {}", item.derived.label()),
                Action::Pin(card.clone(), None),
            )
            .selected(!item.pinned)
            .on_press_maybe(item.pinned.then(|| Action::Pin(card.clone(), None)))
            .into(),
        );
        for column in Column::ALL {
            let current = item.pinned && item.column == column;
            choices.push(
                popover_item(
                    Icon::Board.handle(),
                    match column {
                        Column::Active => "Pin to Active",
                        Column::InProgress => "Pin to In progress",
                        Column::InReview => "Pin to In review",
                        Column::Landed => "Pin to Landed",
                        Column::Stale => "Pin to Stale",
                    },
                    if current {
                        "Current manual placement"
                    } else {
                        ""
                    },
                    Action::Pin(card.clone(), Some(column)),
                )
                .selected(current)
                .on_press_maybe((!current).then(|| Action::Pin(card.clone(), Some(column))))
                .into(),
            );
        }
        widget::scrollable(
            widget::column::with_children(choices)
                .width(CARD_MENU_WIDTH)
                .spacing(space::XXS),
        )
        .height(Length::Shrink)
        .into()
    }
}

fn card_actions_id(card: &CardRef) -> widget::Id {
    format!(
        "work-actions:{}:{}",
        card.project.display(),
        card.key.storage_key()
    )
    .into()
}

fn item_title(item: &Item) -> &str {
    item.pull_request
        .as_ref()
        .map(|pr| pr.title.trim())
        .filter(|title| !title.is_empty())
        .or_else(|| {
            item.linear
                .as_ref()
                .and_then(|issue| issue.title.as_deref())
                .map(str::trim)
                .filter(|title| !title.is_empty())
        })
        .or_else(|| {
            item.branch
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
        })
        .or_else(|| {
            item.worktree
                .as_ref()
                .and_then(|worktree| worktree.path.file_name())
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
        })
        .unwrap_or("Worktree")
}

/// A scan summary, not a rewrite of the daemon's independent work/PR state.
fn lifecycle(item: &Item) -> (&'static str, Toned) {
    if item.error.is_some() {
        return ("Needs attention", design::destructive_color);
    }
    if item
        .sessions
        .iter()
        .any(|session| session.state == ActivityState::Waiting)
    {
        return ("Needs input", design::warning_color);
    }
    if item
        .sessions
        .iter()
        .any(|session| session.state == ActivityState::Working)
    {
        return ("Working", design::success_color);
    }
    if let Some(pr) = &item.pull_request {
        return match (pr.state, pr.draft) {
            (PullRequestState::Merged, _) => ("Merged", merged_color),
            (PullRequestState::Closed, _) => ("Closed", design::muted_color),
            (PullRequestState::Open, true) => ("Draft", design::muted_color),
            (PullRequestState::Open, false)
                if pr.checks.and_then(|checks| checks.outcome()) == Some(CheckOutcome::Failing) =>
            {
                ("Checks failed", design::destructive_color)
            }
            (PullRequestState::Open, false) if pr.review == Some(Review::ChangesRequested) => {
                ("Changes requested", design::warning_color)
            }
            (PullRequestState::Open, false) if pr.review == Some(Review::Required) => {
                ("Review needed", design::accent_color)
            }
            (PullRequestState::Open, false) => ("Open", design::success_color),
        };
    }
    (item.derived.label(), column_tone(item.derived))
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
    if item.error.is_some() {
        return glyph(Icon::Warning, design::destructive_color);
    }
    if item
        .sessions
        .iter()
        .any(|session| session.state == ActivityState::Waiting)
    {
        return glyph(Icon::AgentWaiting, design::warning_color);
    }
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

    type Ui<'a> = iced::runtime::UserInterface<'a, Action, Theme, cosmic::Renderer>;

    fn draw(ui: &mut Ui<'_>, renderer: &mut cosmic::Renderer) {
        ui.draw(
            renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            iced::mouse::Cursor::Unavailable,
        );
    }

    fn labels(ui: &Ui<'_>) -> Vec<String> {
        let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
        nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect()
    }

    fn control_bounds(ui: &Ui<'_>, label: &str) -> iced::Rectangle {
        let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
        let rect = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| node.node().label() == Some(label))
            .unwrap_or_else(|| panic!("missing native control {label}"))
            .node()
            .bounds()
            .unwrap();
        iced::Rectangle {
            x: rect.x0 as f32,
            y: rect.y0 as f32,
            width: (rect.x1 - rect.x0) as f32,
            height: (rect.y1 - rect.y0) as f32,
        }
    }

    fn press(ui: &mut Ui<'_>, renderer: &mut cosmic::Renderer, point: iced::Point) -> Vec<Action> {
        let events = [
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: point }),
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )),
        ];
        let mut actions = Vec::new();
        let _ = ui.update(
            &events,
            iced::mouse::Cursor::Available(point),
            renderer,
            &mut iced::advanced::clipboard::Null,
            &mut actions,
        );
        actions
    }

    fn pull_request_item() -> Item {
        let mut value = item("fix/internal-branch", "example");
        value.pull_request = Some(PullRequest {
            number: 42,
            title: "Keep the readable task title 首要工作 visible".into(),
            url: "https://github.com/example/project/pull/42".into(),
            state: PullRequestState::Open,
            draft: false,
            head_oid: "abc".into(),
            contains_local_head: false,
            checks: None,
            review: None,
            updated_at: String::new(),
        });
        value
    }

    #[derive(Default)]
    struct BoardScrollProbe {
        entries: Vec<(
            iced::advanced::widget::Id,
            iced::Rectangle,
            iced::Rectangle,
            iced::Vector,
        )>,
    }

    impl iced::advanced::widget::Operation for BoardScrollProbe {
        fn traverse(
            &mut self,
            operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation),
        ) {
            operate(self);
        }
        fn scrollable(
            &mut self,
            id: Option<&iced::advanced::widget::Id>,
            bounds: iced::Rectangle,
            content: iced::Rectangle,
            translation: iced::Vector,
            _state: &mut dyn iced::advanced::widget::operation::Scrollable,
        ) {
            self.entries
                .push((id.unwrap().clone(), bounds, content, translation));
        }
    }

    #[derive(Default)]
    struct BoardFocusProbe(Option<iced::advanced::widget::Id>);
    impl iced::advanced::widget::Operation for BoardFocusProbe {
        fn traverse(
            &mut self,
            operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation),
        ) {
            operate(self);
        }
        fn focusable(
            &mut self,
            id: Option<&iced::advanced::widget::Id>,
            _bounds: iced::Rectangle,
            state: &mut dyn iced::advanced::widget::operation::Focusable,
        ) {
            if state.is_focused() {
                self.0 = id.cloned();
            }
        }
    }

    fn scroll_probe(ui: &mut Ui<'_>, renderer: &cosmic::Renderer) -> BoardScrollProbe {
        let mut probe = BoardScrollProbe::default();
        ui.operate(renderer, &mut probe);
        probe
    }

    fn scroll_offsets(ui: &mut Ui<'_>, renderer: &cosmic::Renderer) -> (f32, f32) {
        let probe = scroll_probe(ui, renderer);
        let board = probe
            .entries
            .iter()
            .find(|(id, _, _, _)| id == &iced::advanced::widget::Id::new(board_scroll::ID))
            .unwrap();
        let lane = probe
            .entries
            .iter()
            .find(|(_, bounds, content, _)| content.height > bounds.height + 1.0)
            .unwrap();
        assert!(
            board.2.width > board.1.width + 100.0,
            "real horizontal overflow is required"
        );
        (board.3.x, lane.3.y)
    }

    fn wheel_board_state() -> State {
        let mut state = State::default();
        let items = (0..24)
            .map(|index| {
                let mut item = item(&format!("task-{index:02}"), "example");
                item.column = Column::Active;
                item.derived = Column::Active;
                item
            })
            .collect();
        connected(&mut state, 1, board(items));
        state
    }

    fn board_event(
        ui: &mut Ui<'_>,
        renderer: &mut cosmic::Renderer,
        event: iced::Event,
        point: iced::Point,
    ) {
        let mut actions = Vec::new();
        let _ = ui.update(
            &[event],
            iced::mouse::Cursor::Available(point),
            renderer,
            &mut iced::advanced::clipboard::Null,
            &mut actions,
        );
        assert!(
            actions.is_empty(),
            "scroll/modifier input emitted unrelated actions: {actions:?}"
        );
        draw(ui, renderer);
    }

    fn wheel_event(delta: iced::mouse::ScrollDelta) -> iced::Event {
        iced::Event::Mouse(iced::mouse::Event::WheelScrolled { delta })
    }

    #[test]
    fn board_shift_wheel_routes_to_columns_without_stealing_vertical_lane_transactions() {
        let state = wheel_board_state();
        let original = state.board.as_ref().unwrap().clone();
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut ui = Ui::build(
            state.board_view(800.0),
            iced::Size::new(800.0, 420.0),
            iced::runtime::user_interface::Cache::new(),
            &mut renderer,
        );
        draw(&mut ui, &mut renderer);
        let lane = scroll_probe(&mut ui, &renderer)
            .entries
            .into_iter()
            .find(|(_, bounds, content, _)| content.height > bounds.height + 1.0)
            .unwrap();
        let point = lane.1.center();
        assert_eq!(scroll_offsets(&mut ui, &renderer), (0.0, 0.0));
        board_event(
            &mut ui,
            &mut renderer,
            wheel_event(iced::mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 }),
            point,
        );
        let vertical = scroll_offsets(&mut ui, &renderer);
        assert_eq!(
            vertical,
            (0.0, 60.0),
            "ordinary wheel scrolls inside the actual populated lane"
        );
        board_event(
            &mut ui,
            &mut renderer,
            iced::Event::Keyboard(iced::keyboard::Event::ModifiersChanged(
                iced::keyboard::Modifiers::SHIFT,
            )),
            point,
        );
        board_event(
            &mut ui,
            &mut renderer,
            wheel_event(iced::mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 }),
            point,
        );
        assert_eq!(
            scroll_offsets(&mut ui, &renderer),
            (60.0, vertical.1),
            "Shift must reach the board even during the lane's native wheel transaction"
        );
        board_event(
            &mut ui,
            &mut renderer,
            iced::Event::Keyboard(iced::keyboard::Event::ModifiersChanged(
                iced::keyboard::Modifiers::empty(),
            )),
            point,
        );
        board_event(
            &mut ui,
            &mut renderer,
            wheel_event(iced::mouse::ScrollDelta::Pixels { x: 0.0, y: -11.5 }),
            point,
        );
        assert_eq!(
            scroll_offsets(&mut ui, &renderer),
            (60.0, vertical.1 + 11.5),
            "return to vertical is immediate, without a transaction timeout"
        );
        assert!(Arc::ptr_eq(state.board.as_ref().unwrap(), &original));
        assert_eq!(state.scope, None);
        assert_eq!(state.layout, Layout::Board);
    }

    #[test]
    fn board_wheel_requires_shift_for_native_x_and_preserves_pixel_motion_focus_and_cache() {
        use iced::advanced::widget::{Id, operation};
        let state = wheel_board_state();
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let size = iced::Size::new(800.0, 420.0);
        let mut ui = Ui::build(
            state.board_view(size.width),
            size,
            iced::runtime::user_interface::Cache::new(),
            &mut renderer,
        );
        draw(&mut ui, &mut renderer);
        let point = control_bounds(&ui, "Open task-00").center();
        for delta in [
            iced::mouse::ScrollDelta::Lines { x: -1.0, y: 0.0 },
            iced::mouse::ScrollDelta::Pixels { x: -35.0, y: 0.0 },
        ] {
            board_event(&mut ui, &mut renderer, wheel_event(delta), point);
            assert_eq!(scroll_offsets(&mut ui, &renderer), (0.0, 0.0));
        }
        board_event(
            &mut ui,
            &mut renderer,
            wheel_event(iced::mouse::ScrollDelta::Pixels { x: -12.5, y: -7.5 }),
            point,
        );
        assert_eq!(scroll_offsets(&mut ui, &renderer), (0.0, 7.5));
        let focus = card_actions_id(&CardRef::of(&state.board.as_ref().unwrap().items[0]));
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(focus.clone()),
        );
        board_event(
            &mut ui,
            &mut renderer,
            iced::Event::Keyboard(iced::keyboard::Event::ModifiersChanged(
                iced::keyboard::Modifiers::SHIFT,
            )),
            point,
        );
        board_event(
            &mut ui,
            &mut renderer,
            wheel_event(iced::mouse::ScrollDelta::Pixels { x: -12.5, y: 12.5 }),
            point,
        );
        assert_eq!(
            scroll_offsets(&mut ui, &renderer),
            (12.5, 7.5),
            "real x wins rather than cancelling a diagonal trackpad gesture"
        );
        let cache = ui.into_cache();
        let mut ui = Ui::build(state.board_view(size.width), size, cache, &mut renderer);
        draw(&mut ui, &mut renderer);
        board_event(
            &mut ui,
            &mut renderer,
            wheel_event(iced::mouse::ScrollDelta::Lines { x: -1.0, y: 1.0 }),
            point,
        );
        assert_eq!(
            scroll_offsets(&mut ui, &renderer),
            (72.5, 7.5),
            "cached native Shift state and line scale survive rebuild"
        );
        board_event(
            &mut ui,
            &mut renderer,
            iced::Event::Window(iced::window::Event::Unfocused),
            point,
        );
        board_event(
            &mut ui,
            &mut renderer,
            wheel_event(iced::mouse::ScrollDelta::Pixels { x: -3.0, y: -9.0 }),
            point,
        );
        assert_eq!(
            scroll_offsets(&mut ui, &renderer),
            (72.5, 16.5),
            "lost window focus cannot leave the board in Shift mode"
        );
        // Native widget operations remain available, independent of wheel policy.
        ui.operate(
            &renderer,
            &mut operation::scrollable::scroll_by::<()>(
                Id::new(board_scroll::ID),
                operation::scrollable::AbsoluteOffset { x: 20.0, y: 0.0 },
            ),
        );
        assert_eq!(scroll_offsets(&mut ui, &renderer), (92.5, 16.5));
        let mut focused = BoardFocusProbe::default();
        ui.operate(&renderer, &mut focused);
        assert_eq!(focused.0, Some(focus));
        board_event(
            &mut ui,
            &mut renderer,
            iced::Event::Window(iced::window::Event::Focused),
            point,
        );
        let mut actions = Vec::new();
        let events = [
            iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
                modified_key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
                physical_key: iced::keyboard::key::Physical::Unidentified(
                    iced::keyboard::key::NativeCode::Unidentified,
                ),
                location: iced::keyboard::Location::Standard,
                modifiers: iced::keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            },
            iced::keyboard::Event::KeyReleased {
                key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
                modified_key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
                physical_key: iced::keyboard::key::Physical::Unidentified(
                    iced::keyboard::key::NativeCode::Unidentified,
                ),
                location: iced::keyboard::Location::Standard,
                modifiers: iced::keyboard::Modifiers::empty(),
            },
        ]
        .map(iced::Event::Keyboard);
        let _ = ui.update(
            &events,
            iced::mouse::Cursor::Unavailable,
            &mut renderer,
            &mut iced::advanced::clipboard::Null,
            &mut actions,
        );
        assert!(
            matches!(actions.as_slice(), [Action::Menu(Some(card))] if card == &CardRef::of(&state.board.as_ref().unwrap().items[0]))
        );
    }

    #[test]
    fn board_shift_wheel_ignores_captured_input_and_clipped_or_outside_hits() {
        use iced::advanced::{Layout as NativeLayout, Shell, Widget, layout, widget::Tree};
        let state = wheel_board_state();
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut element = state.board_view(800.0);
        let mut tree = Tree::new(&element);
        let bounds = iced::Rectangle::with_size(iced::Size::new(800.0, 420.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, bounds.size()),
        );
        let mut actions = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &iced::Event::Keyboard(iced::keyboard::Event::ModifiersChanged(
                iced::keyboard::Modifiers::SHIFT,
            )),
            NativeLayout::new(&node),
            iced::mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut actions),
            &bounds,
        );
        for (point, viewport, captured) in [
            (bounds.center(), bounds, true),
            (iced::Point::new(900.0, 300.0), bounds, false),
            (
                bounds.center(),
                iced::Rectangle::new(iced::Point::new(0.0, 0.0), iced::Size::new(100.0, 100.0)),
                false,
            ),
        ] {
            let mut shell = Shell::new(&mut actions);
            if captured {
                shell.capture_event();
            }
            element.as_widget_mut().update(
                &mut tree,
                &wheel_event(iced::mouse::ScrollDelta::Pixels { x: -40.0, y: -20.0 }),
                NativeLayout::new(&node),
                iced::mouse::Cursor::Available(point),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut shell,
                &viewport,
            );
            let mut probe = BoardScrollProbe::default();
            element.as_widget_mut().operate(
                &mut tree,
                NativeLayout::new(&node),
                &renderer,
                &mut probe,
            );
            assert!(
                probe
                    .entries
                    .iter()
                    .all(|(_, _, _, translation)| *translation == iced::Vector::ZERO),
                "captured or invisible input must not move the board or its columns: {:?}",
                probe.entries
            );
        }
        assert!(actions.is_empty());
    }

    #[test]
    fn compact_rows_keep_actionable_checks_and_reviews_during_agent_activity_and_drafts() {
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        for (activity, draft, review) in [
            (Some(ActivityState::Working), false, None),
            (
                Some(ActivityState::Waiting),
                false,
                Some(Review::ChangesRequested),
            ),
            (None, true, Some(Review::ChangesRequested)),
        ] {
            let mut value = pull_request_item();
            let pr = value.pull_request.as_mut().unwrap();
            pr.draft = draft;
            pr.review = review;
            pr.checks = Some(crate::work::Checks {
                passed: 2,
                pending: 0,
                failed: 1,
            });
            if let Some(activity) = activity {
                value.sessions.push(crate::work::Session {
                    id: TerminalId::new(),
                    provider: None,
                    title: Some("Ada".into()),
                    state: activity,
                });
            }
            let mut state = State::default();
            connected(&mut state, 1, board(vec![value]));
            for width in [260.0, 620.0] {
                let shown = &state.board.as_ref().unwrap().items[0];
                let mut ui = Ui::build(
                    state.row(shown),
                    iced::Size::new(width, 400.0),
                    iced::runtime::user_interface::Cache::new(),
                    &mut renderer,
                );
                draw(&mut ui, &mut renderer);
                let visible = labels(&ui);
                assert!(
                    visible.iter().any(|label| label == "Checks failed"),
                    "{visible:?}"
                );
                if review == Some(Review::ChangesRequested) {
                    assert!(
                        visible.iter().any(|label| label == "Changes requested"),
                        "{visible:?}"
                    );
                }
                assert!(
                    !visible
                        .iter()
                        .any(|label| label.contains("fix/internal-branch"))
                );
                let more = control_bounds(&ui, &format!("Actions for {}", item_title(shown)));
                assert!(more.width >= 20.0 && more.x + more.width <= width);
                ui.draw(
                    &mut renderer,
                    &cosmic::theme::active(),
                    &iced::advanced::renderer::Style::default(),
                    iced::mouse::Cursor::Available(more.center()),
                );
                assert_eq!(
                    control_bounds(&ui, &format!("Actions for {}", item_title(shown))),
                    more,
                    "hover must not change the menu/title geometry"
                );
                let actions = press(&mut ui, &mut renderer, more.center());
                assert!(
                    matches!(actions.as_slice(), [Action::Menu(Some(_))]),
                    "More must never also open its parent row: {actions:?}"
                );
            }
        }
    }

    #[test]
    fn retained_row_press_does_not_open_a_replacement_and_old_menu_close_keeps_the_current_owner() {
        let mut second = pull_request_item();
        second.key = ItemKey::Branch("second".into());
        second.branch = Some("second".into());
        let mut state = State::default();
        connected(&mut state, 1, board(vec![pull_request_item(), second]));
        let a = CardRef::of(&state.board.as_ref().unwrap().items[0]);
        let b = CardRef::of(&state.board.as_ref().unwrap().items[1]);
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let size = iced::Size::new(400.0, 300.0);
        let title = format!(
            "Open {}",
            item_title(&state.board.as_ref().unwrap().items[0])
        );
        let mut original = Ui::build(
            state.row(&state.board.as_ref().unwrap().items[0]),
            size,
            iced::runtime::user_interface::Cache::new(),
            &mut renderer,
        );
        draw(&mut original, &mut renderer);
        let hit = control_bounds(&original, &title).center();
        let mut actions = Vec::new();
        let _ = original.update(
            &[iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                iced::mouse::Button::Left,
            ))],
            iced::mouse::Cursor::Available(hit),
            &mut renderer,
            &mut iced::advanced::clipboard::Null,
            &mut actions,
        );
        assert!(actions.is_empty());
        let cache = original.into_cache();
        let mut replacement = Ui::build(
            state.row(&state.board.as_ref().unwrap().items[1]),
            size,
            cache,
            &mut renderer,
        );
        draw(&mut replacement, &mut renderer);
        let _ = replacement.update(
            &[iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            ))],
            iced::mouse::Cursor::Available(hit),
            &mut renderer,
            &mut iced::advanced::clipboard::Null,
            &mut actions,
        );
        assert!(
            actions.is_empty(),
            "held press must not migrate to another card: {actions:?}"
        );
        assert!(
            matches!(press(&mut replacement, &mut renderer, hit).as_slice(), [Action::Open(card)] if card == &b)
        );
        drop(replacement);
        state.update(Action::Menu(Some(a.clone())));
        state.update(Action::Menu(Some(b.clone())));
        state.update(Action::CloseMenu(a));
        assert_eq!(state.menu.as_ref(), Some(&b));
        state.update(Action::CloseMenu(b));
        assert_eq!(state.menu, None);
    }

    #[test]
    fn merged_active_details_explain_independent_states_and_do_not_mutate_the_board() {
        let mut value = pull_request_item();
        value.pull_request.as_mut().unwrap().state = PullRequestState::Merged;
        value.sessions.push(crate::work::Session {
            id: TerminalId::new(),
            provider: None,
            title: Some("Ada".into()),
            state: ActivityState::Working,
        });
        value.pinned = true;
        value.column = Column::InReview;
        let mut state = State::default();
        let original = board(vec![value]);
        connected(&mut state, 1, original.clone());
        let card = CardRef::of(&original.items[0]);
        assert!(state.update(Action::Details(card.clone())).is_none());
        assert_eq!(state.board.as_ref().unwrap().as_ref(), original.as_ref());
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let shown = &state.board.as_ref().unwrap().items[0];
        let mut ui = Ui::build(
            state.row(shown),
            iced::Size::new(320.0, 1000.0),
            iced::runtime::user_interface::Cache::new(),
            &mut renderer,
        );
        draw(&mut ui, &mut renderer);
        let shown_labels = labels(&ui);
        assert!(shown_labels.iter().any(|label| label == "Working"));
        assert!(
            shown_labels
                .iter()
                .any(|label| label == "Pull request: #42 · Merged")
        );
        assert!(
            shown_labels
                .iter()
                .any(|label| label.contains("a session is still active"))
        );
        assert!(
            shown_labels
                .iter()
                .any(|label| label == "Pinned board position: In review")
        );
        drop(ui);
        watched(&mut state, 2, board(vec![]));
        assert_eq!(state.details, None);
        assert!(state.update(Action::Details(card)).is_none());
        assert_eq!(state.details, None);
    }

    #[test]
    fn unrelated_linear_setup_is_absent_and_placement_is_an_intentional_disclosure() {
        let mut state = State::default();
        let mut snapshot = (*board(vec![pull_request_item()])).clone();
        snapshot.linear_unavailable = Some("Set LINEAR_API_KEY or configure your keyring".into());
        connected(&mut state, 1, Arc::new(snapshot));
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut status = Ui::build(
            state.status().unwrap(),
            iced::Size::new(620.0, 100.0),
            iced::runtime::user_interface::Cache::new(),
            &mut renderer,
        );
        draw(&mut status, &mut renderer);
        assert!(
            !labels(&status)
                .iter()
                .any(|label| label.contains("Linear") || label.contains("LINEAR_API_KEY"))
        );
        drop(status);
        let card = CardRef::of(&state.board.as_ref().unwrap().items[0]);
        state.update(Action::Menu(Some(card.clone())));
        let shown = &state.board.as_ref().unwrap().items[0];
        let mut ui = Ui::build(
            state.card_menu(shown, &card),
            iced::Size::new(400.0, 600.0),
            iced::runtime::user_interface::Cache::new(),
            &mut renderer,
        );
        draw(&mut ui, &mut renderer);
        let visible = labels(&ui);
        assert!(
            visible.iter().any(|label| label == "Show details"),
            "{visible:?}"
        );
        assert!(
            visible.iter().any(|label| label == "Board placement…"),
            "{visible:?}"
        );
        assert!(!visible.iter().any(|label| label.starts_with("Pin to")));
        let hit = control_bounds(&ui, "Show details").center();
        let actions = press(&mut ui, &mut renderer, hit);
        assert!(
            matches!(actions.as_slice(), [Action::Details(actual)] if actual == &card),
            "{actions:?}"
        );
    }

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
        state.set_listening(true);
        state.update(Action::Watched(Watch::Board {
            epoch: state.listening_epoch().unwrap(),
            revision,
            board,
            first: false,
        }));
    }

    fn connected(state: &mut State, revision: u64, board: Arc<Board>) {
        state.set_listening(true);
        state.update(Action::Watched(Watch::Board {
            epoch: state.listening_epoch().unwrap(),
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
        state.update(Action::Pin(card.clone(), Some(Column::Stale)));
        state.update(Action::Pinned(Err(
            "the Work board store is unavailable".into()
        )));
        let shown = state.item(&card).unwrap();
        assert_eq!((shown.column, shown.pinned), (Column::InProgress, false));
        assert!(state.error.is_some());
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

    fn current_view_labels(state: &State) -> Vec<String> {
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut ui = Ui::build(
            state.view(),
            iced::Size::new(800.0, 800.0),
            iced::runtime::user_interface::Cache::new(),
            &mut renderer,
        );
        draw(&mut ui, &mut renderer);
        labels(&ui)
    }

    #[test]
    fn hidden_listener_cannot_publish_old_first_or_lost_and_reveal_waits_for_current_board() {
        let mut state = State::default();
        assert_eq!(state.listening_epoch(), None);
        state.set_listening(true);
        let original = state.listening_epoch().unwrap();
        connected(
            &mut state,
            10,
            board(vec![item("feat/original", "cagents")]),
        );
        assert!(!state.awaiting_current);
        state.update(Action::Query("feat".into()));
        state.update(Action::Layout(Layout::List));
        assert!(matches!(
            state.update(Action::Refresh),
            Some(Effect::Refresh)
        ));
        let card = CardRef::of(&state.board.as_ref().unwrap().items[0]);
        assert!(matches!(
            state.update(Action::Pin(card.clone(), Some(Column::Stale))),
            Some(Effect::Pin {
                column: Some(Column::Stale),
                ..
            })
        ));
        let retained = state.board.clone().unwrap();
        let old_first = Watch::Board {
            epoch: original,
            revision: 1_000,
            board: board(vec![item("wrong", "elsewhere")]),
            first: true,
        };
        state.set_listening(false);
        state.update(Action::Watched(old_first.clone()));
        state.update(Action::Watched(Watch::Lost {
            epoch: original,
            error: "old hidden connection".into(),
        }));
        assert!(Arc::ptr_eq(state.board.as_ref().unwrap(), &retained));
        assert_eq!(state.listening_epoch(), None);
        assert!(state.lost.is_none());
        assert!(state.refreshing);

        state.set_listening(true);
        let current = state.listening_epoch().unwrap();
        assert!(current > original);
        state.set_listening(true);
        assert_eq!(state.listening_epoch(), Some(current));
        state.update(Action::Watched(old_first));
        state.update(Action::Watched(Watch::Lost {
            epoch: original,
            error: "old queued failure".into(),
        }));
        assert!(Arc::ptr_eq(state.board.as_ref().unwrap(), &retained));
        assert_eq!(state.item(&card).unwrap().column, Column::Stale);
        assert!(state.refreshing);
        assert_eq!(state.query, "feat");
        assert_eq!(state.layout, Layout::List);
        assert!(state.lost.is_none());
        assert!(state.awaiting_current);
        assert!(
            current_view_labels(&state)
                .iter()
                .any(|label| label == "Updating Work…")
        );

        // A real current first reply can reset the daemon's revision. An old
        // first reply cannot impersonate it even with a larger revision.
        state.update(Action::Watched(Watch::Board {
            epoch: current,
            revision: 1,
            board: board(vec![item("feat/current", "cagents")]),
            first: true,
        }));
        assert_eq!(state.revision, 1);
        assert_eq!(
            state.visible().map(Item::label).collect::<Vec<_>>(),
            ["feat/current"]
        );
        assert!(!state.awaiting_current);
        assert!(
            state.refreshing,
            "visibility does not finish an explicit request"
        );
        assert!(
            !current_view_labels(&state)
                .iter()
                .any(|label| label == "Updating Work…")
        );
        state.update(Action::Refreshed(Ok(())));
        assert!(!state.refreshing);
        state.update(Action::Pinned(Err("original pin refused".into())));
        assert_eq!(state.error.as_deref(), Some("original pin refused"));
        assert_eq!(
            state.visible().map(Item::label).collect::<Vec<_>>(),
            ["feat/current"]
        );

        state.update(Action::Watched(Watch::Lost {
            epoch: current,
            error: "current connection lost".into(),
        }));
        assert!(state.awaiting_current);
        let shown = current_view_labels(&state);
        assert!(shown.iter().any(|label| label == "Updating Work…"));
        assert!(
            shown
                .iter()
                .any(|label| label.contains("current connection lost"))
        );
        connected(
            &mut state,
            1,
            board(vec![item("feat/reconnected", "cagents")]),
        );
        assert!(!state.awaiting_current);
        assert!(state.lost.is_none());
        assert_eq!(state.error.as_deref(), Some("original pin refused"));
    }

    #[test]
    fn closing_and_exhausted_listener_epochs_never_reuse_queued_publication_authority() {
        let mut state = State::default();
        connected(&mut state, 1, board(vec![item("original", "cagents")]));
        let first = state.listening_epoch().unwrap();
        state.reset();
        assert_eq!(state.listening_epoch(), None);
        assert!(state.board.is_none());
        state.set_listening(true);
        let next = state.listening_epoch().unwrap();
        assert!(next > first);
        state.update(Action::Watched(Watch::Board {
            epoch: first,
            revision: 99,
            board: board(vec![item("old closed pane", "cagents")]),
            first: true,
        }));
        assert!(state.board.is_none());

        // Exercise the checked boundary without looping or wrapping a token.
        state.set_listening(false);
        state.watch_epoch = u64::MAX - 1;
        state.set_listening(true);
        assert_eq!(state.listening_epoch(), Some(u64::MAX));
        connected(&mut state, 1, board(vec![item("last epoch", "cagents")]));
        state.reset();
        state.set_listening(true);
        assert_eq!(state.listening_epoch(), None);
        state.update(Action::Watched(Watch::Board {
            epoch: u64::MAX,
            revision: 2,
            board: board(vec![item("must not return", "cagents")]),
            first: true,
        }));
        assert!(state.board.is_none());
        assert!(
            current_view_labels(&state)
                .iter()
                .any(|label| label == "Work updates cannot restart")
        );
    }
}
