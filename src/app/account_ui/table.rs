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
    let stale =
        account.failure.is_some() || snapshot.is_some_and(|snapshot| is_stale(snapshot, now));
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
    let content = if layout.stacked {
        stacked_row(
            account_cell(app, account, project),
            layout.columns.iter().copied().zip(cells).collect(),
            actions(app, account, now, project),
        )
    } else {
        line(
            account_cell(app, account, project),
            cells,
            actions(app, account, now, project),
            ROW_HEIGHT,
        )
    };
    // Keep the account and its controls in the same child slot when a
    // background check fails. The explanation can wrap below the fixed-height
    // row, including when the Status column has been dropped for space.
    let mut lines = vec![content];
    if let Some(reason) = failure_caption(account) {
        lines.push(
            widget::container(
                design::caption(reason)
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .wrapping(cosmic::iced::widget::text::Wrapping::Word),
            )
            .padding(Padding {
                top: 0.0,
                right: space::M,
                bottom: space::M,
                left: space::M,
            })
            .width(Length::Fill)
            .into(),
        );
    }
    widget::column::with_children(lines)
        .width(Length::Fill)
        .into()
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
    if account.failure.is_some() {
        return ("Check failed", design::warning_color);
    }
    let Some(snapshot) = &account.snapshot else {
        return ("Checking", design::muted_color);
    };
    match snapshot.status {
        UsageStatus::Ok => ("Available", design::success_color),
        UsageStatus::Warning => ("Near limit", design::warning_color),
        UsageStatus::Limited => ("Limited", design::destructive_color),
        UsageStatus::NeedsLogin => ("Signed out", design::warning_color),
        UsageStatus::Unavailable => ("Unavailable", design::muted_color),
    }
}

