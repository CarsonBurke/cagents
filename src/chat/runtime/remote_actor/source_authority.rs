//! Store-issued revocation crosses every resident/final-publication path.
//! A scalar cannot acknowledge semantic outcomes or prove provider cleanup.
use super::*;

/// Ignore delayed metadata and absent legacy fields without erasing a token
/// already issued by this exact Store. A different incarnation requires a
/// strictly newer Store clock; neither this layer nor a Snapshot mints one.
pub(super) fn advance(current: &mut Option<SourceAuthority>, conflict: &mut Option<SourceAuthority>, incoming: Option<SourceAuthority>) -> bool {
    if conflict.is_some() { return false; }
    let Some(incoming) = incoming else { return false };
    if let Some(old) = current {
        if incoming.generation < old.generation || incoming == *old { return false; }
        if incoming.generation == old.generation {
            // Preserve BOTH actual contradictory tokens. Later generations
            // cannot repair this exact Store/attachment's quarantined scope.
            *conflict = Some(incoming);
            return true;
        }
    }
    *current = Some(incoming);
    true
}

pub(super) fn merge(current: &mut Option<SourceAuthority>, conflict: &mut Option<SourceAuthority>, incoming: Option<SourceAuthority>, incoming_conflict: Option<SourceAuthority>) -> bool {
    if conflict.is_some() { return false; }
    if let (Some(first), Some(second)) = (incoming, incoming_conflict) {
        // Historical contradiction remains fatal even if a delayed caller
        // observed a higher generation before receiving its complete pair.
        *current = Some(first);
        *conflict = Some(second);
        return true;
    }
    advance(current, conflict, incoming)
}

impl RemoteActor {
    pub(super) fn replace_resident(&self, mut resident: Resident, allow_closed: bool) -> bool {
        let authority = {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            if self.0.stop.load(Ordering::Acquire) && !allow_closed { return false; }
            let mut authority = state.source_authority;
            let mut conflict = state.source_authority_conflict;
            let unavailable = state.source_authority_unavailable.or(resident.source_authority_unavailable);
            let unavailable_changed = unavailable != state.source_authority_unavailable;
            let changed = merge(&mut authority, &mut conflict, resident.source_authority, resident.source_authority_conflict);
            resident.source_authority = authority;
            resident.source_authority_conflict = conflict;
            resident.source_authority_unavailable = unavailable;
            *state = Arc::new(resident);
            (changed.then_some((authority, conflict)).filter(|_| unavailable.is_none()), unavailable_changed.then_some(unavailable).flatten())
        };
        // No Host callback under the resident mutex. Receivers additionally
        // fence generation so concurrent delayed callbacks cannot undo Reset.
        if let Some(cause) = authority.1 { (self.0.notify)(Update::SourceAuthorityUnavailable { cause }); }
        else if let Some(authority) = authority.0 { self.notify_source_authority(authority); }
        true
    }
    pub(super) fn publish_source_authority(&self, incoming: Option<SourceAuthority>, conflicting: Option<SourceAuthority>, unavailable: Option<chat::SourceAuthorityUnavailableCause>) {
        let authority = {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            let state = Arc::make_mut(&mut state);
            let unavailable_changed = state.source_authority_unavailable.is_none() && unavailable.is_some();
            state.source_authority_unavailable = state.source_authority_unavailable.or(unavailable);
            let changed = merge(&mut state.source_authority, &mut state.source_authority_conflict, incoming, conflicting);
            (changed.then_some((state.source_authority, state.source_authority_conflict)).filter(|_| state.source_authority_unavailable.is_none()), unavailable_changed.then_some(state.source_authority_unavailable).flatten())
        };
        if let Some(cause) = authority.1 { (self.0.notify)(Update::SourceAuthorityUnavailable { cause }); }
        else if let Some(authority) = authority.0 { self.notify_source_authority(authority); }
    }
    fn notify_source_authority(&self, (authority, conflict): (Option<SourceAuthority>, Option<SourceAuthority>)) {
        if let Some(authority) = authority {
            (self.0.notify)(Update::SourceAuthority {
                incarnation: authority.incarnation, generation: authority.generation,
            });
            if let Some(conflict) = conflict {
                // Explicit contradiction survives a receiver's higher
                // watermark and scalar coalescing. Never fabricate a token.
                (self.0.notify)(Update::SourceAuthorityConflict {
                    generation: authority.generation,
                    original: authority.incarnation,
                    conflicting: conflict.incarnation,
                });
            }
        }
    }
    pub(super) fn accepts_body(&self, snapshot: &Snapshot) -> bool {
        let state = self.state();
        state.source_authority_unavailable.is_none() && state.source_authority_conflict.is_none() && state.source_authority.is_some_and(|authority|
            !authority.incarnation.is_nil() && snapshot.source_incarnation == Some(authority.incarnation)
                && snapshot.generation >= authority.generation)
    }
    pub(in crate::chat::runtime) fn presentation(&self, chat_id: crate::model::TerminalId, after: Option<u64>) -> Option<crate::terminal_ipc::ServerResponse> {
        let response = self.call::<Option<crate::terminal_ipc::ServerResponse>>("presentation", json!([chat_id, after])).ok().flatten()?;
        let snapshot = match &response {
            crate::terminal_ipc::ServerResponse::ChatSnapshot { snapshot: Some(snapshot), .. } => snapshot,
            crate::terminal_ipc::ServerResponse::ChatSnapshot { snapshot: None, .. } => return Some(response),
            crate::terminal_ipc::ServerResponse::ChatUpdate { update, .. } => &update.metadata,
            _ => return None,
        };
        self.accepts_body(snapshot).then_some(response)
    }
}

