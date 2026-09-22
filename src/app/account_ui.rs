//! Account usage: the status bar's per-provider chips, the popover each one
//! opens, and the Accounts pane.
//!
//! Figures come from [`crate::account::probe`], which runs each provider's
//! own CLI from an async task and does its file work on the blocking pool;
//! nothing here touches a process or a file on the UI thread. This module
//! only decides when to ask, through [`crate::account::schedule`], and draws
//! whatever came back.

use std::collections::BTreeMap;

use super::*;
use crate::account::{
    self, Account, AccountId, Identity, UsageSnapshot, UsageSource, UsageStatus, UsageWindow,
    cache::{Cache, Entry},
    probe::{self, Outcome},
    schedule::{self, Millis, Schedule, Trigger},
};

/// How often a visible, focused window re-reads the clock and asks the
/// schedule whether an account is due.
const TICK: Duration = Duration::from_secs(30);
const STATUS_BAR_HEIGHT: f32 = 24.0;
const CHIP_HEIGHT: f32 = 20.0;
const CHIP_ICON_SIZE: u16 = 12;
const POPOVER_WIDTH: f32 = 320.0;
/// Below this window width a chip keeps its glyph and ring and drops its
/// words, so the bar never pushes against the window edge.
const COMPACT_WIDTH: f32 = design::OVERLAY_DOCK_BREAKPOINT;

pub(super) struct State {
    accounts: Vec<AccountState>,
    /// The clock as of the last event that read it. Views format against
    /// this rather than reading the clock themselves, so a frame is a pure
    /// function of the state and a test can state the time.
    now_ms: Millis,
    popover: Option<AccountId>,
    cache: Option<Cache>,
}

impl State {
    /// The state of a real window, sharing figures with every other window
    /// of this installation through the cache in its state directory.
    pub(super) fn installation() -> Self {
        Self::new(Cache::installation())
    }

    fn new(cache: Option<Cache>) -> Self {
        Self {
            accounts: Account::known()
                .into_iter()
                .map(AccountState::new)
                .collect(),
            now_ms: now_ms(),
            popover: None,
            cache,
        }
    }

    fn account(&self, id: &AccountId) -> Option<&AccountState> {
        self.accounts.iter().find(|account| &account.id == id)
    }

    fn account_mut(&mut self, id: &AccountId) -> Option<&mut AccountState> {
        self.accounts.iter_mut().find(|account| &account.id == id)
    }
}

/// A window with no cache: nothing it learns outlives it or reaches another
/// window, which is what a test fixture wants.
impl Default for State {
    fn default() -> Self {
        Self::new(None)
    }
}

struct AccountState {
    account: Account,
    id: AccountId,
    /// Whether the account has anything to report: its CLI is installed and
    /// its home exists. An absent account gets no chip, because most people
    /// use one of the two CLIs rather than both.
    present: bool,
    identity: Option<Identity>,
    snapshot: Option<UsageSnapshot>,
    schedule: Schedule,
    /// Why the latest probe produced nothing, while the figures on screen
    /// are older than that attempt.
    failure: Option<String>,
}

impl AccountState {
    fn new(account: Account) -> Self {
        Self {
            id: account.id(),
            account,
            present: false,
            identity: None,
            snapshot: None,
            schedule: Schedule::default(),
            failure: None,
        }
    }

    fn name(&self) -> &'static str {
        self.account.provider.label()
    }

    fn plan(&self) -> Option<String> {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.plan.as_deref())
            .or_else(|| self.identity.as_ref()?.plan.as_deref())
            .map(account::plan_label)
    }
}

/// What the startup load found for one account.
#[derive(Debug, Clone)]
pub struct Restored {
    id: AccountId,
    present: bool,
    entry: Option<Entry>,
}

#[derive(Debug, Clone)]
pub enum Action {
    Tick,
    Loaded(Vec<Restored>),
    Probed {
        id: AccountId,
        outcome: Outcome,
    },
    Refresh(AccountId),
    /// Opens the popover for an account, or closes it with `None` or by
    /// naming the account whose popover is already open.
    Popover(Option<AccountId>),
    OpenPane,
    SignIn(AccountId),
    /// The CLI to sign in with was looked for; `None` when it is missing.
    LoginReady {
        id: AccountId,
        program: Option<PathBuf>,
    },
}

impl Action {
    /// Whether this is the reader acting on the usage surfaces, which a
    /// dialog in front of them must stop. Closing the popover never is.
    pub(super) fn blocked_while_modal(&self) -> bool {
        matches!(
            self,
            Self::Refresh(_) | Self::Popover(Some(_)) | Self::OpenPane | Self::SignIn(_)
        )
    }
}

fn now_ms() -> Millis {
    crate::chat::now_ms().unwrap_or_default()
}

fn restore(accounts: &[Account], cache: Option<&Cache>, now: Millis) -> Vec<Restored> {
    let mut entries = cache.map(|cache| cache.load(now)).unwrap_or_default();
    accounts
        .iter()
        .map(|account| Restored {
            id: account.id(),
            present: account.resolved_home().is_some_and(|home| home.is_dir())
                && probe::find_executable(account.provider.executable()).is_some(),
            entry: account.cache_key().and_then(|key| entries.remove(&key)),
        })
        .collect()
}

/// Whether figures observed at `observed_at` still describe the account at
/// `now`: they are recent, and no window has reset since they were read.
fn is_stale(snapshot: &UsageSnapshot, now: Millis) -> bool {
    if now.saturating_sub(snapshot.observed_at) > schedule::DISPLAY_STALE_AFTER {
        return true;
    }
    reset_since(snapshot, now)
}

