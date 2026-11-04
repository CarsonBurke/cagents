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
        let layout = self.layout;
        *self = Self {
            scope,
            layout,
            ..Self::default()
        };
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
                Layout::List => self.list_view(),
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
                    Icon::Branch.handle(),
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
}
