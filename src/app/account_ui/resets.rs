//! Explicit, account-bound reset confirmation shared by usage and chat cards.
use super::*;
use account::resets::{ConsumeOutcome, ConsumeRequest, ConsumeStatus, UsageReset};

#[derive(Debug, Clone, Default)]
pub(super) struct State {
    pub confirmation: Option<Confirmation>,
    pending: Option<Attempt>,
    uncertain: BTreeMap<String, Attempt>,
    feedback: Option<String>,
    feedback_reset: Option<UsageReset>,
    feedback_status: Option<ConsumeStatus>,
    confirmed: BTreeMap<String, ConfirmedReset>,
}

#[derive(Debug, Clone)]
struct ConfirmedReset {
    reset: UsageReset,
    identity: Option<Identity>,
    observed_at: Millis,
    incidents: LimitIncidents,
}

/// Evidence existing before an explicit consume, scoped to one chat lifetime.
#[derive(Debug, Clone, Default)]
pub(in crate::app) struct LimitIncidents(pub BTreeMap<String, std::collections::BTreeSet<u64>>);

impl LimitIncidents {
    pub(in crate::app) fn contains(&self, current: &Self) -> bool {
        !current.0.is_empty()
            && current.0.iter().all(|(key, sources)| {
                !sources.is_empty()
                    && self
                        .0
                        .get(key)
                        .is_some_and(|known| sources.is_subset(known))
            })
    }
}

#[derive(Debug, Clone)]
pub(super) struct Confirmation {
    reset: UsageReset,
    key: String,
    total_count: u64,
    identity: Option<Identity>,
}

#[derive(Debug, Clone)]
struct Attempt {
    incidents: LimitIncidents,
    reset: UsageReset,
    key: String,
    total_count: u64,
    identity: Option<Identity>,
}

impl State {
    pub(super) fn in_flight(&self) -> bool {
        self.pending.is_some()
    }

    /// A recurring program reuses its ID each week. Only a fresh report of a
    /// later, available period can retire an ambiguous attempt from the old one.
    fn reconcile(&mut self, snapshot: &UsageSnapshot, now: Millis) {
        let before = self.uncertain.len();
        self.uncertain.retain(|_, attempt| {
            if attempt.reset.kind != account::resets::ResetKind::WeeklySession {
                return true;
            }
            let Some(period_end) = attempt.reset.expires_at else {
                return true;
            };
            let observed_after_period = i64::try_from(snapshot.observed_at / 1_000)
                .is_ok_and(|observed| observed >= period_end);
            !(period_end <= unix_seconds(now)
                && observed_after_period
                && snapshot.resets.iter().any(|current| {
                    current.id == attempt.reset.id
                        && current.kind == account::resets::ResetKind::WeeklySession
                        && current.is_available(unix_seconds(now))
                        && current.expires_at.is_some_and(|next| next > period_end)
                }))
        });
        if before != self.uncertain.len() && self.uncertain.is_empty() {
            self.feedback = None;
            self.feedback_reset = None;
        }
    }

    fn prepare(&mut self, reset: UsageReset, total_count: u64, identity: Option<Identity>) {
        if self.pending.is_some() || self.uncertain.contains_key(&reset.id) {
            return;
        }
        let key = uuid::Uuid::new_v4().to_string();
        self.confirmation = Some(Confirmation {
            reset,
            key,
            total_count,
            identity,
        });
    }

    fn submit(
        &mut self,
        current: &UsageReset,
        total_count: u64,
        identity: Option<&Identity>,
    ) -> Option<Attempt> {
        if self.pending.is_some() {
            return None;
        }
        let confirmation = self.confirmation.take()?;
        if &confirmation.reset != current
            || confirmation.total_count != total_count
            || confirmation.identity.as_ref() != identity
        {
            self.feedback_status = None;
            self.feedback = Some("Resets changed. Review the current count and try again.".into());
            self.feedback_reset = Some(current.clone());
            return None;
        }
        let attempt = Attempt {
            incidents: LimitIncidents::default(),
            reset: confirmation.reset,
            key: confirmation.key,
            total_count: confirmation.total_count,
            identity: confirmation.identity,
        };
        self.pending = Some(attempt.clone());
        self.feedback = None;
        self.feedback_reset = None;
        Some(attempt)
    }

    fn recover(&mut self, reset_id: &str, identity: Option<&Identity>) -> Option<Attempt> {
        if self.pending.is_some() {
            return None;
        }
        let attempt = self.uncertain.get(reset_id)?;
        if attempt.reset.kind == account::resets::ResetKind::WeeklySession
            || attempt.identity.as_ref() != identity
        {
            return None;
        }
        let attempt = attempt.clone();
        self.pending = Some(attempt.clone());
        self.confirmation = None;
        Some(attempt)
    }

    fn finish(&mut self, key: &str, status: ConsumeStatus, message: String) -> bool {
        if !self
            .pending
            .as_ref()
            .is_some_and(|attempt| attempt.key == key)
        {
            return false;
        }
        let attempt = self.pending.take().expect("pending key checked");
        self.feedback_reset = Some(attempt.reset.clone());
        self.feedback_status = Some(status);
        match status {
            ConsumeStatus::Unconfirmed => {
                self.uncertain.insert(attempt.reset.id.clone(), attempt);
            }
            // A recovery can fail before reaching the provider. That says
            // nothing about the earlier submitted attempt, whose key stays.
            ConsumeStatus::Unavailable => {}
            ConsumeStatus::Reset | ConsumeStatus::NoCredit | ConsumeStatus::NotLimited => {
                self.uncertain.remove(&attempt.reset.id);
            }
        }
        self.feedback = Some(message);
        true
    }
}

/// Owned presentation data lets transcript cards reuse the account controls
/// without keeping provider credentials or a second mutable account state.
#[derive(Clone)]
pub(in crate::app) struct Controls {
    id: AccountId,
    name: String,
    snapshot: UsageSnapshot,
    state: State,
    now: Millis,
    enabled: bool,
    recovery_enabled: bool,
    checking: bool,
    limit_context: bool,
    surface: String,
    identity: Option<Identity>,
}

fn actionable(snapshot: &UsageSnapshot, reset: &UsageReset, now: Millis) -> bool {
    reset.is_available(unix_seconds(now))
        && snapshot.windows.iter().any(|window| {
            (window.used >= account::WARNING_FRACTION || window.binding)
                && window.resets_at.is_none_or(|at| at > unix_seconds(now))
                && reset.clears(window)
        })
}

fn inventory(reset: &UsageReset, now: Millis) -> bool {
    reset.available_count > 0 && reset.expires_at.is_none_or(|at| at > unix_seconds(now))
}

fn normalized_scopes(reset: &UsageReset) -> Vec<account::resets::ResetScope> {
    let mut scopes = reset.scopes.clone();
    scopes.sort_by_key(account::resets::ResetScope::label);
    scopes.dedup();
    scopes
}

fn scope_count(snapshot: &UsageSnapshot, reset: &UsageReset, now: Millis) -> u64 {
    let scopes = normalized_scopes(reset);
    account::resets::inventory_groups(&snapshot.resets, unix_seconds(now))
        .into_iter()
        .find(|group| group.scopes == scopes && group.usable_now)
        .map_or(0, |group| group.count)
}

fn reset_order(left: &UsageReset, right: &UsageReset) -> std::cmp::Ordering {
    left.expires_at
        .unwrap_or(i64::MAX)
        .cmp(&right.expires_at.unwrap_or(i64::MAX))
        .then_with(|| left.id.cmp(&right.id))
}

fn can_offer(snapshot: &UsageSnapshot, state: &State, reset: &UsageReset, now: Millis) -> bool {
    actionable(snapshot, reset, now) && !state.uncertain.contains_key(&reset.id)
}

fn next_reset<'a>(
    snapshot: &'a UsageSnapshot,
    state: &State,
    now: Millis,
) -> Option<&'a UsageReset> {
    snapshot
        .resets
        .iter()
        .filter(|reset| can_offer(snapshot, state, reset, now))
        .min_by(|left, right| reset_order(left, right))
}

struct ScopeChoice {
    reset: UsageReset,
    count: u64,
}

impl ScopeChoice {
    fn label(&self) -> String {
        format!("{} · {} available", self.reset.label(), self.count)
    }
}

/// Offer one selection for each covered scope set, retaining the earliest
/// expiry within a group so the picker never repeats individual credits.
fn scope_choices(snapshot: &UsageSnapshot, state: &State, now: Millis) -> Vec<ScopeChoice> {
    let mut choices: Vec<ScopeChoice> = Vec::new();
    for reset in snapshot
        .resets
        .iter()
        .filter(|reset| can_offer(snapshot, state, reset, now))
    {
        let scopes = normalized_scopes(reset);
        if let Some(choice) = choices
            .iter_mut()
            .find(|choice| normalized_scopes(&choice.reset) == scopes)
        {
            if reset_order(reset, &choice.reset).is_lt() {
                choice.reset = reset.clone();
            }
        } else {
            choices.push(ScopeChoice {
                reset: reset.clone(),
                count: scope_count(snapshot, reset, now),
            });
        }
    }
    choices.sort_by_key(|choice| choice.reset.label());
    choices
}

fn reset_control_id(account: &AccountId, surface: &str, control: &str) -> widget::Id {
    widget::Id::new(format!("usage-reset/{surface}/{account}/{control}"))
}

fn scope_choice_id(account: &AccountId, surface: &str, reset_id: &str) -> widget::Id {
    widget::Id::new(format!("usage-reset-scope/{surface}/{account}/{reset_id}"))
}

fn scope_selector(
    account: &AccountId,
    name: &str,
    surface: &str,
    choices: Vec<ScopeChoice>,
    selected: &str,
    enabled: bool,
) -> Element<'static, Message> {
    let rows = choices
        .into_iter()
        .map(|choice| {
            let chosen = choice.reset.id == selected;
            crate::interaction::keyboard_list_row(
                widget::button::custom(design::caption(choice.label()))
                    .id(scope_choice_id(account, surface, &choice.reset.id))
                    .name(format!("Choose {} reset for {name}", choice.reset.label()))
                    .selected(chosen)
                    .class(crate::ui_controls::control_style(chosen, None))
                    .padding([space::XS, space::S])
                    .width(Length::Fill)
                    .on_press_maybe(enabled.then(|| {
                        Message::AccountUi(Action::ResetStart(
                            account.clone(),
                            choice.reset.id,
                            surface.to_owned(),
                            true,
                        ))
                    })),
            )
        })
        .collect::<Vec<_>>();
    crate::interaction::keyboard_list(
        widget::column::with_children(rows)
            .spacing(space::XXS)
            .width(Length::Fill),
        enabled,
    )
}

impl Controls {
    pub(in crate::app) fn on_surface(mut self, surface: impl Into<String>) -> Self {
        self.surface = surface.into();
        self
    }

    /// A confirmed reset and a fresh matching window establish that this
    /// historical limit is clear. Unknown, unrelated, and newly exhausted
    /// limits retain their ordinary presentation.
    pub(in crate::app) fn limit_cleared(&self, summary: &str) -> bool {
        if !self.enabled
            || is_stale(&self.snapshot, self.now)
            || self.now.saturating_sub(self.snapshot.observed_at) > schedule::STALE_AFTER
            || self.snapshot.observed_at > self.now.saturating_add(schedule::CLOCK_SLACK)
            || self.snapshot.ordinary_usage_allowed == Some(false)
            || !matches!(
                self.snapshot.status,
                UsageStatus::Ok | UsageStatus::Warning | UsageStatus::Limited
            )
        {
            return false;
        }
        let windows = self
            .snapshot
            .windows
            .iter()
            .filter(|window| limit_matches(summary, window))
            .collect::<Vec<_>>();
        if windows.is_empty()
            || windows
                .iter()
                .any(|window| !window.used.is_finite() || window.used >= 1.0 || window.binding)
        {
            return false;
        }
        self.state.confirmed.values().any(|confirmed| {
            confirmed.identity == self.identity
                && confirmed.observed_at <= self.snapshot.observed_at
                && windows.iter().all(|window| confirmed.reset.clears(window))
        })
    }

