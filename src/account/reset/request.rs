//! Proposed bounded device request/report shape. Not registered in the public
//! protocol until the matched-set owner allocates its epoch.
use super::{Allowance, Outcome, ProviderAccount, identifier_str};
use crate::{agent::Provider, model::TerminalId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub session: TerminalId,
    pub action: Action,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Inspect,
    Redeem {
        /// Requested identity, freshly checked on the actual actor before
        /// effects; never a caller-supplied account grant.
        expected: ProviderAccount,
        credit_id: String,
        /// Explicit user intent only. No implied consent from a threshold.
        authorized: bool,
        condition: Option<Threshold>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Threshold {
    pub used_percent: u8,
    pub max_age_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    CodexAppServer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub session: TerminalId,
    pub provider: Provider,
    pub source: Source,
    pub allowance: Allowance,
    pub outcome: Option<Outcome>,
    pub reconciled: bool,
}

impl Request {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if let Action::Redeem {
            expected,
            credit_id,
            authorized,
            condition,
        } = &self.action
        {
            identifier_str(&expected.account_id)?;
            identifier_str(&expected.backend_origin)?;
            identifier_str(credit_id)?;
            if condition.is_some_and(|condition| {
                condition.used_percent == 0
                    || condition.used_percent > 100
                    || condition.max_age_ms == 0
                    || condition.max_age_ms > 60_000
            }) {
                return Err(
                    "Monitor threshold must be1..100 percent with freshness1..60000 milliseconds"
                        .into(),
                );
            }
            if !authorized {
                return Err(
                    "Redeeming a provider reset credit requires explicit --authorize authorization"
                        .into(),
                );
            }
        }
        Ok(())
    }
}

impl Report {
    /// Typed, nonrecursive positional reply. Bounds apply after decoding too;
    /// raw provider JSON/error payloads are never carried across this boundary.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.provider != Provider::Codex || self.allowance.windows.len() > 256 {
            return Err("Unexpected provider or oversized allowance report".into());
        }
        if self
            .allowance
            .account_binding_detail
            .as_ref()
            .is_some_and(|detail| detail.len() > 1024)
        {
            return Err("Oversized allowance identity detail".into());
        }
        if let Some(account) = &self.allowance.account {
            identifier_str(&account.account_id)?;
            identifier_str(&account.backend_origin)?;
        }
        for window in &self.allowance.windows {
            identifier_str(&window.bucket)?;
        }
        if let Some(credits) = &self.allowance.reset_credits {
            if credits.available_count > i64::MAX as u64 {
                return Err("Invalid provider reset-credit count".into());
            }
            if let Some(rows) = &credits.details {
                if rows.len() > 128 {
                    return Err("Oversized reset-credit report".into());
                }
                for row in rows {
                    identifier_str(&row.id)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requested_identity_is_bounded_intent_and_requires_explicit_authorization() {
        let request = Request {
            session: TerminalId::new(),
            action: Action::Redeem {
                expected: ProviderAccount {
                    account_id: "actual-account".into(),
                    backend_origin: "https://chatgpt.com".into(),
                },
                credit_id: "opaque-credit".into(),
                authorized: false,
                condition: None,
            },
        };
        assert!(request.validate().unwrap_err().contains("explicit"));
        let mut authorized = request;
        let Action::Redeem {
            authorized: consent,
            credit_id,
            ..
        } = &mut authorized.action
        else {
            panic!()
        };
        *consent = true;
        *credit_id = "x".repeat(1025);
        assert!(authorized.validate().is_err());
    }
}
