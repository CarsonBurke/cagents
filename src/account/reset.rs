//! Provider plan allowances and earned reset credits. These observations are
//! metadata, never permission to spend a credit or authority over a session.
//! Conversation token counts and goal budgets deliberately have no input here.
use serde::Serialize;
use serde_json::Value;

const MAX_BUCKETS: usize = 128;
const MAX_CREDIT_ROWS: usize = 128;
const MAX_IDENTIFIER_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ProviderAccount {
    pub account_id: String,
    pub backend_origin: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Window {
    pub bucket: String,
    pub period: Period,
    pub used_percent: u32,
    pub resets_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Period {
    Primary,
    Secondary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ResetCredit {
    pub id: String,
    pub supported_type: bool,
    pub available: bool,
    pub expires_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ResetCredits {
    /// Authoritative count; detail rows may be capped or unavailable.
    pub available_count: u64,
    pub details: Option<Vec<ResetCredit>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Allowance {
    pub observed_at_ms: u64,
    pub account: Option<ProviderAccount>,
    pub account_binding_detail: Option<String>,
    pub windows: Vec<Window>,
    pub reset_credits: Option<ResetCredits>,
}

fn identifier(value: &Value) -> Result<String, String> {
    identifier_str(value.as_str().ok_or("Provider identifier is not a string")?).map(str::to_owned)
}

fn identifier_str(value: &str) -> Result<&str, String> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES || value.chars().any(char::is_control) {
        return Err("Provider identifier is empty, oversized or contains control characters".into());
    }
    Ok(value)
}

fn optional_time(value: Option<&Value>) -> Result<Option<u64>, String> {
    match value.filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(value) => value.as_u64().and_then(|seconds| seconds.checked_mul(1000))
            .map(Some).ok_or_else(|| "Provider reset timestamp is invalid".into()),
    }
}

/// Parse actual successful account/read and account/rateLimits/read results
/// from the same provider connection. The caller must independently fence
/// actor/account changes around these reads and any later credit consumption.
pub(crate) fn parse_codex(account: &Value, limits: &Value, observed_at_ms: u64) -> Result<Allowance, String> {
    if account.pointer("/account/type").and_then(Value::as_str) != Some("chatgpt") {
        return Err("Earned Codex resets require an authenticated ChatGPT account".into());
    }
    let identity = match (account.get("workspaceRouting").filter(|value| !value.is_null()),
        limits.get("accountId").filter(|value| !value.is_null())) {
        (Some(routing), Some(usage_id)) => {
            let account_id = identifier(&routing["chatgptAccountId"])?;
            let usage_id = identifier(usage_id)?;
            if account_id != usage_id {
                return Err("Codex account changed between identity and allowance reads".into());
            }
            Some(ProviderAccount {account_id, backend_origin: identifier(&routing["backendOrigin"])?})
        }
        _ => None,
    };
    let mut windows = Vec::new();
    let mut take_bucket = |name: &str, bucket: &Value| -> Result<(), String> {
        if !bucket.is_object() { return Err("Provider allowance bucket is malformed".into()); }
        for (field, period) in [("primary", Period::Primary), ("secondary", Period::Secondary)] {
            let Some(window) = bucket.get(field).filter(|value| !value.is_null()) else {continue};
            let used_percent = window.get("usedPercent").and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .ok_or("Provider allowance percentage is invalid")?;
            windows.push(Window {bucket: name.to_owned(), period, used_percent,
                resets_at_ms: optional_time(window.get("resetsAt"))?});
        }
        Ok(())
    };
    match limits.get("rateLimitsByLimitId").filter(|value| !value.is_null()) {
        Some(value) => {
            let buckets = value.as_object().ok_or("Provider allowance buckets are malformed")?;
            if buckets.len() > MAX_BUCKETS { return Err("Provider allowance bucket limit exceeded".into()); }
            for (name, bucket) in buckets {
                identifier_str(name)?;
                take_bucket(name, bucket)?;
            }
        }
        None => {
            let bucket = limits.get("rateLimits").ok_or("Provider did not return plan allowance")?;
            let name = bucket.get("limitId").and_then(Value::as_str).unwrap_or("codex");
            identifier_str(name)?;
            take_bucket(name, bucket)?;
        }
    }
    let reset_credits = match limits.get("rateLimitResetCredits").filter(|value| !value.is_null()) {
        None => None,
        Some(value) => {
            let available_count = value.get("availableCount").and_then(Value::as_i64).and_then(|count| u64::try_from(count).ok())
                .ok_or("Provider reset-credit count is invalid")?;
            let details = match value.get("credits").filter(|value| !value.is_null()) {
                None => None,
                Some(value) => {
                    let rows = value.as_array().ok_or("Provider reset-credit details are malformed")?;
                    if rows.len() > MAX_CREDIT_ROWS { return Err("Provider reset-credit detail limit exceeded".into()); }
                    let mut ids = std::collections::BTreeSet::new();
                    let mut details = Vec::new();
                    for row in rows {
                        let id = identifier(&row["id"])?;
                        if !ids.insert(id.clone()) { return Err("Provider returned duplicate reset-credit identifiers".into()); }
                        details.push(ResetCredit {id,
                            supported_type: row["resetType"].as_str() == Some("codexRateLimits"),
                            available: row["status"].as_str() == Some("available"),
                            expires_at_ms: optional_time(row.get("expiresAt"))?});
                    }
                    Some(details)
                }
            };
            Some(ResetCredits {available_count, details})
        }
    };
    Ok(Allowance {observed_at_ms, account_binding_detail: identity.is_none()
        .then(|| "Provider did not expose a matching active account identity; redemption is unavailable".into()),
        account: identity, windows, reset_credits})
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Stale,
    UnknownUsage,
    BelowThreshold,
    UnknownAccount,
    AuthorizationRequired,
    InventoryUnknown,
    NoCredits,
    CreditDetailsUnavailable,
    Eligible {credit_id: String},
}

/// A threshold is a scheduling decision only. Even Eligible still requires
/// exact live-owner validation, durable shared-account deduplication and the
/// actual explicit authorization at the daemon's effect boundary.
pub(crate) fn decision(allowance: &Allowance, now_ms: u64, max_age_ms: u64,
    threshold_percent: u8, explicitly_authorized: bool) -> Result<Decision, String> {
    if threshold_percent == 0 || threshold_percent > 100 || max_age_ms == 0 {
        return Err("Use a threshold from 1 to 100 percent and a positive freshness limit".into());
    }
    if now_ms < allowance.observed_at_ms || now_ms - allowance.observed_at_ms > max_age_ms {
        return Ok(Decision::Stale);
    }
    if allowance.windows.is_empty() { return Ok(Decision::UnknownUsage); }
    if allowance.windows.iter().any(|window| window.resets_at_ms.is_some_and(|time| time <= now_ms)) {
        return Ok(Decision::Stale);
    }
    if !allowance.windows.iter().any(|window| window.used_percent >= u32::from(threshold_percent)) {
        return Ok(Decision::BelowThreshold);
    }
    if allowance.account.is_none() { return Ok(Decision::UnknownAccount); }
    if !explicitly_authorized { return Ok(Decision::AuthorizationRequired); }
    let Some(credits) = &allowance.reset_credits else {return Ok(Decision::InventoryUnknown)};
    if credits.available_count == 0 { return Ok(Decision::NoCredits); }
    let Some(rows) = &credits.details else {return Ok(Decision::CreditDetailsUnavailable)};
    Ok(rows.iter().filter(|credit| credit.supported_type && credit.available
        && credit.expires_at_ms.is_none_or(|time| time > now_ms))
        .min_by(|a, b| a.expires_at_ms.unwrap_or(u64::MAX).cmp(&b.expires_at_ms.unwrap_or(u64::MAX))
            .then_with(|| a.id.cmp(&b.id)))
        .map(|credit| Decision::Eligible {credit_id: credit.id.clone()})
        .unwrap_or(Decision::CreditDetailsUnavailable))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture() -> (Value, Value) {
        (json!({"account":{"type":"chatgpt","email":"display@example.invalid"},
            "workspaceRouting":{"chatgptAccountId":"actual-account","backendOrigin":"https://chatgpt.com"}}),
            json!({"accountId":"actual-account","rateLimits":{"primary":{"usedPercent":95,"resetsAt":100}},
                "rateLimitResetCredits":{"availableCount":3,"credits":[{"id":"credit-one",
                    "resetType":"codexRateLimits","status":"available","expiresAt":90}]}}))
    }
    #[test]
    fn actual_allowance_threshold_requires_explicit_authorization_and_preserves_inventory_count() {
        let (account, limits) = fixture();
        let allowance = parse_codex(&account, &limits, 1000).unwrap();
        assert_eq!(allowance.reset_credits.as_ref().unwrap().available_count, 3);
        assert_eq!(decision(&allowance, 1100, 1000, 90, false).unwrap(), Decision::AuthorizationRequired);
        assert_eq!(decision(&allowance, 1100, 1000, 90, true).unwrap(), Decision::Eligible {credit_id:"credit-one".into()});
        assert_eq!(decision(&allowance, 1100, 1000, 99, true).unwrap(), Decision::BelowThreshold);
    }
    #[test]
    fn conversation_tokens_and_scheduled_resets_never_manufacture_allowance_or_inventory() {
        let (account, mut limits) = fixture();
        limits["rateLimits"] = json!({});
        limits["usage"] = json!({"totalTokens":1000000000});
        let allowance = parse_codex(&account, &limits, 1000).unwrap();
        assert_eq!(decision(&allowance, 1100, 1000, 90, true).unwrap(), Decision::UnknownUsage);
        limits["rateLimits"] = json!({"primary":{"usedPercent":99,"resetsAt":1}});
        assert_eq!(decision(&parse_codex(&account, &limits, 1000).unwrap(), 1100, 1000, 90, true).unwrap(), Decision::Stale);
        limits["rateLimits"] = json!({"primary":{"usedPercent":99}});
        limits.as_object_mut().unwrap().remove("rateLimitResetCredits");
        assert_eq!(decision(&parse_codex(&account, &limits, 1000).unwrap(), 1100, 1000, 90, true).unwrap(), Decision::InventoryUnknown);
    }
    #[test]
    fn missing_or_contradictory_provider_identity_never_uses_email_for_shared_account_binding() {
        let (mut account, mut limits) = fixture();
        limits["accountId"] = json!("another-account");
        assert!(parse_codex(&account, &limits, 1000).is_err());
        limits["accountId"] = json!("actual-account");
        account.as_object_mut().unwrap().remove("workspaceRouting");
        let allowance = parse_codex(&account, &limits, 1000).unwrap();
        assert_eq!(decision(&allowance, 1100, 1000, 90, true).unwrap(), Decision::UnknownAccount);
    }
    #[test]
    fn unknown_capped_empty_expired_and_exhausted_inventory_remain_distinct() {
        let (account, mut limits) = fixture();
        for details in [Value::Null, json!([])] {
            limits["rateLimitResetCredits"]["credits"] = details;
            let allowance = parse_codex(&account, &limits, 1000).unwrap();
            assert_eq!(allowance.reset_credits.as_ref().unwrap().available_count, 3);
            assert_eq!(decision(&allowance, 1100, 1000, 90, true).unwrap(), Decision::CreditDetailsUnavailable);
        }
        limits["rateLimitResetCredits"] = json!({"availableCount":0,"credits":[]});
        assert_eq!(decision(&parse_codex(&account, &limits, 1000).unwrap(), 1100, 1000, 90, true).unwrap(), Decision::NoCredits);
        limits["rateLimitResetCredits"] = json!({"availableCount":1,"credits":[{"id":"expired", "resetType":"codexRateLimits","status":"available","expiresAt":1}]});
        assert_eq!(decision(&parse_codex(&account, &limits, 1000).unwrap(), 1100, 1000, 90, true).unwrap(), Decision::CreditDetailsUnavailable);
    }
    #[test]
    fn malformed_and_oversized_provider_fields_fail_before_any_decision() {
        let (account, mut limits) = fixture();
        limits["rateLimitsByLimitId"] = json!({"x".repeat(MAX_IDENTIFIER_BYTES+1):{}});
        assert!(parse_codex(&account, &limits, 1000).is_err());
        limits.as_object_mut().unwrap().remove("rateLimitsByLimitId");
        limits["rateLimits"]["limitId"] = json!("x".repeat(MAX_IDENTIFIER_BYTES+1));
        assert!(parse_codex(&account, &limits, 1000).is_err());
        limits["rateLimits"].as_object_mut().unwrap().remove("limitId");
        limits["rateLimitResetCredits"]["availableCount"] = json!(-1);
        assert!(parse_codex(&account, &limits, 1000).is_err());
        limits["rateLimitResetCredits"] = json!({"availableCount":2,"credits":[{"id":"same"},{"id":"same"}]});
        assert!(parse_codex(&account, &limits, 1000).unwrap_err().contains("duplicate"));
        limits["rateLimitResetCredits"] = json!({"availableCount":1,"credits":[{"id":"x".repeat(MAX_IDENTIFIER_BYTES+1)}]});
        assert!(parse_codex(&account, &limits, 1000).is_err());
    }
}
