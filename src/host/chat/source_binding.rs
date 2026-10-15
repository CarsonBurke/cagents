//! Bounded, positive backend publication history. Unknown is a fence, including
//! an accepted restart whose helper reply was lost. Retirement never erases it.
use super::*;
use crate::terminal_ipc::{ChatSourceBinding, TerminalHandoffReceipt};
use std::collections::VecDeque;

const CAPACITY: usize = 1024;
#[derive(Default)]
pub(in crate::host) struct History {
    entries: BTreeMap<TerminalId, Entry>,
    order: VecDeque<TerminalId>,
    #[cfg(test)]
    lose_next_return_start_reply: bool,
}
/// A bounded history slot held across an off-host operation. Dropping a caller
/// never evicts its pending phase; the Entry itself retains that phase.
#[derive(Clone, Debug)]
pub(in crate::host) struct Reservation {
    id: TerminalId,
    pin: Arc<()>,
}
struct Entry {
    pin: Arc<()>,
    instance: uuid::Uuid,
    resource_epoch: uuid::Uuid,
    unresolved: bool,
    pending_handoff: Option<PendingHandoff>,
    learned_from: Option<ChatSourceBinding>,
    known: Option<(
        chat::runtime::BackendIdentity,
        ChatSourceBinding,
        Option<uuid::Uuid>,
    )>,
}
struct PendingHandoff {
    intent: ChatSourceBinding,
    admitted: ChatSourceBinding,
    witness: chat::runtime::BackendIdentity,
    owner: Handle,
    token: Option<uuid::Uuid>,
    original_pair: Option<uuid::Uuid>,
    created_pair: Option<uuid::Uuid>,
    backend_closed: bool,
    returned: Option<ReturnTarget>,
}
#[derive(Clone)]
struct ReturnTarget {
    reservation: Reservation,
    source: crate::model::TerminalSession,
    attempt: Option<crate::terminal::TerminalStartAttempt>,
    published: Option<(crate::terminal_ipc::TerminalDescriptor, crate::terminal::TerminalHandle)>,
}
impl History {
    fn ensure(&mut self, id: TerminalId) -> Result<(), ProtocolError> {
        if self.entries.contains_key(&id) { return Ok(()); }
        if self.entries.len() >= CAPACITY {
            let position = self.order.iter().position(|candidate| {
                self.entries.get(candidate).is_some_and(|entry| {
                    entry.pending_handoff.is_none() && Arc::strong_count(&entry.pin) == 1
                })
            }).ok_or_else(|| unavailable("Native history capacity is reserved by exact pending operations; no provider was changed"))?;
            let old = self.order.remove(position).expect("selected bounded history entry");
            self.entries.remove(&old);
        }
        self.order.push_back(id);
        self.entries.insert(id, Entry {
            pin: Arc::new(()), instance: uuid::Uuid::nil(), resource_epoch: uuid::Uuid::new_v4(),
            unresolved: false, pending_handoff: None, learned_from: None, known: None,
        });
        Ok(())
    }
    fn pin_existing(&self, id: TerminalId) -> Result<Reservation, ProtocolError> {
        let entry = self.entries.get(&id).ok_or_else(|| unavailable("Native publication history is unknown or evicted"))?;
        Ok(Reservation { id, pin: entry.pin.clone() })
    }
    fn matches_reservation(&self, reservation: &Reservation) -> bool {
        self.entries.get(&reservation.id).is_some_and(|entry| Arc::ptr_eq(&entry.pin, &reservation.pin))
    }
    /// Reserve before any provider mutation. A second operation cannot replace
    /// an exact in-flight slot or its retained ambiguous handoff phase.
    pub(in crate::host) fn mutating(&mut self, id: TerminalId) -> Result<Reservation, ProtocolError> {
        self.ensure(id)?;
        let entry = &self.entries[&id];
        if entry.pending_handoff.is_some() || Arc::strong_count(&entry.pin) != 1 {
            return Err(unavailable("An exact native operation already reserves this resource; no provider was changed"));
        }
        let reservation = self.pin_existing(id)?;
        self.mutating_reserved(&reservation)?;
        Ok(reservation)
    }
    pub(in crate::host) fn mutating_reserved(&mut self, reservation: &Reservation) -> Result<(), ProtocolError> {
        if !self.matches_reservation(reservation) { return Err(unavailable("The exact native publication reservation changed")); }
        let entry = self.entries.get_mut(&reservation.id).expect("same lock");
        if entry.pending_handoff.is_some() { return Err(unavailable("A retained handoff still owns this native resource")); }
        entry.resource_epoch = uuid::Uuid::new_v4();
        entry.known = None; entry.learned_from = None; entry.unresolved = true;
        Ok(())
    }
    pub(in crate::host) fn install_reserved(&mut self, reservation: &Reservation, instance: uuid::Uuid) -> Result<(), ProtocolError> {
        if !self.matches_reservation(reservation) { return Err(unavailable("The native publication reservation changed before installation")); }
        let entry = self.entries.get_mut(&reservation.id).expect("same lock");
        if entry.pending_handoff.is_some() { return Err(unavailable("A pending exact handoff cannot be overwritten by native installation")); }
        entry.instance = instance; entry.resource_epoch = uuid::Uuid::new_v4();
        entry.unresolved = false; entry.learned_from = None; entry.known = None;
        Ok(())
    }
    #[cfg(test)]
    pub(in crate::host) fn installed(&mut self, id: TerminalId, instance: uuid::Uuid) -> Result<(), ProtocolError> {
        let reservation = self.mutating(id)?;
        self.install_reserved(&reservation, instance)
    }
    pub(super) fn retain_resource_epoch(&mut self, id: TerminalId) -> Result<uuid::Uuid, ProtocolError> {
        self.ensure(id)?;
        Ok(self.entries[&id].resource_epoch)
    }
    pub(super) fn resource_epoch(&self, id: TerminalId) -> Option<uuid::Uuid> {
        self.entries.get(&id).map(|entry| entry.resource_epoch)
    }
    #[cfg(test)]
    pub(in crate::host) fn pin_capacity_for_test(&mut self) -> Vec<Reservation> {
        let mut reservations = self.entries.keys().map(|id| self.pin_existing(*id).unwrap()).collect::<Vec<_>>();
        while self.entries.len() < CAPACITY {
            reservations.push(self.mutating(TerminalId::new()).unwrap());
        }
        reservations
    }
    #[cfg(test)]
    pub(in crate::host) fn lose_next_return_start_reply_for_test(&mut self) {
        self.lose_next_return_start_reply = true;
    }
    #[cfg(test)]
    pub(in crate::host) fn return_attempt_for_test(&self, id: TerminalId) -> Option<crate::terminal::TerminalStartAttempt> {
        self.entries.get(&id)?.pending_handoff.as_ref()?.returned.as_ref()?.attempt.clone()
    }
    #[cfg(test)]
    pub(in crate::host) fn pressure_for_test(&mut self) {
        for _ in 0..CAPACITY * 2 {
            self.installed(TerminalId::new(), uuid::Uuid::new_v4()).unwrap();
        }
        assert!(self.entries.len() <= CAPACITY);
    }
    pub(super) fn observed(
        &mut self,
        id: TerminalId,
        instance: uuid::Uuid,
        thread: Option<String>,
        revision: Option<u64>,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(&id).filter(|e| e.instance == instance) else {
            return false;
        };
        let Some((witness, binding, _)) = &entry.known else {
            return false;
        };
        let changed = binding.source.launch.session_id != thread
            || revision != Some(witness.conversation_revision());
        if !changed {
            return false;
        }
        // Exactly one first confirmation may be learned under fresh admission.
        // Any missing revision, reset, second identity or same-final-thread ABA
        // revokes old receipts immediately at producer notification.
        let first = binding.source.launch.session_id.is_none()
            && witness.conversation_revision().checked_add(1) == revision;
        if !first {
            entry.known = None;
            entry.learned_from = None;
        }
        true
    }
    fn invalidate(&mut self, id: TerminalId, instance: uuid::Uuid) -> Result<(), ProtocolError> {
        let entry = self
            .entries
            .get_mut(&id)
            .filter(|e| e.instance == instance)
            .ok_or_else(|| {
                unavailable(
                    "Native backend publication history is unavailable; no owner was changed",
                )
            })?;
        entry.resource_epoch = uuid::Uuid::new_v4();
        entry.known = None;
        entry.learned_from = None;
        entry.unresolved = true;
        Ok(())
    }
    fn publish(
        &mut self,
        source: Session,
        instance: uuid::Uuid,
        witness: chat::runtime::BackendIdentity,
        paired: Option<uuid::Uuid>,
        resolve: bool,
    ) -> Result<ChatSourceBinding, ProtocolError> {
        let entry = self
            .entries
            .get_mut(&source.id)
            .filter(|e| e.instance == instance)
            .ok_or_else(|| {
                unavailable("Native backend publication history was replaced or evicted")
            })?;
        if entry.unresolved && !resolve {
            return Err(unavailable(
                "The exact backend restart remains unresolved; retry its original operation before returning to a terminal",
            ));
        }
        if let Some((previous, binding, previous_pair)) = &entry.known {
            if *previous == witness && binding.source == source && *previous_pair == paired {
                return Ok(binding.clone());
            }
        }
        entry.learned_from = entry
            .known
            .as_ref()
            .and_then(|(previous, binding, previous_pair)| {
                (previous.first_confirmation(&witness)
                    && *previous_pair == paired
                    && first_thread_learning(&binding.source, &source))
                .then(|| binding.clone())
            });
        // Even a failed restart resolved back to A mints fresh authority. An
        // earlier A intent never learns that no intervening backend existed.
        let binding = ChatSourceBinding {
            source,
            execution_instance: instance,
            backend: uuid::Uuid::new_v4(),
        };
        entry.known = Some((witness, binding.clone(), paired));
        entry.unresolved = false;
        Ok(binding)
    }
    fn authorizes_intent(&self, binding: &ChatSourceBinding) -> bool {
        self.entries.get(&binding.source.id).is_some_and(|entry| {
            !entry.unresolved
                && entry.instance == binding.execution_instance
                && entry
                    .known
                    .as_ref()
                    .is_some_and(|(_, issued, _)| issued == binding)
        }) || self.entries.get(&binding.source.id).is_some_and(|entry| {
            !entry.unresolved
                && entry.instance == binding.execution_instance
                && entry.known.is_some()
                && entry.learned_from.as_ref() == Some(binding)
        })
    }
    pub(in crate::host) fn validates(&self, binding: &ChatSourceBinding) -> bool {
        self.entries.get(&binding.source.id).is_some_and(|entry| {
            !entry.unresolved
                && entry.pending_handoff.is_none()
                && entry.instance == binding.execution_instance
                && entry
                    .known
                    .as_ref()
                    .is_some_and(|(_, issued, _)| issued == binding)
        })
    }
}

