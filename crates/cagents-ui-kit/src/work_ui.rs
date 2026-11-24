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
    design::{self, space},
    model::TerminalId,
    ui_controls::{Icon, icon_button, placeholder, popover, popover_item},
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

    /// A board card: what the work is called, the names that place it, then
    /// marks for what may need the user, its agents and how long ago it last
    /// moved. It says what a list row says, stacked to the lane's width.
    fn card<'a>(&'a self, item: &'a Item) -> Element<'a, Action> {
        let card = CardRef::of(item);
        let mut top = vec![
            status_glyph(item),
            design::heading(title(item))
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .ellipsize(iced::widget::text::Ellipsize::End(
                    iced::advanced::text::EllipsizeHeightLimit::Lines(2),
                ))
                .width(Length::Fill)
                .into(),
        ];
        if item.pinned {
            top.push(pinned_mark(item));
        }
        top.push(self.card_menu(item, &card));
        let mut lines = vec![
            widget::row::with_children(top)
                .spacing(space::XS)
                .align_y(Alignment::Start)
                .into(),
        ];
        let names = self.names(item);
        lines.push(
            widget::row::with_children(vec![
                with_tip(
                    crate::ui_controls::FadeText::caption(names.join(" · "))
                        .width(Length::Fill)
                        .into(),
                    details(item),
                ),
                self.age(item),
            ])
            .spacing(space::XS)
            .align_y(Alignment::Center)
            .into(),
        );
        let marks = self.marks(item);
        if !marks.is_empty() || !item.sessions.is_empty() {
            let mut parts = marks;
            parts.push(widget::Space::new().width(Length::Fill).into());
            parts.extend(self.agents(item));
            lines.push(
                widget::row::with_children(parts)
                    .spacing(space::XS)
                    .align_y(Alignment::Center)
                    .into(),
            );
        }
        let content = widget::column::with_children(lines).spacing(space::XS);
        let selected = self.menu.as_ref() == Some(&card);
        let button = widget::button::custom(content)
            .name(open_name(item))
            .class(card_style(selected))
            .padding(space::S)
            .width(Length::Fill)
            .on_press(Action::Open(card.clone()));
        widget::mouse_area(crate::interaction::hover(button))
            .on_right_press(Action::Menu(Some(card)))
            .into()
    }

    /// A list row: its pull request's title, or its branch when it has
    /// none, with marks for what may need the user, then the number, branch
    /// and project that name it. Its folder and how far it has gone from
    /// its base are on hover; its column is said only where its glyph and
    /// age leave it unsaid.
    fn row<'a>(&'a self, item: &'a Item) -> Element<'a, Action> {
        let card = CardRef::of(item);
        let mut title = vec![
            status_glyph(item),
            crate::ui_controls::FadeText::heading(title(item))
                .width(Length::Fill)
                .into(),
        ];
        title.extend(self.marks(item));
        if item.pinned {
            title.push(pinned_mark(item));
        } else if let Some(column) = quiet_column(item.column) {
            title.push(
                design::caption(column)
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
            );
        }
        title.push(self.age(item));
        title.push(self.card_menu(item, &card));
        let names = self.names(item);
        let mut lines = vec![
            widget::row::with_children(title)
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
        ];
        // The second line ends before the age and menu column, so a long
        // branch never runs under them.
        if !names.is_empty() {
            lines.push(
                widget::container(with_tip(
                    crate::ui_controls::FadeText::caption(names.join(" · "))
                        .width(Length::Fill)
                        .into(),
                    details(item),
                ))
                .padding(Padding {
                    left: f32::from(design::ROW_ICON_SIZE) + space::S,
                    ..Padding::ZERO
                })
                .into(),
            );
        }
        let selected = self.menu.as_ref() == Some(&card);
        let button =
            widget::button::custom(widget::column::with_children(lines).spacing(space::XXS))
                .name(open_name(item))
                .class(card_style(selected))
                .padding([space::XS + space::XXS, space::S])
                .width(Length::Fill)
                .on_press(Action::Open(card.clone()));
        widget::mouse_area(crate::interaction::hover(button))
            .on_right_press(Action::Menu(Some(card)))
            .into()
    }

    /// How long ago the card's last activity was, in a column of its own so
    /// the rows' ages line up, with what it was and when on hover.
    fn age<'a>(&'a self, item: &'a Item) -> Element<'a, Action> {
        let age = widget::container(
            design::caption(age(self.now_unix_ms, activity_at(item), false))
                .class(cosmic::theme::Text::Custom(design::muted_text)),
        )
        .width(44)
        .align_x(iced::alignment::Horizontal::Right);
        match &item.last_activity {
            Some(activity) => crate::interaction::tooltip(
                age,
                format!(
                    "{} {}, {}",
                    activity_label(activity.source),
                    crate::work_ui::age(self.now_unix_ms, activity.at_unix_ms, true),
                    local_time(activity.at_unix_ms)
                ),
            ),
            None => age.into(),
        }
    }

    /// The quiet line beneath a card's title, in words: its pull request's
    /// number and branch when the title is the pull request's, then its
    /// project and Linear ticket. `#212 · feat/search · atlas · ENG-12`.
    fn names(&self, item: &Item) -> Vec<String> {
        let mut names = Vec::with_capacity(4);
        if let Some(pull_request) = &item.pull_request {
            names.push(format!("#{}", pull_request.number));
            names.extend(item.branch.clone());
        }
        names.extend(self.context(item));
        names
    }

    /// What places a card among the others in words: its project while every
    /// project is shown, and its Linear ticket.
    fn context(&self, item: &Item) -> Vec<String> {
        let mut context = Vec::with_capacity(2);
        if self.scope.is_none() {
            context.push(item.project_name.clone());
        }
        if let Some(identifier) = item
            .linear
            .as_ref()
            .and_then(|ticket| ticket.identifier.clone())
        {
            context.push(identifier);
        }
        context
    }

    /// Glyphs for what is true of a card now and may need the user, each
    /// saying itself on hover: its pull request's checks and review, and its
    /// worktree's unpushed commits, uncommitted changes, conflicts and
    /// unreadable status. A card with none of them has no marks.
    fn marks<'a>(&'a self, item: &'a Item) -> Vec<Element<'a, Action>> {
        let size = design::COMPACT_ICON_SIZE;
        let mut marks = Vec::new();
        if let Some(pull_request) = &item.pull_request {
            // The leading glyph gives the state unless an agent's activity
            // has its place; then a small glyph among the marks says it.
            if !status_glyph_is_pull_request(item) {
                let state = crate::sidebar::WorktreePullRequestState::from_work(pull_request);
                marks.push(crate::interaction::tooltip(
                    crate::ui_controls::pull_request_state_glyph(Some(state), size),
                    format!("{} pull request", state.label()),
                ));
            }
            marks.extend(pull_request_badges(pull_request));
            if pull_request.state == PullRequestState::Open
                && !pull_request.contains_local_head
                && item.worktree.is_some()
            {
                marks.push(crate::ui_controls::unpushed_glyph(size));
            }
        }
        if let Some(dirty) = &item.dirty {
            if dirty.conflicted > 0 {
                marks.push(crate::interaction::tooltip(
                    sized_glyph(Icon::Warning, design::destructive_color, size),
                    match dirty.conflicted {
                        1 => "1 conflicted file".to_owned(),
                        count => format!("{count} conflicted files"),
                    },
                ));
            }
            if dirty.paths > 0 {
                marks.push(crate::ui_controls::uncommitted_glyph(
                    crate::ui_controls::uncommitted_label(u64::from(dirty.paths), ""),
                    size,
                ));
            }
        }
        if let Some(error) = &item.error {
            marks.push(crate::interaction::tooltip(
                sized_glyph(Icon::Warning, design::destructive_color, size),
                format!("Its status could not be read: {error}"),
            ));
        }
        marks
    }

    /// The card's agents and terminals as the sidebar draws a channel's: the
    /// mark of the one most worth seeing, with how many there are. It lists
    /// them on hover and focuses the one most in need of the user when
    /// pressed; the card's menu focuses any of them.
    fn agents<'a>(&'a self, item: &'a Item) -> Option<Element<'a, Action>> {
        let urgent = urgent_sessions(item);
        let first = urgent.first()?;
        let mut members = crate::sidebar::ChannelMembers::default();
        for session in &item.sessions {
            match session.provider {
                Some(provider) => members.add_agent(
                    provider,
                    crate::sidebar::ChannelMemberState::of(session.state, false),
                ),
                None => members.add_terminal(),
            }
        }
        let mark = crate::sidebar::members_mark(
            &members,
            crate::sidebar::MembersGround::Row,
            crate::sidebar::RowInk::Full,
        )?;
        let tip = urgent
            .iter()
            .map(|session| {
                format!(
                    "{}: {}",
                    session_label(session),
                    session_state(session.state)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some(crate::interaction::tooltip(
            widget::button::custom(mark)
                .name(format!(
                    "Focus {}, {}; {}",
                    session_label(first),
                    session_state(first.state),
                    members.description()
                ))
                .class(crate::ui_controls::control_style(false, None))
                .padding([space::XXS, space::XS])
                .on_press(Action::FocusSession(first.id)),
            tip,
        ))
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
            for session in urgent_sessions(item) {
                let icon = session.provider.map_or_else(
                    || Icon::Terminal.handle(),
                    crate::ui_controls::provider_icon,
                );
                items.push(
                    popover_item(
                        icon,
                        session_label(session),
                        session_state(session.state),
                        Action::FocusSession(session.id),
                    )
                    .name(format!("Focus {}", session_label(session)))
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
    match &item.branch {
        Some(branch) => fields.push(branch.clone()),
        None => fields.push("detached".to_owned()),
    }
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
    /// An agent waiting on the user outranks one at work, as everywhere an
    /// agent's state is summarised.
    fn of(item: &'a Item) -> Self {
        let most_urgent = item
            .sessions
            .iter()
            .filter_map(|session| crate::sidebar::AttentionState::of_activity(session.state))
            .max();
        if let Some(state) = most_urgent.filter(|state| state.waits_on_user()) {
            Self::Waiting(state)
        } else if most_urgent == Some(crate::sidebar::AttentionState::Working) {
            Self::Working
        } else if let Some(pull_request) = &item.pull_request {
            Self::PullRequest(pull_request)
        } else {
            Self::Branch
        }
    }
}

/// What a card is called: its pull request's title, else its branch, else
/// the folder of a checkout whose HEAD is detached, which says more about
/// what it is for than that it is detached; the glyph's tooltip and the
/// card's accessible name say that.
fn title(item: &Item) -> &str {
    if let Some(pull_request) = &item.pull_request {
        return &pull_request.title;
    }
    if let Some(branch) = &item.branch {
        return branch;
    }
    item.worktree
        .as_ref()
        .and_then(|worktree| worktree.path.file_name())
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("Detached HEAD")
}

/// A card's accessible name: what pressing it opens, and its state and
/// column, which no word on it draws.
fn open_name(item: &Item) -> String {
    format!(
        "Open {}, {}",
        title(item),
        status_words(item).replace(" · ", ", ")
    )
}

/// A card's sessions, the one most in need of the user first: waiting for
/// permission, an answer or input, then working, then the rest, each kind in
/// the board's order.
fn urgent_sessions(item: &Item) -> Vec<&crate::work::Session> {
    let mut sessions = item.sessions.iter().collect::<Vec<_>>();
    sessions.sort_by_key(|session| {
        std::cmp::Reverse(crate::sidebar::AttentionState::of_activity(session.state))
    });
    sessions
}

/// What names a session: its title, else its provider.
fn session_label(session: &crate::work::Session) -> &str {
    session.title.as_deref().unwrap_or_else(|| {
        session
            .provider
            .map_or("Terminal", |provider| provider.label())
    })
}

fn session_state(state: ActivityState) -> &'static str {
    match state {
        ActivityState::Idle | ActivityState::Unknown => "Idle",
        ActivityState::Waiting => "Waiting for your input",
        ActivityState::Approval => "Waiting for your permission",
        ActivityState::Question => "Waiting for your answer",
        ActivityState::Working => "Working",
    }
}

fn status_glyph<'a>(item: &Item) -> Element<'a, Action> {
    crate::interaction::tooltip(leading_glyph(item), status_words(item))
}

/// What a card's leading glyph says, and the column it is in: `Draft pull
/// request · In review`. Its tooltip, and part of its accessible name.
fn status_words(item: &Item) -> String {
    use crate::sidebar::AttentionState;
    let glyph = match Leading::of(item) {
        Leading::Working => "An agent is working".to_owned(),
        Leading::Waiting(AttentionState::Approval) => "An agent needs your permission".to_owned(),
        Leading::Waiting(AttentionState::Question) => "An agent asked you a question".to_owned(),
        Leading::Waiting(_) => "An agent is waiting for you".to_owned(),
        Leading::PullRequest(pull_request) => format!(
            "{} pull request",
            crate::sidebar::WorktreePullRequestState::from_work(pull_request).label()
        ),
        Leading::Branch if item.branch.is_none() => "Detached worktree".to_owned(),
        Leading::Branch if item.worktree.is_some() => "Worktree".to_owned(),
        Leading::Branch => "Branch".to_owned(),
    };
    format!("{glyph} · {}", item.column.label())
}

fn leading_glyph<'a>(item: &Item) -> Element<'a, Action> {
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

/// A pinned card's lock, saying on hover where it is pinned and where its
/// state alone would put it.
fn pinned_mark<'a>(item: &Item) -> Element<'a, Action> {
    crate::interaction::tooltip(
        widget::icon(Icon::Lock.handle()).size(design::COMPACT_ICON_SIZE),
        format!(
            "Pinned to {}; its state says {}",
            item.column.label(),
            item.derived.label()
        ),
    )
}

/// The column a list row names in words. Landed asks to be cleaned up and
/// Stale to be looked at again, so they are said, quietly; the others are
/// the usual states of work under way, which the leading glyph's tooltip
/// and the row's accessible name say.
fn quiet_column(column: Column) -> Option<&'static str> {
    matches!(column, Column::Landed | Column::Stale).then(|| column.label())
}

/// What a card's naming line says on hover, one fact a line: its worktree's
/// folder, its Linear ticket's title, and how far it has gone from its base.
fn details(item: &Item) -> Option<String> {
    let mut lines = Vec::with_capacity(4);
    if item.branch.is_none()
        && let Some(head) = &item.head
    {
        lines.push(format!(
            "Detached HEAD at {}",
            head.oid.get(..7).unwrap_or(&head.oid)
        ));
    }
    if let Some(worktree) = &item.worktree {
        lines.push(crate::ui_controls::display_path(&worktree.path).into_owned());
    }
    if let Some(ticket) = &item.linear
        && let Some(title) = &ticket.title
    {
        lines.push(match &ticket.identifier {
            Some(identifier) => format!("{identifier}: {title}"),
            None => title.clone(),
        });
    }
    if let Some(comparison) = &item.comparison
        && (comparison.ahead > 0 || comparison.behind > 0)
    {
        lines.push(format!(
            "{} ahead of and {} behind {}",
            commits(comparison.ahead),
            commits(comparison.behind),
            comparison.base
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// `content` with `tip` on hover, when there is one.
fn with_tip<'a>(content: Element<'a, Action>, tip: Option<String>) -> Element<'a, Action> {
    match tip {
        Some(tip) => crate::interaction::tooltip(content, tip),
        None => content,
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
    sized_glyph(icon, tone, design::ROW_ICON_SIZE)
}

fn sized_glyph<'a, Message: 'a>(icon: Icon, tone: Toned, size: u16) -> Element<'a, Message> {
    widget::icon(icon.handle())
        .size(size)
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

    /// The words a list row draws and the names its controls and marks
    /// publish, in tree order.
    fn row_words(state: &State, item: &Item) -> (Vec<String>, Vec<String>) {
        words(state.row(item).map(|_| ()), 900.0)
    }

    /// The same of a board card, at a lane's width.
    fn card_words(state: &State, item: &Item) -> (Vec<String>, Vec<String>) {
        words(state.card(item).map(|_| ()), COLUMN_WIDTH)
    }

    fn words(mut element: Element<'_, ()>, width: f32) -> (Vec<String>, Vec<String>) {
        use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
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
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 400.0)),
        );
        let mut texts = Vec::new();
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut Texts(&mut texts),
        );
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let names = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect();
        (texts, names)
    }

    /// A list row leads with its pull request and names its branch and
    /// project beneath it in plain words. What needs the user is a glyph
    /// that says itself; the folder, the counts against the base and a
    /// column its glyph already implies are not drawn.
    #[test]
    fn a_list_row_leads_with_its_pull_request_and_marks_what_needs_the_user() {
        let mut state = State::default();
        let mut card = item("feat/android", "cagents");
        card.worktree = Some(crate::work::Worktree {
            path: "/repos/cagents/.worktrees/android".into(),
            primary: false,
            locked: false,
            created_at_unix_ms: None,
        });
        card.comparison = Some(Comparison {
            base: "origin/main".into(),
            ahead: 6,
            behind: 22,
        });
        card.dirty = Some(Dirty {
            paths: 3,
            conflicted: 0,
            newest_modified_at_unix_ms: None,
        });
        card.pull_request = Some(PullRequest {
            number: 1089,
            title: "Add the Android app".into(),
            url: "https://github.com/o/cagents/pull/1089".into(),
            state: PullRequestState::Open,
            draft: false,
            head_oid: "a".into(),
            contains_local_head: false,
            checks: None,
            review: None,
            updated_at: String::new(),
        });
        card.column = Column::InReview;
        card.derived = Column::InReview;
        let mut landed = item("fix/login", "cagents");
        landed.column = Column::Landed;
        landed.derived = Column::Landed;
        connected(&mut state, 1, board(vec![card.clone(), landed.clone()]));

        let (texts, names) = row_words(&state, &card);
        assert_eq!(
            texts,
            ["Add the Android app", "—", "#1089 · feat/android · cagents"],
            "{names:?}"
        );
        assert!(
            names.iter().any(|name| name == "3 uncommitted changes"),
            "{names:?}"
        );
        assert!(
            names
                .iter()
                .any(|name| name == crate::ui_controls::UNPUSHED_LABEL),
            "{names:?}"
        );
        // Its state and column, which no word on it draws, are in its name.
        assert!(
            names
                .iter()
                .any(|name| name == "Open Add the Android app, Open pull request, In review"),
            "{names:?}"
        );
        // A branch without a pull request is named by the branch, and a
        // column nothing else implies is said.
        let (texts, _) = row_words(&state, &landed);
        assert_eq!(texts, ["fix/login", "Landed", "—", "cagents"]);
        // In one project's scope, the project goes unsaid.
        state.update(Action::Scope(Some(landed.project.clone())));
        let (texts, _) = row_words(&state, &landed);
        assert_eq!(texts, ["fix/login", "Landed", "—"]);
    }

    fn terminal(id: u128) -> TerminalId {
        TerminalId::from_uuid(uuid::Uuid::from_u128(id))
    }

    fn session(
        id: u128,
        provider: Option<crate::agent::Provider>,
        state: ActivityState,
    ) -> crate::work::Session {
        crate::work::Session {
            id: terminal(id),
            provider,
            title: None,
            state,
        }
    }

    /// A checkout whose HEAD is detached is called by its folder, which says
    /// what it is for; that it is detached is said where its state is, on
    /// its glyph and in its name, and its search finds it by either.
    #[test]
    fn a_detached_checkout_is_called_by_its_folder() {
        let mut state = State::default();
        let path = PathBuf::from("/repos/atlas/.worktrees/pr-207-review");
        let mut card = item("unused", "atlas");
        card.key = ItemKey::Worktree(path.clone());
        card.branch = None;
        card.head = Some(crate::work::Head {
            oid: "4af0965aa1".into(),
            committed_at_unix_ms: None,
        });
        card.worktree = Some(crate::work::Worktree {
            path,
            primary: false,
            locked: false,
            created_at_unix_ms: None,
        });
        connected(&mut state, 1, board(vec![card.clone()]));

        let (texts, names) = card_words(&state, &card);
        assert_eq!(texts[0], "pr-207-review", "{texts:?}");
        assert!(
            !texts.iter().any(|text| text.contains("Detached")),
            "{texts:?}"
        );
        assert!(
            names
                .iter()
                .any(|name| name == "Open pr-207-review, Detached worktree, In progress"),
            "{names:?}"
        );
        let (texts, _) = row_words(&state, &card);
        assert_eq!(texts[0], "pr-207-review", "{texts:?}");
        assert!(
            details(&card)
                .unwrap()
                .starts_with("Detached HEAD at 4af0965\n")
        );
        state.update(Action::Query("detached".into()));
        assert_eq!(state.visible().count(), 1);
        state.update(Action::Query("207".into()));
        assert_eq!(state.visible().count(), 1);
    }

    /// A card leads with its pull request's title, as its list row does, and
    /// names its number, branch and project quietly beneath it, with its age.
    #[test]
    fn a_card_leads_with_its_pull_request_like_its_list_row() {
        let mut state = State::default();
        let mut card = item("feat/search", "atlas");
        card.pull_request = Some(PullRequest {
            number: 212,
            title: "Rank search results by recency".into(),
            url: "https://github.com/o/atlas/pull/212".into(),
            state: PullRequestState::Open,
            draft: true,
            head_oid: "a".into(),
            contains_local_head: true,
            checks: None,
            review: None,
            updated_at: String::new(),
        });
        connected(&mut state, 1, board(vec![card.clone()]));
        let (texts, _) = card_words(&state, &card);
        assert_eq!(
            texts,
            [
                "Rank search results by recency",
                "#212 · feat/search · atlas",
                "—"
            ]
        );
        let (row, _) = row_words(&state, &card);
        assert_eq!(row[0], texts[0]);
    }

    /// Several agents are one mark with their count, as a channel's are in
    /// the sidebar. Pressing it focuses the one most in need of the user,
    /// whose state the card's glyph says, and the menu focuses any of them.
    #[test]
    fn a_cards_agents_are_one_counted_mark_that_focuses_the_most_urgent() {
        use crate::agent::Provider;
        let mut state = State::default();
        let mut card = item("feat/search", "atlas");
        card.column = Column::Active;
        card.sessions = vec![
            session(1, Some(Provider::Claude), ActivityState::Working),
            session(2, Some(Provider::Claude), ActivityState::Idle),
            session(3, Some(Provider::Claude), ActivityState::Question),
            session(4, Some(Provider::Claude), ActivityState::Approval),
        ];
        card.sessions[3].title = Some("Run the tests".into());
        connected(&mut state, 1, board(vec![card.clone()]));

        assert_eq!(
            urgent_sessions(&card)
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            [terminal(4), terminal(3), terminal(1), terminal(2)]
        );
        assert!(status_words(&card).starts_with("An agent needs your permission"));
        let (texts, names) = card_words(&state, &card);
        // One mark and its count, not a glyph per agent.
        assert_eq!(
            texts.iter().filter(|text| *text == "4").count(),
            1,
            "{texts:?}"
        );
        let focus = names
            .iter()
            .filter(|name| name.starts_with("Focus "))
            .collect::<Vec<_>>();
        assert_eq!(focus.len(), 1, "{names:?}");
        assert!(
            focus[0].starts_with("Focus Run the tests, Waiting for your permission; "),
            "{focus:?}"
        );
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
