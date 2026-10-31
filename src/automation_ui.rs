//! Window-local drafts and paged views of daemon-owned automations. Every
//! mutation carries the revision shown to the reader; confirmation retains an
//! immutable launch description, never a reference to a changing editor.
use crate::{
    account::AccountId,
    agent::Provider,
    agent_execution::{Spec, Surface, Target},
    automations::{
        Automation, Definition, Firing, HistoryCursor, Request, Response, RunDetail, Schedule,
        SchedulePattern, SessionMode, Summary,
    },
    chat::Model,
    design::space,
};
use cosmic::{
    Element,
    iced::{Alignment, Length, widget::text_editor},
    widget,
};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

const PAGE: usize = 50;
/// Narrower than this, the header's primary command is drawn as an icon so
/// the header keeps one row.
const COMPACT_HEADER_WIDTH: f32 = 440.0;
/// From this width the automation list sits beside the selection.
const SIDE_BY_SIDE_WIDTH: f32 = 700.0;
const LIST_WIDTH: f32 = 240.0;
/// The most of a narrow pane's height the stacked list takes from the
/// selection below it, including short split panes.
const STACKED_LIST_SHARE: f32 = 0.4;
const PROVIDERS: [&str; 3] = ["Claude", "Codex", "Omp"];
const SCHEDULES: [&str; 5] = ["Hourly", "Daily", "Weekdays", "Weekly", "Cron"];
const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

#[derive(Debug, Clone)]
pub struct TargetChoice {
    pub label: String,
    pub target: Target,
    /// The account each provider's new work in this directory uses: the
    /// project's own choice, or else the application default.
    pub defaults: BTreeMap<Provider, AccountId>,
}
#[derive(Debug, Clone)]
pub struct AccountChoice {
    pub label: String,
    pub provider: Provider,
    pub id: AccountId,
}

/// The models each provider last listed: the catalog the chat composer
/// offers. A provider absent here has not listed any yet.
pub type Models = BTreeMap<Provider, Vec<Model>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelChoice {
    Default,
    Listed(String),
    Custom,
}

#[derive(Debug, Clone, Copy)]
pub enum Field {
    Name,
    Model,
    Permission,
    Timezone,
    Hour,
    Minute,
    Cron,
    Timeout,
}

#[derive(Debug, Clone)]
pub enum Action {
    Browse,
    /// Leaves the open run for the automation or history it was opened from.
    CloseRun,
    Tick,
    Refresh,
    More,
    Search(String),
    New,
    Select(Uuid),
    Edit,
    CancelEdit,
    KeepDraft(u64),
    DiscardDraft(u64),
    Save,
    Field(Field, String),
    Provider(usize),
    Account(usize),
    Model(ModelChoice),
    Effort(Option<String>),
    Target(usize),
    Schedule(usize),
    Weekday(usize),
    Reuse(bool),
    Precheck(bool),
    ToggleAdvanced,
    Prompt(text_editor::Action),
    PromptCursor(text_editor::Cursor),
    Shell(text_editor::Action),
    ShellCursor(text_editor::Cursor),
    Enable,
    Pause,
    RunNow,
    Remove,
    Confirm(u64),
    CancelConfirmation,
    History,
    AllHistory,
    MoreHistory,
    Lifecycle,
    MoreLifecycle,
    Run(Uuid),
    InspectRun(crate::model::RunId),
    CancelRun,
    OpenSession,
    Received {
        sequence: u64,
        result: std::sync::Arc<Result<Response, String>>,
    },
}

impl Action {
    pub fn answers_draft_dialog(&self) -> bool {
        matches!(self, Self::KeepDraft(_) | Self::DiscardDraft(_))
    }
    pub fn background(&self) -> bool {
        matches!(self, Self::Received { .. } | Self::Tick)
    }
}

pub enum Effect {
    Request { sequence: u64, request: Request },
    OpenRun(RunDetail),
    ReadRun(crate::model::RunId),
}

/// A request in flight. `Reload` rereads the open automation for Refresh,
/// then replaces its history with the newest page.
#[derive(Clone, Copy)]
enum Pending {
    List {
        append: bool,
    },
    Show,
    Reload,
    Save,
    Mutate,
    History {
        append: bool,
    },
    Lifecycle {
        id: Uuid,
        append: bool,
    },
    RefreshList,
    RefreshLoadedList(LoadedListRefresh),
    RefreshAutomation,
    RefreshHistory {
        scope: Option<Uuid>,
    },
    RefreshRun,
    RefreshOlderHistory {
        refresh: HistoryRefresh,
        before: Option<HistoryKey>,
    },
    Run,
    OpenRun,
}

impl Pending {
    fn background_read(self) -> bool {
        matches!(
            self,
            Self::RefreshList
                | Self::RefreshLoadedList(_)
                | Self::RefreshAutomation
                | Self::RefreshHistory { .. }
                | Self::RefreshRun
                | Self::RefreshOlderHistory { .. }
        )
    }
}

/// One bounded read through the UUID range the reader explicitly loaded.
/// This is a cursor, not a snapshot or a promise about refresh latency.
#[derive(Clone, Copy)]
struct LoadedListRefresh {
    after: Option<Uuid>,
    through: Uuid,
}

type HistoryKey = (u64, Uuid);

/// A finite read through the reader's loaded history, not a server snapshot.
/// Both endpoints are actual firing keys; neither is a manual paging cursor.
#[derive(Clone, Copy)]
struct HistoryRefresh {
    scope: Option<Uuid>,
    before: HistoryKey,
    through: HistoryKey,
}

fn history_key(firing: &Firing) -> HistoryKey {
    (firing.created_ms, firing.id)
}

fn history_cursor(key: HistoryKey) -> HistoryCursor {
    HistoryCursor {
        created_ms: key.0,
        id: key.1,
    }
}

/// History's public cursor is a structured exclusive key, not a capability.
/// Canonical UUID text order equals its unsigned 128-bit order. Derive the
/// tight inclusive bound for a real loaded candidate, skipping unrequested
/// newer history. None is the unbounded upper end for the maximal SQL key.
fn including_history_key(key: HistoryKey) -> Result<Option<HistoryKey>, &'static str> {
    i64::try_from(key.0)
        .map_err(|_| "The loaded run history timestamp is outside the supported range")?;
    Ok(if let Some(id) = key.1.as_u128().checked_add(1) {
        Some((key.0, Uuid::from_u128(id)))
    } else if key.0 < i64::MAX as u64 {
        Some((key.0 + 1, Uuid::nil()))
    } else {
        None
    })
}

struct Confirmation {
    generation: u64,
    automation: Automation,
    request: Request,
    title: &'static str,
}

#[derive(PartialEq, Eq)]
struct DraftValues {
    name: String,
    model: String,
    effort: Option<String>,
    permission: String,
    timezone: String,
    hour: String,
    minute: String,
    cron: String,
    timeout: String,
    provider: usize,
    account: Option<usize>,
    target: Option<usize>,
    schedule: usize,
    weekday: usize,
    reuse: bool,
    precheck: bool,
    prompt: String,
    shell: String,
}

pub struct Draft {
    baseline: Option<DraftValues>,
    base: Option<(Uuid, u64)>,
    name: String,
    model: String,
    /// Whether the model is typed rather than chosen from the catalog.
    custom_model: bool,
    effort: Option<String>,
    permission: String,
    timezone: String,
    /// The saved automation's timezone exactly as stored. The daemon treats
    /// any change in its spelling as a material edit, so the same zone typed
    /// in another case saves this string unchanged.
    loaded_timezone: Option<String>,
    /// Zones suggested for the typed timezone, or `None` once it resolves;
    /// recomputed when the timezone changes rather than on every redraw.
    timezone_matches: Option<Vec<&'static str>>,
    hour: String,
    minute: String,
    cron: String,
    timeout: String,
    provider: usize,
    account: Option<usize>,
    /// Whether the account was picked, or saved, rather than preselected as
    /// the target's default; a preselection follows the provider and target.
    account_chosen: bool,
    target: Option<usize>,
    schedule: usize,
    weekday: usize,
    reuse: bool,
    precheck: bool,
    advanced: bool,
    prompt: text_editor::Content<cosmic::Renderer>,
    shell: text_editor::Content<cosmic::Renderer>,
    // Indices are frozen against the options with which this draft opened.
    targets: Vec<TargetChoice>,
    accounts: Vec<AccountChoice>,
}

impl Draft {
    fn new(
        targets: &[TargetChoice],
        accounts: &[AccountChoice],
        existing: Option<&Automation>,
    ) -> Self {
        let mut value = Self {
            baseline: None,
            base: None,
            name: String::new(),
            model: String::new(),
            custom_model: false,
            effort: None,
            permission: String::new(),
            timezone: crate::automations::schedule::system_timezone()
                .unwrap_or_else(|| "UTC".into()),
            loaded_timezone: None,
            timezone_matches: None,
            hour: "9".into(),
            minute: "0".into(),
            cron: "0 9 * * 1-5".into(),
            timeout: "30".into(),
            provider: 0,
            account: None,
            account_chosen: false,
            target: (!targets.is_empty()).then_some(0),
            schedule: 1,
            weekday: 0,
            reuse: false,
            precheck: false,
            advanced: false,
            prompt: text_editor::Content::new(),
            shell: text_editor::Content::new(),
            targets: targets.to_vec(),
            accounts: accounts.to_vec(),
        };
        if let Some(automation) = existing {
            let definition = &automation.definition;
            value.base = Some((automation.id, automation.revision));
            value.name = definition.name.clone();
            value.model = definition.agent.model.clone().unwrap_or_default();
            value.effort = definition.agent.reasoning_effort.clone();
            value.permission = definition.agent.permission_mode.clone().unwrap_or_default();
            value.advanced = !value.permission.is_empty() || definition.precheck.is_some();
            value.timezone = definition.schedule.timezone.clone();
            value.loaded_timezone = Some(definition.schedule.timezone.clone());
            value.provider = provider_index(definition.agent.provider);
            value.account = value
                .accounts
                .iter()
                .position(|entry| Some(&entry.id) == definition.agent.account.as_ref());
            // A saved account is never replaced by a default, even when absent.
            value.account_chosen = true;
            value.target = value
                .targets
                .iter()
                .position(|entry| entry.target == definition.agent.target);
            // A removed worktree remains visible as the old choice, but its
            // next save/enable is revalidated by the daemon and can fail.
            if value.target.is_none() {
                value.target = Some(value.targets.len());
                value.targets.push(TargetChoice {
                    label: format!(
                        "{} (saved target)",
                        definition.agent.target.directory.display()
                    ),
                    target: definition.agent.target.clone(),
                    defaults: BTreeMap::new(),
                });
            }
            value.reuse = definition.session_mode == SessionMode::Reuse;
            value.prompt = text_editor::Content::with_text(&definition.prompt);
            if let Some(precheck) = &definition.precheck {
                value.precheck = true;
                value.shell = text_editor::Content::with_text(&precheck.command);
                value.timeout = precheck.timeout_seconds.to_string();
            }
            match &definition.schedule.pattern {
                SchedulePattern::Hourly { minute } => {
                    value.schedule = 0;
                    value.minute = minute.to_string();
                }
                SchedulePattern::Daily { hour, minute } => {
                    value.schedule = 1;
                    value.hour = hour.to_string();
                    value.minute = minute.to_string();
                }
                SchedulePattern::Weekdays { hour, minute } => {
                    value.schedule = 2;
                    value.hour = hour.to_string();
                    value.minute = minute.to_string();
                }
                SchedulePattern::Weekly {
                    weekday,
                    hour,
                    minute,
                } => {
                    value.schedule = 3;
                    value.weekday = usize::from(*weekday).saturating_sub(1);
                    value.hour = hour.to_string();
                    value.minute = minute.to_string();
                }
                SchedulePattern::Cron { expression } => {
                    value.schedule = 4;
                    value.cron = expression.clone();
                }
            }
        }
        value.preselect_account();
        value.refresh_timezone_matches();
        value.baseline = Some(value.values());
        value
    }

    fn values(&self) -> DraftValues {
        DraftValues {
            name: self.name.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            permission: self.permission.clone(),
            timezone: self.timezone.clone(),
            hour: self.hour.clone(),
            minute: self.minute.clone(),
            cron: self.cron.clone(),
            timeout: self.timeout.clone(),
            provider: self.provider,
            account: self.account,
            target: self.target,
            schedule: self.schedule,
            weekday: self.weekday,
            reuse: self.reuse,
            precheck: self.precheck,
            prompt: self.prompt.text(),
            shell: self.shell.text(),
        }
    }
    fn dirty(&self) -> bool {
        self.baseline
            .as_ref()
            .is_some_and(|baseline| self.values() != *baseline)
    }

    /// Suggestions for the typed timezone; none once it names a zone.
    fn refresh_timezone_matches(&mut self) {
        let query = self.timezone.trim();
        self.timezone_matches = jiff::tz::db()
            .get(query)
            .is_err()
            .then(|| timezone_matches(query, 6));
    }

    /// Unless the reader picked one, the account is the target's default for
    /// the provider, as a new conversation there would use, or the provider's
    /// only account.
    fn preselect_account(&mut self) {
        if self.account_chosen {
            return;
        }
        let provider = provider(self.provider);
        let default = self
            .target
            .and_then(|index| self.targets.get(index))
            .and_then(|target| target.defaults.get(&provider));
        let mut candidates = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| account.provider == provider);
        let only = candidates.clone().nth(1).is_none();
        self.account = candidates
            .clone()
            .find(|(_, account)| Some(&account.id) == default)
            .or_else(|| candidates.next().filter(|_| only))
            .map(|(index, _)| index);
    }

    /// The listed model this draft names, whose efforts it chooses among.
    fn listed<'a>(&self, models: &'a Models) -> Option<&'a Model> {
        let id = self.model.trim();
        models
            .get(&provider(self.provider))?
            .iter()
            .find(|model| model.id == id)
    }

    /// Whether the model is a typed ID: chosen as one, or saved as one the
    /// provider does not list.
    fn custom(&self, models: &Models) -> bool {
        self.custom_model || (!self.model.trim().is_empty() && self.listed(models).is_none())
    }

    /// The efforts the picker offers: the listed model's own, as the chat
    /// composer offers them, and a saved effort it does not list, which stays
    /// visible so that it can be reviewed and cleared.
    fn efforts(&self, models: &Models) -> Vec<String> {
        let mut efforts = self
            .listed(models)
            .map(|model| model.reasoning.clone())
            .unwrap_or_default();
        if let Some(effort) = &self.effort
            && !efforts.contains(effort)
        {
            efforts.push(effort.clone());
        }
        efforts
    }

    fn choose_model(&mut self, choice: ModelChoice, models: &Models) {
        match choice {
            ModelChoice::Default => {
                self.model.clear();
                self.custom_model = false;
            }
            ModelChoice::Listed(id) => {
                self.model = id;
                self.custom_model = false;
            }
            ModelChoice::Custom => self.custom_model = true,
        }
        // An effort carries over to a model that offers it, as it does in the
        // composer. A typed model's efforts are unknown, so it keeps its own.
        let offered = match self.listed(models) {
            Some(model) => self
                .effort
                .as_ref()
                .is_some_and(|effort| model.reasoning.contains(effort)),
            None => self.custom(models),
        };
        if !offered {
            self.effort = None;
        }
    }

    /// The schedule as entered, with a timezone the database knows. The daemon
    /// checks the same again when saving.
    fn schedule(&self) -> Result<Schedule, String> {
        let parse = |text: &str, label: &str| {
            text.trim()
                .parse::<u8>()
                .map_err(|_| format!("Enter a valid {label}"))
        };
        let pattern = match self.schedule {
            0 => SchedulePattern::Hourly {
                minute: parse(&self.minute, "minute")?,
            },
            1 => SchedulePattern::Daily {
                hour: parse(&self.hour, "hour")?,
                minute: parse(&self.minute, "minute")?,
            },
            2 => SchedulePattern::Weekdays {
                hour: parse(&self.hour, "hour")?,
                minute: parse(&self.minute, "minute")?,
            },
            3 => SchedulePattern::Weekly {
                weekday: self.weekday as u8 + 1,
                hour: parse(&self.hour, "hour")?,
                minute: parse(&self.minute, "minute")?,
            },
            _ => SchedulePattern::Cron {
                expression: self.cron.clone(),
            },
        };
        pattern.expression()?;
        let typed = self.timezone.trim();
        // The daemon resolves the zone the same way, ignoring case; the saved
        // name is the database's own spelling of it, except that the saved
        // automation's zone keeps its stored spelling so an unchanged zone is
        // not a material edit.
        let zone = jiff::tz::db()
            .get(typed)
            .map_err(|_| format!("Choose a timezone from the list; {typed:?} is not one"))?;
        let timezone = match &self.loaded_timezone {
            Some(loaded) if loaded.trim().eq_ignore_ascii_case(typed) => loaded.clone(),
            _ => zone.iana_name().unwrap_or(typed).into(),
        };
        Ok(Schedule { pattern, timezone })
    }

    fn definition(&self, models: &Models) -> Result<Definition, String> {
        if let (Some(model), Some(effort)) = (self.listed(models), &self.effort)
            && !model.reasoning.contains(effort)
        {
            return Err(format!(
                "{} does not offer the {effort} reasoning effort; choose one it lists",
                model.label
            ));
        }
        let schedule = self.schedule()?;
        let account = self
            .account
            .and_then(|index| self.accounts.get(index))
            .filter(|account| account.provider == provider(self.provider))
            .ok_or("Choose an explicit provider account")?;
        let target = self
            .target
            .and_then(|index| self.targets.get(index))
            .ok_or("Choose an open project and working directory")?;
        let value = Definition {
            name: self.name.trim().into(),
            prompt: self.prompt.text(),
            agent: Spec {
                provider: provider(self.provider),
                account: Some(account.id.clone()),
                target: target.target.clone(),
                surface: Surface::Chat,
                model: optional(&self.model),
                permission_mode: optional(&self.permission),
                reasoning_effort: self.effort.clone(),
            },
            schedule,
            session_mode: if self.reuse {
                SessionMode::Reuse
            } else {
                SessionMode::Fresh
            },
            precheck: if self.precheck {
                Some(crate::automations::Precheck {
                    command: self.shell.text(),
                    timeout_seconds: self
                        .timeout
                        .parse()
                        .map_err(|_| "Enter a valid precheck timeout")?,
                })
            } else {
                None
            },
        };
        value.validate()?;
        Ok(value)
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
enum HistoryStatus {
    #[default]
    Unrequested,
    Loading,
    Loaded,
    /// Keeps its message, so the history states its own failure.
    Failed(String),
}

/// Whether the automation list has been read. A failure keeps its message,
/// so the list states its own failure and not a later one's.
#[derive(Default, Debug, PartialEq, Eq)]
enum ListStatus {
    #[default]
    Unread,
    Loaded,
    Failed(String),
}

/// The pane's two peer views. A run opened from one stays inside it, so
/// leaving the run returns there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Automations,
    RunHistory,
}

impl Section {
    const ALL: [Self; 2] = [Self::Automations, Self::RunHistory];

    fn label(self) -> &'static str {
        match self {
            Self::Automations => "Automations",
            Self::RunHistory => "Run history",
        }
    }

    /// The tab's accessible name and tooltip.
    fn description(self) -> &'static str {
        match self {
            Self::Automations => "Automations and their schedules",
            Self::RunHistory => "Runs of every automation, including removed ones",
        }
    }

    /// The current tab's accessible name and tooltip while something is open
    /// beneath it, which choosing the tab leaves.
    fn back(self) -> &'static str {
        match self {
            Self::Automations => "Back to the automation list",
            Self::RunHistory => "Back to Run history",
        }
    }

    fn enter(self) -> Action {
        match self {
            Self::Automations => Action::Browse,
            Self::RunHistory => Action::AllHistory,
        }
    }
}

struct LifecycleNotice {
    id: Uuid,
    sequence: u64,
    event: Option<crate::automations::LifecycleEvent>,
}

#[derive(Default)]
pub struct State {
    lifecycle_notice: Option<LifecycleNotice>,
    manual_requests: std::collections::BTreeMap<Uuid, (u64, Uuid)>,
    sequence: u64,
    pending: Option<(u64, Pending)>,
    rows: Vec<Summary>,
    list_status: ListStatus,
    // The newest firing learned from an authoritative first history page.
    // Older pages and detail reads may update it, but cannot replace it.
    last_firings: BTreeMap<Uuid, (u64, Uuid)>,
    more: bool,
    after: Option<Uuid>,
    loaded_list_refresh: Option<LoadedListRefresh>,
    list_changed: bool,
    search: String,
    selected: Option<Automation>,
    draft: Option<Draft>,
    discard_draft: Option<(u64, Action)>,
    discard_generation: u64,
    confirmation: Option<Confirmation>,
    history: Vec<Firing>,
    lifecycle: Option<crate::automations::LifecyclePage>,
    history_status: HistoryStatus,
    history_next: Option<HistoryCursor>,
    history_newer: bool,
    history_refresh: Option<HistoryRefresh>,
    all_history: bool,
    run: Option<RunDetail>,
    error: Option<String>,
    notice: Option<String>,
    targets: Vec<TargetChoice>,
    accounts: Vec<AccountChoice>,
    models: Models,
    archived_accounts: Vec<AccountChoice>,
}

