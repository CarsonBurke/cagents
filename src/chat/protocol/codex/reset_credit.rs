//! A selected credit is consumed only after fresh account/allowance checks
//! on this same connection. ACK is followed by fresh identity/inventory reads.
use super::{Codex, Pending, Step, Value, json};
use crate::account::reset::{Outcome, parse_codex, transport};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Default)]
pub(super) struct State {
    uncertain: Option<transport::Attempt>,
    reservation: Option<Reserved>,
    pub(super) tombstones: BTreeSet<String>,
}
struct Reserved {
    request: transport::Reservation,
    valid: Arc<AtomicBool>,
}

struct Permit {
    request: transport::Consume,
    valid: AtomicBool,
    reservation: Arc<AtomicBool>,
}
impl Permit {
    fn revoke(&self) {
        self.valid.store(false, Ordering::Release);
    }
}
impl crate::chat::protocol::WriteGuard for Permit {
    fn before_first_byte(&self) -> bool {
        if self.valid.load(Ordering::Acquire)
            && self.reservation.load(Ordering::Acquire)
            && self.request.check().is_ok()
        {
            return true;
        }
        self.revoke();
        let _ = self.request.reply.try_send(Err("The wholly unsent reset request was cancelled because its account, reservation or deadline changed; preserve and reconcile the original durable attempt".into()));
        false
    }
}

fn stale_ids(adapter: &Codex) -> Vec<String> {
    adapter
        .pending
        .iter()
        .filter_map(|(id, pending)| {
            match pending {
                Pending::Allowance(read) => read.stale(),
                Pending::ResetCredit(redemption) => redemption.stale(),
                _ => false,
            }
            .then(|| id.clone())
        })
        .collect()
}
fn check_capacity(adapter: &Codex, stale: &[String]) -> Result<(), String> {
    if adapter.reset_state.tombstones.len() + stale.len() > super::MAX_PENDING {
        return Err("Too many unanswered account requests to retain their late-response IDs safely; the live turn and original reset attempt were preserved".into());
    }
    Ok(())
}
fn retire(adapter: &mut Codex, stale: Vec<String>) {
    for id in stale {
        match adapter
            .pending
            .remove(&id)
            .expect("retained stale account request")
        {
            Pending::Allowance(read) => read.retire(),
            Pending::ResetCredit(redemption) => {
                // The queue may still own this permit after Pending is gone.
                redemption.permit.revoke();
                let _ = redemption.request.reply.try_send(Err("The previous reset request expired or its account changed; reconcile the original durable tuple and key".into()));
            }
            _ => unreachable!(),
        }
        adapter.reset_state.tombstones.insert(id);
    }
}
pub(super) fn retire_stale(adapter: &mut Codex) -> Result<(), String> {
    let stale = stale_ids(adapter);
    check_capacity(adapter, &stale)?;
    retire(adapter, stale);
    Ok(())
}

pub(super) fn reserve(adapter: &mut Codex, request: transport::Reservation) -> Result<(), String> {
    request.check()?;
    if !adapter.ready {
        return Err("Codex connection is not ready for reset reservation".into());
    }
    let stale = stale_ids(adapter);
    check_capacity(adapter, &stale)?;
    if adapter.pending.iter().any(|(id, pending)| {
        !stale.contains(id) && matches!(pending, Pending::Allowance(_) | Pending::ResetCredit(_))
    }) {
        return Err("Another account allowance/reset request is still pending".into());
    }
    if adapter
        .reset_state
        .reservation
        .as_ref()
        .is_some_and(|reserved| {
            reserved.request.token != request.token
                && reserved.request.check().is_ok()
                && reserved.valid.load(Ordering::Acquire)
        })
    {
        return Err("Another controller has reserved this exact actor for a reset attempt".into());
    }
    let resolved = adapter
        .reset_state
        .uncertain
        .as_ref()
        .is_some_and(|uncertain| {
            request
                .verified
                .iter()
                .any(|proof| &proof.attempt == uncertain)
        });
    if adapter
        .reset_state
        .uncertain
        .as_ref()
        .is_some_and(|uncertain| uncertain != &request.attempt && !resolved)
    {
        return Err("The original account, origin, credit and key remain uncertain on this actor; reconcile that exact durable attempt before selecting another".into());
    }
    // Every refusal above leaves the retained state intact. Revoke old
    // permits and reserve atomically before the controller writes an intent.
    retire(adapter, stale);
    if let Some(prior) = adapter.reset_state.reservation.take() {
        prior.valid.store(false, Ordering::Release);
    }
    if resolved {
        adapter.reset_state.uncertain = None;
    }
    adapter.reset_state.reservation = Some(Reserved {
        request,
        valid: Arc::new(AtomicBool::new(true)),
    });
    Ok(())
}
pub(super) fn release(adapter: &mut Codex, token: uuid::Uuid) {
    if adapter
        .reset_state
        .reservation
        .as_ref()
        .is_some_and(|reserved| reserved.request.token == token)
        && let Some(reserved) = adapter.reset_state.reservation.take()
    {
        reserved.valid.store(false, Ordering::Release);
    }
}
pub(super) fn invalidate(adapter: &mut Codex) {
    if let Some(reserved) = &adapter.reset_state.reservation {
        reserved.valid.store(false, Ordering::Release);
    }
}

