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
    work::{ActivitySource, Board, Column, Forge, Item, ItemKey, PullRequestState},
};

/// A semantic color, read from the drawing theme.
type Toned = fn(&Theme) -> iced::Color;

/// The narrowest a board column may be before the board scrolls sideways.
const COLUMN_MIN_WIDTH: f32 = 248.0;
/// A column's width when the board scrolls sideways.
const COLUMN_WIDTH: f32 = 280.0;
const SCOPE_MENU_WIDTH: f32 = 280.0;
const CARD_MENU_WIDTH: f32 = 264.0;
/// The details of what the board could not read.
const PROBLEMS_WIDTH: f32 = 440.0;
const PROBLEMS_MAX_HEIGHT: f32 = 360.0;

/// A board card's height before it has been measured: branch, title, facts
/// and footer at the compact sizes.
const CARD_ESTIMATE: f32 = 104.0;
/// A list row's height before it has been measured: title and facts lines.
const ROW_ESTIMATE: f32 = 54.0;

/// The keys a culled column keeps each card's widget state by.
fn card_keys(items: &[&Item]) -> Vec<u64> {
    items
        .iter()
        .map(|item| card_key(&item.project, &item.key))
        .collect()
}

/// A card's key in a culled column, from what identifies it.
fn card_key(project: &std::path::Path, key: &ItemKey) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    project.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
}

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
    /// Every checkout of every open project, to open, create and delete,
    /// which [`crate::worktrees_ui`] draws from the window's own project
    /// model rather than from the daemon's board.
    Worktrees,
}

impl Layout {
    /// Whether this is one of the daemon board's own layouts.
    pub fn is_board(self) -> bool {
        !matches!(self, Self::Worktrees)
    }
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
    /// Opens or closes the details of what the board could not read.
    Problems(bool),
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
    /// Why the subscription is down, while it is retried.
    lost: Option<String>,
    scope: Option<PathBuf>,
    scope_menu: bool,
    /// Whether the details of what the board could not read are open.
    problems_open: bool,
    query: String,
    layout: Layout,
    /// The board layout last shown, which the board returns to from
    /// [`Layout::Worktrees`].
    board_layout: Layout,
    menu: Option<CardRef>,
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
        let (layout, board_layout) = (self.layout, self.board_layout);
        *self = Self {
            scope,
            layout,
            board_layout,
            ..Self::default()
        };
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Whether the pane shows the project checkouts rather than the board.
    pub fn shows_worktrees(&self) -> bool {
        self.layout == Layout::Worktrees
    }

    /// Shows the checkouts, or the board in the layout it last had.
    pub fn show(&mut self, worktrees: bool) {
        self.layout = if worktrees {
            Layout::Worktrees
        } else {
            self.board_layout
        };
        self.menu = None;
        self.scope_menu = false;
        self.problems_open = false;
    }

    pub fn update(&mut self, action: Action) -> Option<Effect> {
        match action {
            Action::Watched(Watch::Board {
                revision,
                board,
                first,
            }) => {
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
                    // Details that are no longer offered do not reopen by
                    // themselves when they are again.
                    if self.problems_open && !disclosed(&problems(&board, self.scope.as_deref())) {
                        self.problems_open = false;
                    }
                    self.confirmed = Some(board.clone());
                    self.board = Some(board);
                    self.lost = None;
                }
                None
            }
            Action::Watched(Watch::Lost(error)) => {
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
            Action::Problems(open) => {
                self.problems_open = open;
                None
            }
            Action::Scope(scope) => {
                self.scope = scope;
                self.scope_menu = false;
                self.problems_open = false;
                None
            }
            Action::Query(query) => {
                self.query = query;
                None
            }
            Action::Layout(layout) => {
                self.layout = layout;
                if layout.is_board() {
                    self.board_layout = layout;
                }
                self.menu = None;
                self.scope_menu = false;
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
                self.menu = menu;
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
                .icon(Icon::Branch.handle())
                .detail(
                    "Open a Git project and its worktrees, branches and pull requests appear here.",
                )
                .into(),
            Some(_) => match self.layout {
                Layout::Board => self.board_view(width),
                // The application draws the checkouts in this pane's place.
                Layout::List | Layout::Worktrees => self.list_view(),
            },
        };
        widget::column::with_children(vec![header, design::horizontal_rule(), body])
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
        let layout = layout_switch(self.layout);
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
    ///
    /// One line, however many projects share a problem: GitHub's rate limit
    /// is the account's, so every project reports it at once. The projects
    /// and their exact reasons are a click away.
    fn status(&self) -> Option<Element<'_, Action>> {
        let board = self.board.as_ref()?;
        // The daemon sends a board only when it changes, so its age says
        // nothing about how current it is; a live subscription is current.
        let mut parts: Vec<Element<'_, Action>> = vec![
            design::caption(cards(self.visible().count()))
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
        ];
        if let Some(error) = &self.lost {
            parts.push(notice(format!("Disconnected: {error}"), design::muted_text));
        }
        if let Some(error) = &self.error {
            // A refresh or a pin the user asked for, which they may retry.
            parts.push(notice(error.clone(), design::warning_text));
        }
        let problems = problems(board, self.scope.as_deref());
        if let Some(problems) = self.problems_view(problems) {
            parts.push(problems);
        }
        Some(
            widget::row::with_children(parts)
                .spacing(space::M)
                .align_y(Alignment::Center)
                .into(),
        )
    }

