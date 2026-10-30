//! Connection-scoped evidence for the Resolve -> Attach recovery handshake.
//!
//! A descriptor names a conversation, not a particular provider attachment.
//! Preserve the exact attachment across both requests, and leave a spent marker
//! after every failed or consumed attempt. Retrying requires a new resolution.

use super::*;

const LIFETIME: Duration = Duration::from_secs(70);

#[cfg(test)]
thread_local! {
    static AFTER_SNAPSHOT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) struct SnapshotHook(std::marker::PhantomData<std::rc::Rc<()>>);

#[cfg(test)]
impl Drop for SnapshotHook {
    fn drop(&mut self) {
        AFTER_SNAPSHOT.with(|slot| slot.borrow_mut().take());
    }
}

#[cfg(test)]
pub(super) fn after_snapshot_for_test(action: impl FnOnce() + 'static) -> SnapshotHook {
    AFTER_SNAPSHOT.with(|slot| {
        assert!(slot.borrow().is_none(), "snapshot hook already owned");
        *slot.borrow_mut() = Some(Box::new(action));
    });
    SnapshotHook(std::marker::PhantomData)
}

#[cfg(test)]
pub(super) fn expire_for_test(host: &SharedHost, client: ClientId) {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let Some(Attempt::Ready(grant)) = state.chat_resolutions.get_mut(&client) else {
        panic!("expiry fixture requires an actual resolved grant");
    };
    grant.expires = Instant::now() - Duration::from_secs(1);
}

pub(in crate::host) enum Attempt {
    Ready(Grant),
    Spent(uuid::Uuid),
}

impl Attempt {
    fn id(&self) -> uuid::Uuid {
        match self {
            Self::Ready(grant) => grant.attempt,
            Self::Spent(id) => *id,
        }
    }
}

pub(in crate::host) struct Grant {
    attempt: uuid::Uuid,
    source: Session,
    resolved: Session,
    record: Session,
    handle: Handle,
    execution_instance: uuid::Uuid,
    proof: HostSessionProof,
    expires: Instant,
}

impl Grant {
    fn current(&self, state: &HostState, client: ClientId) -> bool {
        !state.shutting_down
            && Instant::now() < self.expires
            && state.clients.contains_key(&client)
            && state.proof() == &self.proof
            && state
                .chat_resolutions
                .get(&client)
                .is_some_and(|attempt| attempt.id() == self.attempt)
            && state.chats.get(&self.record.id).is_some_and(|record| {
                record.session == self.record
                    && record.execution_instance == self.execution_instance
                    && record.handle.same_attachment(&self.handle)
            })
    }

    fn resolve_actor(&self) -> Result<Session, ProtocolError> {
        // This always performs an epoch-authenticated RPC. Inspection and
        // cached final snapshots cannot certify a current attachment.
        let resolved = self
            .handle
            .resolve_retained_source(&self.source)
            .map_err(unavailable)?;
        let mut expected = self.record.clone();
        expected.launch.session_id = resolved.launch.session_id.clone();
        expected.native_continuity = resolved.native_continuity.clone();
        if resolved != expected
            || resolved.proof.as_ref() != Some(&self.proof)
            || !matches!(
                resolved.native_continuity,
                chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
            )
        {
            return Err(unavailable(
                "The retained helper changed its exact published recovery descriptor",
            ));
        }
        Ok(resolved)
    }
}

