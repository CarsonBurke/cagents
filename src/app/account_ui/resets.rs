//! Explicit, account-bound reset confirmation shared by usage and chat cards.
use super::*;
use account::resets::{ConsumeOutcome, ConsumeRequest, ConsumeStatus, UsageReset};

#[derive(Debug, Clone, Default)]
pub(super) struct State {
    pub confirmation: Option<Confirmation>,
    pending: Option<Attempt>,
    uncertain: BTreeMap<String, Attempt>,
    feedback: Option<String>,
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
        }
    }

    fn prepare(&mut self, reset: UsageReset, total_count: u64, identity: Option<Identity>) {
        if self.pending.is_some()
            || (reset.kind == account::resets::ResetKind::WeeklySession
                && self.uncertain.contains_key(&reset.id))
        {
            return;
        }
        let key = self
            .uncertain
            .get(&reset.id)
            .map(|attempt| attempt.key.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
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
            self.feedback = Some("Resets changed. Review the current count and try again.".into());
            return None;
        }
        let attempt = Attempt {
            reset: confirmation.reset,
            key: confirmation.key,
            total_count: confirmation.total_count,
            identity: confirmation.identity,
        };
        self.pending = Some(attempt.clone());
        self.feedback = None;
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

struct InventoryGroup {
    scopes: Vec<account::resets::ResetScope>,
    label: String,
    count: u64,
    next_expiry: Option<i64>,
    unknown_expiry: bool,
}

fn normalized_scopes(reset: &UsageReset) -> Vec<account::resets::ResetScope> {
    let mut scopes = reset.scopes.clone();
    scopes.sort_by_key(account::resets::ResetScope::label);
    scopes.dedup();
    scopes
}

fn grouped_inventory(snapshot: &UsageSnapshot, now: Millis) -> Vec<InventoryGroup> {
    let mut groups: Vec<InventoryGroup> = Vec::new();
    for reset in snapshot.resets.iter().filter(|reset| inventory(reset, now)) {
        let scopes = normalized_scopes(reset);
        if let Some(group) = groups.iter_mut().find(|group| group.scopes == scopes) {
            group.count = group.count.saturating_add(reset.available_count);
            group.next_expiry = group.next_expiry.into_iter().chain(reset.expires_at).min();
            group.unknown_expiry |= reset.expires_at.is_none();
        } else {
            groups.push(InventoryGroup {
                label: if scopes.is_empty() {
                    "Unknown scope".into()
                } else {
                    scopes
                        .iter()
                        .map(account::resets::ResetScope::label)
                        .collect::<Vec<_>>()
                        .join(" + ")
                },
                scopes,
                count: reset.available_count,
                next_expiry: reset.expires_at,
                unknown_expiry: reset.expires_at.is_none(),
            });
        }
    }
    groups.sort_by(|left, right| left.label.cmp(&right.label));
    groups
}

fn scope_count(snapshot: &UsageSnapshot, reset: &UsageReset, now: Millis) -> u64 {
    let scopes = normalized_scopes(reset);
    grouped_inventory(snapshot, now)
        .into_iter()
        .find(|group| group.scopes == scopes)
        .map_or(0, |group| group.count)
}

fn next_reset<'a>(
    snapshot: &'a UsageSnapshot,
    state: &State,
    now: Millis,
) -> Option<&'a UsageReset> {
    snapshot
        .resets
        .iter()
        .filter(|reset| {
            actionable(snapshot, reset, now)
                && !(reset.kind == account::resets::ResetKind::WeeklySession
                    && state.uncertain.contains_key(&reset.id))
        })
        .min_by(|left, right| {
            left.expires_at
                .unwrap_or(i64::MAX)
                .cmp(&right.expires_at.unwrap_or(i64::MAX))
                .then_with(|| left.id.cmp(&right.id))
        })
}

impl Controls {
    pub(in crate::app) fn for_limit(&self, summary: &str) -> Option<Self> {
        let mut controls = self.clone();
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
        (!controls.snapshot.resets.is_empty() || !controls.state.uncertain.is_empty())
            .then_some(controls)
    }

