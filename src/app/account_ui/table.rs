//! The Accounts pane: every account as one row of a table grouped by
//! provider, with what each has left in every window, and the controls to
//! add, choose, rename and remove accounts.
//!
//! The table follows the card's width rather than the window's: a column
//! the card has no room for is dropped, least useful first, instead of
//! squeezing every column into illegibility.

use std::cmp::Ordering;

use super::*;

/// How the rows of each provider are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    /// The account with the most left in its headline window first: the one
    /// to start the next conversation on.
    #[default]
    Headroom,
    /// The account whose headline window resets soonest first.
    Reset,
    Name,
}

impl Sort {
    const ALL: [Self; 3] = [Self::Headroom, Self::Reset, Self::Name];

    fn label(self) -> &'static str {
        match self {
            Self::Headroom => "Headroom",
            Self::Reset => "Reset",
            Self::Name => "Name",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Headroom => "Most left first",
            Self::Reset => "Soonest reset first",
            Self::Name => "By name",
        }
    }
}

/// The pane's own interface state.
#[derive(Debug, Default)]
pub(in crate::app) struct Table {
    pub(in crate::app) sort: Sort,
    /// The row whose menu is open.
    pub(in crate::app) menu: Option<AccountId>,
    /// The row whose menu is asking to confirm a removal.
    pub(in crate::app) confirm_remove: Option<AccountId>,
    /// How many open cards run on the account being removed.
    pub(in crate::app) confirm_in_use: usize,
    pub(in crate::app) add_menu: bool,
    pub(in crate::app) own_codex_config: bool,
    /// The row being renamed, and the name typed so far.
    pub(in crate::app) rename: Option<(AccountId, String)>,
    /// A provider home to link, and the path entered so far.
    pub(in crate::app) link: Option<(Provider, String)>,
    pub(in crate::app) linking: bool,
}

impl Table {
    pub(in crate::app) fn close_popups(&mut self) {
        self.menu = None;
        self.confirm_remove = None;
        self.add_menu = false;
    }

    pub(in crate::app) fn has_popup(&self) -> bool {
        self.menu.is_some() || self.add_menu || self.rename.is_some() || self.link.is_some()
    }

    /// Closes the innermost thing open, as Escape does: a popup, and
    /// otherwise a rename in progress.
    pub(in crate::app) fn escape(&mut self) {
        if self.menu.is_some() || self.add_menu {
            self.close_popups();
        } else if !self.linking {
            self.rename = None;
            self.link = None;
        }
    }

    /// Lets go of rows whose account is gone.
    pub(in crate::app) fn forget_missing(&mut self, exists: impl Fn(&AccountId) -> bool) {
        if self.menu.as_ref().is_some_and(|id| !exists(id)) {
            self.menu = None;
        }
        if self.confirm_remove.as_ref().is_some_and(|id| !exists(id)) {
            self.confirm_remove = None;
        }
        if self.rename.as_ref().is_some_and(|(id, _)| !exists(id)) {
            self.rename = None;
        }
    }
}

pub(in crate::app) fn rename_id() -> widget::Id {
    widget::Id::new("accounts.rename")
}

pub(in crate::app) fn link_id() -> widget::Id {
    widget::Id::new("accounts.link-home")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Plan,
    Session,
    Weekly,
    Model,
    Credits,
    Status,
    Updated,
}

const GUTTER: f32 = space::L;
const COLUMN_SPACING: f32 = space::M;
const ACCOUNT_MIN_WIDTH: f32 = 180.0;
const ACTIONS_WIDTH: f32 = design::COMPACT_CONTROL_SIZE * 2.0 + design::GRID_UNIT;
const USAGE_WIDTH: f32 = 128.0;
const NARROW_USAGE_WIDTH: f32 = 96.0;
/// The widest the table grows: past this a row is a long way to read
/// across, and there is nothing more to show.
const MAX_WIDTH: f32 = 1_200.0;

impl Column {
    /// Every optional column, in the order the table drops them as it
    /// narrows: when an account was checked matters least, and what it has
    /// left this session and this week matters most.
    const DROP_ORDER: [Self; 5] = [
        Self::Updated,
        Self::Credits,
        Self::Model,
        Self::Plan,
        Self::Status,
    ];
    /// Left to right.
    const ORDER: [Self; 7] = [
        Self::Plan,
        Self::Session,
        Self::Weekly,
        Self::Model,
        Self::Credits,
        Self::Status,
        Self::Updated,
    ];

    fn width(self, usage: f32) -> f32 {
        match self {
            Self::Plan => 72.0,
            Self::Session | Self::Weekly | Self::Model => usage,
            Self::Credits => 112.0,
            Self::Status => 88.0,
            Self::Updated => 64.0,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::Session => "Session",
            Self::Weekly => "Weekly",
            Self::Model => "Model weekly",
            Self::Credits => "Extra usage",
            Self::Status => "Status",
            Self::Updated => "Updated",
        }
    }
}

/// Which columns a table `width` wide shows, and how wide a usage cell is.
#[derive(Debug, Clone, PartialEq)]
struct Layout {
    columns: Vec<Column>,
    usage: f32,
    /// Too narrow for a row of columns: each account's figures go on a
    /// line of their own under its name, each titled.
    stacked: bool,
    /// Too narrow for the pane's description beside its controls.
    compact_toolbar: bool,
}

/// Below this the toolbar's description goes above its controls.
const TOOLBAR_MIN_WIDTH: f32 = 600.0;

fn layout(width: f32) -> Layout {
    let available = width.min(MAX_WIDTH) - 2.0 * GUTTER;
    let needed = |columns: &[Column], usage: f32| {
        ACCOUNT_MIN_WIDTH
            + ACTIONS_WIDTH
            + columns
                .iter()
                .map(|column| column.width(usage) + COLUMN_SPACING)
                .sum::<f32>()
            + COLUMN_SPACING
    };
    let compact_toolbar = available < TOOLBAR_MIN_WIDTH;
    let mut columns = Column::ORDER.to_vec();
    for drop in Column::DROP_ORDER {
        if needed(&columns, USAGE_WIDTH) <= available {
            return Layout {
                columns,
                usage: USAGE_WIDTH,
                stacked: false,
                compact_toolbar,
            };
        }
        columns.retain(|column| *column != drop);
    }
    let (usage, stacked) = if needed(&columns, USAGE_WIDTH) <= available {
        (USAGE_WIDTH, false)
    } else if needed(&columns, NARROW_USAGE_WIDTH) <= available {
        (NARROW_USAGE_WIDTH, false)
    } else {
        (NARROW_USAGE_WIDTH, true)
    };
    Layout {
        columns,
        usage,
        stacked,
        compact_toolbar,
    }
}

/// The tightest of an account's windows of `kind`.
fn window_of(snapshot: &UsageSnapshot, kind: account::WindowKind) -> Option<&UsageWindow> {
    account::tightest(snapshot.windows.iter().filter(|window| window.kind == kind))
}

/// How one account compares with another under `sort`, ties broken by
/// name so the order never flickers between two equal rows.
fn order(state: &State, sort: Sort, left: &AccountState, right: &AccountState) -> Ordering {
    let name = |account: &AccountState| state.display_name(account).to_lowercase();
    let by_name = || name(left).cmp(&name(right));
    // Rows with no figures go last in either figure order.
    let last = |left: Option<f64>, right: Option<f64>, descending: bool| match (left, right) {
        (Some(left), Some(right)) => {
            let ordering = left.partial_cmp(&right).unwrap_or(Ordering::Equal);
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    match sort {
        Sort::Headroom => last(
            left.headline().map(|window| f64::from(window.left())),
            right.headline().map(|window| f64::from(window.left())),
            true,
        )
        .then_with(by_name),
        Sort::Reset => last(
            left.headline()
                .and_then(|window| window.resets_at)
                .map(|reset| reset as f64),
            right
                .headline()
                .and_then(|window| window.resets_at)
                .map(|reset| reset as f64),
            false,
        )
        .then_with(by_name),
        Sort::Name => by_name(),
    }
}

/// The accounts the pane lists for `provider`, in `sort` order: every
/// account cagents added, and the system account where its CLI is set up.
fn rows<'a>(state: &'a State, provider: Provider, sort: Sort) -> Vec<&'a AccountState> {
    let mut rows: Vec<&AccountState> = state
        .of(provider)
        .filter(|account| account.present || !account.account.is_system())
        .collect();
    rows.sort_by(|left, right| order(state, sort, left, right));
    rows
}

pub(super) fn view(app: &Cagents) -> Element<'_, Message> {
    crate::ui_controls::responsive(move |size| table(app, size.width)).into()
}

fn table(app: &Cagents, width: f32) -> Element<'_, Message> {
    let state = &app.account_ui;
    let layout = layout(width);
    let mut sections: Vec<Element<'_, Message>> =
        vec![toolbar(state, &layout, app.codex_browser_busy())];
    let project = app.accounts_project();
    if let Some((_, name, _)) = project {
        sections.push(
            design::caption(format!(
                "Project: {name} · Choose ‘Use for this project’ from an account’s menu to use it for new shells and conversations in every worktree."
            ))
            .into(),
        );
    }
    if let Some((provider, path)) = &state.table.link {
        sections.push(link_form(state, *provider, path));
    }
    for provider in Account::REPORTED {
        // A provider accounts cannot be added to is listed only while it
        // reports some, or its record of them cannot be read.
        if Account::PROVIDERS.contains(&provider)
            || !rows(state, provider, state.table.sort).is_empty()
            || (provider == Provider::Omp && state.omp_problem().is_some())
        {
            sections.push(group(app, provider, &layout, project));
        }
    }
    widget::scrollable(
        widget::container(
            widget::column::with_children(sections)
                .spacing(space::XL)
                .max_width(MAX_WIDTH),
        )
        .padding(Padding::from([space::L, GUTTER]))
        .center_x(Length::Fill),
    )
    .direction(iced::widget::scrollable::Direction::Vertical(
        design::scrollbar(),
    ))
    .height(Length::Fill)
    .into()
}

