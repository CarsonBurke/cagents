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
};
use cosmic::{
    Element,
    iced::{Length, widget::text_editor},
    widget,
};
use uuid::Uuid;

const PAGE: usize = 50;
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
}
#[derive(Debug, Clone)]
pub struct AccountChoice {
    pub label: String,
    pub provider: Provider,
    pub id: AccountId,
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
    Tick,
    Refresh,
    More,
    Search(String),
    New,
    Select(Uuid),
    Edit,
    CancelEdit,
    Save,
    Field(Field, String),
    Provider(usize),
    Account(usize),
    Target(usize),
    Schedule(usize),
    Weekday(usize),
    Reuse(bool),
    Precheck(bool),
    Prompt(text_editor::Action),
    Shell(text_editor::Action),
    Enable,
    Pause,
    RunNow,
    Remove,
    Confirm(u64),
    CancelConfirmation,
    History,
    AllHistory,
    MoreHistory,
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
    pub fn background(&self) -> bool {
        matches!(self, Self::Received { .. } | Self::Tick)
    }
}

pub enum Effect {
    Request { sequence: u64, request: Request },
    OpenRun(RunDetail),
    ReadRun(crate::model::RunId),
}

#[derive(Clone, Copy)]
enum Pending {
    List { append: bool },
    Show,
    Save,
    Mutate,
    History { append: bool },
    Run,
    OpenRun,
}

struct Confirmation {
    generation: u64,
    automation: Automation,
    request: Request,
    title: &'static str,
}