    /// The first of the board's problems, opening all of them with the
    /// projects each affects. A lone problem of one project reads whole.
    fn problems_view<'a>(&'a self, problems: Vec<Problem<'a>>) -> Option<Element<'a, Action>> {
        let first = problems.first()?;
        let summary = first.summary();
        let affected = affected_projects(&problems);
        if !disclosed(&problems) {
            return Some(notice(summary, design::muted_text));
        }
        let count = if problems.len() == 1 {
            projects(affected)
        } else {
            format!("{} problems", problems.len())
        };
        let anchor = widget::button::custom(
            widget::row::with_children(vec![
                warning_icon(),
                crate::ui_controls::FadeText::caption(summary).into(),
                design::caption(count)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
                widget::icon(Icon::ChevronDown.handle())
                    .size(design::COMPACT_ICON_SIZE)
                    .into(),
            ])
            .spacing(space::XS)
            .align_y(Alignment::Center),
        )
        .name("Show what could not be read")
        .class(crate::ui_controls::control_style(self.problems_open, None))
        .padding([space::XXS, space::XS])
        .on_press(Action::Problems(!self.problems_open));
        let content = self.problems_open.then(|| {
            let mut sections = Vec::with_capacity(problems.len() * 2);
            for (index, problem) in problems.iter().enumerate() {
                if index > 0 {
                    sections.push(design::horizontal_rule());
                }
                sections.push(problem_details(problem));
            }
            widget::container(crate::ui_controls::popover_scroll(
                widget::column::with_children(sections).spacing(space::S),
                widget::Id::new("work-problems"),
                PROBLEMS_MAX_HEIGHT,
                crate::ui_controls::SCROLLING_FRAME,
            ))
            .width(crate::ui_controls::scrolling_width(PROBLEMS_WIDTH))
            .into()
        });
        Some(crate::ui_controls::scrolling_popover(
            crate::interaction::tooltip(anchor, "Show which projects, and why"),
            content,
            widget::popover::Position::Bottom,
            Action::Problems(false),
        ))
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
            crate::interaction::horizontal_wheel_around_vertical(
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
            .padding(Padding::new(space::S).top(space::S + space::XS))
            .width(Length::Fill)
            .into()
        } else {
            // Cards scroll in the open lane right up to its heading, so a
            // card crossing either edge fades into the lane rather than
            // being sliced there. The gap below the heading scrolls with
            // them.
            crate::interaction::overflow_fade_y(
                crate::ui_controls::keep_in_place(
                    widget::scrollable(
                        widget::container(
                            crate::ui_controls::culled_column(card_keys(&items), move |index| {
                                self.card(items[index])
                            })
                            .spacing(space::S)
                            .estimate(CARD_ESTIMATE)
                            .keep_built(self.open_menu_key()),
                        )
                        .padding(design::with_vertical_rail(Padding {
                            top: space::XS,
                            ..Padding::from([0.0, space::XS])
                        })),
                    )
                    .direction(iced::widget::scrollable::Direction::Vertical(
                        design::scrollbar(),
                    ))
                    .height(Length::Fill),
                ),
                lane_fill,
            )
        };
        widget::container(
            widget::column::with_children(vec![header.into(), body]).height(Length::Fill),
        )
        // The lane's own inset at the bottom keeps the fade there inside its
        // rounded corners.
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
        let items = self.visible().collect::<Vec<_>>();
        if items.is_empty() {
            return self.no_match();
        }
        // As in a lane: a row crossing either edge fades into the pane
        // rather than being sliced there.
        crate::interaction::overflow_fade_y(
            crate::ui_controls::keep_in_place(
                widget::scrollable(
                    widget::container(
                        crate::ui_controls::culled_column(card_keys(&items), move |index| {
                            self.row(items[index])
                        })
                        .spacing(space::XS)
                        .estimate(ROW_ESTIMATE)
                        .keep_built(self.open_menu_key()),
                    )
                    .padding(design::with_vertical_rail(Padding::from([
                        space::M,
                        space::L,
                    ]))),
                )
                .direction(iced::widget::scrollable::Direction::Vertical(
                    design::scrollbar(),
                ))
                .width(Length::Fill)
                .height(Length::Fill),
            ),
            design::base,
        )
    }