impl State {
    pub fn has_draft(&self) -> bool {
        self.draft.is_some()
    }
    pub fn draft_dirty(&self) -> bool {
        self.draft.as_ref().is_some_and(Draft::dirty)
    }
    pub fn draft_close_blocker(&self) -> Option<&'static str> {
        if self.draft.is_some() && matches!(self.pending, Some((_, Pending::Save))) {
            Some("Wait for the automation save to finish before closing the window.")
        } else if self.draft_dirty() {
            Some(
                "The automation has unsaved changes. Keep editing or discard the draft before closing the window.",
            )
        } else {
            None
        }
    }
    pub fn can_discard_draft(&self) -> bool {
        self.draft.is_some() && !matches!(self.pending, Some((_, Pending::Save)))
    }
    pub fn discard_draft_before_close(&mut self) -> bool {
        if !self.can_discard_draft() {
            return false;
        }
        self.draft = None;
        self.discard_draft = None;
        true
    }
    pub fn draft_dialog_open(&self) -> bool {
        self.discard_draft.is_some()
    }
    pub fn keep_draft_action(&self) -> Option<Action> {
        self.discard_draft
            .as_ref()
            .map(|(generation, _)| Action::KeepDraft(*generation))
    }
    pub fn draft_dialog(&self) -> Option<Element<'_, Action>> {
        let (generation, _) = self.discard_draft.as_ref()?;
        Some(crate::app::dialog_ui::surface(
            widget::column::with_children(vec![
                crate::design::title("Discard automation changes?").into(),
                crate::design::body("Your unsaved changes will be lost.").into(),
                widget::row::with_children(vec![
                    crate::ui_controls::button::secondary("Keep editing")
                        .id(draft_dialog_focus_id())
                        .on_press(Action::KeepDraft(*generation))
                        .into(),
                    crate::ui_controls::button::destructive("Discard changes")
                        .on_press(Action::DiscardDraft(*generation))
                        .into(),
                ])
                .spacing(space::S)
                .into(),
            ])
            .spacing(space::M),
            560.0,
        ))
    }

    /// Archived homes remain usable by definitions that already selected them,
    /// but are never choices when creating a new automation.
    pub fn set_archived_accounts(&mut self, accounts: Vec<AccountChoice>) {
        self.archived_accounts = accounts;
    }
    pub fn can_open_run(&self) -> bool {
        self.draft.is_none()
            && self.confirmation.is_none()
            && !matches!(
                self.pending,
                Some((_, Pending::Save | Pending::Mutate | Pending::OpenRun))
            )
    }
    /// Notification reads may supersede background polling, never a draft,
    /// confirmation or foreground request owned by the user.
    pub fn can_open_lifecycle(&self) -> bool {
        self.can_open_run()
            && self
                .pending
                .is_none_or(|(_, pending)| pending.background_read())
    }

    /// Adopt only the exact authorized historical receipt. No definition or
    /// latest-row fallback is needed, including for a removed automation.
    pub fn show_lifecycle_receipt(
        &mut self,
        id: Uuid,
        sequence: u64,
        result: Result<crate::automations::LifecyclePage, ()>,
    ) -> bool {
        if !self.can_open_lifecycle() || id.is_nil() || sequence == 0 || sequence > i64::MAX as u64
        {
            return false;
        }
        let event = result.ok().and_then(|mut page| {
            if page.events.len() != 1 || page.next.is_some_and(|next| next != sequence) {
                return None;
            }
            let event = page.events.pop()?;
            (event.automation == id
                && event.sequence == sequence
                && event.actor == crate::automations::LifecycleActor::System
                && matches!(
                    event.cause,
                    crate::automations::LifecycleCause::ProjectClosed
                        | crate::automations::LifecycleCause::ProjectUnavailable
                        | crate::automations::LifecycleCause::StartPaused
                        | crate::automations::LifecycleCause::PrecheckInterrupted
                        | crate::automations::LifecycleCause::PrecheckPaused
                )
                && !event.enabled
                && event.revision > 0
                && event.revision <= i64::MAX as u64
                && event.occurred_ms <= i64::MAX as u64
                && !event.firing.is_some_and(|id| id.is_nil()))
            .then_some(event)
        });
        self.sequence = self.sequence.wrapping_add(1);
        self.pending = None;
        self.history_refresh = None;
        self.selected = None;
        self.run = None;
        self.lifecycle = None;
        self.all_history = false;
        self.error = None;
        self.notice = None;
        self.lifecycle_notice = Some(LifecycleNotice {
            id,
            sequence,
            event,
        });
        true
    }

    pub fn set_choices(
        &mut self,
        targets: Vec<TargetChoice>,
        accounts: Vec<AccountChoice>,
        models: Models,
    ) {
        self.targets = targets;
        self.accounts = accounts;
        self.models = models;
    }
    fn request(&mut self, pending: Pending, request: Request) -> Option<Effect> {
        self.sequence = self.sequence.wrapping_add(1);
        if !pending.background_read() {
            self.history_refresh = None;
        }
        self.pending = Some((self.sequence, pending));
        self.error = None;
        if matches!(pending, Pending::History { .. }) {
            self.history_status = HistoryStatus::Loading;
        }
        Some(Effect::Request {
            sequence: self.sequence,
            request,
        })
    }
    fn refresh_last_outcome(&mut self, firing: &Firing, newest_page: bool) {
        let key = (firing.created_ms, firing.id);
        let known = self.last_firings.get(&firing.automation).copied();
        if known.is_some_and(|latest| key < latest) || (!newest_page && known != Some(key)) {
            return;
        }
        if let Some(row) = self.rows.iter_mut().find(|row| row.id == firing.automation) {
            row.last_state = Some(firing.state);
            self.last_firings.insert(firing.automation, key);
        }
    }

    fn refresh_loaded_list(&mut self, refresh: LoadedListRefresh, rows: Vec<Summary>) {
        // Validate the whole page before changing any row or advancing a
        // cursor. Canonical UUID order is the daemon's immutable List order.
        if rows.len() > PAGE
            || rows
                .first()
                .is_some_and(|row| refresh.after.is_some_and(|after| row.id <= after))
            || rows.windows(2).any(|pair| pair[0].id >= pair[1].id)
        {
            self.list_status =
                ListStatus::Failed("The automation list reply was inconsistent".into());
            self.error = Some("The automation list reply was inconsistent".into());
            return;
        }
        let last = rows.last().map(|row| row.id);
        let complete = rows.len() < PAGE || last.is_some_and(|last| last >= refresh.through);
        let covered = if complete {
            refresh.through
        } else {
            last.expect("a full page has a final UUID")
        };
        // Foreground Save/More can leave a non-prefix display order. Keep its
        // slots: only the response is ordered, not necessarily our loaded Vec.
        // Missing or new IDs request Refresh, never automatic removal/growth.
        let covered_count = self
            .rows
            .iter()
            .filter(|row| refresh.after.is_none_or(|after| row.id > after) && row.id <= covered)
            .count();
        let mut matched = 0;
        for row in rows {
            if row.id > refresh.through {
                // A returned addition beyond the old tail still needs a
                // refresh cue once the manual list was exhausted. Otherwise
                // this can simply be an already-unloaded page.
                self.list_changed |= !self.more;
                continue;
            }
            if let Some(current) = self.rows.iter_mut().find(|loaded| loaded.id == row.id) {
                self.last_firings.remove(&row.id);
                *current = row;
                matched += 1;
            } else {
                // A Show reply may have added a known non-prefix row. When
                // more pages remain, unknown IDs beyond the manual cursor
                // need not be new; they are not a membership census.
                self.list_changed |= !self.more || self.after.is_some_and(|after| row.id <= after);
            }
        }
        if matched != covered_count {
            self.list_changed = true;
        }
        self.list_status = ListStatus::Loaded;
        self.loaded_list_refresh = (!complete).then_some(LoadedListRefresh {
            after: last,
            through: refresh.through,
        });
    }

    fn merge_history_rows(&mut self, rows: Vec<Firing>) {
        // Build once per bounded page, rather than re-finding every loaded
        // row for every full RunDetail response. No insertion or reordering.
        let indices: HashMap<_, _> = self
            .history
            .iter()
            .enumerate()
            .map(|(index, firing)| (firing.id, index))
            .collect();
        let newest = self.history.iter().map(history_key).max();
        for firing in rows {
            if let Some(&index) = indices.get(&firing.id) {
                self.history[index] = firing;
            } else if newest.is_some_and(|newest| history_key(&firing) > newest) {
                self.history_newer = true;
            }
        }
    }

    fn refresh_older_history(&mut self, newest: Option<HistoryKey>, more: bool) -> Option<Effect> {
        let scope = self.selected.as_ref().map(|automation| automation.id);
        if (!self.all_history && scope.is_none()) || !more {
            self.history_refresh = None;
            return None;
        }
        let Some(newest) = newest else {
            self.history_refresh = None;
            return None;
        };
        let through = self.history.iter().map(history_key).min()?;
        let mut refresh = self
            .history_refresh
            .filter(|refresh| refresh.scope == scope)
            .unwrap_or(HistoryRefresh {
                scope,
                before: newest,
                through,
            });
        // Compaction can extend the newest page past our old resume point.
        // Never reread the page whose metadata just arrived.
        refresh.before = refresh.before.min(newest);
        let next = self
            .history
            .iter()
            .filter(|firing| firing.state.is_active() || firing.release_pending)
            .map(history_key)
            .filter(|key| *key < refresh.before && *key >= refresh.through)
            .max();
        let Some(next) = next else {
            self.history_refresh = None;
            return None;
        };
        let before = match including_history_key(next) {
            Ok(before) => before,
            Err(error) => {
                self.history_status = HistoryStatus::Failed(error.into());
                self.error = Some(error.into());
                return None;
            }
        };
        // Only one older page is issued per Tick. If it fails or foreground
        // input supersedes it, resume below this actual loaded candidate on a
        // later Tick. It stays visible/unconfirmed and is revisited on wrap;
        // a persistently failing first row cannot starve every later row.
        self.history_refresh = (next > refresh.through).then_some(HistoryRefresh {
            before: next,
            ..refresh
        });
        self.request(
            Pending::RefreshOlderHistory { refresh, before },
            Request::History {
                id: scope,
                before: before.map(history_cursor),
                limit: PAGE,
            },
        )
    }

    fn valid_history_page(
        &self,
        scope: Option<Uuid>,
        before: Option<HistoryKey>,
        page: &crate::automations::HistoryPage,
    ) -> bool {
        let indices: HashMap<_, _> = self
            .history
            .iter()
            .enumerate()
            .map(|(index, firing)| (firing.id, index))
            .collect();
        let mut seen = std::collections::HashSet::new();
        page.firings.len() <= PAGE
            && page.firings.iter().all(|firing| {
                firing.created_ms <= i64::MAX as u64
                    && before.is_none_or(|before| history_key(firing) < before)
                    && scope.is_none_or(|id| firing.automation == id)
                    && seen.insert(firing.id)
                    && indices.get(&firing.id).is_none_or(|&index| {
                        let previous = &self.history[index];
                        previous.automation == firing.automation
                            && previous.created_ms == firing.created_ms
                    })
            })
            && page
                .firings
                .windows(2)
                .all(|pair| history_key(&pair[0]) > history_key(&pair[1]))
            && page.next.as_ref().is_none_or(|cursor| {
                page.firings.len() == PAGE
                    && page
                        .firings
                        .last()
                        .is_some_and(|firing| history_key(firing) == (cursor.created_ms, cursor.id))
            })
    }

    fn accept_older_history(
        &mut self,
        refresh: HistoryRefresh,
        before: Option<HistoryKey>,
        page: crate::automations::HistoryPage,
    ) {
        let current_scope = self.selected.as_ref().map(|automation| automation.id);
        if current_scope != refresh.scope || (!self.all_history && current_scope.is_none()) {
            return;
        }
        let valid = self.valid_history_page(refresh.scope, before, &page);
        if !valid {
            let error = "The older run history reply was inconsistent".to_owned();
            self.history_status = HistoryStatus::Failed(error.clone());
            self.error = Some(error);
            return;
        }
        self.history_status = HistoryStatus::Loaded;
        // A valid response replaces the failure fallback with its actual
        // strict cursor. Empty/exhausted/past-horizon responses end this pass;
        // absent loaded rows never become terminal or disappear.
        self.history_refresh = page
            .next
            .as_ref()
            .filter(|cursor| (cursor.created_ms, cursor.id) > refresh.through)
            .map(|cursor| HistoryRefresh {
                before: (cursor.created_ms, cursor.id),
                ..refresh
            });
        for firing in &page.firings {
            self.refresh_last_outcome(firing, false);
        }
        let indices: HashMap<_, _> = self
            .history
            .iter()
            .enumerate()
            .map(|(index, firing)| (firing.id, index))
            .collect();
        for firing in page.firings {
            if let Some(&index) = indices.get(&firing.id) {
                self.history[index] = firing;
            }
        }
    }

    pub fn update(&mut self, action: Action) -> Option<Effect> {
        if self.lifecycle_notice.is_some() {
            // This surface is historical only. Retained callbacks from the
            // previous editor cannot mutate or open a different run behind it.
            if !matches!(action, Action::Browse | Action::InspectRun(_)) {
                return None;
            }
            self.lifecycle_notice = None;
        }

        if self.discard_draft.is_some() && !action.background() && !action.answers_draft_dialog() {
            return None;
        }
        if let Action::KeepDraft(generation) | Action::DiscardDraft(generation) = &action {
            if self
                .discard_draft
                .as_ref()
                .is_none_or(|(expected, _)| expected != generation)
            {
                return None;
            }
            let (_, next) = self.discard_draft.take().expect("checked draft intent");
            if matches!(action, Action::DiscardDraft(_)) {
                self.draft = None;
                return self.update(next);
            }
            return None;
        }
        if matches!(
            &action,
            Action::Field(..)
                | Action::Provider(..)
                | Action::Model(..)
                | Action::Effort(..)
                | Action::Account(..)
                | Action::Target(..)
                | Action::Schedule(..)
                | Action::Weekday(..)
                | Action::Reuse(..)
                | Action::Precheck(..)
                | Action::Prompt(..)
                | Action::PromptCursor(..)
                | Action::Shell(..)
                | Action::ShellCursor(..)
        ) {
            self.error = None;
        }
        if let Action::Received { sequence, result } = action {
            let Some((expected, pending)) = self.pending else {
                return None;
            };
            if expected != sequence {
                return None;
            }
            self.pending = None;
            let result =
                std::sync::Arc::try_unwrap(result).unwrap_or_else(|result| (*result).clone());
            if let Pending::RefreshHistory { scope } = pending {
                let current_scope = self.selected.as_ref().map(|automation| automation.id);
                if scope != current_scope || (scope.is_none() && !self.all_history) {
                    return None;
                }
                match &result {
                    Ok(Response::History(page)) if self.valid_history_page(scope, None, page) => {}
                    Err(_) => {}
                    _ => {
                        let error = "The newest run history reply was inconsistent".to_owned();
                        self.history_status = HistoryStatus::Failed(error.clone());
                        self.error = Some(error);
                        return None;
                    }
                }
            }
            if let Pending::RefreshOlderHistory { refresh, before } = pending {
                match result {
                    Ok(Response::History(page)) => self.accept_older_history(refresh, before, page),
                    result => {
                        let error = result.err().unwrap_or_else(|| {
                            "The older run history reply was inconsistent".into()
                        });
                        self.history_status = HistoryStatus::Failed(error.clone());
                        self.error = Some(error);
                    }
                }
                return None;
            }
            if let Pending::RefreshLoadedList(refresh) = pending {
                match result {
                    Ok(Response::Listed(rows)) => self.refresh_loaded_list(refresh, rows),
                    result => {
                        let error = result
                            .err()
                            .unwrap_or_else(|| "The automation list reply was inconsistent".into());
                        self.list_status = ListStatus::Failed(error.clone());
                        self.error = Some(error);
                    }
                }
                return None;
            }
            match result {
                Err(error) => {
                    if matches!(
                        pending,
                        Pending::History { .. } | Pending::RefreshHistory { .. }
                    ) {
                        self.history_status = HistoryStatus::Failed(error.clone());
                    }
                    if matches!(
                        pending,
                        Pending::List { append: false } | Pending::RefreshList
                    ) {
                        self.list_status = ListStatus::Failed(error.clone());
                    }
                    self.error = Some(error);
                }
                Ok(Response::Listed(rows)) => {
                    self.list_status = ListStatus::Loaded;
                    self.more = rows.len() == PAGE;
                    self.after = rows.last().map(|row| row.id);
                    if !matches!(pending, Pending::List { append: true }) {
                        self.rows.clear();
                        self.list_changed = false;
                    }
                    self.loaded_list_refresh = None;
                    for row in rows {
                        // The list is authoritative but does not carry the latest
                        // firing identity. Retire our earlier detail-read fence.
                        self.last_firings.remove(&row.id);
                        if let Some(current) =
                            self.rows.iter_mut().find(|current| current.id == row.id)
                        {
                            *current = row;
                        } else {
                            self.rows.push(row);
                        }
                    }
                    self.last_firings
                        .retain(|id, _| self.rows.iter().any(|row| row.id == *id));
                    // Refreshing with an automation open also reads it and its
                    // newest runs; its poll keeps a stable page instead.
                    if matches!(pending, Pending::List { append: false })
                        && let Some(id) = self.selected.as_ref().map(|selected| selected.id)
                    {
                        return self.request(Pending::Reload, Request::Show { id });
                    }
                }
                Ok(Response::Automation(automation)) => {
                    let id = automation.id;
                    if self.selected.as_ref().map(|selected| selected.id) != Some(id) {
                        self.lifecycle = None;
                    }
                    if let Some(row) = self.rows.iter_mut().find(|row| row.id == id) {
                        *row = automation.summary(row.last_state);
                    } else {
                        self.rows.push(automation.summary(None));
                        self.rows.sort_by_key(|row| row.id);
                    }
                    self.selected = Some(automation);
                    if matches!(pending, Pending::Save) {
                        self.draft = None;
                        self.all_history = false;
                        self.history.clear();
                        self.history_newer = false;
                        self.history_status = HistoryStatus::Unrequested;
                        self.history_next = None;
                        self.notice = Some(
                            "Saved. Review the launch settings before enabling or running.".into(),
                        );
                    }
                    // The loaded page stays until this read succeeds, so a
                    // failed reload leaves the poll refreshing it in place.
                    return self.request(
                        if self.history_status == HistoryStatus::Unrequested
                            || matches!(pending, Pending::Reload)
                        {
                            Pending::History { append: false }
                        } else {
                            Pending::RefreshHistory { scope: Some(id) }
                        },
                        Request::History {
                            id: Some(id),
                            before: None,
                            limit: PAGE,
                        },
                    );
                }
                Ok(Response::History(page)) => {
                    let newest_page = !matches!(pending, Pending::History { append: true });
                    for firing in &page.firings {
                        self.refresh_last_outcome(firing, newest_page);
                    }
                    self.history_status = HistoryStatus::Loaded;
                    if matches!(pending, Pending::RefreshHistory { .. }) {
                        // Keep the reader's paginated snapshot stable. Automatically
                        // inserting every new run would grow the mounted list forever
                        // and create gaps between its newest and older pages.
                        let newest = page.firings.iter().map(history_key).min();
                        let more = page.next.is_some();
                        if self.history.is_empty() {
                            self.history = page.firings;
                            self.history_next = page.next;
                            self.history_newer = false;
                        } else {
                            self.merge_history_rows(page.firings);
                        }
                        return self.refresh_older_history(newest, more);
                    }
                    if !matches!(pending, Pending::History { append: true }) {
                        self.history.clear();
                        self.history_newer = false;
                    }
                    for firing in page.firings {
                        if let Some(current) = self
                            .history
                            .iter_mut()
                            .find(|current| current.id == firing.id)
                        {
                            *current = firing;
                        } else {
                            self.history.push(firing);
                        }
                    }
                    self.history.sort_unstable_by_key(|firing| {
                        std::cmp::Reverse((firing.created_ms, firing.id))
                    });
                    self.history_next = page.next;
                }
                Ok(Response::Audit(mut page)) => {
                    if let Pending::Lifecycle { id, append } = pending {
                        if self
                            .selected
                            .as_ref()
                            .is_some_and(|selected| selected.id == id)
                            && page.events.iter().all(|event| event.automation == id)
                        {
                            if append {
                                if let Some(previous) = self.lifecycle.take() {
                                    let mut events = previous.events;
                                    for event in page.events {
                                        if !events
                                            .iter()
                                            .any(|existing| existing.sequence == event.sequence)
                                        {
                                            events.push(event);
                                        }
                                    }
                                    events.truncate(crate::automations::MAX_LIFECYCLE_EVENTS);
                                    page.events = events;
                                }
                            }
                            self.lifecycle = Some(page);
                        }
                    }
                }
                Ok(Response::Detail(detail)) => {
                    self.refresh_last_outcome(&detail.firing, false);
                    if matches!(pending, Pending::OpenRun) {
                        self.run = Some(detail.clone());
                        return Some(Effect::OpenRun(detail));
                    }
                    let run = detail.run.as_ref().map(|run| run.id);
                    self.run = Some(detail);
                    return run.map(Effect::ReadRun);
                }
                Ok(Response::Firing(firing)) => {
                    self.refresh_last_outcome(&firing, true);
                    if let crate::automations::Trigger::Manual { request } = firing.trigger {
                        if self
                            .manual_requests
                            .get(&firing.automation)
                            .is_some_and(|(_, previous)| *previous == request)
                        {
                            self.manual_requests.remove(&firing.automation);
                        }
                    }
                    self.notice = Some(format!("Run requested: {}", state_label(firing.state)));
                    self.history.insert(0, firing);
                }
                Ok(Response::Removed) => {
                    self.lifecycle = None;
                    if let Some(selected) = self.selected.take() {
                        self.rows.retain(|row| row.id != selected.id);
                        self.last_firings.remove(&selected.id);
                    }
                    self.notice = Some("Automation removed. Past runs remain in history.".into());
                }
                Ok(Response::CancellationRequested) => {
                    self.notice = Some(
                        "Cancellation requested. Refresh the run for its confirmed outcome.".into(),
                    )
                }
                Ok(Response::Proposal(_) | Response::Proposals(_)) => {
                    self.error = Some("Unexpected automation approval response".into());
                }
            }
            return None;
        }
        if let Action::InspectRun(run) = action {
            if !self.can_open_run() {
                return None;
            }
            if self
                .run
                .as_ref()
                .and_then(|detail| detail.run.as_ref())
                .map(|current| current.id)
                != Some(run)
            {
                self.notice = None;
            }
            // Notification activation can race a restored card's initial list
            // read. Supersede reads using the request sequence; never discard
            // a user's draft, consent, or an in-flight mutation.
            self.selected = None;
            self.lifecycle = None;
            self.all_history = false;
            self.run = None;
            self.history.clear();
            self.history_newer = false;
            self.history_next = None;
            return self.request(Pending::Run, Request::ShowRun { run });
        }
        if let Some((_, pending)) = self.pending {
            if !pending.background_read() || matches!(action, Action::Tick) {
                return None;
            }
            // User input owns the next request. Its sequence (or no pending
            // request) fences off the superseded read's eventual response.
            self.pending = None;
        }
        if matches!(action, Action::Tick) {
            if self.draft.is_some() || self.confirmation.is_some() {
                return None;
            }
            if let Some(run) = &self.run {
                return self.request(
                    Pending::RefreshRun,
                    Request::ShowFiring { id: run.firing.id },
                );
            }
            if let Some(automation) = &self.selected {
                return self.request(
                    Pending::RefreshAutomation,
                    Request::Show { id: automation.id },
                );
            }
            if self.all_history {
                return self.request(
                    Pending::RefreshHistory { scope: None },
                    Request::History {
                        id: None,
                        before: None,
                        limit: PAGE,
                    },
                );
            }
            if self.rows.len() <= PAGE {
                return self.request(
                    Pending::RefreshList,
                    Request::List {
                        after: None,
                        limit: PAGE,
                    },
                );
            }
            let refresh = self.loaded_list_refresh.unwrap_or(LoadedListRefresh {
                after: None,
                through: self.rows.iter().map(|row| row.id).max()?,
            });
            return self.request(
                Pending::RefreshLoadedList(refresh),
                Request::List {
                    after: refresh.after,
                    limit: PAGE,
                },
            );
        }
        if let Action::Confirm(generation) = action {
            let Some(confirmation) = self.confirmation.take() else {
                return None;
            };
            if confirmation.generation != generation {
                self.confirmation = Some(confirmation);
                return None;
            }
            if let Request::RunNow {
                id,
                execution_revision,
                request,
                ..
            } = &confirmation.request
            {
                self.manual_requests
                    .insert(*id, (*execution_revision, *request));
            }
            return self.request(Pending::Mutate, confirmation.request);
        }
        if matches!(
            action,
            Action::New | Action::Select(_) | Action::Edit | Action::CancelEdit
        ) && self.draft_dirty()
        {
            self.discard_generation = self.discard_generation.wrapping_add(1);
            self.discard_draft = Some((self.discard_generation, action));
            return None;
        }
        self.confirmation = None;
        match action {
            Action::Browse => {
                if self.selected.is_some() || self.run.is_some() || self.all_history {
                    self.notice = None;
                }
                self.run = None;
                self.selected = None;
                self.lifecycle = None;
                self.all_history = false;
                return self.request(
                    Pending::List { append: false },
                    Request::List {
                        after: None,
                        limit: PAGE,
                    },
                );
            }
            Action::Refresh => {
                if let Some(run) = &self.run {
                    return self.request(Pending::Run, Request::ShowFiring { id: run.firing.id });
                }
                if self.all_history {
                    return self.request(
                        Pending::History { append: false },
                        Request::History {
                            id: None,
                            before: None,
                            limit: PAGE,
                        },
                    );
                }
                return self.request(
                    Pending::List { append: false },
                    Request::List {
                        after: None,
                        limit: PAGE,
                    },
                );
            }
            Action::More if self.more => {
                return self.request(
                    Pending::List { append: true },
                    Request::List {
                        after: self.after,
                        limit: PAGE,
                    },
                );
            }
            Action::Search(value) => self.search = value,
            Action::New => {
                self.notice = None;
                self.draft = Some(Draft::new(&self.targets, &self.accounts, None));
                self.run = None;
            }
            Action::Select(id) => {
                if self.selected.as_ref().map(|automation| automation.id) != Some(id)
                    || self.run.is_some()
                    || self.all_history
                {
                    self.notice = None;
                }
                self.draft = None;
                self.all_history = false;
                self.selected = None;
                self.lifecycle = None;
                self.history.clear();
                self.history_newer = false;
                self.history_status = HistoryStatus::Unrequested;
                self.history_next = None;
                self.run = None;
                return self.request(Pending::Show, Request::Show { id });
            }
            Action::Edit => {
                self.draft = self.selected.as_ref().map(|value| {
                    let mut accounts = self.accounts.clone();
                    if let Some(saved) = self.archived_accounts.iter().find(|account| {
                        Some(&account.id) == value.definition.agent.account.as_ref()
                            && account.provider == value.definition.agent.provider
                    }) {
                        accounts.push(saved.clone());
                    }
                    Draft::new(&self.targets, &accounts, Some(value))
                });
                if self.draft.is_some() {
                    self.notice = None;
                }
            }
            Action::CancelEdit => self.draft = None,
            Action::KeepDraft(_) | Action::DiscardDraft(_) => {}
            Action::Save => {
                if let Some(draft) = &self.draft {
                    match draft.definition(&self.models) {
                        Ok(definition) => {
                            let request = match draft.base {
                                Some((id, revision)) => Request::Edit {
                                    id,
                                    revision,
                                    definition,
                                },
                                None => Request::Create { definition },
                            };
                            return self.request(Pending::Save, request);
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
            }
            Action::Field(field, value) => {
                if let Some(draft) = &mut self.draft {
                    *match field {
                        Field::Name => &mut draft.name,
                        Field::Model => &mut draft.model,
                        Field::Permission => &mut draft.permission,
                        Field::Timezone => &mut draft.timezone,
                        Field::Hour => &mut draft.hour,
                        Field::Minute => &mut draft.minute,
                        Field::Cron => &mut draft.cron,
                        Field::Timeout => &mut draft.timeout,
                    } = value;
                    if matches!(field, Field::Timezone) {
                        draft.refresh_timezone_matches();
                    }
                }
            }
            Action::Provider(index) if index < PROVIDERS.len() => {
                if let Some(draft) = &mut self.draft
                    && draft.provider != index
                {
                    draft.provider = index;
                    // An account belongs to one provider; the new one's default
                    // replaces it, even one picked for the previous provider.
                    draft.account_chosen = false;
                    draft.preselect_account();
                    // Model IDs and effort names belong to one provider.
                    draft.choose_model(ModelChoice::Default, &self.models);
                }
            }
            Action::Model(choice) => {
                if let Some(draft) = &mut self.draft {
                    draft.choose_model(choice, &self.models);
                }
            }
            Action::Effort(effort) => {
                if let Some(draft) = &mut self.draft {
                    draft.effort = effort;
                }
            }
            Action::Account(index) => {
                if let Some(draft) = &mut self.draft {
                    draft.account = draft
                        .accounts
                        .iter()
                        .enumerate()
                        .filter(|(_, account)| account.provider == provider(draft.provider))
                        .nth(index)
                        .map(|(index, _)| index);
                    draft.account_chosen = true;
                }
            }
            Action::Target(index) => {
                if let Some(draft) = &mut self.draft {
                    if index < draft.targets.len() {
                        draft.target = Some(index);
                        draft.preselect_account();
                    }
                }
            }
            Action::Schedule(index) if index < SCHEDULES.len() => {
                if let Some(draft) = &mut self.draft {
                    draft.schedule = index;
                }
            }
            Action::Weekday(index) if index < WEEKDAYS.len() => {
                if let Some(draft) = &mut self.draft {
                    draft.weekday = index;
                }
            }
            Action::Reuse(value) => {
                if let Some(draft) = &mut self.draft {
                    draft.reuse = value;
                }
            }
            Action::Precheck(value) => {
                if let Some(draft) = &mut self.draft {
                    draft.precheck = value;
                }
            }
            Action::ToggleAdvanced => {
                if let Some(draft) = &mut self.draft {
                    draft.advanced = !draft.advanced;
                }
            }
            Action::PromptCursor(cursor) => {
                if let Some(draft) = &mut self.draft {
                    draft.prompt.move_to(cursor);
                }
            }
            Action::ShellCursor(cursor) => {
                if let Some(draft) = &mut self.draft {
                    draft.shell.move_to(cursor);
                }
            }
            Action::Prompt(action) => {
                if let Some(draft) = &mut self.draft {
                    draft.prompt.perform(action);
                }
            }
            Action::Shell(action) => {
                if let Some(draft) = &mut self.draft {
                    draft.shell.perform(action);
                }
            }
            Action::Enable | Action::RunNow | Action::Remove => {
                if let Some(automation) = &self.selected {
                    self.sequence = self.sequence.wrapping_add(1);
                    let (request, title) = match action {
                        Action::Enable => (
                            Request::SetEnabled {
                                id: automation.id,
                                revision: automation.revision,
                                enabled: true,
                                confirmed: true,
                            },
                            "Enable scheduled execution?",
                        ),
                        Action::RunNow => (
                            Request::RunNow {
                                id: automation.id,
                                execution_revision: automation.execution_revision,
                                request: self
                                    .manual_requests
                                    .get(&automation.id)
                                    .filter(|(revision, _)| {
                                        *revision == automation.execution_revision
                                    })
                                    .map(|(_, request)| *request)
                                    .unwrap_or_else(Uuid::new_v4),
                                confirmed: true,
                            },
                            "Run this automation now?",
                        ),
                        _ => (
                            Request::Remove {
                                id: automation.id,
                                revision: automation.revision,
                            },
                            "Remove this automation?",
                        ),
                    };
                    self.confirmation = Some(Confirmation {
                        generation: self.sequence,
                        automation: automation.clone(),
                        request,
                        title,
                    });
                }
            }
            Action::Pause => {
                if let Some(automation) = &self.selected {
                    return self.request(
                        Pending::Mutate,
                        Request::SetEnabled {
                            id: automation.id,
                            revision: automation.revision,
                            enabled: false,
                            confirmed: false,
                        },
                    );
                }
            }
            Action::Lifecycle | Action::MoreLifecycle => {
                if let Some(selected) = &self.selected {
                    let id = selected.id;
                    let append = matches!(action, Action::MoreLifecycle);
                    let before = if append {
                        self.lifecycle.as_ref().and_then(|page| page.next)
                    } else {
                        None
                    };
                    if append && before.is_none() {
                        return None;
                    }
                    return self.request(
                        Pending::Lifecycle { id, append },
                        Request::Audit {
                            id,
                            before,
                            limit: PAGE,
                        },
                    );
                }
            }
            Action::History | Action::MoreHistory | Action::AllHistory => {
                if matches!(action, Action::AllHistory) {
                    if !self.all_history || self.selected.is_some() || self.run.is_some() {
                        self.notice = None;
                    }
                    self.all_history = true;
                    self.history.clear();
                    self.history_newer = false;
                    self.history_next = None;
                    self.history_status = HistoryStatus::Unrequested;
                    self.selected = None;
                    self.lifecycle = None;
                    self.run = None;
                }
                if self.all_history || self.selected.is_some() {
                    let append = matches!(action, Action::MoreHistory);
                    return self.request(
                        Pending::History { append },
                        Request::History {
                            id: self.selected.as_ref().map(|automation| automation.id),
                            before: if append {
                                self.history_next.clone()
                            } else {
                                None
                            },
                            limit: PAGE,
                        },
                    );
                }
            }
            Action::Run(id) => {
                if self.run.as_ref().map(|detail| detail.firing.id) != Some(id) {
                    self.notice = None;
                }
                return self.request(Pending::Run, Request::ShowFiring { id });
            }
            Action::CloseRun => {
                if self.run.is_some() {
                    self.notice = None;
                }
                self.run = None;
            }
            Action::CancelRun => {
                if let Some(run) = &self.run {
                    return self.request(
                        Pending::Mutate,
                        Request::Cancel {
                            id: run.firing.automation,
                            firing: run.firing.id,
                        },
                    );
                }
            }
            Action::OpenSession => {
                if let Some(run) = &self.run {
                    return self
                        .request(Pending::OpenRun, Request::ShowFiring { id: run.firing.id });
                }
            }
            _ => {}
        }
        None
    }

    pub fn view(&self) -> Element<'_, Action> {
        crate::ui_controls::responsive(move |size| self.view_layout(size)).into()
    }

    /// Which of the pane's two views is on screen. A run keeps the view it
    /// was opened from.
    fn section(&self) -> Section {
        if self.all_history {
            Section::RunHistory
        } else {
            Section::Automations
        }
    }

    /// What returns the view on screen to its root, while something is open
    /// beneath it: a run, or a selected automation.
    fn section_root(&self) -> Option<Action> {
        match self.section() {
            Section::RunHistory => self.run.is_some().then_some(Action::CloseRun),
            Section::Automations => {
                (self.run.is_some() || self.selected.is_some()).then_some(Action::Browse)
            }
        }
    }

    /// The view switcher's tabs. The view on screen is selected, and choosing
    /// it again returns from whatever is open beneath it; while a command is
    /// in flight no tab publishes.
    fn view_tabs(&self, busy: bool) -> Vec<crate::ui_controls::ViewTab<'static, Action>> {
        let current = self.section();
        let root = self.section_root();
        Section::ALL
            .into_iter()
            .map(|section| {
                let returns = section == current && root.is_some();
                crate::ui_controls::ViewTab {
                    label: section.label().into(),
                    description: if returns {
                        section.back()
                    } else {
                        section.description()
                    }
                    .into(),
                    count: None,
                    selected: section == current,
                    id: None,
                    on_press: if busy {
                        None
                    } else if section == current {
                        root.clone()
                    } else {
                        Some(section.enter())
                    },
                }
            })
            .collect()
    }

    /// The levels above the open run: the view it was opened from, then its
    /// automation. A level still loaded behind the run is returned to as it
    /// was rather than read again.
    ///
    /// The name leads only to an automation that can be shown. Inside Run
    /// history it stays plain: the run may outlive its automation, and
    /// leaving for the other view would discard the loaded page. Opened from
    /// an alert, only a listed automation is linked.
    fn run_trail<'a>(&self, run: &'a RunDetail) -> [(&'a str, Option<Action>); 2] {
        let section = self.section();
        let automation = run.firing.automation;
        let name = match section {
            Section::RunHistory => None,
            Section::Automations
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|selected| selected.id == automation) =>
            {
                Some(Action::CloseRun)
            }
            Section::Automations => self
                .rows
                .iter()
                .any(|row| row.id == automation)
                .then_some(Action::Select(automation)),
        };
        [
            (
                section.label(),
                Some(match section {
                    Section::RunHistory => Action::CloseRun,
                    Section::Automations => Action::Browse,
                }),
            ),
            (run.definition.name.as_str(), name),
        ]
    }

    /// The pane's one header row, above every layout so its controls keep
    /// their place at every width.
    ///
    /// The view switcher leads it: the tab strip already names the pane, and
    /// a title beside an "Automations" tab would say it twice. A draft or a
    /// confirmation takes the switcher's place with its own trail, since each
    /// carries its own way out and its own primary action.
    fn header(&self, busy: bool, width: f32) -> Element<'_, Action> {
        let compact = width < COMPACT_HEADER_WIDTH;
        let view = self.section().label();
        let mut row = widget::row::with_capacity(4)
            .spacing(space::S)
            .align_y(Alignment::Center)
            .height(Length::Fixed(crate::design::PRIMARY_CONTROL_SIZE));
        let opened = |id: Uuid| {
            self.selected
                .as_ref()
                .filter(|selected| selected.id == id)
                .map(|selected| selected.definition.name.as_str())
        };
        let trail = if let Some(draft) = &self.draft {
            // An edit returns to the automation it changes; a new draft to
            // the view it was started from.
            Some(match draft.base.and_then(|(id, _)| opened(id)) {
                Some(name) => breadcrumb(
                    [(view, None), (name, Some(Action::CancelEdit))],
                    "Edit",
                    Some(1),
                    busy,
                ),
                None => breadcrumb(
                    [(view, Some(Action::CancelEdit))],
                    if draft.base.is_some() {
                        "Edit automation"
                    } else {
                        "New automation"
                    },
                    None,
                    busy,
                ),
            })
        } else {
            self.confirmation.as_ref().map(|confirmation| {
                breadcrumb(
                    [
                        (view, None),
                        (
                            confirmation.automation.definition.name.as_str(),
                            Some(Action::CancelConfirmation),
                        ),
                    ],
                    match confirmation.request {
                        Request::SetEnabled { .. } => "Enable",
                        Request::RunNow { .. } => "Run now",
                        _ => "Remove",
                    },
                    Some(1),
                    busy,
                )
            })
        };
        let progress = self
            .pending
            .filter(|_| busy)
            .map(|(_, pending)| progress_label(pending));
        let Some(trail) = trail else {
            row = row
                .push(crate::ui_controls::nested_view_tabs(
                    self.view_tabs(busy),
                    compact,
                ))
                .push(self.refresh_control(progress));
            let new = (!busy).then_some(Action::New);
            // A narrow pane keeps one row by naming the command in its
            // tooltip instead of its label.
            let button: Element<'_, Action> = if compact {
                crate::ui_controls::icon_button(crate::ui_controls::Icon::Plus, "New automation")
                    .on_press_maybe(new)
                    .into()
            } else {
                crate::ui_controls::button::primary("New automation")
                    .on_press_maybe(new)
                    .into()
            };
            return widget::container(row.push(button))
                .padding([space::S, space::L])
                .width(Length::Fill)
                .into();
        };
        row = row.push(trail);
        if let Some(progress) = progress {
            row = row.push(crate::interaction::tooltip(
                widget::container(crate::ui_controls::activity_spinner(
                    14.0,
                    crate::design::muted_color,
                ))
                .center(Length::Fixed(crate::design::PRIMARY_CONTROL_SIZE)),
                progress,
            ));
        }
        widget::container(row)
            .padding([space::S, space::L])
            .width(Length::Fill)
            .into()
    }

    /// Refresh, which stays the same control while any foreground request
    /// is in flight, whether Refresh started it or not ("Opening agent…"):
    /// disabled, with progress in place of its glyph and the request as its
    /// name, so a keyboard user's focus stays on it.
    ///
    /// Its tooltip belongs to the glyph rather than the button, so it can
    /// still name the request while the button is disabled.
    fn refresh_control(&self, progress: Option<&'static str>) -> Element<'_, Action> {
        let label = progress.unwrap_or(if self.run.is_some() {
            "Refresh run"
        } else if self.all_history {
            "Refresh run history"
        } else if self.list_changed {
            "Automation list changed; refresh to show additions or removals"
        } else {
            "Refresh automations"
        });
        let glyph: Element<'_, Action> = match progress {
            Some(_) => crate::ui_controls::activity_spinner(14.0, crate::design::muted_color),
            None => widget::icon(crate::ui_controls::Icon::Refresh.handle())
                .size(crate::design::ROW_ICON_SIZE)
                .into(),
        };
        widget::button::custom(crate::interaction::tooltip(
            widget::container(glyph)
                .class(crate::design::transparent())
                .center(Length::Fill),
            label,
        ))
        .id(refresh_id())
        .name(label)
        .class(crate::ui_controls::control_style(
            self.list_changed && self.run.is_none() && !self.all_history,
            None,
        ))
        .width(crate::design::PRIMARY_CONTROL_SIZE)
        .height(crate::design::PRIMARY_CONTROL_SIZE)
        .padding(0)
        .on_press_maybe(progress.is_none().then_some(Action::Refresh))
        .into()
    }

    fn view_layout(&self, size: cosmic::iced::Size) -> Element<'_, Action> {
        if let Some(receipt) = &self.lifecycle_notice {
            let mut content = widget::column::with_children(vec![
                button("Back to automations", Action::Browse, true),
                crate::design::title("Automation lifecycle receipt").into(),
                crate::design::body(format!(
                    "Automation {} · Receipt {}",
                    receipt.id, receipt.sequence
                ))
                .into(),
            ])
            .spacing(space::M)
            .width(Length::Fill);
            if let Some(event) = &receipt.event {
                content = content
                    .push(crate::design::heading(event.cause.label()))
                    .push(crate::design::body(format!("{} · {} · Revision {}",
                        timestamp(event.occurred_ms), event.actor.label(), event.revision)))
                    .push(crate::design::body("This records a past system event. It does not describe the automation's current status."));
            } else {
                content = content
                    .push(crate::design::heading("This exact receipt is unavailable"))
                    .push(crate::design::body("It may have expired from retained history or no longer be available on this backend. The original notice remains in Attention."));
            }
            return widget::scrollable(content.padding(space::L))
                .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
                    crate::design::scrollbar(),
                ))
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        }

        let width = size.width;
        let busy = self
            .pending
            .is_some_and(|(_, pending)| !pending.background_read());
        let mut listing: Vec<Element<'_, Action>> = Vec::new();
        // A pane that only states it is empty centers that statement rather
        // than scrolling it.
        let mut scrolls = true;
        let mut content = widget::column::with_capacity(8)
            .spacing(space::M)
            .width(Length::Fill);
        // A failed read that left nothing to show is stated in place, beside
        // its retry, rather than above it as well.
        let browsing = self.draft.is_none() && self.confirmation.is_none() && self.run.is_none();
        // The list's own failure, while it is the list's whole content.
        let list_failure = match &self.list_status {
            ListStatus::Failed(error)
                if browsing
                    && !self.all_history
                    && self.selected.is_none()
                    && self.rows.is_empty()
                    && !busy =>
            {
                Some(error.as_str())
            }
            _ => None,
        };
        // The history's own failure, while it has nothing else to show.
        let history_failure = match &self.history_status {
            HistoryStatus::Failed(error)
                if browsing
                    && (self.all_history || self.selected.is_some())
                    && self.history.is_empty() =>
            {
                Some(error.as_str())
            }
            _ => None,
        };
        if let Some(error) = &self.error
            && self.draft.is_none()
            && list_failure != Some(error.as_str())
            && history_failure != Some(error.as_str())
        {
            content = content.push(
                crate::design::body(error).class(cosmic::theme::Text::Custom(|theme| {
                    cosmic::iced::widget::text::Style {
                        color: Some(crate::design::destructive_color(theme)),
                        ..Default::default()
                    }
                })),
            );
        }
        if let Some(notice) = &self.notice {
            content = content.push(crate::design::body(notice));
        }
        if let Some(confirmation) = &self.confirmation {
            content =
                content
                    .push(crate::design::title(confirmation.title))
                    .push(launch_description(
                        &confirmation.automation,
                        self.accounts.iter().chain(&self.archived_accounts),
                    ));
            content = content.push(crate::design::body(if matches!(confirmation.request, Request::Remove { .. }) {
                "Past runs remain in history. Active or uncertain work must be closed before removal."
            } else {
                "The agent may change files and use tools allowed by these permissions. Confirm only this displayed launch configuration."
            }));
            content = content.push(
                widget::row::with_children(vec![
                    button("Cancel", Action::CancelConfirmation, !busy),
                    if matches!(confirmation.request, Request::Remove { .. }) {
                        crate::ui_controls::button::destructive("Remove automation")
                            .on_press_maybe(
                                (!busy).then_some(Action::Confirm(confirmation.generation)),
                            )
                            .into()
                    } else {
                        button("Confirm", Action::Confirm(confirmation.generation), !busy)
                    },
                ])
                .spacing(space::S)
                .wrap(),
            );
        } else if let Some(draft) = &self.draft {
            content = content.push(draft.view(busy, &self.models, self.error.as_deref(), width));
        } else {
            if self.run.is_none() {
                if !self.all_history {
                    let query = self.search.to_lowercase();
                    for row in self.rows.iter().filter(|row| {
                        row.name.to_lowercase().contains(&query)
                            || row
                                .directory
                                .to_string_lossy()
                                .to_lowercase()
                                .contains(&query)
                    }) {
                        let next = row
                            .next
                            .as_ref()
                            .map(|next| next.local.as_str())
                            .unwrap_or("—");
                        let selected = self
                            .selected
                            .as_ref()
                            .is_some_and(|selected| selected.id == row.id);
                        let mut row_content = widget::column::with_children(vec![
                            widget::button::custom(
                                widget::column::with_children(vec![
                                    crate::design::heading(&row.name).into(),
                                    crate::design::caption(if row.enabled {
                                        "Enabled"
                                    } else {
                                        "Paused"
                                    })
                                    .into(),
                                    crate::design::caption(row.directory.display().to_string())
                                        .into(),
                                    crate::design::caption(format!("Next: {next}")).into(),
                                    crate::design::caption(format!(
                                        "Last: {}",
                                        row.last_state.map(state_label).unwrap_or("No runs")
                                    ))
                                    .into(),
                                ])
                                .spacing(space::XS),
                            )
                            .name(format!("Select automation {}", row.name))
                            .class(crate::sidebar::sidebar_button_style(selected))
                            .selected(selected)
                            .padding(space::S)
                            .height(Length::Shrink)
                            .width(Length::Fill)
                            .on_press_maybe((!busy).then_some(Action::Select(row.id)))
                            .into(),
                        ])
                        .spacing(space::XS)
                        .width(Length::Fill);
                        if let Some(blocked) = &row.blocked {
                            row_content = row_content.push(crate::design::caption(blocked));
                        }
                        listing.push(crate::ui_controls::owned(row.id, row_content));
                    }
                    if listing.is_empty() && !self.rows.is_empty() {
                        listing.push(
                            crate::design::caption(if self.more {
                                "No loaded automations match."
                            } else {
                                "No automations match."
                            })
                            .into(),
                        );
                    }
                    if self.more {
                        listing.push(button("Load more automations", Action::More, !busy));
                    }
                    // A read in flight is announced by the header's progress,
                    // so no statement anticipates the list or selection it is
                    // about to replace. Rows already on screen keep theirs.
                    let empty = if self.selected.is_some()
                        || matches!(self.pending, Some((_, Pending::Show | Pending::Reload)))
                    {
                        None
                    } else if !self.rows.is_empty() {
                        Some(
                            crate::ui_controls::placeholder("Select an automation")
                                .detail("Its schedule, launch settings and runs appear here."),
                        )
                    } else if busy {
                        None
                    } else if let Some(error) = list_failure {
                        Some(
                            crate::ui_controls::placeholder("Couldn't load automations")
                                .icon(crate::ui_controls::Icon::Warning.handle())
                                .detail(error)
                                .action(
                                    crate::ui_controls::button::secondary("Retry")
                                        .on_press(Action::Refresh),
                                ),
                        )
                    } else if self.list_status == ListStatus::Unread {
                        // Another read, such as an alert's run, took the
                        // list's place before it was read.
                        Some(
                            crate::ui_controls::placeholder("Automations aren't loaded")
                                .icon(crate::ui_controls::Icon::Alarm.handle())
                                .action(
                                    crate::ui_controls::button::secondary("Load automations")
                                        .on_press(Action::Refresh),
                                ),
                        )
                    } else {
                        Some(
                            crate::ui_controls::placeholder("No automations yet")
                                .icon(crate::ui_controls::Icon::Alarm.handle())
                                .detail(
                                    "Run an agent on a schedule in an open project. Automations keep running while this window is closed.",
                                ),
                        )
                    };
                    if let Some(empty) = empty {
                        scrolls = false;
                        content = content.push(empty);
                    }
                    if let Some(selected) = &self.selected {
                        content = content
                            .push(crate::design::title(selected.definition.name.as_str()))
                            .push(launch_description(
                                selected,
                                self.accounts.iter().chain(&self.archived_accounts),
                            ))
                            .push(crate::design::body(format!(
                                "{} · Next: {} ({})",
                                if selected.enabled {
                                    "Enabled"
                                } else {
                                    "Paused"
                                },
                                selected
                                    .next
                                    .as_ref()
                                    .map(|next| next.local.as_str())
                                    .unwrap_or("—"),
                                selected.definition.schedule.timezone
                            )));
                        if let Some(blocked) = &selected.blocked {
                            content = content.push(crate::design::body(blocked));
                        }
                        content = content.push(
                            widget::row::with_children(vec![
                                button("Edit", Action::Edit, !busy),
                                if selected.enabled {
                                    button("Pause", Action::Pause, !busy)
                                } else {
                                    button(
                                        "Enable…",
                                        Action::Enable,
                                        !busy && selected.blocked.is_none(),
                                    )
                                },
                                button(
                                    if self.manual_requests.get(&selected.id).is_some_and(
                                        |(revision, _)| *revision == selected.execution_revision,
                                    ) {
                                        "Retry run…"
                                    } else {
                                        "Run now…"
                                    },
                                    Action::RunNow,
                                    !busy
                                        && selected.active_firing.is_none()
                                        && selected.blocked.is_none(),
                                ),
                                button(
                                    "Remove…",
                                    Action::Remove,
                                    !busy
                                        && selected.active_firing.is_none()
                                        && selected.blocked.is_none(),
                                ),
                            ])
                            .spacing(space::S)
                            .wrap(),
                        );
                    }
                }
                if self.selected.is_some() {
                    content = content.push(crate::design::title("Lifecycle")).push(button(
                        "Refresh lifecycle",
                        Action::Lifecycle,
                        !busy,
                    ));
                    if let Some(page) = &self.lifecycle {
                        if page.events.is_empty() {
                            content =
                                content.push(crate::design::body("No recorded lifecycle events."));
                        }
                        for event in &page.events {
                            let actor = event.actor.label();
                            content = content.push(crate::design::body(format!(
                                "{} · {} · {} · Revision {}",
                                timestamp(event.occurred_ms),
                                event.cause.label(),
                                actor,
                                event.revision,
                            )));
                            if let Some(firing) = event.firing {
                                content = content.push(button_owned(
                                    format!("Inspect run {firing}"),
                                    Action::Run(firing),
                                    !busy,
                                ));
                            }
                        }
                        if page.earlier_events_unavailable {
                            content = content.push(crate::design::body(
                                "Earlier lifecycle events are unavailable. Their actor and cause are unknown.",
                            ));
                        }
                        if page.next.is_some()
                            && page.events.len() < crate::automations::MAX_LIFECYCLE_EVENTS
                        {
                            content = content.push(button(
                                "Older lifecycle events",
                                Action::MoreLifecycle,
                                !busy,
                            ));
                        }
                    } else {
                        content = content.push(crate::design::body(
                            "Load lifecycle receipts to inspect changes to this automation.",
                        ));
                    }
                }
                if self.selected.is_some() || self.all_history {
                    if self.selected.is_some() {
                        content = content.push(crate::design::title("Run history"));
                    }
                    if self.history_newer {
                        content = content.push(crate::design::body(
                            "Newer runs are available. Refresh to show the latest page.",
                        ));
                    }
                    let empty = match &self.history_status {
                        HistoryStatus::Loading if self.history.is_empty() => {
                            Some(crate::ui_controls::placeholder("Loading run history…"))
                        }
                        HistoryStatus::Failed(error) if self.history.is_empty() => Some(
                            crate::ui_controls::placeholder("Couldn't load run history")
                                .icon(crate::ui_controls::Icon::Warning.handle())
                                .detail(error)
                                .action(
                                    crate::ui_controls::button::secondary("Retry")
                                        .on_press_maybe((!busy).then_some(Action::History)),
                                ),
                        ),
                        HistoryStatus::Failed(_) => {
                            content = content.push(crate::design::caption(
                                "Couldn't refresh run history. These runs may be out of date.",
                            ));
                            None
                        }
                        HistoryStatus::Loaded if self.history.is_empty() => Some(
                            crate::ui_controls::placeholder("No runs yet")
                                .icon(crate::ui_controls::Icon::History.handle())
                                .detail(if self.all_history {
                                    "Runs of every automation appear here, newest first."
                                } else {
                                    "Runs appear here when the schedule fires or you choose Run now."
                                }),
                        ),
                        HistoryStatus::Unrequested
                        | HistoryStatus::Loading
                        | HistoryStatus::Loaded => None,
                    };
                    if let Some(empty) = empty {
                        // History alone centers its state; under a selected
                        // automation it follows the details.
                        if self.all_history {
                            scrolls = false;
                            content = content.push(empty);
                        } else {
                            content = content.push(empty.height(Length::Shrink));
                        }
                    }
                    for firing in &self.history {
                        let origin = match &firing.trigger {
                            crate::automations::Trigger::Manual { .. } => "Manual".to_owned(),
                            crate::automations::Trigger::Scheduled { occurrence } => {
                                format!("Scheduled {}", occurrence.local)
                            }
                        };
                        content = content.push(button_owned(
                            format!(
                                "{} · {} · {} · {}{}",
                                firing.name,
                                timestamp(firing.created_ms),
                                origin,
                                state_label(firing.state),
                                if firing.release_pending {
                                    " · Session release pending"
                                } else {
                                    ""
                                }
                            ),
                            Action::Run(firing.id),
                            !busy,
                        ));
                    }
                    if self.history_next.is_some() {
                        content = content.push(button("Older runs", Action::MoreHistory, !busy));
                    }
                }
            }
            if let Some(run) = &self.run {
                content = content.push(breadcrumb(self.run_trail(run), "Run", Some(1), busy));
                content = content
                    .push(crate::design::title(format!(
                        "{} · {}",
                        run.definition.name,
                        state_label(run.firing.state)
                    )))
                    .push(crate::design::body(format!(
                        "Started: {}\nTarget: {}",
                        timestamp(run.firing.created_ms),
                        run.definition.agent.target.directory.display()
                    )));
                if let Some(finished) = run.firing.finished_ms {
                    content = content.push(crate::design::body(format!(
                        "Finished: {}",
                        timestamp(finished)
                    )));
                }
                if run.firing.release_pending {
                    content = content.push(crate::design::body(
                        "Session release pending. New runs cannot start until cleanup finishes.",
                    ));
                }
                if let Some(diagnostic) = &run.firing.diagnostic {
                    content = content.push(crate::design::body(diagnostic));
                }
                if let Some(result) = run.run.as_ref().map(|run| &run.result) {
                    content = content.push(crate::design::body(result.text.as_str()));
                    if let Some(diagnostic) = &result.diagnostic {
                        content = content.push(crate::design::body(diagnostic));
                    }
                    if result.truncated {
                        content = content.push(crate::design::body(
                            "This result was shortened to keep history bounded.",
                        ));
                    }
                }
                content = content.push(
                    widget::row::with_children(vec![
                        button(
                            "Cancel run",
                            Action::CancelRun,
                            !busy && run.firing.state.is_active(),
                        ),
                        button(
                            "Open agent",
                            Action::OpenSession,
                            !busy
                                && run
                                    .agent
                                    .as_ref()
                                    .is_some_and(|agent| !agent.closed && agent.session.is_some()),
                        ),
                    ])
                    .spacing(space::S)
                    .wrap(),
                );
            }
        }
        let vertical =
            || cosmic::iced::widget::scrollable::Direction::Vertical(crate::design::scrollbar());
        let content: Element<'_, Action> = if scrolls {
            widget::scrollable(content.padding(crate::design::with_vertical_rail(
                cosmic::iced::Padding::from(space::L),
            )))
            .direction(vertical())
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
        } else {
            content.padding(space::L).height(Length::Fill).into()
        };
        let body = if listing.is_empty() {
            content
        } else {
            let mut search = crate::ui_controls::field::search("Filter automations", &self.search)
                .on_input(Action::Search)
                .width(Length::Fill);
            if !self.search.is_empty() {
                search = search.trailing_icon(crate::ui_controls::field::clear(Action::Search(
                    String::new(),
                )));
            }
            let side_by_side = width >= SIDE_BY_SIDE_WIDTH;
            // The field stays put while its rows scroll beneath it.
            let list = widget::column::with_children(vec![
                widget::container(search)
                    .padding([space::S, space::S, 0.0, space::S])
                    .into(),
                widget::scrollable(
                    widget::column::with_children(listing)
                        .spacing(space::XS)
                        .padding(crate::design::with_vertical_rail(
                            cosmic::iced::Padding::from([0.0, space::S, space::S, space::S]),
                        )),
                )
                .direction(vertical())
                .height(if side_by_side {
                    Length::Fill
                } else {
                    Length::Shrink
                })
                .into(),
            ])
            .spacing(space::XS);
            if side_by_side {
                widget::row::with_children(vec![
                    widget::container(list)
                        .width(LIST_WIDTH)
                        .height(Length::Fill)
                        .into(),
                    crate::design::vertical_rule(),
                    content,
                ])
                .height(Length::Fill)
                .into()
            } else {
                // Stacked, the list takes the height of its rows up to a
                // share of the pane, leaving the rest to the selection.
                widget::column::with_children(vec![
                    widget::container(list)
                        .width(Length::Fill)
                        .max_height(size.height * STACKED_LIST_SHARE)
                        .into(),
                    crate::design::horizontal_rule(),
                    content,
                ])
                .height(Length::Fill)
                .into()
            }
        };
        widget::column::with_children(vec![
            self.header(busy, width),
            crate::design::horizontal_rule(),
            body,
        ])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
}