fn reservation(adapter: &Codex, request: &transport::Consume) -> Result<Arc<AtomicBool>, String> {
    let reserved = adapter
        .reset_state
        .reservation
        .as_ref()
        .filter(|reserved| {
            reserved.request.token == request.reservation
                && reserved.request.attempt == request.attempt()
                && reserved.valid.load(Ordering::Acquire)
        })
        .ok_or("The reset controller's exact actor reservation is no longer current")?;
    reserved.request.check()?;
    Ok(reserved.valid.clone())
}

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
    permit: Arc<Permit>,
}
impl Redemption {
    #[cfg(test)]
    pub(super) fn expire_for_test(&mut self) {
        self.request.expires = std::time::Instant::now() - std::time::Duration::from_millis(1);
    }
    fn stale(&self) -> bool {
        self.invalidated
            || self.request.check().is_err()
            || !self.permit.valid.load(Ordering::Acquire)
            || !self.permit.reservation.load(Ordering::Acquire)
    }
    pub(super) fn invalidate(&mut self) {
        self.invalidated = true;
        self.permit.revoke();
    }
    pub(super) fn write_guard(&self) -> Option<Arc<dyn crate::chat::protocol::WriteGuard>> {
        matches!(self.stage, Stage::Consume)
            .then(|| self.permit.clone() as Arc<dyn crate::chat::protocol::WriteGuard>)
    }
}

pub(super) fn start(adapter: &mut Codex, request: transport::Consume) -> Result<Step, String> {
    request.check()?;
    if !adapter.ready {
        return Err("Codex connection is not ready for reset-credit redemption".into());
    }
    retire_stale(adapter)?;
    let reserved = reservation(adapter, &request)?;
    if let Some(uncertain) = &adapter.reset_state.uncertain {
        if uncertain != &request.attempt() || !request.reconcile {
            return Err("An uncertain reset requires explicit reconciliation of the original account, origin, credit and key".into());
        }
    }
    if adapter
        .pending
        .values()
        .any(|pending| matches!(pending, Pending::Allowance(_) | Pending::ResetCredit(_)))
    {
        return Err("Another account allowance/reset request is still pending; reconcile it before retrying".into());
    }
    let permit = Arc::new(Permit {
        request: request.clone(),
        valid: AtomicBool::new(true),
        reservation: reserved,
    });
    let mut step = Step::default();
    adapter.call(
        &mut step,
        "account/read",
        json!({"refreshToken":false}),
        Pending::ResetCredit(Redemption {
            request,
            stage: Stage::Identity,
            invalidated: false,
            permit,
        }),
    )?;
    Ok(step)
}

