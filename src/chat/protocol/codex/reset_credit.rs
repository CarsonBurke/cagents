//! A selected credit is consumed only after fresh account/allowance checks
//! on this same connection. ACK is followed by fresh identity/inventory reads.
use super::{Codex, Pending, Step, Value, json};
use crate::account::reset::{Outcome, parse_codex, transport};

#[derive(Clone)]
enum Stage {
    Identity,
    Limits(Value),
    Consume,
    VerifiedIdentity(Outcome),
    VerifiedLimits(Value, Outcome),
}
#[derive(Clone)]
pub(super) struct Redemption {
    request: transport::Consume,
    stage: Stage,
    pub(super) invalidated: bool,
}

pub(super) fn start(adapter: &mut Codex, request: transport::Consume) -> Result<Step, String> {
    request.check()?;
    if !adapter.ready { return Err("Codex connection is not ready for reset-credit redemption".into()); }
    if adapter.pending.values().any(|pending| matches!(pending, Pending::Allowance(_) | Pending::ResetCredit(_))) {
        return Err("Another account allowance/reset request is still pending; reconcile it before retrying".into());
    }
    let mut step = Step::default();
    adapter.call(&mut step, "account/read", json!({"refreshToken":false}), Pending::ResetCredit(Redemption {request, stage:Stage::Identity, invalidated:false}))?;
    Ok(step)
}

pub(super) fn receive(adapter: &mut Codex, mut redemption: Redemption, frame: &Value) -> Result<Step, String> {
    let mut step = Step::default();
    let result = (|| {
        redemption.request.check()?;
        if redemption.invalidated { return Err("Provider login changed during reset redemption; reconcile the original durable attempt".into()); }
        if let Some(error) = frame.get("error") {
            return Err(format!("Codex reset request failed (provider code {}); preserve and reconcile the same durable attempt before retrying", error.get("code").and_then(Value::as_i64).map(|code| code.to_string()).unwrap_or_else(|| "unknown".into())));
        }
        let result = frame.get("result").ok_or("Codex reset reply has no result; outcome requires reconciliation")?;
        match redemption.stage.clone() {
            Stage::Identity | Stage::VerifiedIdentity(_) => {
                let account = super::allowance::account_identity(result)?;
                // Never send allowance or consume under a changed identity.
                if account.pointer("/workspaceRouting/chatgptAccountId").and_then(Value::as_str) != Some(redemption.request.expected.account_id.as_str())
                    || account.pointer("/workspaceRouting/backendOrigin").and_then(Value::as_str) != Some(redemption.request.expected.backend_origin.as_str()) {
                    return Err("The actual provider account no longer matches the authorized reset account; no further operation was sent".into());
                }
                redemption.stage = match redemption.stage { Stage::VerifiedIdentity(outcome) => Stage::VerifiedLimits(account, outcome), _ => Stage::Limits(account) };
                adapter.call(&mut step, "account/rateLimits/read", json!({}), Pending::ResetCredit(redemption.clone()))?;
                Ok(None)
            }
            Stage::Limits(account) | Stage::VerifiedLimits(account, _) => {
                let now = crate::chat::now_ms().ok_or("System clock cannot timestamp reset verification")?;
                let allowance = parse_codex(&account, result, now)?;
                if allowance.account.as_ref() != Some(&redemption.request.expected) {
                    return Err("Provider account changed between reset identity and allowance reads".into());
                }
                if let Stage::VerifiedLimits(_, outcome) = redemption.stage {
                    if allowance.reset_credits.is_none() { return Err("Reset acknowledgement arrived but fresh inventory is unavailable; reconcile the durable attempt".into()); }
                    return Ok(Some(transport::Consumed {outcome, allowance}));
                }
                if let Some(condition) = redemption.request.condition {
                    let decision = crate::account::reset::decision(&allowance, now, condition.max_age_ms, condition.used_percent, true)?;
                    if !matches!(decision, crate::account::reset::Decision::Eligible {..}) {
                        return Err(format!("Shared allowance no longer meets the authorized monitor threshold ({decision:?}); no consuming request was sent. Any earlier uncertain attempt remains unresolved"));
                    }
                }
                if !redemption.request.reconcile {
                    if allowance.reset_credits.as_ref().is_none_or(|credits| credits.available_count == 0) { return Err("Provider reports no available reset credits; no credit was consumed".into()); }
                    let credit = allowance.reset_credits.as_ref().and_then(|credits| credits.details.as_ref())
                        .and_then(|rows| rows.iter().find(|credit| credit.id == redemption.request.credit_id));
                    if credit.is_none_or(|credit| !credit.supported_type || !credit.available || credit.expires_at_ms.is_some_and(|at| at <= now)) {
                        return Err("The selected provider reset credit is not freshly available; no credit was consumed".into());
                    }
                }
                redemption.request.check()?;
                let params = json!({"idempotencyKey":redemption.request.key,"creditId":redemption.request.credit_id});
                redemption.stage = Stage::Consume;
                adapter.call(&mut step, "account/rateLimitResetCredit/consume", params, Pending::ResetCredit(redemption.clone()))?;
                Ok(None)
            }
            Stage::Consume => {
                let outcome = Outcome::parse(result)?;
                redemption.stage = Stage::VerifiedIdentity(outcome);
                adapter.call(&mut step, "account/read", json!({"refreshToken":false}), Pending::ResetCredit(redemption.clone()))?;
                Ok(None)
            }
        }
    })();
    match result {
        Ok(None) => {}
        result => { let _ = redemption.request.reply.send(result.map(|value| value.expect("verified reset result"))); }
    }
    Ok(step)
}