fn first_thread_learning(before: &Session, after: &Session) -> bool {
    if before.launch.session_id.is_some() || after.launch.session_id.is_none() {
        return false;
    }
    let mut promoted = before.clone();
    promoted.launch.session_id = after.launch.session_id.clone();
    promoted.native_continuity = after.native_continuity.clone();
    promoted == *after
}

// Snapshot publication may own the actor state mutex across filesystem I/O.
// A descriptor sampled under HostLock must never wait for that publication.
fn try_source_descriptor(record: &Record) -> Result<Session, ProtocolError> {
    let identity = record.handle.try_close_identity().map_err(unavailable)?;
    let mut source = record.session.clone();
    source.launch.session_id = identity.session_id;
    source.native_continuity = identity.native_continuity;
    Ok(source)
}

/// Must run while the caller holds the exact per-chat transition. No private
/// helper RPC or provider inspection occurs under HostLock.
pub(super) fn refresh(
    host: &SharedHost,
    id: TerminalId,
    resolve: bool,
) -> Result<ChatSourceBinding, ProtocolError> {
    let (handle, instance, proof) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get(&id)
            .ok_or_else(|| unavailable("Native backend no longer resident"))?;
        (
            record.handle.clone(),
            record.execution_instance,
            state.proof().clone(),
        )
    };
    let witness = handle.backend_identity(&proof).map_err(unavailable)?;
    if !witness.valid() || instance.is_nil() {
        return Err(unavailable("Native backend evidence is incomplete"));
    }
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let record = state
        .chats
        .get(&id)
        .filter(|r| r.execution_instance == instance && r.handle.same_actor(&handle))
        .ok_or_else(|| unavailable("Native actor changed while authenticating its backend"))?;
    if !handle.backend_identity_is_current(&witness) {
        return Err(unavailable(
            "Provider identity changed while authenticating its backend; retry the current pane",
        ));
    }
    let mut source = try_source_descriptor(record)?;
    source.launch.session_id = witness.conversation();
    validate_proof(source.proof.as_ref(), state.proof())?;
    let paired = record
        .paired_terminal
        .as_ref()
        .map(|worker| worker.execution_instance());
    let binding = state
        .native_sources
        .publish(source, instance, witness, paired, resolve)?;
    state.chats.get_mut(&id).expect("same lock").source_binding = Some(binding.clone());
    Ok(binding)
}

/// Publish uncertainty BEFORE crossing a provider-mutating RPC, including
/// repeated calls inspecting an accepted but not yet acknowledged restart.
pub(super) fn restarting(
    host: &SharedHost,
    id: TerminalId,
    handle: &Handle,
) -> Result<Reservation, ProtocolError> {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let instance = state
        .chats
        .get(&id)
        .filter(|r| r.handle.same_actor(handle))
        .map(|r| r.execution_instance)
        .ok_or_else(|| unavailable("Native actor changed before backend restart"))?;
    let reservation = state.native_sources.mutating(id)?;
    state.native_sources.entries.get_mut(&id).expect("reserved").instance = instance;
    state.chats.get_mut(&id).expect("same lock").source_binding = None;
    Ok(reservation)
}

/// Invalidate the old publication at every actual pair mutation, including
/// pair B which publishes and retires before the next UI observation. Repeating
/// the same publication is harmless; a cleared/replaced pair never revives A.
pub(in crate::host) fn pair_changed(state: &mut HostState, id: TerminalId) {
    let Some(record) = state.chats.get(&id) else {
        return;
    };
    let paired = record
        .paired_terminal
        .as_ref()
        .map(|worker| worker.execution_instance());
    let terminal = record.session.terminal.clone();
    let instance = record.execution_instance;
    let Some(entry) = state
        .native_sources
        .entries
        .get_mut(&id)
        .filter(|entry| entry.instance == instance)
    else {
        return;
    };
    if entry.known.as_ref().is_some_and(|(_, binding, old_pair)| {
        *old_pair != paired || binding.source.terminal != terminal
    }) {
        entry.resource_epoch = uuid::Uuid::new_v4();
        entry.known = None;
        entry.learned_from = None;
        state.chats.get_mut(&id).expect("same lock").source_binding = None;
    }
}

/// Private continuation proof captured under the lifecycle claim BEFORE the
/// handoff's own pair mutation. It is never supplied by a client.
pub(super) struct Continuation {
    publication: Reservation,
    returned: Option<ReturnTarget>,
    instance: uuid::Uuid,
    witness: chat::runtime::BackendIdentity,
    handle: Handle,
    claim: Option<uuid::Uuid>,
}
pub(super) fn continuation(
    host: &SharedHost,
    binding: &ChatSourceBinding,
) -> Result<Continuation, ProtocolError> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let entry = state
        .native_sources
        .entries
        .get(&binding.source.id)
        .filter(|entry| entry.instance == binding.execution_instance)
        .ok_or_else(|| unavailable("The original backend publication was evicted"))?;
    let witness = if let Some(pending) = &entry.pending_handoff {
        if pending.admitted != *binding {
            return Err(unavailable("Another captured handoff retains this source"));
        }
        pending.witness.clone()
    } else {
        if !state.native_sources.authorizes_intent(binding) {
            return Err(unavailable(
                "The source changed before its own pair transition",
            ));
        }
        entry
            .known
            .as_ref()
            .expect("validated publication")
            .0
            .clone()
    };
    let handle = state
        .chats
        .get(&binding.source.id)
        .filter(|record| record.execution_instance == binding.execution_instance)
        .ok_or_else(|| unavailable("The original backend is no longer resident"))?
        .handle
        .clone();
    Ok(Continuation {
        publication: state.native_sources.pin_existing(binding.source.id)?,
        returned: entry.pending_handoff.as_ref().and_then(|pending| pending.returned.clone()),
        instance: binding.execution_instance,
        witness,
        handle,
        claim: None,
    })
}

/// Retry only a privately retained original operation. An intervening native
/// owner or unrelated pair refuses; absence/eviction never reconstructs proof.
pub(super) fn pending_admitted(
    host: &SharedHost,
    client: ClientId,
    expected: &ChatSourceBinding,
) -> Result<Option<ChatSourceBinding>, ProtocolError> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let Some(entry) = state.native_sources.entries.get(&expected.source.id) else {
        return Ok(None);
    };
    let Some(pending) = &entry.pending_handoff else {
        return Ok(None);
    };
    validate_proof(expected.source.proof.as_ref(), state.proof())?;
    let record = attached(&state, client, expected.source.id)?;
    let pair = record
        .paired_terminal
        .as_ref()
        .map(|worker| worker.execution_instance());
    if pending.intent != *expected
        || entry.instance != expected.execution_instance
        || record.execution_instance != entry.instance
        || !record.handle.same_actor(&pending.owner)
        || (pair != pending.original_pair
            && !(pending.created_pair.is_some() && pair == pending.created_pair))
    {
        return Err(unavailable(
            "This pending handoff belongs to another exact backend or paired worker; no owner was changed",
        ));
    }
    Ok(Some(pending.admitted.clone()))
}
/// A worker was retained before pair publication/RPC. Recover only that exact
/// private creation, including failure between spawn and local pair insertion.
pub(super) fn restore_created_pair(
    host: &SharedHost,
    client: ClientId,
    expected: &ChatSourceBinding,
) -> Result<(), ProtocolError> {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let Some(pending) = state
        .native_sources
        .entries
        .get(&expected.source.id)
        .and_then(|entry| entry.pending_handoff.as_ref())
    else {
        return Ok(());
    };
    if pending.intent != *expected {
        return Err(unavailable("Another handoff operation retains this source"));
    }
    let Some(created) = pending.created_pair else {
        return Ok(());
    };
    let record = attached(&state, client, expected.source.id)?;
    if !record.handle.same_actor(&pending.owner)
        || record.execution_instance != pending.admitted.execution_instance
    {
        return Err(unavailable("The retained handoff backend was replaced"));
    }
    if record
        .paired_terminal
        .as_ref()
        .is_some_and(|worker| worker.execution_instance() == created)
    {
        return Ok(());
    }
    if record.paired_terminal.is_some() || pending.original_pair.is_some() {
        return Err(unavailable(
            "The pending handoff has a different original pair; no source was rebound",
        ));
    }
    let worker = state
        .terminals
        .get(&expected.source.id)
        .filter(|record| record.handle.execution_instance() == created)
        .ok_or_else(|| {
            unavailable(
                "The exact created handoff worker is unavailable; no replacement was adopted",
            )
        })?
        .handle
        .clone();
    let descriptor = state.descriptor(client, expected.source.id)?;
    let record = state
        .chats
        .get_mut(&expected.source.id)
        .expect("same lock exact owner");
    record.session.terminal = Some(descriptor.session);
    record.paired_terminal = Some(worker);
    pair_changed(&mut state, expected.source.id);
    Ok(())
}

pub(super) fn remember_created_pair(
    host: &SharedHost,
    id: TerminalId,
    continuation: &Continuation,
    worker: &crate::terminal::TerminalHandle,
) -> Result<(), ProtocolError> {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let pending = state
        .native_sources
        .entries
        .get_mut(&id)
        .filter(|entry| entry.instance == continuation.instance)
        .and_then(|entry| entry.pending_handoff.as_mut())
        .filter(|pending| {
            pending.token == continuation.claim && pending.owner.same_actor(&continuation.handle)
        })
        .ok_or_else(|| {
            unavailable("The claimed handoff changed before recording its new worker")
        })?;
    pending.created_pair = Some(worker.execution_instance());
    Ok(())
}