    /// A later rejection must not inherit an earlier reset's cleared label or
    /// success message, even before the next provider usage update arrives.
    pub(in crate::app) fn for_incident(&self, incident: &LimitIncidents) -> Self {
        let mut controls = self.clone();
        controls
            .state
            .confirmed
            .retain(|_, proof| proof.incidents.contains(incident));
        if controls.state.feedback_status == Some(ConsumeStatus::Reset)
            && !controls
                .state
                .feedback_reset
                .as_ref()
                .is_some_and(|reset| controls.state.confirmed.contains_key(&reset.id))
        {
            controls.state.feedback = None;
            controls.state.feedback_reset = None;
        }
        controls
    }

    pub(in crate::app) fn limit_guidance(&self, summary: &str, fallback: &str) -> String {
        let covered = self.enabled
            && self.snapshot.resets.iter().any(|reset| {
                can_offer(&self.snapshot, &self.state, reset, self.now)
                    && self.snapshot.windows.iter().any(|window| {
                        limit_matches(summary, window)
                            && reset.clears(window)
                            && (window.used >= account::WARNING_FRACTION || window.binding)
                            && window
                                .resets_at
                                .is_none_or(|at| at > unix_seconds(self.now))
                    })
            });
        if !covered {
            return fallback.into();
        }
        let wait = summary.split_once(" · resets ").map_or_else(
            || "for the limit to reset".into(),
            |(_, at)| format!("until {at}"),
        );
        let account_blocked = self.snapshot.ordinary_usage_allowed == Some(false)
            || self.snapshot.windows.iter().any(|window| {
                matches!(
                    window.kind,
                    account::WindowKind::Session | account::WindowKind::Weekly
                ) && (window.used >= 1.0 || window.binding)
                    && window
                        .resets_at
                        .is_none_or(|at| at > unix_seconds(self.now))
            });
        let alternative = if !account_blocked
            && self.snapshot.windows.iter().any(|window| {
                window.kind == account::WindowKind::Scoped && limit_matches(summary, window)
            }) {
            "another model or account"
        } else {
            "another account"
        };
        format!("Use a reset now, choose {alternative}, or wait {wait}.")
    }

    pub(in crate::app) fn for_limit(&self, summary: &str) -> Option<Self> {
        let cleared = self.limit_cleared(summary);
        let mut controls = self.clone();
        controls.limit_context = true;
        controls
            .snapshot
            .windows
            .retain(|window| limit_matches(summary, window));
        controls.snapshot.resets.retain(|reset| {
            inventory(reset, controls.now)
                && controls
                    .snapshot
                    .windows
                    .iter()
                    .any(|window| reset.clears(window))
        });
        if controls
            .state
            .confirmation
            .as_ref()
            .is_some_and(|confirmation| {
                !controls
                    .snapshot
                    .resets
                    .iter()
                    .any(|reset| reset.id == confirmation.reset.id)
            })
        {
            controls.state.confirmation = None;
        }
        controls.state.uncertain.retain(|_, attempt| {
            controls
                .snapshot
                .windows
                .iter()
                .any(|window| attempt.reset.clears(window))
        });
        if controls.state.pending.as_ref().is_some_and(|attempt| {
            !controls
                .snapshot
                .windows
                .iter()
                .any(|window| attempt.reset.clears(window))
        }) {
            controls.state.pending = None;
        }
        if !controls.state.feedback_reset.as_ref().is_some_and(|reset| {
            controls
                .snapshot
                .windows
                .iter()
                .any(|window| reset.clears(window))
        }) {
            controls.state.feedback = None;
            controls.state.feedback_reset = None;
        }
        (!controls.snapshot.resets.is_empty()
            || !controls.state.uncertain.is_empty()
            || controls.state.pending.is_some()
            || controls.state.feedback.is_some()
            || controls.checking
            || cleared)
            .then_some(controls)
    }

    pub(in crate::app) fn view(self) -> Element<'static, Message> {
        let mut rows: Vec<Element<'static, Message>> = Vec::new();
        for line in account::resets::inventory_lines(&self.snapshot.resets, unix_seconds(self.now))
        {
            rows.push(
                design::caption(line)
                    .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                    .into(),
            );
        }
        if self.checking && self.state.pending.is_none() {
            rows.push(design::caption("Checking usage…").into());
        }
        if let Some(pending) = &self.state.pending {
            rows.push(design::caption(format!("Using {}…", pending.reset.reset_phrase())).into());
        } else if let Some(confirmation) = &self.state.confirmation {
            let choices = scope_choices(&self.snapshot, &self.state, self.now);
            if choices.len() > 1 {
                rows.push(design::caption("Reset scope").into());
                rows.push(scope_selector(
                    &self.id,
                    &self.name,
                    &self.surface,
                    choices,
                    &confirmation.reset.id,
                    self.enabled,
                ));
            }
            rows.push(
                design::body(confirmation.reset.confirmation_question(&self.name))
                    .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                    .into(),
            );
            rows.push(
                design::caption(
                    confirmation
                        .reset
                        .confirmation_effect(confirmation.total_count),
                )
                .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                .into(),
            );
            rows.push(
                widget::row::with_children(vec![
                    crate::ui_controls::button::primary("Use reset")
                        .id(reset_control_id(&self.id, &self.surface, "confirm"))
                        .on_press_maybe(
                            self.enabled
                                .then(|| Message::AccountUi(Action::ResetConfirm(self.id.clone()))),
                        )
                        .into(),
                    crate::ui_controls::button::secondary("Cancel")
                        .id(reset_control_id(&self.id, &self.surface, "cancel"))
                        .on_press(Message::AccountUi(Action::ResetCancel(self.id.clone())))
                        .into(),
                ])
                .spacing(space::S)
                .into(),
            );
        } else if let Some(reset) = next_reset(&self.snapshot, &self.state, self.now) {
            let label = if self.limit_context {
                "Use a reset".into()
            } else {
                reset.action_label()
            };
            rows.push(
                crate::ui_controls::button::secondary(label)
                    .id(reset_control_id(&self.id, &self.surface, "start"))
                    .name(reset.confirmation_question(&self.name))
                    .on_press_maybe(self.enabled.then(|| {
                        Message::AccountUi(Action::ResetStart(
                            self.id.clone(),
                            reset.id.clone(),
                            self.surface.clone(),
                            scope_choices(&self.snapshot, &self.state, self.now).len() > 1,
                        ))
                    }))
                    .into(),
            );
        }
        if self.state.pending.is_none() {
            for attempt in self.state.uncertain.values() {
                let recurring = attempt.reset.kind == account::resets::ResetKind::WeeklySession;
                rows.push(
                    design::caption(format!(
                        "Requested {} for {} · {} available when confirmed.",
                        attempt.reset.reset_phrase(),
                        self.name,
                        attempt.total_count,
                    ))
                    .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                    .into(),
                );
                if !recurring {
                    rows.push(
                        design::caption("Retries reuse the original request.")
                            .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                            .into(),
                    );
                }
                rows.push(
                    crate::ui_controls::button::secondary(if recurring {
                        "Check usage"
                    } else {
                        "Retry same reset"
                    })
                    .name(format!(
                        "{} for {} · {}",
                        if recurring {
                            "Check usage"
                        } else {
                            "Retry same reset"
                        },
                        self.name,
                        attempt.reset.label()
                    ))
                    .on_press_maybe(self.recovery_enabled.then(|| {
                        Message::AccountUi(if recurring {
                            Action::Refresh(self.id.clone())
                        } else {
                            Action::ResetRecover(self.id.clone(), attempt.reset.id.clone())
                        })
                    }))
                    .into(),
                );
            }
        }
        if let Some(feedback) = self.state.feedback {
            rows.push(
                design::caption(feedback)
                    .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                    .into(),
            );
        }
        widget::column::with_children(rows)
            .spacing(space::XS)
            .width(Length::Fill)
            .into()
    }
}

/// Failure summaries are produced by our classifier, never provider titles or
/// arbitrary assistant text. An unnamed Claude limit needs an exhausted account
/// window; guessing between account and model limits could spend the wrong grant.
fn limit_matches(summary: &str, window: &UsageWindow) -> bool {
    let Some((provider, rest)) = summary.split_once(' ') else {
        return false;
    };
    let Some((name, _)) = rest.split_once(" limit reached") else {
        return false;
    };
    if provider == "Codex" {
        return window.key.starts_with("codex:")
            && matches!(
                window.kind,
                account::WindowKind::Session | account::WindowKind::Weekly
            );
    }
    match name {
        "weekly" => window.kind == account::WindowKind::Weekly,
        "session" | "5-hour" | "five-hour" => window.kind == account::WindowKind::Session,
        "usage" => {
            matches!(
                window.kind,
                account::WindowKind::Session | account::WindowKind::Weekly
            ) && (window.binding || window.used >= 1.0)
        }
        model if model.starts_with(|character: char| character.is_ascii_uppercase()) => {
            account::resets::ResetScope::Model(
                model.split_whitespace().next().unwrap_or(model).into(),
            )
            .clears(window)
        }
        _ => false,
    }
}

impl Cagents {
    pub(super) fn usage_reset_controls(&self, state: &AccountState) -> Option<Controls> {
        let snapshot = state.snapshot.as_ref()?;
        let checking = state.queued == Some(Trigger::Limit)
            || state
                .pending_probe
                .as_ref()
                .is_some_and(|ticket| ticket.limit_identity.is_some());
        let mut reset_state = state.reset.clone();
        reset_state.reconcile(snapshot, self.account_ui.now_ms);
        if snapshot
            .resets
            .iter()
            .all(|reset| !inventory(reset, self.account_ui.now_ms))
            && reset_state.pending.is_none()
            && reset_state.confirmation.is_none()
            && reset_state.feedback.is_none()
            && reset_state.uncertain.is_empty()
            && reset_state.confirmed.is_empty()
            && !checking
        {
            return None;
        }
        Some(Controls {
            id: state.id.clone(),
            name: self.account_ui.display_name(state),
            snapshot: snapshot.clone(),
            state: reset_state,
            identity: state.identity.clone(),
            checking,
            limit_context: false,
            surface: "accounts".into(),
            now: self.account_ui.now_ms,
            recovery_enabled: state.present
                && !state.reset.in_flight()
                && self.account_ui.login_conflict(state).is_none()
                && !self.account_sign_in_running(&state.account),
            enabled: state.present
                && !checking
                && !state.reset.in_flight()
                && state.failure.is_none()
                && !is_stale(snapshot, self.account_ui.now_ms)
                && self.account_ui.login_conflict(state).is_none()
                && !self.account_sign_in_running(&state.account),
        })
    }

    pub(in crate::app) fn chat_reset_controls(
        &self,
        provider: Provider,
        recipe: &LaunchRecipe,
    ) -> Option<Controls> {
        self.usage_reset_controls(account_of_recipe(
            &self.account_ui.accounts,
            provider,
            recipe,
        )?)
    }