    pub(in crate::app) fn view(self) -> Element<'static, Message> {
        let mut rows: Vec<Element<'static, Message>> = Vec::new();
        for group in grouped_inventory(&self.snapshot, self.now) {
            let expiry = group
                .next_expiry
                .map(|at| {
                    format!(
                        " · {} {}",
                        if group.unknown_expiry {
                            "next known expiry"
                        } else {
                            "next expires"
                        },
                        account::format_local_moment(at, unix_seconds(self.now))
                    )
                })
                .unwrap_or_default();
            rows.push(
                design::caption(format!(
                    "{}: {} reset{} left{expiry}",
                    group.label,
                    group.count,
                    if group.count == 1 { "" } else { "s" },
                ))
                .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                .into(),
            );
        }
        if let Some(pending) = &self.state.pending {
            rows.push(design::caption(format!("Using a {} reset…", pending.reset.label())).into());
        } else if let Some(confirmation) = &self.state.confirmation {
            rows.push(
                design::body(format!("Use a reset for {}?", self.name))
                    .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                    .into(),
            );
            rows.push(
                design::caption(format!(
                    "{} · {} left. This cannot be undone.",
                    confirmation.reset.label(),
                    confirmation.total_count,
                ))
                .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                .into(),
            );
            rows.push(
                widget::row::with_children(vec![
                    crate::ui_controls::button::primary("Use reset")
                        .on_press_maybe(
                            self.enabled
                                .then(|| Message::AccountUi(Action::ResetConfirm(self.id.clone()))),
                        )
                        .into(),
                    crate::ui_controls::button::secondary("Cancel")
                        .on_press(Message::AccountUi(Action::ResetCancel(self.id.clone())))
                        .into(),
                ])
                .spacing(space::S)
                .into(),
            );
        } else if let Some(reset) = next_reset(&self.snapshot, &self.state, self.now) {
            let label = if self.state.uncertain.is_empty() {
                "Use a reset".into()
            } else {
                format!("Use a {} reset", reset.label())
            };
            rows.push(
                crate::ui_controls::button::secondary(label)
                    .name(format!("Use a {} reset for {}", reset.label(), self.name))
                    .on_press_maybe(self.enabled.then(|| {
                        Message::AccountUi(Action::ResetStart(self.id.clone(), reset.id.clone()))
                    }))
                    .into(),
            );
        }
        if self.state.pending.is_none() {
            for attempt in self.state.uncertain.values() {
                let recurring = attempt.reset.kind == account::resets::ResetKind::WeeklySession;
                rows.push(
                    design::caption(format!(
                        "{} for {} · {} left when confirmed.",
                        attempt.reset.label(),
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
        {
            return None;
        }
        Some(Controls {
            id: state.id.clone(),
            name: self.account_ui.display_name(state),
            snapshot: snapshot.clone(),
            state: reset_state,
            now: self.account_ui.now_ms,
            recovery_enabled: state.present
                && self.account_ui.login_conflict(state).is_none()
                && !self.account_sign_in_running(&state.account),
            enabled: state.present
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

    pub(super) fn start_usage_reset(&mut self, id: AccountId, reset_id: String) -> Task<Message> {
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
            actionable(&controls.snapshot, reset, now_ms()).then(|| {
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
            state.reset.prepare(reset, count, identity);
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
                state.reset.confirmation = None;
                state.reset.feedback =
                    Some("Reset availability changed. Check usage and try again.".into());
            }
            return Task::none();
        };
        let state = self.account_ui.account_mut(&id).expect("account checked");
        let Some(attempt) = state.reset.submit(&reset, count, state.identity.as_ref()) else {
            return Task::none();
        };
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
        let (status, message, snapshot) = match result {
            Ok(outcome) => (
                outcome.status,
                outcome.message.unwrap_or_else(|| {
                    match outcome.status {
                        ConsumeStatus::Reset => "Reset used. Usage refreshed.",
                        ConsumeStatus::NoCredit => {
                            "This reset is no longer available. Usage refreshed."
                        }
                        ConsumeStatus::NotLimited => "No reset was needed. Usage refreshed.",
                        ConsumeStatus::Unconfirmed => {
                            "Reset could not be confirmed. Check usage before trying again."
                        }
                        ConsumeStatus::Unavailable => {
                            "Reset unavailable. Check usage and try again."
                        }
                    }
                    .into()
                }),
                outcome.snapshot,
            ),
            Err(_) => (
                ConsumeStatus::Unconfirmed,
                "Reset could not be confirmed. Checking usage before another attempt.".into(),
                None,
            ),
        };
        if !state.reset.finish(&key, status, message) {
            return Task::none();
        }
        if let Some(snapshot) = snapshot {
            state.schedule.observe(snapshot.observed_at);
            if state
                .snapshot
                .as_ref()
                .is_none_or(|current| current.observed_at <= snapshot.observed_at)
            {
                state.snapshot = Some(snapshot);
            }
            state.failure = None;
            self.account_ui.now_ms = now_ms();
            Task::none()
        } else {
            state.schedule.invalidate();
            self.update_account_ui(Action::Refresh(id))
        }
    }
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
    fn uncertainty_reuses_the_original_idempotency_key_after_another_confirmation() {
        let mut state = State::default();
        let grant = reset(ResetScope::AccountWeekly);
        state.prepare(grant.clone(), 2, None);
        let first = state.submit(&grant, 2, None).unwrap();
        assert!(state.finish(&first.key, ConsumeStatus::Unconfirmed, "Check usage".into()));
        state.prepare(grant.clone(), 2, None);
        assert_eq!(state.confirmation.as_ref().unwrap().key, first.key);
        let second = state.submit(&grant, 2, None).unwrap();
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
            .filter(|text| text.starts_with("Account limits:"))
            .collect();
        assert_eq!(inventory.len(), 1);
        assert!(inventory[0].contains("3 resets left · next expires"));
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
        let groups = grouped_inventory(&snapshot, now);
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
        assert_eq!(state.confirmation.as_ref().unwrap().key, original.key);
        let next_retry = state.recover(&grant.id, None).unwrap();
        assert_eq!(next_retry.key, original.key);
    }
}