fn toolbar<'a>(state: &'a State, layout: &Layout, browser_busy: bool) -> Element<'a, Message> {
    let action = |action| Message::AccountUi(action);
    let sort = crate::ui_controls::segmented_controls(
        Sort::ALL
            .into_iter()
            .map(|sort| {
                let selected = state.table.sort == sort;
                crate::interaction::tooltip(
                    widget::button::custom(
                        design::caption(sort.label())
                            .class(cosmic::theme::Text::Default)
                            .wrapping(cosmic::iced::widget::text::Wrapping::None),
                    )
                    .name(sort.description())
                    .selected(selected)
                    .class(crate::ui_controls::control_style(selected, None))
                    .padding([space::XXS, space::S])
                    .on_press(action(Action::Sort(sort))),
                    sort.description(),
                )
            })
            .collect(),
    );
    let mut add_content: Vec<Element<'a, Message>> = vec![
        widget::icon(Icon::Plus.handle())
            .size(design::COMPACT_ICON_SIZE)
            .into(),
    ];
    // A card too narrow for columns has no room for the words either.
    if !layout.stacked {
        add_content.push(
            design::body("Add account")
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .into(),
        );
    }
    let add_button = widget::button::custom(
        widget::row::with_children(add_content)
            .spacing(design::GRID_UNIT * 1.5)
            .align_y(Alignment::Center),
    )
    .name("Add account")
    .selected(state.table.add_menu)
    .class(cosmic::theme::Button::Suggested)
    .padding([space::XS, space::M])
    .on_press(action(Action::AddMenu(!state.table.add_menu)));
    let add_button: Element<'a, Message> = if layout.stacked {
        crate::interaction::tooltip(add_button, "Add account")
    } else {
        add_button.into()
    };
    let add = crate::ui_controls::popover(
        add_button,
        state.table.add_menu.then(|| {
            widget::container(
                widget::column::with_children(vec![
                    crate::ui_controls::popover_item(
                        crate::ui_controls::provider_icon(Provider::Claude),
                        if state.installed.get(&Provider::Claude) == Some(&false) { "Install Claude" } else { "Claude" },
                        if state.installed.get(&Provider::Claude) == Some(&false) { "CLI not found on PATH" } else { "Sign in with a Claude subscription" },
                        action(if state.installed.get(&Provider::Claude) == Some(&false) { Action::Install(Provider::Claude) } else { Action::Add {
                            provider: Provider::Claude,
                            device: false,
                        }}),
                    )
                    .into(),
                    crate::ui_controls::popover_item(
                        crate::ui_controls::provider_icon(Provider::Codex),
                        if state.installed.get(&Provider::Codex) == Some(&false) { "Install Codex" } else { "ChatGPT (Codex)" },
                        if crate::device::is_remote() {
                            "Sign in on another device with a one-time code"
                        } else if browser_busy {
                            "A browser sign-in is pending; finish it or use a code"
                        } else {
                            "Sign in through your browser"
                        },
                        action(Action::Add {
                            provider: Provider::Codex,
                            device: false,
                        }),
                    )
                    .on_press_maybe((!browser_busy).then(|| {
                        action(if state.installed.get(&Provider::Codex) == Some(&false) { Action::Install(Provider::Codex) } else { Action::Add {
                            provider: Provider::Codex,
                            device: false,
                        }})
                    }))
                    .into(),
                    crate::ui_controls::popover_item(
                        crate::ui_controls::provider_icon(Provider::Codex),
                        "ChatGPT (Codex) with a code",
                        "Sign in on another device with a one-time code",
                        action(if state.installed.get(&Provider::Codex) == Some(&false) {
                            Action::Install(Provider::Codex)
                        } else { Action::Add { provider: Provider::Codex, device: true } }),
                    )
                    .into(),
                    crate::ui_controls::popover_item(
                        crate::ui_controls::provider_icon(Provider::Claude),
                        "Existing Claude home",
                        "Use a Claude login and history already on this computer",
                        action(Action::LinkStart(Provider::Claude)),
                    )
                    .into(),
                    crate::ui_controls::popover_item(
                        crate::ui_controls::provider_icon(Provider::Codex),
                        "Existing Codex home",
                        "Use a Codex login and history already on this computer",
                        action(Action::LinkStart(Provider::Codex)),
                    )
                    .into(),
                    design::horizontal_rule(),
                    crate::ui_controls::field::checkbox(state.table.own_codex_config)
                        .label("Keep a separate Codex configuration")
                        .on_toggle(|own| Message::AccountUi(Action::OwnCodexConfig(own)))
                        .into(),
                    design::caption("Logins stay separate. Codex accounts can share preferences; new Claude accounts start with a copy of your preferences.")
                        .wrapping(cosmic::iced::widget::text::Wrapping::Word).into(),
                ])
                .spacing(space::XXS),
            )
            .width(300)
            .into()
        }),
        widget::popover::Position::Bottom,
        action(Action::AddMenu(false)),
    );
    let description = widget::column::with_children(vec![
        design::heading("Accounts").into(),
        design::caption(
            "Usage limits as each provider's own CLI reports them. Every login stays with its CLI; cagents never reads credentials.",
        )
        .wrapping(cosmic::iced::widget::text::Wrapping::Word)
        .into(),
    ])
    .spacing(space::XS)
    .width(Length::Fill);
    if layout.compact_toolbar {
        return widget::column::with_children(vec![
            description.into(),
            widget::row::with_children(vec![
                sort,
                iced::widget::Space::new().width(Length::Fill).into(),
                add,
            ])
            .spacing(space::M)
            .align_y(Alignment::Center)
            .into(),
        ])
        .spacing(space::M)
        .into();
    }
    widget::row::with_children(vec![description.into(), sort, add])
        .spacing(space::M)
        .align_y(Alignment::Center)
        .into()
}

fn link_form<'a>(state: &'a State, provider: Provider, path: &'a str) -> Element<'a, Message> {
    let name = provider.label();
    widget::container(
        widget::column::with_children(vec![
            design::heading(format!("Use an existing {name} home")).into(),
            design::caption("Choose the provider's home directory. Its login and session history stay where they are; removing this link will leave its files untouched.")
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into(),
            crate::ui_controls::field::text("Absolute path to the provider home", path)
                .id(link_id())
                .on_input(|value| Message::AccountUi(Action::LinkInput(value)))
                .on_submit(|_| Message::AccountUi(Action::LinkCommit))
                .width(Length::Fill)
                .into(),
            widget::row::with_children(vec![
                crate::ui_controls::button::secondary("Cancel")
                    .on_press_maybe((!state.table.linking).then_some(Message::AccountUi(Action::LinkCancel)))
                    .into(),
                crate::ui_controls::button::primary(if state.table.linking { "Adding…" } else { "Use home" })
                    .on_press_maybe((!state.table.linking && !path.trim().is_empty()).then_some(Message::AccountUi(Action::LinkCommit)))
                    .into(),
            ])
            .spacing(space::S)
            .into(),
        ])
        .spacing(space::S),
    )
    .padding(space::M)
    .class(design::surface(Some(design::raised), [design::RADIUS_M; 4], true))
    .into()
}

fn group<'a>(
    app: &'a Cagents,
    provider: Provider,
    layout: &Layout,
    project: Option<(crate::model::ProjectId, &'a str, &'a Path)>,
) -> Element<'a, Message> {
    let state = &app.account_ui;
    let rows = rows(state, provider, state.table.sort);
    let logins: Vec<&Login> = state
        .logins
        .iter()
        .filter(|login| login.provider == provider)
        .collect();
    let count = rows.len();
    let mut header = vec![
        widget::icon(crate::ui_controls::provider_icon(provider))
            .size(design::ROW_ICON_SIZE)
            .into(),
        design::heading(provider.label()).into(),
        design::caption(match count {
            1 => "1 account".to_owned(),
            count => format!("{count} accounts"),
        })
        .into(),
        iced::widget::Space::new().width(Length::Fill).into(),
    ];
    if Account::PROVIDERS.contains(&provider) && state.installed.get(&provider) != Some(&false) {
        header.push(
            crate::ui_controls::icon_button(
                Icon::Plus,
                format!("Add a {} account", provider.label()),
            )
            .compact()
            .on_press(Message::AccountUi(Action::Add {
                provider,
                device: false,
            }))
            .into(),
        );
    }
    let header = widget::row::with_children(header)
        .spacing(space::S)
        .align_y(Alignment::Center);

    let mut lines: Vec<Element<'a, Message>> = Vec::new();
    if (count > 0 || !logins.is_empty()) && !layout.stacked {
        lines.push(column_titles(layout));
    }
    for account in &rows {
        lines.push(design::horizontal_rule());
        lines.push(row(app, account, layout, project));
    }
    for login in logins {
        lines.push(design::horizontal_rule());
        lines.push(login_row(login));
    }
    if lines.is_empty() {
        let empty = match state.omp_problem().filter(|_| provider == Provider::Omp) {
            Some(problem) => format!(
                "{problem}, so the accounts {} keeps cannot be listed.",
                probe::cli_name(provider)
            ),
            None if state.installed.get(&provider) == Some(&false) => format!(
                "{} CLI not found on PATH. Install it, then refresh Accounts.",
                probe::cli_name(provider)
            ),
            None => format!(
                "No {} account yet. {} signs in with its own login, in a terminal here.",
                provider.label(),
                probe::cli_name(provider),
            ),
        };
        lines.push(
            widget::container(
                design::caption(empty).wrapping(cosmic::iced::widget::text::Wrapping::Word),
            )
            .padding([space::M, space::M])
            .into(),
        );
        if state.installed.get(&provider) == Some(&false) {
            lines.push(
                crate::ui_controls::button::primary(format!("Install {}", provider.label()))
                    .on_press(Message::AccountUi(Action::Install(provider)))
                    .into(),
            );
        }
    }
    widget::column::with_children(vec![
        widget::container(header).padding([0.0, space::XS]).into(),
        widget::container(widget::column::with_children(lines))
            .width(Length::Fill)
            .class(design::surface(
                Some(design::raised),
                [design::RADIUS_M; 4],
                true,
            ))
            .into(),
    ])
    .spacing(space::S)
    .into()
}