/// A failed read does not invalidate the last figures, but they no longer
/// describe the outcome of the most recent check.
fn failure_caption(account: &AccountState) -> Option<String> {
    let failure = account.failure.as_ref()?;
    // Remote transport diagnostics can be longer than the probe's own fixed
    // summaries. Bound the always-visible text without copying the full
    // diagnostic; the existing tooltip still carries it in full.
    let mut chars = failure.chars();
    let mut detail: String = chars
        .by_ref()
        .take(240)
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    if chars.next().is_some() {
        detail.push('…');
    } else {
        detail.truncate(detail.trim_end().len());
    }
    if !matches!(detail.chars().last(), Some('.' | '!' | '?' | '…')) {
        detail.push('.');
    }
    let figures = if account.snapshot.as_ref().is_some_and(|snapshot| {
        matches!(
            snapshot.status,
            UsageStatus::Ok | UsageStatus::Warning | UsageStatus::Limited
        )
    }) {
        " Showing last known usage."
    } else {
        ""
    };
    Some(format!(
        "Last check failed: {detail}{figures} Refresh to try again."
    ))
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
                "{count} open {} this home. The daemon refuses to forget it while a hosted process uses it.",
                if count == 1 { "card uses" } else { "cards use" }
            )
        } else {
            format!(
                "{count} open {} this account. Its processes, login and files are kept; managed account removal is unavailable.",
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
            design::body(if account.account.is_linked() {
                format!("Forget {}?", state.display_name(account))
            } else {
                "Managed account removal is unavailable".into()
            })
            .into(),
            design::caption(if account.account.is_linked() {
                "This only removes the home from cagents. Its login, sessions and files stay where they are.".to_owned()
            } else {
                "This build keeps this account registered, with its login, conversations and files unchanged. It cannot safely delete a managed account.".to_owned()
            })
            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
            .into(),
            in_use_warning(state.table.confirm_in_use, account.account.is_linked()),
            widget::row::with_children(vec![
                iced::widget::Space::new().width(Length::Fill).into(),
                crate::ui_controls::button::secondary("Cancel")
                    .on_press(action(Action::RemoveCancel))
                    .into(),
                crate::ui_controls::button::destructive(if account.account.is_linked() {
                    "Forget home"
                } else {
                    "Removal unavailable"
                })
                    .on_press_maybe(account.account.is_linked().then(|| action(Action::RemoveConfirm(id.clone()))))
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
                    if account.account.is_linked() { "Forget home" } else { "Removal unavailable" },
                    if account.account.is_linked() {
                        "Forget this home without changing its files"
                    } else {
                        "Managed account removal is unavailable; its login, conversations and files are kept"
                    },
                    action(Action::Remove(id.clone())),
                )
                .on_press_maybe(account.account.is_linked().then(|| action(Action::Remove(id.clone()))))
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
    }
}

fn login_row<'a>(login: &'a Login) -> Element<'a, Message> {
    let phase = login_caption(login);
    let account = widget::row::with_children(vec![
        if login.phase == LoginPhase::CallbackBlocked {
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
    let cancel: Element<'a, Message> = if login.phase == LoginPhase::CallbackBlocked {
        widget::row::with_children(vec![
            crate::ui_controls::button::quiet("Use code")
                .on_press(Message::AccountUi(Action::Add {
                    provider: Provider::Codex,
                    device: true,
                }))
                .into(),
            crate::ui_controls::icon_button(Icon::Close, "Dismiss failed sign-in")
                .compact()
                .on_press(Message::AccountUi(Action::CancelLogin(login.key)))
                .into(),
        ])
        .into()
    } else if login.phase == LoginPhase::Waiting {
        crate::ui_controls::icon_button(Icon::Close, "Cancel sign-in")
            .compact()
            .on_press(Message::AccountUi(Action::CancelLogin(login.key)))
            .into()
    } else {
        iced::widget::Space::new().into()
    };
    line(account, Vec::new(), cancel, ROW_HEIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_failed_check_keeps_usage_and_explains_it_in_wide_and_narrow_rows() {
        use iced::advanced::{
            Layout as WidgetLayout, layout,
            widget::{Operation, Tree},
        };

        #[derive(Default)]
        struct Texts(Vec<(String, iced::Rectangle)>);
        impl Operation for Texts {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
                self.0.push((text.to_owned(), bounds));
            }
        }
        let renderer = crate::app::tests::headless_renderer();
        let measure = |app: &Cagents, id: &AccountId, width: f32| {
            let mut element = row(
                app,
                app.account_ui.account(id).unwrap(),
                &layout(width),
                None,
            );
            let mut tree = Tree::new(element.as_widget());
            let available = width.min(MAX_WIDTH) - 2.0 * GUTTER;
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(available, 900.0)),
            );
            let mut texts = Texts::default();
            element.as_widget_mut().operate(
                &mut tree,
                WidgetLayout::new(&node),
                &renderer,
                &mut texts,
            );
            (texts.0, node.size(), node.children()[0].bounds())
        };
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let id = app.account_ui.add_managed_fixture(Provider::Codex, 1167);
        let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "", app.account_ui.now_ms);
        snapshot.detail = None;
        snapshot.windows = vec![UsageWindow {
            key: "session".into(),
            kind: account::WindowKind::Session,
            label: "Session".into(),
            used: 0.4,
            resets_at: None,
            span_secs: None,
            binding: false,
        }];
        let owner = app.account_ui.account(&id).unwrap().owner;
        // Exercise accepted probe completions; no returned background Task is polled.
        let _ = app.update(Message::AccountUi(Action::Probed {
            owner,
            id: id.clone(),
            outcome: Outcome::Answered {
                identity: None,
                catalog: None,
                snapshot: snapshot.clone(),
            },
        }));
        let widths = [1_400.0, 1_100.0, 520.0, 360.0];
        let healthy = widths.map(|width| measure(&app, &id, width));
        assert!(healthy[0].0.iter().any(|(text, _)| text == "Available"));
        assert!(healthy.iter().all(|(texts, _, _)| {
            texts.iter().any(|(text, _)| text == "60% left")
                && !texts
                    .iter()
                    .any(|(text, _)| text.starts_with("Last check failed:"))
        }));
        let failure = "Codex did not answer within 30 seconds";
        let _ = app.update(Message::AccountUi(Action::Probed {
            owner,
            id: id.clone(),
            outcome: Outcome::Failed(failure.into()),
        }));
        assert_eq!(
            app.account_ui.account(&id).unwrap().snapshot.as_ref(),
            Some(&snapshot)
        );
        let caption = "Last check failed: Codex did not answer within 30 seconds. Showing last known usage. Refresh to try again.";
        let mut wide_caption_height = None;
        for (index, width) in widths.into_iter().enumerate() {
            let (texts, size, controls) = measure(&app, &id, width);
            assert_eq!(
                controls, healthy[index].2,
                "existing controls stay in child zero"
            );
            assert!(texts.iter().any(|(text, _)| text == "60% left"));
            assert!(!texts.iter().any(|(text, _)| text == "Available"));
            assert_eq!(
                texts.iter().any(|(text, _)| text == "Check failed"),
                layout(width).columns.contains(&Column::Status),
            );
            let matches = texts
                .iter()
                .filter(|(text, _)| text == caption)
                .collect::<Vec<_>>();
            assert_eq!(
                matches.len(),
                1,
                "reason must be rendered, not tooltip-only: {width}"
            );
            let bounds = matches[0].1;
            assert!(bounds.width.is_finite() && bounds.height.is_finite());
            assert!(bounds.width > 0.0 && bounds.height > 0.0);
            assert!(bounds.y >= controls.y + controls.height);
            assert!(bounds.x >= 0.0 && bounds.x + bounds.width <= size.width + 0.1);
            assert!(bounds.y + bounds.height <= size.height + 0.1);
            assert!(size.height > healthy[index].1.height);
            if index == 0 {
                wide_caption_height = Some(bounds.height);
            }
            if width == 360.0 {
                assert!(
                    bounds.height > wide_caption_height.unwrap(),
                    "narrow reason wraps"
                );
                assert!(!layout(width).columns.contains(&Column::Status));
            }
        }
        let _ = app.update(Message::AccountUi(Action::Probed {
            owner,
            id: id.clone(),
            outcome: Outcome::Answered {
                identity: None,
                catalog: None,
                snapshot: snapshot.clone(),
            },
        }));
        assert_eq!(
            app.account_ui.account(&id).unwrap().snapshot.as_ref(),
            Some(&snapshot)
        );
        for (index, width) in widths.into_iter().enumerate() {
            let (texts, size, controls) = measure(&app, &id, width);
            assert_eq!((size, controls), (healthy[index].1, healthy[index].2));
            assert!(texts.iter().any(|(text, _)| text == "60% left"));
            assert!(
                !texts
                    .iter()
                    .any(|(text, _)| text.starts_with("Last check failed:"))
            );
            assert!(!texts.iter().any(|(text, _)| text == "Check failed"));
            if layout(width).columns.contains(&Column::Status) {
                assert!(texts.iter().any(|(text, _)| text == "Available"));
            }
        }
        // Long transport diagnostics cannot turn the visible row into an
        // unbounded body. Unicode stays intact and guidance is outside the cap.
        let long_failure = "界🙂é ".repeat(4_000);
        let _ = app.update(Message::AccountUi(Action::Probed {
            owner,
            id: id.clone(),
            outcome: Outcome::Failed(long_failure.clone()),
        }));
        let state = app.account_ui.account(&id).unwrap();
        assert_eq!(state.failure.as_ref(), Some(&long_failure));
        assert_eq!(state.snapshot.as_ref(), Some(&snapshot));
        // status_badge's existing tooltip still formats this whole state.failure.
        let expected = format!(
            "Last check failed: {}… Showing last known usage. Refresh to try again.",
            "界🙂é ".repeat(60),
        );
        let (texts, size, controls) = measure(&app, &id, 360.0);
        let captions = texts
            .iter()
            .filter(|(text, _)| text.starts_with("Last check failed:"))
            .collect::<Vec<_>>();
        assert_eq!(captions.len(), 1);
        assert_eq!(captions[0].0, expected);
        assert!(captions[0].0.chars().count() < 340);
        assert!(captions[0].1.height.is_finite() && captions[0].1.height > 0.0);
        assert!(captions[0].1.x + captions[0].1.width <= size.width + 0.1);
        assert!(captions[0].1.y + captions[0].1.height <= size.height + 0.1);
        assert_eq!(controls, healthy[3].2);
        // Existing sentence punctuation is not duplicated by the caption.
        let _ = app.update(Message::AccountUi(Action::Probed {
            owner,
            id: id.clone(),
            outcome: Outcome::Failed("Codex could not read usage just now.".into()),
        }));
        let (texts, _, _) = measure(&app, &id, 520.0);
        assert!(texts.iter().any(|(text, _)| text ==
            "Last check failed: Codex could not read usage just now. Showing last known usage. Refresh to try again."));
        // A first failed check explains the failure without claiming cached figures.
        let unchecked = app.account_ui.add_managed_fixture(Provider::Codex, 1168);
        let _ = app.update(Message::AccountUi(Action::Probed {
            owner: app.account_ui.account(&unchecked).unwrap().owner,
            id: unchecked.clone(),
            outcome: Outcome::Failed(failure.into()),
        }));
        let (texts, _, _) = measure(&app, &unchecked, 360.0);
        assert!(texts.iter().any(|(text, _)| text
            == "Last check failed: Codex did not answer within 30 seconds. Refresh to try again."));
        assert!(
            !texts
                .iter()
                .any(|(text, _)| text.contains("last known usage"))
        );
    }

    #[test]
    fn removal_menus_keep_managed_files_and_only_allow_linked_forget() {
        use iced::advanced::{clipboard, widget::Operation};
        use iced::runtime::{UserInterface, user_interface::Cache};
        use iced::{Event, Point, Rectangle, Size, mouse};

        #[derive(Default)]
        struct Texts(Vec<(String, Rectangle)>);
        impl Operation for Texts {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, bounds: Rectangle, text: &str) {
                self.0.push((text.into(), bounds));
            }
        }
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let managed = app.account_ui.add_managed_fixture(Provider::Codex, 1087);
        let directory = tempfile::tempdir().unwrap();
        let linked_home = directory.path().join("linked");
        std::fs::create_dir(&linked_home).unwrap();
        let linked_key = uuid::Uuid::new_v4();
        let mut registry = app.account_ui.registry.clone();
        registry
            .link(
                Provider::Codex,
                linked_key,
                linked_home.clone(),
                None,
                app.account_ui.homes.as_ref(),
                1,
            )
            .unwrap();
        app.account_ui.set_registry(registry);
        let linked = Account::linked(Provider::Codex, linked_key, linked_home).id();
        let mut renderer = crate::app::tests::headless_renderer();
        for width in [700.0, 320.0] {
            for (id, enabled) in [(managed.clone(), false), (linked.clone(), true)] {
                app.account_ui.table.confirm_remove = Some(id.clone());
                app.account_ui.table.confirm_in_use = 1;
                let account = app.account_ui.account(&id).unwrap();
                let element = menu(&app, account, None);
                let mut ui = UserInterface::build(
                    element,
                    Size::new(width, 600.0),
                    Cache::new(),
                    &mut renderer,
                );
                let mut texts = Texts::default();
                ui.operate(&renderer, &mut texts);
                let button = if enabled {
                    "Forget home"
                } else {
                    "Removal unavailable"
                };
                assert!(!texts.0.iter().any(|(text, _)| text.contains("Resuming saved conversations will require")));
                let caption = if enabled {
                    "This only removes the home from cagents. Its login, sessions and files stay where they are."
                } else {
                    "This build keeps this account registered, with its login, conversations and files unchanged. It cannot safely delete a managed account."
                };
                let bounds = texts
                    .0
                    .iter()
                    .find(|(text, _)| text == caption)
                    .expect("visible truthful caption")
                    .1;
                assert!(bounds.width > 0.0 && bounds.height > 0.0 && bounds.height.is_finite());
                assert!(bounds.x >= 0.0 && bounds.x + bounds.width <= width + 0.1);
                assert!(bounds.y + bounds.height < 600.0);
                let target = texts
                    .0
                    .iter()
                    .find(|(text, _)| text == button)
                    .expect("button label")
                    .1;
                let point = Point::new(target.center_x(), target.center_y());
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
                if enabled {
                    assert!(
                        matches!(messages.as_slice(), [Message::AccountUi(Action::RemoveConfirm(received))] if *received == id)
                    );
                } else {
                    assert!(
                        messages.is_empty(),
                        "managed removal has no native callback"
                    );
                }
            }
        }
    }
}
