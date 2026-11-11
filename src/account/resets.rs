//! Reset inventory and explicit reset attempts, shared by the CLI and UI.
use serde::{Deserialize, Serialize};

use super::{AccountId, UsageSnapshot, UsageWindow, WindowKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetKind {
    Granted,
    WeeklySession,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetScope {
    AccountSession,
    AccountWeekly,
    Model(String),
    /// The provider resets its eligible account windows as one operation.
    Account,
}

impl ResetScope {
    pub fn label(&self) -> String {
        match self {
            Self::AccountSession => "Session".into(),
            Self::AccountWeekly => "Weekly".into(),
            Self::Model(model) => model.clone(),
            Self::Account => "Account limits".into(),
        }
    }

    pub fn clears(&self, window: &UsageWindow) -> bool {
        match self {
            Self::AccountSession => {
                window.kind == WindowKind::Session && !window.key.starts_with("weekly_scoped:")
            }
            Self::AccountWeekly => window.kind == WindowKind::Weekly,
            Self::Model(model) => {
                window.kind == WindowKind::Scoped
                    && super::claude::window_scope(window)
                        .or_else(|| super::codex::window_scope(window))
                        .is_some_and(|scope| {
                            scope.eq_ignore_ascii_case(model)
                                || scope
                                    .split_whitespace()
                                    .next()
                                    .is_some_and(|family| family.eq_ignore_ascii_case(model))
                        })
            }
            // Codex's reset type promises Codex rate limits, not unrelated
            // metered buckets, spend controls, or model allowances.
            Self::Account => {
                window.key.starts_with("codex:")
                    && matches!(window.kind, WindowKind::Session | WindowKind::Weekly)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReset {
    /// Opaque provider selection, never a credential.
    pub id: String,
    pub kind: ResetKind,
    pub scopes: Vec<ResetScope>,
    pub available_count: u64,
    pub expires_at: Option<i64>,
    /// Inventory can include grants that are paused or not next in line.
    pub usable: bool,
}

impl UsageReset {
    pub fn label(&self) -> String {
        self.scopes
            .iter()
            .map(ResetScope::label)
            .collect::<Vec<_>>()
            .join(" + ")
    }

    pub fn clears(&self, window: &UsageWindow) -> bool {
        self.scopes.iter().any(|scope| scope.clears(window))
    }

    pub fn is_available(&self, now_secs: i64) -> bool {
        self.available_count > 0
            && self.usable
            && !self.scopes.is_empty()
            && self.expires_at.is_none_or(|expires| expires > now_secs)
    }
}

impl UsageSnapshot {
    /// One usable reset for a window that is near or at its limit.
    pub fn actionable_reset(&self, window: &UsageWindow, now_secs: i64) -> Option<&UsageReset> {
        (window.used >= super::WARNING_FRACTION || window.binding)
            .then(|| {
                self.resets
                    .iter()
                    .find(|reset| reset.is_available(now_secs) && reset.clears(window))
            })
            .flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumeRequest {
    pub account: AccountId,
    pub reset_id: String,
    pub idempotency_key: String,
    pub expected_identity: Option<super::Identity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsumeStatus {
    Reset,
    NoCredit,
    NotLimited,
    /// A request may have reached the provider; never present this as failure
    /// to spend, and never automatically make a second attempt.
    Unconfirmed,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsumeOutcome {
    pub status: ConsumeStatus,
    pub message: Option<String>,
    pub snapshot: Option<UsageSnapshot>,
}

impl ConsumeOutcome {
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: ConsumeStatus::Unavailable,
            message: Some(message.into()),
            snapshot: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(key: &str, kind: WindowKind) -> UsageWindow {
        UsageWindow {
            key: key.into(),
            kind,
            label: key.into(),
            used: 1.0,
            resets_at: Some(200),
            span_secs: None,
            binding: false,
        }
    }
    #[test]
    fn model_resets_and_account_windows_stay_separate() {
        let weekly = window("weekly_all", WindowKind::Weekly);
        let fable = window("weekly_scoped:Fable 5.2", WindowKind::Scoped);
        let opus = window("weekly_scoped:Opus", WindowKind::Scoped);
        assert!(ResetScope::Model("Fable".into()).clears(&fable));
        assert!(!ResetScope::Model("Fable".into()).clears(&weekly));
        assert!(!ResetScope::Model("Fable".into()).clears(&opus));
        assert!(ResetScope::AccountWeekly.clears(&weekly));
        assert!(!ResetScope::AccountWeekly.clears(&fable));
        assert!(!ResetScope::AccountSession.clears(&weekly));
        assert!(ResetScope::Account.clears(&window("codex:secondary", WindowKind::Weekly)));
        assert!(!ResetScope::Account.clears(&window("spark:secondary", WindowKind::Scoped)));
    }
    #[test]
    fn expired_paused_empty_and_low_usage_resets_are_not_actions() {
        let mut reset = UsageReset {
            id: "grant".into(),
            kind: ResetKind::Granted,
            scopes: vec![ResetScope::AccountWeekly],
            available_count: 2,
            expires_at: Some(200),
            usable: true,
        };
        assert!(reset.is_available(199));
        assert!(!reset.is_available(200));
        reset.usable = false;
        assert!(!reset.is_available(100));
        reset.usable = true;
        reset.available_count = 0;
        assert!(!reset.is_available(100));
        reset.available_count = 2;
        let mut snapshot = UsageSnapshot::state(super::super::UsageStatus::Warning, "", 0);
        snapshot.resets.push(reset);
        let mut weekly = window("weekly_all", WindowKind::Weekly);
        assert!(snapshot.actionable_reset(&weekly, 100).is_some());
        weekly.used = 0.1;
        assert!(snapshot.actionable_reset(&weekly, 100).is_none());
    }
}