    /// The card whose menu is open stays built while it scrolls away, so
    /// its menu stays open and closes on a click elsewhere.
    fn open_menu_key(&self) -> Option<u64> {
        self.menu
            .as_ref()
            .map(|card| card_key(&card.project, &card.key))
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
            crate::ui_controls::FadeText::heading(
                item.branch.as_deref().unwrap_or("Detached HEAD"),
            )
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
        let button = widget::button::custom(content)
            .name(format!("Open {}", item.label()))
            .class(card_style(selected))
            .padding(space::S)
            .width(Length::Fill)
            .on_press(Action::Open(card.clone()));
        widget::mouse_area(crate::interaction::hover(button))
            .on_right_press(Action::Menu(Some(card)))
            .into()
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
                crate::ui_controls::fade_text(format!(
                    "#{} {}",
                    pull_request.number, pull_request.title
                ))
                .class(cosmic::theme::Text::Custom(design::muted_text))
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
                design::caption(crate::ui_controls::display_path(&worktree.path))
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
        let button =
            widget::button::custom(widget::column::with_children(lines).spacing(space::XS))
                .name(format!("Open {}", item.label()))
                .class(card_style(selected))
                .padding([space::XS + space::XXS, space::S])
                .width(Length::Fill)
                .on_press(Action::Open(card.clone()));
        widget::mouse_area(crate::interaction::hover(button))
            .on_right_press(Action::Menu(Some(card)))
            .into()
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
            // The leading glyph gives the state unless an agent's activity
            // has its place; then a small glyph beside the facts says it.
            if !status_glyph_is_pull_request(item) {
                let state = crate::sidebar::WorktreePullRequestState::from_work(pull_request);
                facts.push(crate::interaction::tooltip(
                    crate::ui_controls::pull_request_state_glyph(
                        Some(state),
                        design::COMPACT_ICON_SIZE,
                    ),
                    format!("{} pull request", state.label()),
                ));
            }
            facts.extend(pull_request_badges(pull_request));
            if pull_request.state == PullRequestState::Open
                && !pull_request.contains_local_head
                && item.worktree.is_some()
            {
                facts.push(crate::interaction::tooltip(
                    badge("Unpushed", Tone::Themed(design::muted_icon_color)),
                    "The worktree has commits the pull request does not",
                ));
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
                        Tone::Themed(design::muted_icon_color),
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
                    crate::sidebar::attention_mark(crate::sidebar::AttentionState::Working, 12)
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
                    ActivityState::Idle | ActivityState::Unknown => "idle",
                    ActivityState::Waiting => "waiting for you",
                    ActivityState::Approval => "waiting for your permission",
                    ActivityState::Question => "waiting for your answer",
                    ActivityState::Working => "working",
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
        let anchor = icon_button(Icon::More, "Card actions")
            .compact()
            .selected(open)
            .on_press(Action::Menu((!open).then(|| card.clone())));
        let content = open.then(|| {
            let mut items: Vec<Element<'a, Action>> = Vec::new();
            if let Some(worktree) = &item.worktree {
                items.push(
                    popover_item(
                        Icon::OpenFolder.handle(),
                        "Open worktree",
                        crate::ui_controls::display_path(&worktree.path),
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
                        Icon::Browser.handle(),
                        "Open Linear issue",
                        ticket.identifier.clone().unwrap_or_default(),
                        Action::OpenUrl(url.clone()),
                    )
                    .into(),
                );
            }
            if let Some(branch) = &item.branch {
                let action = copy_branch_action(item);
                let enabled = action.is_some();
                let control = popover_item(
                    Icon::Copy.handle(),
                    "Copy branch name",
                    branch.as_str(),
                    Action::Copy(branch.clone()),
                )
                .on_press_maybe(action);
                items.push(if enabled {
                    control.into()
                } else {
                    crate::interaction::tooltip(control, "This branch name is not valid UTF-8")
                });
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
                items.push(
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
                    .on_press_maybe((!current).then(|| Action::Pin(card.clone(), Some(column))))
                    .into(),
                );
            }
            if item.pinned {
                items.push(
                    popover_item(
                        Icon::Undo.handle(),
                        "Unpin",
                        format!("Return to {}", item.derived.label()),
                        Action::Pin(card.clone(), None),
                    )
                    .into(),
                );
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

/// The pane's views: the board's two layouts and the project checkouts.
pub fn layout_switch<'a>(current: Layout) -> Element<'a, Action> {
    crate::ui_controls::segmented_controls(vec![
        icon_button(Icon::Board, "Board")
            .selected(current == Layout::Board)
            .on_press(Action::Layout(Layout::Board))
            .into(),
        icon_button(Icon::List, "List, newest first")
            .selected(current == Layout::List)
            .on_press(Action::Layout(Layout::List))
            .into(),
        icon_button(Icon::Worktree, "Worktrees")
            .selected(current == Layout::Worktrees)
            .on_press(Action::Layout(Layout::Worktrees))
            .into(),
    ])
}

/// Clipboard text cannot preserve an invalid branch name's raw bytes.
fn copy_branch_action(item: &Item) -> Option<Action> {
    if let ItemKey::Branch(identity) = &item.key
        && crate::work::branch_name(identity).to_str().is_none()
    {
        return None;
    }
    item.branch.clone().map(Action::Copy)
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
        Column::InProgress => design::muted_icon_color,
        Column::InReview => design::accent_color,
        Column::Landed => merged_color,
        Column::Stale => design::muted_color,
    }
}

/// Landed work takes the tone merged pull requests have everywhere.
fn merged_color(theme: &Theme) -> iced::Color {
    crate::sidebar::WorktreePullRequestState::Merged.color(theme)
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

/// Whether the card's leading glyph is its pull request's state, rather than
/// an agent's activity or a branch.
fn status_glyph_is_pull_request(item: &Item) -> bool {
    matches!(Leading::of(item), Leading::PullRequest(_))
}

/// What the card's leading glyph says at a glance: a working agent, else an
/// agent waiting, else the pull request's state, else a branch.
enum Leading<'a> {
    Working,
    /// The most urgent wait among the card's agents.
    Waiting(crate::sidebar::AttentionState),
    PullRequest(&'a crate::work::PullRequest),
    Branch,
}

impl<'a> Leading<'a> {
    fn of(item: &'a Item) -> Self {
        let any = |state| item.sessions.iter().any(|session| session.state == state);
        if any(ActivityState::Working) {
            Self::Working
        } else if let Some(wait) = item
            .sessions
            .iter()
            .filter_map(|session| crate::sidebar::AttentionState::of_activity(session.state))
            .filter(|state| state.waits_on_user())
            .max()
        {
            Self::Waiting(wait)
        } else if let Some(pull_request) = &item.pull_request {
            Self::PullRequest(pull_request)
        } else {
            Self::Branch
        }
    }
}

fn status_glyph<'a>(item: &Item) -> Element<'a, Action> {
    match Leading::of(item) {
        Leading::Working => widget::container(crate::sidebar::attention_mark(
            crate::sidebar::AttentionState::Working,
            12,
        ))
        .center(Length::Fixed(f32::from(design::ROW_ICON_SIZE)))
        .into(),
        Leading::Waiting(state) => crate::sidebar::attention_mark(state, design::ROW_ICON_SIZE),
        Leading::PullRequest(pull_request) => pull_request_glyph(pull_request),
        // A checked-out branch is a worktree, drawn as the sidebar draws it.
        Leading::Branch if item.worktree.is_some() => glyph(Icon::Worktree, design::muted_color),
        Leading::Branch => glyph(Icon::Branch, design::muted_color),
    }
}

/// A pull request's state as its leading glyph: open, draft, merged or closed.
pub(crate) fn pull_request_glyph<'a, Message: 'a>(
    pull_request: &crate::work::PullRequest,
) -> Element<'a, Message> {
    crate::ui_controls::pull_request_state_glyph(
        Some(crate::sidebar::WorktreePullRequestState::from_work(
            pull_request,
        )),
        design::ROW_ICON_SIZE,
    )
}

/// What is true of an open pull request now: its checks and review, as
/// glyphs whose tooltips say the counts and the decision. Its state is its
/// leading glyph's to say.
pub(crate) fn pull_request_badges<'a, Message: 'a>(
    pull_request: &crate::work::PullRequest,
) -> Vec<Element<'a, Message>> {
    if pull_request.state != PullRequestState::Open {
        return Vec::new();
    }
    let size = design::COMPACT_ICON_SIZE;
    pull_request
        .checks
        .and_then(crate::ui_controls::ChecksSummary::of_counts)
        .map(|summary| crate::ui_controls::checks_glyph(&summary, size))
        .into_iter()
        .chain(
            pull_request
                .review
                .map(|review| crate::ui_controls::pull_request_review_glyph(review, size)),
        )
        .collect()
}

fn glyph<'a, Message: 'a>(icon: Icon, tone: Toned) -> Element<'a, Message> {
    widget::icon(icon.handle())
        .size(design::ROW_ICON_SIZE)
        .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(move |theme| {
            iced::widget::svg::Style {
                color: Some(tone(theme)),
            }
        })))
        .into()
}

/// What the board could not read, for one reason: GitHub's rate limit is
/// the account's, so every project reports the same one at once.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Problem<'a> {
    kind: ProblemKind,
    reason: &'a str,
    /// The projects it affects, in board order; none for the board's own.
    projects: Vec<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProblemKind {
    /// Git could not read the projects.
    Unreadable,
    /// GitHub has not answered for the projects since the daemon started, so
    /// their cards have no pull requests.
    PullRequestsUnknown,
    /// GitHub's last answer stands; reading it again failed.
    PullRequestsStale,
    /// Linear could not be asked about the cards' tickets.
    Tickets,
}

impl Problem<'_> {
    /// What happened, without the reason or the projects.
    fn headline(&self) -> String {
        match (self.kind, self.projects.as_slice()) {
            (ProblemKind::Unreadable, [project]) => format!("{project} could not be read"),
            (ProblemKind::Unreadable, names) => {
                format!("{} could not be read", projects(names.len()))
            }
            (ProblemKind::PullRequestsUnknown, _) => "Pull requests could not be read".to_owned(),
            (ProblemKind::PullRequestsStale, _) => "Pull requests may be out of date".to_owned(),
            (ProblemKind::Tickets, _) => "Linear tickets could not be looked up".to_owned(),
        }
    }