/// Whether a window reset between the observation and `now`, which makes
/// its figure wrong however recent the observation is.
fn reset_since(snapshot: &UsageSnapshot, now: Millis) -> bool {
    snapshot.windows.iter().any(|window| {
        window
            .resets_at
            .and_then(|seconds| u64::try_from(seconds).ok())
            .map(|seconds| seconds.saturating_mul(1_000))
            .is_some_and(|reset| reset > snapshot.observed_at && reset <= now)
    })
}

/// The whole percentage left in a window, rounded toward caution: a window
/// 42.4% used has 57% left, not 58%.
fn percent_left(used: f32) -> u32 {
    // Float noise must not turn an exact 43% into 44% used.
    let used = (used * 100.0 - 0.001).ceil().clamp(0.0, 100.0);
    (100.0 - used) as u32
}

fn unix_seconds(now: Millis) -> i64 {
    i64::try_from(now / 1_000).unwrap_or(i64::MAX)
}

/// When a window resets, as the popover states it.
fn reset_line(window: &UsageWindow, now: Millis) -> Option<String> {
    let resets_at = window.resets_at?;
    let now_seconds = unix_seconds(now);
    let moment = account::format_local_moment(resets_at, now_seconds);
    Some(if resets_at <= now_seconds {
        format!("Reset at {moment}; refresh for current figures")
    } else {
        format!(
            "Resets in {} · {moment}",
            account::format_remaining(resets_at - now_seconds)
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChipTone {
    Current,
    /// Figures the reader should not plan around: old, or from before a
    /// reset.
    Stale,
    /// Something the reader has to do.
    Attention,
}

/// What an account's chip says.
#[derive(Debug, Clone, PartialEq)]
struct Chip {
    text: String,
    /// The used fraction of the binding window, drawn as the ring.
    gauge: Option<f32>,
    tone: ChipTone,
    /// Kept when the bar is too narrow for the rest of the text.
    essential: bool,
    tooltip: String,
}

/// The window a chip leads with: the one binding the account, unless that is
/// one model's allowance - running out of it leaves every other model
/// working - in which case the tightest account-wide window.
fn headline_window(snapshot: &UsageSnapshot) -> Option<&UsageWindow> {
    let binding = snapshot.binding_window()?;
    if binding.kind != account::WindowKind::Scoped {
        return Some(binding);
    }
    Some(
        account::tightest(
            snapshot
                .windows
                .iter()
                .filter(|window| window.kind != account::WindowKind::Scoped),
        )
        .unwrap_or(binding),
    )
}

fn chip(account: &AccountState, now: Millis) -> Chip {
    let name = account.name();
    let Some(snapshot) = &account.snapshot else {
        return match &account.failure {
            Some(failure) => Chip {
                text: "Unavailable".into(),
                gauge: None,
                tone: ChipTone::Stale,
                essential: false,
                tooltip: format!("{name} usage could not be read: {failure}"),
            },
            None => Chip {
                text: "Checking…".into(),
                gauge: None,
                tone: ChipTone::Stale,
                essential: false,
                tooltip: format!("Checking {name} usage…"),
            },
        };
    };
    match snapshot.status {
        UsageStatus::NeedsLogin => Chip {
            text: "Sign in".into(),
            gauge: None,
            tone: ChipTone::Attention,
            essential: true,
            tooltip: format!("{name} is signed out. Sign in to see its usage."),
        },
        UsageStatus::Unavailable => Chip {
            text: "Unavailable".into(),
            gauge: None,
            tone: ChipTone::Stale,
            essential: false,
            tooltip: snapshot
                .detail
                .clone()
                .unwrap_or_else(|| format!("{name} reports no usage")),
        },
        UsageStatus::Ok | UsageStatus::Warning | UsageStatus::Limited => {
            let Some(window) = headline_window(snapshot) else {
                return Chip {
                    text: "No limits".into(),
                    gauge: None,
                    tone: ChipTone::Current,
                    essential: false,
                    tooltip: format!("{name} reports no usage limits for this account"),
                };
            };
            let stale = is_stale(snapshot, now);
            let left = percent_left(window.used);
            let now_seconds = unix_seconds(now);
            let remaining = window
                .resets_at
                .filter(|reset| *reset > now_seconds)
                .map(|reset| account::format_remaining(reset - now_seconds));
            let as_of = stale.then(|| {
                format!(
                    "as of {}",
                    account::format_local_moment(unix_seconds(snapshot.observed_at), now_seconds)
                )
            });
            let mut text = format!("{left}% left");
            if let Some(when) = as_of.as_ref().or(remaining.as_ref()) {
                text.push_str(" · ");
                text.push_str(when);
            }
            let mut tooltip = format!("{name} · {}: {left}% left", window.label);
            if snapshot.status == UsageStatus::Limited {
                tooltip.push_str(", limited");
            }
            if let Some(remaining) = &remaining {
                tooltip.push_str(&format!(", resets in {remaining}"));
            }
            if let Some(as_of) = &as_of {
                tooltip.push_str(&format!(" ({as_of})"));
            }
            // A model's own allowance binding is worth knowing, but it is not
            // what the account as a whole has left.
            if let Some(scoped) = snapshot
                .binding_window()
                .filter(|binding| binding.key != window.key)
            {
                tooltip.push_str(&format!(
                    "\n{}: {}% left",
                    scoped.label,
                    percent_left(scoped.used)
                ));
            }
            Chip {
                text,
                gauge: Some(window.used),
                tone: if stale {
                    ChipTone::Stale
                } else {
                    ChipTone::Current
                },
                essential: false,
                tooltip,
            }
        }
    }
}

/// The terminal that runs `account`'s login command: the CLI at `program`,
/// the same executable the probes run, started in the reader's home - not in
/// a project, whose own CLI settings have no business in a login.
///
/// `auth` and `login` are account management, which the launch recognizer
/// does not take for an agent conversation.
fn login_launch(account: &Account, program: &Path, home: PathBuf) -> Option<LaunchRecipe> {
    let command: &[&str] = match account.provider {
        crate::agent::Provider::Claude => &["auth", "login"],
        crate::agent::Provider::Codex => &["login"],
        crate::agent::Provider::Omp => return None,
    };
    let environment: BTreeMap<String, String> = account
        .probe_environment()
        .map(|(variable, home)| (variable.to_owned(), home.display().to_string()))
        .into_iter()
        .collect();
    Some(LaunchRecipe {
        program: program.to_str()?.to_owned(),
        arguments: command
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        working_directory: home,
        environment,
    })
}

/// Whether `recipe` runs a provider's sign-in, as [`login_launch`] builds
/// it or as a reader types it.
pub(super) fn is_sign_in(recipe: &LaunchRecipe) -> bool {
    let program = Path::new(&recipe.program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let arguments: Vec<&str> = recipe.arguments.iter().map(String::as_str).collect();
    matches!(
        (program, arguments.as_slice()),
        ("claude", ["auth", "login"]) | ("codex", ["login"])
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Placement {
    Popover,
    Pane,
}

impl Cagents {
    /// Whether anything shows account usage, which is the only reason to
    /// ask for it.
    fn accounts_surface_visible(&self) -> bool {
        self.settings.show_status_bar || self.utility_card(&CardKind::Accounts).is_some()
    }

    /// Ticks while anything shows account usage. Unfocused, a tick only
    /// moves the clock the figures are read against - "resets in", "updated
    /// ... ago" - since [`Schedule::should_probe`] probes a focused window
    /// alone.
    pub(super) fn account_subscription(&self) -> Subscription<Message> {
        if !self.accounts_surface_visible() {
            return Subscription::none();
        }
        cosmic::iced::time::every(TICK).map(|_| Message::AccountUi(Action::Tick))
    }

    /// Reads what earlier probes, from this window or another, left in the
    /// cache, and which accounts have a CLI to ask.
    pub(super) fn load_accounts(&self) -> Task<Message> {
        let accounts: Vec<Account> = self
            .account_ui
            .accounts
            .iter()
            .map(|account| account.account.clone())
            .collect();
        let cache = self.account_ui.cache.clone();
        cosmic::task::future(async move {
            let restored =
                tokio::task::spawn_blocking(move || restore(&accounts, cache.as_ref(), now_ms()))
                    .await
                    .unwrap_or_default();
            Message::AccountUi(Action::Loaded(restored))
        })
    }

    /// The status bar was shown or hidden, or the Accounts pane opened.
    pub(super) fn accounts_surface_changed(&mut self) -> Task<Message> {
        if !self.settings.show_status_bar {
            self.account_ui.popover = None;
        }
        self.probe_accounts(Trigger::Focus)
    }

    /// Called with the main window's focus changes, after `refresh_ui` has
    /// recorded them; `regained` is whether the window was unfocused before.
    pub(super) fn accounts_focus_changed(
        &mut self,
        focused: bool,
        regained: bool,
    ) -> Task<Message> {
        if !focused {
            // An anchored popup outliving the reader's attention would still
            // be open, over stale figures, when they come back.
            self.account_ui.popover = None;
        }
        if !regained {
            return Task::none();
        }
        self.probe_accounts(Trigger::Focus)
    }

    /// The popover's share of a key event, like the chrome's popups: a
    /// dialog opening closes it, and Escape closes it before anything behind.
    pub(super) fn account_key_event(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        self.account_ui.popover.as_ref()?;
        if self.modal_is_open() {
            self.account_ui.popover = None;
            return None;
        }
        is_escape_press(event).then(|| {
            self.account_ui.popover = None;
            Task::none()
        })
    }

    fn probe_accounts(&mut self, trigger: Trigger) -> Task<Message> {
        let now = now_ms();
        self.account_ui.now_ms = now;
        if !self.accounts_surface_visible() {
            return Task::none();
        }
        let ids: Vec<AccountId> = self
            .account_ui
            .accounts
            .iter()
            .map(|account| account.id.clone())
            .collect();
        Task::batch(
            ids.iter()
                .filter_map(|id| self.probe_account(id, trigger, now)),
        )
    }

    fn probe_account(
        &mut self,
        id: &AccountId,
        trigger: Trigger,
        now: Millis,
    ) -> Option<Task<Message>> {
        let focused = self.refresh_ui.focused;
        let cache = self.account_ui.cache.clone();
        let state = self.account_ui.account_mut(id)?;
        // Figures from before a reset are wrong however recent they are, so
        // they are refreshed as eagerly as on the reader's return.
        let trigger = match trigger {
            Trigger::Tick
                if state
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| reset_since(snapshot, now)) =>
            {
                Trigger::Focus
            }
            trigger => trigger,
        };
        if !state.schedule.should_probe(trigger, now, focused) {
            return None;
        }
        // The cache is only as fresh as the last probe, which is exactly
        // what a reader asking again, or signing in, is trying to get past.
        let force = trigger == Trigger::Manual || state.schedule.awaiting_login(now);
        state.schedule.start(trigger, now);
        let account = state.account.clone();
        let id = state.id.clone();
        Some(cosmic::task::future(async move {
            let outcome = probe::run(account, cache, force).await;
            Message::AccountUi(Action::Probed { id, outcome })
        }))
    }

    pub(super) fn update_account_ui(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Tick => self.probe_accounts(Trigger::Tick),
            Action::Loaded(restored) => {
                for restored in restored {
                    let Some(state) = self.account_ui.account_mut(&restored.id) else {
                        continue;
                    };
                    // A probe that answered first knows better than the load.
                    if state.snapshot.is_some() {
                        continue;
                    }
                    state.present = restored.present;
                    if let Some(entry) = restored.entry.filter(|_| restored.present) {
                        let mut snapshot = entry.snapshot;
                        snapshot.source = UsageSource::Cache;
                        state.schedule.observe(snapshot.observed_at);
                        state.identity = entry.identity;
                        state.snapshot = Some(snapshot);
                    }
                }
                self.probe_accounts(Trigger::Focus)
            }
            Action::Probed { id, outcome } => {
                let now = now_ms();
                self.account_ui.now_ms = now;
                let Some(state) = self.account_ui.account_mut(&id) else {
                    return Task::none();
                };
                match outcome {
                    // Another window's cached answer can be older than what
                    // this window already shows, and then says nothing new.
                    Outcome::Answered { snapshot, .. }
                        if snapshot.source == UsageSource::Cache
                            && state.snapshot.as_ref().is_some_and(|current| {
                                snapshot.observed_at < current.observed_at
                            }) =>
                    {
                        state.schedule.abandon();
                    }
                    // A probe's own answer is always the newest.
                    Outcome::Answered { identity, snapshot } => {
                        state.schedule.succeed(snapshot.observed_at);
                        if snapshot.status != UsageStatus::NeedsLogin {
                            state.schedule.login_settled();
                        }
                        state.present = snapshot.status != UsageStatus::Unavailable;
                        // A signed-out answer, or an identity lookup that failed
                        // this time, names nobody; the account last seen is
                        // still the one it is, or the one to sign back in to.
                        if identity.is_some() || snapshot.status == UsageStatus::Unavailable {
                            state.identity = identity;
                        }
                        state.snapshot = Some(snapshot);
                        state.failure = None;
                    }
                    Outcome::Failed(message) => {
                        state.schedule.fail(now);
                        state.failure = Some(message);
                    }
                    Outcome::Busy => state.schedule.abandon(),
                }
                if !state.present && self.account_ui.popover.as_ref() == Some(&id) {
                    self.account_ui.popover = None;
                }
                Task::none()
            }
            Action::Refresh(id) => {
                let now = now_ms();
                self.account_ui.now_ms = now;
                self.probe_account(&id, Trigger::Manual, now)
                    .unwrap_or_else(Task::none)
            }
            Action::Popover(id) => {
                let id = id.filter(|id| self.account_ui.popover.as_ref() != Some(id));
                self.account_ui.popover = id.clone();
                let now = now_ms();
                self.account_ui.now_ms = now;
                // Opening the popover is the reader looking at the figures.
                id.and_then(|id| self.probe_account(&id, Trigger::Focus, now))
                    .unwrap_or_else(Task::none)
            }
            Action::OpenPane => {
                self.account_ui.popover = None;
                let open = self.open_utility_card(CardKind::Accounts);
                Task::batch([open, self.accounts_surface_changed()])
            }
            Action::SignIn(id) => self.sign_in(id),
            Action::LoginReady { id, program } => self.open_login(&id, program),
        }
    }

    /// Finds the CLI to sign in with off the interface thread, the same way
    /// the probes find it.
    fn sign_in(&mut self, id: AccountId) -> Task<Message> {
        let Some(account) = self
            .account_ui
            .account(&id)
            .map(|state| state.account.clone())
        else {
            return Task::none();
        };
        self.account_ui.popover = None;
        cosmic::task::future(async move {
            let program = tokio::task::spawn_blocking(move || {
                probe::find_executable(account.provider.executable())
            })
            .await
            .ok()
            .flatten();
            Message::AccountUi(Action::LoginReady { id, program })
        })
    }

    fn open_login(&mut self, id: &AccountId, program: Option<PathBuf>) -> Task<Message> {
        let Some(state) = self.account_ui.account(id) else {
            return Task::none();
        };
        let launch = program
            .zip(std::env::var_os("HOME"))
            .and_then(|(program, home)| {
                login_launch(&state.account, &program, PathBuf::from(home))
            });
        let Some(launch) = launch else {
            self.status = Some(TransientNotice::error(format!(
                "{} could not be started to sign in.",
                state.name()
            )));
            return Task::none();
        };
        // A sign-in keeps no transcript: see `is_sign_in`.
        let session = fresh_terminal_session(launch);
        let now = now_ms();
        if let Some(state) = self.account_ui.account_mut(id) {
            state.schedule.await_login(now);
        }
        self.insert_terminal_card(session, None)
    }

    pub(super) fn status_bar(&self) -> Option<Element<'_, Message>> {
        if !self.settings.show_status_bar {
            return None;
        }
        let now = self.account_ui.now_ms;
        let compact = self.window_width < COMPACT_WIDTH;
        let chips: Vec<Element<'_, Message>> = self
            .account_ui
            .accounts
            .iter()
            .filter(|account| account.present)
            .map(|account| self.account_chip(account, now, compact))
            .collect();
        // A bar with nothing to report is only a line across the window.
        if chips.is_empty() {
            return None;
        }
        let bar = widget::container(
            widget::row::with_children(vec![
                iced::widget::Space::new().width(Length::Fill).into(),
                widget::row::with_children(chips)
                    .spacing(design::GRID_UNIT)
                    .align_y(Alignment::Center)
                    .into(),
            ])
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .height(STATUS_BAR_HEIGHT - design::BORDER_WIDTH)
        .padding(Padding::from([0.0, design::GRID_UNIT * 2.0]))
        .align_y(Alignment::Center);
        Some(
            widget::column::with_children(vec![design::horizontal_rule(), bar.into()])
                .width(Length::Fill)
                .into(),
        )
    }

    fn account_chip<'a>(
        &'a self,
        account: &'a AccountState,
        now: Millis,
        compact: bool,
    ) -> Element<'a, Message> {
        let summary = chip(account, now);
        let open = self.account_ui.popover.as_ref() == Some(&account.id) && !self.modal_is_open();
        let mut items: Vec<Element<'a, Message>> = vec![
            widget::icon(crate::ui_controls::provider_icon(account.account.provider))
                .size(CHIP_ICON_SIZE)
                .into(),
        ];
        if let Some(used) = summary.gauge {
            let tone = if summary.tone == ChipTone::Stale {
                design::muted_color
            } else {
                design::budget_tone(used)
            };
            items.push(crate::ui_controls::budget_radial(used, tone));
        }
        if !compact || summary.essential {
            let text =
                design::caption(summary.text).wrapping(cosmic::iced::widget::text::Wrapping::None);
            items.push(
                match summary.tone {
                    ChipTone::Current => text.class(cosmic::theme::Text::Default),
                    ChipTone::Stale => text,
                    ChipTone::Attention => {
                        text.class(cosmic::theme::Text::Custom(design::warning_text))
                    }
                }
                .into(),
            );
        }
        let button = widget::button::custom(
            widget::row::with_children(items)
                .spacing(design::GRID_UNIT * 1.5)
                .align_y(Alignment::Center),
        )
        .name(summary.tooltip.clone())
        .selected(open)
        .class(crate::ui_controls::control_style(open, None))
        .height(CHIP_HEIGHT)
        .padding(Padding::from([0.0, design::GRID_UNIT * 1.5]))
        .on_press(Message::AccountUi(Action::Popover(Some(
            account.id.clone(),
        ))));
        let anchor =
            crate::interaction::tooltip(crate::interaction::hover(button), summary.tooltip);
        crate::ui_controls::popover(
            anchor,
            open.then(|| {
                widget::container(self.account_details(account, now, Placement::Popover))
                    .width(POPOVER_WIDTH)
                    .padding(design::GRID_UNIT)
                    .into()
            }),
            widget::popover::Position::Top,
            Message::AccountUi(Action::Popover(None)),
        )
    }

    /// Everything known about one account: the popover's content, and one
    /// section of the Accounts pane.
    fn account_details<'a>(
        &'a self,
        account: &'a AccountState,
        now: Millis,
        placement: Placement,
    ) -> Element<'a, Message> {
        let spacing = design::GRID_UNIT * 2.0;
        let mut rows: Vec<Element<'a, Message>> =
            vec![self.account_header(account, now, placement)];
        if let Some(line) = account.identity.as_ref().and_then(identity_line) {
            rows.push(
                design::caption(line)
                    .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                    .into(),
            );
        }
        match &account.snapshot {
            None if account.failure.is_none() => {
                rows.push(design::caption("Checking usage…").into());
            }
            None => {}
            Some(snapshot) => match snapshot.status {
                UsageStatus::NeedsLogin => {
                    rows.push(
                        design::body("Signed out")
                            .class(cosmic::theme::Text::Custom(design::warning_text))
                            .into(),
                    );
                    rows.push(
                        design::caption(format!(
                            "{} no longer has a working login. Sign in again to see this account's usage.",
                            account.name()
                        ))
                        .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                        .into(),
                    );
                    rows.push(
                        widget::button::suggested("Sign in")
                            .on_press(Message::AccountUi(Action::SignIn(account.id.clone())))
                            .into(),
                    );
                }
                UsageStatus::Unavailable => {
                    rows.push(
                        design::caption(
                            snapshot
                                .detail
                                .clone()
                                .unwrap_or_else(|| "No usage is reported for this account".into()),
                        )
                        .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                        .into(),
                    );
                }
                UsageStatus::Ok | UsageStatus::Warning | UsageStatus::Limited => {
                    let stale = is_stale(snapshot, now);
                    rows.extend(
                        snapshot
                            .windows
                            .iter()
                            .map(|window| usage_window_row(window, now, stale)),
                    );
                    if snapshot.windows.is_empty() {
                        rows.push(
                            design::caption("No usage limits are reported for this account.")
                                .into(),
                        );
                    }
                    if let Some(credits) = &snapshot.credits {
                        rows.push(
                            widget::row::with_children(vec![
                                design::caption("Extra usage").width(Length::Fill).into(),
                                design::caption(credits.summary())
                                    .class(cosmic::theme::Text::Default)
                                    .into(),
                            ])
                            .into(),
                        );
                    }
                }
            },
        }
        let mut footer: Vec<Element<'a, Message>> = Vec::new();
        if let Some(snapshot) = &account.snapshot {
            let ago = account::format_ago(unix_seconds(now) - unix_seconds(snapshot.observed_at));
            let source = match snapshot.source {
                UsageSource::Probe => "",
                UsageSource::Cache => " · saved",
                UsageSource::Live => " · from a running conversation",
            };
            let updated = format!("Updated {ago}{source}");
            footer.push(design::caption(updated).into());
        }
        if let Some(failure) = &account.failure {
            footer.push(
                design::caption(format!("Last check failed: {failure}"))
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                    .into(),
            );
        }
        if !footer.is_empty() {
            rows.push(design::horizontal_rule());
            rows.push(widget::column::with_children(footer).spacing(2).into());
        }
        widget::column::with_children(rows)
            .spacing(spacing)
            .width(Length::Fill)
            .into()
    }

    fn account_header<'a>(
        &'a self,
        account: &'a AccountState,
        now: Millis,
        placement: Placement,
    ) -> Element<'a, Message> {
        let mut items: Vec<Element<'a, Message>> = vec![
            widget::icon(crate::ui_controls::provider_icon(account.account.provider))
                .size(design::ROW_ICON_SIZE)
                .into(),
            design::heading(account.name()).into(),
        ];
        if let Some(plan) = account.plan() {
            items.push(design::badge(
                plan,
                design::Tone::Themed(design::accent_color),
            ));
        }
        items.push(iced::widget::Space::new().width(Length::Fill).into());
        if account.present || account.snapshot.is_some() {
            let refreshing = account.schedule.in_flight(now);
            let ready = !refreshing && account.schedule.manual_ready(now);
            let label = if refreshing {
                "Checking…"
            } else if ready {
                "Check now"
            } else {
                "Checked moments ago"
            };
            items.push(
                crate::ui_controls::compact_icon_control(
                    crate::ui_controls::symbolic_icon("view-refresh-symbolic"),
                    label,
                    ready.then(|| Message::AccountUi(Action::Refresh(account.id.clone()))),
                )
                .into(),
            );
        }
        if placement == Placement::Popover {
            items.push(
                crate::ui_controls::compact_icon_control(
                    crate::ui_controls::symbolic_icon("view-pop-out-symbolic"),
                    "Open in the Accounts pane",
                    Some(Message::AccountUi(Action::OpenPane)),
                )
                .into(),
            );
        }
        widget::row::with_children(items)
            .spacing(design::GRID_UNIT * 2.0)
            .align_y(Alignment::Center)
            .into()
    }

    /// The Accounts pane: every account the usage surfaces know about,
    /// including those with nothing to report and why.
    pub(super) fn accounts_pane(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let now = self.account_ui.now_ms;
        let mut sections: Vec<Element<'_, Message>> = vec![
            design::heading("Accounts").into(),
            design::caption(
                "Usage limits as each provider's own CLI reports them. cagents asks the CLI and never reads its credentials.",
            )
            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
            .into(),
        ];
        for account in &self.account_ui.accounts {
            let details: Element<'_, Message> = if account.present || account.snapshot.is_some() {
                self.account_details(account, now, Placement::Pane)
            } else {
                widget::column::with_children(vec![
                    self.account_header(account, now, Placement::Pane),
                    design::caption(format!(
                        "{} is not set up on this computer.",
                        account.name()
                    ))
                    .into(),
                ])
                .spacing(design::GRID_UNIT * 2.0)
                .into()
            };
            sections.push(
                widget::container(details)
                    .padding(design::GRID_UNIT * 3.0)
                    .width(Length::Fill)
                    .class(design::surface(
                        Some(design::raised),
                        [design::RADIUS_M; 4],
                        true,
                    ))
                    .into(),
            );
        }
        widget::scrollable(
            widget::container(
                widget::column::with_children(sections)
                    .spacing(spacing.space_s)
                    .max_width(design::READABLE_MEASURE),
            )
            .padding(spacing.space_m)
            .center_x(Length::Fill),
        )
        .direction(iced::widget::scrollable::Direction::Vertical(
            design::scrollbar(),
        ))
        .height(Length::Fill)
        .into()
    }
}