pub(super) fn remember_released_pair(
    state: &mut HostState,
    id: TerminalId,
    continuation: &Continuation,
    worker: &crate::terminal::TerminalHandle,
) -> Result<(), ProtocolError> {
    let pending = state
        .native_sources
        .entries
        .get_mut(&id)
        .filter(|entry| entry.instance == continuation.instance)
        .and_then(|entry| entry.pending_handoff.as_mut())
        .filter(|pending| {
            pending.token == continuation.claim && pending.owner.same_actor(&continuation.handle)
        })
        .ok_or_else(|| {
            unavailable("The claimed handoff changed while releasing its exact worker")
        })?;
    let instance = worker.execution_instance();
    if pending.original_pair != Some(instance) && pending.created_pair != Some(instance) {
        return Err(unavailable(
            "The released worker belongs to another operation; its successor was preserved",
        ));
    }
    pending.original_pair = None;
    pending.created_pair = None;
    Ok(())
}

/// Publish pending ownership before asking the actual backend to linearize a
/// handoff. Lost replies retain this exact operation and revoke old receipts.
/// No host lock survives the actor/helper call.
pub(super) fn claim_continuation(
    host: &SharedHost,
    intent: &ChatSourceBinding,
    binding: &ChatSourceBinding,
    continuation: &mut Continuation,
) -> Result<(), ProtocolError> {
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !state.native_sources.matches_reservation(&continuation.publication) {
            return Err(unavailable("The exact source publication reservation changed before its claim"));
        }
        let original_pair = state
            .chats
            .get(&binding.source.id)
            .filter(|record| {
                record.execution_instance == continuation.instance
                    && record.handle.same_actor(&continuation.handle)
            })
            .ok_or_else(|| unavailable("The original source owner changed before its claim"))?
            .paired_terminal
            .as_ref()
            .map(|worker| worker.execution_instance());
        let entry = state
            .native_sources
            .entries
            .get_mut(&binding.source.id)
            .filter(|entry| entry.instance == continuation.instance)
            .ok_or_else(|| {
                unavailable("The backend publication was evicted; no owner was changed")
            })?;
        match &entry.pending_handoff {
            Some(pending)
                if pending.intent != *intent
                    || pending.admitted != *binding
                    || pending.witness != continuation.witness
                    || !pending.owner.same_actor(&continuation.handle) =>
            {
                return Err(unavailable(
                    "Another exact handoff operation retains this backend",
                ));
            }
            Some(_) => {}
            None => {
                entry.resource_epoch = uuid::Uuid::new_v4();
                entry.pending_handoff = Some(PendingHandoff {
                    intent: intent.clone(),
                    admitted: binding.clone(),
                    witness: continuation.witness.clone(),
                    owner: continuation.handle.clone(),
                    token: None,
                    original_pair,
                    created_pair: None,
                    backend_closed: false,
                    returned: continuation.returned.clone(),
                });
            }
        }
    }
    let already_closed = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.native_sources.entries[&binding.source.id].pending_handoff.as_ref()
            .filter(|pending| pending.intent == *intent && pending.admitted == *binding && pending.backend_closed)
            .and_then(|pending| pending.token)
    };
    if let Some(token) = already_closed {
        continuation.claim = Some(token);
        return Ok(());
    }
    let claim = match continuation
        .handle
        .claim_backend_source(&continuation.witness)
        .map_err(unavailable)?
    {
        chat::runtime::BackendSourceClaim::Granted(token) => token,
        chat::runtime::BackendSourceClaim::Refused(reason) => {
            // Only the authenticated owner can prove non-admission. Transport
            // uncertainty above deliberately keeps the pending operation.
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(entry) = state
                .native_sources
                .entries
                .get_mut(&binding.source.id)
                .filter(|entry| entry.instance == continuation.instance)
                && entry.pending_handoff.as_ref().is_some_and(|pending| {
                    pending.intent == *intent
                        && pending.admitted == *binding
                        && pending.token.is_none()
                })
            {
                entry.pending_handoff = None;
                if let Some(record) = state.chats.get_mut(&binding.source.id).filter(|record| {
                    record.execution_instance == continuation.instance
                        && record.handle.same_actor(&continuation.handle)
                }) {
                    record.returning_terminal = false;
                }
            }
            return Err(unavailable(reason));
        }
    };
    continuation.claim = Some(claim);
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let entry = state
        .native_sources
        .entries
        .get_mut(&binding.source.id)
        .filter(|entry| entry.instance == continuation.instance)
        .ok_or_else(|| {
            unavailable("The claimed backend history changed; its exact owner remains retained")
        })?;
    let pending = entry
        .pending_handoff
        .as_mut()
        .filter(|pending| pending.intent == *intent && pending.admitted == *binding)
        .ok_or_else(|| {
            unavailable("The claimed backend operation changed; its exact owner remains retained")
        })?;
    pending.token = Some(claim);
    Ok(())
}
pub(super) fn claimed_publication(
    state: &HostState,
    binding: &ChatSourceBinding,
    continuation: &Continuation,
) -> bool {
    state
        .native_sources
        .entries
        .get(&binding.source.id)
        .is_some_and(|entry| {
            !entry.unresolved
                && entry.instance == continuation.instance
                && entry.pending_handoff.as_ref().is_some_and(|pending| {
                    pending.token == continuation.claim
                        && pending.token.is_some()
                        && pending.owner.same_actor(&continuation.handle)
                        && pending.witness == continuation.witness
                })
                && entry.known.as_ref().is_some_and(|(witness, current, _)| {
                    *current == *binding && *witness == continuation.witness
                })
        })
}
pub(super) fn update_claimed_source(
    continuation: &Continuation,
    source: Session,
    pair: Option<crate::terminal::holder::Record>,
) -> Result<(), String> {
    continuation.handle.update_claimed_native_source(
        &continuation.witness,
        continuation.claim.ok_or("No exact backend source claim")?,
        source,
        None,
        pair,
    )
}
pub(super) fn finish_continuation(
    host: &SharedHost,
    id: TerminalId,
    continuation: &Continuation,
) -> Result<(), ProtocolError> {
    let claim = continuation
        .claim
        .ok_or_else(|| unavailable("The exact handoff claim is unconfirmed"))?;
    continuation
        .handle
        .release_backend_source(&continuation.witness, claim)
        .map_err(unavailable)?;
    clear_continuation(host, id, continuation, claim)
}
pub(super) fn close_continuation(
    host: &SharedHost,
    id: TerminalId,
    continuation: &Continuation,
) -> Result<(), ProtocolError> {
    let claim = continuation
        .claim
        .ok_or_else(|| unavailable("The exact handoff claim is unconfirmed"))?;
    {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let pending = state.native_sources.entries.get(&id).and_then(|entry| entry.pending_handoff.as_ref())
            .filter(|pending| pending.token == Some(claim) && pending.owner.same_actor(&continuation.handle))
            .ok_or_else(|| unavailable("The exact close operation is unavailable"))?;
        if pending.backend_closed { return Ok(()); }
    }
    continuation.handle.close_backend_source(&continuation.witness, claim).map_err(unavailable)?;
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let pending = state.native_sources.entries.get_mut(&id).and_then(|entry| entry.pending_handoff.as_mut())
        .filter(|pending| pending.token == Some(claim) && pending.owner.same_actor(&continuation.handle))
        .ok_or_else(|| unavailable("The positive closed owner lost its exact retained operation"))?;
    pending.backend_closed = true;
    Ok(())
}
fn clear_continuation(
    host: &SharedHost,
    id: TerminalId,
    continuation: &Continuation,
    claim: uuid::Uuid,
) -> Result<(), ProtocolError> {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let entry = state
        .native_sources
        .entries
        .get_mut(&id)
        .filter(|entry| entry.instance == continuation.instance)
        .ok_or_else(|| {
            unavailable("The disposed handoff history changed; no receipt was issued")
        })?;
    if !entry
        .pending_handoff
        .as_ref()
        .is_some_and(|pending| pending.token == Some(claim))
    {
        return Err(unavailable(
            "The disposed handoff operation changed; no receipt was issued",
        ));
    }
    entry.pending_handoff = None;
    Ok(())
}