impl Draft {
    fn view<'a>(
        &'a self,
        busy: bool,
        models: &'a Models,
        error: Option<&'a str>,
        width: f32,
    ) -> Element<'a, Action> {
        let provider_accounts = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| account.provider == provider(self.provider))
            .collect::<Vec<_>>();
        let work = widget::column::with_children(vec![
            input("Name", &self.name, Field::Name),
            crate::design::caption("Project / working directory").into(),
            crate::ui_controls::field::select(
                self.targets
                    .iter()
                    .map(|target| target.label.clone())
                    .collect::<Vec<_>>(),
                self.target,
                Action::Target,
            )
            .into(),
            crate::design::caption("Provider").into(),
            crate::ui_controls::field::select(
                PROVIDERS.to_vec(),
                Some(self.provider),
                Action::Provider,
            )
            .into(),
            crate::design::caption("Account").into(),
            crate::ui_controls::field::select(
                provider_accounts
                    .iter()
                    .map(|(_, account)| account.label.clone())
                    .collect::<Vec<_>>(),
                provider_accounts
                    .iter()
                    .position(|(index, _)| Some(*index) == self.account),
                Action::Account,
            )
            .into(),
            self.model_picker(models),
            self.effort_picker(models),
            crate::design::caption("Instructions").into(),
            crate::ui_controls::field::editor("Automation instructions", &self.prompt)
                .on_action(Action::Prompt)
                .on_cursor(Action::PromptCursor)
                .height(220)
                .into(),
        ])
        .spacing(space::S)
        .width(Length::Fill);
        let mut schedule = widget::column::with_children(vec![
            crate::design::caption("Repeat").into(),
            crate::ui_controls::field::select(
                SCHEDULES.to_vec(),
                Some(self.schedule),
                Action::Schedule,
            )
            .into(),
        ])
        .spacing(space::S)
        .width(Length::Fill);
        if self.schedule == 4 {
            schedule = schedule.push(input("Five-field cron", &self.cron, Field::Cron));
        } else {
            let mut time = Vec::new();
            if self.schedule != 0 {
                time.push(input("Hour (0–23)", &self.hour, Field::Hour));
            }
            time.push(input("Minute (0–59)", &self.minute, Field::Minute));
            schedule = schedule.push(widget::row::with_children(time).spacing(space::S));
            if self.schedule == 3 {
                schedule = schedule.push(crate::ui_controls::field::select(
                    WEEKDAYS.to_vec(),
                    Some(self.weekday),
                    Action::Weekday,
                ));
            }
        }
        schedule = schedule
            .push(self.timezone_picker())
            .push(self.next_run())
            .push(crate::design::horizontal_rule())
            .push(
                crate::ui_controls::field::checkbox(self.reuse)
                    .label("Continue in the same agent")
                    .on_toggle(Action::Reuse),
            )
            .push(crate::design::caption(
                "Otherwise, each run starts a fresh session. Taking over the agent pauses reuse.",
            ));
        let work = form_section("Work", "What the agent should do", work.into());
        let schedule = form_section("Schedule", "When it should run", schedule.into());
        let layout: Element<'a, Action> = if width >= 820.0 {
            widget::row::with_children(vec![
                widget::container(work).width(Length::FillPortion(2)).into(),
                widget::container(schedule)
                    .width(Length::FillPortion(1))
                    .into(),
            ])
            .spacing(space::L)
            .into()
        } else {
            widget::column::with_children(vec![work, schedule])
                .spacing(space::L)
                .into()
        };
        let mut fields = widget::column::with_children(vec![
            crate::design::heading(if self.base.is_some() {
                "Edit automation"
            } else {
                "New automation"
            })
            .into(),
            layout,
            widget::button::custom(crate::design::caption(if self.advanced {
                "▾ Advanced settings"
            } else {
                "▸ Advanced settings"
            }))
            // The disclosure glyph is decorative; say what pressing it does.
            .name(if self.advanced {
                "Hide advanced settings"
            } else {
                "Show advanced settings"
            })
            .class(crate::ui_controls::control_style(false, None))
            .on_press(Action::ToggleAdvanced)
            .into(),
        ])
        .spacing(space::L)
        .width(Length::Fill);
        if self.advanced {
            let mut advanced = widget::column::with_children(vec![
                input("Permission mode", &self.permission, Field::Permission),
                crate::design::caption("Leave blank to use the provider default.").into(),
                crate::ui_controls::field::checkbox(self.precheck)
                    .label("Run a shell precheck")
                    .on_toggle(Action::Precheck)
                    .into(),
            ])
            .spacing(space::S);
            if self.precheck {
                advanced = advanced
                    .push(crate::design::caption(
                        "Runs in the selected directory. A non-zero exit skips the agent.",
                    ))
                    .push(
                        crate::ui_controls::field::editor("Precheck shell command", &self.shell)
                            .on_action(Action::Shell)
                            .on_cursor(Action::ShellCursor)
                            .height(120),
                    )
                    .push(input(
                        "Timeout in seconds (1–300)",
                        &self.timeout,
                        Field::Timeout,
                    ));
            }
            fields = fields.push(form_section(
                "Execution",
                "Optional permissions and precheck",
                advanced.into(),
            ));
        }
        fields = fields.push(crate::design::caption("New automations start paused. Changes to schedule or execution settings pause existing automations. Runs never overlap; missed runs older than 15 minutes are skipped."));
        if let Some(error) = error {
            fields = fields.push(
                crate::design::body(error).class(cosmic::theme::Text::Custom(|theme| {
                    cosmic::iced::widget::text::Style {
                        color: Some(crate::design::destructive_color(theme)),
                        ..Default::default()
                    }
                })),
            );
        }
        let fields = fields.push(
            widget::row::with_children(vec![
                button("Cancel", Action::CancelEdit, !busy),
                crate::ui_controls::button::primary("Save automation")
                    .on_press_maybe((!busy).then_some(Action::Save))
                    .into(),
            ])
            .spacing(space::S)
            .wrap(),
        );
        widget::container(fields)
            .max_width(1040)
            .width(Length::Fill)
            .into()
    }
}