fn identity_line(identity: &Identity) -> Option<String> {
    let parts: Vec<&str> = [identity.email.as_deref(), identity.organization.as_deref()]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn usage_window_row<'a>(window: &'a UsageWindow, now: Millis, stale: bool) -> Element<'a, Message> {
    let tone = if stale {
        design::Tone::Themed(design::muted_color)
    } else {
        design::Tone::Themed(design::budget_tone(window.used))
    };
    let mut rows: Vec<Element<'a, Message>> = vec![
        widget::row::with_children(vec![
            design::body(window.label.as_str())
                .width(Length::Fill)
                .into(),
            design::caption(format!("{}% left", percent_left(window.used)))
                .class(cosmic::theme::Text::Default)
                .into(),
        ])
        .align_y(Alignment::Center)
        .into(),
        crate::ui_controls::budget_bar(window.used, tone),
    ];
    if let Some(line) = reset_line(window, now) {
        rows.push(design::caption(line).into());
    }
    widget::column::with_children(rows)
        .spacing(design::GRID_UNIT)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::WindowKind;

    const NOW: Millis = 1_790_000_000_000;

    fn state_with(snapshot: Option<UsageSnapshot>) -> AccountState {
        let mut state = AccountState::new(Account::system(crate::agent::Provider::Claude));
        state.present = true;
        state.snapshot = snapshot;
        state
    }

    fn window(used: f32, resets_at: Option<i64>) -> UsageWindow {
        UsageWindow {
            key: "session".into(),
            kind: WindowKind::Session,
            label: "Session".into(),
            used,
            resets_at,
            span_secs: Some(5 * 3_600),
            binding: false,
        }
    }

    fn snapshot(windows: Vec<UsageWindow>, observed_at: Millis) -> UsageSnapshot {
        let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "", observed_at);
        snapshot.detail = None;
        snapshot.windows = windows;
        snapshot.refresh_status(false, false);
        snapshot
    }

    #[test]
    fn percent_left_rounds_toward_caution_without_float_noise() {
        assert_eq!(percent_left(0.43), 57);
        assert_eq!(percent_left(0.424), 57);
        assert_eq!(percent_left(0.0), 100);
        assert_eq!(percent_left(1.0), 0);
        assert_eq!(percent_left(1.3), 0);
        assert_eq!(percent_left(0.001), 99);
    }

    #[test]
    fn a_current_chip_states_what_is_left_and_when_it_resets() {
        let reset = unix_seconds(NOW) + 2 * 3_600 + 14 * 60;
        let chip = chip(
            &state_with(Some(snapshot(vec![window(0.38, Some(reset))], NOW))),
            NOW,
        );
        assert_eq!(chip.text, "62% left · 2h 14m");
        assert_eq!(chip.gauge, Some(0.38));
        assert_eq!(chip.tone, ChipTone::Current);
        assert!(chip.tooltip.contains("Session: 62% left"));
    }

    /// Claude marks one model's exhausted weekly allowance as the binding
    /// limit while the account keeps working on every other model.
    #[test]
    fn a_chip_leads_with_the_account_not_one_models_allowance() {
        let mut scoped = window(1.0, None);
        scoped.key = "weekly_scoped:fable".into();
        scoped.kind = WindowKind::Scoped;
        scoped.label = "Weekly · Fable".into();
        scoped.binding = true;
        let mut weekly = window(0.95, None);
        weekly.key = "weekly_all".into();
        weekly.kind = WindowKind::Weekly;
        weekly.label = "Weekly".into();
        let state = state_with(Some(snapshot(
            vec![window(0.43, None), weekly, scoped],
            NOW,
        )));
        let chip = chip(&state, NOW);
        assert_eq!(chip.text, "5% left");
        assert!(
            chip.tooltip.contains("Weekly · Fable: 0% left"),
            "{}",
            chip.tooltip
        );
    }

    #[test]
    fn old_figures_and_figures_from_before_a_reset_are_shown_as_of_when_they_were_read() {
        let observed = NOW - schedule::DISPLAY_STALE_AFTER - 1;
        let old = chip(
            &state_with(Some(snapshot(vec![window(0.5, None)], observed))),
            NOW,
        );
        assert_eq!(old.tone, ChipTone::Stale);
        assert!(old.text.starts_with("50% left · as of "), "{}", old.text);

        let reset = unix_seconds(NOW) - 60;
        let before_reset = snapshot(vec![window(0.9, Some(reset))], NOW - 5 * 60_000);
        assert!(reset_since(&before_reset, NOW));
        assert_eq!(
            chip(&state_with(Some(before_reset)), NOW).tone,
            ChipTone::Stale
        );
    }

    #[test]
    fn a_signed_out_account_asks_for_a_sign_in_and_an_unanswered_one_says_so() {
        let signed_out = UsageSnapshot::state(UsageStatus::NeedsLogin, "signed out", NOW);
        let chip_for_login = chip(&state_with(Some(signed_out)), NOW);
        assert_eq!(chip_for_login.text, "Sign in");
        assert_eq!(chip_for_login.tone, ChipTone::Attention);
        assert!(chip_for_login.essential);

        assert_eq!(chip(&state_with(None), NOW).text, "Checking…");
        let mut failed = state_with(None);
        failed.failure = Some("Claude Code did not answer".into());
        assert_eq!(chip(&failed, NOW).text, "Unavailable");
    }

    #[test]
    fn a_login_terminal_runs_the_probed_cli_at_home_and_is_not_an_agent() {
        let launch = login_launch(
            &Account::system(crate::agent::Provider::Claude),
            Path::new("/opt/bin/claude"),
            PathBuf::from("/home/reader"),
        )
        .expect("Claude has a login command");
        assert_eq!(launch.program, "/opt/bin/claude");
        assert_eq!(launch.arguments, ["auth", "login"]);
        assert_eq!(launch.working_directory, Path::new("/home/reader"));
        assert!(launch.environment.is_empty());
        assert_eq!(crate::agent::Provider::recognize_launch(&launch), None);

        let managed = Account {
            provider: crate::agent::Provider::Codex,
            home: account::AccountHome::Managed(PathBuf::from("/accounts/work")),
        };
        let launch = login_launch(&managed, Path::new("/usr/bin/codex"), PathBuf::from("/h"))
            .expect("Codex logs in");
        assert_eq!(launch.arguments, ["login"]);
        assert_eq!(crate::agent::Provider::recognize_launch(&launch), None);
        assert_eq!(
            launch.environment.get("CODEX_HOME").map(String::as_str),
            Some("/accounts/work")
        );
        assert!(is_sign_in(&launch));
    }

    #[test]
    fn only_a_providers_sign_in_is_kept_off_the_disk() {
        let recipe = |program: &str, arguments: &[&str]| LaunchRecipe {
            program: program.to_owned(),
            arguments: arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
            working_directory: PathBuf::from("/h"),
            environment: BTreeMap::new(),
        };
        assert!(is_sign_in(&recipe("/opt/bin/claude", &["auth", "login"])));
        assert!(is_sign_in(&recipe("codex", &["login"])));
        assert!(!is_sign_in(&recipe("claude", &[])));
        assert!(!is_sign_in(&recipe("claude", &["login"])));
        assert!(!is_sign_in(&recipe("codex", &["login", "--api-key"])));
        assert!(!is_sign_in(&recipe("/bin/fish", &["login"])));
    }
}