/// Reserve the exact future target before returning/claim/close. An ambiguous
/// operation keeps this same ID and slot through retry; no latest-ID discovery.
pub(super) fn reserve_return_target(
    host: &SharedHost, continuation: &mut Continuation, source: crate::model::TerminalSession,
) -> Result<crate::model::TerminalSession, ProtocolError> {
    if let Some(target) = &continuation.returned { return Ok(target.source.clone()); }
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    state.native_sources.ensure(source.id)?;
    let reservation = state.native_sources.pin_existing(source.id)?;
    continuation.returned = Some(ReturnTarget { reservation, source: source.clone(), attempt: None, published: None });
    Ok(source)
}
/// Unknown after an actual spawn attempt never means no provider. Reconcile
/// only the original owner's typed ticket, retaining it on every uncertainty.
pub(super) fn prepare_return_attempt(
    host: &SharedHost, id: TerminalId, continuation: &Continuation, source: crate::model::TerminalSession,
) -> Result<Option<(crate::terminal_ipc::TerminalDescriptor, crate::terminal::TerminalHandle)>, ProtocolError> {
    use crate::terminal::TerminalStartDisposition as Disposition;
    let (attempt, published) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let pending = state.native_sources.entries.get(&id).and_then(|entry| entry.pending_handoff.as_ref())
            .filter(|pending| pending.backend_closed && pending.token == continuation.claim && pending.owner.same_actor(&continuation.handle))
            .ok_or_else(|| unavailable("The exact closed source operation is unavailable"))?;
        let target = pending.returned.as_ref().filter(|target| target.source.id == source.id)
            .ok_or_else(|| unavailable("The exact attempted return target changed"))?;
        (target.attempt.clone(), target.published.clone())
    };
    if published.is_some() { return Ok(published); }
    if let Some(attempt) = &attempt {
        if attempt.terminal_id() != source.id || attempt.session() != &source {
            return Err(unavailable("The retained terminal attempt does not match the exact return recipe"));
        }
        let disposition = match attempt.disposition() {
            Disposition::Unknown => attempt.retry_cleanup(), // off HostLock, bounded existing queue only
            disposition => disposition,
        };
        match disposition {
            Disposition::Published => {
                let published = attempt.published().ok_or_else(|| unavailable("The exact published return worker is busy; no replacement was started"))?;
                remember_return_worker(host, id, continuation, published.0.clone(), published.1.clone())?;
                return Ok(Some(published));
            }
            Disposition::Pending | Disposition::Unknown => return Err(unavailable("The exact previous terminal start is unconfirmed; no second resume process was started")),
            Disposition::NoProvider | Disposition::FamilySettled => {}
        }
    }
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let pending = state.native_sources.entries.get_mut(&id).and_then(|entry| entry.pending_handoff.as_mut())
        .filter(|pending| pending.backend_closed && pending.token == continuation.claim && pending.owner.same_actor(&continuation.handle))
        .ok_or_else(|| unavailable("The exact closed source operation changed during start reconciliation"))?;
    let target = pending.returned.as_mut().filter(|target| target.source.id == source.id
        && target.attempt.as_ref().map(|attempt| attempt.id()) == attempt.as_ref().map(|attempt| attempt.id()))
        .ok_or_else(|| unavailable("The exact terminal start attempt was replaced"))?;
    // Only the original owner's positive NoProvider/family receipt above may
    // release an attempted ticket. Keep the same privately reserved target ID.
    target.attempt = None;
    target.source = source;
    Ok(None)
}
pub(super) fn mark_return_attempt(host: &SharedHost, id: TerminalId, continuation: &Continuation) -> Result<crate::terminal::TerminalStartAttempt, ProtocolError> {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    #[cfg(test)]
    let lose_start_reply = std::mem::take(&mut state.native_sources.lose_next_return_start_reply);
    let pending = state.native_sources.entries.get_mut(&id).and_then(|entry| entry.pending_handoff.as_mut())
        .filter(|pending| pending.backend_closed && pending.token == continuation.claim && pending.owner.same_actor(&continuation.handle))
        .ok_or_else(|| unavailable("The exact closed source operation is unavailable"))?;
    let target = pending.returned.as_mut().ok_or_else(|| unavailable("The exact return target was not reserved"))?;
    if target.attempt.is_some() { return Err(unavailable("The exact previous terminal start is unconfirmed; no second resume process was started")); }
    let attempt = crate::terminal::TerminalStartAttempt::new(target.source.clone());
    #[cfg(test)]
    if lose_start_reply { attempt.lose_worker_start_reply_for_test(); }
    target.attempt = Some(attempt.clone());
    Ok(attempt)
}
pub(super) fn remember_return_worker(
    host: &SharedHost, id: TerminalId, continuation: &Continuation,
    descriptor: crate::terminal_ipc::TerminalDescriptor, worker: crate::terminal::TerminalHandle,
) -> Result<(), ProtocolError> {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.terminals.get(&descriptor.session.id).is_none_or(|record| !record.handle.same_worker(&worker)) {
        return Err(unavailable("The exact returned worker changed before retention"));
    }
    let pending = state.native_sources.entries.get_mut(&id).and_then(|entry| entry.pending_handoff.as_mut())
        .filter(|pending| pending.backend_closed && pending.token == continuation.claim && pending.owner.same_actor(&continuation.handle))
        .ok_or_else(|| unavailable("The exact returned operation changed"))?;
    let target = pending.returned.as_mut().filter(|target| target.source.id == descriptor.session.id)
        .ok_or_else(|| unavailable("The returned worker belongs to another attempted target"))?;
    if target.attempt.as_ref().is_none_or(|attempt| attempt.worker_instance() != Some(worker.execution_instance())
        || attempt.published().is_none_or(|(issued, actual)| issued != descriptor || !actual.same_worker(&worker))) {
        return Err(unavailable("The returned worker has no matching exact successful start ticket"));
    }
    target.published = Some((descriptor, worker));
    Ok(())
}
pub(super) fn finish_closed_return(
    state: &mut HostState, id: TerminalId, continuation: &Continuation, receipt: &TerminalHandoffReceipt,
) -> Result<(), ProtocolError> {
    state.validate_issued_terminal_handoff(receipt)?;
    let pending = state.native_sources.entries.get(&id).and_then(|entry| entry.pending_handoff.as_ref())
        .filter(|pending| pending.backend_closed && pending.token == continuation.claim && pending.owner.same_actor(&continuation.handle))
        .ok_or_else(|| unavailable("The exact closed return operation changed before publication"))?;
    let target = pending.returned.as_ref().ok_or_else(|| unavailable("The exact returned resource reservation is unavailable"))?;
    if !state.native_sources.matches_reservation(&target.reservation)
        || target.published.as_ref().is_none_or(|(descriptor, worker)| descriptor != &receipt.terminal || worker.execution_instance() != receipt.terminal.execution_instance)
    { return Err(unavailable("The exact returned worker receipt does not match its reserved operation")); }
    state.native_sources.entries.get_mut(&id).expect("same lock").pending_handoff = None;
    Ok(())
}

/// Private close context. The caller must already hold this chat's lifecycle
/// transition and its own exact user/owner close admission. Capturing a pending
/// handoff is not authorization to close an otherwise unrelated session.
pub(in crate::host) struct PendingClose {
    intent: ChatSourceBinding,
    admitted: ChatSourceBinding,
    continuation: Continuation,
    closed: bool,
}
impl PendingClose {
    pub(in crate::host) fn admitted(&self) -> &Session { &self.admitted.source }
    pub(in crate::host) fn binding(&self) -> &ChatSourceBinding { &self.admitted }
    pub(in crate::host) fn owner(&self) -> &Handle { &self.continuation.handle }
    pub(in crate::host) fn witness(&self) -> &chat::runtime::BackendIdentity { &self.continuation.witness }
    pub(in crate::host) fn claim_token(&self) -> Option<uuid::Uuid> { self.continuation.claim }
    pub(in crate::host) fn close_original(&mut self, host: &SharedHost) -> Result<(), ProtocolError> {
        claim_continuation(host, &self.intent, &self.admitted, &mut self.continuation)?;
        close_continuation(host, self.admitted.source.id, &self.continuation)?;
        self.closed = true;
        Ok(())
    }
}
pub(in crate::host) fn capture_pending_close(host: &SharedHost, source: &Session) -> Result<Option<PendingClose>, ProtocolError> {
    let captured = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = state.native_sources.entries.get(&source.id) else { return Ok(None); };
        let Some(pending) = &entry.pending_handoff else { return Ok(None); };
        validate_proof(source.proof.as_ref(), state.proof())?;
        if pending.original_pair.is_some() || pending.created_pair.is_some()
            || pending.returned.as_ref().is_some_and(|target| target.attempt.is_some() || target.published.is_some()) {
            return Err(unavailable("This handoff retains attempted or paired terminal ownership; exact cleanup disposition is still required"));
        }
        if (source != &pending.intent.source && source != &pending.admitted.source)
            || state.chats.get(&source.id).is_none_or(|record| record.execution_instance != entry.instance || !record.handle.same_actor(&pending.owner)) {
            return Err(unavailable("The close does not name the exact retained handoff source"));
        }
        (pending.intent.clone(), pending.admitted.clone())
    };
    let continuation = continuation(host, &captured.1)?;
    Ok(Some(PendingClose { intent: captured.0, admitted: captured.1, continuation, closed: false }))
}

/// Requires the companion bound-close producer's sealed positive actual owner
/// receipt. Generic Accepted/completion/absence cannot call this CAS.
pub(in crate::host) fn release_pending_close(
    host: &SharedHost, pending: &PendingClose, proof: &super::bound_close::ClosedPending,
) -> Result<(), ProtocolError> {
    if !pending.closed || pending.claim_token().is_none() || !proof.authenticates(pending) {
        return Err(unavailable("The exact pending close has no authenticated positive owner disposition"));
    }
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let id = pending.admitted.source.id;
    if !state.native_sources.matches_reservation(&pending.continuation.publication)
        || state.chats.get(&id).is_some_and(|record| record.execution_instance != pending.admitted.execution_instance || !record.handle.same_actor(pending.owner())) {
        return Err(unavailable("The source changed before exact pending close retirement"));
    }
    let entry = state.native_sources.entries.get_mut(&id).expect("reserved exact slot");
    let matches = entry.pending_handoff.as_ref().is_some_and(|current| {
        entry.instance == pending.admitted.execution_instance
            && current.intent == pending.intent && current.admitted == pending.admitted
            && current.witness == *pending.witness() && current.owner.same_actor(pending.owner())
            && current.token == pending.claim_token() && current.backend_closed
            && current.original_pair.is_none() && current.created_pair.is_none()
            && current.returned.as_ref().is_none_or(|target| target.attempt.is_none() && target.published.is_none())
    });
    if !matches { return Err(unavailable("The exact pending phase changed; cleanup authority was retained")); }
    entry.pending_handoff = None;
    entry.known = None;
    entry.learned_from = None;
    entry.unresolved = true;
    entry.resource_epoch = uuid::Uuid::new_v4();
    if let Some(record) = state.chats.get_mut(&id) {
        record.source_binding = None;
        record.returning_terminal = false;
    }
    Ok(())
}

/// The operation's own paired source may change, but its actual backend and
/// confirmed conversation must remain exactly the one authenticated beforehand.
pub(super) fn complete_pair(
    host: &SharedHost,
    id: TerminalId,
    previous: &Continuation,
) -> Result<ChatSourceBinding, ProtocolError> {
    let binding = refresh(host, id, false)?;
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let known = state
        .native_sources
        .entries
        .get(&id)
        .and_then(|entry| entry.known.as_ref());
    if binding.execution_instance != previous.instance
        || known.is_none_or(|(witness, _, _)| witness != &previous.witness)
    {
        return Err(unavailable(
            "The actual provider backend changed during its terminal pairing; ownership was retained",
        ));
    }
    Ok(binding)
}