/// A table line: the account cell filling what the fixed columns leave, the
/// fixed columns, and the actions.
fn line<'a>(
    account: Element<'a, Message>,
    cells: Vec<(f32, Element<'a, Message>)>,
    actions: Element<'a, Message>,
    height: f32,
) -> Element<'a, Message> {
    let mut items: Vec<Element<'a, Message>> = vec![
        widget::container(account)
            .width(Length::Fill)
            .center_y(Length::Fill)
            .into(),
    ];
    items.extend(cells.into_iter().map(|(width, cell)| {
        widget::container(cell)
            .width(width)
            .center_y(Length::Fill)
            .into()
    }));
    items.push(
        widget::container(actions)
            .width(ACTIONS_WIDTH)
            .align_x(iced::alignment::Horizontal::Right)
            .center_y(Length::Fill)
            .into(),
    );
    widget::container(
        widget::row::with_children(items)
            .spacing(COLUMN_SPACING)
            .height(height),
    )
    .padding([0.0, space::M])
    .into()
}

fn column_titles<'a>(layout: &Layout) -> Element<'a, Message> {
    line(
        design::caption("Account").into(),
        layout
            .columns
            .iter()
            .map(|column| {
                (
                    column.width(layout.usage),
                    design::caption(column.title()).into(),
                )
            })
            .collect(),
        iced::widget::Space::new().into(),
        design::ROW_HEIGHT,
    )
}

const ROW_HEIGHT: f32 = 52.0;

fn dash<'a>() -> Element<'a, Message> {
    design::caption("—").into()
}

fn row<'a>(
    app: &'a Cagents,
    account: &'a AccountState,
    layout: &Layout,
    project: Option<(crate::model::ProjectId, &'a str, &'a Path)>,
) -> Element<'a, Message> {
    let state = &app.account_ui;
    let now = state.now_ms;
    let snapshot = account.snapshot.as_ref().filter(|snapshot| {
        matches!(
            snapshot.status,
            UsageStatus::Ok | UsageStatus::Warning | UsageStatus::Limited
        )
    });
    let stale = snapshot.is_some_and(|snapshot| is_stale(snapshot, now));
    let cells = layout
        .columns
        .iter()
        .map(|column| {
            let cell = match column {
                Column::Plan => account.plan().map_or_else(dash, |plan| {
                    crate::ui_controls::badge(plan, design::Tone::Themed(design::accent_color))
                }),
                Column::Session => usage_cell(
                    snapshot.and_then(|snapshot| window_of(snapshot, account::WindowKind::Session)),
                    now,
                    stale,
                ),
                Column::Weekly => usage_cell(
                    snapshot.and_then(|snapshot| window_of(snapshot, account::WindowKind::Weekly)),
                    now,
                    stale,
                ),
                Column::Model => {
                    let window = snapshot
                        .and_then(|snapshot| window_of(snapshot, account::WindowKind::Scoped));
                    let cell = usage_cell(window, now, stale);
                    match window {
                        Some(window) => crate::interaction::tooltip(cell, window.label.clone()),
                        None => cell,
                    }
                }
                Column::Credits => snapshot
                    .and_then(|snapshot| snapshot.credits.as_ref())
                    .map_or_else(dash, |credits| {
                        design::caption(credits.summary())
                            .class(cosmic::theme::Text::Default)
                            .wrapping(cosmic::iced::widget::text::Wrapping::None)
                            .into()
                    }),
                Column::Status => status_badge(account),
                Column::Updated => account.snapshot.as_ref().map_or_else(dash, |snapshot| {
                    let text = design::caption(account::format_ago(
                        unix_seconds(now) - unix_seconds(snapshot.observed_at),
                    ))
                    .wrapping(cosmic::iced::widget::text::Wrapping::None);
                    if stale {
                        text.class(cosmic::theme::Text::Custom(design::warning_text))
                            .into()
                    } else {
                        text.into()
                    }
                }),
            };
            (column.width(layout.usage), cell)
        })
        .collect::<Vec<_>>();
    if layout.stacked {
        return stacked_row(
            account_cell(app, account, project),
            layout.columns.iter().copied().zip(cells).collect(),
            actions(app, account, now, project),
        );
    }
    line(
        account_cell(app, account, project),
        cells,
        actions(app, account, now, project),
        ROW_HEIGHT,
    )
}

/// A row for a card too narrow for columns: the account and its actions,
/// then its figures side by side beneath, each under its column's title.
fn stacked_row<'a>(
    account: Element<'a, Message>,
    cells: Vec<(Column, (f32, Element<'a, Message>))>,
    actions: Element<'a, Message>,
) -> Element<'a, Message> {
    let figures = cells.into_iter().map(|(column, (_, cell))| {
        widget::column::with_children(vec![design::caption(column.title()).into(), cell])
            .spacing(space::XS)
            .width(Length::Fill)
            .into()
    });
    widget::column::with_children(vec![
        line(account, Vec::new(), actions, ROW_HEIGHT),
        widget::container(
            widget::row::with_children(figures.collect::<Vec<_>>()).spacing(COLUMN_SPACING),
        )
        .padding(Padding {
            top: 0.0,
            right: space::M,
            bottom: space::M,
            left: space::M,
        })
        .into(),
    ])
    .into()
}

fn account_cell<'a>(
    app: &'a Cagents,
    account: &'a AccountState,
    project: Option<(crate::model::ProjectId, &'a str, &'a Path)>,
) -> Element<'a, Message> {
    let state = &app.account_ui;
    let renaming = state
        .table
        .rename
        .as_ref()
        .filter(|(id, _)| id == &account.id);
    let name: Element<'a, Message> = match renaming {
        Some((_, draft)) => {
            crate::ui_controls::field::text(state.display_name(account), draft.as_str())
                .id(rename_id())
                .on_input(|text| Message::AccountUi(Action::RenameInput(text)))
                .on_submit(|_| Message::AccountUi(Action::RenameCommit))
                .width(Length::Fill)
                .into()
        }
        // Filling the cell is what lets a long name give way to the badge
        // after it instead of running under the row's actions.
        None => design::body(state.display_name(account))
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                cosmic::iced::advanced::text::EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill)
            .into(),
    };
    let mut title: Vec<Element<'a, Message>> = vec![name];
    let several = state
        .of(account.account.provider)
        .filter(|other| other.present || !other.account.is_system())
        .count()
        > 1;
    if several && state.registry.is_default(&account.account) && renaming.is_none() {
        title.push(crate::ui_controls::badge(
            "Default",
            design::Tone::Themed(design::muted_color),
        ));
    }
    if let Some((_, _, repository)) = project
        && state
            .registry
            .is_project_default(repository, &account.account)
        && renaming.is_none()
    {
        title.push(crate::ui_controls::badge(
            "This project",
            design::Tone::Themed(design::accent_color),
        ));
    }
    if let Some(conflict) = state.login_conflict(account)
        && renaming.is_none()
    {
        let tone = match conflict {
            LoginConflict::Changed { .. } => design::destructive_color,
            LoginConflict::SameHome { .. } | LoginConflict::SameLogin { .. } => {
                design::warning_color
            }
        };
        title.push(crate::interaction::tooltip(
            crate::ui_controls::badge(conflict.label(), design::Tone::Themed(tone)),
            conflict.detail(),
        ));
    }
    let email = account.email(&state.registry);
    let mut subtitle: Vec<String> = Vec::new();
    if account.account.is_brokered() {
        // Its name already carries its address.
        subtitle.push(format!(
            "Kept by {}, which picks the login to use",
            probe::cli_name(account.account.provider)
        ));
    } else if let Some(email) = email.filter(|email| *email != state.display_name(account)) {
        subtitle.push(email.to_owned());
    }
    if account.account.is_system() {
        subtitle.push(format!(
            "{}'s own login",
            probe::cli_name(account.account.provider)
        ));
    } else if account.account.is_linked() {
        if let Some(home) = account.account.resolved_home() {
            subtitle.push(format!("Existing home · {}", home.display()));
        }
    }
    if renaming.is_some() {
        subtitle =
            vec!["Enter to save, Escape to cancel. Leave empty for the default name.".into()];
    }
    let mut lines: Vec<Element<'a, Message>> = vec![
        widget::row::with_children(title)
            .spacing(space::S)
            .align_y(Alignment::Center)
            .into(),
    ];
    if !subtitle.is_empty() {
        lines.push(
            design::caption(subtitle.join(" · "))
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                    cosmic::iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                ))
                .into(),
        );
    }
    widget::row::with_children(vec![
        widget::icon(crate::ui_controls::provider_icon(account.account.provider))
            .size(design::ROW_ICON_SIZE)
            .into(),
        widget::column::with_children(lines)
            .spacing(1)
            .width(Length::Fill)
            .into(),
    ])
    .spacing(design::GRID_UNIT * 2.5)
    .align_y(Alignment::Center)
    .into()
}

/// A usage window as a table cell: what is left, when it resets, and a bar
/// in the tone of how close the window is to its end.
fn usage_cell<'a>(window: Option<&UsageWindow>, now: Millis, stale: bool) -> Element<'a, Message> {
    let Some(window) = window else {
        return dash();
    };
    let tone = if stale {
        design::Tone::Themed(design::muted_color)
    } else {
        design::Tone::Themed(design::budget_tone(window.used))
    };
    let mut figures: Vec<Element<'a, Message>> = vec![
        design::caption(format!("{}% left", percent_left(window.used)))
            .class(cosmic::theme::Text::Default)
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .into(),
        iced::widget::Space::new().width(Length::Fill).into(),
    ];
    if let Some(countdown) = reset_countdown(window, now) {
        figures.push(
            design::caption(countdown)
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .into(),
        );
    }
    widget::column::with_children(vec![
        widget::row::with_children(figures)
            .spacing(space::XS)
            .into(),
        crate::ui_controls::budget_bar(window.left(), tone),
    ])
    .spacing(space::XS)
    .width(Length::Fill)
    .into()
}