#[cfg(test)]
mod app_tests {
    use super::*;
    use crate::app::tests::context_inspector_test_app;

    fn signed_out(app: &mut Cagents) -> AccountId {
        let state = &mut app.account_ui.accounts[0];
        state.present = true;
        state.snapshot = Some(UsageSnapshot::state(
            UsageStatus::NeedsLogin,
            "signed out",
            NOW,
        ));
        state.id.clone()
    }

    const NOW: Millis = 1_790_000_000_000;

    #[test]
    fn the_status_bar_follows_its_setting_and_hiding_it_closes_the_popover() {
        let mut app = context_inspector_test_app(false);
        let id = signed_out(&mut app);
        assert!(app.status_bar().is_some());
        let _ = app.update(Message::AccountUi(Action::Popover(Some(id.clone()))));
        assert_eq!(app.account_ui.popover.as_ref(), Some(&id));
        let _ = app.update(Message::SettingsChanged {
            field: SettingsField::ShowStatusBar,
            value: "false".into(),
        });
        assert!(!app.settings.show_status_bar);
        assert!(app.status_bar().is_none());
        assert_eq!(app.account_ui.popover, None);
    }

    #[test]
    fn a_chip_toggles_its_popover_and_escape_closes_it_first() {
        let mut app = context_inspector_test_app(false);
        let id = signed_out(&mut app);
        let _ = app.update(Message::AccountUi(Action::Popover(Some(id.clone()))));
        assert_eq!(app.account_ui.popover.as_ref(), Some(&id));
        let _ = app.update(Message::AccountUi(Action::Popover(Some(id.clone()))));
        assert_eq!(app.account_ui.popover, None);
        let _ = app.update(Message::AccountUi(Action::Popover(Some(id))));
        let escape = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        // Captured by a focused terminal or not, Escape reaches the popover.
        let _ = app.update(Message::KeyEvent {
            event: escape.clone(),
            captured: true,
        });
        assert_eq!(app.account_ui.popover, None);
        assert!(app.account_key_event(&escape).is_none());
    }