/// Admission compares fresh actual evidence under the same transition claim.
/// An unknown thread may learn its first authenticated identity on that exact
/// backend/pair; a known identity, changed pair or intervening restart never may.
pub(super) fn require(
    host: &SharedHost,
    client: ClientId,
    expected: &ChatSourceBinding,
) -> Result<ChatSourceBinding, ProtocolError> {
    let previous = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        validate_proof(expected.source.proof.as_ref(), state.proof())?;
        let record = attached(&state, client, expected.source.id)?;
        let current = try_source_descriptor(record)?;
        if current.launch.provider != expected.source.launch.provider
            || current.launch.recipe != expected.source.launch.recipe
            || expected
                .source
                .launch
                .session_id
                .as_ref()
                .is_some_and(|thread| current.launch.session_id.as_ref() != Some(thread))
        {
            return Err(unavailable(
                "The captured provider conversation changed; no owner was stopped",
            ));
        }
        if record.execution_instance != expected.execution_instance
            || !state.native_sources.authorizes_intent(expected)
        {
            return Err(unavailable(
                "The captured native backend changed; no provider or console was stopped",
            ));
        }
        state.native_sources.entries[&expected.source.id]
            .known
            .as_ref()
            .expect("validated publication")
            .clone()
    };
    let current = refresh(host, expected.source.id, false)?;
    if current == *expected {
        return Ok(current);
    }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let learned = state
        .native_sources
        .entries
        .get(&expected.source.id)
        .is_some_and(|entry| {
            entry.learned_from.as_ref() == Some(expected)
                || entry.known.as_ref().is_some_and(|(witness, _, pair)| {
                    previous.0.first_confirmation(witness) && previous.2 == *pair
                })
        });
    let mut promoted = expected.source.clone();
    promoted.launch.session_id = current.source.launch.session_id.clone();
    promoted.native_continuity = current.source.native_continuity.clone();
    if expected.source.launch.session_id.is_none()
        && current.source.launch.session_id.is_some()
        && learned
        && promoted == current.source
    {
        return Ok(current);
    }
    Err(unavailable(
        "The captured native backend changed; no provider or console was stopped",
    ))
}

/// Adds only authenticated metadata to the response produced for this request.
/// A backend not ready/inspectable grants no destructive authority.
pub(super) fn decorate(host: &SharedHost, response: &mut ServerResponse) {
    let id = match response {
        ServerResponse::ChatAttached { session, .. } => session.id,
        ServerResponse::ChatSnapshot { chat_id, .. }
        | ServerResponse::ChatUpdate { chat_id, .. } => *chat_id,
        _ => return,
    };
    let resident = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .contains_key(&id);
    if !resident {
        return;
    }
    let binding = refresh(host, id, false).ok();
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let instance = state
        .chats
        .get(&id)
        .map(|r| r.execution_instance)
        .unwrap_or_default();
    let binding = binding.filter(|_| {
        state
            .native_sources
            .entries
            .get(&id)
            .is_some_and(|entry| entry.pending_handoff.is_none())
    });
    match response {
        ServerResponse::ChatAttached {
            execution_instance,
            binding: out,
            ..
        }
        | ServerResponse::ChatSnapshot {
            execution_instance,
            binding: out,
            ..
        }
        | ServerResponse::ChatUpdate {
            execution_instance,
            binding: out,
            ..
        } => {
            *execution_instance = instance;
            *out = binding;
        }
        _ => unreachable!(),
    }
}

pub(in crate::host) fn validate_receipt(
    state: &HostState,
    receipt: &TerminalHandoffReceipt,
) -> Result<(), ProtocolError> {
    if receipt.terminal_native_epoch.is_nil()
        || state
            .native_sources
            .resource_epoch(receipt.terminal.session.id)
            != Some(receipt.terminal_native_epoch)
        || state.native_actor_startups.contains_key(&receipt.source.id)
        || state
            .native_actor_startups
            .contains_key(&receipt.terminal.session.id)
        || receipt.source_execution_instance != receipt.source_binding.execution_instance
        || !state.native_sources.validates(
            receipt
                .retained_binding
                .as_ref()
                .unwrap_or(&receipt.source_binding),
        )
    {
        return Err(unavailable(
            "The handoff's original native backend was superseded or its history is unavailable",
        ));
    }
    Ok(())
}