/// How an account stands, in a word and a tone.
fn status(account: &AccountState) -> (&'static str, fn(&cosmic::Theme) -> iced::Color) {
    let Some(snapshot) = &account.snapshot else {
        return if account.failure.is_some() {
            ("Unavailable", design::muted_color)
        } else {
            ("Checking", design::muted_color)
        };
    };
    match snapshot.status {
        UsageStatus::Ok => ("Available", design::success_color),
        UsageStatus::Warning => ("Near limit", design::warning_color),
        UsageStatus::Limited => ("Limited", design::destructive_color),
        UsageStatus::NeedsLogin => ("Signed out", design::warning_color),
        UsageStatus::Unavailable => ("Unavailable", design::muted_color),
    }
}

fn status_badge<'a>(account: &AccountState) -> Element<'a, Message> {
    let (label, tone) = status(account);
    let badge = crate::ui_controls::badge(label, design::Tone::Themed(tone));
    match (&account.failure, &account.snapshot) {
        (Some(failure), _) => {
            crate::interaction::tooltip(badge, format!("Last check failed: {failure}"))
        }
        (None, Some(snapshot)) if snapshot.status == UsageStatus::Unavailable => {
            match &snapshot.detail {
                Some(detail) => crate::interaction::tooltip(badge, detail.clone()),
                None => badge,
            }
        }
        _ => badge,
    }
}

fn actions<'a>(
    app: &'a Cagents,
    account: &'a AccountState,
    now: Millis,
    project: Option<(crate::model::ProjectId, &'a str, &'a Path)>,
) -> Element<'a, Message> {
    let state = &app.account_ui;
    let open = state.table.menu.as_ref() == Some(&account.id);
    let removing = state.removing.contains(&account.id);
    let menu_button = crate::ui_controls::icon_button(Icon::More, "Account actions")
        .compact()
        .on_press_maybe(
            (!removing).then(|| Message::AccountUi(Action::Menu(Some(account.id.clone())))),
        );
    // Omp names, signs in to and forgets its accounts itself, which leaves
    // nothing to do with one here but read it again. The space keeps its
    // refresh control in line with the other rows'.
    let menu: Element<'a, Message> = if account.account.is_brokered() {
        iced::widget::Space::new()
            .width(design::COMPACT_CONTROL_SIZE)
            .into()
    } else {
        crate::ui_controls::popover(
            menu_button,
            open.then(|| menu(app, account, project)),
            widget::popover::Position::Bottom,
            Message::AccountUi(Action::Menu(None)),
        )
        .into()
    };
    let first: Element<'a, Message> = if removing {
        design::caption("Removing…").into()
    } else {
        refresh_control(account, now).into()
    };
    widget::row::with_children(vec![first, menu])
        .spacing(space::XS)
        .align_y(Alignment::Center)
        .into()
}

/// What removing an account does to the cards open on it.
fn in_use_warning<'a>(cards: usize, linked: bool) -> Element<'a, Message> {
    match cards {
        0 => iced::widget::Space::new().into(),
        count => design::caption(if linked {
            format!(
                "{count} open {} on this home and will keep running.",
                if count == 1 { "card runs" } else { "cards run" }
            )
        } else {
            format!(
                "{count} open {} this account. Running processes are not stopped, but may lose access after sign-out.",
                if count == 1 { "card uses" } else { "cards use" },
            )
        })
        .wrapping(cosmic::iced::widget::text::Wrapping::Word)
        .into(),
    }
}

fn menu<'a>(
    app: &'a Cagents,
    account: &'a AccountState,
    project: Option<(crate::model::ProjectId, &'a str, &'a Path)>,
) -> Element<'a, Message> {
    let state = &app.account_ui;
    let action = |action| Message::AccountUi(action);
    let id = &account.id;
    let content: Element<'a, Message> = if state.table.confirm_remove.as_ref() == Some(id) {
        widget::column::with_children(vec![
            design::body(format!("Remove {}?", state.display_name(account))).into(),
            design::caption(if account.account.is_linked() {
                "This only removes the home from cagents. Its login, sessions and files stay where they are.".to_owned()
            } else {
                format!(
                    "{} signs this account out and cagents deletes its files. Resuming saved conversations will require choosing another account that can access them.",
                    probe::cli_name(account.account.provider)
                )
            })
            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
            .into(),
            in_use_warning(state.table.confirm_in_use, account.account.is_linked()),
            widget::row::with_children(vec![
                iced::widget::Space::new().width(Length::Fill).into(),
                crate::ui_controls::button::secondary("Cancel")
                    .on_press(action(Action::RemoveCancel))
                    .into(),
                crate::ui_controls::button::destructive("Remove")
                    .on_press(action(Action::RemoveConfirm(id.clone())))
                    .into(),
            ])
            .spacing(space::S)
            .into(),
        ])
        .spacing(space::S)
        .padding(space::S)
        .into()
    } else {
        let mut items: Vec<Element<'a, Message>> = Vec::new();
        if state.login_conflict(account).is_none() && !state.registry.is_default(&account.account) {
            items.push(
                crate::ui_controls::popover_item(
                    Icon::Check,
                    "Set as default",
                    "New conversations and shell terminals use this account",
                    action(Action::SetDefault(id.clone())),
                )
                .into(),
            );
        }
        if let Some((project, _, repository)) = project {
            if state
                .registry
                .is_project_default(repository, &account.account)
            {
                items.push(
                    crate::ui_controls::popover_item(
                        Icon::Clear,
                        "Use global default for this project",
                        "Remove this project's account choice",
                        action(Action::ClearProjectDefault(
                            project,
                            account.account.provider,
                        )),
                    )
                    .into(),
                );
            } else if state.login_conflict(account).is_none() {
                items.push(
                    crate::ui_controls::popover_item(
                        Icon::Folder,
                        "Use for this project",
                        "New shells and conversations in every worktree of this project use this account",
                        action(Action::SetProjectDefault(project, id.clone())),
                    )
                    .into(),
                );
            }
        }
        if let Some(accept) = state.reported_login_action(account) {
            let reported = account
                .identity
                .as_ref()
                .and_then(|identity| identity.email.as_deref())
                .unwrap_or("reported login");
            items.push(
                crate::ui_controls::button::secondary(format!("Use {reported} for this account"))
                    .on_press(action(accept))
                    .into(),
            );
        }
        items.push(
            crate::ui_controls::popover_item(
                Icon::Terminal,
                "Sign in again",
                "Run the provider's login in a terminal",
                action(Action::SignIn(id.clone())),
            )
            .into(),
        );
        if account.account.provider == Provider::Codex && !crate::device::is_remote() {
            items.push(
                crate::ui_controls::popover_item(
                    Icon::Terminal,
                    "Sign in again with a code",
                    "Use a one-time code to sign in to this account without a local browser callback",
                    action(Action::SignInWithCode(id.clone())),
                )
                .into(),
            );
        }
        items.push(
            crate::ui_controls::popover_item(
                Icon::Changes,
                "Rename",
                "Give this account a name of your own",
                action(Action::Rename(id.clone())),
            )
            .into(),
        );
        if account.account.registry_key().is_some() {
            if let Some(retained) = state.duplicate_to_keep(account) {
                items.push(
                    crate::ui_controls::popover_item(
                        Icon::Folder,
                        "Archive duplicate",
                        format!("Keep {}. Preserve this home's login, files and saved sessions. Link its home to restore it.", state.display_name(retained)),
                        action(Action::ArchiveDuplicate(id.clone())),
                    )
                    .into(),
                );
            }
            items.push(
                crate::ui_controls::popover_item(
                    Icon::Minus,
                    "Remove",
                    if account.account.is_linked() {
                        "Forget this home without changing its files"
                    } else {
                        "Sign out and delete this account's files"
                    },
                    action(Action::Remove(id.clone())),
                )
                .into(),
            );
        }
        widget::column::with_children(items)
            .spacing(space::XXS)
            .into()
    };
    widget::container(content).width(300).into()
}

/// What a pending sign-in's row says it is waiting for.
fn login_caption(login: &Login) -> &'static str {
    match login.phase {
        // Its home has been asked already, so the terminal has ended and
        // there is nothing left to finish in it: the wait is for the next ask.
        LoginPhase::Waiting if login.attempts > 0 => "Checking which account signed in…",
        LoginPhase::Waiting => "Finish signing in in the terminal. Closing it cancels.",
        LoginPhase::Verifying => "Checking which account signed in…",
        LoginPhase::Settling => "Saving…",
        LoginPhase::CallbackBlocked => account::login::PORT_BUSY,
        LoginPhase::PreparingCode(_) => "Preparing a code sign-in…",
        LoginPhase::CodeStarted => {
            "Code sign-in opened. This failed browser sign-in can be dismissed."
        }
        LoginPhase::Dismissing(_) => "Removing cancelled sign-in…",
        LoginPhase::DismissFailed => "Could not remove the cancelled sign-in. Retry dismissing it.",
    }
}

