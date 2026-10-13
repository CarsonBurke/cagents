//! Non-secret account and caller usage reported by the selected daemon.
use super::{AccountId, Credits, Identity, UsageSnapshot, UsageWindow};
use crate::{agent::Provider, model::TerminalId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Query {
    pub provider: Option<Provider>,
    pub account: Option<AccountId>,
    /// A probe is explicit and addresses exactly one configured account.
    pub refresh: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub sampled_at: u64,
    pub caller: Option<Caller>,
    pub accounts: Vec<AccountInfo>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Caller {
    pub session: TerminalId,
    pub provider: Option<Provider>,
    pub account: Option<AccountId>,
    pub account_detail: Option<String>,
    /// Latest provider-reported context occupancy, distinct from cumulative
    /// usage. Zero is the underlying usage model's unknown sentinel.
    pub context_tokens: Option<u64>,
    pub context_limit: Option<u64>,
    pub context_remaining: Option<u64>,
    pub context_used_fraction: Option<f64>,
    pub usage: Option<crate::chat::Usage>,
}

impl Caller {
    pub fn new(
        session: TerminalId,
        provider: Option<Provider>,
        account: Option<AccountId>,
        account_detail: Option<String>,
        usage: Option<crate::chat::Usage>,
    ) -> Self {
        let context_limit = usage
            .as_ref()
            .map(|u| u.context_limit)
            .filter(|limit| *limit > 0);
        let context_tokens = usage
            .as_ref()
            .filter(|u| u.context_tokens > 0)
            .map(|u| u.context_tokens);
        Self {
            session,
            provider,
            account,
            account_detail,
            context_remaining: context_tokens
                .zip(context_limit)
                .map(|(used, limit)| limit.saturating_sub(used)),
            context_used_fraction: context_tokens
                .zip(context_limit)
                .map(|(used, limit)| used as f64 / limit as f64),
            context_tokens,
            context_limit,
            usage,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub id: AccountId,
    pub provider: Provider,
    pub label: Option<String>,
    /// Registry identity is retained separately from the last observed login.
    pub registered_identity: Option<Identity>,
    pub observed_identity: Option<Identity>,
    pub identity_conflict: bool,
    pub selected_by_caller: bool,
    pub default_for_new_chats: bool,
    pub archived: bool,
    pub usage: Option<UsageSnapshot>,
    pub remaining: Vec<RemainingWindow>,
    pub credits: Option<Credits>,
    /// Derived only from a provider-reported used fraction, never token costs.
    pub credits_remaining_fraction: Option<f32>,
    /// Limit minus used in identical currency/units. A direct balance takes precedence.
    pub credits_remaining: Option<super::Money>,
    pub age_ms: Option<u64>,
    pub stale: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemainingWindow {
    pub window: UsageWindow,
    pub remaining_fraction: f32,
    /// A reset which passed makes old occupancy unsuitable for planning.
    pub reset_elapsed: bool,
}

impl AccountInfo {
    pub fn set_usage(&mut self, usage: UsageSnapshot, now: u64) {
        self.age_ms = now.checked_sub(usage.observed_at);
        self.remaining = usage
            .windows
            .iter()
            .map(|window| RemainingWindow {
                remaining_fraction: window.left(),
                reset_elapsed: window
                    .resets_at
                    .is_some_and(|at| at >= 0 && (at as u64).saturating_mul(1000) <= now),
                window: window.clone(),
            })
            .collect();
        self.stale = self
            .age_ms
            .is_none_or(|age| age >= super::schedule::DISPLAY_STALE_AFTER)
            || self.remaining.iter().any(|window| window.reset_elapsed);
        self.credits_remaining_fraction = usage
            .credits
            .as_ref()
            .filter(|credits| credits.enabled && !credits.unlimited)
            .and_then(|credits| credits.used_fraction)
            .map(|used| (1.0 - used).clamp(0.0, 1.0));
        self.credits_remaining = usage.credits.as_ref().and_then(remaining_credit);
        self.credits = usage.credits.clone();
        self.usage = Some(usage);
    }
}

fn remaining_credit(credits: &Credits) -> Option<super::Money> {
    if !credits.enabled || credits.unlimited || credits.balance.is_some() {
        return None;
    }
    let (used, limit) = (credits.used.as_ref()?, credits.limit.as_ref()?);
    (used.currency == limit.currency
        && used.exponent == limit.exponent
        && used.minor >= 0
        && limit.minor >= 0)
        .then(|| super::Money {
            minor: limit.minor.saturating_sub(used.minor).max(0),
            exponent: limit.exponent,
            currency: limit.currency.clone(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn caller_context_distinguishes_limit_only_from_occupancy_and_saturates() {
        let id = TerminalId::new();
        let unknown = Caller::new(id, None, None, None, Some(Default::default()));
        assert_eq!(unknown.context_tokens, None);
        assert_eq!(unknown.context_limit, None);
        let fresh = Caller::new(
            id,
            None,
            None,
            None,
            Some(crate::chat::Usage {
                context_limit: 100,
                ..Default::default()
            }),
        );
        assert_eq!(fresh.context_limit, Some(100));
        assert_eq!(
            fresh.context_tokens, None,
            "limit metadata alone does not prove empty context"
        );
        assert_eq!(fresh.context_remaining, None);
        assert_eq!(fresh.context_used_fraction, None);
        let full = Caller::new(
            id,
            None,
            None,
            None,
            Some(crate::chat::Usage {
                context_limit: 100,
                context_tokens: 120,
                ..Default::default()
            }),
        );
        assert_eq!(full.context_remaining, Some(0));
        assert_eq!(full.context_used_fraction, Some(1.2));
    }
    #[test]
    fn account_remaining_keeps_reset_and_freshness_evidence() {
        let mut info = AccountInfo {
            id: "codex/system".parse().unwrap(),
            provider: crate::agent::Provider::Codex,
            label: None,
            registered_identity: None,
            observed_identity: None,
            identity_conflict: false,
            selected_by_caller: false,
            default_for_new_chats: true,
            archived: false,
            usage: None,
            remaining: vec![],
            credits: None,
            credits_remaining_fraction: None,
            credits_remaining: None,
            age_ms: None,
            stale: true,
            detail: None,
        };
        let mut usage = UsageSnapshot::state(super::super::UsageStatus::Ok, "", 1000);
        usage.windows.push(UsageWindow {
            key: "weekly".into(),
            kind: super::super::WindowKind::Weekly,
            label: "Weekly".into(),
            used: 0.8,
            resets_at: Some(3),
            span_secs: Some(604800),
            binding: false,
        });
        info.set_usage(usage.clone(), 2000);
        assert!(!info.stale);
        assert!((info.remaining[0].remaining_fraction - 0.2).abs() < 0.0001);
        assert_eq!(info.age_ms, Some(1000));
        info.set_usage(usage.clone(), 3000);
        assert!(info.stale);
        assert!(info.remaining[0].reset_elapsed);
        usage.observed_at = 4000;
        info.set_usage(usage, 2000);
        assert_eq!(info.age_ms, None);
        assert!(info.stale);
    }

    #[test]
    fn brokered_account_ids_roundtrip_through_cli_filter() {
        let account = super::super::Account::brokered(
            crate::agent::Provider::Omp,
            "openai-codex".into(),
            "account-key".into(),
        );
        assert_eq!(
            account.id().as_str().parse::<AccountId>().unwrap(),
            account.id()
        );
        assert!("omp/../0123456789abcdef".parse::<AccountId>().is_err());
        assert!("omp/openai/secret".parse::<AccountId>().is_err());
    }
    #[test]
    fn credits_preserve_direct_balance_and_unknown_or_mixed_units() {
        let money = |minor, exponent, currency: &str| super::super::Money {
            minor,
            exponent,
            currency: currency.into(),
        };
        let mut credits = Credits {
            enabled: true,
            unlimited: false,
            used: Some(money(300, 2, "USD")),
            limit: Some(money(1000, 2, "USD")),
            balance: None,
            used_fraction: None,
        };
        assert_eq!(remaining_credit(&credits), Some(money(700, 2, "USD")));
        credits.balance = Some("provider balance".into());
        assert_eq!(remaining_credit(&credits), None);
        credits.balance = None;
        credits.limit = Some(money(1000, 3, "USD"));
        assert_eq!(remaining_credit(&credits), None);
        credits.limit = Some(money(1000, 2, "EUR"));
        assert_eq!(remaining_credit(&credits), None);
        credits.unlimited = true;
        credits.limit = Some(money(1000, 2, "USD"));
        assert_eq!(remaining_credit(&credits), None);
    }
}