/// Old host tests insert a real owned actor directly instead of using the
/// production publication seam. Establish that seam explicitly, then require
/// genuine child/helper evidence; no timestamp, nil ID or synthetic process.
#[cfg(test)]
pub(in crate::host) fn fixture_binding(host: &SharedHost, id: TerminalId) -> ChatSourceBinding {
    {
        let mut state = host.lock().unwrap();
        if let Some(receipt) = state.chat_handoffs.get(&id).and_then(|h| h.issued()) {
            return receipt.source_binding.clone();
        }
        let instance = state.chats[&id].execution_instance;
        assert!(
            !instance.is_nil(),
            "fixture must publish an actual actor incarnation"
        );
        if !state.native_sources.entries.contains_key(&id) {
            state.native_sources.installed(id, instance).unwrap();
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match refresh(host, id, false) {
            Ok(binding) => return binding,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "fixture must own a concrete actual backend: {}",
                    error.message
                );
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// Exercises production publication/admission with real runtime owners. Only
/// test construction skips launching through the installed provider registry.
#[cfg(test)]
pub(crate) struct BackendFixture {
    host: SharedHost,
    id: TerminalId,
    handle: Handle,
}
#[cfg(test)]
impl BackendFixture {
    pub(crate) fn new(source: Session, handle: Handle, instance: uuid::Uuid) -> Self {
        let mut state = HostState::new(source.proof.clone().expect("authenticated fixture"));
        state.register_client(1);
        state.native_sources.installed(source.id, instance).unwrap();
        state.chats.insert(
            source.id,
            Record {
                execution_instance: instance,
                console_recipe: None,
                paired_terminal: None,
                handoff_receipt: None,
                source_binding: None,
                session: source.clone(),
                handle: handle.clone(),
                source_process: None,
                subscribers: BTreeMap::from([(1, false)]),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        Self {
            host: Arc::new(HostLock::new(state)),
            id: source.id,
            handle,
        }
    }
    pub(crate) fn capture(&self) -> Result<ChatSourceBinding, ProtocolError> {
        refresh(&self.host, self.id, false)
    }
    pub(crate) fn begin_restart(&self) {
        restarting(&self.host, self.id, &self.handle).unwrap();
    }
    pub(crate) fn resolve(&self) -> ChatSourceBinding {
        refresh(&self.host, self.id, true).unwrap()
    }
    pub(crate) fn retire(&self) {
        drop(self.host.lock().unwrap().chats.remove(&self.id));
    }
    pub(crate) fn validates(&self, binding: &ChatSourceBinding) -> bool {
        self.host.lock().unwrap().native_sources.validates(binding)
    }
    pub(crate) fn return_refuses(&self, binding: ChatSourceBinding) -> String {
        // Actual owning host dispatch, including its transition and attachment
        // admission. Refusal must happen before any provider/terminal teardown.
        process_inner(
            &self.host,
            1,
            ClientRequest::ChatReturnTerminal {
                chat_id: self.id,
                expected: binding,
            },
            None,
        )
        .err()
        .expect("stale captured intent must refuse")
        .message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::protocol::{Adapter, ProcessSpec, Step};
    use std::sync::Mutex;
    struct Idle;
    impl Adapter for Idle {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: chat::Command) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    struct Owner(Handle);
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.0.close();
        }
    }
    fn actor(source: &Session) -> Owner {
        actor_with(source, Box::new(Idle))
    }
    fn actor_with(source: &Session, adapter: Box<dyn Adapter>) -> Owner {
        actor_with_notify(source, adapter, |_| {})
    }
    fn actor_with_notify(
        source: &Session,
        adapter: Box<dyn Adapter>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Owner {
        let spec = ProcessSpec::from(&source.launch.recipe);
        let mut initial = Step::default();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, adapter, initial, store, None, notify).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while handle.provider_process().is_none() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        Owner(handle)
    }
    fn source(path: &std::path::Path) -> Session {
        Session {
            id: TerminalId::new(),
            proof: Some(HostSessionProof {
                host_session_id: "binding-test".into(),
                boot_id: crate::daemon::read_boot_id().unwrap(),
                host_incarnation: "binding-test".into(),
            }),
            launch: chat::Launch {
                provider: Provider::Omp,
                session_id: None,
                recipe: crate::model::LaunchRecipe {
                    program: "/bin/cat".into(),
                    arguments: vec![],
                    working_directory: path.into(),
                    environment: BTreeMap::new(),
                    unset_environment: Default::default(),
                },
            },
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
        }
    }
    #[test]
    fn pinned_history_capacity_refuses_new_mutation_and_same_id_replacement() {
        let mut history = History::default();
        let mut reservations = Vec::new();
        for _ in 0..CAPACITY {
            reservations.push(history.mutating(TerminalId::new()).unwrap());
        }
        let attempted = TerminalId::new();
        assert!(history.mutating(attempted).unwrap_err().message.contains("capacity is reserved"));
        assert!(!history.entries.contains_key(&attempted), "refused admission cannot publish a new resource");
        let original = reservations[0].id;
        let epoch = history.resource_epoch(original).unwrap();
        assert!(history.installed(original, uuid::Uuid::new_v4()).is_err(), "same-ID installation cannot overwrite an in-flight reservation");
        assert_eq!(history.resource_epoch(original), Some(epoch));
        assert_eq!(history.entries.len(), CAPACITY);
        drop(reservations.remove(0));
        let accepted = history.mutating(attempted).unwrap();
        assert_eq!(accepted.id, attempted);
        assert!(!history.entries.contains_key(&original), "only released completed/unknown history may be evicted");
        assert_eq!(history.entries.len(), CAPACITY);
    }

    #[test]
    fn owned_native_confirmation_error_revokes_previous_binding_before_frame_application() {
        use std::os::unix::fs::PermissionsExt;
        const EXACT: &str = "host::chat::source_binding::tests::owned_native_confirmation_error_revokes_previous_binding_before_frame_application";
        if chat::runtime::isolated_backend_contract(EXACT) { return; }
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let program = directory.path().join("codex");
        // A real child owns the endpoint and speaks framed WebSocket. It only
        // echoes fixture JSON; no installed provider/account/service is used.
        std::fs::write(&program, r##"#!/usr/bin/python3
import base64, hashlib, json, socket, struct, sys
path = sys.argv[sys.argv.index('--remote') + 1].removeprefix('unix://')
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.settimeout(15)
listener.bind(path)
listener.listen(1)
peer, _ = listener.accept()
peer.settimeout(15)
def exact(count):
    data = b''
    while len(data) < count:
        part = peer.recv(count - len(data))
        if not part: raise EOFError()
        data += part
    return data
header = b''
while not header.endswith(b'\r\n\r\n'):
    header += exact(1)
    if len(header) > 8192: raise ValueError('oversized HTTP header')
key = next(line.split(b':', 1)[1].strip() for line in header.split(b'\r\n') if line.lower().startswith(b'sec-websocket-key:'))
accept = base64.b64encode(hashlib.sha1(key + b'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest())
peer.sendall(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + b'\r\n\r\n')
def frame(opcode, data):
    if len(data) >= 126: raise ValueError('fixture response cap')
    peer.sendall(bytes([128 | opcode, len(data)]) + data)
try:
    while True:
        first, second = exact(2)
        count = second & 127
        if count == 126: count = struct.unpack('!H', exact(2))[0]
        if count == 127: count = struct.unpack('!Q', exact(8))[0]
        if count > 4096: raise ValueError('fixture request cap')
        mask = exact(4) if second & 128 else b'\0' * 4
        data = bytes(value ^ mask[index % 4] for index, value in enumerate(exact(count)))
        opcode = first & 15
        if opcode == 8: break
        if opcode == 9:
            frame(10, data)
        elif opcode == 1:
            request = json.loads(data)
            frame(1, json.dumps({'thread': request['thread']}).encode())
except (EOFError, BrokenPipeError, ConnectionResetError):
    pass
finally:
    peer.close()
    listener.close()
"##).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut source = chat::native_holder::fixture_source();
        let proof = source.proof.as_mut().unwrap();
        proof.boot_id = crate::daemon::read_boot_id().unwrap();
        source.owned_codex_listener.as_mut().unwrap().allocator = proof.clone();
        source.launch.recipe.program = program.to_string_lossy().into_owned();
        source.launch.recipe.working_directory = directory.path().into();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let _lease = crate::daemon::HostLease::acquire(&paths, source.proof.as_ref().unwrap()).unwrap();
        struct NativeFrames { identity: chat::protocol::ServerIdentity }
        impl Adapter for NativeFrames {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step { writes: vec![serde_json::json!({"thread":"first"})], ..Step::default() })
            }
            fn command(&mut self, command: chat::Command) -> Result<Step, String> {
                let chat::Command::SetModel { model, .. } = command else { return Err("unexpected fixture command".into()); };
                Ok(Step { writes: vec![serde_json::json!({"thread":model})], ..Step::default() })
            }
            fn receive(&mut self, value: serde_json::Value) -> Result<Step, String> {
                let id = value["thread"].as_str().ok_or("missing fixture thread")?.to_owned();
                Ok(Step { events: vec![chat::Event::Session { id, title: None }, chat::Event::Ready(true)], ..Step::default() })
            }
            fn server_identity(&self) -> Option<&chat::protocol::ServerIdentity> { Some(&self.identity) }
        }
        let mut spec = ProcessSpec::from(&source.launch.recipe);
        spec.transport = chat::protocol::TransportSpec::OwnedUnixWebSocket {
            socket: source.owned_codex_listener.as_ref().unwrap().socket.clone(),
        };
        let mut adapter: Box<dyn Adapter> = Box::new(NativeFrames { identity: chat::native_holder::fixture_identity() });
        let mut initial = adapter.initialize().unwrap();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let instance = uuid::Uuid::new_v4();
        let notifications: Arc<Mutex<Option<Weak<HostLock>>>> = Default::default();
        let notified = notifications.clone();
        let id = source.id;
        let handle = Handle::start_owned(spec, adapter, initial, store, source.clone(), Default::default(), move |update| {
            let host = notified.lock().unwrap().clone();
            if let Some(host) = host { changed_owned(&host, id, instance, update); }
        }).unwrap();
        let owner = Owner(handle.clone());
        let fixture = BackendFixture::new(source.clone(), handle.clone(), instance);
        *notifications.lock().unwrap() = Some(Arc::downgrade(&fixture.host));
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.confirmed_session_id().as_deref() != Some("first")
            || !matches!(handle.native_continuity(), chat::NativeContinuity::Durable { .. }) {
            assert!(Instant::now() < deadline, "owned native server did not confirm first conversation");
            thread::sleep(Duration::from_millis(2));
        }
        assert!(matches!(handle.native_continuity(), chat::NativeContinuity::Durable { .. }));
        let binding = fixture.capture().unwrap();
        assert!(fixture.validates(&binding));
        handle.command(chat::Command::SetModel { model: "second".into(), reasoning: None }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = handle.snapshot(None).unwrap();
            if snapshot.error.as_deref().is_some_and(|error| error.contains("cannot change conversations")) { break; }
            assert!(Instant::now() < deadline, "actual durable holder refusal missing: {:?}", snapshot.error);
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(handle.confirmed_session_id().as_deref(), Some("first"), "failed confirmation never positively publishes second thread");
        assert!(!fixture.validates(&binding), "actual early Identity callback must revoke first binding despite failed holder.confirm, without any refresh/read");
        assert!(fixture.host.lock().unwrap().chats[&id].source_binding.is_none());
        let _ = owner.0.close();
    }

    #[test]
    fn binding_refresh_and_require_refuse_busy_presentation_without_holding_host() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let owner = actor(&source);
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), uuid::Uuid::new_v4());
        let binding = fixture.capture().unwrap();
        for admission in [false, true] {
            let (held, held_rx) = crossbeam_channel::bounded(1);
            let (release, release_rx) = crossbeam_channel::bounded(1);
            let state_owner = owner.0.clone();
            let holder = thread::spawn(move || {
                state_owner.with_presentation_state_locked(|| {
                    held.send(()).unwrap();
                    // Bounded even if an assertion on the caller fails.
                    let _ = release_rx.recv_timeout(Duration::from_secs(2));
                });
            });
            held_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let host = fixture.host.clone();
            let expected = binding.clone();
            let (result, result_rx) = crossbeam_channel::bounded(1);
            let request = thread::spawn(move || {
                let response = if admission {
                    require(&host, 1, &expected)
                } else {
                    refresh(&host, expected.source.id, false)
                };
                let _ = result.send(response);
            });
            let response = result_rx.recv_timeout(Duration::from_millis(300));
            // Inspect the actual global census while presentation is still held.
            let census_available = fixture.host.try_lock().is_ok();
            let _ = release.send(());
            holder.join().unwrap();
            request.join().unwrap();
            let reason = response
                .expect("binding must refuse without waiting for presentation IO")
                .expect_err("busy presentation cannot publish or admit authority");
            assert!(
                reason.message.contains("presentation metadata is busy"),
                "{reason:?}"
            );
            assert!(
                census_available,
                "binding cannot retain the global host behind actor state"
            );
            assert!(
                fixture.validates(&binding),
                "busy sampling cannot mint or replace history"
            );
            assert!(
                !owner.0.is_completed(),
                "metadata contention cannot stop the provider"
            );
        }
    }

    #[test]
    fn retired_same_id_actor_cannot_resurrect_an_earlier_backend_binding() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let first = actor(&source);
        let fixture = BackendFixture::new(source.clone(), first.0.clone(), uuid::Uuid::new_v4());
        let a = fixture.capture().unwrap();
        let second = actor(&source);
        let b = uuid::Uuid::new_v4();
        {
            let mut state = fixture.host.lock().unwrap();
            state.native_sources.installed(source.id, b).unwrap();
            let record = state.chats.get_mut(&source.id).unwrap();
            record.execution_instance = b;
            record.handle = second.0.clone();
            record.source_binding = None;
        }
        let binding_b = fixture.capture().unwrap();
        assert_ne!(a.backend, binding_b.backend);
        assert!(!fixture.validates(&a));
        assert!(
            fixture
                .return_refuses(a.clone())
                .contains("captured native backend changed")
        );
        assert!(
            !second.0.is_completed(),
            "stale A consent cannot close real B"
        );
        second.0.close().unwrap();
        fixture.retire();
        assert!(
            !fixture.validates(&a),
            "B's retirement never recreates A authority"
        );
        assert!(
            fixture.validates(&binding_b),
            "exact latest history survives retirement"
        );
    }
    #[test]
    fn ambiguous_restart_fences_same_record_and_eviction_never_grants_authority() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let owner = actor(&source);
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), uuid::Uuid::new_v4());
        let a = fixture.capture().unwrap();
        fixture.begin_restart();
        assert!(fixture.capture().is_err());
        assert!(
            fixture
                .return_refuses(a.clone())
                .contains("captured native backend changed")
        );
        assert!(!owner.0.is_completed());
        let resolved = fixture.resolve();
        assert_ne!(
            resolved.backend, a.backend,
            "even a proved failed restart cannot revive an old user intent"
        );
        let mut state = fixture.host.lock().unwrap();
        for _ in 0..CAPACITY {
            state
                .native_sources
                .installed(TerminalId::new(), uuid::Uuid::new_v4()).unwrap();
        }
        assert!(!state.native_sources.validates(&resolved));
        assert!(state.native_sources.entries.len() <= CAPACITY);
    }
    #[test]
    fn publication_binds_full_source_and_exact_paired_worker_even_on_the_same_backend() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let owner = actor(&source);
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), uuid::Uuid::new_v4());
        let before = fixture.capture().unwrap();
        let mut state = fixture.host.lock().unwrap();
        let witness = state.native_sources.entries[&source.id]
            .known
            .as_ref()
            .unwrap()
            .0
            .clone();
        let terminal = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            source.id,
            source.launch.recipe.clone(),
            None,
        )
        .session;
        let mut paired_source = source.clone();
        paired_source.terminal = Some(terminal);
        let first_worker = uuid::Uuid::new_v4();
        let paired = state
            .native_sources
            .publish(
                paired_source.clone(),
                before.execution_instance,
                witness.clone(),
                Some(first_worker),
                false,
            )
            .unwrap();
        assert_ne!(
            paired.backend, before.backend,
            "pair publication cannot reuse no-pair consent"
        );
        assert!(!state.native_sources.validates(&before));
        let successor = state
            .native_sources
            .publish(
                paired_source.clone(),
                before.execution_instance,
                witness.clone(),
                Some(uuid::Uuid::new_v4()),
                false,
            )
            .unwrap();
        assert_ne!(
            successor.backend, paired.backend,
            "same descriptor cannot alias another concrete terminal worker"
        );
        assert!(!state.native_sources.validates(&paired));
        let restored = state
            .native_sources
            .publish(
                source.clone(),
                before.execution_instance,
                witness,
                None,
                false,
            )
            .unwrap();
        assert_ne!(
            restored.backend, before.backend,
            "retired pair never revives earlier source consent"
        );
        assert!(!state.native_sources.authorizes_intent(&before));
    }

    struct Learns;
    impl Adapter for Learns {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn receive(&mut self, value: serde_json::Value) -> Result<Step, String> {
            let threads: Vec<&str> = if value["thread"] == "batch" {
                vec!["first", "second"]
            } else if value["thread"] == "aba" {
                vec!["second", "first"]
            } else {
                vec![value["thread"].as_str().unwrap()]
            };
            Ok(Step {
                events: threads
                    .into_iter()
                    .map(|id| chat::Event::Session {
                        id: id.into(),
                        title: None,
                    })
                    .collect(),
                ..Step::default()
            })
        }
        fn command(&mut self, command: chat::Command) -> Result<Step, String> {
            let chat::Command::SetModel { model, .. } = command else {
                return Ok(Step::default());
            };
            Ok(Step {
                writes: vec![serde_json::json!({"thread": model})],
                ..Step::default()
            })
        }
    }

    #[test]
    fn unobserved_retained_native_owner_never_grants_backend_or_completed_claim_authority() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let owner = actor(&source);
        let proof = source.proof.as_ref().unwrap();
        let witness = owner.0.backend_identity(proof).unwrap();
        assert!(owner.0.backend_identity_is_current(&witness));
        owner.0.withdraw_backend_observation_for_test();
        assert!(
            owner
                .0
                .backend_identity(proof)
                .unwrap_err()
                .contains("not observed")
        );
        assert!(!owner.0.backend_identity_is_current(&witness));
        assert!(matches!(
            owner.0.claim_backend_source(&witness).unwrap(),
            chat::runtime::BackendSourceClaim::Refused(_)
        ));
        assert!(!owner.0.is_completed());
        owner.0.close().unwrap();
        assert!(owner.0.is_completed());
        // Ordinary positive cleanup correctly restores observed completion.
        // Independently exercise the completed retained/unknown marker gate;
        // this is a negative predicate fixture, not Native cleanup evidence.
        owner.0.withdraw_backend_observation_for_test();
        assert!(owner.0.backend_identity(proof).is_err());
        assert!(!owner.0.backend_identity_is_current(&witness));
        assert!(matches!(
            owner.0.claim_backend_source(&witness).unwrap(),
            chat::runtime::BackendSourceClaim::Refused(_)
        ));
    }

    #[test]
    fn handoff_owner_claim_refuses_conversation_changed_after_host_require_without_teardown() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let owner = actor_with(&source, Box::new(Learns));
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), uuid::Uuid::new_v4());
        owner
            .0
            .command(chat::Command::SetModel {
                model: "first".into(),
                reasoning: None,
            })
            .unwrap();
        let wait = |thread: &str| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while owner.0.confirmed_session_id().as_deref() != Some(thread) {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(2));
            }
        };
        wait("first");
        let binding = fixture.capture().unwrap();
        let mut continuation = continuation(&fixture.host, &binding).unwrap();
        owner
            .0
            .command(chat::Command::SetModel {
                model: "second".into(),
                reasoning: None,
            })
            .unwrap();
        wait("second");
        let reason =
            claim_continuation(&fixture.host, &binding, &binding, &mut continuation).unwrap_err();
        assert!(
            reason
                .message
                .contains("actual provider conversation changed"),
            "{reason:?}"
        );
        assert!(!owner.0.is_completed());
        assert_eq!(owner.0.confirmed_session_id().as_deref(), Some("second"));
        assert!(
            fixture.host.lock().unwrap().native_sources.entries[&source.id]
                .pending_handoff
                .is_none(),
            "an actual owner refusal clears only the unadmitted operation; transport ambiguity does not"
        );
    }

    #[test]
    fn lost_source_claim_reply_retains_exact_actor_and_refuses_queued_conversation_changes() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let owner = actor_with(&source, Box::new(Learns));
        let proof = source.proof.as_ref().unwrap();
        owner
            .0
            .command(chat::Command::SetModel {
                model: "first".into(),
                reasoning: None,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while owner.0.confirmed_session_id().as_deref() != Some("first") {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        let witness = owner.0.backend_identity(proof).unwrap();
        owner
            .0
            .claim_backend_source_losing_reply_for_test(&witness)
            .unwrap();
        let refusal = owner
            .0
            .command(chat::Command::SetModel {
                model: "second".into(),
                reasoning: None,
            })
            .unwrap_err();
        assert!(refusal.contains("pending terminal handoff"));
        assert_eq!(owner.0.confirmed_session_id().as_deref(), Some("first"));
        let token = owner
            .0
            .claim_backend_source(&witness)
            .unwrap()
            .into_result()
            .unwrap();
        assert_eq!(
            owner
                .0
                .claim_backend_source(&witness)
                .unwrap()
                .into_result()
                .unwrap(),
            token,
            "retry recovers the actual retained operation, not a new claim"
        );
        assert!(
            owner
                .0
                .release_backend_source(&witness, uuid::Uuid::new_v4())
                .is_err()
        );
        assert!(!owner.0.is_completed());
        owner.0.release_backend_source(&witness, token).unwrap();
        owner
            .0
            .command(chat::Command::SetModel {
                model: "second".into(),
                reasoning: None,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while owner.0.confirmed_session_id().as_deref() != Some("second") {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            owner
                .0
                .close_backend_source(&witness, token)
                .unwrap_err()
                .contains("changed")
        );
        assert!(
            !owner.0.is_completed(),
            "stale consent cannot stop the actual successor conversation"
        );
        let current = owner.0.backend_identity(proof).unwrap();
        let token = owner
            .0
            .claim_backend_source(&current)
            .unwrap()
            .into_result()
            .unwrap();
        owner.0.close_backend_source(&current, token).unwrap();
        assert!(owner.0.is_completed());
    }

    #[test]
    fn source_claim_refuses_prior_blocked_provider_writes_without_discarding_them() {
        use std::os::fd::{AsRawFd, FromRawFd};
        struct Queued;
        impl Adapter for Queued {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn command(&mut self, _: chat::Command) -> Result<Step, String> {
                Ok(Step {
                    writes: vec![serde_json::json!({"queued": "x".repeat(1024 * 1024)})],
                    ..Step::default()
                })
            }
        }
        struct Resume(std::fs::File);
        impl Drop for Resume {
            fn drop(&mut self) {
                // Only the original already-validated owned process is signaled.
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.0.as_raw_fd(),
                        libc::SIGCONT,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
        }
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let owner = actor_with(&source, Box::new(Queued));
        let process = owner.0.provider_process().unwrap();
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, process.pid, 0) };
        assert!(raw >= 0);
        let descriptor = unsafe { std::fs::File::from_raw_fd(raw as i32) };
        assert_eq!(
            crate::daemon::holder_registry::Process::read(process.pid).unwrap(),
            process
        );
        let resume = Resume(descriptor);
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    resume.0.as_raw_fd(),
                    libc::SIGSTOP,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            },
            0
        );
        let witness = owner
            .0
            .backend_identity(source.proof.as_ref().unwrap())
            .unwrap();
        owner.0.command(chat::Command::Refresh).unwrap();
        let reason = owner
            .0
            .claim_backend_source(&witness)
            .unwrap()
            .into_result()
            .unwrap_err();
        assert!(reason.contains("input/confirmation is pending"), "{reason}");
        assert!(!owner.0.is_completed());
        assert!(owner.0.backend_identity_is_current(&witness));
        drop(resume);
        owner.0.close().unwrap();
    }

    #[test]
    fn first_confirmed_thread_learning_keeps_only_the_original_unknown_intent() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let owner = actor_with(&source, Box::new(Learns));
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), uuid::Uuid::new_v4());
        let unknown = fixture.capture().unwrap();
        let learn = |thread: &str| {
            owner
                .0
                .command(chat::Command::SetModel {
                    model: thread.into(),
                    reasoning: None,
                })
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while owner.0.confirmed_session_id().as_deref() != Some(thread) {
                assert!(
                    Instant::now() < deadline,
                    "actual provider pipe must confirm {thread}"
                );
                thread::sleep(Duration::from_millis(2));
            }
        };
        learn("first");
        let first = fixture.capture().unwrap(); // UI may read before Return.
        assert_eq!(first.source.launch.session_id.as_deref(), Some("first"));
        assert_eq!(require(&fixture.host, 1, &unknown).unwrap(), first);
        assert!(
            !fixture.validates(&unknown),
            "old unknown is not general receipt authority"
        );
        learn("second");
        let second = fixture.capture().unwrap();
        assert_eq!(second.source.launch.session_id.as_deref(), Some("second"));
        assert!(require(&fixture.host, 1, &unknown).is_err());
        assert!(require(&fixture.host, 1, &first).is_err());
        assert!(
            !owner.0.is_completed(),
            "known-thread or old unknown refusal has no teardown effects"
        );
    }

    #[test]
    fn typed_handoff_ack_authenticates_the_full_operation_and_never_clears_a_successor() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let owner = actor(&source);
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), uuid::Uuid::new_v4());
        let binding = fixture.capture().unwrap();
        // Metadata-only issuer fixture: it deliberately grants NO terminal
        // teardown or source-transition authority; ACK must not require either.
        let mut descriptor = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            TerminalId::new(),
            source.launch.recipe.clone(),
            None,
        );
        descriptor.execution_instance = uuid::Uuid::new_v4();
        let a = TerminalHandoffReceipt {
            operation: uuid::Uuid::new_v4(),
            source: source.clone(),
            source_execution_instance: binding.execution_instance,
            source_binding: binding,
            retained_binding: None,
            terminal_native_epoch: uuid::Uuid::new_v4(),
            terminal: descriptor,
        };
        let mut b = a.clone();
        b.operation = uuid::Uuid::new_v4();
        b.terminal.execution_instance = uuid::Uuid::new_v4();
        {
            let mut state = fixture.host.lock().unwrap();
            state.remember_terminal_handoff(a.clone());
            state.remember_terminal_handoff(b.clone());
            state.chat_handoffs.insert(
                source.id,
                TerminalHandoff::from_receipt(
                    b.clone(),
                    0,
                    BTreeMap::new(),
                    Instant::now() + ACKNOWLEDGEMENT_GRACE,
                ),
            );
        }
        let mut forged = b.clone();
        forged.terminal.execution_instance = a.terminal.execution_instance;
        assert!(
            process_inner(
                &fixture.host,
                1,
                ClientRequest::ChatCommitTerminalReceipt { receipt: forged },
                None
            )
            .is_err()
        );
        assert!(
            process_inner(
                &fixture.host,
                1,
                ClientRequest::ChatCommitTerminal {
                    terminal_id: a.terminal.session.id
                },
                None
            )
            .is_err()
        );
        process_inner(
            &fixture.host,
            1,
            ClientRequest::ChatCommitTerminalReceipt { receipt: a },
            None,
        )
        .unwrap();
        assert_eq!(
            fixture.host.lock().unwrap().chat_handoffs[&source.id]
                .issued
                .as_ref(),
            Some(&b)
        );
        for _ in 0..2 {
            process_inner(
                &fixture.host,
                1,
                ClientRequest::ChatCommitTerminalReceipt { receipt: b.clone() },
                None,
            )
            .unwrap();
            assert!(
                !fixture
                    .host
                    .lock()
                    .unwrap()
                    .chat_handoffs
                    .contains_key(&source.id)
            );
        }
        assert!(
            !owner.0.is_completed(),
            "metadata ACK never stops any provider or terminal"
        );
    }

    #[test]
    fn unobserved_conversation_transitions_never_promote_old_unknown_return_consent() {
        for one_frame in [false, true] {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let source = source(directory.path());
            let owner = actor_with(&source, Box::new(Learns));
            let fixture = BackendFixture::new(source, owner.0.clone(), uuid::Uuid::new_v4());
            let unknown = fixture.capture().unwrap();
            let send = |value: &str, confirmed: &str| {
                owner
                    .0
                    .command(chat::Command::SetModel {
                        model: value.into(),
                        reasoning: None,
                    })
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                while owner.0.confirmed_session_id().as_deref() != Some(confirmed) {
                    assert!(
                        Instant::now() < deadline,
                        "actual provider frame did not confirm {confirmed}"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
            };
            if one_frame {
                send("batch", "second");
            } else {
                send("first", "first");
                send("second", "second");
            }
            // No capture, ChatRead or host observation happened between first
            // and second. The actual producer history must still fence Return.
            assert!(require(&fixture.host, 1, &unknown).is_err());
            assert!(
                fixture
                    .return_refuses(unknown)
                    .contains("captured native backend changed")
            );
            assert!(!owner.0.is_completed());
        }
    }

    #[test]
    fn returned_resource_native_publication_or_ambiguous_start_survives_retirement() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let original = actor(&source);
        let fixture = BackendFixture::new(source.clone(), original.0.clone(), uuid::Uuid::new_v4());
        let binding = fixture.capture().unwrap();
        // A returned worker may already have retired: only its positive,
        // bounded publication history remains. No absence grants authority.
        let mut terminal = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            TerminalId::new(),
            source.launch.recipe.clone(),
            None,
        );
        terminal.execution_instance = uuid::Uuid::new_v4();
        let receipt = {
            let mut state = fixture.host.lock().unwrap();
            state
                .observe_terminal_worker_for_test(terminal.session.id, terminal.execution_instance);
            let receipt = TerminalHandoffReceipt {
                operation: uuid::Uuid::new_v4(),
                source: source.clone(),
                source_execution_instance: binding.execution_instance,
                source_binding: binding.clone(),
                retained_binding: None,
                terminal_native_epoch: state
                    .native_sources
                    .retain_resource_epoch(terminal.session.id).unwrap(),
                terminal: terminal.clone(),
            };
            state.remember_terminal_handoff(receipt.clone());
            state.validate_terminal_handoff_receipt(&receipt).unwrap();
            receipt
        };
        let mut native_source = source.clone();
        native_source.id = terminal.session.id;
        let successor = actor(&native_source); // actual independent provider process
        {
            let mut state = fixture.host.lock().unwrap();
            let instance = uuid::Uuid::new_v4();
            state.native_sources.installed(native_source.id, instance).unwrap();
            state.chats.insert(
                native_source.id,
                Record {
                    execution_instance: instance,
                    console_recipe: None,
                    paired_terminal: None,
                    handoff_receipt: None,
                    source_binding: None,
                    session: native_source.clone(),
                    handle: successor.0.clone(),
                    source_process: None,
                    subscribers: BTreeMap::from([(1, false)]),
                    observations: BTreeMap::new(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
            assert!(state.validate_terminal_handoff_receipt(&receipt).is_err());
        }
        successor.0.close().unwrap();
        {
            let mut state = fixture.host.lock().unwrap();
            drop(state.chats.remove(&native_source.id));
            assert!(state.chats.get(&native_source.id).is_none());
            assert_eq!(
                state.latest_terminal_worker_for_test(native_source.id),
                Some(terminal.execution_instance)
            );
            assert!(
                state.native_sources.validates(&binding),
                "original C0 backend history is unchanged"
            );
            assert!(
                state.validate_terminal_handoff_receipt(&receipt).is_err(),
                "retired B cannot restore C0 -> T1 consent"
            );
        }
        // A DIFFERENT returned resource is fenced before any provider creation,
        // including failure before Record install. Clearing pending metadata
        // cannot repair that explicitly unresolved transition.
        let pending_id = TerminalId::new();
        let mut pending = receipt.clone();
        pending.operation = uuid::Uuid::new_v4();
        pending.terminal.session.id = pending_id;
        {
            let mut state = fixture.host.lock().unwrap();
            state.observe_terminal_worker_for_test(pending_id, pending.terminal.execution_instance);
            pending.terminal_native_epoch = state.native_sources.retain_resource_epoch(pending_id).unwrap();
            state.remember_terminal_handoff(pending.clone());
            state.validate_terminal_handoff_receipt(&pending).unwrap();
            state.native_sources.mutating(pending_id).unwrap(); // actual production pre-creation seam
            assert!(!state.chats.contains_key(&pending_id));
            assert!(state.validate_terminal_handoff_receipt(&pending).is_err());
            for _ in 0..CAPACITY {
                state
                    .native_sources
                    .retain_resource_epoch(TerminalId::new()).unwrap();
            }
            assert_eq!(state.native_sources.resource_epoch(pending_id), None);
            assert!(
                state.validate_terminal_handoff_receipt(&pending).is_err(),
                "eviction never reconstructs an absence stamp"
            );
        }
        assert!(!original.0.is_completed());
    }

    #[test]
    fn same_final_conversation_aba_revokes_issued_receipt_without_any_read_refresh() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let instance = uuid::Uuid::new_v4();
        let callback_host = Arc::new(Mutex::new(Weak::<HostLock>::new()));
        let target = callback_host.clone();
        let id = source.id;
        let owner = actor_with_notify(&source, Box::new(Learns), move |update| {
            let weak = target.lock().unwrap().clone();
            changed_owned(&weak, id, instance, update);
        });
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), instance);
        *callback_host.lock().unwrap() = Arc::downgrade(&fixture.host);
        owner
            .0
            .command(chat::Command::SetModel {
                model: "first".into(),
                reasoning: None,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while owner.0.confirmed_session_id().as_deref() != Some("first") {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        let binding = fixture.capture().unwrap();
        let revision = owner.0.observed_backend_revision().unwrap();
        let proof = fixture.host.lock().unwrap().proof().clone();
        let stale_witness = owner.0.backend_identity(&proof).unwrap();
        assert!(owner.0.backend_identity_is_current(&stale_witness));
        let receipt = {
            let mut state = fixture.host.lock().unwrap();
            let mut terminal = crate::terminal_ipc::TerminalDescriptor::relaunchable(
                TerminalId::new(),
                source.launch.recipe.clone(),
                None,
            );
            terminal.execution_instance = uuid::Uuid::new_v4();
            state
                .observe_terminal_worker_for_test(terminal.session.id, terminal.execution_instance);
            let receipt = TerminalHandoffReceipt {
                operation: uuid::Uuid::new_v4(),
                source: binding.source.clone(),
                source_execution_instance: instance,
                source_binding: binding.clone(),
                retained_binding: None,
                terminal_native_epoch: state
                    .native_sources
                    .retain_resource_epoch(terminal.session.id).unwrap(),
                terminal,
            };
            state.remember_terminal_handoff(receipt.clone());
            state.validate_terminal_handoff_receipt(&receipt).unwrap();
            receipt
        };
        owner
            .0
            .command(chat::Command::SetModel {
                model: "aba".into(),
                reasoning: None,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while fixture.validates(&binding) {
            assert!(
                Instant::now() < deadline,
                "actual identity callback must revoke same-final-thread authority without a fresh Read"
            );
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(owner.0.confirmed_session_id().as_deref(), Some("first"));
        assert_eq!(owner.0.observed_backend_revision(), Some(revision + 2));
        assert!(
            !owner.0.backend_identity_is_current(&stale_witness),
            "a fresh-read witness cannot be republished after a producer-side ABA revocation"
        );
        assert!(
            fixture
                .host
                .lock()
                .unwrap()
                .validate_terminal_handoff_receipt(&receipt)
                .is_err()
        );
        assert!(!owner.0.is_completed());
    }

    #[test]
    fn oversized_handoff_metadata_refuses_before_any_owned_provider_or_terminal_effect() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut source = source(directory.path());
        source
            .launch
            .recipe
            .environment
            .insert("HANDOFF_METADATA_FIXTURE".into(), "x".repeat(70 * 1024));
        let owner = actor(&source);
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), uuid::Uuid::new_v4());
        let binding = fixture.capture().unwrap();
        let refusal = fixture.return_refuses(binding);
        assert!(refusal.contains("acknowledgement budget"), "{refusal}");
        assert!(
            !owner.0.is_completed(),
            "preflight must precede provider teardown"
        );
        let state = fixture.host.lock().unwrap();
        assert!(!state.chats[&source.id].returning_terminal);
        assert!(state.terminals.is_empty());
        assert!(state.chat_handoffs.is_empty());
    }
}