impl Server {
    pub(super) fn refresh_source_authority_metadata(&self) {
        // A cache-only read can rotate the source after Local actor EOF. Its
        // cheap cache remains truthful, while no actor remains to emit notify.
        // Try only resident metadata here: control admission never waits on
        // a source worker's Store/state locks or Journal SQLite operation.
        let unavailable = self.actor.source_authority_unavailable();
        let actual = self.actor.source_authority();
        if actual.is_none() && unavailable.is_none() { return; }
        let mut journal = match self.journal.try_lock() {
            Ok(journal) => journal,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        };
        let before = (journal.source_authority(), journal.source_authority_conflict(), journal.source_authority_unavailable());
        if let Some(cause) = unavailable { let _ = journal.append(&Update::SourceAuthorityUnavailable {cause}); }
        else if let Some(actual) = actual { let _ = journal.append(&Update::SourceAuthority {incarnation:actual.incarnation,generation:actual.generation}); }
        if before != (journal.source_authority(), journal.source_authority_conflict(), journal.source_authority_unavailable()) {
            self.changed.fetch_add(1, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reset_scalar_precedes_body_notification_and_delayed_metadata_cannot_restore_old_body() {
        let mut actor = RemoteActor::fixture();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink = observed.clone();
        Arc::get_mut(&mut actor.0).unwrap().notify = Arc::new(move |update| sink.lock().unwrap().push(update));
        let authority = SourceAuthority { incarnation: uuid::Uuid::new_v4(), generation: 52 };
        let mut resident = actor.state().as_ref().clone();
        resident.generation = 52;
        resident.source_authority = Some(authority);
        assert!(actor.publish_resident(resident));
        (actor.0.notify)(Update::Observed { generation: 52 });
        assert!(matches!(observed.lock().unwrap().as_slice(), [Update::SourceAuthority {incarnation, generation:52}, Update::Observed {generation:52}] if *incarnation==authority.incarnation));
        assert!(!actor.accepts_body(&Snapshot {generation:51,source_incarnation:Some(authority.incarnation),..Default::default()}), "scalar arrives before an already queued old body");
        assert!(actor.accepts_body(&Snapshot {generation:52,source_incarnation:Some(authority.incarnation),..Default::default()}));
        assert!(!actor.accepts_body(&Snapshot {generation:53,source_incarnation:Some(uuid::Uuid::new_v4()),..Default::default()}));
        let mut delayed = actor.state().as_ref().clone();
        delayed.generation = 51;
        delayed.source_authority = Some(SourceAuthority { incarnation: uuid::Uuid::new_v4(), generation: 51 });
        assert!(actor.publish_resident(delayed));
        assert_eq!(actor.state().source_authority, Some(authority));
        assert!(!actor.accepts_body(&Snapshot {generation:51,source_incarnation:Some(authority.incarnation),..Default::default()}));
        assert_eq!(observed.lock().unwrap().len(), 2, "stale metadata never emits another token");
    }
    #[test]
    fn conflicting_tokens_stay_quarantined_across_later_metadata_and_fresh_attachment() {
        let mut actor = RemoteActor::fixture();
        let observed = Arc::new(Mutex::new(Vec::new())); let sink = observed.clone();
        Arc::get_mut(&mut actor.0).unwrap().notify = Arc::new(move |update| sink.lock().unwrap().push(update));
        let first = SourceAuthority {incarnation:uuid::Uuid::new_v4(),generation:10};
        let conflict = SourceAuthority {incarnation:uuid::Uuid::new_v4(),generation:10};
        actor.publish_source_authority(Some(first), Some(conflict), None);
        actor.publish_source_authority(Some(SourceAuthority {incarnation:uuid::Uuid::new_v4(),generation:20}), None, None);
        assert_eq!(actor.state().source_authority, Some(first));
        assert_eq!(actor.state().source_authority_conflict, Some(conflict));
        assert!(!actor.accepts_body(&Snapshot {generation:20,source_incarnation:Some(first.incarnation),..Default::default()}));
        assert!(matches!(observed.lock().unwrap().as_slice(), [Update::SourceAuthority {incarnation:a,generation:10},Update::SourceAuthorityConflict {original,conflicting,generation:10}] if *a==first.incarnation && *original==first.incarnation && *conflicting==conflict.incarnation));
        let fresh = RemoteActor::fixture();
        fresh.publish_source_authority(Some(SourceAuthority {incarnation:uuid::Uuid::new_v4(),generation:20}), None, None);
        assert!(fresh.replace_resident(actor.state().as_ref().clone(), false));
        assert_eq!(fresh.state().source_authority_conflict, Some(conflict));
        assert!(!fresh.accepts_body(&Snapshot {generation:30,source_incarnation:Some(first.incarnation),..Default::default()}));
    }
    #[test]
    fn explicit_clock_unavailability_revokes_body_without_minting_an_ordered_pair() {
        let actor = RemoteActor::fixture();
        let cause = chat::SourceAuthorityUnavailableCause::ClockExhausted;
        actor.publish_source_authority(None, None, Some(cause));
        actor.publish_source_authority(Some(SourceAuthority {incarnation:uuid::Uuid::new_v4(),generation:20}), None, None);
        assert_eq!(actor.state().source_authority_unavailable, Some(cause));
        assert!(!actor.accepts_body(&Snapshot::default()));
        let fresh = RemoteActor::fixture();
        fresh.replace_resident(actor.state().as_ref().clone(), false);
        assert_eq!(fresh.state().source_authority_unavailable, Some(cause));
    }
    #[test]
    fn final_publication_and_explicit_refresh_can_publish_authority_after_proxy_stop() {
        let actor = RemoteActor::fixture();
        actor.publish_closed(Snapshot::default());
        let authority = SourceAuthority { incarnation: uuid::Uuid::new_v4(), generation: 9 };
        let mut resident = actor.state().as_ref().clone();
        resident.source_authority = Some(authority);
        assert!(!actor.publish_resident(resident.clone()));
        assert_eq!(actor.state().source_authority, None);
        assert!(!actor.accepts_body(&Snapshot::default()));
        assert!(actor.replace_resident(resident, true));
        assert_eq!(actor.state().source_authority, Some(authority));
        actor.publish_source_authority(None, None, None);
        assert_eq!(actor.state().source_authority, Some(authority), "legacy final receipt means unavailable, never a replacement token");
    }
}