fn form_section<'a>(
    title: &'static str,
    subtitle: &'static str,
    content: Element<'a, Action>,
) -> Element<'a, Action> {
    widget::container(
        widget::column::with_children(vec![
            crate::design::heading(title).into(),
            crate::design::caption(subtitle).into(),
            crate::design::horizontal_rule(),
            content,
        ])
        .spacing(space::M),
    )
    .padding(space::L)
    .width(Length::Fill)
    .class(crate::design::surface(
        Some(crate::design::raised),
        [crate::design::RADIUS_M; 4],
        false,
    ))
    .into()
}

impl Draft {
    /// The timezone is typed to search the system's zones; a match fills it.
    fn timezone_picker(&self) -> Element<'_, Action> {
        let mut column = widget::column::with_children(vec![
            crate::design::body("Timezone").into(),
            crate::ui_controls::field::search("Search timezones, e.g. Vancouver", &self.timezone)
                .on_input(|value| Action::Field(Field::Timezone, value))
                .into(),
        ])
        .spacing(space::XS);
        if let Some(matches) = &self.timezone_matches {
            if matches.is_empty() {
                column = column.push(crate::design::caption(if self.timezone.trim().is_empty() {
                    "Type a city or region to find its timezone."
                } else {
                    "No timezone matches this search."
                }));
            }
            for name in matches.iter().copied() {
                column = column.push(
                    widget::button::custom(crate::design::body(name))
                        .name(name)
                        .class(crate::ui_controls::control_style(false, None))
                        .width(Length::Fill)
                        .on_press(Action::Field(Field::Timezone, name.to_owned())),
                );
            }
        }
        column.into()
    }

    /// When the schedule as entered runs next, or why it cannot run.
    fn next_run(&self) -> Element<'_, Action> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        let text = self
            .schedule()
            .and_then(|schedule| {
                crate::automations::schedule::describe_next(
                    &schedule,
                    now,
                    &jiff::tz::TimeZone::system(),
                )
            })
            .map_or_else(|error| error, |next| format!("Next run: {next}"));
        crate::design::caption(text).into()
    }

    /// The provider's listed models, as the composer offers them, or a typed
    /// ID for one it does not list. Before a provider has listed any, the
    /// model is typed.
    fn model_picker<'a>(&'a self, models: &'a Models) -> Element<'a, Action> {
        let listed = models
            .get(&provider(self.provider))
            .map(Vec::as_slice)
            .unwrap_or_default();
        if listed.is_empty() {
            return input(
                "Model (blank uses provider default)",
                &self.model,
                Field::Model,
            );
        }
        let custom = self.custom(models);
        let labels = std::iter::once("Provider default".to_owned())
            .chain(listed.iter().map(|model| model.label.clone()))
            .chain(std::iter::once("Custom model ID…".to_owned()))
            .collect::<Vec<_>>();
        let selected = if custom {
            labels.len() - 1
        } else {
            self.listed(models)
                .and_then(|model| listed.iter().position(|entry| entry.id == model.id))
                .map_or(0, |index| index + 1)
        };
        let ids = listed
            .iter()
            .map(|model| model.id.clone())
            .collect::<Vec<_>>();
        let mut column = widget::column::with_children(vec![
            crate::design::body("Model").into(),
            crate::ui_controls::field::select(labels, Some(selected), move |index| {
                Action::Model(match index {
                    0 => ModelChoice::Default,
                    index => ids
                        .get(index - 1)
                        .cloned()
                        .map_or(ModelChoice::Custom, ModelChoice::Listed),
                })
            })
            .into(),
        ])
        .spacing(space::XS);
        if custom {
            column = column.push(
                crate::ui_controls::field::text("Model ID", &self.model)
                    .on_input(|value| Action::Field(Field::Model, value)),
            );
        }
        column.into()
    }

    /// The efforts the chosen model advertises. The provider default, or a
    /// model that advertises none, runs at the provider's default effort.
    fn effort_picker<'a>(&'a self, models: &'a Models) -> Element<'a, Action> {
        let efforts = self.efforts(models);
        if efforts.is_empty() {
            return widget::column::with_children(vec![
                crate::design::body("Reasoning effort").into(),
                crate::design::caption(if self.listed(models).is_some() {
                    "Provider default. This model does not offer a choice."
                } else {
                    "Provider default. Choose a listed model to pick an effort."
                })
                .into(),
            ])
            .spacing(space::XS)
            .into();
        }
        let selected = self
            .effort
            .as_ref()
            .and_then(|effort| efforts.iter().position(|entry| entry == effort))
            .map_or(0, |index| index + 1);
        let labels = std::iter::once("Provider default".to_owned())
            .chain(efforts.iter().cloned())
            .collect::<Vec<_>>();
        widget::column::with_children(vec![
            crate::design::body("Reasoning effort").into(),
            crate::ui_controls::field::select(labels, Some(selected), move |index| {
                Action::Effort(
                    index
                        .checked_sub(1)
                        .and_then(|index| efforts.get(index).cloned()),
                )
            })
            .into(),
        ])
        .spacing(space::XS)
        .into()
    }
}

fn launch_description<'a>(
    automation: &'a Automation,
    mut accounts: impl Iterator<Item = &'a AccountChoice>,
) -> Element<'a, Action> {
    let definition = &automation.definition;
    let account = accounts
        .find(|account| Some(&account.id) == definition.agent.account.as_ref())
        .map(|account| account.label.as_str())
        .unwrap_or("Saved account is unavailable");
    let description = format!(
        "Provider: {}\nAccount: {}\nProject: {}\nDirectory: {}\nModel: {}\nReasoning effort: {}\nPermissions: {}\nSession: {}\nSchedule: {} ({})\nRevision: {}",
        PROVIDERS[provider_index(definition.agent.provider)],
        account,
        definition.agent.target.repository.display(),
        definition.agent.target.directory.display(),
        definition
            .agent
            .model
            .as_deref()
            .unwrap_or("Provider default"),
        definition
            .agent
            .reasoning_effort
            .as_deref()
            .unwrap_or("Provider default"),
        definition
            .agent
            .permission_mode
            .as_deref()
            .unwrap_or("Provider default"),
        if definition.session_mode == SessionMode::Fresh {
            "Fresh"
        } else {
            "Reuse this automation's agent"
        },
        definition
            .schedule
            .pattern
            .expression()
            .unwrap_or_else(|_| "Invalid schedule".into()),
        definition.schedule.timezone,
        automation.revision
    );
    let mut column = widget::column::with_children(vec![
        crate::design::body(description).into(),
        crate::design::body(definition.prompt.as_str()).into(),
    ])
    .spacing(space::S);
    if let Some(precheck) = &definition.precheck {
        column = column.push(crate::design::body(format!(
            "Shell precheck ({} seconds):\n{}",
            precheck.timeout_seconds, precheck.command
        )));
    }
    column.into()
}
/// What the header's progress says a command in flight is doing.
fn progress_label(pending: Pending) -> &'static str {
    match pending {
        Pending::List { .. } => "Loading automations…",
        Pending::Show => "Loading automation…",
        Pending::Reload => "Refreshing automation…",
        Pending::Save => "Saving automation…",
        Pending::Mutate => "Updating automation…",
        Pending::History { .. } => "Loading run history…",
        Pending::Lifecycle { .. } => "Loading lifecycle…",
        Pending::Run => "Loading run…",
        Pending::OpenRun => "Opening agent…",
        // Background reads draw no progress, but name themselves should one
        // ever be shown.
        Pending::RefreshList | Pending::RefreshLoadedList(_) => "Refreshing automations…",
        Pending::RefreshAutomation => "Refreshing automation…",
        Pending::RefreshHistory { .. } | Pending::RefreshOlderHistory { .. } => {
            "Refreshing run history…"
        }
        Pending::RefreshRun => "Refreshing run…",
    }
}

/// The levels above the current page on one line, then the page itself.
///
/// A level with a press message is a way back to it, drawn unavailable while
/// `busy`; one without cannot be returned to and is plain text. The level at
/// `name` names an automation. It alone gives up width, ending in an
/// ellipsis with its full text in a tooltip, so the views and the current
/// page around it always keep their whole labels.
fn breadcrumb<'a>(
    parents: impl IntoIterator<Item = (&'a str, Option<Action>)>,
    current: &'a str,
    name: Option<usize>,
    busy: bool,
) -> Element<'a, Action> {
    let label = |text: &'a str| {
        crate::design::body(text)
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                cosmic::iced::advanced::text::EllipsizeHeightLimit::Lines(1),
            ))
    };
    let mut trail = widget::row::with_capacity(5)
        .spacing(space::XXS)
        .align_y(Alignment::Center);
    for (index, (text, action)) in parents.into_iter().enumerate() {
        let named = name == Some(index);
        let crumb: Element<'a, Action> = match action {
            Some(action) => {
                let back = format!("Back to {text}");
                crate::interaction::tooltip(
                    crate::interaction::hover(
                        widget::button::custom(label(text))
                            .name(back.clone())
                            .class(crumb_style())
                            .padding([space::XXS, space::XS])
                            .on_press_maybe((!busy).then_some(action)),
                    ),
                    back,
                )
            }
            None => {
                let crumb = widget::container(
                    label(text).class(cosmic::theme::Text::Custom(crate::design::muted_text)),
                )
                .padding([space::XXS, space::XS]);
                if named {
                    crate::interaction::tooltip(crumb, text)
                } else {
                    crumb.into()
                }
            }
        };
        trail = trail
            .push(if named {
                crate::ui_controls::shrinkable(crumb)
            } else {
                crumb
            })
            .push(crate::ui_controls::passive_icon(
                crate::ui_controls::Icon::ChevronRight.handle(),
                crate::design::COMPACT_ICON_SIZE,
            ));
    }
    trail.push(label(current)).into()
}

/// A trail level that leads back: quiet until hovered, as a peer view tab
/// is, and visibly unavailable while a command is in flight.
fn crumb_style() -> cosmic::theme::Button {
    use cosmic::iced::Background;
    let appearance = |focused: bool, theme: &cosmic::Theme| {
        let component = &theme.current_container().component;
        widget::button::Style {
            background: focused.then(|| Background::Color(component.selected_state_color().into())),
            text_color: Some(crate::design::muted_color(theme)),
            ..crate::design::button_style()
        }
    };
    cosmic::theme::Button::Custom {
        active: Box::new(appearance),
        disabled: Box::new(move |theme| widget::button::Style {
            text_color: Some(crate::design::disabled_color(theme)),
            ..appearance(false, theme)
        }),
        hovered: Box::new(move |focused, theme| widget::button::Style {
            background: Some(Background::Color(
                theme.current_container().component.hover.into(),
            )),
            text_color: Some(theme.current_container().component.on.into()),
            ..appearance(focused, theme)
        }),
        pressed: Box::new(move |focused, theme| widget::button::Style {
            background: Some(Background::Color(
                theme.current_container().component.pressed.into(),
            )),
            text_color: Some(theme.current_container().component.on.into()),
            ..appearance(focused, theme)
        }),
    }
}

fn button(label: &'static str, action: Action, enabled: bool) -> Element<'static, Action> {
    let mut button = crate::ui_controls::button::secondary(label);
    if enabled {
        button = button.on_press(action);
    }
    button.into()
}
fn button_owned(label: String, action: Action, enabled: bool) -> Element<'static, Action> {
    // List and history entries can span several lines. Standard buttons reserve
    // a single-line height, hiding target and scheduling details.
    let mut button = widget::button::custom(crate::design::body(label.clone()))
        .name(label)
        .padding(space::M)
        .class(crate::sidebar::sidebar_button_style(false))
        .height(Length::Shrink);
    if enabled {
        button = button.on_press(action);
    }
    button.width(Length::Fill).into()
}
fn refresh_id() -> widget::Id {
    widget::Id::new("automation.refresh")
}
pub fn draft_dialog_focus_id() -> widget::Id {
    widget::Id::new("dialog.automation-draft.keep")
}
pub fn draft_editor_focus_id() -> widget::Id {
    widget::Id::new("automation-draft.name")
}
fn input<'a>(label: &'static str, value: &'a str, field: Field) -> Element<'a, Action> {
    let input = crate::ui_controls::field::text(label, value)
        .on_input(move |value| Action::Field(field, value));
    let input = if matches!(field, Field::Name) {
        input.id(draft_editor_focus_id())
    } else {
        input
    };
    widget::column::with_children(vec![crate::design::caption(label).into(), input.into()])
        .spacing(space::XS)
        .into()
}
fn timestamp(milliseconds: u64) -> String {
    crate::chat::local_time((milliseconds / 1000).min(i64::MAX as u64) as i64)
        .map(|time| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                time.year + 1900,
                time.month + 1,
                time.day,
                time.hour,
                time.minute
            )
        })
        .unwrap_or_else(|| "Unavailable time".into())
}

fn provider(index: usize) -> Provider {
    [Provider::Claude, Provider::Codex, Provider::Omp]
        .get(index)
        .copied()
        .unwrap_or(Provider::Claude)
}
fn provider_index(provider: Provider) -> usize {
    match provider {
        Provider::Claude => 0,
        Provider::Codex => 1,
        Provider::Omp => 2,
    }
}
/// Every timezone the schedule can name, read once from the system database.
fn timezone_names() -> &'static [String] {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = crate::automations::schedule::timezones();
        names.sort_unstable();
        names
    })
}

/// At most `limit` timezones for `query`, which matches any part of a name
/// with spaces for underscores: a city (`vancouver`, `new york`) before a
/// region or a fragment elsewhere in the name.
fn timezone_matches(query: &str, limit: usize) -> Vec<&'static str> {
    let query = query.trim().to_lowercase().replace(' ', "_");
    if query.is_empty() {
        return Vec::new();
    }
    let mut matches = timezone_names()
        .iter()
        .filter_map(|name| {
            let lower = name.to_lowercase();
            let city = lower.rsplit('/').next().unwrap_or(&lower);
            let rank = if city.starts_with(&query) {
                0
            } else if lower.starts_with(&query) {
                1
            } else if lower.contains(&query) {
                2
            } else {
                return None;
            };
            Some((rank, name.as_str()))
        })
        .collect::<Vec<_>>();
    matches.sort_unstable();
    matches
        .into_iter()
        .take(limit)
        .map(|(_, name)| name)
        .collect()
}