    pub(super) fn start_usage_reset(
        &mut self,
        id: AccountId,
        reset_id: String,
        surface: String,
        choose_scope: bool,
    ) -> Task<Message> {
        let now = now_ms();
        self.account_ui.now_ms = now;
        if let Some(state) = self.account_ui.account_mut(&id)
            && let Some(snapshot) = &state.snapshot
        {
            state.reset.reconcile(snapshot, now);
        }
        let reset = self.account_ui.account(&id).and_then(|state| {
            let controls = self.usage_reset_controls(state)?;
            controls.enabled.then_some(())?;
            let reset = controls
                .snapshot
                .resets
                .iter()
                .find(|reset| reset.id == reset_id)?;
            can_offer(&controls.snapshot, &state.reset, reset, now_ms()).then(|| {
                (
                    reset.clone(),
                    scope_count(&controls.snapshot, reset, controls.now),
                    state.identity.clone(),
                )
            })
        });
        if let Some((reset, count, identity)) = reset
            && let Some(state) = self.account_ui.account_mut(&id)
        {
            let reset_id = reset.id.clone();
            state.reset.prepare(reset, count, identity);
            if state.reset.confirmation.is_some() {
                // Native buttons participate in Tab traversal; arrow movement
                // stays within the scope list and Enter only selects a scope.
                let several = choose_scope
                    && state.snapshot.as_ref().is_some_and(|snapshot| {
                        scope_choices(snapshot, &state.reset, now).len() > 1
                    });
                let focus = if several {
                    scope_choice_id(&id, &surface, &reset_id)
                } else {
                    reset_control_id(&id, &surface, "cancel")
                };
                return iced::widget::operation::focus(focus).map(cosmic::Action::App);
            }
        }
        Task::none()
    }

    pub(super) fn confirm_usage_reset(&mut self, id: AccountId) -> Task<Message> {
        self.account_ui.now_ms = now_ms();
        let reset = self.account_ui.account(&id).and_then(|state| {
            let controls = self.usage_reset_controls(state)?;
            controls.enabled.then_some(())?;
            let confirmation = state.reset.confirmation.as_ref()?;
            controls
                .snapshot
                .resets
                .iter()
                .find(|reset| reset.id == confirmation.reset.id)
                .filter(|reset| actionable(&controls.snapshot, reset, now_ms()))
                .map(|reset| {
                    (
                        reset.clone(),
                        scope_count(&controls.snapshot, reset, controls.now),
                    )
                })
        });
        let Some((reset, count)) = reset else {
            if let Some(state) = self.account_ui.account_mut(&id) {
                state.reset.feedback_reset = state
                    .reset
                    .confirmation
                    .take()
                    .map(|confirmation| confirmation.reset);
                state.reset.feedback_status = None;
                state.reset.feedback =
                    Some("Reset availability changed. Check usage and try again.".into());
            }
            return Task::none();
        };
        let mut incidents = LimitIncidents::default();
        for card in self.cards.values() {
            if let CardRuntime::Chat(runtime) = card
                && self
                    .chat_account(
                        runtime.session.launch.provider,
                        &runtime.session.launch.recipe,
                    )
                    .as_ref()
                    == Some(&id)
            {
                incidents.0.extend(runtime.reset_limit_incidents().0);
            }
        }
        let state = self.account_ui.account_mut(&id).expect("account checked");
        let Some(mut attempt) = state.reset.submit(&reset, count, state.identity.as_ref()) else {
            return Task::none();
        };
        attempt.incidents = incidents;
        state.reset.pending = Some(attempt.clone());
        let owner = state.owner;
        // A pre-consume probe must not overwrite the refreshed result.
        state.pending_probe = None;
        state.schedule.abandon();
        submit_attempt(id, owner, attempt)
    }

    pub(super) fn recover_usage_reset(&mut self, id: AccountId, reset_id: String) -> Task<Message> {
        let trusted = self
            .account_ui
            .account(&id)
            .and_then(|state| self.usage_reset_controls(state))
            .is_some_and(|controls| controls.recovery_enabled);
        if !trusted {
            return Task::none();
        }
        let state = self.account_ui.account_mut(&id).expect("account checked");
        let Some(attempt) = state.reset.recover(&reset_id, state.identity.as_ref()) else {
            return Task::none();
        };
        state.pending_probe = None;
        state.schedule.abandon();
        submit_attempt(id, state.owner, attempt)
    }

    pub(super) fn finish_usage_reset(
        &mut self,
        id: AccountId,
        owner: u64,
        key: String,
        result: Result<ConsumeOutcome, String>,
    ) -> Task<Message> {
        let Some(state) = self
            .account_ui
            .account_mut(&id)
            .filter(|state| state.owner == owner)
        else {
            return Task::none();
        };
        let submitted = state.reset.pending.clone();
        let recurring =
            state.reset.pending.as_ref().is_some_and(|attempt| {
                attempt.reset.kind == account::resets::ResetKind::WeeklySession
            });
        let (status, message, snapshot) = match result {
            Ok(outcome) => {
                let message = reset_feedback(
                    outcome.status,
                    outcome.message.as_deref(),
                    outcome.snapshot.is_some(),
                    recurring,
                );
                (outcome.status, message, outcome.snapshot)
            }
            Err(_) => (
                ConsumeStatus::Unconfirmed,
                reset_feedback(ConsumeStatus::Unconfirmed, None, false, recurring),
                None,
            ),
        };
        if !state.reset.finish(&key, status, message) {
            return Task::none();
        }
        if let Some(snapshot) = snapshot {
            let mut merged_inventory = false;
            if status == ConsumeStatus::Reset
                && let Some(attempt) = submitted
            {
                state.reset.confirmed.insert(
                    attempt.reset.id.clone(),
                    ConfirmedReset {
                        reset: attempt.reset,
                        identity: attempt.identity,
                        observed_at: snapshot.observed_at,
                        incidents: attempt.incidents,
                    },
                );
            }
            state.schedule.observe(snapshot.observed_at);
            if state.inventory_observed_at.is_none_or(|known| {
                snapshot.observed_at >= known
                    || known > now_ms().saturating_add(schedule::CLOCK_SLACK)
            }) {
                state.inventory_observed_at = Some(snapshot.observed_at);
                if let Some(current) = state.snapshot.as_mut()
                    && current.observed_at > snapshot.observed_at
                {
                    merge_complete_inventory(current, &snapshot);
                    merged_inventory = true;
                }
            }
            if state
                .snapshot
                .as_ref()
                .is_none_or(|current| current.observed_at <= snapshot.observed_at)
            {
                state.snapshot = Some(snapshot);
            }
            state.failure = None;
            let provider = state.account.provider;
            self.account_ui.now_ms = now_ms();
            let publish = if merged_inventory {
                self.save_live(id)
            } else {
                Task::none()
            };
            Task::batch([publish, self.probe_queued(provider)])
        } else {
            state.schedule.invalidate();
            self.update_account_ui(Action::Refresh(id))
        }
    }
}

fn reset_feedback(
    status: ConsumeStatus,
    detail: Option<&str>,
    refreshed: bool,
    recurring: bool,
) -> String {
    if status == ConsumeStatus::Unconfirmed {
        let recovery = if recurring {
            "Check usage before doing more."
        } else {
            "Review usage, then use Retry same reset if needed."
        };
        return format!(
            "The reset could not be confirmed. {recovery}{}",
            if refreshed {
                ""
            } else {
                " Current usage could not be refreshed."
            }
        );
    }
    if let Some(detail) = detail.filter(|detail| {
        !detail.trim().is_empty() && !detail.to_ascii_lowercase().contains("idempotency")
    }) {
        return detail.to_owned();
    }
    let message = match status {
        ConsumeStatus::Reset => "Reset used.",
        ConsumeStatus::NoCredit => "This reset is no longer available.",
        ConsumeStatus::NotLimited => "These limits no longer need a reset.",
        ConsumeStatus::Unavailable => "Reset unavailable. Check usage and try again.",
        ConsumeStatus::Unconfirmed => unreachable!(),
    };
    format!(
        "{message}{}",
        if refreshed {
            " Usage refreshed."
        } else {
            " Current usage could not be refreshed."
        }
    )
}