    /// One line: what happened and why, naming the project when only one is
    /// affected.
    fn summary(&self) -> String {
        match (self.kind, self.projects.as_slice()) {
            (ProblemKind::PullRequestsUnknown | ProblemKind::PullRequestsStale, [project]) => {
                let headline = self.headline();
                let (first, rest) = headline.split_at(1);
                format!("{project}: {}{rest}: {}", first.to_lowercase(), self.reason)
            }
            _ => format!("{}: {}", self.headline(), self.reason),
        }
    }
}

/// What the board could not read about the projects in `scope`, one entry
/// per reason, those affecting the most projects first. A forge reason
/// shared by projects GitHub answered before and projects it never has is
/// one problem: the account's, not theirs.
fn problems<'a>(board: &'a Board, scope: Option<&std::path::Path>) -> Vec<Problem<'a>> {
    let mut problems: Vec<Problem<'a>> = Vec::new();
    let mut add = |kind: ProblemKind, reason: &'a str, project: Option<&'a str>| {
        let forge = |kind| {
            matches!(
                kind,
                ProblemKind::PullRequestsUnknown | ProblemKind::PullRequestsStale
            )
        };
        let same = |problem: &&mut Problem<'a>| {
            problem.reason == reason
                && (problem.kind == kind || (forge(problem.kind) && forge(kind)))
        };
        match problems.iter_mut().find(same) {
            Some(problem) => {
                // Out of date somewhere says more than unknown everywhere.
                if kind == ProblemKind::PullRequestsStale {
                    problem.kind = kind;
                }
                problem.projects.extend(project);
            }
            None => problems.push(Problem {
                kind,
                reason,
                projects: project.into_iter().collect(),
            }),
        }
    };
    for project in board
        .projects
        .iter()
        .filter(|project| scope.is_none_or(|scope| project.repository == scope))
    {
        let name = Some(project.name.as_str());
        if let Some(error) = &project.scan_error {
            add(ProblemKind::Unreadable, error, name);
        }
        match &project.forge {
            Forge::Unavailable { reason } => add(ProblemKind::PullRequestsUnknown, reason, name),
            Forge::Synced {
                error: Some(error), ..
            } => add(ProblemKind::PullRequestsStale, error, name),
            Forge::Pending | Forge::NotGitHub | Forge::Synced { error: None, .. } => {}
        }
    }
    if let Some(reason) = &board.linear_unavailable {
        add(ProblemKind::Tickets, reason, None);
    }
    // Stable: equally wide problems keep the board's order.
    problems.sort_by_key(|problem| std::cmp::Reverse(problem.projects.len()));
    problems
}