pub(super) fn receive(
    adapter: &mut Codex,
    mut redemption: Redemption,
    frame: &Value,
) -> Result<Step, String> {
    let mut step = Step::default();
    let result = (|| {
        redemption.request.check()?;
        if !redemption.permit.reservation.load(Ordering::Acquire) {
            return Err(
                "The original reset reservation was released; reconcile its durable attempt".into(),
            );
        }
        if redemption.invalidated {
            return Err("Provider login changed during reset redemption; reconcile the original durable attempt".into());
        }
        if let Some(error) = frame.get("error") {
            return Err(format!(
                "Codex reset request failed (provider code {}); preserve and reconcile the same durable attempt before retrying",
                error
                    .get("code")
                    .and_then(Value::as_i64)
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".into())
            ));
        }
        let result = frame
            .get("result")
            .ok_or("Codex reset reply has no result; outcome requires reconciliation")?;
        match redemption.stage.clone() {
            Stage::Identity | Stage::VerifiedIdentity(_) => {
                let account = super::allowance::account_identity(result)?;
                // Never send allowance or consume under a changed identity.
                if account
                    .pointer("/workspaceRouting/chatgptAccountId")
                    .and_then(Value::as_str)
                    != Some(redemption.request.expected.account_id.as_str())
                    || account
                        .pointer("/workspaceRouting/backendOrigin")
                        .and_then(Value::as_str)
                        != Some(redemption.request.expected.backend_origin.as_str())
                {
                    return Err("The actual provider account no longer matches the authorized reset account; no further operation was sent".into());
                }
                redemption.stage = match redemption.stage {
                    Stage::VerifiedIdentity(outcome) => Stage::VerifiedLimits(account, outcome),
                    _ => Stage::Limits(account),
                };
                adapter.call(
                    &mut step,
                    "account/rateLimits/read",
                    json!({}),
                    Pending::ResetCredit(redemption.clone()),
                )?;
                Ok(None)
            }
            Stage::Limits(account) | Stage::VerifiedLimits(account, _) => {
                let now = crate::chat::now_ms()
                    .ok_or("System clock cannot timestamp reset verification")?;
                let allowance = parse_codex(&account, result, now)?;
                if allowance.account.as_ref() != Some(&redemption.request.expected) {
                    return Err(
                        "Provider account changed between reset identity and allowance reads"
                            .into(),
                    );
                }
                if let Stage::VerifiedLimits(_, outcome) = redemption.stage {
                    if allowance.reset_credits.is_none() {
                        return Err("Reset acknowledgement arrived but fresh inventory is unavailable; reconcile the durable attempt".into());
                    }
                    return Ok(Some(transport::Consumed { outcome, allowance }));
                }
                if let Some(condition) = redemption.request.condition {
                    let decision = crate::account::reset::decision(
                        &allowance,
                        now,
                        condition.max_age_ms,
                        condition.used_percent,
                        true,
                    )?;
                    if !matches!(decision, crate::account::reset::Decision::Eligible { .. }) {
                        return Err(format!(
                            "Shared allowance no longer meets the authorized monitor threshold ({decision:?}); no consuming request was sent. Any earlier uncertain attempt remains unresolved"
                        ));
                    }
                }
                if !redemption.request.reconcile {
                    if allowance
                        .reset_credits
                        .as_ref()
                        .is_none_or(|credits| credits.available_count == 0)
                    {
                        return Err(
                            "Provider reports no available reset credits; no credit was consumed"
                                .into(),
                        );
                    }
                    let credit = allowance
                        .reset_credits
                        .as_ref()
                        .and_then(|credits| credits.details.as_ref())
                        .and_then(|rows| {
                            rows.iter()
                                .find(|credit| credit.id == redemption.request.credit_id)
                        });
                    if credit.is_none_or(|credit| {
                        !credit.supported_type
                            || !credit.available
                            || credit.expires_at_ms.is_some_and(|at| at <= now)
                    }) {
                        return Err("The selected provider reset credit is not freshly available; no credit was consumed".into());
                    }
                }
                redemption.request.check()?;
                reservation(adapter, &redemption.request)?;
                let params = json!({"idempotencyKey":redemption.request.key,"creditId":redemption.request.credit_id});
                redemption.stage = Stage::Consume;
                adapter.call(
                    &mut step,
                    "account/rateLimitResetCredit/consume",
                    params,
                    Pending::ResetCredit(redemption.clone()),
                )?;
                adapter.reset_state.uncertain = Some(redemption.request.attempt());
                Ok(None)
            }
            Stage::Consume => {
                let outcome = Outcome::parse(result)?;
                redemption.stage = Stage::VerifiedIdentity(outcome);
                adapter.call(
                    &mut step,
                    "account/read",
                    json!({"refreshToken":false}),
                    Pending::ResetCredit(redemption.clone()),
                )?;
                Ok(None)
            }
        }
    })();
    match result {
        Ok(None) => {}
        result => {
            redemption.permit.revoke();
            if result.is_ok()
                && adapter.reset_state.uncertain.as_ref() == Some(&redemption.request.attempt())
            {
                adapter.reset_state.uncertain = None;
            }
            let _ = redemption
                .request
                .reply
                .try_send(result.map(|value| value.expect("verified reset result")));
        }
    }
    Ok(step)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::protocol::WriteGuard;
    use std::time::{Duration, Instant};

    #[test]
    fn expired_consume_permit_and_reservation_stay_revoked_when_a_fresh_retry_is_issued() {
        let (reply, receiver) = crossbeam_channel::bounded(1);
        let expected = crate::account::reset::ProviderAccount {
            account_id: "actual-account".into(),
            backend_origin: "https://chatgpt.com".into(),
        };
        let expired = transport::Consume::fixture(
            Instant::now() - Duration::from_millis(1),
            expected.clone(),
            reply,
        );
        assert!(transport::Reservation::fixture(&expired).check().is_err());
        let old = Permit {
            request: expired.clone(),
            valid: AtomicBool::new(true),
            reservation: Arc::new(AtomicBool::new(true)),
        };
        assert!(!old.before_first_byte());
        assert!(receiver.try_recv().unwrap().is_err());
        // Repeated guard checks cannot block when the requester channel fills
        // or convert the old deadline into the new retry's deadline.
        assert!(!old.before_first_byte());
        let (reply, _) = crossbeam_channel::bounded(1);
        let fresh =
            transport::Consume::fixture(Instant::now() + Duration::from_secs(2), expected, reply);
        assert_eq!(expired.key, fresh.key);
        assert_ne!(expired.reservation, fresh.reservation);
        let retry = Permit {
            request: fresh,
            valid: AtomicBool::new(true),
            reservation: Arc::new(AtomicBool::new(true)),
        };
        assert!(retry.before_first_byte());
        assert!(!old.before_first_byte());
    }
}