pub(in crate::host) fn resolve(
    host: &SharedHost,
    client: ClientId,
    source: &Session,
) -> Result<Session, ProtocolError> {
    let attempt = uuid::Uuid::new_v4();
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !state.clients.contains_key(&client) {
            return Err(error("Native recovery requires a registered connection"));
        }
        // Invalidate first, including when source admission or the RPC fails.
        // One slot per registered connection bounds both handles and markers.
        state
            .chat_resolutions
            .insert(client, Attempt::Spent(attempt));
    }
    // Startup discovery can miss an unvisited retained owner. Resolve its
    // exact issued source through ordinary authenticated adoption before
    // asking for lineage. This route never executes a saved launch recipe,
    // even if the holder disappears between discovery and authentication.
    let (missing, cleanup) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (
            !state.chats.contains_key(&source.id),
            state
                .chats
                .get(&source.id)
                .is_some_and(|record| record.handle.is_close_only()),
        )
    };
    if cleanup {
        super::retained_actor::recover_cleanup_attachment(host, source).map_err(unavailable)?;
    }
    if missing {
        // Client zero is the existing unattached startup-adoption mode.
        // Resolve must not subscribe this connection before its Attach.
        if let Err(native_error) = super::native_recovery::restore(host, 0, source) {
            // A missing holder is not permission to execute its old recipe.
            // The separate history constructor can only page saved data and
            // repeats owner/managed-work fences under its admission guard.
            if !source.is_saved_claude_history() {
                return Err(unavailable(native_error));
            }
            super::capacity::recover_saved_history(host, source)?;
        }
    }
    if source.is_saved_claude_history() {
        super::capacity::register_saved_history_alias(host, source)?;
    }
    let resolved = super::resolve_upgrade(host, source)?;
    let mut grant = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get(&source.id)
            .ok_or_else(|| unavailable("Resolved native actor is no longer hosted"))?;
        if record.handle.actor_identity().is_none() {
            // Existing local/Codex resolution keeps its ordinary attachment
            // contract. It cannot mint the retained-Claude transition witness.
            if matches!(
                resolved.native_continuity,
                chat::NativeContinuity::RetainedClaudeActor { .. }
            ) {
                return Err(unavailable("Retained recovery lost its exact remote actor"));
            }
            state.chat_resolutions.remove(&client);
            return Ok(resolved);
        }
        Grant {
            attempt,
            source: source.clone(),
            resolved,
            record: record.session.clone(),
            handle: record.handle.clone(),
            execution_instance: record.execution_instance,
            proof: state.proof().clone(),
            expires: Instant::now() + LIFETIME,
        }
    };
    grant.resolved = grant.resolve_actor()?;
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    if !grant.current(&state, client) {
        return Err(unavailable(
            "The native actor changed while resolving its exact recovery source",
        ));
    }
    let resolved = grant.resolved.clone();
    state.chat_resolutions.insert(client, Attempt::Ready(grant));
    Ok(resolved)
}

/// Ordinary fresh/History attachments have no attempt and keep their existing
/// path. Once this connection resolves a recovery, every Attach consumes that
/// attempt before validation; no failure can fall through to ordinary Attach.
pub(in crate::host) fn attach(
    host: &SharedHost,
    client: ClientId,
    requested: &Session,
) -> Result<Option<ServerResponse>, ProtocolError> {
    let (grant, newly_subscribed) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(attempt) = state.chat_resolutions.get_mut(&client) else {
            return Ok(None);
        };
        let id = attempt.id();
        let Attempt::Ready(grant) = std::mem::replace(attempt, Attempt::Spent(id)) else {
            return Err(unavailable(
                "Native recovery attempt is spent; resolve again",
            ));
        };
        if requested != &grant.resolved || !grant.current(&state, client) {
            return Err(unavailable(
                "Native recovery attachment no longer matches its exact resolved actor",
            ));
        }
        let record = state
            .chats
            .get_mut(&grant.record.id)
            .expect("checked actor");
        let newly_subscribed = if let std::collections::btree_map::Entry::Vacant(entry) =
            record.subscribers.entry(client)
        {
            entry.insert(false);
            record.observations.insert(
                client,
                Observation {
                    after_generation: record.handle.observe_next_update(),
                    ..Observation::default()
                },
            );
            true
        } else {
            false
        };
        (grant, newly_subscribed)
    };
    let mut subscription = ProvisionalSubscription {
        host,
        client,
        grant: &grant,
        rollback: newly_subscribed,
    };
    let response = super::session_snapshot(&grant.record, &grant.handle).map_err(unavailable)?;
    #[cfg(test)]
    if let Some(action) = AFTER_SNAPSHOT.with(|slot| slot.borrow_mut().take()) {
        // A test-owned cutpoint, outside HostLock and without production state.
        action();
    }
    let ServerResponse::ChatAttached { session, snapshot } = &response else {
        unreachable!("session_snapshot returns ChatAttached")
    };
    if session != &grant.resolved || snapshot.session_id != grant.resolved.launch.session_id {
        return Err(unavailable(
            "Native recovery snapshot changed the exact resolved conversation",
        ));
    }
    // Hydration may use a final-snapshot cache. Reauthenticate the captured
    // epoch and exact issued lineage after it, outside the HOST lock.
    if grant.resolve_actor()? != grant.resolved {
        return Err(unavailable(
            "Native recovery lineage changed while its snapshot was loading",
        ));
    }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if !grant.current(&state, client) {
        return Err(unavailable(
            "Native recovery attachment changed while its snapshot was loading",
        ));
    }
    subscription.rollback = false;
    Ok(Some(response))
}

struct ProvisionalSubscription<'a> {
    host: &'a SharedHost,
    client: ClientId,
    grant: &'a Grant,
    rollback: bool,
}

impl Drop for ProvisionalSubscription<'_> {
    fn drop(&mut self) {
        if !self.rollback {
            return;
        }
        let mut state = self.host.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(record) = state.chats.get_mut(&self.grant.record.id)
            && record.execution_instance == self.grant.execution_instance
            && record.handle.same_attachment(&self.grant.handle)
        {
            record.subscribers.remove(&self.client);
            record.observations.remove(&self.client);
        }
    }
}