/// Whether `problems` take more than their summary to tell: more than one,
/// or one shared by several projects.
fn disclosed(problems: &[Problem<'_>]) -> bool {
    match problems {
        [] => false,
        [problem] => problem.projects.len() > 1,
        _ => true,
    }
}

/// How many distinct projects `problems` affect between them.
fn affected_projects(problems: &[Problem<'_>]) -> usize {
    let mut names = problems
        .iter()
        .flat_map(|problem| problem.projects.iter().copied())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    names.len()
}

/// One problem in the details: what happened, its exact reason, and every
/// project it affects.
fn problem_details<'a>(problem: &Problem<'a>) -> Element<'a, Action> {
    let mut lines: Vec<Element<'a, Action>> = vec![
        widget::row::with_children(vec![
            warning_icon(),
            design::body(problem.headline()).into(),
        ])
        .spacing(space::XS)
        .align_y(Alignment::Center)
        .into(),
        design::caption(problem.reason)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
            .into(),
    ];
    // A lone unreadable project is named by its headline already.
    let named = problem.kind == ProblemKind::Unreadable && problem.projects.len() == 1;
    if !problem.projects.is_empty() && !named {
        lines.push(
            design::caption(problem.projects.join(", "))
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .into(),
        );
    }
    widget::column::with_children(lines)
        .spacing(space::XXS)
        .width(Length::Fill)
        .into()
}

fn warning_icon<'a>() -> Element<'a, Action> {
    widget::icon(Icon::Warning.handle())
        .size(design::COMPACT_ICON_SIZE)
        .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(|theme| {
            iced::widget::svg::Style {
                color: Some(design::warning_color(theme)),
            }
        })))
        .into()
}

/// One line of the status, its icon marking it as a problem and its text no
/// louder than `ink`; it fades where the header runs out of room, and its
/// tooltip holds the whole of it.
fn notice<'a>(text: String, ink: fn(&Theme) -> iced::widget::text::Style) -> Element<'a, Action> {
    crate::interaction::tooltip(
        widget::row::with_children(vec![
            warning_icon(),
            crate::ui_controls::FadeText::caption(text.clone())
                .class(cosmic::theme::Text::Custom(ink))
                .into(),
        ])
        .spacing(space::XS)
        .align_y(Alignment::Center),
        text,
    )
}

