//! Positive disposition of an unattempted, unpaired handoff's original owner.
//! This is an internal proof producer. Unit Accepted, completion and absence
//! cannot construct its seal or retire the retained source-binding phase.
use super::*;
use crate::daemon::holder_registry::{self as registry, Kind};
use super::source_binding::PendingClose;

/// Constructed only after the exact claimed backend close AND its authenticated
/// native cleanup receipt. A Remote additionally finishes semantic retirement.
/// No wire serialization, public constructor or Clone can manufacture a seal.
pub(super) struct ClosedPending {
    binding: crate::terminal_ipc::ChatSourceBinding,
    witness: chat::runtime::BackendIdentity,
    owner: Handle,
    claim: uuid::Uuid,
}
impl ClosedPending {
    pub(super) fn authenticates(&self, pending: &PendingClose) -> bool {
        self.binding == *pending.binding()
            && self.witness == *pending.witness()
            && self.owner.same_actor(pending.owner())
            && pending.claim_token() == Some(self.claim)
    }
}

/// Caller retains this chat's existing lifecycle claim and exact close
/// admission. None means there is no pending phase and ordinary close continues.
/// Unknown or attempted/pair ownership is an error before any close effect.
pub(super) fn close_unattempted(host: &SharedHost, source: &Session)
    -> Result<Option<()>, String>
{
    let Some(mut pending) = source_binding::capture_pending_close(host, source)
        .map_err(|error| error.message)? else { return Ok(None); };
    super::super::agent_execution::retirement_registry(host)?;
    super::super::agent_execution::preflight_retirement(host, pending.admitted())?;
    let proof = host.lock().unwrap_or_else(|p| p.into_inner()).proof().clone();
    // Capture the actual native owner before effects. A plain Local backend
    // without native receipt production stays Unknown; its generic close result
    // is not upgraded into this stronger native-owner disposition.
    let actor = pending.owner().actor_identity();
    let expected = if let Some(identity) = &actor {
        identity.record.clone()
    } else {
        // A post-close finalizer failure must retry from the retained exact
        // cleanup owner or its authenticated receipt, not a dead helper RPC.
        let retained = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.native_closing.get(&source.id)
                .filter(|close| close.owns(close.owner_record(), pending.admitted()))
                .map(|close| close.owner_record().clone())
        };
        let record = if let Some(record) = retained {
            record
        } else if let Some((record, _)) = chat::native_holder::closed_source(pending.admitted(), &proof)? {
            record
        } else {
            let socket = registry::socket(&registry::directory(Kind::Native)?, source.id);
            let record = registry::evidence(&socket, Kind::Native)?
                .ok_or("This pending handoff has no authenticated native cleanup owner; its phase was retained")?;
            let (recovery, _) = chat::native_holder::inspect(&record, &proof)?;
            if recovery.source != *pending.admitted()
                && !recovery.previous_sources.iter().any(|issued| issued == pending.admitted()) {
                return Err("The native owner does not cover this exact pending source; no provider was stopped".into());
            }
            record
        };
        if Handle::is_remote_record(&record) {
            return Err("The pending Local owner changed to a retained actor; no provider was stopped".into());
        }
        record
    };
    native_cleanup::preflight_current_retirements(host, pending.admitted(), &expected, &proof, None)?;
    // This invokes the actual expected UUID/process/thread/revision + token
    // predicate. Generic close_for_resume would lose this original-owner scope.
    pending.close_original(host).map_err(|error| error.message)?;
    native_cleanup::close_retained_inner_then(host, pending.admitted(), false, None, || {
    let receipt = chat::native_holder::closed_source(pending.admitted(), &proof)?
        .filter(|(record, paired_closed)| record == &expected && *paired_closed)
        .ok_or("The original owner lacks its exact complete native cleanup receipt; the pending phase was retained")?;
    if let Some(identity) = actor {
        if receipt.0 != identity.record {
            return Err("Semantic cleanup belongs to another retained actor; the pending phase was retained".into());
        }
        // Unlike a Local server's family-close receipt, a retained actor must
        // also prove final semantic acknowledgement and exact helper retirement.
        pending.owner().finish_retained_close()?;
    }
    let seal = ClosedPending {
        binding: pending.binding().clone(), witness: pending.witness().clone(),
        owner: pending.owner().clone(), claim: pending.claim_token()
            .ok_or("The original backend close has no confirmed claim token")?,
    };
    source_binding::release_pending_close(host, &pending, &seal)
        .map_err(|error| error.message)
    })?;
    Ok(Some(()))
}