    #[test]
    fn the_pop_out_opens_one_accounts_pane_and_closes_the_popover() {
        let mut app = context_inspector_test_app(false);
        let id = signed_out(&mut app);
        let _ = app.update(Message::AccountUi(Action::Popover(Some(id))));
        let _ = app.update(Message::AccountUi(Action::OpenPane));
        let pane = app
            .utility_card(&CardKind::Accounts)
            .expect("the pop-out opens the Accounts pane");
        assert_eq!(app.account_ui.popover, None);
        assert!(matches!(app.cards.get(&pane), Some(CardRuntime::Accounts)));
        let _ = app.update(Message::AccountUi(Action::OpenPane));
        assert_eq!(app.utility_card(&CardKind::Accounts), Some(pane));
        assert_eq!(
            app.durable
                .cards
                .iter()
                .filter(|card| card.existence.is_open() && card.kind == CardKind::Accounts)
                .count(),
            1
        );
    }

    #[test]
    fn a_probe_answer_updates_the_account_and_a_failure_keeps_its_figures() {
        let mut app = context_inspector_test_app(false);
        let id = app.account_ui.accounts[0].id.clone();
        let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "", NOW);
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
        let _ = app.update(Message::AccountUi(Action::Probed {
            id: id.clone(),
            outcome: Outcome::Answered {
                identity: None,
                snapshot: snapshot.clone(),
            },
        }));
        let state = app.account_ui.account(&id).unwrap();
        assert!(state.present);
        assert_eq!(state.snapshot.as_ref(), Some(&snapshot));
        let _ = app.update(Message::AccountUi(Action::Probed {
            id: id.clone(),
            outcome: Outcome::Failed("timed out".into()),
        }));
        let state = app.account_ui.account(&id).unwrap();
        assert_eq!(state.snapshot.as_ref(), Some(&snapshot));
        assert_eq!(state.failure.as_deref(), Some("timed out"));
        assert_eq!(state.schedule.failures(), 1);
        // Another window's older answer, read back from the cache, changes
        // nothing - not the figures, and not who the account belongs to.
        let mut older = UsageSnapshot::state(UsageStatus::NeedsLogin, "signed out", NOW - 1);
        older.source = UsageSource::Cache;
        let _ = app.update(Message::AccountUi(Action::Probed {
            id: id.clone(),
            outcome: Outcome::Answered {
                identity: Some(account::Identity {
                    email: Some("someone@example.com".into()),
                    organization: None,
                    plan: None,
                }),
                snapshot: older,
            },
        }));
        let state = app.account_ui.account(&id).unwrap();
        assert_eq!(state.snapshot.as_ref(), Some(&snapshot));
        assert_eq!(state.identity, None);
        assert!(!state.schedule.in_flight(NOW));
    }
}
