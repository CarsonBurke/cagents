use super::{Handle, SharedHost, caller, directory, registry, resolve, routing_lineages};
use crate::{
    agent_execution::{
        Authority, Compaction, CompactionState, Lineage, TrackedCompaction, directory::UsageLimit,
    },
    chat::runtime::{CompactionOwner, Handle as Conversation, IdleCompaction, Rejection},
    model::TerminalId,
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use uuid::Uuid;

const RECEIPT_LIFETIME: Duration = Duration::from_secs(15 * 60);
const MAX_RECEIPTS: usize = 512;

pub(super) struct Receipt {
    created: Instant,
    session: TerminalId,
    owner: CompactionOwner,
    state: CompactionState,
}

/// A compaction belongs to the exact live conversation the caller selected.
#[derive(Clone)]
pub(super) struct Pending {
    conversation: Conversation,
    request: Uuid,
    /// The requesting connection has finished its first attempt. Only the
    /// courier can dispatch after that, so a second request coalesces.
    deferred: bool,
}

pub(in crate::host::agent_execution) fn compact(
    host: &SharedHost,
    authority: &Authority,
    name: &str,
    tracked: bool,
) -> Result<TrackedCompaction, String> {
    // Like sending a message, compaction requires a valid caller, including
    // when the target is a peer. It confers no other control of the target.
    caller(host, authority)?;
    if super::defers_for_update(host) {
        return Err("Compaction is paused while cagents updates".into());
    }
    let live = directory::live_sessions(host);
    let registry = registry(host)?;
    let (lineage, lineages) = {
        let store = registry.lock().unwrap_or_else(|p| p.into_inner());
        let agents = store.agents(None)?;
        let lineages = routing_lineages(&store, &agents, &live, None)?;
        let lineage = resolve(
            &agents,
            &live,
            &|id| lineages.get(&id).copied().unwrap_or(Lineage(id)),
            name,
        )?;
        (lineage, lineages)
    };
    let mut matching = live.iter().filter(|entry| {
        lineages
            .get(&entry.id)
            .copied()
            .unwrap_or(Lineage(entry.id))
            == lineage
    });
    let first = matching
        .next()
        .ok_or("The session's provider is no longer running")?;
    let target = if let Some(second) = matching.next() {
        let prefix = name.to_ascii_lowercase();
        let mut exact = [first, second]
            .into_iter()
            .chain(matching)
            .filter(|entry| entry.id.to_string().starts_with(&prefix));
        match (exact.next(), exact.next()) {
            (Some(entry), None) => entry,
            _ => {
                return Err(
                    "This lineage has multiple live sessions; name the exact session ID".into(),
                );
            }
        }
    } else {
        first
    };
    let session = target.id;
    let Some(conversation) = target.chat.as_ref() else {
        return Ok(reply(session, None, CompactionState::Unsupported));
    };
    if conversation.compact_support() == Some(false) {
        return Ok(reply(session, None, CompactionState::Unsupported));
    }
    let courier = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .mailbox
        .clone()
        .ok_or("The conversation courier is unavailable")?;
    let now = super::super::now();
    let limited = target.provider.is_some_and(|provider| {
        directory::chat_limit(
            provider,
            conversation,
            || {
                super::super::account_info::Accounts::load(now).and_then(|accounts| {
                    accounts.of(provider, &conversation.execution_environment().0)
                })
            },
            now,
        )
        .is_some()
    });
    courier.request_compaction(host, session, conversation, limited, tracked)
}

fn reply(session: TerminalId, request: Option<Uuid>, state: CompactionState) -> TrackedCompaction {
    TrackedCompaction {
        request,
        compaction: Compaction { session, state },
    }
}

pub(in crate::host::agent_execution) fn status(
    host: &SharedHost,
    authority: &Authority,
    request: Uuid,
) -> Result<TrackedCompaction, String> {
    caller(host, authority)?;
    let courier = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .mailbox
        .clone()
        .ok_or("The conversation courier is unavailable")?;
    let (session, state, owner) = {
        let state = courier.0.state.lock().unwrap_or_else(|p| p.into_inner());
        let receipt = state
            .compaction_receipts
            .get(&request)
            .filter(|receipt| receipt.created.elapsed() < RECEIPT_LIFETIME)
            .ok_or("The compaction receipt is unavailable")?;
        (
            receipt.session,
            receipt.state.clone(),
            receipt.owner.upgrade(),
        )
    };
    let state = if matches!(state, CompactionState::Submitted) {
        match owner {
            Some(owner) => match owner.compaction_outcome(request) {
                Ok(Some(outcome)) => outcome.into(),
                Ok(None) if !owner.is_completed() => state,
                Ok(None) => owner.compaction_outcome(request)?.map_or_else(
                    || {
                        CompactionState::Unconfirmed(
                            "The original provider stopped without confirming compaction".into(),
                        )
                    },
                    Into::into,
                ),
                Err(reason) => CompactionState::Unconfirmed(reason),
            },
            None => CompactionState::Unconfirmed(
                "The original conversation is no longer available".into(),
            ),
        }
    } else {
        state
    };
    Ok(reply(session, Some(request), state))
}

impl Handle {
    fn request_compaction(
        &self,
        host: &SharedHost,
        session: TerminalId,
        conversation: &Conversation,
        limited: bool,
        tracked: bool,
    ) -> Result<TrackedCompaction, String> {
        let request = {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            let existing = state.compactions.get(&session);
            if existing.is_some_and(|pending| !pending.conversation.same_attachment(conversation)) {
                return Err("A compaction is still queued for the previous conversation".into());
            }
            let existing = existing.map(|pending| pending.request);
            let request = existing.unwrap_or_else(Uuid::new_v4);
            if tracked {
                state
                    .compaction_receipts
                    .retain(|_, receipt| receipt.created.elapsed() < RECEIPT_LIFETIME);
            }
            if tracked && !state.compaction_receipts.contains_key(&request) {
                if state.compaction_receipts.len() >= MAX_RECEIPTS {
                    return Err("Too many tracked compactions are pending or recent".into());
                }
                state.compaction_receipts.insert(
                    request,
                    Receipt {
                        created: Instant::now(),
                        session,
                        owner: conversation.compaction_owner(),
                        state: CompactionState::Queued,
                    },
                );
            }
            if existing.is_some() {
                if tracked && let Some(receipt) = state.compaction_receipts.get_mut(&request) {
                    receipt.created = Instant::now();
                }
                return Ok(reply(
                    session,
                    tracked.then_some(request),
                    CompactionState::Queued,
                ));
            }
            state.compactions.insert(
                session,
                Pending {
                    conversation: conversation.clone(),
                    request,
                    deferred: false,
                },
            );
            request
        };
        let (result, _) = self.try_compact(host, session, conversation, limited, request);
        self.record_compaction(session, request, &result);
        if matches!(result, Ok(CompactionState::Queued)) {
            // The actor may have become idle during our first attempt.
            self.wake();
        }
        if tracked {
            let state = tracked_state(&result);
            return Ok(reply(session, Some(request), state));
        }
        result.map(|state| reply(session, None, state))
    }

    fn record_compaction(
        &self,
        session: TerminalId,
        request: Uuid,
        result: &Result<CompactionState, String>,
    ) {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(receipt) = state.compaction_receipts.get_mut(&request) {
            receipt.state = tracked_state(result);
        }
        if matches!(result, Ok(CompactionState::Queued)) {
            if let Some(pending) = state.compactions.get_mut(&session) {
                pending.deferred = true;
            }
        } else {
            state.compactions.remove(&session);
        }
    }

    pub(super) fn deliver_compactions(
        &self,
        host: &SharedHost,
        limits: &BTreeMap<TerminalId, UsageLimit>,
    ) -> Option<u64> {
        let pending: Vec<_> = self
            .0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .compactions
            .iter()
            .filter(|(_, pending)| pending.deferred)
            .map(|(&session, pending)| (session, pending.clone()))
            .collect();
        let mut due = None;
        for (session, pending) in pending {
            let Some((result, wait)) = super::with_control(host, || {
                self.try_compact(
                    host,
                    session,
                    &pending.conversation,
                    limits.contains_key(&session),
                    pending.request,
                )
            }) else {
                // Upgrade-fence release wakes the shared courier; otherwise
                // retry with its normal bounded backoff.
                return Some(super::super::now().saturating_add(super::RETRY.as_millis() as u64));
            };
            self.record_compaction(session, pending.request, &result);
            if matches!(result, Ok(CompactionState::Queued)) {
                due = due.into_iter().chain(wait).min();
                continue;
            }
            match result {
                Ok(CompactionState::Unconfirmed(reason)) | Err(reason) => {
                    tracing::warn!(%session, %reason, "queued compaction could not be confirmed");
                }
                _ => {}
            }
        }
        due
    }

    fn try_compact(
        &self,
        host: &SharedHost,
        session: TerminalId,
        conversation: &Conversation,
        limited: bool,
        request: Uuid,
    ) -> (Result<CompactionState, String>, Option<u64>) {
        use CompactionState::{Queued, Submitted, Unconfirmed, Unsupported};
        let held = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            if !state.chats.get(&session).is_some_and(|record| {
                record.provider_running() && record.handle.same_attachment(conversation)
            }) {
                return (
                    Err("The session's provider is no longer running".into()),
                    None,
                );
            }
            state.held_followups.contains_key(&session)
        };
        if conversation.compact_support() == Some(false) {
            return (Ok(Unsupported), None);
        }
        if limited || !conversation.takes_courier() {
            return (Ok(Queued), None);
        }
        if held && let Some(until) = self.yields(session, super::super::now()) {
            return (Ok(Queued), Some(until));
        }
        let result = match conversation.try_tracked_compaction(request) {
            Ok(IdleCompaction::Submitted) => Ok(Submitted),
            Ok(IdleCompaction::Busy) => Ok(Queued),
            Ok(IdleCompaction::Retry) => {
                return (
                    Ok(Queued),
                    Some(super::super::now().saturating_add(super::RETRY.as_millis() as u64)),
                );
            }
            Ok(IdleCompaction::Unsupported) => Ok(Unsupported),
            Err(Rejection::Refused(reason)) => Err(reason),
            Err(Rejection::Unacknowledged(reason)) => Ok(Unconfirmed(reason)),
        };
        if matches!(result, Ok(Submitted | Unconfirmed(_))) {
            super::super::given_work(host, session);
        }
        (result, None)
    }
}

// Even an uncertain actor admission may later produce a receipt. Only
// retry admission when the actor proved the command had no effects.
fn tracked_state(result: &Result<CompactionState, String>) -> CompactionState {
    match result {
        Ok(CompactionState::Unconfirmed(_)) => CompactionState::Submitted,
        Ok(state) => state.clone(),
        Err(reason) => CompactionState::Failed(reason.clone()),
    }
}