fn optional(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn automation() -> Automation {
        Automation {
            id: Uuid::new_v4(),
            revision: 4,
            execution_revision: 3,
            definition: Definition {
                name: "Daily review".into(),
                prompt: "Review changes".into(),
                agent: Spec {
                    provider: Provider::Claude,
                    account: Some(crate::account::Account::system(Provider::Claude).id()),
                    target: Target {
                        repository: "/fixture".into(),
                        directory: "/fixture".into(),
                        placement: None,
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                },
                schedule: Schedule {
                    pattern: SchedulePattern::Daily { hour: 9, minute: 0 },
                    timezone: "UTC".into(),
                },
                session_mode: SessionMode::Fresh,
                precheck: None,
            },
            target_identity: crate::automations::DirectoryIdentity {
                device: 1,
                inode: 2,
            },
            enabled: false,
            created_ms: 1,
            updated_ms: 2,
            next: None,
            active_firing: None,
            reuse_agent: None,
            channel: None,
            blocked: None,
        }
    }

    #[test]
    fn automation_draft_dirty_baseline_tracks_values_and_restored_edits() {
        for existing in [None, Some(automation())] {
            let mut draft = Draft::new(&[], &[], existing.as_ref());
            assert!(!draft.dirty());
            draft.advanced = !draft.advanced;
            draft.custom_model = !draft.custom_model;
            draft
                .prompt
                .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
            assert!(
                !draft.dirty(),
                "disclosure, chooser and caret do not change saved values"
            );
            let original = draft.name.clone();
            draft.name.push_str(" revised");
            assert!(draft.dirty());
            draft.name = original;
            assert!(!draft.dirty(), "editing back to the baseline is clean");
            draft.timeout = "invalid timeout".into();
            assert!(draft.dirty(), "invalid input is still unsaved work");
        }
    }
    #[test]
    fn automation_draft_navigation_requires_generation_fenced_discard() {
        let mut state = State::default();
        state.update(Action::New);
        state.update(Action::Field(Field::Name, "Keep this draft".into()));
        let selected = Uuid::new_v4();
        assert!(state.update(Action::Select(selected)).is_none());
        assert!(state.draft_dialog_open());
        let old = state.discard_generation;
        state.update(Action::Field(Field::Name, "ignored under modal".into()));
        assert_eq!(state.draft.as_ref().unwrap().name, "Keep this draft");
        state.update(Action::KeepDraft(old));
        assert!(!state.draft_dialog_open());
        assert!(state.draft_dirty());
        state.update(Action::CancelEdit);
        let current = state.discard_generation;
        state.update(Action::DiscardDraft(old));
        assert!(state.draft_dialog_open());
        assert!(state.has_draft());
        state.update(Action::DiscardDraft(current));
        assert!(!state.has_draft());
        state.update(Action::New);
        assert!(
            state.update(Action::Select(selected)).is_some(),
            "untouched New may be replaced"
        );
        assert!(!state.has_draft());
    }
    #[test]
    fn automation_draft_edit_and_new_replacement_preserve_work_until_confirmed() {
        let mut state = State {
            selected: Some(automation()),
            ..Default::default()
        };
        state.update(Action::Edit);
        assert!(!state.draft_dirty());
        state.update(Action::Field(Field::Name, "Edited definition".into()));
        for next in [Action::New, Action::Edit] {
            state.update(next);
            assert_eq!(state.draft.as_ref().unwrap().name, "Edited definition");
            assert!(state.draft_dialog().is_some());
            state.update(state.keep_draft_action().unwrap());
        }
        state.update(Action::New);
        let generation = state.discard_generation;
        state.update(Action::DiscardDraft(generation));
        assert_eq!(state.draft.as_ref().unwrap().name, "");
        assert!(!state.draft_dirty());
    }
    #[test]
    fn automation_draft_close_cannot_discard_an_inflight_save() {
        let mut state = State::default();
        state.update(Action::New);
        state.update(Action::Field(Field::Name, "Unsaved".into()));
        state.pending = Some((7, Pending::Save));
        assert!(state.draft_close_blocker().unwrap().contains("save"));
        assert!(!state.can_discard_draft());
        assert!(!state.discard_draft_before_close());
        state.update(Action::Received {
            sequence: 7,
            result: std::sync::Arc::new(Err("Save failed".into())),
        });
        assert!(state.draft_dirty());
        assert!(state.can_discard_draft());
        assert!(state.discard_draft_before_close());
        assert!(state.draft_close_blocker().is_none());
    }

    #[test]
    fn run_activation_supersedes_startup_reads_and_ignores_their_late_replies() {
        let mut state = State::default();
        let Some(Effect::Request { sequence: old, .. }) = state.update(Action::Refresh) else {
            panic!("startup list request expected");
        };
        assert!(state.can_open_run());
        let run = crate::model::RunId::new();
        let Some(Effect::Request { sequence, request }) = state.update(Action::InspectRun(run))
        else {
            panic!("exact notification run request expected");
        };
        assert!(matches!(request, Request::ShowRun { run: requested } if requested == run));
        assert_ne!(sequence, old);
        state.update(Action::Received {
            sequence: old,
            result: std::sync::Arc::new(Ok(Response::Listed(Vec::new()))),
        });
        assert!(matches!(state.pending, Some((current, Pending::Run)) if current == sequence));
    }

    #[test]
    fn run_activation_does_not_discard_edits_consent_or_mutations() {
        let run = crate::model::RunId::new();
        for pending in [Pending::Save, Pending::Mutate, Pending::OpenRun] {
            let mut state = State {
                pending: Some((1, pending)),
                ..State::default()
            };
            assert!(!state.can_open_run());
            assert!(state.update(Action::InspectRun(run)).is_none());
            assert!(state.pending.is_some());
        }
        let mut state = State {
            selected: Some(automation()),
            ..State::default()
        };
        state.update(Action::Enable);
        assert!(!state.can_open_run());
        assert!(state.update(Action::InspectRun(run)).is_none());
        assert!(state.confirmation.is_some());
        state.update(Action::Edit);
        assert!(!state.can_open_run());
        assert!(state.update(Action::InspectRun(run)).is_none());
        assert!(state.draft.is_some());
    }

    #[test]
    fn refreshing_choices_preserves_the_open_drafts_launch_selection() {
        let existing = automation();
        let account = AccountChoice {
            label: "Original account".into(),
            id: existing.definition.agent.account.clone().unwrap(),
            provider: Provider::Claude,
        };
        let target = TargetChoice {
            label: "Original target".into(),
            target: existing.definition.agent.target.clone(),
            defaults: BTreeMap::new(),
        };
        let mut state = State {
            selected: Some(existing.clone()),
            ..State::default()
        };
        state.set_choices(vec![target], vec![account], Models::new());
        state.update(Action::Edit);
        // A refresh may remove or reorder the live catalog while the editor
        // is open; its numeric selections must retain their original meaning.
        state.set_choices(Vec::new(), Vec::new(), Models::new());
        let draft = state.draft.as_ref().unwrap();
        assert_eq!(
            draft.definition(&Models::new()).unwrap(),
            existing.definition
        );
        assert_eq!(draft.accounts[0].label, "Original account");
        assert_eq!(draft.targets[0].label, "Original target");
        assert!(state.accounts.is_empty());
        assert!(state.targets.is_empty());
    }

    #[test]
    fn editing_invalidates_a_queued_automation_confirmation() {
        let mut state = State {
            selected: Some(automation()),
            ..State::default()
        };
        state.update(Action::Enable);
        let old = state.confirmation.as_ref().unwrap().generation;
        state.update(Action::Edit);
        state.update(Action::Field(Field::Name, "New launch".into()));
        assert!(state.update(Action::Confirm(old)).is_none());
        assert!(state.pending.is_none());
        assert_eq!(state.draft.as_ref().unwrap().name, "New launch");
    }

    fn history_firing(created_ms: u64, state: crate::automations::FiringState) -> Firing {
        Firing {
            id: Uuid::new_v4(),
            automation: Uuid::new_v4(),
            name: "Example".into(),
            revision: 1,
            trigger: crate::automations::Trigger::Manual {
                request: Uuid::new_v4(),
            },
            state,
            release_pending: false,
            cancel_requested: false,
            created_ms,
            started_ms: None,
            finished_ms: None,
            agent: None,
            run: None,
            skip: None,
            diagnostic: None,
        }
    }

    #[test]
    fn latest_outcome_follows_history_without_older_details_overwriting_it() {
        use crate::automations::{FiringState, HistoryPage};
        let selected = automation();
        let mut state = State {
            rows: vec![selected.summary(None)],
            selected: Some(selected.clone()),
            ..Default::default()
        };
        let mut latest = history_firing(100, FiringState::Accepted);
        latest.automation = selected.id;
        for outcome in [
            FiringState::Accepted,
            FiringState::Running,
            FiringState::Succeeded,
        ] {
            latest.state = outcome;
            let Some(Effect::Request { sequence, .. }) = state.update(Action::History) else {
                panic!("history request expected");
            };
            state.update(Action::Received {
                sequence,
                result: std::sync::Arc::new(Ok(Response::History(HistoryPage {
                    firings: vec![latest.clone()],
                    next: None,
                }))),
            });
            assert_eq!(state.rows[0].last_state, Some(outcome));
        }
        let mut older = history_firing(1, FiringState::Failed);
        older.automation = selected.id;
        state.refresh_last_outcome(&older, false);
        assert_eq!(state.rows[0].last_state, Some(FiringState::Succeeded));
        // Even an authoritative newest page can include older firings too.
        state.refresh_last_outcome(&older, true);
        assert_eq!(state.rows[0].last_state, Some(FiringState::Succeeded));
        let Some(Effect::Request { sequence, .. }) = state.update(Action::History) else {
            panic!("history request expected");
        };
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Err("offline".into())),
        });
        assert_eq!(state.rows[0].last_state, Some(FiringState::Succeeded));
    }

    fn history_page(firings: Vec<Firing>, more: bool) -> crate::automations::HistoryPage {
        let next = more
            .then(|| history_cursor(history_key(firings.last().expect("nonempty cursor page"))));
        crate::automations::HistoryPage { firings, next }
    }

    #[test]
    fn newer_runs_update_last_outcome_without_replacing_paginated_history() {
        use crate::automations::{FiringState, HistoryPage};
        let selected = automation();
        let mut loaded = history_firing(1, FiringState::Succeeded);
        loaded.automation = selected.id;
        let mut newer = history_firing(100, FiringState::Running);
        newer.automation = selected.id;
        let mut state = State {
            all_history: true,
            rows: vec![selected.summary(Some(loaded.state))],
            history: vec![loaded.clone()],
            history_status: HistoryStatus::Loaded,
            pending: Some((1, Pending::RefreshHistory { scope: None })),
            ..Default::default()
        };
        state.update(Action::Received {
            sequence: 1,
            result: std::sync::Arc::new(Ok(Response::History(HistoryPage {
                firings: vec![newer.clone()],
                next: None,
            }))),
        });
        assert_eq!(state.history[0].id, loaded.id);
        assert!(state.history_newer);
        assert_eq!(state.rows[0].last_state, Some(FiringState::Running));
        newer.state = FiringState::Failed;
        state.pending = Some((2, Pending::RefreshHistory { scope: None }));
        state.update(Action::Received {
            sequence: 2,
            result: std::sync::Arc::new(Ok(Response::History(HistoryPage {
                firings: vec![newer],
                next: None,
            }))),
        });
        assert_eq!(state.rows[0].last_state, Some(FiringState::Failed));
    }

    #[test]
    fn authoritative_list_outcomes_retire_earlier_detail_fences() {
        use crate::automations::FiringState;
        let automation = automation();
        let mut old = history_firing(100, FiringState::Succeeded);
        old.automation = automation.id;
        let mut state = State {
            rows: vec![automation.summary(Some(old.state))],
            last_firings: BTreeMap::from([(automation.id, (old.created_ms, old.id))]),
            pending: Some((1, Pending::List { append: false })),
            ..Default::default()
        };
        // Another firing finishes after our cached history. The list reports
        // its state but has no firing identity that could authorize old detail.
        state.update(Action::Received {
            sequence: 1,
            result: std::sync::Arc::new(Ok(Response::Listed(vec![
                automation.summary(Some(FiringState::Failed)),
            ]))),
        });
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Run(old.id)) else {
            panic!("old run detail expected");
        };
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Detail(RunDetail {
                definition: automation.definition,
                firing: old,
                agent: None,
                run: None,
            }))),
        });
        assert_eq!(state.rows[0].last_state, Some(FiringState::Failed));
    }

    #[test]
    fn periodic_list_reads_allow_search_and_new_drafts_without_accepting_late_replies() {
        let automation = automation();
        let row = automation.summary(None);
        let mut state = State {
            rows: vec![row.clone()],
            ..Default::default()
        };
        let Some(Effect::Request { sequence: poll, .. }) = state.update(Action::Tick) else {
            panic!("list poll expected");
        };
        assert!(state.pending.unwrap().1.background_read());
        assert!(state.update(Action::Tick).is_none());
        state.update(Action::Search("daily".into()));
        assert_eq!(state.search, "daily");
        state.update(Action::Received {
            sequence: poll,
            result: std::sync::Arc::new(Ok(Response::Listed(Vec::new()))),
        });
        assert_eq!(state.rows.len(), 1, "superseded list is ignored");
        let Some(Effect::Request { sequence: poll, .. }) = state.update(Action::Tick) else {
            panic!("next list poll expected");
        };
        state.update(Action::New);
        assert!(state.draft.is_some());
        state.update(Action::Received {
            sequence: poll,
            result: std::sync::Arc::new(Ok(Response::Listed(vec![row]))),
        });
        assert!(state.pending.is_none());
        assert!(state.draft.is_some());
        // An explicit initial load still retains its foreground guard.
        state.draft = None;
        state.update(Action::Refresh);
        state.update(Action::New);
        assert!(state.draft.is_none());
    }

    #[test]
    fn periodic_run_reads_allow_navigation_and_ignore_superseded_replies() {
        let detail = RunDetail {
            definition: automation().definition,
            firing: history_firing(100, crate::automations::FiringState::Running),
            agent: None,
            run: None,
        };
        let mut state = State {
            run: Some(detail.clone()),
            ..Default::default()
        };
        let Some(Effect::Request { sequence: poll, .. }) = state.update(Action::Tick) else {
            panic!("run poll expected");
        };
        assert!(state.pending.unwrap().1.background_read());
        assert!(state.update(Action::Tick).is_none());
        let Some(Effect::Request {
            sequence: browse,
            request,
        }) = state.update(Action::Browse)
        else {
            panic!("navigation supersedes the run poll");
        };
        assert!(matches!(request, Request::List { .. }));
        state.update(Action::Received {
            sequence: poll,
            result: std::sync::Arc::new(Ok(Response::Detail(detail))),
        });
        assert!(state.run.is_none());
        assert_eq!(state.pending.unwrap().0, browse);
    }

    /// Where the pane's text, its one text field and its scrolling regions
    /// land, in the order the view builds them.
    #[derive(Default)]
    struct Outline {
        texts: Vec<(String, cosmic::iced::Rectangle)>,
        field: Option<cosmic::iced::Rectangle>,
        /// Each scrolling region's viewport and the content it scrolls.
        scrollables: Vec<(cosmic::iced::Rectangle, cosmic::iced::Rectangle)>,
    }

    impl cosmic::iced::advanced::widget::Operation for Outline {
        fn traverse(
            &mut self,
            visit: &mut dyn FnMut(&mut dyn cosmic::iced::advanced::widget::Operation),
        ) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, bounds: cosmic::iced::Rectangle, text: &str) {
            self.texts.push((text.to_owned(), bounds));
        }
        fn text_input(
            &mut self,
            _: Option<&widget::Id>,
            bounds: cosmic::iced::Rectangle,
            _: &mut dyn cosmic::iced::advanced::widget::operation::TextInput,
        ) {
            self.field.get_or_insert(bounds);
        }
        fn scrollable(
            &mut self,
            _: Option<&widget::Id>,
            bounds: cosmic::iced::Rectangle,
            content: cosmic::iced::Rectangle,
            _: cosmic::iced::Vector,
            _: &mut dyn cosmic::iced::advanced::widget::operation::Scrollable,
        ) {
            self.scrollables.push((bounds, content));
        }
    }

    impl Outline {
        fn of(state: &State, width: f32, height: f32) -> Self {
            use cosmic::iced::Size;
            use cosmic::iced::advanced::{Layout, layout, widget::Tree};
            let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let mut element = state.view();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(width, height)),
            );
            let mut outline = Self::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut outline);
            outline
        }

        /// The first text that reads `label`.
        fn text(&self, label: &str) -> cosmic::iced::Rectangle {
            self.texts
                .iter()
                .find(|(text, _)| text == label)
                .unwrap_or_else(|| panic!("{label:?} is drawn"))
                .1
        }

        /// The last text that reads `label`: below the header, when both
        /// draw it.
        fn last(&self, label: &str) -> cosmic::iced::Rectangle {
            self.texts
                .iter()
                .rfind(|(text, _)| text == label)
                .unwrap_or_else(|| panic!("{label:?} is drawn"))
                .1
        }
    }

    /// Completes a real Save action/reply rather than seeding a notice string.
    fn saved_notice_state() -> State {
        let (mut state, mut saved) = editing(None, None);
        state.update(Action::Field(Field::Name, "Saved automation A".into()));
        let Some(Effect::Request {
            sequence,
            request:
                Request::Edit {
                    id,
                    revision,
                    definition,
                },
        }) = state.update(Action::Save)
        else {
            panic!("the valid draft sends its explicit Save");
        };
        assert_eq!(id, saved.id);
        assert_eq!(revision, saved.revision);
        assert_eq!(definition.name, "Saved automation A");
        // Navigation cannot replace a draft while that Save is in flight.
        for action in [Action::New, Action::Edit] {
            assert!(state.update(action).is_none());
            assert!(matches!(state.pending, Some((pending, Pending::Save)) if pending == sequence));
            assert_eq!(state.draft.as_ref().unwrap().name, "Saved automation A");
        }
        saved.revision += 1;
        saved.definition = definition;
        let id = saved.id;
        let Some(Effect::Request {
            sequence: history_sequence,
            request: Request::History {
                id: Some(observed), ..
            },
        }) = state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Automation(saved))),
        })
        else {
            panic!("the saved detail requests its own history");
        };
        assert_eq!(observed, id);
        state.update(Action::Received {
            sequence: history_sequence,
            result: std::sync::Arc::new(Ok(Response::History(crate::automations::HistoryPage {
                firings: vec![],
                next: None,
            }))),
        });
        assert!(state.pending.is_none());
        assert!(!state.has_draft());
        assert!(
            state
                .notice
                .as_deref()
                .is_some_and(|notice| notice.starts_with("Saved."))
        );
        let outline = Outline::of(&state, 900.0, 700.0);
        assert!(outline.texts.iter().any(
            |(text, _)| text == "Saved. Review the launch settings before enabling or running."
        ));
        state
    }

    fn assert_unsaved_form(state: &State) {
        let outline = Outline::of(state, 900.0, 700.0);
        assert!(
            !outline
                .texts
                .iter()
                .any(|(text, _)| text
                    == "Saved. Review the launch settings before enabling or running."),
            "the previous success must not label this draft saved"
        );
        outline.text("Save automation");
    }

    #[test]
    fn saved_confirmation_retires_when_new_opens_an_unsaved_blank_draft() {
        let mut state = saved_notice_state();
        assert!(
            state.update(Action::New).is_none(),
            "opening a form sends no mutation"
        );
        let draft = state.draft.as_ref().unwrap();
        assert!(draft.base.is_none());
        assert_eq!(draft.name, "");
        assert!(draft.prompt.text().is_empty());
        assert!(!state.draft_dirty());
        assert!(state.notice.is_none());
        assert_unsaved_form(&state);
        state.update(Action::Field(Field::Name, "Unsaved automation B".into()));
        assert!(state.draft_dirty());
        assert_unsaved_form(&state);
        state.update(Action::CancelEdit);
        assert!(
            state.draft_dialog_open(),
            "closing still protects the new unsaved work"
        );
        state.update(state.keep_draft_action().unwrap());
        assert_eq!(state.draft.as_ref().unwrap().name, "Unsaved automation B");
        assert_unsaved_form(&state);
    }

    #[test]
    fn saved_confirmation_retires_only_after_edit_enters_a_draft() {
        let mut state = saved_notice_state();
        // An Edit action without a selection cannot enter an editor and must
        // not retire the completed operation's notice.
        let selected = state.selected.take();
        let notice = state.notice.clone();
        assert!(state.update(Action::Edit).is_none());
        assert!(!state.has_draft());
        assert_eq!(state.notice, notice);
        state.selected = selected;
        assert!(state.update(Action::Edit).is_none());
        assert!(state.draft.as_ref().unwrap().base.is_some());
        assert!(!state.draft_dirty());
        assert!(state.notice.is_none());
        assert_unsaved_form(&state);
        state.update(Action::Field(Field::Name, "Unsaved revision of A".into()));
        assert!(state.draft_dirty());
        assert_unsaved_form(&state);
        // Rejected and canceled discard answers cannot replace the edit or
        // revive a save confirmation from the previous revision.
        state.update(Action::New);
        let generation = state.discard_generation;
        assert!(state.draft_dialog_open());
        state.update(Action::DiscardDraft(generation.wrapping_add(1)));
        assert!(state.draft_dialog_open());
        assert_eq!(state.draft.as_ref().unwrap().name, "Unsaved revision of A");
        state.update(Action::KeepDraft(generation));
        assert!(!state.draft_dialog_open());
        assert!(state.draft_dirty());
        assert_eq!(state.draft.as_ref().unwrap().name, "Unsaved revision of A");
        assert_unsaved_form(&state);
    }

    #[test]
    fn automation_list_reads_views_then_search_then_rows_at_every_width() {
        let state = State {
            rows: vec![automation().summary(None)],
            ..Default::default()
        };
        // Either side of the split between stacked and side-by-side lists.
        for width in [480.0, 900.0] {
            let outline = Outline::of(&state, width, 700.0);
            let tab = outline.text("Automations");
            let search = outline.field.expect("the list offers its search field");
            let row = outline.text("Daily review");
            assert!(
                tab.y + tab.height <= search.y,
                "at {width} px the view switcher precedes the search field: {tab:?} {search:?}"
            );
            assert!(
                search.y + search.height <= row.y,
                "at {width} px the search field sits above the rows it filters"
            );
            let views = outline.text("Run history");
            assert!(
                views.y + views.height <= search.y,
                "at {width} px the view switcher belongs to the header, above the list"
            );
            if width < 700.0 {
                // The stacked list takes the height of its rows, not a fixed
                // band that leaves empty space between search and header.
                let (list, rows) = outline.scrollables[0];
                assert!(
                    list.height <= rows.height + 0.5,
                    "the stacked list is no taller than its rows: {list:?} {rows:?}"
                );
            }
        }
        // With nothing to filter, the pane offers no field and says so.
        let empty = Outline::of(
            &State {
                list_status: ListStatus::Loaded,
                ..Default::default()
            },
            480.0,
            700.0,
        );
        assert!(empty.field.is_none());
        assert!(empty.text("Automations").y < empty.text("No automations yet").y);
    }

    #[test]
    fn a_short_stacked_pane_keeps_the_selected_automation_scrollable() {
        let owner = automation();
        let mut rows = vec![owner.summary(None)];
        for index in 0..4 {
            let mut other = automation();
            other.definition.name = format!("Other automation {index}");
            rows.push(other.summary(None));
        }
        let state = State {
            rows,
            selected: Some(owner),
            list_status: ListStatus::Loaded,
            history_status: HistoryStatus::Loaded,
            ..Default::default()
        };
        for width in [300.0, 480.0] {
            for height in [160.0, 240.0, 360.0] {
                let outline = Outline::of(&state, width, height);
                assert_eq!(
                    outline.scrollables.len(),
                    2,
                    "list and selection scroll independently"
                );
                let list = outline.scrollables[0].0;
                let details = outline.scrollables[1].0;
                assert!(
                    list.height <= height * STACKED_LIST_SHARE + 0.5,
                    "at {width}x{height} the list must leave room below its header: {list:?}"
                );
                assert!(
                    details.height > 0.0,
                    "at {width}x{height} the selected automation must have a usable viewport: {details:?}"
                );
                assert!(
                    details.y + details.height <= height + 0.5,
                    "the selected viewport stays inside the pane: {details:?}"
                );
            }
        }
    }

    #[test]
    fn automation_switcher_selects_the_view_on_screen_and_a_run_keeps_its_origin() {
        use crate::automations::FiringState;
        fn selected(state: &State, busy: bool) -> Section {
            let tabs = state.view_tabs(busy);
            let chosen = tabs.iter().filter(|tab| tab.selected).collect::<Vec<_>>();
            assert_eq!(chosen.len(), 1, "exactly one view is current");
            assert_eq!(
                chosen[0].on_press.is_some(),
                !busy && state.section_root().is_some(),
                "the current view publishes only a return from what is open beneath it"
            );
            for tab in tabs.iter().filter(|tab| !tab.selected) {
                assert_eq!(tab.on_press.is_some(), !busy, "{} is a way out", tab.label);
            }
            *Section::ALL
                .iter()
                .find(|section| section.label() == chosen[0].label)
                .unwrap()
        }
        let mut state = State::default();
        assert_eq!(selected(&state, false), Section::Automations);
        assert_eq!(selected(&state, true), Section::Automations);
        assert!(matches!(
            state.update(Section::RunHistory.enter()),
            Some(Effect::Request {
                request: Request::History { id: None, .. },
                ..
            })
        ));
        state.pending = None;
        assert_eq!(selected(&state, false), Section::RunHistory);

        // A run opened from history stays inside it; its trail leads back to
        // the loaded page rather than reading it again.
        let firing = history_firing(100, FiringState::Succeeded);
        state.history = vec![firing.clone()];
        state.history_status = HistoryStatus::Loaded;
        let detail = RunDetail {
            definition: automation().definition,
            firing: firing.clone(),
            agent: None,
            run: None,
        };
        state.run = Some(detail.clone());
        assert_eq!(selected(&state, false), Section::RunHistory);
        let [(root, back), (name, automation_link)] = state.run_trail(&detail);
        assert_eq!((root, name), ("Run history", "Daily review"));
        assert!(
            automation_link.is_none(),
            "the trail stays inside Run history"
        );
        assert!(matches!(back, Some(Action::CloseRun)));
        assert!(
            matches!(state.view_tabs(false)[1].on_press, Some(Action::CloseRun)),
            "the current tab also returns to the page"
        );
        assert!(
            state.update(Action::CloseRun).is_none(),
            "closing a run reads nothing"
        );
        assert!(state.run.is_none());
        assert_eq!(selected(&state, false), Section::RunHistory);
        assert_eq!(state.history.len(), 1, "the history page is kept");

        // From an automation's own history, its name leads back to it and
        // the root leaves for the list.
        let mut owner = automation();
        owner.id = firing.automation;
        state.all_history = false;
        state.selected = Some(owner);
        state.run = Some(detail.clone());
        assert_eq!(selected(&state, false), Section::Automations);
        let [(root, list), (_, back)] = state.run_trail(&detail);
        assert_eq!(root, "Automations");
        assert!(matches!(list, Some(Action::Browse)));
        assert!(matches!(back, Some(Action::CloseRun)));
        state.update(Action::CloseRun);
        assert!(state.run.is_none() && state.selected.is_some());
        assert!(matches!(
            state.update(Section::RunHistory.enter()),
            Some(Effect::Request { .. })
        ));
        assert!(state.selected.is_none() && state.all_history);
    }

    #[test]
    fn refreshing_with_an_automation_open_reads_the_list_it_and_its_newest_runs() {
        use crate::automations::FiringState;
        let owner = automation();
        let mut state = State {
            rows: vec![owner.summary(None)],
            selected: Some(owner.clone()),
            history: vec![history_firing(100, FiringState::Succeeded)],
            history_status: HistoryStatus::Loaded,
            history_newer: true,
            ..Default::default()
        };
        let Some(Effect::Request {
            sequence,
            request: Request::List { .. },
        }) = state.update(Action::Refresh)
        else {
            panic!("the list is read first");
        };
        let Some(Effect::Request {
            sequence,
            request: Request::Show { id },
        }) = state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Listed(vec![owner.summary(None)]))),
        })
        else {
            panic!("then the open automation");
        };
        assert_eq!(id, owner.id);
        assert!(matches!(
            state.update(Action::Received {
                sequence,
                result: std::sync::Arc::new(Ok(Response::Automation(owner))),
            }),
            Some(Effect::Request {
                request: Request::History { before: None, .. },
                ..
            })
        ));
        assert!(
            matches!(state.pending, Some((_, Pending::History { append: false }))),
            "the newest page replaces the kept one"
        );
    }

    #[test]
    fn a_failed_refresh_keeps_the_loaded_history_for_the_poll_to_refresh() {
        use crate::automations::FiringState;
        let owner = automation();
        let kept = history_firing(100, FiringState::Succeeded);
        let mut state = State {
            rows: vec![owner.summary(None)],
            selected: Some(owner.clone()),
            history: vec![kept.clone()],
            history_status: HistoryStatus::Loaded,
            ..Default::default()
        };
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Refresh) else {
            panic!("the list is read first");
        };
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Listed(vec![owner.summary(None)]))),
        }) else {
            panic!("then the open automation");
        };
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Err("offline".into())),
        });
        assert_eq!(state.history_status, HistoryStatus::Loaded);
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Tick) else {
            panic!("the poll resumes");
        };
        assert!(matches!(
            state.pending,
            Some((_, Pending::RefreshAutomation))
        ));
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Automation(owner))),
        });
        assert!(
            matches!(state.pending, Some((_, Pending::RefreshHistory { .. }))),
            "the poll refreshes the loaded page in the background"
        );
        assert_eq!(state.history.len(), 1, "the loaded page stays on screen");
    }

    /// The buttons the pane announces, by accessible name, and whether each
    /// can be pressed.
    fn buttons(state: &State, width: f32) -> Vec<(String, bool)> {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut element = state.view();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, 700.0)),
        );
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter(|node| node.node().role() == iced_accessibility::accesskit::Role::Button)
            .map(|node| {
                (
                    node.node().label().unwrap_or_default().to_owned(),
                    !node.node().is_disabled(),
                )
            })
            .collect()
    }

    fn run_of(owner: &Automation) -> RunDetail {
        let mut firing = history_firing(100, crate::automations::FiringState::Succeeded);
        firing.automation = owner.id;
        RunDetail {
            definition: owner.definition.clone(),
            firing,
            agent: None,
            run: None,
        }
    }

    #[test]
    fn a_run_trail_never_leaves_run_history_or_links_an_automation_it_cannot_show() {
        let owner = automation();
        let detail = run_of(&owner);
        let links_name = |state: &State| {
            buttons(state, 900.0)
                .into_iter()
                .any(|(name, enabled)| enabled && name.contains("Daily review"))
        };
        // Opened from Run history: the automation may have been removed, and
        // the loaded page must survive, so the name is not a way out.
        let mut state = State {
            all_history: true,
            history: vec![detail.firing.clone()],
            history_status: HistoryStatus::Loaded,
            run: Some(detail.clone()),
            ..Default::default()
        };
        assert!(!links_name(&state), "{:?}", buttons(&state, 900.0));
        assert!(
            buttons(&state, 900.0).contains(&("Back to Run history".to_owned(), true)),
            "the trail leads back to the loaded page: {:?}",
            buttons(&state, 900.0)
        );
        // Opened from an alert with no list behind it: unknown, so unlinked.
        state.all_history = false;
        state.history.clear();
        assert!(!links_name(&state), "{:?}", buttons(&state, 900.0));
        // A listed automation can be shown.
        state.rows = vec![owner.summary(None)];
        assert!(links_name(&state), "{:?}", buttons(&state, 900.0));
    }

    #[test]
    fn the_current_tab_returns_to_its_view_from_anything_open_beneath_it() {
        let owner = automation();
        let tab = |state: &State, name: &str| {
            buttons(state, 900.0)
                .into_iter()
                .find(|(announced, _)| announced == name)
                .unwrap_or_else(|| panic!("{name:?} in {:?}", buttons(state, 900.0)))
                .1
        };
        let mut state = State {
            rows: vec![owner.summary(None)],
            ..Default::default()
        };
        assert!(
            !tab(&state, "Automations and their schedules"),
            "the list on screen is not chosen again"
        );
        // Where it leads back to is what the tab is named for.
        state.selected = Some(owner.clone());
        assert!(
            tab(&state, "Back to the automation list"),
            "a selection is left through its tab"
        );
        state.run = Some(run_of(&owner));
        assert!(
            tab(&state, "Back to the automation list"),
            "a run is left through its tab"
        );
        state.all_history = true;
        assert!(tab(&state, "Back to Run history"));
    }

    #[test]
    fn an_empty_list_is_not_claimed_while_reading_or_after_a_failed_read() {
        let texts = |state: &State| {
            Outline::of(state, 900.0, 700.0)
                .texts
                .into_iter()
                .map(|(text, _)| text)
                .collect::<Vec<_>>()
        };
        let says = |state: &State, text: &str| texts(state).iter().any(|drawn| drawn == text);
        let fail = |state: &mut State, pending: Pending, error: &str| {
            state.pending = Some((7, pending));
            state.update(Action::Received {
                sequence: 7,
                result: std::sync::Arc::new(Err(error.into())),
            });
        };
        let loaded = State {
            list_status: ListStatus::Loaded,
            ..Default::default()
        };
        assert!(says(&loaded, "No automations yet"));
        // An alert opening its run before any list has been read.
        let mut state = State {
            pending: Some((1, Pending::Run)),
            ..Default::default()
        };
        assert!(
            !says(&state, "No automations yet"),
            "a read in flight is not an empty list"
        );

        // The run's failure is its own: the list was never read, not empty
        // and not failed.
        fail(&mut state, Pending::Run, "Run unavailable");
        assert!(says(&state, "Run unavailable"), "{:?}", texts(&state));
        assert!(!says(&state, "No automations yet"));
        assert!(!says(&state, "Couldn't load automations"));
        assert!(
            buttons(&state, 900.0).contains(&("Load automations".to_owned(), true)),
            "{:?}",
            buttons(&state, 900.0)
        );

        // The list's own failure is stated once, in place, with a retry.
        fail(
            &mut state,
            Pending::List { append: false },
            "Daemon unavailable",
        );
        assert!(says(&state, "Couldn't load automations"));
        assert!(!says(&state, "No automations yet"));
        assert_eq!(
            texts(&state)
                .iter()
                .filter(|text| *text == "Daemon unavailable")
                .count(),
            1,
            "{:?}",
            texts(&state)
        );
        assert!(
            buttons(&state, 900.0).contains(&("Retry".to_owned(), true)),
            "{:?}",
            buttons(&state, 900.0)
        );

        // A later failure elsewhere is not the list's.
        let mut state = loaded;
        fail(&mut state, Pending::Run, "Run unavailable");
        assert!(says(&state, "Run unavailable"));
        assert!(!says(&state, "Couldn't load automations"));
    }

    #[test]
    fn a_long_name_gives_up_width_before_the_trail_around_it() {
        let mut owner = automation();
        owner.definition.name =
            "Summarize every pull request opened against the release branch overnight".to_owned();
        let state = State {
            all_history: true,
            run: Some(run_of(&owner)),
            ..Default::default()
        };
        let wide = Outline::of(&state, 2000.0, 700.0);
        let name = owner.definition.name.as_str();
        for width in [300.0, 360.0, 480.0] {
            let outline = Outline::of(&state, width, 700.0);
            for level in ["Run history", "Run"] {
                assert!(
                    outline.last(level).width >= wide.last(level).width - 0.5,
                    "at {width} px {level:?} keeps its whole label: {:?} of {:?}",
                    outline.last(level),
                    wide.last(level)
                );
            }
            let run = outline.last("Run");
            assert!(
                run.x + run.width <= width,
                "at {width} px the trail ends inside the pane: {run:?}"
            );
            assert!(
                outline.text(name).width < wide.text(name).width,
                "at {width} px the name is shortened instead"
            );
        }
    }

    #[test]
    fn a_draft_trail_returns_to_the_view_it_was_started_from() {
        let mut state = State::default();
        state.update(Action::AllHistory);
        state.pending = None;
        state.update(Action::New);
        let trail = buttons(&state, 900.0);
        assert!(
            trail.contains(&("Back to Run history".to_owned(), true)),
            "{trail:?}"
        );
        assert!(!trail.iter().any(|(name, _)| name == "Back to Automations"));
        state.update(Action::CancelEdit);
        assert!(state.draft.is_none());
        assert_eq!(state.section(), Section::RunHistory);

        // An edit leads back to the automation it changes.
        let owner = automation();
        let mut state = State {
            rows: vec![owner.summary(None)],
            selected: Some(owner),
            ..Default::default()
        };
        state.update(Action::Edit);
        assert!(
            buttons(&state, 900.0).contains(&("Back to Daily review".to_owned(), true)),
            "{:?}",
            buttons(&state, 900.0)
        );
    }

    #[test]
    fn a_narrow_header_draws_its_tabs_compact_ahead_of_its_commands() {
        let state = State {
            rows: vec![automation().summary(None)],
            list_status: ListStatus::Loaded,
            ..Default::default()
        };
        let wide = Outline::of(&state, 900.0, 700.0).text("Run history");
        for width in [300.0, 360.0] {
            let tab = Outline::of(&state, width, 700.0).text("Run history");
            assert!(
                tab.height < wide.height,
                "at {width} px the tabs use caption text: {tab:?} {wide:?}"
            );
            // Refresh and New automation, each an icon, follow the tabs.
            let commands = 2.0 * (crate::design::PRIMARY_CONTROL_SIZE + space::S);
            assert!(
                tab.x + tab.width <= width - space::L - commands,
                "at {width} px the last tab ends before the header's commands: {tab:?}"
            );
        }
    }

    #[test]
    fn refresh_stays_in_place_while_its_work_is_in_flight() {
        let state = |pending| State {
            rows: vec![automation().summary(None)],
            list_status: ListStatus::Loaded,
            pending,
            ..Default::default()
        };
        let idle = buttons(&state(None), 900.0);
        let busy = buttons(&state(Some((1, Pending::List { append: false }))), 900.0);
        let at = |buttons: &[(String, bool)], name: &str| {
            buttons
                .iter()
                .position(|(label, _)| label == name)
                .unwrap_or_else(|| panic!("{name:?} in {buttons:?}"))
        };
        let refresh = at(&idle, "Refresh automations");
        assert!(idle[refresh].1);
        assert_eq!(
            at(&busy, "Loading automations…"),
            refresh,
            "the same control names the work: {busy:?}"
        );
        assert!(!busy[refresh].1, "and cannot start it again");
    }

    #[test]
    fn refresh_keeps_keyboard_focus_through_a_failed_request_and_retry() {
        use cosmic::iced::advanced::{
            Layout, Shell, clipboard, layout, mouse,
            widget::{Operation, Tree, operation},
        };
        use cosmic::iced::{Event, Rectangle, Size, keyboard};
        #[derive(Default)]
        struct Focused(Option<bool>);
        impl Operation for Focused {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                _: Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                if id == Some(&refresh_id()) {
                    self.0 = Some(state.is_focused());
                }
            }
        }
        fn enter(
            state: &State,
            tree: &mut Tree,
            renderer: &cosmic::Renderer,
            focus_once: bool,
        ) -> Vec<Action> {
            let mut element = state.view();
            tree.diff(element.as_widget_mut());
            let viewport = Rectangle::with_size(Size::new(900.0, 700.0));
            let node = element.as_widget_mut().layout(
                tree,
                renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            if focus_once {
                let mut focus = operation::focusable::focus::<()>(refresh_id());
                element
                    .as_widget_mut()
                    .operate(tree, Layout::new(&node), renderer, &mut focus);
            }
            let mut focused = Focused::default();
            element
                .as_widget_mut()
                .operate(tree, Layout::new(&node), renderer, &mut focused);
            assert_eq!(
                focused.0,
                Some(true),
                "the retained Refresh control keeps keyboard focus"
            );
            let event = Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            });
            let mut actions = Vec::new();
            element.as_widget_mut().update(
                tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut actions),
                &viewport,
            );
            actions
        }
        let owner = automation();
        let mut state = State {
            rows: vec![owner.summary(None)],
            list_status: ListStatus::Loaded,
            ..Default::default()
        };
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut tree = Tree::new(state.view().as_widget());
        let actions = enter(&state, &mut tree, &renderer, true);
        assert!(matches!(actions.as_slice(), [Action::Refresh]));
        let Some(Effect::Request { sequence, .. }) =
            state.update(actions.into_iter().next().unwrap())
        else {
            panic!("the focused control starts its real list read");
        };
        assert!(
            enter(&state, &mut tree, &renderer, false).is_empty(),
            "Enter cannot duplicate an in-flight read"
        );
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Err("offline list fixture".into())),
        });
        assert!(matches!(state.list_status, ListStatus::Failed(_)));
        let actions = enter(&state, &mut tree, &renderer, false);
        assert!(
            matches!(actions.as_slice(), [Action::Refresh]),
            "the same keyboard focus retries after failure"
        );
        assert!(matches!(
            state.update(actions.into_iter().next().unwrap()),
            Some(Effect::Request {
                request: Request::List { after: None, .. },
                ..
            })
        ));
    }

    #[test]
    fn user_navigation_supersedes_background_history_without_accepting_its_late_reply() {
        use crate::automations::FiringState;
        let selected = automation();
        let mut old_run = history_firing(100, FiringState::Running);
        old_run.automation = selected.id;
        let mut state = State {
            selected: Some(selected.clone()),
            history: vec![old_run.clone()],
            history_status: HistoryStatus::Loaded,
            ..Default::default()
        };
        let Some(Effect::Request { sequence: show, .. }) = state.update(Action::Tick) else {
            panic!("background definition poll");
        };
        assert!(state.pending.unwrap().1.background_read());
        assert!(
            state.update(Action::Tick).is_none(),
            "ticks coalesce while a read is pending"
        );
        let Some(Effect::Request {
            sequence: history, ..
        }) = state.update(Action::Received {
            sequence: show,
            result: std::sync::Arc::new(Ok(Response::Automation(selected))),
        })
        else {
            panic!("background history poll");
        };
        assert_eq!(
            state.history_status,
            HistoryStatus::Loaded,
            "cached history remains usable while polling"
        );
        let fresh = (151..201)
            .rev()
            .map(|created| {
                let mut firing = history_firing(created, FiringState::Running);
                firing.automation = old_run.automation;
                firing
            })
            .collect();
        let Some(Effect::Request {
            sequence: detail, ..
        }) = state.update(Action::Received {
            sequence: history,
            result: std::sync::Arc::new(Ok(Response::History(history_page(fresh, true)))),
        })
        else {
            panic!("active row poll");
        };
        let next = Uuid::new_v4();
        let Some(Effect::Request {
            sequence: selection,
            request,
        }) = state.update(Action::Select(next))
        else {
            panic!("user selection interrupts polling");
        };
        assert!(matches!(request, Request::Show { id } if id == next));
        assert!(selection > detail);
        assert!(
            !state.pending.unwrap().1.background_read(),
            "foreground loads retain their guard"
        );
        assert!(
            state
                .update(Action::Received {
                    sequence: detail,
                    result: std::sync::Arc::new(Ok(Response::History(history_page(
                        vec![old_run],
                        false
                    ))))
                })
                .is_none()
        );
        assert!(state.selected.is_none());
        assert!(state.history.is_empty());
        assert_eq!(state.pending.unwrap().0, selection);
    }

    #[test]
    fn editing_supersedes_a_background_poll_but_not_an_in_flight_save() {
        let mut state = State {
            selected: Some(automation()),
            ..Default::default()
        };
        state.update(Action::Tick);
        state.update(Action::Edit);
        assert!(state.draft.is_some());
        assert!(state.pending.is_none());
        let draft = state.draft.as_ref().unwrap().name.clone();
        state.pending = Some((99, Pending::Save));
        assert!(state.update(Action::Select(Uuid::new_v4())).is_none());
        assert_eq!(state.draft.as_ref().unwrap().name, draft);
        assert!(matches!(state.pending, Some((99, Pending::Save))));
    }

    #[test]
    fn background_history_refresh_preserves_older_pages_and_updates_active_rows() {
        use crate::automations::FiringState;
        for terminal_pending in [false, true] {
            let newest = history_firing(100, FiringState::Running);
            let mut older = history_firing(
                1,
                if terminal_pending {
                    FiringState::Failed
                } else {
                    FiringState::Running
                },
            );
            older.release_pending = terminal_pending;
            let cursor = HistoryCursor {
                created_ms: 1,
                id: older.id,
            };
            let mut state = State {
                all_history: true,
                history: vec![newest.clone(), older.clone()],
                history_next: Some(cursor.clone()),
                history_status: HistoryStatus::Loaded,
                ..Default::default()
            };
            let Some(Effect::Request { sequence, .. }) = state.update(Action::Tick) else {
                panic!("history poll expected");
            };
            let mut completed = newest.clone();
            completed.state = FiringState::Succeeded;
            let mut newest_page = vec![completed];
            newest_page
                .extend((1..PAGE).map(|i| history_firing(100 - i as u64, FiringState::Succeeded)));
            let expected_before =
                history_cursor(including_history_key(history_key(&older)).unwrap().unwrap());
            let Some(Effect::Request { sequence, request }) = state.update(Action::Received {
                sequence,
                result: std::sync::Arc::new(Ok(Response::History(history_page(newest_page, true)))),
            }) else {
                panic!("active older firing poll expected");
            };
            assert!(
                matches!(request, Request::History { id: None, before: Some(before), limit: PAGE } if before == expected_before)
            );
            assert_eq!(state.history.len(), 2);
            assert_eq!(state.history_next, Some(cursor));
            let mut completed = older;
            completed.state = if terminal_pending {
                FiringState::Failed
            } else {
                FiringState::Succeeded
            };
            completed.release_pending = false;
            state.update(Action::Received {
                sequence,
                result: std::sync::Arc::new(Ok(Response::History(history_page(
                    vec![completed],
                    false,
                )))),
            });
            assert_eq!(state.history[0].state, FiringState::Succeeded);
            assert_eq!(
                state.history[1].state,
                if terminal_pending {
                    FiringState::Failed
                } else {
                    FiringState::Succeeded
                }
            );
            assert!(
                !state.history[1].release_pending,
                "older terminal rows must lose stale pending badges"
            );
            assert!(
                state.run.is_none(),
                "background polling must not open run detail"
            );
            assert!(state.pending.is_none());
        }
    }

    #[test]
    fn background_new_runs_preserve_snapshot_and_cursor_without_growing_the_list() {
        use crate::automations::{FiringState, HistoryPage};
        let original = (0..PAGE)
            .rev()
            .map(|i| history_firing((i + 100) as u64, FiringState::Succeeded))
            .collect::<Vec<_>>();
        let cursor = HistoryCursor {
            created_ms: 100,
            id: original.last().unwrap().id,
        };
        let mut state = State {
            all_history: true,
            history: original.clone(),
            history_next: Some(cursor.clone()),
            history_status: HistoryStatus::Loaded,
            ..Default::default()
        };
        for poll in 1..=20 {
            let Some(Effect::Request { sequence, .. }) = state.update(Action::Tick) else {
                panic!("history poll expected");
            };
            let firings = (0..PAGE)
                .rev()
                .map(|i| history_firing((1000 + poll * PAGE + i) as u64, FiringState::Succeeded))
                .collect();
            state.update(Action::Received {
                sequence,
                result: std::sync::Arc::new(Ok(Response::History(HistoryPage {
                    firings,
                    next: None,
                }))),
            });
            assert_eq!(
                state.history.len(),
                PAGE,
                "automatic polls never add unrequested pages"
            );
            assert_eq!(state.history_next, Some(cursor.clone()));
            assert!(state.history_newer);
        }
        let Some(Effect::Request { sequence, .. }) = state.update(Action::History) else {
            panic!("explicit refresh expected");
        };
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Err("offline".into())),
        });
        assert_eq!(
            state.history.len(),
            PAGE,
            "failed refresh retains the reader's snapshot"
        );
        assert_eq!(state.history_next, Some(cursor.clone()));
        assert!(
            state.history_newer,
            "failed refresh retains the newer-runs notice"
        );
        let Some(Effect::Request { sequence, request }) = state.update(Action::MoreHistory) else {
            panic!("older snapshot page expected");
        };
        assert!(
            matches!(request, Request::History { before: Some(before), .. } if before == cursor)
        );
        let older = history_firing(0, FiringState::Succeeded);
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::History(HistoryPage {
                firings: vec![older],
                next: None,
            }))),
        });
        assert_eq!(
            state.history.len(),
            PAGE + 1,
            "only explicit pagination adds older rows"
        );
        let Some(Effect::Request { sequence, .. }) = state.update(Action::History) else {
            panic!("explicit newest-page refresh expected");
        };
        let newest = history_firing(5000, FiringState::Succeeded);
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::History(HistoryPage {
                firings: vec![newest.clone()],
                next: None,
            }))),
        });
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].id, newest.id);
        assert!(!state.history_newer);
    }

    #[test]
    fn an_initially_empty_history_can_populate_one_bounded_page_automatically() {
        use crate::automations::{FiringState, HistoryPage};
        let mut state = State {
            all_history: true,
            history_status: HistoryStatus::Loaded,
            ..Default::default()
        };
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Tick) else {
            panic!("empty history poll expected");
        };
        let newest = history_firing(100, FiringState::Succeeded);
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::History(HistoryPage {
                firings: vec![newest.clone()],
                next: None,
            }))),
        });
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].id, newest.id);
        assert!(!state.history_newer);
    }

    #[test]
    fn selecting_an_automation_loads_history_and_distinguishes_failure_from_empty() {
        let mut state = State::default();
        let automation = automation();
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Select(automation.id))
        else {
            panic!("definition request expected");
        };
        assert!(state.selected.is_none());
        let Some(Effect::Request { sequence, request }) = state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Automation(automation.clone()))),
        }) else {
            panic!("history request expected after selection");
        };
        assert!(
            matches!(request, Request::History { id: Some(id), before: None, .. } if id == automation.id)
        );
        assert_eq!(state.history_status, HistoryStatus::Loading);
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Err("offline".into())),
        });
        assert_eq!(
            state.history_status,
            HistoryStatus::Failed("offline".into())
        );
        let Some(Effect::Request { sequence, .. }) = state.update(Action::History) else {
            panic!("history retry expected");
        };
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::History(crate::automations::HistoryPage {
                firings: Vec::new(),
                next: None,
            }))),
        });
        assert_eq!(state.history_status, HistoryStatus::Loaded);
        assert!(state.history.is_empty());
    }

    #[test]
    fn all_automation_history_is_independent_of_current_definition_selection() {
        let mut state = State {
            selected: Some(automation()),
            ..Default::default()
        };
        let Some(Effect::Request {
            request:
                Request::History {
                    id: None,
                    before: None,
                    ..
                },
            ..
        }) = state.update(Action::AllHistory)
        else {
            panic!("global history request expected")
        };
        assert!(state.selected.is_none());
        assert!(state.all_history);
        state.pending = None;
        assert!(matches!(
            state.update(Action::Tick),
            Some(Effect::Request {
                request: Request::History {
                    id: None,
                    before: None,
                    ..
                },
                ..
            })
        ));
        state.pending = None;
        assert!(matches!(
            state.update(Action::Refresh),
            Some(Effect::Request {
                request: Request::History {
                    id: None,
                    before: None,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn confirmation_uses_the_immutable_displayed_launch_revision() {
        let selected = automation();
        let mut state = State {
            selected: Some(selected.clone()),
            ..State::default()
        };
        state.update(Action::Enable);
        let confirmation = state.confirmation.as_ref().unwrap();
        let generation = confirmation.generation;
        assert_eq!(confirmation.automation, selected);
        // An external update cannot change the already displayed consent.
        state.selected.as_mut().unwrap().revision += 1;
        let Some(Effect::Request {
            request:
                Request::SetEnabled {
                    revision,
                    confirmed,
                    ..
                },
            ..
        }) = state.update(Action::Confirm(generation))
        else {
            panic!("expected exact confirmation");
        };
        assert_eq!(revision, selected.revision);
        assert!(confirmed);
    }

    #[test]
    fn uncertain_manual_reply_reuses_its_idempotency_key() {
        let mut state = State {
            selected: Some(automation()),
            ..State::default()
        };
        state.update(Action::RunNow);
        let generation = state.confirmation.as_ref().unwrap().generation;
        let Some(Effect::Request {
            sequence,
            request: original,
        }) = state.update(Action::Confirm(generation))
        else {
            panic!("expected run request");
        };
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Err("Connection lost after send".into())),
        });
        state.update(Action::RunNow);
        let generation = state.confirmation.as_ref().unwrap().generation;
        let Some(Effect::Request { request: retry, .. }) =
            state.update(Action::Confirm(generation))
        else {
            panic!("expected retry");
        };
        assert_eq!(
            retry, original,
            "transport ambiguity cannot mint a second run"
        );
    }

    #[test]
    fn stale_response_cannot_replace_the_current_automation_view() {
        let mut state = State::default();
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Refresh) else {
            panic!("expected query");
        };
        state.update(Action::Received {
            sequence: sequence.wrapping_sub(1),
            result: std::sync::Arc::new(Ok(Response::Automation(automation()))),
        });
        assert!(state.selected.is_none());
        assert!(state.pending.is_some());
    }

    #[test]
    fn every_automation_button_is_exposed_by_name() {
        let names = |state: &State| {
            crate::ui_controls::accessibility_probe::accessible_buttons(
                state.view(),
                cosmic::iced::Size::new(1200.0, 800.0),
            )
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>()
        };
        let exposes = |names: &[String], expected: &[&str]| {
            for name in expected {
                assert!(
                    names.iter().any(|found| found == name),
                    "{name} in {names:?}"
                );
            }
            assert!(
                names.iter().all(|name| !name.is_empty()),
                "unnamed button in {names:?}"
            );
        };
        // Each of these button rows wraps when the pane is narrow.
        let mut state = State {
            selected: Some(automation()),
            ..State::default()
        };
        exposes(
            &names(&state),
            &[
                "New automation",
                "Refresh automations",
                "Edit",
                "Enable…",
                "Run now…",
                "Remove…",
            ],
        );
        state.update(Action::RunNow);
        exposes(&names(&state), &["Cancel", "Confirm"]);
        state.update(Action::CancelConfirmation);
        state.update(Action::New);
        state.update(Action::Field(Field::Timezone, "Vancouver".into()));
        exposes(
            &names(&state),
            &[
                "Cancel",
                "Save automation",
                "Show advanced settings",
                "America/Vancouver",
            ],
        );
        state.update(Action::ToggleAdvanced);
        exposes(&names(&state), &["Hide advanced settings"]);
    }

    fn models() -> Models {
        let model = |id: &str, reasoning: &[&str]| Model {
            id: id.into(),
            label: id.to_uppercase(),
            reasoning: reasoning.iter().map(|&effort| effort.into()).collect(),
        };
        Models::from([(
            Provider::Codex,
            vec![
                model("luna", &["low", "medium", "high"]),
                model("sol", &["medium", "xhigh"]),
                model("plain", &[]),
            ],
        )])
    }

    /// An editor open on a saved Codex definition with every choice present.
    fn editing(model: Option<&str>, effort: Option<&str>) -> (State, Automation) {
        let mut existing = automation();
        let agent = &mut existing.definition.agent;
        agent.provider = Provider::Codex;
        agent.account = Some(crate::account::Account::system(Provider::Codex).id());
        agent.model = model.map(Into::into);
        agent.reasoning_effort = effort.map(Into::into);
        let agent = existing.definition.agent.clone();
        let mut state = State {
            selected: Some(existing.clone()),
            ..State::default()
        };
        state.set_choices(
            vec![TargetChoice {
                label: "Fixture".into(),
                target: agent.target.clone(),
                defaults: BTreeMap::new(),
            }],
            vec![AccountChoice {
                label: "Codex".into(),
                provider: Provider::Codex,
                id: agent.account.clone().unwrap(),
            }],
            models(),
        );
        state.update(Action::Edit);
        (state, existing)
    }

    fn saved_effort(state: &State) -> Result<Option<String>, String> {
        let draft = state.draft.as_ref().unwrap();
        draft
            .definition(&state.models)
            .map(|definition| definition.agent.reasoning_effort)
    }

    #[test]
    fn a_saved_reasoning_effort_reopens_and_saves_unchanged() {
        let (state, existing) = editing(Some("luna"), Some("low"));
        let draft = state.draft.as_ref().unwrap();
        assert_eq!(draft.efforts(&state.models), ["low", "medium", "high"]);
        assert_eq!(
            draft.definition(&state.models).unwrap(),
            existing.definition
        );
    }

    #[test]
    fn efforts_follow_the_models_the_composer_lists() {
        let (mut state, _) = editing(Some("luna"), None);
        state.update(Action::Effort(Some("medium".into())));
        assert_eq!(saved_effort(&state), Ok(Some("medium".into())));
        // An effort carries over to a model that offers it, and only to one.
        state.update(Action::Model(ModelChoice::Listed("sol".into())));
        assert_eq!(saved_effort(&state), Ok(Some("medium".into())));
        state.update(Action::Model(ModelChoice::Listed("plain".into())));
        assert_eq!(saved_effort(&state), Ok(None));
        assert!(
            state
                .draft
                .as_ref()
                .unwrap()
                .efforts(&state.models)
                .is_empty()
        );
        state.update(Action::Model(ModelChoice::Listed("sol".into())));
        state.update(Action::Effort(Some("xhigh".into())));
        state.update(Action::Model(ModelChoice::Default));
        assert_eq!(saved_effort(&state), Ok(None));
        // A typed model keeps the effort chosen for it; the provider decides.
        state.update(Action::Model(ModelChoice::Listed("luna".into())));
        state.update(Action::Effort(Some("high".into())));
        state.update(Action::Model(ModelChoice::Custom));
        state.update(Action::Field(Field::Model, "gpt-future".into()));
        assert_eq!(saved_effort(&state), Ok(Some("high".into())));
        // Model IDs and efforts belong to the provider they were chosen for.
        state.update(Action::Provider(provider_index(Provider::Claude)));
        let draft = state.draft.as_ref().unwrap();
        assert!(draft.model.is_empty() && draft.effort.is_none());
    }

    #[test]
    fn an_effort_the_listed_model_does_not_offer_is_refused_by_name() {
        // A definition written through the CLI is shown as saved, not dropped.
        let (state, _) = editing(Some("sol"), Some("low"));
        let draft = state.draft.as_ref().unwrap();
        assert_eq!(draft.efforts(&state.models), ["medium", "xhigh", "low"]);
        let error = saved_effort(&state).unwrap_err();
        assert!(error.contains("SOL does not offer the low"), "{error}");
        // A model the provider has not listed cannot be checked here.
        let (state, _) = editing(Some("unlisted"), Some("low"));
        assert!(state.draft.as_ref().unwrap().custom(&state.models));
        assert_eq!(saved_effort(&state), Ok(Some("low".into())));
    }

    fn account(label: &str, provider: Provider) -> AccountChoice {
        AccountChoice {
            label: label.into(),
            provider,
            id: format!("{}/{}", provider.executable(), Uuid::new_v4())
                .parse()
                .unwrap(),
        }
    }

    fn selected_account(state: &State) -> Option<&str> {
        let draft = state.draft.as_ref().unwrap();
        draft
            .account
            .map(|index| draft.accounts[index].label.as_str())
    }

    #[test]
    fn a_new_automation_starts_on_the_targets_default_account() {
        let work = account("Work", Provider::Claude);
        let personal = account("Personal", Provider::Claude);
        let codex = account("Codex", Provider::Codex);
        let target = |label: &str, claude: &AccountChoice| TargetChoice {
            label: label.into(),
            target: automation().definition.agent.target,
            defaults: BTreeMap::from([(Provider::Claude, claude.id.clone())]),
        };
        let mut state = State::default();
        state.set_choices(
            vec![
                target("Work project", &work),
                target("Side project", &personal),
            ],
            vec![work.clone(), personal.clone(), codex],
            Models::new(),
        );
        state.update(Action::New);
        assert_eq!(selected_account(&state), Some("Work"));
        // The preselection follows the target until the reader picks one.
        state.update(Action::Target(1));
        assert_eq!(selected_account(&state), Some("Personal"));
        state.update(Action::Account(0));
        state.update(Action::Target(0));
        state.update(Action::Target(1));
        assert_eq!(selected_account(&state), Some("Work"));
        // With no default of its own, a provider's only account is chosen.
        state.update(Action::Provider(provider_index(Provider::Codex)));
        assert_eq!(selected_account(&state), Some("Codex"));
        // A provider with no account leaves the choice to the reader.
        state.update(Action::Provider(provider_index(Provider::Omp)));
        assert_eq!(selected_account(&state), None);
        // Changing provider replaced the picked account with a default.
        state.update(Action::Provider(provider_index(Provider::Claude)));
        assert_eq!(selected_account(&state), Some("Personal"));
    }

    #[test]
    fn a_new_automation_starts_in_the_system_timezone() {
        let mut state = State::default();
        state.update(Action::New);
        let timezone = &state.draft.as_ref().unwrap().timezone;
        assert_eq!(
            Some(timezone),
            crate::automations::schedule::system_timezone()
                .or_else(|| Some("UTC".into()))
                .as_ref()
        );
    }

    #[test]
    fn timezones_are_found_by_city_before_any_other_part() {
        assert_eq!(timezone_matches("vancouver", 6), ["America/Vancouver"]);
        assert_eq!(timezone_matches("New york", 1), ["America/New_York"]);
        let europe = timezone_matches("europe/", 6);
        assert_eq!(europe.len(), 6);
        assert!(europe.iter().all(|name| name.starts_with("Europe/")));
        // A city match ranks before a name that only contains the query.
        let city = |name: &str| name.rsplit('/').next().unwrap().to_lowercase();
        let ho = timezone_matches("ho", 200);
        let first_other = ho.iter().position(|name| !city(name).starts_with("ho"));
        assert!(first_other.is_some_and(|index| index > 0));
        assert!(
            ho[first_other.unwrap()..]
                .iter()
                .all(|name| !city(name).starts_with("ho"))
        );
        assert!(timezone_matches("", 6).is_empty());
        assert!(timezone_matches("Mars", 6).is_empty());
    }

    #[test]
    fn an_unknown_timezone_is_refused_before_the_daemon_sees_it() {
        let (mut state, existing) = editing(None, None);
        state.update(Action::Field(Field::Timezone, "Vancouver".into()));
        let draft = state.draft.as_ref().unwrap();
        let error = draft.definition(&state.models).unwrap_err();
        assert!(error.contains("\"Vancouver\""), "{error}");
        state.update(Action::Field(Field::Timezone, "america/vancouver".into()));
        let saved = state
            .draft
            .as_ref()
            .unwrap()
            .definition(&state.models)
            .unwrap();
        assert_eq!(saved.schedule.timezone, "America/Vancouver");
        assert_eq!(saved.agent, existing.definition.agent);
    }

    #[test]
    fn a_saved_timezone_keeps_its_spelling_so_a_rename_stays_immaterial() {
        let mut existing = automation();
        existing.definition.schedule.timezone = "america/vancouver".into();
        let mut draft = Draft::new(&[], &[], Some(&existing));
        draft.name = "Renamed review".into();
        assert_eq!(draft.schedule().unwrap(), existing.definition.schedule);
        // The same zone typed in another case is still the saved one.
        draft.timezone = "America/Vancouver".into();
        assert_eq!(draft.schedule().unwrap().timezone, "america/vancouver");
        // A different zone takes the database's spelling.
        draft.timezone = "europe/london".into();
        assert_eq!(draft.schedule().unwrap().timezone, "Europe/London");
    }

    #[test]
    fn absent_saved_account_cannot_silently_switch_to_a_default() {
        let selected = automation();
        let claude = account("Only Claude account", Provider::Claude);
        let draft = Draft::new(&[], &[claude], Some(&selected));
        assert!(
            draft
                .definition(&Models::new())
                .unwrap_err()
                .contains("explicit provider account")
        );
    }

    #[test]
    fn archived_accounts_remain_editable_only_for_their_existing_automation() {
        let archived = account("Archived login", Provider::Claude);
        let mut existing = automation();
        existing.definition.agent.provider = Provider::Claude;
        existing.definition.agent.account = Some(archived.id.clone());
        let mut state = State {
            selected: Some(existing),
            ..State::default()
        };
        state.set_archived_accounts(vec![archived.clone()]);
        state.update(Action::Edit);
        assert_eq!(selected_account(&state), Some("Archived login"));
        assert_eq!(
            state
                .draft
                .as_ref()
                .unwrap()
                .definition(&state.models)
                .unwrap()
                .agent
                .account,
            Some(archived.id.clone())
        );
        state.update(Action::New);
        assert!(state.draft.as_ref().unwrap().accounts.is_empty());
    }
    #[test]
    fn automation_lifecycle_pages_are_read_only_bounded_and_deduplicated() {
        use crate::automations::{LifecycleActor, LifecycleCause, LifecycleEvent, LifecyclePage};
        let automation = automation();
        let id = automation.id;
        let mut state = State {
            selected: Some(automation),
            ..Default::default()
        };
        let event = |sequence| LifecycleEvent {
            sequence,
            automation: id,
            revision: sequence,
            occurred_ms: 100,
            cause: LifecycleCause::Disabled,
            actor: LifecycleActor::UserManagement,
            enabled: false,
            blocked: false,
            firing: None,
        };
        let Some(Effect::Request { sequence, request }) = state.update(Action::Lifecycle) else {
            panic!("audit request");
        };
        assert_eq!(
            request,
            Request::Audit {
                id,
                before: None,
                limit: PAGE
            }
        );
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Audit(LifecyclePage {
                events: vec![event(3), event(2)],
                next: Some(2),
                earlier_events_unavailable: false,
            }))),
        });
        let Some(Effect::Request { sequence, request }) = state.update(Action::MoreLifecycle)
        else {
            panic!("older audit request");
        };
        assert_eq!(
            request,
            Request::Audit {
                id,
                before: Some(2),
                limit: PAGE
            }
        );
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Audit(LifecyclePage {
                events: vec![event(2), event(1)],
                next: None,
                earlier_events_unavailable: true,
            }))),
        });
        let page = state.lifecycle.as_ref().unwrap();
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
        assert!(page.earlier_events_unavailable);
        assert!(state.update(Action::MoreLifecycle).is_none());
        assert!(
            state.history.is_empty(),
            "lifecycle does not fabricate execution outcomes"
        );
    }

    #[test]
    fn automation_lifecycle_create_receipt_retires_the_previous_selection_audit() {
        use crate::automations::LifecyclePage;
        let mut first = automation();
        first.id = Uuid::new_v4();
        let mut created = first.clone();
        created.id = Uuid::new_v4();
        let mut state = State {
            selected: Some(first),
            lifecycle: Some(LifecyclePage {
                events: vec![],
                next: None,
                earlier_events_unavailable: true,
            }),
            ..Default::default()
        };
        state.update(Action::New);
        assert!(state.has_draft());
        // Exercise the same saved-create callback as Action::Save. Definition
        // editing/validation has its own tests; attribution belongs to this reducer.
        let Some(Effect::Request { sequence, .. }) = state.request(
            Pending::Save,
            Request::Create {
                definition: created.definition.clone(),
            },
        ) else {
            panic!("create request");
        };
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Automation(created.clone()))),
        });
        assert_eq!(state.selected.as_ref().unwrap().id, created.id);
        assert!(
            state.lifecycle.is_none(),
            "the new definition must never inherit another audit"
        );
    }

    #[test]
    fn automation_lifecycle_old_context_and_wrong_automation_receipts_are_refused() {
        use crate::automations::{LifecycleActor, LifecycleCause, LifecycleEvent, LifecyclePage};
        let selected = automation();
        let id = selected.id;
        let mut state = State {
            selected: Some(selected),
            ..Default::default()
        };
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Lifecycle) else {
            panic!("audit request");
        };
        let bad = LifecyclePage {
            events: vec![LifecycleEvent {
                sequence: 1,
                automation: Uuid::new_v4(),
                revision: 1,
                occurred_ms: 100,
                cause: LifecycleCause::Created,
                actor: LifecycleActor::UserManagement,
                enabled: false,
                blocked: false,
                firing: None,
            }],
            next: None,
            earlier_events_unavailable: false,
        };
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Audit(bad))),
        });
        assert!(state.lifecycle.is_none());
        assert_eq!(state.selected.as_ref().unwrap().id, id);
        let other = Uuid::new_v4();
        assert!(
            matches!(state.update(Action::Select(other)), Some(Effect::Request { request: Request::Show { id }, .. }) if id == other)
        );
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Audit(LifecyclePage {
                events: vec![],
                next: None,
                earlier_events_unavailable: false,
            }))),
        });
        assert!(state.lifecycle.is_none());
    }
    fn list_refresh_rows(count: usize) -> Vec<Summary> {
        (1..=count)
            .map(|index| {
                let mut value = automation();
                value.id = Uuid::from_u128(index as u128 * 2);
                value.definition.name = format!("Job {index:04}");
                value.enabled = true;
                value.summary(Some(crate::automations::FiringState::Running))
            })
            .collect()
    }

    fn receive_list(state: &mut State, sequence: u64, rows: Vec<Summary>) {
        assert!(
            state
                .update(Action::Received {
                    sequence,
                    result: std::sync::Arc::new(Ok(Response::Listed(rows))),
                })
                .is_none()
        );
    }

    #[test]
    fn loaded_automation_pages_refresh_outcomes_without_moving_native_search_or_pagination() {
        use cosmic::iced::advanced::widget::{Operation, operation};
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{Event, Point, Rectangle, Size, keyboard, mouse};
        type Ui<'a> = UserInterface<'a, Action, cosmic::Theme, cosmic::Renderer>;
        fn frame<'a>(state: &'a State, cache: Cache, renderer: &mut cosmic::Renderer) -> Ui<'a> {
            let mut ui =
                UserInterface::build(state.view(), Size::new(900.0, 700.0), cache, renderer);
            ui.draw(
                renderer,
                &cosmic::theme::active(),
                &cosmic::iced::advanced::renderer::Style::default(),
                mouse::Cursor::Unavailable,
            );
            ui
        }
        fn bounds(ui: &Ui<'_>, name: &str) -> Rectangle {
            let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
            let rect = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .find(|node| node.node().label() == Some(name))
                .unwrap_or_else(|| panic!("mounted {name}"))
                .node()
                .bounds()
                .unwrap();
            Rectangle::new(
                Point::new(rect.x0 as f32, rect.y0 as f32),
                Size::new((rect.x1 - rect.x0) as f32, (rect.y1 - rect.y0) as f32),
            )
        }
        #[derive(Default)]
        struct Search(Option<widget::Id>);
        impl Operation for Search {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text_input(
                &mut self,
                id: Option<&widget::Id>,
                _: Rectangle,
                _: &mut dyn operation::TextInput,
            ) {
                assert!(self.0.is_none(), "one mounted list search");
                self.0 = id.cloned();
            }
        }

        let rows = list_refresh_rows(PAGE + 1);
        let ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();
        let mut state = State::default();
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Refresh) else {
            panic!()
        };
        receive_list(&mut state, sequence, rows[..PAGE].to_vec());
        state.update(Action::Search("Job 0001".into()));
        // Filtering leaves the real More control in this viewport, without
        // faking an offscreen click or changing the loaded-row budget.
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut ui = frame(&state, Cache::new(), &mut renderer);
        let point = bounds(&ui, "Load more automations").center();
        assert!(Rectangle::with_size(Size::new(900.0, 700.0)).contains(point));
        let mut messages = Vec::new();
        let _ = ui.update(
            &[
                Event::Mouse(mouse::Event::CursorMoved { position: point }),
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ],
            mouse::Cursor::Available(point),
            &mut renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert!(
            matches!(messages.as_slice(), [Action::More]),
            "{messages:?}"
        );
        let cache = ui.into_cache();
        let Some(Effect::Request { sequence, request }) = state.update(messages.remove(0)) else {
            panic!()
        };
        assert_eq!(
            request,
            Request::List {
                after: Some(rows[PAGE - 1].id),
                limit: PAGE
            }
        );
        receive_list(&mut state, sequence, vec![rows[PAGE].clone()]);
        let manual_after = state.after;
        assert!(!state.more);
        let mut ui = frame(&state, cache, &mut renderer);
        let refresh_bounds = bounds(&ui, "Refresh automations");
        let search_bounds = bounds(&ui, "Filter automations");
        let mut search = Search::default();
        ui.operate(&renderer, &mut search);
        let search_id = search.0.unwrap();
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(search_id.clone()),
        );
        ui.operate(
            &renderer,
            &mut operation::text_input::select_all::<()>(search_id),
        );
        let cache = ui.into_cache();

        let old_firing = Uuid::from_u128(9000);
        state.last_firings.insert(rows[0].id, (1, old_firing));
        let Some(Effect::Request { sequence, request }) = state.update(Action::Tick) else {
            panic!("a loaded second page must not stop status refresh");
        };
        assert_eq!(
            request,
            Request::List {
                after: None,
                limit: PAGE
            }
        );
        let mut first = rows[..PAGE].to_vec();
        first[0].enabled = false;
        first[0].blocked = Some("Fixture cleanup needs inspection".into());
        first[0].last_state = Some(crate::automations::FiringState::Failed);
        receive_list(&mut state, sequence, first.clone());
        assert!(!state.rows[0].enabled);
        assert_eq!(
            state.rows[0].last_state,
            Some(crate::automations::FiringState::Failed)
        );
        assert!(!state.last_firings.contains_key(&rows[0].id));
        let mut old_detail = history_firing(1, crate::automations::FiringState::Running);
        old_detail.id = old_firing;
        old_detail.automation = rows[0].id;
        state.refresh_last_outcome(&old_detail, false);
        assert_eq!(
            state.rows[0].last_state,
            Some(crate::automations::FiringState::Failed)
        );
        let outline = Outline::of(&state, 900.0, 700.0);
        outline.text("Last: Failed");
        outline.text("Fixture cleanup needs inspection");
        let ui = frame(&state, cache, &mut renderer);
        assert_eq!(bounds(&ui, "Filter automations"), search_bounds);
        let cache = ui.into_cache();

        let Some(Effect::Request { sequence, request }) = state.update(Action::Tick) else {
            panic!()
        };
        assert_eq!(
            request,
            Request::List {
                after: Some(rows[PAGE - 1].id),
                limit: PAGE
            }
        );
        let mut last = rows[PAGE].clone();
        last.last_state = Some(crate::automations::FiringState::Succeeded);
        // A genuinely observed new tail row requests Refresh but cannot
        // extend this pass or grow the reader's loaded snapshot.
        let beyond = list_refresh_rows(PAGE + 2).pop().unwrap();
        receive_list(&mut state, sequence, vec![last, beyond]);
        assert!(state.loaded_list_refresh.is_none());
        assert_eq!(
            state.rows[PAGE].last_state,
            Some(crate::automations::FiringState::Succeeded)
        );
        assert!(state.list_changed);
        let Some(Effect::Request { sequence, request }) = state.update(Action::Tick) else {
            panic!()
        };
        assert_eq!(
            request,
            Request::List {
                after: None,
                limit: PAGE
            },
            "completed passes wrap"
        );
        let mut unknown = first[1].clone();
        unknown.id = Uuid::from_u128(3);
        first[1] = unknown;
        receive_list(&mut state, sequence, first);
        assert_eq!(state.rows.iter().map(|row| row.id).collect::<Vec<_>>(), ids);
        assert_eq!(state.after, manual_after);
        assert!(!state.more);
        assert!(state.selected.is_none() && state.draft.is_none() && state.run.is_none());
        let mut ui = frame(&state, cache, &mut renderer);
        assert_eq!(
            bounds(
                &ui,
                "Automation list changed; refresh to show additions or removals"
            ),
            refresh_bounds
        );
        assert_eq!(bounds(&ui, "Filter automations"), search_bounds);
        let mut messages = Vec::new();
        let _ = ui.update(
            &[Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character("z".into()),
                modified_key: keyboard::Key::Character("z".into()),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyZ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: Some("z".into()),
                repeat: false,
            })],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert!(
            matches!(messages.as_slice(), [Action::Search(value)] if value == "z"),
            "search focus and selection survived every cached reply: {messages:?}"
        );
        let _ = ui.into_cache();
        state.update(messages.remove(0));
        state.update(Action::Search("Job 0051".into()));
        Outline::of(&state, 900.0, 700.0).text("Last: Succeeded");
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Refresh) else {
            panic!()
        };
        receive_list(&mut state, sequence, rows[..PAGE].to_vec());
        assert!(!state.list_changed);
        assert_eq!(state.rows, rows[..PAGE]);
        assert_eq!(state.after, Some(rows[PAGE - 1].id));
        assert!(state.more);

        // Adding an earlier row's warning must not move a later button's
        // native focused state to its neighbour in the retained widget tree.
        let mut state = State::default();
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Refresh) else {
            panic!()
        };
        receive_list(&mut state, sequence, rows[..PAGE].to_vec());
        let Some(Effect::Request { sequence, .. }) = state.update(Action::More) else {
            panic!()
        };
        receive_list(&mut state, sequence, rows[PAGE..].to_vec());
        state.update(Action::Search("Job 000".into()));
        let mut ui = frame(&state, Cache::new(), &mut renderer);
        let third = bounds(&ui, "Select automation Job 0003");
        assert!(Rectangle::with_size(Size::new(900.0, 700.0)).contains(third.center()));
        struct ButtonAt {
            bounds: Rectangle,
            id: Option<widget::Id>,
        }
        impl Operation for ButtonAt {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: Rectangle,
                _: &mut dyn operation::Focusable,
            ) {
                if bounds == self.bounds {
                    assert!(self.id.is_none(), "one native row target");
                    self.id = id.cloned();
                }
            }
        }
        let mut target = ButtonAt {
            bounds: third,
            id: None,
        };
        ui.operate(&renderer, &mut target);
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(target.id.expect("actual third row button")),
        );
        let cache = ui.into_cache();
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Tick) else {
            panic!()
        };
        let mut changed = rows[..PAGE].to_vec();
        changed[0].blocked = Some("Earlier row now needs inspection".into());
        receive_list(&mut state, sequence, changed);
        let mut ui = frame(&state, cache, &mut renderer);
        assert!(bounds(&ui, "Select automation Job 0003").y > third.y);
        let mut messages = Vec::new();
        let _ = ui.update(
            &[Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            })],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert!(
            matches!(messages.as_slice(), [Action::Select(id)] if *id == rows[2].id),
            "the original third row retains focus through an earlier warning: {messages:?}"
        );
        let _ = ui.into_cache();
        assert!(
            matches!(state.update(messages.remove(0)), Some(Effect::Request { request: Request::Show { id }, .. }) if id == rows[2].id)
        );

        // A rename may remove an earlier row from an unchanged search. The
        // held native press must not transfer to another UUID in its slot.
        let mut state = State::default();
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Refresh) else {
            panic!()
        };
        receive_list(&mut state, sequence, rows[..PAGE].to_vec());
        let Some(Effect::Request { sequence, .. }) = state.update(Action::More) else {
            panic!()
        };
        receive_list(&mut state, sequence, rows[PAGE..].to_vec());
        state.update(Action::Search("Job 000".into()));
        let mut ui = frame(&state, Cache::new(), &mut renderer);
        let point = bounds(&ui, "Select automation Job 0003").center();
        let mut messages = Vec::new();
        let _ = ui.update(
            &[
                Event::Mouse(mouse::Event::CursorMoved { position: point }),
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            ],
            mouse::Cursor::Available(point),
            &mut renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert!(
            messages.is_empty(),
            "a held row press has not selected yet: {messages:?}"
        );
        let cache = ui.into_cache();
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Tick) else {
            panic!()
        };
        let mut changed = rows[..PAGE].to_vec();
        changed[0].name = "Renamed outside current search".into();
        receive_list(&mut state, sequence, changed);
        let mut ui = frame(&state, cache, &mut renderer);
        let replacement = bounds(&ui, "Select automation Job 0004").center();
        assert_eq!(
            replacement, point,
            "the old held slot now belongs to another UUID"
        );
        let _ = ui.update(
            &[
                Event::Mouse(mouse::Event::CursorMoved {
                    position: replacement,
                }),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ],
            mouse::Cursor::Available(replacement),
            &mut renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert!(
            messages.is_empty(),
            "a filtered replacement cannot inherit the old row press: {messages:?}"
        );
        let point = bounds(&ui, "Select automation Job 0003").center();
        let _ = ui.update(
            &[
                Event::Mouse(mouse::Event::CursorMoved { position: point }),
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ],
            mouse::Cursor::Available(point),
            &mut renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert!(
            matches!(messages.as_slice(), [Action::Select(id)] if *id == rows[2].id),
            "a fresh press still selects the actual current UUID: {messages:?}"
        );
        let _ = ui.into_cache();
        assert!(
            matches!(state.update(messages.remove(0)), Some(Effect::Request { request: Request::Show { id }, .. }) if id == rows[2].id)
        );
    }

    #[test]
    fn paginated_list_refresh_rejects_bad_pages_and_yields_to_foreground_intent() {
        use crate::automations::HistoryPage;

        fn loaded() -> (State, Vec<Summary>) {
            let rows = list_refresh_rows(PAGE * 2);
            let mut state = State::default();
            let Some(Effect::Request { sequence, .. }) = state.update(Action::Refresh) else {
                panic!()
            };
            receive_list(&mut state, sequence, rows[..PAGE].to_vec());
            let Some(Effect::Request { sequence, .. }) = state.update(Action::More) else {
                panic!()
            };
            receive_list(&mut state, sequence, rows[PAGE..].to_vec());
            (state, rows)
        }
        for bad in [
            "duplicate",
            "reversed",
            "oversized",
            "before",
            "wrong response",
            "error",
        ] {
            let (mut state, rows) = loaded();
            let Some(Effect::Request { sequence, .. }) = state.update(Action::Tick) else {
                panic!()
            };
            receive_list(&mut state, sequence, rows[..PAGE].to_vec());
            let cursor = state
                .loaded_list_refresh
                .map(|value| (value.after, value.through));
            let manual = (state.after, state.more);
            let Some(Effect::Request { sequence, .. }) = state.update(Action::Tick) else {
                panic!()
            };
            let mut changed = rows[PAGE].clone();
            changed.enabled = false;
            let result = match bad {
                "duplicate" => Ok(Response::Listed(vec![changed.clone(), changed])),
                "reversed" => Ok(Response::Listed(vec![rows[PAGE + 1].clone(), changed])),
                "oversized" => Ok(Response::Listed(
                    list_refresh_rows(PAGE * 2 + 1)[PAGE..].to_vec(),
                )),
                "before" => Ok(Response::Listed(vec![rows[0].clone()])),
                "wrong response" => Ok(Response::Automation(automation())),
                _ => Err("Fixture list read failed".into()),
            };
            state.update(Action::Received {
                sequence,
                result: std::sync::Arc::new(result),
            });
            assert_eq!(
                state.rows, rows,
                "{bad} may not partly update a valid prefix"
            );
            assert_eq!((state.after, state.more), manual);
            assert_eq!(
                state
                    .loaded_list_refresh
                    .map(|value| (value.after, value.through)),
                cursor
            );
            assert!(!state.list_changed);
            assert!(state.selected.is_none() && state.history.is_empty());
            assert!(matches!(state.list_status, ListStatus::Failed(_)));
            assert!(
                matches!(state.update(Action::Tick), Some(Effect::Request { request: Request::List { after: Some(after), limit: PAGE }, .. }) if after == rows[PAGE-1].id)
            );
        }
        for intent in ["search", "more", "refresh", "new", "select"] {
            let (mut state, rows) = loaded();
            let manual = (state.after, state.more);
            let Some(Effect::Request { sequence: old, .. }) = state.update(Action::Tick) else {
                panic!()
            };
            let action = match intent {
                "search" => Action::Search("Job 0099".into()),
                "more" => Action::More,
                "refresh" => Action::Refresh,
                "new" => Action::New,
                _ => Action::Select(rows[0].id),
            };
            let effect = state.update(action);
            let accepted = state.pending.map(|(sequence, _)| sequence);
            let mut stale = rows[..PAGE].to_vec();
            stale[0].enabled = false;
            receive_list(&mut state, old, stale);
            assert_eq!(state.rows, rows, "{intent} retires the old read");
            assert_eq!((state.after, state.more), manual);
            assert_eq!(state.pending.map(|(sequence, _)| sequence), accepted);
            assert!(!state.list_changed);
            match intent {
                "search" => {
                    assert_eq!(state.search, "Job 0099");
                    assert!(state.pending.is_none());
                }
                "new" => assert!(state.draft.is_some()),
                "more" => assert!(
                    matches!(effect, Some(Effect::Request { request: Request::List { after: Some(after), .. }, .. }) if after == rows.last().unwrap().id)
                ),
                "refresh" => assert!(matches!(
                    effect,
                    Some(Effect::Request {
                        request: Request::List { after: None, .. },
                        ..
                    })
                )),
                _ => {
                    let Some(Effect::Request {
                        sequence,
                        request: Request::Show { id },
                    }) = effect
                    else {
                        panic!()
                    };
                    assert_eq!(id, rows[0].id);
                    let mut selected = automation();
                    selected.id = id;
                    assert!(
                        matches!(state.update(Action::Received { sequence, result: std::sync::Arc::new(Ok(Response::Automation(selected))) }), Some(Effect::Request { request: Request::History { id: Some(actual), .. }, .. }) if actual == id)
                    );
                    assert_eq!(state.selected.as_ref().unwrap().id, id);
                }
            }
        }

        // Save can insert a row outside the foreground UUID prefix. A failed
        // Browse retains it; More appends its real next page after that row.
        // Refresh must use UUID membership, without sorting the reader's slots.
        let (mut state, mut saved) = editing(None, None);
        state.update(Action::CancelEdit);
        let rows = list_refresh_rows(PAGE * 3);
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Browse) else {
            panic!()
        };
        receive_list(&mut state, sequence, rows[..PAGE].to_vec());
        state.update(Action::New);
        state.update(Action::Provider(1));
        state.update(Action::Field(Field::Name, "Saved outlier".into()));
        state.update(Action::Field(Field::Timezone, "UTC".into()));
        state.update(Action::Prompt(text_editor::Action::Edit(
            text_editor::Edit::Insert('x'),
        )));
        let Some(Effect::Request {
            sequence,
            request: Request::Create { definition },
        }) = state.update(Action::Save)
        else {
            panic!("the ordinary new draft issues Create: {:?}", state.error);
        };
        saved.id = Uuid::from_u128(1000);
        saved.definition = definition;
        let Some(Effect::Request {
            sequence: history,
            request: Request::History { id: Some(id), .. },
        }) = state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Automation(saved.clone()))),
        })
        else {
            panic!()
        };
        assert_eq!(id, saved.id);
        state.update(Action::Received {
            sequence: history,
            result: std::sync::Arc::new(Ok(Response::History(HistoryPage {
                firings: vec![],
                next: None,
            }))),
        });
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Browse) else {
            panic!()
        };
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Err("Fixture transient list failure".into())),
        });
        assert!(state.selected.is_none());
        let Some(Effect::Request { sequence, request }) = state.update(Action::More) else {
            panic!()
        };
        assert_eq!(
            request,
            Request::List {
                after: Some(rows[PAGE - 1].id),
                limit: PAGE
            }
        );
        receive_list(&mut state, sequence, rows[PAGE..PAGE * 2].to_vec());
        assert_eq!(state.rows[PAGE].id, saved.id);
        assert!(
            state.rows[PAGE].id > state.rows[PAGE + 1].id,
            "ordinary More leaves a non-sorted display snapshot"
        );
        let ids = state.rows.iter().map(|row| row.id).collect::<Vec<_>>();
        let manual = (state.after, state.more);
        for page in 0..3 {
            let Some(Effect::Request { sequence, request }) = state.update(Action::Tick) else {
                panic!()
            };
            assert_eq!(
                request,
                Request::List {
                    after: (page > 0).then(|| rows[page * PAGE - 1].id),
                    limit: PAGE
                }
            );
            receive_list(
                &mut state,
                sequence,
                rows[page * PAGE..(page + 1) * PAGE].to_vec(),
            );
            assert!(
                !state.list_changed,
                "unloaded rows beyond the manual cursor are not claimed new"
            );
        }
        let Some(Effect::Request { sequence, request }) = state.update(Action::Tick) else {
            panic!("the saved maximum UUID remains in this finite pass")
        };
        assert_eq!(
            request,
            Request::List {
                after: Some(rows.last().unwrap().id),
                limit: PAGE
            }
        );
        let mut outlier = saved.summary(Some(crate::automations::FiringState::Failed));
        outlier.enabled = false;
        receive_list(&mut state, sequence, vec![outlier.clone()]);
        assert_eq!(state.rows[PAGE], outlier);
        assert_eq!(state.rows.iter().map(|row| row.id).collect::<Vec<_>>(), ids);
        assert_eq!((state.after, state.more), manual);
        assert!(state.loaded_list_refresh.is_none());
        assert!(!state.list_changed);
        assert!(state.selected.is_none() && state.draft.is_none() && state.history.is_empty());
    }

    fn budget_history_rows(count: usize) -> Vec<Firing> {
        (1..=count)
            .rev()
            .map(|i| {
                let mut firing = history_firing(i as u64, crate::automations::FiringState::Running);
                firing.id = Uuid::from_u128(i as u128);
                firing.automation = Uuid::from_u128(10_000 + i as u128);
                firing
            })
            .collect()
    }

    fn load_budget_history(state: &mut State, backend: &[Firing], pages: usize) {
        for page in 0..pages {
            let Some(Effect::Request {
                sequence,
                request:
                    Request::History {
                        id: None,
                        before,
                        limit: PAGE,
                    },
            }) = state.update(if page == 0 {
                Action::AllHistory
            } else {
                Action::MoreHistory
            })
            else {
                panic!("ordinary manual history page")
            };
            assert_eq!(
                before,
                (page > 0).then(|| history_cursor(history_key(&backend[page * PAGE - 1])))
            );
            let end = (page + 1) * PAGE;
            assert!(
                state
                    .update(Action::Received {
                        sequence,
                        result: std::sync::Arc::new(Ok(Response::History(history_page(
                            backend[page * PAGE..end].to_vec(),
                            end < backend.len()
                        ))))
                    })
                    .is_none()
            );
        }
    }

    fn budget_poll(state: &mut State, backend: &[Firing]) -> Option<(u64, HistoryCursor)> {
        let Some(Effect::Request {
            sequence,
            request:
                Request::History {
                    id: None,
                    before: None,
                    limit: PAGE,
                },
        }) = state.update(Action::Tick)
        else {
            panic!("one newest metadata page per Tick")
        };
        match state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::History(history_page(
                backend[..backend.len().min(PAGE)].to_vec(),
                backend.len() > PAGE,
            )))),
        }) {
            Some(Effect::Request {
                sequence,
                request:
                    Request::History {
                        id: None,
                        before: Some(before),
                        limit: PAGE,
                    },
            }) => Some((sequence, before)),
            None => None,
            _ => panic!("never fan out full RunDetail reads"),
        }
    }

    fn receive_budget_page(
        state: &mut State,
        sequence: u64,
        before: &HistoryCursor,
        backend: &[Firing],
    ) {
        let older = backend
            .iter()
            .filter(|firing| history_key(firing) < (before.created_ms, before.id))
            .collect::<Vec<_>>();
        let page = older
            .iter()
            .take(PAGE)
            .map(|firing| (**firing).clone())
            .collect();
        assert!(
            state
                .update(Action::Received {
                    sequence,
                    result: std::sync::Arc::new(Ok(Response::History(history_page(
                        page,
                        older.len() > PAGE
                    ))))
                })
                .is_none(),
            "no immediate third read or detail chain"
        );
        assert!(state.pending.is_none());
    }

    #[test]
    fn history_poll_batches_loaded_metadata_once_and_fairly_reaches_older_release() {
        use crate::automations::FiringState;
        let mut backend = budget_history_rows(PAGE * 4);
        backend[PAGE * 3 - 1].state = FiringState::Failed;
        backend[PAGE * 3 - 1].release_pending = true;
        let mut state = State::default();
        load_budget_history(&mut state, &backend, 3);
        let ids = state
            .history
            .iter()
            .map(|firing| firing.id)
            .collect::<Vec<_>>();
        let manual = state.history_next.clone();
        backend[PAGE * 3 - 1].release_pending = false;
        for tick in 0..4 {
            let (sequence, before) = budget_poll(&mut state, &backend).expect("one older batch");
            let expected = if tick % 2 == 0 { PAGE } else { PAGE * 2 };
            assert_eq!(
                before,
                history_cursor(
                    including_history_key(history_key(&backend[expected]))
                        .unwrap()
                        .unwrap()
                )
            );
            assert!(
                backend[..PAGE]
                    .iter()
                    .all(|fresh| history_key(fresh) >= (before.created_ms, before.id)),
                "fresh newest rows are not requested twice"
            );
            receive_budget_page(&mut state, sequence, &before, &backend);
            assert_eq!(
                state
                    .history
                    .iter()
                    .map(|firing| firing.id)
                    .collect::<Vec<_>>(),
                ids
            );
            assert_eq!(state.history_next, manual);
            assert!(!state.history_newer);
            assert!(state.run.is_none() && state.selected.is_none() && state.draft.is_none());
            if tick >= 1 {
                assert_eq!(state.history[PAGE * 3 - 1].state, FiringState::Failed);
                assert!(
                    !state.history[PAGE * 3 - 1].release_pending,
                    "tail cleanup metadata refreshed without full details"
                );
            }
        }
        assert_eq!(
            including_history_key((7, Uuid::from_u128(u128::MAX))).unwrap(),
            Some((8, Uuid::nil()))
        );
        assert_eq!(
            including_history_key((i64::MAX as u64, Uuid::from_u128(u128::MAX))).unwrap(),
            None
        );
        assert!(including_history_key((u64::MAX, Uuid::nil())).is_err());
        assert!(including_history_key((u64::MAX, Uuid::from_u128(u128::MAX))).is_err());
    }

    #[test]
    fn history_batch_cursor_survives_new_arrivals_compaction_errors_and_foreground_supersession() {
        use crate::automations::{FiringState, HistoryPage};
        let original = budget_history_rows(PAGE * 4);
        let mut state = State::default();
        load_budget_history(&mut state, &original, 3);
        let ids = state
            .history
            .iter()
            .map(|firing| firing.id)
            .collect::<Vec<_>>();
        let manual = state.history_next.clone();
        let mut burst = (0..PAGE)
            .map(|i| {
                let mut firing = history_firing(1_000_000 - i as u64, FiringState::Running);
                firing.id = Uuid::from_u128(1_000_000 - i as u128);
                firing
            })
            .collect::<Vec<_>>();
        burst.extend(original.clone());
        let (sequence, before) = budget_poll(&mut state, &burst).unwrap();
        assert_eq!(
            before,
            history_cursor(
                including_history_key(history_key(&original[0]))
                    .unwrap()
                    .unwrap()
            ),
            "skip the entire unrequested million-key gap in one bounded query"
        );
        receive_budget_page(&mut state, sequence, &before, &burst);
        assert_eq!(state.history_next, manual);
        // Start the independent compaction/new-arrival pass from the same
        // ordinarily loaded snapshot.
        let mut state = State::default();
        load_budget_history(&mut state, &original, 3);
        // New rows lie before a finite scan; a deleted loaded row must not
        // become terminal or be dropped merely because no page returns it.
        let missing = original[PAGE + 5].id;
        for tick in 0..5 {
            let mut backend = budget_history_rows(PAGE * (6 + tick));
            backend.retain(|firing| firing.id != missing);
            for firing in &mut backend {
                if ids.contains(&firing.id) {
                    firing.state = FiringState::Succeeded;
                }
            }
            if let Some((sequence, before)) = budget_poll(&mut state, &backend) {
                receive_budget_page(&mut state, sequence, &before, &backend);
            }
            assert_eq!(
                state
                    .history
                    .iter()
                    .map(|firing| firing.id)
                    .collect::<Vec<_>>(),
                ids
            );
            assert_eq!(state.history_next, manual);
        }
        assert!(state.history_newer);
        assert_eq!(
            state.history.last().unwrap().state,
            FiringState::Succeeded,
            "new arrivals cannot restart a finite old range forever"
        );
        assert_eq!(
            state
                .history
                .iter()
                .find(|firing| firing.id == missing)
                .unwrap()
                .state,
            FiringState::Running
        );

        // A failed first batch advances only past an actually issued loaded
        // candidate. Later rows get a turn before the failed row is retried.
        let mut state = State::default();
        load_budget_history(&mut state, &original, 3);
        let (sequence, first) = budget_poll(&mut state, &original).unwrap();
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Err("bounded page failure".into())),
        });
        assert!(matches!(state.history_status, HistoryStatus::Failed(_)));
        let (sequence, second) = budget_poll(&mut state, &original).unwrap();
        assert!((second.created_ms, second.id) < (first.created_ms, first.id));
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Err("another bounded page failure".into())),
        });
        let (sequence, third) = budget_poll(&mut state, &original).unwrap();
        assert!((third.created_ms, third.id) < (second.created_ms, second.id));
        let mut completed = original.clone();
        for firing in &mut completed {
            firing.state = FiringState::Succeeded;
        }
        receive_budget_page(&mut state, sequence, &third, &completed);
        assert_eq!(
            state.history[PAGE].state,
            FiringState::Running,
            "failed candidate is not falsely settled"
        );
        assert_eq!(state.history[PAGE + 1].state, FiringState::Running);
        assert_eq!(state.history[PAGE + 2].state, FiringState::Succeeded);
        for _ in 0..3 {
            if let Some((sequence, before)) = budget_poll(&mut state, &completed) {
                receive_budget_page(&mut state, sequence, &before, &completed);
            }
        }
        assert_eq!(
            state.history[PAGE].state,
            FiringState::Succeeded,
            "wrap revisits the original failed candidate"
        );

        for next in [
            Action::Search("new filter".into()),
            Action::New,
            Action::Select(original[0].automation),
            Action::Refresh,
        ] {
            let mut state = State::default();
            load_budget_history(&mut state, &original, 3);
            let (sequence, before) = budget_poll(&mut state, &original).unwrap();
            state.update(next);
            let pending = state.pending.map(|(sequence, _)| sequence);
            let snapshot = state.history.clone();
            state.update(Action::Received {
                sequence,
                result: std::sync::Arc::new(Ok(Response::History(history_page(
                    original[PAGE..PAGE * 2].to_vec(),
                    true,
                )))),
            });
            assert_eq!(
                state.history, snapshot,
                "late valid page cannot cross foreground context"
            );
            assert_eq!(state.pending.map(|(sequence, _)| sequence), pending);
            assert!(before.created_ms > original.last().unwrap().created_ms);
        }

        for bad in ["foreign", "order", "cursor", "duplicate", "detail"] {
            let mut state = State::default();
            load_budget_history(&mut state, &original, 3);
            let snapshot = state.history.clone();
            let outcomes = state.last_firings.clone();
            let Some(Effect::Request { sequence, .. }) = state.update(Action::Tick) else {
                panic!("newest poll")
            };
            let mut page = history_page(original[..PAGE].to_vec(), true);
            match bad {
                "foreign" => page.firings[0].automation = Uuid::new_v4(),
                "order" => page.firings.swap(0, 1),
                "duplicate" => page.firings[1] = page.firings[0].clone(),
                "cursor" => page.next = Some(history_cursor(history_key(&original[0]))),
                _ => {}
            }
            let response = if bad == "detail" {
                Response::Detail(RunDetail {
                    definition: automation().definition,
                    firing: original[0].clone(),
                    agent: None,
                    run: None,
                })
            } else {
                Response::History(page)
            };
            assert!(
                state
                    .update(Action::Received {
                        sequence,
                        result: std::sync::Arc::new(Ok(response))
                    })
                    .is_none()
            );
            assert_eq!(
                state.history, snapshot,
                "newest malformed page cannot enter known rows: {bad}"
            );
            assert_eq!(
                state.last_firings, outcomes,
                "no outcome mutation before validation"
            );
            assert!(matches!(state.history_status, HistoryStatus::Failed(_)));
            assert!(state.run.is_none() && state.history_refresh.is_none());
        }

        let owner = automation();
        let mut scoped = original.clone();
        for firing in &mut scoped {
            firing.automation = owner.id;
        }
        let mut state = State::default();
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Select(owner.id)) else {
            panic!("select exact owner")
        };
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Automation(owner.clone()))),
        }) else {
            panic!("initial scoped history")
        };
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::History(history_page(
                scoped[..PAGE].to_vec(),
                true,
            )))),
        });
        let Some(Effect::Request { sequence, .. }) = state.update(Action::MoreHistory) else {
            panic!("older scoped page")
        };
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::History(history_page(
                scoped[PAGE..PAGE * 2].to_vec(),
                true,
            )))),
        });
        let Some(Effect::Request {
            sequence,
            request: Request::Show { id },
        }) = state.update(Action::Tick)
        else {
            panic!("selected definition still uses its existing Show read")
        };
        assert_eq!(id, owner.id);
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Automation(owner))),
        }) else {
            panic!("scoped newest read")
        };
        let snapshot = state.history.clone();
        let mut foreign = history_firing(999, FiringState::Succeeded);
        foreign.automation = Uuid::new_v4();
        assert!(
            state
                .update(Action::Received {
                    sequence,
                    result: std::sync::Arc::new(Ok(Response::History(history_page(
                        vec![foreign],
                        false
                    ))))
                })
                .is_none()
        );
        assert_eq!(
            state.history, snapshot,
            "unseen foreign firing cannot change a selected scope"
        );
        assert!(matches!(state.history_status, HistoryStatus::Failed(_)));

        // Typed pending rejects full-detail and malformed page shapes before
        // any update, while an empty valid response keeps absent rows intact.
        for bad in [
            "foreign",
            "order",
            "cursor",
            "oversized",
            "identity",
            "duplicate",
            "detail",
            "empty",
        ] {
            let mut state = State::default();
            load_budget_history(&mut state, &original, 3);
            let (sequence, _) = budget_poll(&mut state, &original).unwrap();
            let snapshot = state.history.clone();
            let mut page = history_page(original[PAGE..PAGE * 2].to_vec(), true);
            match bad {
                "foreign" => page.firings[0].automation = Uuid::new_v4(),
                "order" => page.firings.swap(0, 1),
                "duplicate" => page.firings[1] = page.firings[0].clone(),
                "cursor" => page.next = Some(history_cursor(history_key(&original[0]))),
                "oversized" => page = history_page(original[PAGE..PAGE * 2 + 1].to_vec(), false),
                "identity" => page.firings[0].created_ms -= 1,
                "empty" => {
                    page = HistoryPage {
                        firings: vec![],
                        next: None,
                    }
                }
                _ => {}
            }
            let response = if bad == "detail" {
                Response::Detail(RunDetail {
                    definition: automation().definition,
                    firing: original[PAGE].clone(),
                    agent: None,
                    run: None,
                })
            } else {
                Response::History(page)
            };
            assert!(
                state
                    .update(Action::Received {
                        sequence,
                        result: std::sync::Arc::new(Ok(response))
                    })
                    .is_none()
            );
            assert_eq!(
                state.history, snapshot,
                "atomic rejection/absence preserves loaded rows: {bad}"
            );
            assert_eq!(state.history_next, manual);
            assert!(state.run.is_none());
            if bad == "empty" {
                assert!(state.history_refresh.is_none());
            } else {
                assert!(
                    matches!(state.history_status, HistoryStatus::Failed(_)),
                    "{bad}"
                );
            }
        }
    }

    #[test]
    fn completed_run_notice_stays_with_its_automation_and_not_a_new_selection() {
        use crate::automations::{FiringState, HistoryPage, Trigger};

        fn receive_automation(state: &mut State, sequence: u64, value: Automation) {
            let Some(Effect::Request {
                sequence,
                request: Request::History { id, .. },
            }) = state.update(Action::Received {
                sequence,
                result: std::sync::Arc::new(Ok(Response::Automation(value.clone()))),
            })
            else {
                panic!("the selected definition reads its history")
            };
            assert_eq!(id, Some(value.id));
            state.update(Action::Received {
                sequence,
                result: std::sync::Arc::new(Ok(Response::History(HistoryPage {
                    firings: Vec::new(),
                    next: None,
                }))),
            });
        }
        let a = automation();
        let mut b = a.clone();
        b.id = Uuid::new_v4();
        b.definition.name = "Unrequested automation B".into();
        let mut state = State {
            rows: vec![a.summary(None), b.summary(None)],
            selected: Some(a.clone()),
            ..Default::default()
        };
        state.update(Action::RunNow);
        let generation = state.confirmation.as_ref().unwrap().generation;
        let Some(Effect::Request {
            sequence,
            request: Request::RunNow { id, request, .. },
        }) = state.update(Action::Confirm(generation))
        else {
            panic!("explicit run A")
        };
        assert_eq!(id, a.id);
        let mut firing = history_firing(100, FiringState::Accepted);
        firing.automation = a.id;
        firing.trigger = Trigger::Manual { request };
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Firing(firing))),
        });
        let message = "Run requested: Queued";
        assert_eq!(state.notice.as_deref(), Some(message));
        Outline::of(&state, 900.0, 700.0).text(message);

        // Ordinary reads of A preserve its own confirmation, including an
        // explicit same-subject selection and a list-based Refresh.
        for action in [Action::Tick, Action::Select(a.id)] {
            let Some(Effect::Request {
                sequence,
                request: Request::Show { id },
            }) = state.update(action)
            else {
                panic!("same A read")
            };
            assert_eq!(id, a.id);
            receive_automation(&mut state, sequence, a.clone());
            assert_eq!(state.notice.as_deref(), Some(message));
        }
        let Some(Effect::Request {
            sequence,
            request: Request::List { .. },
        }) = state.update(Action::Refresh)
        else {
            panic!("explicit Refresh")
        };
        let Some(Effect::Request {
            sequence,
            request: Request::Show { id },
        }) = state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Listed(vec![
                a.summary(None),
                b.summary(None),
            ]))),
        })
        else {
            panic!("Refresh reloads A")
        };
        assert_eq!(id, a.id);
        receive_automation(&mut state, sequence, a.clone());
        assert_eq!(state.notice.as_deref(), Some(message));

        // Refused navigation cannot consume the prior confirmation or an
        // accepted/uncertain request's original UUID.
        state.update(Action::RunNow);
        let generation = state.confirmation.as_ref().unwrap().generation;
        let Some(Effect::Request {
            sequence,
            request: Request::RunNow { request, .. },
        }) = state.update(Action::Confirm(generation))
        else {
            panic!("second explicit run A")
        };
        assert!(state.update(Action::Select(b.id)).is_none());
        assert_eq!(state.selected.as_ref().unwrap().id, a.id);
        assert_eq!(state.notice.as_deref(), Some(message));
        assert_eq!(
            state.manual_requests.get(&a.id),
            Some(&(a.execution_revision, request))
        );
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Err("Unobserved fixture reply".into())),
        });
        let Some(Effect::Request {
            sequence,
            request: Request::Show { id },
        }) = state.update(Action::Select(b.id))
        else {
            panic!("accepted selection B")
        };
        assert_eq!(id, b.id);
        receive_automation(&mut state, sequence, b.clone());
        assert_eq!(state.selected.as_ref().unwrap().id, b.id);
        assert!(state.notice.is_none());
        assert_eq!(
            state.manual_requests.get(&a.id),
            Some(&(a.execution_revision, request))
        );
        for width in [900.0, 360.0] {
            assert!(
                !Outline::of(&state, width, 700.0)
                    .texts
                    .iter()
                    .any(|(text, _)| text == message)
            );
        }

        // A removal already returned to the list has no selected subject to
        // leave: refreshing that list should retain its useful confirmation.
        state.update(Action::Remove);
        let generation = state.confirmation.as_ref().unwrap().generation;
        let Some(Effect::Request {
            sequence,
            request: Request::Remove { id, .. },
        }) = state.update(Action::Confirm(generation))
        else {
            panic!("explicit remove B")
        };
        assert_eq!(id, b.id);
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Removed)),
        });
        let removed = "Automation removed. Past runs remain in history.";
        assert_eq!(state.notice.as_deref(), Some(removed));
        Outline::of(&state, 900.0, 700.0).text(removed);
        assert!(state.selected.is_none());
        let Some(Effect::Request { sequence, .. }) = state.update(Action::Browse) else {
            panic!("already-list Browse")
        };
        state.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Listed(vec![a.summary(None)]))),
        });
        assert_eq!(state.notice.as_deref(), Some(removed));
        Outline::of(&state, 900.0, 700.0).text(removed);
    }

    #[test]
    fn cancellation_notice_retires_only_when_accepted_navigation_leaves_its_run() {
        fn detail(owner: &Automation) -> RunDetail {
            let mut value = run_of(owner);
            let run = crate::model::RunId::new();
            value.firing.state = crate::automations::FiringState::Running;
            value.firing.run = Some(run);
            value.run = Some(crate::agent_execution::Run {
                id: run,
                agent: crate::model::AgentId::new(),
                state: crate::agent_execution::State::Running,
                revision: 1,
                accepted_ms: 100,
                started_ms: Some(100),
                finished_ms: None,
                provider_key: None,
                result: Default::default(),
            });
            value
        }
        fn canceled(owner: &Automation, firing: &RunDetail) -> State {
            let mut state = State {
                selected: Some(owner.clone()),
                ..Default::default()
            };
            let Some(Effect::Request {
                sequence,
                request: Request::ShowFiring { id },
            }) = state.update(Action::Run(firing.firing.id))
            else {
                panic!("open actual firing")
            };
            assert_eq!(id, firing.firing.id);
            state.update(Action::Received {
                sequence,
                result: std::sync::Arc::new(Ok(Response::Detail(firing.clone()))),
            });
            let Some(Effect::Request {
                sequence,
                request:
                    Request::Cancel {
                        id,
                        firing: requested,
                    },
            }) = state.update(Action::CancelRun)
            else {
                panic!("explicit cancel")
            };
            assert_eq!((id, requested), (owner.id, firing.firing.id));
            assert!(
                state
                    .update(Action::InspectRun(crate::model::RunId::new()))
                    .is_none()
            );
            assert_eq!(state.run.as_ref().unwrap().firing.id, firing.firing.id);
            state.update(Action::Received {
                sequence,
                result: std::sync::Arc::new(Ok(Response::CancellationRequested)),
            });
            state
        }
        let owner = automation();
        let first = detail(&owner);
        let other = detail(&owner);
        let message = "Cancellation requested. Refresh the run for its confirmed outcome.";
        let mut state = canceled(&owner, &first);
        assert_eq!(state.notice.as_deref(), Some(message));
        Outline::of(&state, 900.0, 700.0).text(message);
        // Same firing read through normal tick, Refresh, row selection and
        // notification activation is not a new subject or completion.
        for action in [
            Action::Tick,
            Action::Refresh,
            Action::Run(first.firing.id),
            Action::InspectRun(first.run.as_ref().unwrap().id),
        ] {
            let Some(Effect::Request { sequence, .. }) = state.update(action) else {
                panic!("read the same run")
            };
            state.update(Action::Received {
                sequence,
                result: std::sync::Arc::new(Ok(Response::Detail(first.clone()))),
            });
            assert_eq!(state.notice.as_deref(), Some(message));
            assert_eq!(
                state.run.as_ref().unwrap().firing.state,
                crate::automations::FiringState::Running
            );
        }
        for action in [
            Action::Run(other.firing.id),
            Action::InspectRun(other.run.as_ref().unwrap().id),
        ] {
            let mut state = canceled(&owner, &first);
            let Some(Effect::Request { sequence, request }) = state.update(action) else {
                panic!("accepted other run read")
            };
            assert!(
                matches!(request, Request::ShowFiring { id } if id == other.firing.id)
                    || matches!(request, Request::ShowRun { run } if run == other.run.as_ref().unwrap().id)
            );
            state.update(Action::Received {
                sequence,
                result: std::sync::Arc::new(Ok(Response::Detail(other.clone()))),
            });
            assert_eq!(state.run.as_ref().unwrap().firing.id, other.firing.id);
            assert!(state.notice.is_none());
            assert!(
                !Outline::of(&state, 360.0, 700.0)
                    .texts
                    .iter()
                    .any(|(text, _)| text == message)
            );
        }
        for action in [Action::CloseRun, Action::Browse, Action::AllHistory] {
            let mut state = canceled(&owner, &first);
            state.update(action);
            assert!(state.run.is_none());
            assert!(state.notice.is_none());
        }
    }
    fn notice_page(id: Uuid, sequence: u64) -> crate::automations::LifecyclePage {
        crate::automations::LifecyclePage {
            events: vec![crate::automations::LifecycleEvent {
                automation: id,
                sequence,
                revision: 4,
                occurred_ms: 600,
                cause: crate::automations::LifecycleCause::PrecheckInterrupted,
                actor: crate::automations::LifecycleActor::System,
                enabled: false,
                blocked: true,
                firing: None,
            }],
            next: None,
            earlier_events_unavailable: false,
        }
    }

    #[test]
    fn exact_no_run_receipt_is_read_only_even_without_a_current_definition_and_at_narrow_width() {
        let id = Uuid::new_v4();
        let mut state = State::default();
        state.pending = Some((7, Pending::RefreshList));
        assert!(state.show_lifecycle_receipt(id, 42, Ok(notice_page(id, 42))));
        assert_eq!(
            state
                .lifecycle_notice
                .as_ref()
                .unwrap()
                .event
                .as_ref()
                .unwrap()
                .sequence,
            42
        );
        assert!(state.selected.is_none() && state.run.is_none() && state.rows.is_empty());
        for action in [
            Action::Tick,
            Action::Refresh,
            Action::Enable,
            Action::RunNow,
            Action::Remove,
            Action::OpenSession,
            Action::Run(Uuid::new_v4()),
            Action::New,
        ] {
            assert!(state.update(action).is_none());
        }
        assert!(
            state
                .update(Action::Received {
                    sequence: 7,
                    result: std::sync::Arc::new(Ok(Response::Listed(vec![
                        automation().summary(None)
                    ])))
                })
                .is_none()
        );
        assert!(state.rows.is_empty() && state.manual_requests.is_empty());
        for width in [280.0, 900.0] {
            let outline = Outline::of(&state, width, 600.0);
            assert!(
                outline
                    .texts
                    .iter()
                    .any(|(text, _)| text == "Precheck interrupted")
            );
            assert!(
                outline
                    .texts
                    .iter()
                    .any(|(text, _)| text.contains("past system event"))
            );
            assert!(
                !outline
                    .texts
                    .iter()
                    .any(|(text, _)| text.contains("Run now")
                        || text == "Enable"
                        || text.contains("Inspect run"))
            );
            assert!(outline.field.is_none());
        }
        assert!(matches!(
            state.update(Action::Browse),
            Some(Effect::Request {
                request: Request::List { after: None, .. },
                ..
            })
        ));
        assert!(state.lifecycle_notice.is_none());
    }

    #[test]
    fn expired_foreign_or_malformed_lifecycle_reply_never_substitutes_a_latest_row() {
        let id = Uuid::new_v4();
        let mut wrong_actor = notice_page(id, 42);
        wrong_actor.events[0].actor = crate::automations::LifecycleActor::UserManagement;
        let mut wrong_cause = notice_page(id, 42);
        wrong_cause.events[0].cause = crate::automations::LifecycleCause::Enabled;
        let mut long = notice_page(id, 42);
        long.events.push(long.events[0].clone());
        let mut cursor = notice_page(id, 42);
        cursor.next = Some(41);
        let mut invalid = notice_page(id, 42);
        invalid.events[0].revision = 0;
        for reply in [
            Err(()),
            Ok(crate::automations::LifecyclePage {
                events: vec![],
                next: None,
                earlier_events_unavailable: true,
            }),
            Ok(notice_page(Uuid::new_v4(), 42)),
            Ok(notice_page(id, 43)),
            Ok(wrong_actor),
            Ok(wrong_cause),
            Ok(long),
            Ok(cursor),
            Ok(invalid),
        ] {
            let mut state = State {
                selected: Some(automation()),
                ..Default::default()
            };
            assert!(state.show_lifecycle_receipt(id, 42, reply));
            let selected = state.lifecycle_notice.as_ref().unwrap();
            assert_eq!((selected.id, selected.sequence), (id, 42));
            assert!(selected.event.is_none());
            assert!(state.selected.is_none() && state.run.is_none());
            assert!(
                Outline::of(&state, 280.0, 600.0)
                    .texts
                    .iter()
                    .any(|(text, _)| text == "This exact receipt is unavailable")
            );
        }
    }

    #[test]
    fn lifecycle_activation_preserves_edits_consent_and_foreground_request_ownership() {
        let id = Uuid::new_v4();
        let owner = automation();
        let mut draft = State {
            selected: Some(owner.clone()),
            ..Default::default()
        };
        draft.update(Action::Edit);
        draft.update(Action::Field(Field::Name, "Keep this draft".into()));
        assert!(!draft.show_lifecycle_receipt(id, 42, Ok(notice_page(id, 42))));
        assert_eq!(draft.draft.as_ref().unwrap().name, "Keep this draft");
        assert!(draft.lifecycle_notice.is_none());
        let mut selecting = State::default();
        let Some(Effect::Request { sequence, .. }) = selecting.update(Action::Select(owner.id))
        else {
            panic!("foreground selection")
        };
        assert!(!selecting.show_lifecycle_receipt(id, 42, Ok(notice_page(id, 42))));
        assert_eq!(selecting.pending.unwrap().0, sequence);
        let owner_id = owner.id;
        let Some(Effect::Request { sequence, request }) = selecting.update(Action::Received {
            sequence,
            result: std::sync::Arc::new(Ok(Response::Automation(owner))),
        }) else {
            panic!("selection must finish its foreground history request");
        };
        assert!(matches!(request, Request::History {
            id: Some(actual), before: None, limit: PAGE,
        } if actual == owner_id));
        assert!(!selecting.show_lifecycle_receipt(id, 42, Ok(notice_page(id, 42))));
        assert!(
            matches!(selecting.pending, Some((actual, Pending::History { append: false })) if actual == sequence)
        );
        assert!(
            selecting
                .update(Action::Received {
                    sequence,
                    result: std::sync::Arc::new(Ok(Response::History(
                        crate::automations::HistoryPage {
                            firings: Vec::new(),
                            next: None,
                        }
                    ))),
                })
                .is_none()
        );
        assert!(selecting.pending.is_none());
        assert!(selecting.show_lifecycle_receipt(id, 42, Ok(notice_page(id, 42))));
        // Existing AgentRun navigation still leaves the historical-only panel.
        let run = crate::model::RunId::new();
        assert!(
            matches!(selecting.update(Action::InspectRun(run)),Some(Effect::Request{request:Request::ShowRun{run:actual},..}) if actual==run)
        );
        assert!(selecting.lifecycle_notice.is_none());
    }
}
pub fn state_label(state: crate::automations::FiringState) -> &'static str {
    use crate::automations::FiringState::*;
    match state {
        Prechecking => "Checking",
        Accepted => "Queued",
        Running => "Running",
        Waiting => "Waiting for input",
        Succeeded => "Succeeded",
        Failed => "Failed",
        Skipped => "Skipped",
        Cancelled => "Cancelled",
        Interrupted => "Outcome uncertain",
    }
}
