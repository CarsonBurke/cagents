use super::{Handle, SharedHost, caller, directory, registry, resolve, routing_lineages};
use crate::{
    agent_execution::{Authority, Compaction, CompactionState, Lineage, directory::UsageLimit},
    chat::runtime::{Handle as Conversation, IdleCompaction, Rejection},
    model::TerminalId,
};
use std::collections::{BTreeMap, btree_map::Entry};

/// A compaction belongs to the exact live conversation the caller selected.
#[derive(Clone)]
pub(super) struct Pending {
    conversation: Conversation,
    /// The requesting connection has finished its first attempt. Only the
    /// courier can dispatch after that, so a second request coalesces.
    deferred: bool,
}

pub(in crate::host::agent_execution) fn compact(
    host: &SharedHost,
    authority: &Authority,
    name: &str,
) -> Result<Compaction, String> {
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
        return Ok(Compaction {
            session,
            state: CompactionState::Unsupported,
        });
    };
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
    courier
        .request_compaction(host, session, conversation, limited)
        .map(|state| Compaction { session, state })
}

impl Handle {
    fn request_compaction(
        &self,
        host: &SharedHost,
        session: TerminalId,
        conversation: &Conversation,
        limited: bool,
    ) -> Result<CompactionState, String> {
        {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            match state.compactions.entry(session) {
                Entry::Occupied(_) => return Ok(CompactionState::Queued),
                Entry::Vacant(entry) => {
                    entry.insert(Pending {
                        conversation: conversation.clone(),
                        deferred: false,
                    });
                }
            }
        }
        let (result, _) = self.try_compact(host, session, conversation, limited);
        {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            if matches!(result, Ok(CompactionState::Queued)) {
                if let Some(pending) = state.compactions.get_mut(&session) {
                    pending.deferred = true;
                }
            } else {
                state.compactions.remove(&session);
            }
        }
        if matches!(result, Ok(CompactionState::Queued)) {
            // The actor may have become idle during our first attempt.
            self.wake();
        }
        result
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
            .map(|(&session, pending)| (session, pending.conversation.clone()))
            .collect();
        let mut due = None;
        for (session, conversation) in pending {
            let Some((result, wait)) = super::with_control(host, || {
                self.try_compact(host, session, &conversation, limits.contains_key(&session))
            }) else {
                // Upgrade-fence release wakes the shared courier; otherwise
                // retry with its normal bounded backoff.
                return Some(super::super::now().saturating_add(super::RETRY.as_millis() as u64));
            };
            if matches!(result, Ok(CompactionState::Queued)) {
                due = due.into_iter().chain(wait).min();
                continue;
            }
            self.0
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .compactions
                .remove(&session);
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
        let result = match conversation.try_idle_compact() {
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