fn login_row<'a>(login: &'a Login) -> Element<'a, Message> {
    let phase = login_caption(login);
    let account = widget::row::with_children(vec![
        if matches!(
            login.phase,
            LoginPhase::CallbackBlocked | LoginPhase::CodeStarted | LoginPhase::DismissFailed
        ) {
            widget::icon(Icon::Warning.handle())
                .size(design::ROW_ICON_SIZE)
                .into()
        } else {
            crate::ui_controls::activity_spinner(
                f32::from(design::ROW_ICON_SIZE),
                design::accent_color,
            )
        },
        widget::column::with_children(vec![
            design::body(format!("New {} account", login.provider.label())).into(),
            crate::interaction::tooltip(
                design::caption(phase)
                    .wrapping(cosmic::iced::widget::text::Wrapping::None)
                    .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                        cosmic::iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                    )),
                phase,
            ),
        ])
        .spacing(1)
        .width(Length::Fill)
        .into(),
    ])
    .spacing(design::GRID_UNIT * 2.5)
    .align_y(Alignment::Center)
    .into();
    let cancel: Element<'a, Message> =
        if login.phase == LoginPhase::CallbackBlocked && login.provider == Provider::Codex {
            widget::row::with_children(vec![
                crate::ui_controls::button::quiet("Use code")
                    .on_press(Message::AccountUi(Action::UseCodeForBlockedLogin(
                        login.key,
                    )))
                    .into(),
                crate::ui_controls::icon_button(Icon::Close, "Dismiss failed sign-in")
                    .compact()
                    .on_press(Message::AccountUi(Action::CancelLogin(login.key)))
                    .into(),
            ])
            .into()
        } else if matches!(
            login.phase,
            LoginPhase::Waiting
                | LoginPhase::PreparingCode(_)
                | LoginPhase::CallbackBlocked
                | LoginPhase::CodeStarted
                | LoginPhase::DismissFailed
        ) {
            let label = match login.phase {
                LoginPhase::DismissFailed => "Retry dismissing sign-in",
                LoginPhase::CallbackBlocked | LoginPhase::CodeStarted => "Dismiss failed sign-in",
                _ => "Cancel sign-in",
            };
            crate::ui_controls::icon_button(Icon::Close, label)
                .compact()
                .on_press(Message::AccountUi(Action::CancelLogin(login.key)))
                .into()
        } else {
            iced::widget::Space::new().into()
        };
    // Recovery controls need their intrinsic width. Ordinary account rows
    // keep the table's fixed action column.
    let row = widget::container(
        widget::row::with_children(vec![
            widget::container(account)
                .width(Length::Fill)
                .center_y(Length::Fill)
                .into(),
            widget::container(cancel)
                .width(Length::Shrink)
                .center_y(Length::Fill)
                .into(),
        ])
        .spacing(COLUMN_SPACING)
        .height(ROW_HEIGHT),
    )
    .padding([0.0, space::M]);
    crate::ui_controls::owned((login.key, login.phase, login.provider), row)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Completion messages below are explicit component fixtures. Every real
    // preparation, cleanup and terminal task is kept unpolled; these tests
    // establish reducer/widget custody, not discovery or authentication.
    fn blocked_login(key: u128) -> Login {
        let key = uuid::Uuid::from_u128(key);
        Login {
            provider: Provider::Codex,
            key,
            home: PathBuf::from("/cagents-fixture/accounts/codex").join(key.to_string()),
            phase: LoginPhase::CallbackBlocked,
            attempts: 0,
            next_check: 0,
            started_at: 0,
            recovered: false,
        }
    }

    fn recovery_app() -> Cagents {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.account_ui.homes = Some(Homes::at("/cagents-fixture/accounts"));
        app.account_ui.logins.push(blocked_login(195401));
        app
    }

    // Capture the production row's mouse callback, rather than inventing an
    // action that could conceal an unkeyed Add control in the rendered row.
    fn click_login_control(login: &Login, label: &str) -> Action {
        use iced::advanced::clipboard;
        use iced::runtime::{UserInterface, user_interface::Cache};
        use iced::{Event, Size, mouse};

        let mut renderer = crate::app::tests::headless_renderer();
        let mut ui = UserInterface::build(
            login_row(login),
            Size::new(1000.0, 200.0),
            Cache::new(),
            &mut renderer,
        );
        ui.draw(
            &mut renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            mouse::Cursor::Unavailable,
        );
        let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let bounds = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| node.node().label() == Some(label))
            .unwrap_or_else(|| panic!("missing actual login control: {label}"))
            .node()
            .bounds()
            .unwrap();
        assert!(bounds.x1 > bounds.x0 && bounds.y1 > bounds.y0);
        let point = iced::Point::new(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
        let mut messages = Vec::new();
        let _ = ui.update(
            &[
                Event::Mouse(mouse::Event::CursorMoved { position: point }),
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ],
            mouse::Cursor::Available(point),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(messages.len(), 1, "actual {label} callback: {messages:?}");
        match messages.pop().unwrap() {
            Message::AccountUi(action) => action,
            message => panic!("unexpected login callback: {message:?}"),
        }
    }

    fn phase(app: &Cagents, key: uuid::Uuid) -> LoginPhase {
        app.account_ui
            .logins
            .iter()
            .find(|login| login.key == key)
            .unwrap()
            .phase
    }

    fn preparation(app: &Cagents, key: uuid::Uuid) -> uuid::Uuid {
        let LoginPhase::PreparingCode(request) = phase(app, key) else {
            panic!("expected a reserved code preparation");
        };
        request
    }

    fn dismissal(app: &Cagents, key: uuid::Uuid) -> uuid::Uuid {
        let LoginPhase::Dismissing(request) = phase(app, key) else {
            panic!("expected a reserved dismissal");
        };
        request
    }

    fn replacement_popup(app: &mut Cagents) {
        drop(app.update_account_ui(Action::AddMenu(true)));
        assert!(app.account_ui.table.add_menu);
    }

    fn draw_recovery_ui(
        ui: &mut iced::runtime::UserInterface<'_, Message, cosmic::Theme, cosmic::Renderer>,
        renderer: &mut cosmic::Renderer,
    ) {
        ui.draw(
            renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            iced::mouse::Cursor::Unavailable,
        );
    }

    fn cached_recovery_row<'a>(
        login: &'a Login,
        width: f32,
        cache: iced::runtime::user_interface::Cache,
        renderer: &mut cosmic::Renderer,
    ) -> iced::runtime::UserInterface<'a, Message, cosmic::Theme, cosmic::Renderer> {
        let mut ui = iced::runtime::UserInterface::build(
            login_row(login),
            iced::Size::new(width, 200.0),
            cache,
            renderer,
        );
        draw_recovery_ui(&mut ui, renderer);
        ui
    }

    fn recovery_control(
        ui: &iced::runtime::UserInterface<'_, Message, cosmic::Theme, cosmic::Renderer>,
        label: &str,
    ) -> iced::Rectangle {
        let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
        let bounds = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| node.node().label() == Some(label))
            .unwrap_or_else(|| panic!("missing cached recovery control: {label}"))
            .node()
            .bounds()
            .unwrap();
        iced::Rectangle {
            x: bounds.x0 as f32,
            y: bounds.y0 as f32,
            width: (bounds.x1 - bounds.x0) as f32,
            height: (bounds.y1 - bounds.y0) as f32,
        }
    }

    fn recovery_mouse(
        ui: &mut iced::runtime::UserInterface<'_, Message, cosmic::Theme, cosmic::Renderer>,
        renderer: &mut cosmic::Renderer,
        point: iced::Point,
        buttons: &[iced::mouse::Event],
    ) -> Vec<Message> {
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
        draw_recovery_ui(ui, renderer);
        messages
    }

    fn assert_recovery_callback(messages: &[Message], label: &str, key: uuid::Uuid) {
        if label == "Use code" {
            assert!(matches!(messages,
                [Message::AccountUi(Action::UseCodeForBlockedLogin(actual))] if *actual == key));
        } else {
            assert!(matches!(messages,
                [Message::AccountUi(Action::CancelLogin(actual))] if *actual == key));
        }
    }

    #[test]
    fn full_accounts_table_contains_disjoint_recovery_controls_at_narrow_and_wide_widths() {
        use iced::mouse::{Button, Event};
        use iced::runtime::{UserInterface, user_interface::Cache};

        let mut renderer = crate::app::tests::headless_renderer();
        let token = uuid::Uuid::from_u128(195420);
        let cases: [(LoginPhase, &str, &[&str]); 6] = [
            (
                LoginPhase::CallbackBlocked,
                account::login::PORT_BUSY,
                &["Use code", "Dismiss failed sign-in"],
            ),
            (
                LoginPhase::PreparingCode(token),
                "Preparing a code sign-in…",
                &["Cancel sign-in"],
            ),
            (
                LoginPhase::CodeStarted,
                "Code sign-in opened. This failed browser sign-in can be dismissed.",
                &["Dismiss failed sign-in"],
            ),
            (
                LoginPhase::Dismissing(token),
                "Removing cancelled sign-in…",
                &[],
            ),
            (
                LoginPhase::DismissFailed,
                "Could not remove the cancelled sign-in. Retry dismissing it.",
                &["Retry dismissing sign-in"],
            ),
            (
                LoginPhase::Waiting,
                "Finish signing in in the terminal. Closing it cancels.",
                &["Cancel sign-in"],
            ),
        ];
        for width in [300.0, 1000.0] {
            for populated in [false, true] {
                for (phase, caption, expected) in cases {
                    let mut app = recovery_app();
                    app.account_ui.accounts.clear();
                    if populated {
                        for (provider, key) in
                            [(Provider::Claude, 195421), (Provider::Codex, 195422)]
                        {
                            let id = app.account_ui.add_managed_fixture(provider, key);
                            app.account_ui.set_fixture_label(
                                &id,
                                Some("A deliberately long healthy account name beside recovery"),
                            );
                        }
                    }
                    let key = app.account_ui.logins[0].key;
                    // Static layout fixtures for each state; the surrounding
                    // component parents exercise their real task admission.
                    app.account_ui.logins[0].phase = phase;
                    let size = iced::Size::new(width, 1600.0);
                    // The production responsive view includes the toolbar,
                    // provider groups, ordinary rows, and real scrollable.
                    let mut ui =
                        UserInterface::build(view(&app), size, Cache::new(), &mut renderer);
                    draw_recovery_ui(&mut ui, &mut renderer);
                    let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
                    let nodes = nodes
                        .root()
                        .iter()
                        .chain(nodes.children())
                        .collect::<Vec<_>>();
                    assert!(
                        nodes.iter().any(|node| node.node().label() == Some(caption)
                            || node.node().value() == Some(caption)),
                        "missing recovery status {phase:?} at {width}"
                    );
                    let buttons = nodes
                        .iter()
                        .filter(|node| {
                            node.node().role() == iced_accessibility::accesskit::Role::Button
                        })
                        .map(|node| {
                            let label = node.node().label().unwrap_or("unlabelled").to_owned();
                            let bounds = node.node().bounds().unwrap();
                            (
                                label,
                                iced::Rectangle {
                                    x: bounds.x0 as f32,
                                    y: bounds.y0 as f32,
                                    width: (bounds.x1 - bounds.x0) as f32,
                                    height: (bounds.y1 - bounds.y0) as f32,
                                },
                            )
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(
                        buttons
                            .iter()
                            .filter(|(label, _)| label == "Account actions")
                            .count(),
                        if populated { 2 } else { 0 }
                    );
                    for (index, (label, bounds)) in buttons.iter().enumerate() {
                        assert!(
                            bounds.width > 0.0
                                && bounds.height > 0.0
                                && bounds.x >= -0.5
                                && bounds.y >= -0.5
                                && bounds.x + bounds.width <= width + 0.5
                                && bounds.y + bounds.height <= size.height + 0.5,
                            "full-table target escapes {width}px viewport: {label} {bounds:?}, {phase:?}"
                        );
                        for (other, next) in &buttons[index + 1..] {
                            let overlap_x = (bounds.x + bounds.width).min(next.x + next.width)
                                - bounds.x.max(next.x);
                            let overlap_y = (bounds.y + bounds.height).min(next.y + next.height)
                                - bounds.y.max(next.y);
                            assert!(
                                overlap_x <= 0.5 || overlap_y <= 0.5,
                                "full-table targets overlap at {width}: {label} {bounds:?} / {other} {next:?}"
                            );
                        }
                    }
                    let control_labels = [
                        "Use code",
                        "Dismiss failed sign-in",
                        "Cancel sign-in",
                        "Retry dismissing sign-in",
                    ];
                    let actual = buttons
                        .iter()
                        .filter(|(label, _)| control_labels.contains(&label.as_str()))
                        .map(|(label, _)| label.as_str())
                        .collect::<Vec<_>>();
                    assert_eq!(
                        actual, expected,
                        "recovery actions for {phase:?} at {width}"
                    );
                    let mut captured = Vec::new();
                    for label in expected {
                        let point = recovery_control(&ui, label).center();
                        let messages = recovery_mouse(
                            &mut ui,
                            &mut renderer,
                            point,
                            &[
                                Event::ButtonPressed(Button::Left),
                                Event::ButtonReleased(Button::Left),
                            ],
                        );
                        assert_recovery_callback(&messages, label, key);
                        captured.extend(messages);
                    }
                    drop(ui);
                    let held = captured
                        .into_iter()
                        .map(|message| match message {
                            Message::AccountUi(action) => app.update_account_ui(action),
                            _ => unreachable!("only row callbacks were captured"),
                        })
                        .collect::<Vec<_>>();
                    // Applying a captured action only constructs its task.
                    // Neither discovery nor cleanup/authentication is polled.
                    drop(held);
                }
            }
        }
    }

    #[test]
    fn a_held_recovery_press_cannot_transfer_to_another_login_or_provider() {
        use iced::mouse::{Button, Event};
        use iced::runtime::user_interface::Cache;

        let mut renderer = crate::app::tests::headless_renderer();
        for width in [300.0, 1000.0] {
            let first = blocked_login(195430);
            let mut other_provider = first.clone();
            other_provider.provider = Provider::Claude;
            other_provider.home =
                PathBuf::from("/cagents-fixture/accounts/claude").join(first.key.to_string());
            for (next, label, next_label) in [
                (blocked_login(195431), "Use code", "Use code"),
                (
                    blocked_login(195432),
                    "Dismiss failed sign-in",
                    "Dismiss failed sign-in",
                ),
                (
                    other_provider,
                    "Dismiss failed sign-in",
                    "Dismiss failed sign-in",
                ),
            ] {
                let mut ui = cached_recovery_row(&first, width, Cache::new(), &mut renderer);
                let point = recovery_control(&ui, label).center();
                assert!(
                    recovery_mouse(
                        &mut ui,
                        &mut renderer,
                        point,
                        &[Event::ButtonPressed(Button::Left)]
                    )
                    .is_empty()
                );
                let cache = ui.into_cache();
                let mut ui = cached_recovery_row(&next, width, cache, &mut renderer);
                let point = recovery_control(&ui, next_label).center();
                assert!(
                    recovery_mouse(
                        &mut ui,
                        &mut renderer,
                        point,
                        &[Event::ButtonReleased(Button::Left)]
                    )
                    .is_empty(),
                    "held {label} transferred ownership at {width}px"
                );
                let messages = recovery_mouse(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[
                        Event::ButtonPressed(Button::Left),
                        Event::ButtonReleased(Button::Left),
                    ],
                );
                assert_recovery_callback(&messages, next_label, next.key);
                drop(ui);
                let mut app = recovery_app();
                app.account_ui.logins = vec![next];
                let Message::AccountUi(action) = messages.into_iter().next().unwrap() else {
                    unreachable!()
                };
                let held = app.update_account_ui(action);
                assert_eq!(held.units(), 1, "fresh replacement press remains usable");
                drop(held);
            }
        }
    }

    #[test]
    fn a_held_cancel_press_cannot_transfer_across_code_preparation_retry() {
        use iced::mouse::{Button, Event};
        use iced::runtime::user_interface::Cache;

        let mut renderer = crate::app::tests::headless_renderer();
        for width in [300.0, 1000.0] {
            let mut app = recovery_app();
            let key = app.account_ui.logins[0].key;
            let code = click_login_control(&app.account_ui.logins[0], "Use code");
            let first_held = app.update_account_ui(code);
            let first = preparation(&app, key);
            let mut ui = cached_recovery_row(
                &app.account_ui.logins[0],
                width,
                Cache::new(),
                &mut renderer,
            );
            let point = recovery_control(&ui, "Cancel sign-in").center();
            assert!(
                recovery_mouse(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonPressed(Button::Left)]
                )
                .is_empty()
            );
            let cache = ui.into_cache();
            // A labeled component failure retires the captured preparation;
            // neither its held producer nor the retry producer is executed.
            drop(app.update_account_ui(Action::HomeReady {
                request: first,
                result: Err("captured preparation failed during held press".into()),
            }));
            let code = click_login_control(&app.account_ui.logins[0], "Use code");
            let retry_held = app.update_account_ui(code);
            let retry = preparation(&app, key);
            assert_ne!(first, retry);
            let mut ui =
                cached_recovery_row(&app.account_ui.logins[0], width, cache, &mut renderer);
            let point = recovery_control(&ui, "Cancel sign-in").center();
            assert!(
                recovery_mouse(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonReleased(Button::Left)]
                )
                .is_empty(),
                "the previous preparation's press must not cancel the retry"
            );
            assert_eq!(preparation(&app, key), retry);
            let fresh = recovery_mouse(
                &mut ui,
                &mut renderer,
                point,
                &[
                    Event::ButtonPressed(Button::Left),
                    Event::ButtonReleased(Button::Left),
                ],
            );
            assert_recovery_callback(&fresh, "Cancel sign-in", key);
            drop(ui);
            let Message::AccountUi(cancel) = fresh.into_iter().next().unwrap() else {
                unreachable!()
            };
            let cancel_held = app.update_account_ui(cancel);
            assert!(app.account_ui.home_requests[&retry].cancelled);
            assert!(matches!(phase(&app, key), LoginPhase::Dismissing(_)));
            drop((first_held, retry_held, cancel_held));
        }
    }

    #[test]
    fn a_held_recovery_press_cannot_transfer_across_admission_and_dismissal_failure() {
        use iced::mouse::{Button, Event};
        use iced::runtime::user_interface::Cache;

        let mut renderer = crate::app::tests::headless_renderer();
        for width in [300.0, 1000.0] {
            let mut app = recovery_app();
            let key = app.account_ui.logins[0].key;
            let code = click_login_control(&app.account_ui.logins[0], "Use code");
            let mut ui = cached_recovery_row(
                &app.account_ui.logins[0],
                width,
                Cache::new(),
                &mut renderer,
            );
            let point = recovery_control(&ui, "Dismiss failed sign-in").center();
            assert!(
                recovery_mouse(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonPressed(Button::Left)]
                )
                .is_empty()
            );
            let cache = ui.into_cache();
            let code_held = app.update_account_ui(code);
            let preparation = preparation(&app, key);
            let mut ui =
                cached_recovery_row(&app.account_ui.logins[0], width, cache, &mut renderer);
            let point = recovery_control(&ui, "Cancel sign-in").center();
            assert!(
                recovery_mouse(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonReleased(Button::Left)]
                )
                .is_empty(),
                "a blocked-row Dismiss press must not transfer to code preparation"
            );
            assert!(
                recovery_mouse(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonPressed(Button::Left)]
                )
                .is_empty()
            );
            let cache = ui.into_cache();
            let cancel = click_login_control(&app.account_ui.logins[0], "Cancel sign-in");
            let cancel_held = app.update_account_ui(cancel);
            let request = dismissal(&app, key);
            assert!(app.account_ui.home_requests[&preparation].cancelled);
            // A fast explicit component failure can arrive before a frame
            // shows Dismissing. The old press still must not become Retry.
            drop(app.update_account_ui(Action::LoginDismissed {
                key,
                request,
                result: Err("captured dismissal failed during held press".into()),
            }));
            let mut ui =
                cached_recovery_row(&app.account_ui.logins[0], width, cache, &mut renderer);
            let point = recovery_control(&ui, "Retry dismissing sign-in").center();
            assert!(
                recovery_mouse(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonReleased(Button::Left)]
                )
                .is_empty(),
                "the preparation's Cancel press must not transfer to dismissal Retry"
            );
            let retry = recovery_mouse(
                &mut ui,
                &mut renderer,
                point,
                &[
                    Event::ButtonPressed(Button::Left),
                    Event::ButtonReleased(Button::Left),
                ],
            );
            assert_recovery_callback(&retry, "Retry dismissing sign-in", key);
            drop(ui);
            let Message::AccountUi(action) = retry.into_iter().next().unwrap() else {
                unreachable!()
            };
            let retry_held = app.update_account_ui(action);
            assert!(matches!(phase(&app, key), LoginPhase::Dismissing(_)));
            drop((code_held, cancel_held, retry_held));
        }
    }

    #[test]
    fn an_unchanged_recovery_owner_keeps_a_valid_press_across_cached_rebuilds() {
        use iced::mouse::{Button, Event};
        use iced::runtime::user_interface::Cache;

        let mut renderer = crate::app::tests::headless_renderer();
        for width in [300.0, 1000.0] {
            for label in ["Use code", "Dismiss failed sign-in"] {
                let first = blocked_login(195440);
                let mut next = first.clone();
                next.attempts = 2;
                next.next_check = 123;
                let mut ui = cached_recovery_row(&first, width, Cache::new(), &mut renderer);
                let point = recovery_control(&ui, label).center();
                assert!(
                    recovery_mouse(
                        &mut ui,
                        &mut renderer,
                        point,
                        &[Event::ButtonPressed(Button::Left)]
                    )
                    .is_empty()
                );
                let cache = ui.into_cache();
                let mut ui = cached_recovery_row(&next, width, cache, &mut renderer);
                assert!(recovery_control(&ui, label).contains(point));
                let release = recovery_mouse(
                    &mut ui,
                    &mut renderer,
                    point,
                    &[Event::ButtonReleased(Button::Left)],
                );
                assert_recovery_callback(&release, label, first.key);
                drop(ui);
                let mut app = recovery_app();
                app.account_ui.logins = vec![next];
                let Message::AccountUi(action) = release.into_iter().next().unwrap() else {
                    unreachable!()
                };
                let held = app.update_account_ui(action);
                assert_eq!(held.units(), 1);
                drop(held);
            }
        }
    }

    #[test]
    fn a_code_recovery_is_reserved_before_its_real_task_is_polled() {
        let mut app = recovery_app();
        let key = app.account_ui.logins[0].key;
        let code = click_login_control(&app.account_ui.logins[0], "Use code");
        assert!(matches!(code, Action::UseCodeForBlockedLogin(owner) if owner == key));
        assert!(code.blocked_while_modal());
        let held = app.update_account_ui(code.clone());
        assert_eq!(held.units(), 1);
        let request = preparation(&app, key);
        assert_eq!(app.account_ui.home_requests.len(), 1);
        assert_eq!(
            app.account_ui.home_requests[&request].blocked_login,
            Some(key)
        );
        replacement_popup(&mut app);
        app.status = Some(TransientNotice::warning("unrelated popup notice"));
        let notice = app.status.clone();
        let registry = app.account_ui.registry.clone();
        let cards = app.durable.cards.len();
        assert_eq!(app.update_account_ui(code).units(), 0);
        assert!(app.account_ui.table.add_menu);
        assert_eq!(app.status, notice);
        assert_eq!(app.account_ui.registry, registry);
        assert_eq!(app.durable.cards.len(), cards);
        assert_eq!(preparation(&app, key), request);
        assert_eq!(app.account_ui.home_requests.len(), 1);
        // No discovery, creation, provider or authority checks run: held is
        // the real action's future, and it is never polled.
        drop(held);
    }

    #[test]
    fn a_code_failure_releases_only_its_request_and_an_old_reply_cannot_end_retry() {
        let mut app = recovery_app();
        app.account_ui.logins.push(blocked_login(195402));
        let key = app.account_ui.logins[0].key;
        let other = app.account_ui.logins[1].key;
        let code = click_login_control(&app.account_ui.logins[0], "Use code");
        let held = app.update_account_ui(code.clone());
        let request = preparation(&app, key);
        let other_code = click_login_control(&app.account_ui.logins[1], "Use code");
        let other_held = app.update_account_ui(other_code);
        let other_request = preparation(&app, other);
        replacement_popup(&mut app);
        // Explicit component completion for the captured request. The held
        // producer is not executed, so this is not provider failure evidence.
        drop(app.update_account_ui(Action::HomeReady {
            request,
            result: Err("captured first preparation failure".into()),
        }));
        assert_eq!(phase(&app, key), LoginPhase::CallbackBlocked);
        assert_eq!(preparation(&app, other), other_request);
        assert!(app.account_ui.home_requests.contains_key(&other_request));
        assert!(!app.account_ui.home_requests.contains_key(&request));
        assert!(app.account_ui.table.add_menu);
        let notice = app.status.as_ref().unwrap();
        assert_eq!(notice.key, Some(NoticeKey::SignIn(key)));
        assert_eq!(
            notice.details.as_deref(),
            Some("captured first preparation failure")
        );
        let retry = click_login_control(&app.account_ui.logins[0], "Use code");
        let retry_held = app.update_account_ui(retry);
        let retry_request = preparation(&app, key);
        assert_ne!(retry_request, request);
        replacement_popup(&mut app);
        let notice = app.status.clone();
        assert_eq!(
            app.update_account_ui(Action::HomeReady {
                request,
                result: Err("replayed old failure".into()),
            })
            .units(),
            0
        );
        assert_eq!(preparation(&app, key), retry_request);
        assert_eq!(preparation(&app, other), other_request);
        assert_eq!(app.account_ui.home_requests.len(), 2);
        assert!(app.account_ui.table.add_menu);
        assert_eq!(app.status, notice);
        drop((held, other_held, retry_held));
    }

    #[test]
    fn dismissal_reserves_before_settlement_and_failed_cleanup_is_explicitly_retryable() {
        let mut app = recovery_app();
        let key = app.account_ui.logins[0].key;
        let code = click_login_control(&app.account_ui.logins[0], "Use code");
        let cancel = click_login_control(&app.account_ui.logins[0], "Dismiss failed sign-in");
        let held = app.update_account_ui(cancel.clone());
        assert_eq!(held.units(), 1);
        let request = dismissal(&app, key);
        replacement_popup(&mut app);
        assert_eq!(app.update_account_ui(code.clone()).units(), 0);
        assert_eq!(app.update_account_ui(cancel).units(), 0);
        // Older unscoped verification/settlement callbacks cannot take this
        // new dismissal's custody or turn it back into automatic probing.
        assert_eq!(
            app.update_account_ui(Action::LoginVerified {
                key,
                outcome: Outcome::Busy
            })
            .units(),
            0
        );
        assert_eq!(
            app.update_account_ui(Action::LoginSettled {
                key,
                result: Ok(Settled::Abandoned(None)),
            })
            .units(),
            0
        );
        assert_eq!(dismissal(&app, key), request);
        assert_eq!(app.update_account_ui(Action::LoginTick).units(), 0);
        assert!(app.account_ui.table.add_menu);
        drop(app.update_account_ui(Action::LoginDismissed {
            key,
            request,
            result: Err("captured dismissal failure".into()),
        }));
        assert_eq!(phase(&app, key), LoginPhase::DismissFailed);
        assert_eq!(
            app.status.as_ref().unwrap().details.as_deref(),
            Some("captured dismissal failure")
        );
        assert_eq!(app.update_account_ui(code).units(), 0);
        assert_eq!(app.update_account_ui(Action::LoginTick).units(), 0);
        let retry = click_login_control(&app.account_ui.logins[0], "Retry dismissing sign-in");
        let retry_held = app.update_account_ui(retry);
        assert_eq!(retry_held.units(), 1);
        let retry_request = dismissal(&app, key);
        assert_ne!(retry_request, request);
        let notice = app.status.clone();
        drop(app.update_account_ui(Action::LoginDismissed {
            key,
            request,
            result: Ok(None),
        }));
        assert_eq!(dismissal(&app, key), retry_request);
        assert_eq!(app.status, notice);
        assert!(app.account_ui.table.add_menu);
        drop(app.update_account_ui(Action::LoginDismissed {
            key,
            request: retry_request,
            result: Ok(None),
        }));
        assert!(app.account_ui.logins.is_empty());
        assert!(app.account_ui.home_requests.is_empty());
        assert!(app.account_ui.table.add_menu);
        drop((held, retry_held));
    }

    #[test]
    fn a_cancelled_code_success_keeps_its_late_home_cleanup_owned_and_retryable() {
        let mut app = recovery_app();
        let key = app.account_ui.logins[0].key;
        let code = click_login_control(&app.account_ui.logins[0], "Use code");
        let held = app.update_account_ui(code.clone());
        let request = preparation(&app, key);
        let cancel = click_login_control(&app.account_ui.logins[0], "Cancel sign-in");
        let dismissal_held = app.update_account_ui(cancel);
        let dismiss_request = dismissal(&app, key);
        assert!(app.account_ui.home_requests[&request].cancelled);
        assert_eq!(app.update_account_ui(code).units(), 0);
        drop(app.update_account_ui(Action::LoginDismissed {
            key,
            request: dismiss_request,
            result: Ok(None),
        }));
        assert!(app.account_ui.logins.is_empty());
        assert!(app.account_ui.home_requests.contains_key(&request));
        app.account_ui.logins.push(blocked_login(195403));
        let other = app.account_ui.logins[0].key;
        replacement_popup(&mut app);
        let late = blocked_login(195404);
        let prepared = (late.key, late.home.clone(), PathBuf::from("/fixture/codex"));
        let cards = app.durable.cards.len();
        let cleanup_held = app.update_account_ui(Action::HomeReady {
            request,
            result: Ok(prepared.clone()),
        });
        assert_eq!(cleanup_held.units(), 1);
        let cleanup_request = dismissal(&app, late.key);
        assert_eq!(phase(&app, other), LoginPhase::CallbackBlocked);
        assert_eq!(
            app.durable.cards.len(),
            cards,
            "cancelled success cannot open a terminal"
        );
        assert!(app.account_ui.home_requests.is_empty());
        assert!(app.account_ui.table.add_menu);
        assert_eq!(
            app.update_account_ui(Action::HomeReady {
                request,
                result: Ok(prepared)
            })
            .units(),
            0
        );
        assert_eq!(dismissal(&app, late.key), cleanup_request);
        assert_eq!(app.account_ui.logins.len(), 2);
        drop(app.update_account_ui(Action::LoginDismissed {
            key: late.key,
            request: cleanup_request,
            result: Err("captured late-home cleanup failure".into()),
        }));
        assert_eq!(phase(&app, late.key), LoginPhase::DismissFailed);
        assert_eq!(
            app.status.as_ref().unwrap().key,
            Some(NoticeKey::SignIn(late.key))
        );
        assert_eq!(
            app.status.as_ref().unwrap().details.as_deref(),
            Some("captured late-home cleanup failure")
        );
        assert_eq!(app.update_account_ui(Action::LoginTick).units(), 0);
        let retry = click_login_control(
            app.account_ui
                .logins
                .iter()
                .find(|login| login.key == late.key)
                .unwrap(),
            "Retry dismissing sign-in",
        );
        let retry_held = app.update_account_ui(retry);
        let retry_request = dismissal(&app, late.key);
        drop(app.update_account_ui(Action::LoginDismissed {
            key: late.key,
            request: cleanup_request,
            result: Ok(None),
        }));
        assert_eq!(dismissal(&app, late.key), retry_request);
        drop(app.update_account_ui(Action::LoginDismissed {
            key: late.key,
            request: retry_request,
            result: Ok(None),
        }));
        assert_eq!(app.account_ui.logins.len(), 1);
        assert_eq!(phase(&app, other), LoginPhase::CallbackBlocked);
        assert!(app.account_ui.table.add_menu);
        assert_eq!(app.durable.cards.len(), cards);
        drop((held, dismissal_held, cleanup_held, retry_held));
    }

    #[test]
    fn home_callbacks_keep_the_same_request_and_consumed_login_custody() {
        for remote_completion in [false, true] {
            let mut app = recovery_app();
            let key = app.account_ui.logins[0].key;
            let code = click_login_control(&app.account_ui.logins[0], "Use code");
            let held = app.update_account_ui(code.clone());
            let request = preparation(&app, key);
            // Both completion adapters are component inputs. Neither local
            // discovery nor remote Create/service authentication is executed.
            let failure = if remote_completion {
                Action::RemoteHomeReady {
                    request,
                    result: Err("captured remote preparation failure".into()),
                }
            } else {
                Action::HomeReady {
                    request,
                    result: Err("captured local preparation failure".into()),
                }
            };
            drop(app.update_account_ui(failure));
            assert_eq!(phase(&app, key), LoginPhase::CallbackBlocked);
            assert_eq!(
                app.status.as_ref().unwrap().key,
                Some(NoticeKey::SignIn(key))
            );
            let retry = click_login_control(&app.account_ui.logins[0], "Use code");
            let retry_held = app.update_account_ui(retry);
            let retry_request = preparation(&app, key);
            replacement_popup(&mut app);
            let ready = blocked_login(195405);
            let recipe = login_launch(
                &ready.account(),
                Path::new("/fixture/codex"),
                PathBuf::from("/fixture/reader"),
                true,
            )
            .unwrap();
            let success = |request| {
                if remote_completion {
                    Action::RemoteHomeReady {
                        request,
                        result: Ok(crate::account::remote::Created {
                            key: ready.key,
                            home: ready.home.clone(),
                            recipe: recipe.clone(),
                        }),
                    }
                } else {
                    Action::HomeReady {
                        request,
                        result: Ok((
                            ready.key,
                            ready.home.clone(),
                            PathBuf::from("/fixture/codex"),
                        )),
                    }
                }
            };
            let cards = app.durable.cards.len();
            assert_eq!(app.update_account_ui(success(request)).units(), 0);
            assert_eq!(preparation(&app, key), retry_request);
            assert_eq!(app.durable.cards.len(), cards);
            assert!(app.account_ui.table.add_menu);
            let launch_held = app.update_account_ui(success(retry_request));
            assert_eq!(phase(&app, key), LoginPhase::CodeStarted);
            assert!(!app.notice_stands(NoticeKey::SignIn(key)));
            assert_eq!(phase(&app, ready.key), LoginPhase::Waiting);
            assert_eq!(app.durable.cards.len(), cards + 1);
            assert!(app.account_ui.home_requests.is_empty());
            assert!(app.account_ui.table.add_menu);
            let card = app.sign_in_card(Provider::Codex, &ready.home).unwrap();
            let CardKind::Terminal(session) = &app.card_state(card).unwrap().kind else {
                panic!("component completion must prepare a sign-in terminal");
            };
            assert_eq!(session.launch.arguments, ["login", "--device-auth"]);
            assert_eq!(
                session.launch.environment["CODEX_HOME"],
                ready.home.display().to_string()
            );
            assert_eq!(app.update_account_ui(code).units(), 0);
            assert_eq!(app.update_account_ui(success(retry_request)).units(), 0);
            assert_eq!(app.durable.cards.len(), cards + 1);
            let dismiss = click_login_control(
                app.account_ui
                    .logins
                    .iter()
                    .find(|login| login.key == key)
                    .unwrap(),
                "Dismiss failed sign-in",
            );
            let dismiss_held = app.update_account_ui(dismiss);
            assert!(matches!(phase(&app, key), LoginPhase::Dismissing(_)));
            assert_eq!(phase(&app, ready.key), LoginPhase::Waiting);
            drop((held, retry_held, launch_held, dismiss_held));
        }
    }

    #[test]
    fn a_blocked_code_callback_requires_its_current_login_provider_and_phase() {
        let mut app = recovery_app();
        let code = click_login_control(&app.account_ui.logins[0], "Use code");
        let original = app.account_ui.logins[0].clone();
        let cards = app.durable.cards.len();
        for replacement in [
            blocked_login(195499),
            Login {
                provider: Provider::Claude,
                ..original.clone()
            },
            Login {
                phase: LoginPhase::Waiting,
                ..original.clone()
            },
        ] {
            app.account_ui.logins = vec![replacement];
            replacement_popup(&mut app);
            assert_eq!(app.update_account_ui(code.clone()).units(), 0);
            assert!(app.account_ui.home_requests.is_empty());
            assert!(app.account_ui.table.add_menu);
            assert_eq!(app.durable.cards.len(), cards);
        }
    }

    #[test]
    fn a_sign_in_waiting_to_be_asked_again_no_longer_points_at_its_terminal() {
        let mut login = Login {
            provider: Provider::Codex,
            key: uuid::Uuid::from_u128(1),
            home: "/cagents-fixture/accounts/codex/1".into(),
            phase: LoginPhase::Waiting,
            attempts: 0,
            next_check: 0,
            started_at: 0,
            recovered: false,
        };
        assert!(login_caption(&login).starts_with("Finish signing in in the terminal"));
        // Between asks the phase is `Waiting` again, with the terminal ended.
        login.attempts = 1;
        assert_eq!(login_caption(&login), "Checking which account signed in…");
    }

    #[test]
    fn a_narrowing_card_drops_the_least_useful_columns_first() {
        let wide = layout(1_400.0);
        assert_eq!(wide.columns, Column::ORDER.to_vec());
        assert_eq!(wide.usage, USAGE_WIDTH);

        let mut previous = wide.columns.len();
        let mut dropped = Vec::new();
        for width in (300..1_400).rev().step_by(10) {
            let layout = layout(width as f32);
            assert!(layout.columns.len() <= previous, "{width}");
            previous = layout.columns.len();
            for column in Column::ORDER {
                if !layout.columns.contains(&column) && !dropped.contains(&column) {
                    dropped.push(column);
                }
            }
        }
        assert_eq!(dropped, Column::DROP_ORDER.to_vec());
        assert!(!wide.stacked && !wide.compact_toolbar);
        let narrow = layout(520.0);
        assert_eq!(narrow.columns, [Column::Session, Column::Weekly]);
        assert_eq!(narrow.usage, NARROW_USAGE_WIDTH);
        assert!(!narrow.stacked && narrow.compact_toolbar);
        // Never a row with no room for the account's name: its figures
        // move under it instead.
        let stacked = layout(360.0);
        assert_eq!(stacked.columns, [Column::Session, Column::Weekly]);
        assert!(stacked.stacked);
    }

    fn account_with(provider: Provider, key: u128, left: Option<(f32, i64)>) -> AccountState {
        let account = if key == 0 {
            Account::system(provider)
        } else {
            Account::managed(
                provider,
                uuid::Uuid::from_u128(key),
                PathBuf::from(format!("/accounts/{key}")),
            )
        };
        let mut state = AccountState::new(account);
        state.present = true;
        state.snapshot = left.map(|(used, resets_at)| {
            let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "", 1_000);
            snapshot.detail = None;
            snapshot.windows = vec![UsageWindow {
                key: "session".into(),
                kind: account::WindowKind::Session,
                label: "Session".into(),
                used,
                resets_at: Some(resets_at),
                span_secs: None,
                binding: false,
            }];
            snapshot
        });
        state
    }

    #[test]
    fn rows_sort_by_headroom_reset_or_name_with_unknowns_last() {
        let mut state = State::default();
        state.accounts = vec![
            account_with(Provider::Claude, 0, None),
            account_with(Provider::Claude, 1, Some((0.9, 500))),
            account_with(Provider::Claude, 2, Some((0.2, 900))),
            account_with(Provider::Claude, 3, Some((0.5, 100))),
        ];
        let keys = |sort| {
            rows(&state, Provider::Claude, sort)
                .iter()
                .map(|row| row.account.registry_key().map_or(0, |key| key.as_u128()))
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(Sort::Headroom), [2, 3, 1, 0]);
        assert_eq!(keys(Sort::Reset), [3, 1, 2, 0]);
        // Every managed account is "Claude account" until it is named or
        // its address is known; the system account is "System default".
        assert_eq!(keys(Sort::Name)[0], 1);
        assert_eq!(*keys(Sort::Name).last().unwrap(), 0);
    }

    #[test]
    fn an_absent_system_account_is_not_listed_but_a_managed_one_always_is() {
        let mut state = State::default();
        let mut system = account_with(Provider::Codex, 0, None);
        system.present = false;
        let mut managed = account_with(Provider::Codex, 7, None);
        managed.present = false;
        state.accounts = vec![system, managed];
        let listed = rows(&state, Provider::Codex, Sort::Name);
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].account.is_system());
    }

    #[test]
    fn escape_closes_a_menu_before_a_rename() {
        let id = Account::system(Provider::Claude).id();
        let mut table = Table {
            menu: Some(id.clone()),
            rename: Some((id.clone(), "Work".into())),
            ..Table::default()
        };
        table.escape();
        assert_eq!(table.menu, None);
        assert!(table.rename.is_some());
        table.escape();
        assert_eq!(table.rename, None);
        assert!(!table.has_popup());
    }
}