pub struct Draft {
    base: Option<(Uuid, u64)>,
    name: String,
    model: String,
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
            base: None,
            name: String::new(),
            model: String::new(),
            permission: String::new(),
            timezone: "UTC".into(),
            hour: "9".into(),
            minute: "0".into(),
            cron: "0 9 * * 1-5".into(),
            timeout: "30".into(),
            provider: 0,
            account: None,
            target: (!targets.is_empty()).then_some(0),
            schedule: 1,
            weekday: 0,
            reuse: false,
            precheck: false,
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
            value.permission = definition.agent.permission_mode.clone().unwrap_or_default();
            value.timezone = definition.schedule.timezone.clone();
            value.provider = provider_index(definition.agent.provider);
            value.account = value
                .accounts
                .iter()
                .position(|entry| Some(&entry.id) == definition.agent.account.as_ref());
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
        value
    }

    fn definition(&self) -> Result<Definition, String> {
        let parse = |text: &str, label: &str| {
            text.parse::<u8>()
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
            },
            schedule: Schedule {
                pattern,
                timezone: self.timezone.trim().into(),
            },
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

#[derive(Default)]
pub struct State {
    manual_requests: std::collections::BTreeMap<Uuid, (u64, Uuid)>,
    sequence: u64,
    pending: Option<(u64, Pending)>,
    rows: Vec<Summary>,
    more: bool,
    after: Option<Uuid>,
    search: String,
    selected: Option<Automation>,
    draft: Option<Draft>,
    confirmation: Option<Confirmation>,
    history: Vec<Firing>,
    history_next: Option<HistoryCursor>,
    all_history: bool,
    run: Option<RunDetail>,
    error: Option<String>,
    notice: Option<String>,
    targets: Vec<TargetChoice>,
    accounts: Vec<AccountChoice>,
}

impl State {
    pub fn can_open_run(&self) -> bool {
        self.pending.is_none() && self.draft.is_none() && self.confirmation.is_none()
    }
    pub fn set_choices(&mut self, targets: Vec<TargetChoice>, accounts: Vec<AccountChoice>) {
        self.targets = targets;
        self.accounts = accounts;
    }
    fn request(&mut self, pending: Pending, request: Request) -> Option<Effect> {
        self.sequence = self.sequence.wrapping_add(1);
        self.pending = Some((self.sequence, pending));
        self.error = None;
        Some(Effect::Request {
            sequence: self.sequence,
            request,
        })
    }
    pub fn update(&mut self, action: Action) -> Option<Effect> {
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
            match result {
                Err(error) => self.error = Some(error),
                Ok(Response::Listed(rows)) => {
                    self.more = rows.len() == PAGE;
                    self.after = rows.last().map(|row| row.id);
                    if !matches!(pending, Pending::List { append: true }) {
                        self.rows.clear();
                    }
                    for row in rows {
                        if let Some(current) =
                            self.rows.iter_mut().find(|current| current.id == row.id)
                        {
                            *current = row;
                        } else {
                            self.rows.push(row);
                        }
                    }
                }
                Ok(Response::Automation(automation)) => {
                    let id = automation.id;
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
                        self.history_next = None;
                        self.notice = Some(
                            "Saved. Review the launch settings before enabling or running.".into(),
                        );
                    }
                }
                Ok(Response::History(page)) => {
                    if !matches!(pending, Pending::History { append: true }) {
                        self.history.clear();
                    }
                    self.history.extend(page.firings);
                    self.history_next = page.next;
                }
                Ok(Response::Detail(detail)) => {
                    if matches!(pending, Pending::OpenRun) {
                        self.run = Some(detail.clone());
                        return Some(Effect::OpenRun(detail));
                    }
                    let run = detail.run.as_ref().map(|run| run.id);
                    self.run = Some(detail);
                    return run.map(Effect::ReadRun);
                }
                Ok(Response::Firing(firing)) => {
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
                    if let Some(selected) = self.selected.take() {
                        self.rows.retain(|row| row.id != selected.id);
                    }
                    self.notice = Some("Automation removed. Past runs remain in history.".into());
                }
                Ok(Response::CancellationRequested) => {
                    self.notice = Some(
                        "Cancellation requested. Refresh the run for its confirmed outcome.".into(),
                    )
                }
            }
            return None;
        }
        if self.pending.is_some() {
            return None;
        }
        if matches!(action, Action::Tick) {
            if self.draft.is_some() || self.confirmation.is_some() {
                return None;
            }
            if let Some(run) = &self.run {
                return self.request(Pending::Run, Request::ShowFiring { id: run.firing.id });
            }
            if let Some(automation) = &self.selected {
                return self.request(Pending::Show, Request::Show { id: automation.id });
            }
            if self.all_history {
                // Keep paged history stable until an explicit refresh.
                return None;
            }
            if self.rows.len() <= PAGE {
                return self.request(
                    Pending::List { append: false },
                    Request::List {
                        after: None,
                        limit: PAGE,
                    },
                );
            }
            return None;
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
        self.confirmation = None;
        match action {
            Action::Browse => {
                self.run = None;
                self.selected = None;
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
                self.draft = Some(Draft::new(&self.targets, &self.accounts, None));
                self.run = None;
            }
            Action::Select(id) => {
                self.draft = None;
                self.all_history = false;
                self.history.clear();
                self.history_next = None;
                self.run = None;
                return self.request(Pending::Show, Request::Show { id });
            }
            Action::Edit => {
                self.draft = self
                    .selected
                    .as_ref()
                    .map(|value| Draft::new(&self.targets, &self.accounts, Some(value)))
            }
            Action::CancelEdit => self.draft = None,
            Action::Save => {
                if let Some(draft) = &self.draft {
                    match draft.definition() {
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
                }
            }
            Action::Provider(index) if index < PROVIDERS.len() => {
                if let Some(draft) = &mut self.draft {
                    draft.provider = index;
                    draft.account = None;
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
                }
            }
            Action::Target(index) => {
                if let Some(draft) = &mut self.draft {
                    if index < draft.targets.len() {
                        draft.target = Some(index);
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
            Action::History | Action::MoreHistory | Action::AllHistory => {
                if matches!(action, Action::AllHistory) {
                    self.all_history = true;
                    self.selected = None;
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
            Action::Run(id) => return self.request(Pending::Run, Request::ShowFiring { id }),
            Action::InspectRun(run) => {
                self.selected = None;
                self.all_history = false;
                self.run = None;
                self.history.clear();
                self.history_next = None;
                return self.request(Pending::Run, Request::ShowRun { run });
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
        let busy = self.pending.is_some();
        let mut content = widget::column::with_children(vec![
            widget::text::title3("Automations").into(),
            widget::text::body("Recurring agent work continues while this window is closed.")
                .into(),
        ])
        .spacing(12)
        .padding(16)
        .width(Length::Fill);
        if let Some(error) = &self.error {
            content = content.push(widget::text::body(error));
        }
        if let Some(notice) = &self.notice {
            content = content.push(widget::text::body(notice));
        }
        if busy {
            content = content.push(widget::text::body("Updating…"));
        }
        if let Some(confirmation) = &self.confirmation {
            content = content
                .push(widget::text::title4(confirmation.title))
                .push(launch_description(&confirmation.automation, &self.accounts));
            content = content.push(widget::text::body(if matches!(confirmation.request, Request::Remove { .. }) {
                "Past runs remain in history. Active or uncertain work must be closed before removal."
            } else {
                "The agent may change files and use tools allowed by these permissions. Confirm only this displayed launch configuration."
            }));
            content = content.push(
                widget::row::with_children(vec![
                    button("Cancel", Action::CancelConfirmation, !busy),
                    button("Confirm", Action::Confirm(confirmation.generation), !busy),
                ])
                .spacing(8)
                .wrap(),
            );
        } else if let Some(draft) = &self.draft {
            content = content.push(draft.view(busy));
        } else {
            content = content.push(
                widget::row::with_children(vec![
                    button("New automation", Action::New, !busy),
                    button(
                        "Automations",
                        Action::Browse,
                        !busy && (self.run.is_some() || self.all_history),
                    ),
                    button("Refresh", Action::Refresh, !busy),
                    button("All run history", Action::AllHistory, !busy),
                ])
                .spacing(8)
                .wrap(),
            );
            if self.run.is_none() {
                if !self.all_history {
                    content = content.push(
                        widget::text_input("Search loaded automations", &self.search)
                            .on_input(Action::Search),
                    );
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
                        let label = format!(
                            "{} · {}\n{}\nNext: {} · Last: {}",
                            row.name,
                            if row.enabled { "Enabled" } else { "Paused" },
                            row.directory.display(),
                            next,
                            row.last_state.map(state_label).unwrap_or("No runs")
                        );
                        content = content.push(button_owned(label, Action::Select(row.id), !busy));
                        if let Some(blocked) = &row.blocked {
                            content = content.push(widget::text::body(blocked));
                        }
                    }
                    if self.rows.is_empty() {
                        content = content.push(widget::text::body(
                            "Create an automation to schedule work in an open project.",
                        ));
                    }
                    if self.more {
                        content =
                            content.push(button("Load more automations", Action::More, !busy));
                    }
                    if let Some(selected) = &self.selected {
                        content = content
                            .push(widget::text::title4(selected.definition.name.as_str()))
                            .push(launch_description(selected, &self.accounts))
                            .push(widget::text::body(format!(
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
                            content = content.push(widget::text::body(blocked));
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
                            .spacing(8)
                            .wrap(),
                        );
                    }
                }
                if self.selected.is_some() || self.all_history {
                    content = content.push(button("Refresh history", Action::History, !busy));
                    if self.history.is_empty() && !busy {
                        content = content.push(widget::text::body("No runs in this history page."));
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
                                "{} · {} · {} · {}",
                                firing.name,
                                timestamp(firing.created_ms),
                                origin,
                                state_label(firing.state)
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
                content = content
                    .push(widget::text::title4(format!(
                        "{} · {}",
                        run.definition.name,
                        state_label(run.firing.state)
                    )))
                    .push(widget::text::body(format!(
                        "Started: {}\nTarget: {}",
                        timestamp(run.firing.created_ms),
                        run.definition.agent.target.directory.display()
                    )));
                if let Some(finished) = run.firing.finished_ms {
                    content = content.push(widget::text::body(format!(
                        "Finished: {}",
                        timestamp(finished)
                    )));
                }
                if let Some(diagnostic) = &run.firing.diagnostic {
                    content = content.push(widget::text::body(diagnostic));
                }
                if let Some(result) = run.run.as_ref().map(|run| &run.result) {
                    content = content.push(widget::text::body(result.text.as_str()));
                    if let Some(diagnostic) = &result.diagnostic {
                        content = content.push(widget::text::body(diagnostic));
                    }
                    if result.truncated {
                        content = content.push(widget::text::body(
                            "This result was shortened to keep history bounded.",
                        ));
                    }
                }
                content = content.push(
                    widget::row::with_children(vec![
                        button("Refresh run", Action::Run(run.firing.id), !busy),
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
                    .spacing(8)
                    .wrap(),
                );
            }
        }
        widget::scrollable(content)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    }
}

impl Draft {
    fn view(&self, busy: bool) -> Element<'_, Action> {
        let provider_accounts = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| account.provider == provider(self.provider))
            .collect::<Vec<_>>();
        let mut fields = widget::column::with_children(vec![
            widget::text::title4(if self.base.is_some() {
                "Edit automation"
            } else {
                "New automation"
            })
            .into(),
            input("Name", &self.name, Field::Name),
            widget::text::body("Project / working directory").into(),
            widget::dropdown(
                self.targets
                    .iter()
                    .map(|target| target.label.clone())
                    .collect::<Vec<_>>(),
                self.target,
                Action::Target,
            )
            .into(),
            widget::text::body("Provider").into(),
            widget::dropdown(PROVIDERS.to_vec(), Some(self.provider), Action::Provider).into(),
            widget::text::body("Account").into(),
            widget::dropdown(
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
            input(
                "Model (blank uses provider default)",
                &self.model,
                Field::Model,
            ),
            input(
                "Permission mode (blank uses provider default)",
                &self.permission,
                Field::Permission,
            ),
            widget::text::body("Prompt").into(),
            cosmic::iced::widget::text_editor(&self.prompt)
                .on_action(Action::Prompt)
                .height(160)
                .into(),
            widget::text::body("Schedule").into(),
            widget::dropdown(SCHEDULES.to_vec(), Some(self.schedule), Action::Schedule).into(),
            input(
                "IANA timezone, e.g. America/Vancouver",
                &self.timezone,
                Field::Timezone,
            ),
        ])
        .spacing(8)
        .width(Length::Fill);
        if self.schedule == 4 {
            fields = fields.push(input("Five-field cron", &self.cron, Field::Cron));
        } else {
            if self.schedule != 0 {
                fields = fields.push(input("Hour (0–23)", &self.hour, Field::Hour));
            }
            fields = fields.push(input("Minute (0–59)", &self.minute, Field::Minute));
            if self.schedule == 3 {
                fields = fields.push(widget::dropdown(
                    WEEKDAYS.to_vec(),
                    Some(self.weekday),
                    Action::Weekday,
                ));
            }
        }
        fields = fields.push(widget::checkbox(self.reuse).label("Reuse this automation's idle agent").on_toggle(Action::Reuse))
            .push(widget::text::body("Fresh sessions close after a result. Reuse keeps the same account, model and permissions; interactive takeover pauses automation reuse."))
            .push(widget::checkbox(self.precheck).label("Run a shell precheck").on_toggle(Action::Precheck));
        if self.precheck {
            fields = fields.push(widget::text::body("Runs once in the selected directory. A non-zero exit skips the agent; commands can change files."))
                .push(cosmic::iced::widget::text_editor(&self.shell).on_action(Action::Shell).height(100))
                .push(input("Precheck timeout in seconds (1–300)", &self.timeout, Field::Timeout));
        }
        fields.push(widget::text::body("New automations start paused. Changing the schedule or execution settings requires enabling them again. Missed runs older than 15 minutes are skipped; overlapping runs never start."))
            .push(widget::row::with_children(vec![button("Cancel", Action::CancelEdit, !busy), button("Save", Action::Save, !busy)]).spacing(8).wrap()).into()
    }
}

fn launch_description<'a>(
    automation: &'a Automation,
    accounts: &'a [AccountChoice],
) -> Element<'a, Action> {
    let definition = &automation.definition;
    let account = accounts
        .iter()
        .find(|account| Some(&account.id) == definition.agent.account.as_ref())
        .map(|account| account.label.as_str())
        .unwrap_or("Saved account is unavailable");
    let description = format!(
        "Provider: {}\nAccount: {}\nProject: {}\nDirectory: {}\nModel: {}\nPermissions: {}\nSession: {}\nSchedule: {} ({})\nRevision: {}",
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
        widget::text::body(description).into(),
        widget::text::body(definition.prompt.as_str()).into(),
    ])
    .spacing(8);
    if let Some(precheck) = &definition.precheck {
        column = column.push(widget::text::body(format!(
            "Shell precheck ({} seconds):\n{}",
            precheck.timeout_seconds, precheck.command
        )));
    }
    column.into()
}
fn button(label: &'static str, action: Action, enabled: bool) -> Element<'static, Action> {
    let mut button = widget::button::standard(label);
    if enabled {
        button = button.on_press(action);
    }
    button.into()
}
fn button_owned(label: String, action: Action, enabled: bool) -> Element<'static, Action> {
    // List and history entries can span several lines. Standard buttons reserve
    // a single-line height, hiding target and scheduling details.
    let mut button = widget::button::custom(widget::text::body(label.clone()))
        .name(label)
        .padding(12)
        .height(Length::Shrink);
    if enabled {
        button = button.on_press(action);
    }
    button.width(Length::Fill).into()
}
fn input<'a>(label: &'static str, value: &'a str, field: Field) -> Element<'a, Action> {
    widget::column::with_children(vec![
        widget::text::body(label).into(),
        widget::text_input(label, value)
            .on_input(move |value| Action::Field(field, value))
            .into(),
    ])
    .spacing(4)
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
                        channel: None,
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
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
        };
        let mut state = State {
            selected: Some(existing.clone()),
            ..State::default()
        };
        state.set_choices(vec![target], vec![account]);
        state.update(Action::Edit);
        // A refresh may remove or reorder the live catalog while the editor
        // is open; its numeric selections must retain their original meaning.
        state.set_choices(Vec::new(), Vec::new());
        let draft = state.draft.as_ref().unwrap();
        assert_eq!(draft.definition().unwrap(), existing.definition);
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
        assert!(state.update(Action::Tick).is_none());
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
    fn absent_saved_account_cannot_silently_switch_to_a_default() {
        let selected = automation();
        let draft = Draft::new(&[], &[], Some(&selected));
        assert!(
            draft
                .definition()
                .unwrap_err()
                .contains("explicit provider account")
        );
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