fn submit_attempt(id: AccountId, owner: u64, attempt: Attempt) -> Task<Message> {
    let key = attempt.key;
    let request = ConsumeRequest {
        account: id.clone(),
        reset_id: attempt.reset.id,
        idempotency_key: key.clone(),
        expected_identity: attempt.identity,
    };
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            match crate::device_service::request(crate::device_service::Operation::AccountReset {
                request,
            })? {
                crate::device_service::Reply::AccountReset(value) => {
                    serde_json::from_value(value).map_err(|error| error.to_string())
                }
                _ => Err("The device did not return the reset result".into()),
            }
        })
        .await
        .map_err(|_| "The reset result could not be read".to_owned())
        .and_then(|result| result);
        Message::AccountUi(Action::ResetFinished {
            id,
            owner,
            key,
            result,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::context_inspector_test_app;
    use account::resets::{ResetKind, ResetScope};

    fn reset(scope: ResetScope) -> UsageReset {
        UsageReset {
            id: "fixture-grant".into(),
            kind: ResetKind::Granted,
            scopes: vec![scope],
            available_count: 2,
            expires_at: None,
            usable: true,
        }
    }

    fn window(kind: account::WindowKind, key: &str, used: f32) -> UsageWindow {
        UsageWindow {
            key: key.into(),
            kind,
            label: key.into(),
            used,
            resets_at: None,
            span_secs: None,
            binding: false,
        }
    }

    #[test]
    fn a_model_grant_cannot_offer_a_reset_for_an_account_limit() {
        let model = window(account::WindowKind::Scoped, "weekly_scoped:Fable", 1.0);
        let weekly = window(account::WindowKind::Weekly, "weekly_all", 1.0);
        assert!(limit_matches(
            "Claude Fable 5.2 limit reached · resets tomorrow",
            &model
        ));
        assert!(!limit_matches(
            "Claude weekly limit reached · resets tomorrow",
            &model
        ));
        assert!(!limit_matches("Claude Fable limit reached", &weekly));
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now_ms());
        snapshot.windows = vec![weekly];
        assert!(!actionable(
            &snapshot,
            &reset(ResetScope::Model("Fable".into())),
            now_ms()
        ));
        snapshot.windows = vec![model];
        assert!(!actionable(
            &snapshot,
            &reset(ResetScope::AccountWeekly),
            now_ms()
        ));
        assert!(actionable(
            &snapshot,
            &reset(ResetScope::Model("Fable".into())),
            now_ms()
        ));
    }

    #[test]
    fn a_reset_requires_a_nearly_full_covered_window_and_unexpired_grant() {
        let now = now_ms();
        let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "", now);
        snapshot.windows = vec![window(account::WindowKind::Weekly, "weekly_all", 0.2)];
        let mut grant = reset(ResetScope::AccountWeekly);
        assert!(!actionable(&snapshot, &grant, now));
        snapshot.windows[0].used = account::WARNING_FRACTION;
        assert!(actionable(&snapshot, &grant, now));
        grant.expires_at = Some(unix_seconds(now));
        assert!(!actionable(&snapshot, &grant, now));
        grant.expires_at = None;
        snapshot.windows[0].resets_at = Some(unix_seconds(now));
        assert!(!actionable(&snapshot, &grant, now));
    }

    #[test]
    fn cancelling_and_double_submitting_never_start_an_extra_attempt() {
        let mut state = State::default();
        let grant = reset(ResetScope::AccountWeekly);
        state.prepare(grant.clone(), 2, None);
        state.confirmation = None;
        assert!(state.submit(&grant, 2, None).is_none());
        assert!(!state.in_flight());
        state.prepare(grant.clone(), 2, None);
        let attempt = state.submit(&grant, 2, None).unwrap();
        assert!(uuid::Uuid::parse_str(&attempt.key).is_ok());
        assert!(state.submit(&grant, 2, None).is_none());
        state.prepare(grant.clone(), 2, None);
        assert!(state.confirmation.is_none());
    }

    #[test]
    fn uncertainty_recovery_reuses_the_original_idempotency_key() {
        let mut state = State::default();
        let grant = reset(ResetScope::AccountWeekly);
        state.prepare(grant.clone(), 2, None);
        let first = state.submit(&grant, 2, None).unwrap();
        assert!(state.finish(&first.key, ConsumeStatus::Unconfirmed, "Check usage".into()));
        state.prepare(grant.clone(), 2, None);
        assert!(state.confirmation.is_none());
        let second = state.recover(&grant.id, None).unwrap();
        assert_eq!(second.key, first.key);
        assert!(state.finish(&second.key, ConsumeStatus::Reset, "Used".into()));
        state.prepare(grant, 2, None);
        assert_ne!(state.confirmation.as_ref().unwrap().key, first.key);
    }

    #[test]
    fn inventory_changes_require_a_new_confirmation() {
        let mut state = State::default();
        let mut grant = reset(ResetScope::AccountWeekly);
        state.prepare(grant.clone(), 2, None);
        grant.available_count = 1;
        assert!(state.submit(&grant, 2, None).is_none());
        assert!(!state.in_flight());
        assert!(state.feedback.as_ref().unwrap().contains("Resets changed"));
    }

    #[test]
    fn an_older_attempt_cannot_complete_the_current_attempt() {
        let mut state = State::default();
        let grant = reset(ResetScope::AccountWeekly);
        state.prepare(grant.clone(), 2, None);
        let current = state.submit(&grant, 2, None).unwrap();
        assert!(!state.finish("older-key", ConsumeStatus::Reset, "Used".into()));
        assert_eq!(state.pending.as_ref().unwrap().key, current.key);
        assert!(state.feedback.is_none());
    }

    #[test]
    fn reset_completion_cannot_replace_a_different_account_owner() {
        let mut app = context_inspector_test_app(false);
        let id = Account::system(Provider::Claude).id();
        let grant = reset(ResetScope::AccountWeekly);
        let state = app.account_ui.account_mut(&id).unwrap();
        state.reset.prepare(grant.clone(), 2, None);
        let attempt = state.reset.submit(&grant, 2, None).unwrap();
        let owner = state.owner;
        let mut before = UsageSnapshot::state(UsageStatus::Limited, "before", now_ms());
        before.resets.push(grant);
        state.snapshot = Some(before.clone());
        state.owner = owner.wrapping_add(1);
        drop(app.finish_usage_reset(
            id.clone(),
            owner,
            attempt.key,
            Ok(ConsumeOutcome {
                status: ConsumeStatus::Reset,
                message: None,
                snapshot: Some(UsageSnapshot::state(UsageStatus::Ok, "new", now_ms())),
            }),
        ));
        let state = app.account_ui.account(&id).unwrap();
        assert_eq!(state.snapshot.as_ref(), Some(&before));
        assert!(state.reset.in_flight());
    }
    #[test]
    fn multiple_credits_render_one_scope_count_and_choose_earliest_expiry() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree},
        };
        #[derive(Default)]
        struct Texts(Vec<String>);
        impl Operation for Texts {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
                self.0.push(text.into());
            }
        }
        let now = now_ms();
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
        snapshot
            .windows
            .push(window(account::WindowKind::Weekly, "codex:secondary", 1.0));
        for (id, expiry) in [("later", 400), ("first", 200), ("middle", 300)] {
            let mut credit = reset(ResetScope::Account);
            credit.id = id.into();
            credit.available_count = 1;
            credit.expires_at = Some(unix_seconds(now) + expiry);
            snapshot.resets.push(credit);
        }
        let chosen = next_reset(&snapshot, &State::default(), now).unwrap();
        assert_eq!(chosen.id, "first");
        assert_eq!(scope_count(&snapshot, chosen, now), 3);
        let mut state = State::default();
        state.prepare(chosen.clone(), 3, None);
        assert_eq!(state.confirmation.as_ref().unwrap().total_count, 3);
        let controls = Controls {
            id: Account::system(Provider::Codex).id(),
            name: "Fixture".into(),
            snapshot,
            state: State::default(),
            now,
            enabled: true,
            recovery_enabled: true,
            checking: false,
            limit_context: false,
            surface: "accounts".into(),
            identity: None,
        };
        let renderer = crate::app::tests::headless_renderer();
        let mut element = controls.view();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(320.0, 900.0)),
        );
        let mut texts = Texts::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
        let inventory: Vec<_> = texts
            .0
            .iter()
            .filter(|text| text.starts_with("3 account resets"))
            .collect();
        assert_eq!(inventory.len(), 1);
        assert!(inventory[0].contains("3 account resets · first expires"));
    }

    #[test]
    fn identity_changes_cancel_the_confirmation_before_submission() {
        let mut state = State::default();
        let grant = reset(ResetScope::AccountWeekly);
        let original = Identity {
            organization: Some("fixture-organization-a".into()),
            ..Identity::default()
        };
        let changed = Identity {
            organization: Some("fixture-organization-b".into()),
            ..Identity::default()
        };
        state.prepare(grant.clone(), 2, Some(original));
        assert!(state.submit(&grant, 2, Some(&changed)).is_none());
        assert!(!state.in_flight());
    }
    #[test]
    fn a_depleted_ambiguous_grant_can_be_checked_without_blocking_another_scope() {
        let now = now_ms();
        let original = reset(ResetScope::Model("Fable".into()));
        let mut state = State::default();
        state.prepare(original.clone(), 2, None);
        let attempt = state.submit(&original, 2, None).unwrap();
        state.finish(
            &attempt.key,
            ConsumeStatus::Unconfirmed,
            "Retry same reset".into(),
        );
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
        snapshot.windows = vec![
            window(account::WindowKind::Scoped, "weekly_scoped:Fable", 0.0),
            window(account::WindowKind::Weekly, "weekly_all", 1.0),
        ];
        let mut depleted = original.clone();
        depleted.available_count = 0;
        snapshot.resets.push(depleted);
        let mut weekly = reset(ResetScope::AccountWeekly);
        weekly.id = "weekly-fixture".into();
        snapshot.resets.push(weekly.clone());
        assert_eq!(
            next_reset(&snapshot, &State::default(), now).unwrap().id,
            weekly.id
        );
        let recovery = state.recover(&original.id, None).unwrap();
        assert_eq!(recovery.key, attempt.key);
        assert_eq!(recovery.reset.id, original.id);
        assert!(
            state.recover(&original.id, None).is_none(),
            "no concurrent retry"
        );
        state.finish(
            &recovery.key,
            ConsumeStatus::Unconfirmed,
            "Check again".into(),
        );
        state.prepare(weekly.clone(), 2, None);
        let separate = state.submit(&weekly, 2, None).unwrap();
        assert_ne!(separate.key, recovery.key);
        assert!(state.uncertain.contains_key(&original.id));
    }

    #[test]
    fn a_recurring_session_reset_can_only_refresh_after_uncertainty() {
        let mut state = State::default();
        let mut recurring = reset(ResetScope::AccountSession);
        recurring.kind = ResetKind::WeeklySession;
        state.prepare(recurring.clone(), 1, None);
        let attempt = state.submit(&recurring, 1, None).unwrap();
        state.finish(
            &attempt.key,
            ConsumeStatus::Unconfirmed,
            "Check usage".into(),
        );
        assert!(state.recover(&recurring.id, None).is_none());
        state.prepare(recurring.clone(), 1, None);
        assert!(state.confirmation.is_none());
    }

    #[test]
    fn unknown_scope_and_partial_expiry_details_remain_understandable() {
        let now = now_ms();
        let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "", now);
        let mut known = reset(ResetScope::Account);
        known.expires_at = Some(unix_seconds(now) + 200);
        let mut count_only = reset(ResetScope::Account);
        count_only.id = "count-only".into();
        let mut unknown = reset(ResetScope::Account);
        unknown.scopes.clear();
        snapshot.resets = vec![known, count_only, unknown];
        let groups = account::resets::inventory_groups(&snapshot.resets, unix_seconds(now));
        assert_eq!(groups.len(), 2);
        let account = groups
            .iter()
            .find(|group| group.label == "Account limits")
            .unwrap();
        assert_eq!(account.count, 4);
        assert_eq!(account.next_expiry, Some(unix_seconds(now) + 200));
        assert!(account.unknown_expiry);
        assert!(groups.iter().any(|group| group.label == "Unknown scope"));
    }
    #[test]
    fn a_new_recurring_period_retires_only_expired_uncertainty_from_a_fresh_report() {
        let now = now_ms();
        let period_end = unix_seconds(now) - 10;
        let mut recurring = reset(ResetScope::AccountSession);
        recurring.id = "juniper_tide".into();
        recurring.kind = ResetKind::WeeklySession;
        recurring.expires_at = Some(period_end);
        let mut state = State::default();
        state.prepare(recurring.clone(), 1, None);
        let previous = state.submit(&recurring, 1, None).unwrap();
        state.finish(
            &previous.key,
            ConsumeStatus::Unconfirmed,
            "Check usage".into(),
        );
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
        snapshot
            .windows
            .push(window(account::WindowKind::Session, "session", 1.0));
        let mut next_period = recurring.clone();
        next_period.expires_at = Some(period_end + 7 * 86_400);
        snapshot.resets.push(next_period.clone());
        snapshot.observed_at = u64::try_from(period_end - 1).unwrap() * 1_000;
        state.reconcile(&snapshot, now);
        assert!(
            state.uncertain.contains_key(&recurring.id),
            "old cached inventory cannot release the guard"
        );
        snapshot.observed_at = now;
        snapshot.resets[0].usable = false;
        state.reconcile(&snapshot, now);
        assert!(
            state.uncertain.contains_key(&recurring.id),
            "a later but unavailable program is not a new offer"
        );
        snapshot.resets[0].usable = true;
        state.reconcile(&snapshot, now);
        assert!(state.uncertain.is_empty());
        assert!(state.feedback.is_none());
        assert_eq!(next_reset(&snapshot, &state, now).unwrap().id, recurring.id);
        state.prepare(next_period.clone(), 1, None);
        let next = state.submit(&next_period, 1, None).unwrap();
        assert_ne!(
            next.key, previous.key,
            "a new period starts a new logical request"
        );
    }
    #[test]
    fn unavailable_recovery_preserves_the_original_uncertain_request_key() {
        let mut state = State::default();
        let grant = reset(ResetScope::AccountWeekly);
        state.prepare(grant.clone(), 2, None);
        let original = state.submit(&grant, 2, None).unwrap();
        assert!(state.finish(
            &original.key,
            ConsumeStatus::Unconfirmed,
            "Unconfirmed".into()
        ));
        let retry = state.recover(&grant.id, None).unwrap();
        assert_eq!(retry.key, original.key);
        assert!(state.finish(
            &retry.key,
            ConsumeStatus::Unavailable,
            "Provider unavailable".into()
        ));
        assert_eq!(state.uncertain.get(&grant.id).unwrap().key, original.key);
        state.prepare(grant.clone(), 2, None);
        assert!(state.confirmation.is_none());
        let next_retry = state.recover(&grant.id, None).unwrap();
        assert_eq!(next_retry.key, original.key);
    }
    #[test]
    fn confirmation_can_choose_each_actionable_scope_and_cannot_change_a_submitted_attempt() {
        let now = now_ms();
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
        snapshot.windows = vec![
            window(account::WindowKind::Session, "session", 0.82),
            window(account::WindowKind::Scoped, "weekly_scoped:Fable", 1.0),
        ];
        let mut session = reset(ResetScope::AccountSession);
        session.id = "session-fixture".into();
        session.available_count = 1;
        let mut fable = reset(ResetScope::Model("Fable".into()));
        fable.id = "fable-fixture".into();
        snapshot.resets = vec![session.clone(), fable.clone()];
        let mut state = State::default();
        let choices = scope_choices(&snapshot, &state, now);
        assert_eq!(choices.len(), 2);
        assert!(choices.iter().any(
            |choice| choice.reset.id == session.id && choice.label() == "Session · 1 available"
        ));
        assert!(
            choices.iter().any(
                |choice| choice.reset.id == fable.id && choice.label() == "Fable · 2 available"
            )
        );
        state.prepare(session, 1, None);
        let selected = choices
            .iter()
            .find(|choice| choice.reset.id == fable.id)
            .unwrap();
        state.prepare(selected.reset.clone(), selected.count, None);
        let confirmed = state.confirmation.as_ref().unwrap();
        assert_eq!(confirmed.reset.id, fable.id);
        assert_eq!(confirmed.total_count, 2);
        let attempt = state.submit(&selected.reset, selected.count, None).unwrap();
        let other = choices
            .iter()
            .find(|choice| choice.reset.id != fable.id)
            .unwrap();
        state.prepare(other.reset.clone(), other.count, None);
        assert!(state.confirmation.is_none());
        let pending = state.pending.as_ref().unwrap();
        assert_eq!(pending.reset.id, fable.id);
        assert_eq!(pending.key, attempt.key);
        assert_eq!(pending.total_count, 2);
        let controls = Controls {
            id: Account::system(Provider::Claude).id(),
            name: "Fixture".into(),
            snapshot,
            state: State::default(),
            now,
            enabled: true,
            recovery_enabled: true,
            checking: false,
            limit_context: false,
            surface: "accounts".into(),
            identity: None,
        };
        let card = controls.for_limit("Claude Fable limit reached").unwrap();
        let card_choices = scope_choices(&card.snapshot, &card.state, now);
        assert_eq!(card_choices.len(), 1);
        assert_eq!(card_choices[0].reset.id, fable.id);
    }
    #[test]
    fn an_uncertain_grant_has_only_its_recovery_action_and_other_scopes_stay_available() {
        let now = now_ms();
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
        snapshot.windows = vec![
            window(account::WindowKind::Session, "session", 0.82),
            window(account::WindowKind::Scoped, "weekly_scoped:Fable", 1.0),
        ];
        let mut fable = reset(ResetScope::Model("Fable".into()));
        fable.id = "fable-fixture".into();
        let mut session = reset(ResetScope::AccountSession);
        session.id = "session-fixture".into();
        snapshot.resets = vec![fable.clone(), session.clone()];
        let mut state = State::default();
        state.prepare(fable.clone(), 2, None);
        let attempt = state.submit(&fable, 2, None).unwrap();
        state.finish(
            &attempt.key,
            ConsumeStatus::Unconfirmed,
            "Unconfirmed".into(),
        );
        assert!(!can_offer(&snapshot, &state, &fable, now));
        assert!(can_offer(&snapshot, &state, &session, now));
        let choices = scope_choices(&snapshot, &state, now);
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].reset.id, session.id);
        assert_eq!(state.recover(&fable.id, None).unwrap().key, attempt.key);
    }

    #[test]
    fn reset_feedback_is_plain_and_preserves_actionable_failures_and_refresh_uncertainty() {
        let unconfirmed = reset_feedback(
            ConsumeStatus::Unconfirmed,
            Some("Retry with the same idempotency key"),
            false,
            false,
        );
        assert!(!unconfirmed.contains("idempotency"));
        assert!(unconfirmed.contains("Retry same reset"));
        assert!(unconfirmed.contains("Current usage could not be refreshed"));
        let recurring = reset_feedback(
            ConsumeStatus::Unconfirmed,
            Some("Retry with the same idempotency key"),
            true,
            true,
        );
        assert!(recurring.contains("Check usage"));
        assert!(!recurring.contains("Retry same reset"));
        let login = "The account login changed. Refresh usage and confirm again.";
        assert_eq!(
            reset_feedback(ConsumeStatus::Unavailable, Some(login), true, false),
            login
        );
        let used = reset_feedback(ConsumeStatus::Reset, None, false, false);
        assert!(used.starts_with("Reset used."));
        assert!(!used.contains("Usage refreshed"));
    }

    #[test]
    fn actual_account_scope_controls_support_tab_arrows_enter_and_cancel_without_spending() {
        use iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Operation, Tree, operation},
        };
        use iced::{Event, Rectangle, Size, mouse};
        fn perform<T: Send + 'static>(
            element: &mut Element<'_, Message>,
            tree: &mut Tree,
            node: &layout::Node,
            renderer: &cosmic::Renderer,
            mut operation: Box<dyn Operation<T>>,
        ) -> Option<T> {
            loop {
                element.as_widget_mut().operate(
                    tree,
                    Layout::new(node),
                    renderer,
                    &mut operation::black_box(operation.as_mut()),
                );
                match operation.finish() {
                    operation::Outcome::Chain(next) => operation = next,
                    operation::Outcome::Some(value) => return Some(value),
                    operation::Outcome::None => return None,
                }
            }
        }
        let press = |key| keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        let now = now_ms();
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
        snapshot.windows = vec![
            window(account::WindowKind::Session, "session", 0.82),
            window(account::WindowKind::Scoped, "weekly_scoped:Fable", 1.0),
        ];
        let mut session = reset(ResetScope::AccountSession);
        session.id = "session-fixture".into();
        session.available_count = 1;
        let mut fable = reset(ResetScope::Model("Fable".into()));
        fable.id = "fable-fixture".into();
        snapshot.resets = vec![session.clone(), fable.clone()];
        let mut app = context_inspector_test_app(false);
        let id = Account::system(Provider::Claude).id();
        let state = app.account_ui.account_mut(&id).unwrap();
        state.present = true;
        state.snapshot = Some(snapshot);
        app.account_ui.installed.insert(Provider::Claude, true);
        drop(app.update(Message::AccountUi(Action::ResetStart(
            id.clone(),
            session.id.clone(),
            "accounts".into(),
            true,
        ))));
        assert!(
            app.account_ui
                .account(&id)
                .unwrap()
                .reset
                .confirmation
                .is_some()
        );
        assert!(
            app.core.keyboard_nav(),
            "the actual account confirmation keeps COSMIC Tab routing enabled"
        );
        assert!(
            app.account_key_event(&press(keyboard::key::Named::Tab))
                .is_none(),
            "account popup handling leaves Tab to COSMIC FocusNext"
        );
        let renderer = crate::app::tests::headless_renderer();
        let mut element = app.accounts_pane();
        let mut tree = Tree::new(element.as_widget());
        let bounds = Rectangle::with_size(Size::new(1200.0, 1800.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        let fable_id = scope_choice_id(&id, "accounts", &fable.id);
        let session_id = scope_choice_id(&id, "accounts", &session.id);
        let mut reached = false;
        for _ in 0..100 {
            perform(
                &mut element,
                &mut tree,
                &node,
                &renderer,
                Box::new(operation::focusable::focus_next::<()>()),
            );
            if perform(
                &mut element,
                &mut tree,
                &node,
                &renderer,
                Box::new(operation::focusable::find_focused()),
            ) == Some(fable_id.clone())
            {
                reached = true;
                break;
            }
        }
        assert!(
            reached,
            "Tab traversal reaches the first scope in the actual Accounts pane"
        );
        let mut messages = Vec::new();
        for (key, expected) in [
            (keyboard::key::Named::ArrowDown, session_id),
            (keyboard::key::Named::ArrowUp, fable_id),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &Event::Keyboard(press(key)),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &bounds,
            );
            assert_eq!(
                perform(
                    &mut element,
                    &mut tree,
                    &node,
                    &renderer,
                    Box::new(operation::focusable::find_focused())
                ),
                Some(expected)
            );
            assert!(messages.is_empty(), "arrow focus never submits an action");
        }
        element.as_widget_mut().update(
            &mut tree,
            &Event::Keyboard(press(keyboard::key::Named::Enter)),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(messages.as_slice(), [Message::AccountUi(Action::ResetStart(account, reset, surface, choose_scope))] if account == &id && reset == &fable.id && surface == "accounts" && *choose_scope)
        );
        for _ in 0..3 {
            perform(
                &mut element,
                &mut tree,
                &node,
                &renderer,
                Box::new(operation::focusable::focus_next::<()>()),
            );
        }
        element.as_widget_mut().update(
            &mut tree,
            &Event::Keyboard(press(keyboard::key::Named::Enter)),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(messages.last(), Some(Message::AccountUi(Action::ResetCancel(account))) if account == &id),
            "Tab also reaches Cancel"
        );
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, Message::AccountUi(Action::ResetConfirm(_))))
        );
        drop(element);
        let chosen = messages.remove(0);
        drop(app.update(chosen));
        let reset = &app.account_ui.account(&id).unwrap().reset;
        assert_eq!(reset.confirmation.as_ref().unwrap().reset.id, fable.id);
        assert!(
            reset.pending.is_none(),
            "Enter selected Fable without spending"
        );
        drop(app.update(messages.pop().unwrap()));
        assert!(
            app.account_ui
                .account(&id)
                .unwrap()
                .reset
                .confirmation
                .is_none()
        );
    }
    #[test]
    fn simultaneous_account_and_popover_controls_have_distinct_ids_and_keyboard_origins() {
        use iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Operation, Tree, operation::Focusable},
        };
        use iced::{Event, Rectangle, Size, mouse};
        #[derive(Default)]
        struct Focuses(Vec<(widget::Id, bool)>);
        impl Operation for Focuses {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                _: Rectangle,
                state: &mut dyn Focusable,
            ) {
                if let Some(id) = id {
                    self.0.push((id.clone(), state.is_focused()));
                }
            }
        }
        let now = now_ms();
        let id = Account::system(Provider::Claude).id();
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
        snapshot.windows = vec![
            window(account::WindowKind::Session, "session", 1.0),
            window(account::WindowKind::Scoped, "weekly_scoped:Fable", 1.0),
        ];
        let mut fable = reset(ResetScope::Model("Fable".into()));
        fable.id = "fable-fixture".into();
        let mut session = reset(ResetScope::AccountSession);
        session.id = "session-fixture".into();
        snapshot.resets = vec![fable.clone(), session.clone()];
        let mut state = State::default();
        state.prepare(fable.clone(), 2, None);
        let controls = Controls {
            id: id.clone(),
            name: "Fixture".into(),
            snapshot,
            state,
            now,
            enabled: true,
            recovery_enabled: true,
            checking: false,
            limit_context: false,
            surface: "accounts".into(),
            identity: None,
        };
        let mut element: Element<'_, Message> = widget::row::with_children(vec![
            controls.clone().on_surface("accounts").view(),
            controls.on_surface("popover").view(),
        ])
        .spacing(space::M)
        .into();
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let bounds = Rectangle::with_size(Size::new(800.0, 900.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        let mut focuses = Focuses::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut focuses);
        let mut unique = std::collections::HashSet::new();
        for (id, _) in &focuses.0 {
            assert!(
                unique.insert(id.clone()),
                "each mounted control has one stable ID"
            );
        }
        assert_eq!(
            unique.len(),
            8,
            "two scope options plus confirm/cancel on each surface"
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut iced::advanced::widget::operation::focusable::focus::<()>(scope_choice_id(
                &id, "popover", &fable.id,
            )),
        );
        let mut messages = Vec::new();
        for key in [keyboard::key::Named::ArrowDown, keyboard::key::Named::Enter] {
            let event = Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(key),
                modified_key: keyboard::Key::Named(key),
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            });
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &bounds,
            );
        }
        assert!(
            matches!(messages.as_slice(), [Message::AccountUi(Action::ResetStart(account, reset, surface, choose_scope))] if account == &id && reset == &session.id && surface == "popover" && *choose_scope)
        );
        let mut focused = Focuses::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut focused);
        assert_eq!(
            focused
                .0
                .into_iter()
                .filter(|(_, focused)| *focused)
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            [scope_choice_id(&id, "popover", &session.id)]
        );
    }

    #[test]
    fn limit_card_pending_and_feedback_only_describe_the_reset_that_covers_it() {
        let now = now_ms();
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
        snapshot.windows = vec![
            window(account::WindowKind::Session, "session", 1.0),
            window(account::WindowKind::Scoped, "weekly_scoped:Fable", 1.0),
        ];
        let mut session = reset(ResetScope::AccountSession);
        session.id = "session-fixture".into();
        let mut fable = reset(ResetScope::Model("Fable".into()));
        fable.id = "fable-fixture".into();
        snapshot.resets = vec![session.clone(), fable.clone()];
        for (grant, related, unrelated) in [
            (
                session.clone(),
                "Claude session limit reached",
                "Claude Fable limit reached",
            ),
            (
                fable.clone(),
                "Claude Fable limit reached",
                "Claude session limit reached",
            ),
        ] {
            let mut state = State::default();
            state.prepare(grant.clone(), 2, None);
            let attempt = state.submit(&grant, 2, None).unwrap();
            let mut controls = Controls {
                id: Account::system(Provider::Claude).id(),
                name: "Fixture".into(),
                snapshot: snapshot.clone(),
                state,
                now,
                enabled: false,
                recovery_enabled: false,
                checking: false,
                limit_context: false,
                surface: "accounts".into(),
                identity: None,
            };
            assert!(controls.for_limit(related).unwrap().state.pending.is_some());
            let other = controls.for_limit(unrelated).unwrap();
            assert!(other.state.pending.is_none());
            assert!(
                !other.enabled,
                "hiding an unrelated pending label does not release the account-wide submit guard"
            );
            controls.state.finish(
                &attempt.key,
                ConsumeStatus::Reset,
                "Reset used. Usage refreshed.".into(),
            );
            controls.enabled = true;
            assert!(
                controls
                    .for_limit(related)
                    .unwrap()
                    .state
                    .feedback
                    .is_some()
            );
            let other = controls.for_limit(unrelated).unwrap();
            assert!(
                other.state.feedback.is_none(),
                "the other limit must not appear to have been reset"
            );
            assert_eq!(other.snapshot.windows[0].used, 1.0);
            assert_eq!(other.snapshot.resets[0].available_count, 2);
        }
    }
    #[test]
    fn enter_starting_a_narrowed_limit_card_focuses_its_rendered_cancel_control() {
        use iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Operation, Tree, operation, operation::Focusable},
        };
        use iced::{Event, Rectangle, Size, futures::StreamExt, mouse};
        #[derive(Default)]
        struct Focused(Option<widget::Id>);
        impl Operation for Focused {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                _: Rectangle,
                state: &mut dyn Focusable,
            ) {
                if state.is_focused() {
                    self.0 = id.cloned();
                }
            }
        }
        let now = now_ms();
        let id = Account::system(Provider::Claude).id();
        let surface = "chat/fixture-card/fixture-incident";
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
        snapshot.windows = vec![
            window(account::WindowKind::Session, "session", 0.82),
            window(account::WindowKind::Scoped, "weekly_scoped:Fable", 1.0),
        ];
        let mut session = reset(ResetScope::AccountSession);
        session.id = "session-fixture".into();
        let mut fable = reset(ResetScope::Model("Fable".into()));
        fable.id = "fable-fixture".into();
        snapshot.resets = vec![session, fable.clone()];
        let mut app = context_inspector_test_app(false);
        let account = app.account_ui.account_mut(&id).unwrap();
        account.present = true;
        account.snapshot = Some(snapshot);
        let controls = app
            .usage_reset_controls(app.account_ui.account(&id).unwrap())
            .unwrap();
        assert_eq!(
            scope_choices(&controls.snapshot, &controls.state, now).len(),
            2
        );
        let card = controls
            .for_limit("Claude Fable limit reached")
            .unwrap()
            .on_surface(surface);
        assert_eq!(scope_choices(&card.snapshot, &card.state, now).len(), 1);
        let renderer = crate::app::tests::headless_renderer();
        let bounds = Rectangle::with_size(Size::new(400.0, 900.0));
        let mut element = card.view();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(reset_control_id(&id, surface, "start")),
        );
        let enter = Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &enter,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(messages.as_slice(), [Message::AccountUi(Action::ResetStart(account, reset, origin, choose_scope))] if account == &id && reset == &fable.id && origin == surface && !*choose_scope)
        );
        let task = app.update(messages.pop().unwrap());
        assert!(app.account_ui.account(&id).unwrap().reset.pending.is_none());
        let card = app
            .usage_reset_controls(app.account_ui.account(&id).unwrap())
            .unwrap()
            .for_limit("Claude Fable limit reached")
            .unwrap()
            .on_surface(surface);
        let mut confirmation = card.view();
        let mut tree = Tree::new(confirmation.as_widget());
        let node = confirmation.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        executor.block_on(async {
            let mut stream = iced::runtime::task::into_stream(task)
                .expect("opening confirmation focuses its own control");
            while let Ok(Some(action)) =
                tokio::time::timeout(Duration::from_millis(250), stream.next()).await
            {
                if let iced::runtime::Action::Widget(mut operation) = action {
                    loop {
                        confirmation.as_widget_mut().operate(
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                            operation.as_mut(),
                        );
                        match operation.finish() {
                            operation::Outcome::Chain(next) => operation = next,
                            _ => break,
                        }
                    }
                }
            }
        });
        let mut focused = Focused::default();
        confirmation.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut focused,
        );
        assert_eq!(
            focused.0,
            Some(reset_control_id(&id, surface, "cancel")),
            "a single-scope card cannot focus the account's unmounted scope selector"
        );
        confirmation.as_widget_mut().update(
            &mut tree,
            &enter,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(messages.as_slice(), [Message::AccountUi(Action::ResetCancel(account))] if account == &id)
        );
        assert!(
            app.account_ui.account(&id).unwrap().reset.pending.is_none(),
            "neither Enter started a consume"
        );
    }
    #[test]
    fn confirmed_refreshed_limits_clear_only_the_named_scope_and_new_limits_restore_normal_state() {
        let now = now_ms();
        for (scope, summary, unrelated) in [
            (
                ResetScope::Model("Fable".into()),
                "Claude Fable limit reached",
                "Claude weekly limit reached",
            ),
            (
                ResetScope::AccountWeekly,
                "Claude weekly limit reached",
                "Claude Fable limit reached",
            ),
        ] {
            let mut app = context_inspector_test_app(false);
            let id = Account::system(Provider::Claude).id();
            let grant = reset(scope);
            let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
            snapshot.windows = vec![
                window(account::WindowKind::Session, "session", 1.0),
                window(account::WindowKind::Weekly, "weekly_all", 1.0),
                window(account::WindowKind::Scoped, "weekly_scoped:Fable", 1.0),
            ];
            snapshot.resets = vec![grant.clone()];
            let state = app.account_ui.account_mut(&id).unwrap();
            state.present = true;
            state.snapshot = Some(snapshot.clone());
            state.reset.prepare(grant.clone(), 2, None);
            let attempt = state.reset.submit(&grant, 2, None).unwrap();
            let owner = state.owner;
            for window in &mut snapshot.windows {
                if grant.clears(window) {
                    window.used = 0.0;
                    window.binding = false;
                }
            }
            snapshot.resets[0].available_count = 0;
            drop(app.finish_usage_reset(
                id.clone(),
                owner,
                attempt.key,
                Ok(ConsumeOutcome {
                    status: ConsumeStatus::Reset,
                    message: None,
                    snapshot: Some(snapshot),
                }),
            ));
            let controls = app
                .usage_reset_controls(app.account_ui.account(&id).unwrap())
                .unwrap();
            assert!(controls.limit_cleared(summary));
            assert!(!controls.limit_cleared(unrelated));
            let mut blocked = controls.clone();
            blocked.snapshot.ordinary_usage_allowed = Some(false);
            assert!(
                !blocked.limit_cleared(summary),
                "explicit provider denial overrides low windows"
            );
            blocked.snapshot.ordinary_usage_allowed = Some(true);
            assert!(blocked.limit_cleared(summary));
            assert!(
                !controls.limit_cleared("Claude usage limit reached"),
                "an unrelated exhausted account window still binds"
            );
            let resolved = controls.for_limit(summary).unwrap();
            assert!(
                resolved.snapshot.resets.is_empty(),
                "last reset count0 does not discard the matching result"
            );
            assert!(
                resolved
                    .state
                    .feedback
                    .as_ref()
                    .unwrap()
                    .contains("Reset used")
            );
            let mut stale = controls.clone();
            stale.now += schedule::STALE_AFTER + 1;
            assert!(!stale.limit_cleared(summary));
            let mut unavailable = controls.clone();
            unavailable.snapshot.status = UsageStatus::Unavailable;
            assert!(!unavailable.limit_cleared(summary));
            let mut changed_login = controls.clone();
            changed_login.identity = Some(Identity {
                organization: Some("another-fixture-login".into()),
                ..Identity::default()
            });
            assert!(!changed_login.limit_cleared(summary));
            let mut missing = controls.clone();
            missing.snapshot.windows.clear();
            assert!(!missing.limit_cleared(summary));
            let mut later = controls;
            for window in &mut later.snapshot.windows {
                if grant.clears(window) {
                    window.used = 1.0;
                }
            }
            assert!(
                !later.limit_cleared(summary),
                "later exhaustion restores the current limit card"
            );
        }
    }

    #[test]
    fn no_credit_unconfirmed_and_unrefreshed_results_cannot_mark_a_limit_cleared() {
        let now = now_ms();
        for (status, refreshed) in [
            (ConsumeStatus::NoCredit, true),
            (ConsumeStatus::Unconfirmed, true),
            (ConsumeStatus::Unavailable, true),
            (ConsumeStatus::Reset, false),
        ] {
            let mut app = context_inspector_test_app(false);
            let id = Account::system(Provider::Claude).id();
            let grant = reset(ResetScope::Model("Fable".into()));
            let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "", now);
            snapshot.windows = vec![
                window(account::WindowKind::Scoped, "weekly_scoped:Fable", 0.0),
                window(account::WindowKind::Weekly, "weekly_all", 0.0),
            ];
            snapshot.resets = vec![grant.clone()];
            let state = app.account_ui.account_mut(&id).unwrap();
            state.present = true;
            state.snapshot = Some(snapshot.clone());
            state.reset.prepare(grant.clone(), 2, None);
            let attempt = state.reset.submit(&grant, 2, None).unwrap();
            let owner = state.owner;
            drop(app.finish_usage_reset(
                id.clone(),
                owner,
                attempt.key,
                Ok(ConsumeOutcome {
                    status,
                    message: None,
                    snapshot: refreshed.then_some(snapshot),
                }),
            ));
            let controls = app
                .usage_reset_controls(app.account_ui.account(&id).unwrap())
                .unwrap();
            assert!(
                !controls.limit_cleared("Claude Fable limit reached"),
                "{status:?}, refreshed={refreshed}"
            );
            assert!(!controls.limit_cleared("Claude weekly limit reached"));
        }
    }
    #[test]
    fn a_new_same_scope_incident_inherits_neither_cleared_proof_nor_success_feedback() {
        let now = now_ms();
        let grant = reset(ResetScope::Model("Fable".into()));
        let id = Account::system(Provider::Claude).id();
        let old = LimitIncidents(BTreeMap::from([(
            "card/1/old".into(),
            BTreeSet::from([1, 2]),
        )]));
        let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "", now);
        snapshot.windows = vec![window(
            account::WindowKind::Scoped,
            "weekly_scoped:Fable",
            0.0,
        )];
        let mut state = State::default();
        state.feedback = Some("Reset used. Usage refreshed.".into());
        state.feedback_reset = Some(grant.clone());
        state.feedback_status = Some(ConsumeStatus::Reset);
        state.confirmed.insert(
            grant.id.clone(),
            ConfirmedReset {
                reset: grant,
                identity: None,
                observed_at: now,
                incidents: old.clone(),
            },
        );
        let controls = Controls {
            id,
            name: "Fixture".into(),
            snapshot,
            state,
            now,
            enabled: true,
            recovery_enabled: true,
            checking: false,
            limit_context: false,
            surface: "accounts".into(),
            identity: None,
        };
        let previous = controls.for_incident(&old);
        assert!(previous.limit_cleared("Claude Fable limit reached"));
        assert!(previous.state.feedback.is_some());
        let remaining_old_source =
            LimitIncidents(BTreeMap::from([("card/1/old".into(), BTreeSet::from([1]))]));
        assert!(
            controls
                .for_incident(&remaining_old_source)
                .limit_cleared("Claude Fable limit reached")
        );
        for current in [
            LimitIncidents(BTreeMap::from([("card/1/new".into(), BTreeSet::from([1]))])),
            LimitIncidents(BTreeMap::from([(
                "card/1/old".into(),
                BTreeSet::from([1, 3]),
            )])),
            LimitIncidents(BTreeMap::from([("card/2/old".into(), BTreeSet::from([1]))])),
        ] {
            let later = controls.for_incident(&current);
            assert!(!later.limit_cleared("Claude Fable limit reached"));
            assert!(
                later.state.feedback.is_none(),
                "new rejection must not show an older reset success"
            );
        }
        assert!(
            controls.state.feedback.is_some(),
            "general account view keeps its recent result"
        );
    }
    #[test]
    fn authoritative_codex_denial_prevents_a_cleared_card_despite_low_windows() {
        let now = now_ms();
        let grant = reset(ResetScope::Account);
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
        snapshot.windows = vec![
            window(account::WindowKind::Session, "codex:primary", 0.1),
            window(account::WindowKind::Weekly, "codex:secondary", 0.1),
        ];
        let mut state = State::default();
        state.confirmed.insert(
            grant.id.clone(),
            ConfirmedReset {
                reset: grant,
                identity: None,
                observed_at: now,
                incidents: LimitIncidents::default(),
            },
        );
        let mut controls = Controls {
            id: Account::system(Provider::Codex).id(),
            name: "Fixture".into(),
            snapshot,
            state,
            now,
            enabled: true,
            recovery_enabled: true,
            checking: false,
            limit_context: false,
            surface: "accounts".into(),
            identity: None,
        };
        for allowed in [None, Some(true)] {
            controls.snapshot.ordinary_usage_allowed = allowed;
            assert!(controls.limit_cleared("Codex usage limit reached"));
        }
        controls.snapshot.ordinary_usage_allowed = Some(false);
        assert!(!controls.limit_cleared("Codex usage limit reached"));
    }
    #[test]
    fn a_limit_event_forces_fresh_reset_inventory_instead_of_recent_cached_windows() {
        let now = now_ms();
        let id = Account::system(Provider::Codex).id();
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "codex".into(),
            arguments: vec![],
            working_directory: "/fixture".into(),
            environment: BTreeMap::new(),
        };
        for available in [1, 0] {
            let mut app = context_inspector_test_app(false);
            app.account_ui.registry_ready = true;
            app.refresh_ui.focused = true;
            let state = app.account_ui.account_mut(&id).unwrap();
            state.present = true;
            let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "cached", now);
            snapshot.windows = vec![window(account::WindowKind::Session, "codex:primary", 0.0)];
            state.snapshot = Some(snapshot.clone());
            state.schedule.observe(now);
            drop(app.account_limit_reported(Provider::Codex, &recipe));
            let ticket = app.account_ui.pending_probe_for_test(&id).unwrap();
            assert!(ticket.force);
            let checking = app
                .usage_reset_controls(app.account_ui.account(&id).unwrap())
                .unwrap();
            assert!(checking.checking);
            assert!(
                !checking.enabled,
                "known stale inventory cannot start a new confirmation"
            );
            assert!(
                checking
                    .for_limit("Codex usage limit reached")
                    .unwrap()
                    .checking
            );
            snapshot.status = UsageStatus::Limited;
            snapshot.ordinary_usage_allowed = Some(false);
            snapshot.windows[0].used = 1.0;
            let mut grant = reset(ResetScope::Account);
            grant.available_count = available;
            snapshot.resets = vec![grant];
            snapshot.observed_at = now + 1;
            let owner = app.account_ui.account(&id).unwrap().owner;
            drop(app.account_probed(
                id.clone(),
                owner,
                ticket,
                Outcome::Answered {
                    snapshot,
                    identity: None,
                    catalog: None,
                },
            ));
            let state = app.account_ui.account(&id).unwrap();
            assert_eq!(state.snapshot.as_ref().unwrap().windows[0].used, 1.0);
            let controls = app.usage_reset_controls(state);
            if available == 1 {
                let controls = controls.unwrap();
                assert!(next_reset(&controls.snapshot, &controls.state, controls.now).is_some());
                assert_eq!(
                    account::resets::inventory_groups(
                        &controls.snapshot.resets,
                        unix_seconds(controls.now)
                    )[0]
                    .count,
                    1
                );
                drop(app.account_limit_reported(Provider::Codex, &recipe));
                assert!(
                    app.account_ui.pending_probe_for_test(&id).is_none(),
                    "burst is coalesced until the existing tick"
                );
                let checking = app
                    .usage_reset_controls(app.account_ui.account(&id).unwrap())
                    .unwrap();
                assert!(checking.checking);
                assert!(!checking.enabled);
                assert!(
                    checking.recovery_enabled,
                    "checking does not hide same-key recovery"
                );
                let renderer = crate::app::tests::headless_renderer();
                let mut element = checking.view();
                use iced::advanced::{
                    Layout, layout,
                    widget::{Operation, Tree},
                };
                #[derive(Default)]
                struct Texts(Vec<String>);
                impl Operation for Texts {
                    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                        visit(self);
                    }
                    fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
                        self.0.push(text.into());
                    }
                }
                let mut tree = Tree::new(element.as_widget());
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(iced::Size::ZERO, iced::Size::new(320.0, 900.0)),
                );
                let mut texts = Texts::default();
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut texts,
                );
                assert!(texts.0.iter().any(|text| text == "Checking usage…"));
            } else {
                assert!(
                    controls.is_none(),
                    "fresh zero inventory hides the reset action"
                );
            }
        }
    }
    #[test]
    fn newer_partial_windows_keep_fresh_forced_inventory_and_cannot_renew_its_due_time() {
        let now = now_ms();
        let id = Account::system(Provider::Codex).id();
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "codex".into(),
            arguments: vec![],
            working_directory: "/fixture".into(),
            environment: BTreeMap::new(),
        };
        for count in [1, 0] {
            let mut app = context_inspector_test_app(false);
            let directory = tempfile::tempdir().unwrap();
            app.account_ui.cache = Some(Cache::at(directory.path()));
            app.account_ui.registry_ready = true;
            app.refresh_ui.focused = true;
            let state = app.account_ui.account_mut(&id).unwrap();
            state.present = true;
            let mut live =
                UsageSnapshot::state(UsageStatus::Limited, "newer live windows", now + 2);
            live.windows = vec![window(account::WindowKind::Session, "codex:primary", 1.0)];
            live.ordinary_usage_allowed = Some(false);
            live.resets = vec![reset(ResetScope::Account)];
            state.snapshot = Some(live.clone());
            state.schedule.observe(now + 2);
            drop(app.account_limit_reported(Provider::Codex, &recipe));
            let ticket = app.account_ui.pending_probe_for_test(&id).unwrap();
            let owner = app.account_ui.account(&id).unwrap().owner;
            let mut complete =
                UsageSnapshot::state(UsageStatus::Ok, "full response before live frame", now + 1);
            complete.windows = vec![window(account::WindowKind::Session, "codex:primary", 0.0)];
            let mut grant = reset(ResetScope::Account);
            grant.available_count = count;
            complete.resets = vec![grant];
            drop(app.account_probed(
                id.clone(),
                owner,
                ticket,
                Outcome::Answered {
                    identity: None,
                    snapshot: complete,
                    catalog: None,
                },
            ));
            let state = app.account_ui.account(&id).unwrap();
            assert_eq!(state.snapshot.as_ref().unwrap().windows, live.windows);
            assert_eq!(state.snapshot.as_ref().unwrap().observed_at, now + 2);
            assert_eq!(
                state.snapshot.as_ref().unwrap().ordinary_usage_allowed,
                Some(false)
            );
            assert_eq!(
                state.snapshot.as_ref().unwrap().resets[0].available_count,
                count
            );
            assert_eq!(state.inventory_observed_at, Some(now + 1));
            let entries = app.account_ui.take_live_entries();
            assert_eq!(
                entries.len(),
                1,
                "merged full inventory is republished through the coalesced writer"
            );
            assert_eq!(entries[0].1.snapshot.windows, live.windows);
            assert_eq!(entries[0].1.snapshot.observed_at, now + 2);
            assert_eq!(entries[0].1.snapshot.resets[0].available_count, count);
            // A consume/recovery can publish even newer full inventory while
            // another forced response is in transit. That inventory wins too.
            let state = app.account_ui.account_mut(&id).unwrap();
            state.inventory_observed_at = Some(now + 3);
            state.snapshot.as_mut().unwrap().observed_at = now + 4;
            state.snapshot.as_mut().unwrap().resets[0].available_count = 0;
            let late = state.start_probe(Trigger::Limit, now);
            let mut old = UsageSnapshot::state(UsageStatus::Ok, "older inventory", now + 1);
            old.resets = vec![reset(ResetScope::Account)];
            drop(app.account_probed(
                id.clone(),
                owner,
                late,
                Outcome::Answered {
                    identity: None,
                    snapshot: old,
                    catalog: None,
                },
            ));
            let state = app.account_ui.account(&id).unwrap();
            assert_eq!(state.inventory_observed_at, Some(now + 3));
            assert_eq!(
                state.snapshot.as_ref().unwrap().resets[0].available_count,
                0
            );
            // Continued live traffic updates quota age, not inventory age.
            let state = app.account_ui.account_mut(&id).unwrap();
            state.inventory_observed_at = Some(now - schedule::REFRESH_INTERVAL);
            state.schedule.observe(now);
            state
                .schedule
                .start(Trigger::Limit, now - schedule::LIMIT_DEBOUNCE);
            state.schedule.succeed(now);
            drop(app.probe_account(&id, Trigger::Tick, now));
            assert!(app.account_ui.pending_probe_is_forced_for_test(&id));
        }
    }
    #[test]
    fn a_complete_read_keeps_explicit_permission_when_a_newer_partial_window_omits_it() {
        let now = now_ms();
        let id = Account::system(Provider::Codex).id();
        for (partial_allowed, full_allowed, expected) in [
            (None, false, false),
            (Some(false), true, false),
            (Some(true), false, true),
        ] {
            let mut app = context_inspector_test_app(false);
            let state = app.account_ui.account_mut(&id).unwrap();
            let mut partial = UsageSnapshot::state(UsageStatus::Ok, "newer partial", now + 2);
            partial.ordinary_usage_allowed = partial_allowed;
            partial.windows = vec![window(account::WindowKind::Session, "codex:primary", 0.0)];
            state.snapshot = Some(partial);
            let owner = state.owner;
            let ticket = state.start_probe(Trigger::Limit, now);
            let mut full = UsageSnapshot::state(UsageStatus::Limited, "complete", now + 1);
            full.ordinary_usage_allowed = Some(full_allowed);
            full.plan = Some("Fixture plan".into());
            full.windows = vec![window(account::WindowKind::Session, "codex:primary", 1.0)];
            drop(app.account_probed(
                id.clone(),
                owner,
                ticket,
                Outcome::Answered {
                    identity: None,
                    snapshot: full,
                    catalog: None,
                },
            ));
            let current = app
                .account_ui
                .account(&id)
                .unwrap()
                .snapshot
                .as_ref()
                .unwrap();
            assert_eq!(current.ordinary_usage_allowed, Some(expected));
            assert_eq!(
                current.status,
                if expected {
                    UsageStatus::Ok
                } else {
                    UsageStatus::Limited
                }
            );
            assert_eq!(
                current.windows[0].used, 0.0,
                "newer partial windows stay authoritative"
            );
            assert_eq!(current.plan.as_deref(), Some("Fixture plan"));
            assert_eq!(current.observed_at, now + 2);
            if !expected {
                assert_eq!(
                    chip(app.account_ui.account(&id).unwrap(), "Fixture", now + 2).text,
                    "Limited",
                    "an account denial must not advertise 100% available overall"
                );
            }
        }
    }
    #[test]
    fn a_consume_result_merged_with_newer_partial_usage_is_published_for_other_windows() {
        let now = now_ms();
        let mut app = context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        app.account_ui.cache = Some(Cache::at(directory.path()));
        let id = Account::system(Provider::Codex).id();
        let grant = reset(ResetScope::Account);
        let state = app.account_ui.account_mut(&id).unwrap();
        state.present = true;
        let mut partial = UsageSnapshot::state(UsageStatus::Limited, "newer live usage", now + 2);
        partial.windows = vec![window(account::WindowKind::Session, "codex:primary", 1.0)];
        partial.resets = vec![grant.clone()];
        state.snapshot = Some(partial.clone());
        state.reset.prepare(grant.clone(), 2, None);
        let attempt = state.reset.submit(&grant, 2, None).unwrap();
        let owner = state.owner;
        let mut complete = UsageSnapshot::state(
            UsageStatus::Ok,
            "consume response before live frame",
            now + 1,
        );
        let mut remaining = grant;
        remaining.available_count = 1;
        complete.resets = vec![remaining];
        drop(app.finish_usage_reset(
            id,
            owner,
            attempt.key,
            Ok(ConsumeOutcome {
                status: ConsumeStatus::Reset,
                message: None,
                snapshot: Some(complete),
            }),
        ));
        let entries = app.account_ui.take_live_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1.snapshot.windows, partial.windows);
        assert_eq!(entries[0].1.snapshot.observed_at, now + 2);
        assert_eq!(entries[0].1.snapshot.resets[0].available_count, 1);
    }
    #[test]
    fn account_buttons_name_the_selected_grant_while_limit_buttons_keep_the_generic_action() {
        use iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Operation, Tree, operation},
        };
        use iced::{Event, Rectangle, Size, mouse};
        #[derive(Default)]
        struct Texts(Vec<String>);
        impl Operation for Texts {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, _: Rectangle, text: &str) {
                self.0.push(text.into());
            }
        }
        let now = now_ms();
        let mut combined = reset(ResetScope::AccountSession);
        combined.scopes.push(ResetScope::AccountWeekly);
        for (grant, window, summary, account_label) in [
            (
                reset(ResetScope::Model("Fable".into())),
                window(account::WindowKind::Scoped, "weekly_scoped:Fable", 1.0),
                "Claude Fable limit reached",
                "Use Fable reset",
            ),
            (
                combined,
                window(account::WindowKind::Weekly, "weekly_all", 1.0),
                "Claude weekly limit reached",
                "Use Session + Weekly reset",
            ),
        ] {
            let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
            snapshot.windows.push(window);
            snapshot.resets.push(grant.clone());
            let controls = Controls {
                id: Account::system(Provider::Claude).id(),
                name: "Fixture".into(),
                snapshot,
                state: State::default(),
                now,
                enabled: true,
                recovery_enabled: true,
                checking: false,
                limit_context: false,
                surface: "accounts".into(),
                identity: None,
            };
            // Surface identifiers affect focus routing, never the action copy.
            for (controls, expected) in [
                (controls.clone().on_surface("accounts"), account_label),
                (controls.clone().on_surface("popover"), account_label),
                (controls.clone().on_surface("chat/arbitrary"), account_label),
                (
                    controls.for_limit(summary).unwrap().on_surface("accounts"),
                    "Use a reset",
                ),
            ] {
                let renderer = crate::app::tests::headless_renderer();
                let start = reset_control_id(&controls.id, &controls.surface, "start");
                let expected_account = controls.id.clone();
                let expected_surface = controls.surface.clone();
                let mut element = controls.view();
                let mut tree = Tree::new(element.as_widget());
                let bounds = Rectangle::with_size(Size::new(420.0, 900.0));
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, bounds.size()),
                );
                let mut texts = Texts::default();
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut texts,
                );
                let actions: Vec<_> = texts
                    .0
                    .iter()
                    .map(String::as_str)
                    .filter(|text| text.starts_with("Use "))
                    .collect();
                assert_eq!(
                    actions,
                    [expected],
                    "the rendered primary button names its context"
                );
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut operation::focusable::focus::<()>(start),
                );
                let key = keyboard::Key::Named(keyboard::key::Named::Enter);
                let mut messages = Vec::new();
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Keyboard(keyboard::Event::KeyPressed {
                        key: key.clone(),
                        modified_key: key,
                        physical_key: keyboard::key::Physical::Unidentified(
                            keyboard::key::NativeCode::Unidentified,
                        ),
                        location: keyboard::Location::Standard,
                        modifiers: keyboard::Modifiers::empty(),
                        text: None,
                        repeat: false,
                    }),
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &bounds,
                );
                assert!(
                    matches!(messages.as_slice(), [Message::AccountUi(Action::ResetStart(account, reset_id, surface, false))]
                    if account == &expected_account && reset_id == &grant.id && surface == &expected_surface)
                );
            }
        }
    }

    #[test]
    fn held_inventory_stays_scope_first_without_an_available_or_spend_action() {
        let now = now_ms();
        let mut held = reset(ResetScope::AccountSession);
        held.scopes.push(ResetScope::AccountWeekly);
        held.available_count = 1;
        held.usable = false;
        held.expires_at = Some(unix_seconds(now) + 86_400);
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
        snapshot
            .windows
            .push(window(account::WindowKind::Weekly, "weekly_all", 1.0));
        snapshot.resets.push(held.clone());
        let lines = account::resets::inventory_lines(&snapshot.resets, unix_seconds(now));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("1 Session + Weekly reset · not usable now · expires "));
        assert!(!lines[0].contains("available"));
        assert_eq!(scope_count(&snapshot, &held, now), 0);
        assert!(next_reset(&snapshot, &State::default(), now).is_none());
    }

    #[test]
    fn reset_copy_counts_only_usable_grants_and_explains_the_effect_on_every_surface() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree},
        };
        #[derive(Default)]
        struct Texts(Vec<String>);
        impl Operation for Texts {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
                self.0.push(text.into());
            }
        }
        let now = now_ms();
        let grant = reset(ResetScope::Model("Fable".into()));
        let mut paused = grant.clone();
        paused.id = "not-next".into();
        paused.usable = false;
        paused.available_count = 9;
        let mut combined = reset(ResetScope::AccountSession);
        combined.id = "combined".into();
        combined.scopes.push(ResetScope::AccountWeekly);
        combined.available_count = 1;
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
        snapshot.windows = vec![
            window(account::WindowKind::Scoped, "weekly_scoped:Fable", 1.0),
            window(account::WindowKind::Weekly, "weekly_all", 1.0),
        ];
        let mut held_combined = combined.clone();
        held_combined.id = "held-combined".into();
        held_combined.usable = false;
        snapshot.resets = vec![grant.clone(), paused, combined, held_combined];
        assert_eq!(scope_count(&snapshot, &grant, now), 2);
        let groups = account::resets::inventory_groups(&snapshot.resets, unix_seconds(now));
        assert!(
            groups
                .iter()
                .any(|group| group.usable_now && group.count == 2)
        );
        assert!(
            groups
                .iter()
                .any(|group| !group.usable_now && group.count == 9)
        );
        let mut state = State::default();
        state.prepare(grant, 2, None);
        let controls = Controls {
            id: Account::system(Provider::Claude).id(),
            name: "fixture@example.test".into(),
            snapshot,
            state,
            now,
            enabled: true,
            recovery_enabled: true,
            checking: false,
            limit_context: false,
            surface: "accounts".into(),
            identity: None,
        };
        let renderer = crate::app::tests::headless_renderer();
        for surface in ["accounts", "popover", "chat/fixture/card"] {
            let mut element = controls.clone().on_surface(surface).view();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(420.0, 900.0)),
            );
            let mut texts = Texts::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
            for expected in [
                "2 Fable resets",
                "9 Fable resets · not usable now",
                "1 Session + Weekly reset",
                "1 Session + Weekly reset · not usable now",
                "Use a Fable reset for fixture@example.test?",
                "This clears the Fable weekly limit now. 2 available; this can’t be undone.",
            ] {
                assert!(
                    texts.0.iter().any(|text| text == expected),
                    "{surface}: missing {expected:?}"
                );
            }
        }
    }

    #[test]
    fn actionable_limit_guidance_offers_only_a_reset_covering_the_current_limit() {
        let now = now_ms();
        let fable = reset(ResetScope::Model("Fable".into()));
        let mut weekly = reset(ResetScope::AccountWeekly);
        weekly.id = "weekly".into();
        let mut snapshot = UsageSnapshot::state(UsageStatus::Limited, "", now);
        snapshot.windows = vec![
            window(account::WindowKind::Scoped, "weekly_scoped:Fable", 1.0),
            window(account::WindowKind::Weekly, "weekly_all", 0.96),
        ];
        snapshot.resets = vec![fable, weekly];
        let mut controls = Controls {
            id: Account::system(Provider::Claude).id(),
            name: "Fixture".into(),
            snapshot,
            state: State::default(),
            now,
            enabled: true,
            recovery_enabled: true,
            checking: false,
            limit_context: false,
            surface: "accounts".into(),
            identity: None,
        };
        let summary = "Claude Fable limit reached · resets Jan 1, 12pm (UTC)";
        let fallback = "Choose another model or account, or wait for the reported reset.";
        assert_eq!(
            controls.limit_guidance(summary, fallback),
            "Use a reset now, choose another model or account, or wait until Jan 1, 12pm (UTC)."
        );
        controls.snapshot.windows[1].used = 1.0;
        assert_eq!(
            controls.limit_guidance(summary, fallback),
            "Use a reset now, choose another account, or wait until Jan 1, 12pm (UTC).",
            "resetting Fable does not make another model bypass exhausted account quota"
        );
        controls.snapshot.windows[1].used = 0.96;
        for (provider, scope, kind, key, account_summary) in [
            (
                Provider::Claude,
                ResetScope::AccountWeekly,
                account::WindowKind::Weekly,
                "weekly_all",
                "Claude weekly limit reached · resets Jan 1, 12pm (UTC)",
            ),
            (
                Provider::Claude,
                ResetScope::AccountSession,
                account::WindowKind::Session,
                "session",
                "Claude session limit reached · resets Jan 1, 12pm (UTC)",
            ),
            (
                Provider::Codex,
                ResetScope::Account,
                account::WindowKind::Session,
                "codex:primary",
                "Codex usage limit reached · resets Jan 1, 12pm (UTC)",
            ),
        ] {
            let mut account_controls = controls.clone();
            account_controls.id = Account::system(provider).id();
            account_controls.snapshot.windows = vec![window(kind, key, 1.0)];
            account_controls.snapshot.resets = vec![reset(scope)];
            assert_eq!(
                account_controls.limit_guidance(
                    account_summary,
                    "Wait for the reported reset or choose another account."
                ),
                "Use a reset now, choose another account, or wait until Jan 1, 12pm (UTC).",
                "a model switch does not bypass {account_summary}"
            );
        }
        controls.snapshot.resets[0].usable = false;
        assert_eq!(
            controls.limit_guidance(summary, fallback),
            fallback,
            "an account grant cannot clear a model limit"
        );
        assert!(
            controls
                .limit_guidance("Claude weekly limit reached", fallback)
                .starts_with("Use a reset now,")
        );
        controls.snapshot.resets[1].usable = false;
        controls.snapshot.resets[0].usable = true;
        assert_eq!(
            controls.limit_guidance("Claude weekly limit reached", fallback),
            fallback,
            "a model grant cannot clear the account window"
        );
        let original = controls.snapshot.resets[0].clone();
        controls.state.prepare(original.clone(), 2, None);
        let attempt = controls.state.submit(&original, 2, None).unwrap();
        controls.state.finish(
            &attempt.key,
            ConsumeStatus::Unconfirmed,
            "Result unknown".into(),
        );
        assert_eq!(
            controls.limit_guidance(summary, fallback),
            fallback,
            "an uncertain original request offers recovery, not a new reset"
        );
        controls.state = State::default();
        controls.enabled = false;
        assert_eq!(
            controls.limit_guidance(summary, fallback),
            fallback,
            "pending or stale state keeps existing guidance"
        );
    }
}