/// A raised card that lifts on hover and stays lifted while its menu is open.
pub(crate) fn card_style(selected: bool) -> cosmic::theme::Button {
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

fn projects(count: usize) -> String {
    match count {
        1 => "1 project".to_owned(),
        count => format!("{count} projects"),
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

    const RATE_LIMIT: &str = "GitHub rate limit reached. Retrying at 18:42 UTC";

    /// A board of `count` projects, each with a card, that GitHub's rate
    /// limit stopped reading: the first three had never been read.
    fn rate_limited(count: usize) -> Arc<Board> {
        let mut board = board(
            (0..count)
                .map(|index| item("main", &format!("project-{index:02}")))
                .collect(),
        );
        for (index, project) in Arc::make_mut(&mut board).projects.iter_mut().enumerate() {
            project.forge = if index < 3 {
                Forge::Unavailable {
                    reason: RATE_LIMIT.into(),
                }
            } else {
                Forge::Synced {
                    at_unix_ms: 1_000,
                    error: Some(RATE_LIMIT.into()),
                }
            };
        }
        board
    }

    /// What `element` draws as text and reads out, in order.
    fn texts(mut element: Element<'_, Action>) -> Vec<String> {
        use cosmic::iced::advanced::{Layout, layout, widget::Tree};
        struct Texts<'a>(&'a mut Vec<String>);
        impl iced::advanced::widget::Operation for Texts<'_> {
            fn traverse(
                &mut self,
                operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<()>),
            ) {
                operate(self);
            }
            fn text(
                &mut self,
                _id: Option<&iced::advanced::widget::Id>,
                _bounds: iced::Rectangle,
                text: &str,
            ) {
                self.0.push(text.to_owned());
            }
        }
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(1200.0, 40.0)),
        );
        let mut texts = Vec::new();
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut Texts(&mut texts),
        );
        texts
    }

    #[test]
    fn one_rate_limit_reads_once_however_many_projects_it_stops() {
        let mut state = State::default();
        connected(&mut state, 1, rate_limited(16));
        let problems = problems(state.board.as_ref().unwrap(), None);
        assert_eq!(problems.len(), 1, "{problems:?}");
        // Out of date for most, unknown for three: the account's one problem.
        assert_eq!(problems[0].kind, ProblemKind::PullRequestsStale);
        assert_eq!(problems[0].projects.len(), 16);
        assert_eq!(
            problems[0].summary(),
            format!("Pull requests may be out of date: {RATE_LIMIT}")
        );

        let texts = texts(state.status().unwrap());
        assert_eq!(
            texts,
            [
                "16 cards".to_owned(),
                format!("Pull requests may be out of date: {RATE_LIMIT}"),
                "16 projects".to_owned(),
            ],
        );
    }

    #[test]
    fn distinct_reasons_stay_distinct_and_the_widest_leads() {
        let mut board = rate_limited(5);
        let projects = &mut Arc::make_mut(&mut board).projects;
        projects[1].forge = Forge::Unavailable {
            reason: "gh is not signed in to github.example.com".into(),
        };
        projects[2].scan_error = Some("not a git repository".into());
        Arc::make_mut(&mut board).linear_unavailable = Some("no Linear API key".into());
        let problems = problems(&board, None);
        let described = problems
            .iter()
            .map(|problem| (problem.summary(), problem.projects.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            described,
            [
                (
                    format!("Pull requests may be out of date: {RATE_LIMIT}"),
                    vec!["project-00", "project-02", "project-03", "project-04"],
                ),
                (
                    "project-01: pull requests could not be read: \
                     gh is not signed in to github.example.com"
                        .to_owned(),
                    vec!["project-01"],
                ),
                (
                    "project-02 could not be read: not a git repository".to_owned(),
                    vec!["project-02"],
                ),
                (
                    "Linear tickets could not be looked up: no Linear API key".to_owned(),
                    vec![],
                ),
            ],
        );
        assert_eq!(affected_projects(&problems), 5);
        let mut state = State::default();
        connected(&mut state, 1, board);
        assert_eq!(
            texts(state.status().unwrap())[1..],
            [
                format!("Pull requests may be out of date: {RATE_LIMIT}"),
                "4 problems".to_owned(),
            ],
        );
    }

    #[test]
    fn a_lone_problem_names_its_project_and_a_scope_shows_only_its_own() {
        let limited = rate_limited(3);
        let scope = limited.projects[1].repository.clone();
        let mut state = State::default();
        connected(&mut state, 1, limited);
        state.update(Action::Scope(Some(scope)));
        assert_eq!(
            texts(state.status().unwrap()),
            [
                "1 card".to_owned(),
                format!("project-01: pull requests could not be read: {RATE_LIMIT}"),
            ],
        );
        // A problem only Linear has reads whole too.
        let mut board = board(Vec::new());
        Arc::make_mut(&mut board).linear_unavailable = Some("no Linear API key".into());
        assert!(!disclosed(&problems(&board, None)));
    }

    #[test]
    fn the_details_close_when_nothing_is_left_to_detail() {
        let mut state = State::default();
        connected(&mut state, 1, rate_limited(4));
        state.update(Action::Problems(true));
        // Still several projects: the open details follow the board.
        watched(&mut state, 2, rate_limited(3));
        assert!(state.problems_open);
        // Read again: nothing to show, and nothing reopens by itself later.
        watched(&mut state, 3, board(vec![item("main", "project-00")]));
        assert!(!state.problems_open);
        watched(&mut state, 4, rate_limited(4));
        assert!(!state.problems_open);
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

    #[cfg(unix)]
    #[test]
    fn non_utf8_branch_copy_is_disabled_without_affecting_the_literal_label() {
        let mut invalid = item(r"legacy-\xFF", "cagents");
        invalid.key = ItemKey::Branch(":6c65676163792dff".into());
        assert!(copy_branch_action(&invalid).is_none());

        let literal = item("legacy-\u{fffd}", "cagents");
        assert!(matches!(
            copy_branch_action(&literal),
            Some(Action::Copy(name)) if name == "legacy-\u{fffd}"
        ));

        invalid.key = ItemKey::Branch(":6c65676163792dff:second-checkout".into());
        assert!(copy_branch_action(&invalid).is_none());

        let worktree_path = PathBuf::from("/repos/cagents/.worktrees/legacy-raw");
        invalid.worktree = Some(crate::work::Worktree {
            path: worktree_path.clone(),
            primary: false,
            locked: false,
            created_at_unix_ms: None,
        });
        let mut state = State::default();
        watched(&mut state, 1, board(vec![invalid.clone(), literal]));
        let effect = state.update(Action::Pin(CardRef::of(&invalid), Some(Column::Stale)));
        assert!(matches!(
            effect,
            Some(Effect::Pin { item, column: Some(Column::Stale), .. })
                if item == "branch::6c65676163792dff:second-checkout"
        ));
        let effect = state.update(Action::OpenWorktree(CardRef::of(&invalid)));
        assert!(matches!(
            effect,
            Some(Effect::OpenWorktree { project, path })
                if project == invalid.project && path == worktree_path
        ));
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
    fn a_refused_pin_preserves_the_confirmed_pin_and_derived_state() {
        let mut state = State::default();
        let mut pinned = item("feat/board", "cagents");
        pinned.column = Column::Stale;
        pinned.pinned = true;
        watched(&mut state, 1, board(vec![pinned]));
        let confirmed = state.confirmed.clone().unwrap();
        let card = CardRef::of(&confirmed.items[0]);

        state.update(Action::Pin(card.clone(), Some(Column::InReview)));
        let shown = state.item(&card).unwrap();
        assert_eq!(
            (shown.column, shown.pinned, shown.derived),
            (Column::InReview, true, Column::InProgress)
        );
        assert!(Arc::ptr_eq(state.confirmed.as_ref().unwrap(), &confirmed));
        let original = &confirmed.items[0];
        assert_eq!(
            (original.column, original.pinned, original.derived),
            (Column::Stale, true, Column::InProgress)
        );

        state.update(Action::Pinned(Err("Permission denied".into())));
        let shown = state.item(&card).unwrap();
        assert_eq!(
            (shown.column, shown.pinned, shown.derived),
            (Column::Stale, true, Column::InProgress)
        );
        assert!(Arc::ptr_eq(state.confirmed.as_ref().unwrap(), &confirmed));
        assert!(Arc::ptr_eq(state.board.as_ref().unwrap(), &confirmed));
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
    fn an_open_pull_request_says_its_checks_and_review_as_named_glyphs() {
        use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
        let mut pull_request = PullRequest {
            number: 42,
            title: "Fix login redirect".into(),
            url: "https://github.com/o/redact/pull/42".into(),
            state: PullRequestState::Open,
            draft: false,
            head_oid: "a".into(),
            contains_local_head: true,
            checks: Some(crate::work::Checks {
                passed: 10,
                failed: 2,
                pending: 0,
                ..Default::default()
            }),
            review: Some(crate::work::Review::ChangesRequested),
            updated_at: String::new(),
        };
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let described = |pull_request: &PullRequest| {
            let mut element: Element<'_, ()> =
                widget::row::with_children(pull_request_badges(pull_request)).into();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(400.0, 40.0)),
            );
            let mut texts = Vec::new();
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut Texts(&mut texts),
            );
            let nodes = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                mouse::Cursor::Unavailable,
            );
            let names = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .filter_map(|node| node.node().label().map(str::to_owned))
                .collect::<Vec<_>>();
            (names, texts, node.size().height)
        };
        let (names, texts, height) = described(&pull_request);
        assert_eq!(names, ["Checks: 2 failing, 10 passed", "Changes requested"]);
        assert!(texts.is_empty(), "no words are drawn: {texts:?}");
        assert_eq!(height, f32::from(design::COMPACT_ICON_SIZE));
        pull_request.checks = Some(crate::work::Checks {
            cancelled: 1,
            skipped: 2,
            ..Default::default()
        });
        assert_eq!(
            described(&pull_request).0,
            ["Checks: 1 cancelled, 2 skipped", "Changes requested"]
        );
        // A merged pull request's checks and review are history; its state
        // glyph says all a card needs.
        pull_request.state = PullRequestState::Merged;
        assert!(described(&pull_request).0.is_empty());

        struct Texts<'a>(&'a mut Vec<String>);
        impl iced::advanced::widget::Operation for Texts<'_> {
            fn traverse(
                &mut self,
                operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<()>),
            ) {
                operate(self);
            }
            fn text(
                &mut self,
                _id: Option<&iced::advanced::widget::Id>,
                _bounds: iced::Rectangle,
                text: &str,
            ) {
                self.0.push(text.to_owned());
            }
        }
    }

    /// Frames of a Work board at a long-lived installation's scale, 600 cards
    /// and, for headroom, 3,000, every one with an open pull request whose
    /// checks and review are known, in the board and list layouts. A rebuilt
    /// frame makes the view again, as every message does; a scrolled frame
    /// delivers a wheel step and the redraw that follows it, as scrolling
    /// does. Run it optimized:
    ///
    /// ```sh
    /// scripts/cargo-gate test --locked --profile local -p cagents-ui \
    ///     --lib -- --ignored --nocapture work_ui::tests::board_frame_workload
    /// ```
    #[test]
    #[ignore = "timing workload; run optimized with --ignored --nocapture"]
    fn board_frame_workload() {
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use std::time::{Duration, Instant};
        const FRAMES: usize = 48;
        const WARMUP: usize = 8;
        const SCREEN: iced::Size = iced::Size::new(1240.0, 900.0);
        const STEP: f32 = 96.0;

        /// The furthest any scrollable has been scrolled down.
        struct Scrolled(f32);
        impl iced::advanced::widget::Operation for Scrolled {
            fn traverse(
                &mut self,
                operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<()>),
            ) {
                operate(self);
            }
            fn scrollable(
                &mut self,
                _id: Option<&iced::advanced::widget::Id>,
                _bounds: iced::Rectangle,
                _content_bounds: iced::Rectangle,
                translation: iced::Vector,
                _state: &mut dyn iced::advanced::widget::operation::Scrollable,
            ) {
                self.0 = self.0.max(translation.y);
            }
        }

        let reviews = [
            Some(crate::work::Review::Approved),
            Some(crate::work::Review::ChangesRequested),
            Some(crate::work::Review::Required),
            None,
        ];
        let millis = |took: Duration| took.as_secs_f64() * 1e3;
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let theme = cosmic::Theme::dark();
        let style = iced::advanced::renderer::Style::default();
        for cards in [600, 3000] {
            let items = (0..cards)
                .map(|index| {
                    let mut item = item(&format!("feature/change-{index}"), "cagents");
                    item.column = [Column::Active, Column::InProgress, Column::InReview][index % 3];
                    item.pull_request = Some(PullRequest {
                        number: index as u64 + 1,
                        title: format!("Change number {index}"),
                        url: format!("https://github.com/o/cagents/pull/{}", index + 1),
                        state: PullRequestState::Open,
                        draft: false,
                        head_oid: "a".into(),
                        contains_local_head: true,
                        checks: Some(crate::work::Checks {
                            passed: 40 + index as u32 % 7,
                            failed: index as u32 % 3,
                            pending: index as u32 % 2,
                            ..Default::default()
                        }),
                        review: reviews[index % reviews.len()],
                        updated_at: String::new(),
                    });
                    item
                })
                .collect();
            let mut state = State::default();
            connected(&mut state, 1, board(items));
            // Within the first lane, and within the list.
            for (name, list, pointer) in [
                ("board", false, iced::Point::new(120.0, 500.0)),
                ("list", true, iced::Point::new(620.0, 500.0)),
            ] {
                if list {
                    state.update(Action::Layout(Layout::List));
                }
                let mut cache = Cache::new();
                // The view is built lazily inside layout, so building the
                // interface is where the cards' widgets are made.
                let mut stages = [Duration::ZERO; 2];
                for frame in 0..WARMUP + FRAMES {
                    let clock = Instant::now();
                    let mut ui = UserInterface::build(
                        state.view(),
                        SCREEN,
                        std::mem::replace(&mut cache, Cache::new()),
                        &mut renderer,
                    );
                    let built = clock.elapsed();
                    let clock = Instant::now();
                    ui.draw(
                        &mut renderer,
                        &theme,
                        &style,
                        iced::mouse::Cursor::Unavailable,
                    );
                    let drawn = clock.elapsed();
                    cache = ui.into_cache();
                    if frame >= WARMUP {
                        for (stage, took) in stages.iter_mut().zip([built, drawn]) {
                            *stage += took;
                        }
                    }
                }
                let [build, draw] = stages.map(|stage| millis(stage) / FRAMES as f64);

                let view = state.view();
                let mut ui = UserInterface::build(view, SCREEN, cache, &mut renderer);
                let cursor = iced::mouse::Cursor::Available(pointer);
                ui.draw(&mut renderer, &theme, &style, cursor);
                let wheel = iced::Event::Mouse(iced::mouse::Event::WheelScrolled {
                    delta: iced::mouse::ScrollDelta::Pixels { x: 0.0, y: -STEP },
                });
                let (mut total, mut worst) = (Duration::ZERO, Duration::ZERO);
                for frame in 0..WARMUP + FRAMES {
                    let redraw = iced::Event::Window(iced::window::Event::RedrawRequested(
                        iced::time::Instant::now(),
                    ));
                    let clock = Instant::now();
                    let mut messages = Vec::new();
                    let _ = ui.update(
                        &[wheel.clone(), redraw],
                        cursor,
                        &mut renderer,
                        &mut iced::advanced::clipboard::Null,
                        &mut messages,
                    );
                    ui.draw(&mut renderer, &theme, &style, cursor);
                    let took = clock.elapsed();
                    if frame >= WARMUP {
                        total += took;
                        worst = worst.max(took);
                    }
                }
                let mut scrolled = Scrolled(0.0);
                ui.operate(&renderer, &mut scrolled);
                assert!(
                    scrolled.0 >= STEP * (WARMUP + FRAMES) as f32 * 0.9,
                    "the {name} scrolled only {} px",
                    scrolled.0
                );
                // The first wheel of a gesture, which on a board that scrolls
                // sideways decides where the gesture goes: a pointer motion
                // this long after the last wheel ends the gesture before.
                const GESTURES: usize = 8;
                let mut started = Duration::ZERO;
                for _ in 0..GESTURES {
                    std::thread::sleep(Duration::from_millis(120));
                    let moved =
                        iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: pointer });
                    let _ = ui.update(
                        &[moved],
                        cursor,
                        &mut renderer,
                        &mut iced::advanced::clipboard::Null,
                        &mut Vec::new(),
                    );
                    let clock = Instant::now();
                    let _ = ui.update(
                        std::slice::from_ref(&wheel),
                        cursor,
                        &mut renderer,
                        &mut iced::advanced::clipboard::Null,
                        &mut Vec::new(),
                    );
                    started += clock.elapsed();
                }
                eprintln!(
                    "work {name}, {cards} cards: rebuilt frame {build:.3} ms view, build and \
                     layout + {draw:.3} ms draw; scrolled frame {:.3} ms mean, {:.3} ms worst; \
                     a gesture's first wheel {:.3} ms",
                    millis(total) / FRAMES as f64,
                    millis(worst),
                    millis(started) / GESTURES as f64,
                );
            }
        }
    }

    #[test]
    fn the_wheel_scrolls_the_lane_under_the_pointer_when_the_board_scrolls_sideways() {
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        struct Offsets(Vec<iced::Vector>);
        impl iced::advanced::widget::Operation for Offsets {
            fn traverse(
                &mut self,
                operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<()>),
            ) {
                operate(self);
            }
            fn scrollable(
                &mut self,
                _id: Option<&iced::advanced::widget::Id>,
                _bounds: iced::Rectangle,
                _content_bounds: iced::Rectangle,
                translation: iced::Vector,
                _state: &mut dyn iced::advanced::widget::operation::Scrollable,
            ) {
                self.0.push(translation);
            }
        }
        let items = (0..30)
            .map(|index| item(&format!("feature/change-{index}"), "cagents"))
            .collect();
        let mut state = State::default();
        connected(&mut state, 1, board(items));
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        // Too narrow for every lane, so the board scrolls sideways.
        let mut ui = UserInterface::build(
            state.view(),
            iced::Size::new(1000.0, 700.0),
            Cache::new(),
            &mut renderer,
        );
        // The runtime redraws before any input arrives; a scrollable's first
        // redraw records its viewport.
        let _ = ui.update(
            &[iced::Event::Window(iced::window::Event::RedrawRequested(
                iced::time::Instant::now(),
            ))],
            iced::mouse::Cursor::Unavailable,
            &mut renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Vec::new(),
        );
        // Within the second lane, In progress, where the cards are.
        let position = iced::Point::new(440.0, 400.0);
        let _ = ui.update(
            &[
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }),
                iced::Event::Mouse(iced::mouse::Event::WheelScrolled {
                    delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: -2.0 },
                }),
            ],
            iced::mouse::Cursor::Available(position),
            &mut renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Vec::new(),
        );
        let mut offsets = Offsets(Vec::new());
        ui.operate(&renderer, &mut offsets);
        // The board's own scrollable stays put, and the lane scrolls.
        assert_eq!(
            offsets.0,
            [iced::Vector::new(0.0, 0.0), iced::Vector::new(0.0, 120.0)]
        );
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
}
