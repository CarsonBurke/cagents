//! Off-HostLock orchestration for one authenticated, exact live source. The
//! host supplies a current-record recheck; this module never selects by recipe.
use super::{
    attempts::{Admission, Ledger},
    request::{Action, Report, Request, Source},
    transport,
};
use crate::{agent::Provider, chat::runtime::Handle};
use std::{path::Path, time::Instant};

struct Lease<'a> {
    handle: &'a Handle,
    token: uuid::Uuid,
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.handle.release_reset_credit(self.token);
    }
}

pub(crate) fn execute(
    handle: &Handle,
    request: Request,
    root: Option<&Path>,
    expires: Instant,
    current: impl Fn() -> Result<(), String>,
) -> Result<Report, String> {
    request.validate()?;
    current()?;
    let mut allowance = handle.read_allowance(expires)?;
    current()?;
    let mut outcome = None;
    let mut reconciled = false;
    if let Action::Redeem {
        expected,
        credit_id,
        authorized: _,
        condition,
    } = request.action
    {
        if allowance.account.as_ref() != Some(&expected) {
            return Err("The target's actual provider account differs from the requested reset account; no credit was consumed".into());
        }
        let mut ledger = Ledger::open(
            root.ok_or("Durable reset-attempt storage is unavailable; no credit was consumed")?,
            &expected,
        )?;
        // Re-read after acquiring the actual-account lease: a different
        // session may have reset this shared allowance while we waited.
        current()?;
        allowance = handle.read_allowance(expires)?;
        if allowance.account.as_ref() != Some(&expected) {
            return Err(
                "Provider account changed before reset intent; no credit was consumed".into(),
            );
        }
        let now = crate::chat::now_ms().ok_or("System clock cannot timestamp reset intent")?;
        let meets_condition = match condition {
            Some(condition) => matches!(
                super::decision(
                    &allowance,
                    now,
                    condition.max_age_ms,
                    condition.used_percent,
                    true
                )?,
                super::Decision::Eligible { .. }
            ),
            None => true,
        };
        if !meets_condition && ledger.uncertain_credit()?.is_some() {
            current()?;
            return Err("An earlier reset attempt remains uncertain although current usage is below the monitor condition. No consuming request was sent; explicitly reconcile the original account and credit before retrying".into());
        }
        if meets_condition {
            // Reserve this exact actor before writing an intent. Different
            // account/origin ledgers cannot race through a check-only gate.
            // Current terminal records can resolve an old actor-local tuple
            // completed by another pane, including verified no-effect ACKs.
            let reservation = transport::Reservation::prepared(
                expires,
                expected,
                credit_id.clone(),
                ledger.original_key(&credit_id)?,
                ledger.verified_attempts(&allowance)?,
            );
            let _lease = Lease {
                handle,
                token: reservation.token,
            };
            current()?;
            handle.reserve_reset_credit(reservation.clone())?;
            current()?;
            reservation.check()?;
            match ledger.begin_available(&credit_id, now, &allowance)? {
                Admission::Complete(prior) => {
                    outcome = Some(prior);
                    reconciled = true;
                }
                Admission::Pending {
                    idempotency_key,
                    reconcile_first,
                } => {
                    if idempotency_key != reservation.attempt.key {
                        return Err("Reset intent no longer matches its actor reservation".into());
                    }
                    // That durable record is intentionally retained after EVERY
                    // error below, including missing RPC support and timeout.
                    current()?;
                    let (reply, response) = crossbeam_channel::bounded(1);
                    handle.consume_reset_credit(transport::Consume::prepared(
                        &reservation,
                        reconcile_first,
                        condition,
                        reply,
                    ))?;
                    let consumed = response.recv_deadline(expires)
                    .map_err(|_| "Provider reset outcome is uncertain; preserve and reconcile the original attempt before retrying".to_string())??;
                    current()?;
                    if Instant::now() >= expires {
                        return Err("Reset response arrived after its deadline; reconcile the durable attempt".into());
                    }
                    ledger.finish(&credit_id, consumed.outcome, &consumed.allowance)?;
                    allowance = consumed.allowance;
                    outcome = Some(consumed.outcome);
                    reconciled = reconcile_first;
                }
            }
        }
    }
    current()?;
    if Instant::now() >= expires {
        return Err(
            "Allowance operation deadline expired; the running source was preserved".into(),
        );
    }
    let report = Report {
        session: request.session,
        provider: Provider::Codex,
        source: Source::CodexAppServer,
        allowance,
        outcome,
        reconciled,
    };
    report.validate()?;
    Ok(report)
}
