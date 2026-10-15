//! Store-issued revocation crosses every resident/final-publication path.
//! A scalar cannot acknowledge semantic outcomes or prove provider cleanup.
use super::*;

/// Ignore delayed metadata and absent legacy fields without erasing a token
/// already issued by this exact Store. A different incarnation requires a
/// strictly newer Store clock; neither this layer nor a Snapshot mints one.
pub(super) fn advance(current: &mut Option<SourceAuthority>, incoming: Option<SourceAuthority>) -> bool {
    let Some(incoming) = incoming.filter(|token| !token.incarnation.is_nil()) else { return false };
    if current.is_some_and(|old| incoming.generation <= old.generation) { return false; }
    *current = Some(incoming);
    true
}

impl RemoteActor {
    pub(super) fn replace_resident(&self, mut resident: Resident, allow_closed: bool) -> bool {
        let authority = {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            if self.0.stop.load(Ordering::Acquire) && !allow_closed { return false; }
            let mut authority = state.source_authority;
            let changed = advance(&mut authority, resident.source_authority);
            resident.source_authority = authority;
            *state = Arc::new(resident);
            changed.then_some(authority).flatten()
        };
        // No Host callback under the resident mutex. Receivers additionally
        // fence generation so concurrent delayed callbacks cannot undo Reset.
        if let Some(authority) = authority { self.notify_source_authority(authority); }
        true
    }
    pub(super) fn publish_source_authority(&self, incoming: Option<SourceAuthority>) {
        let authority = {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            let state = Arc::make_mut(&mut state);
            advance(&mut state.source_authority, incoming).then_some(state.source_authority).flatten()
        };
        if let Some(authority) = authority { self.notify_source_authority(authority); }
    }
    fn notify_source_authority(&self, authority: SourceAuthority) {
        (self.0.notify)(Update::SourceAuthority {
            incarnation: authority.incarnation, generation: authority.generation,
        });
    }
    pub(super) fn accepts_body(&self, generation: u64) -> bool {
        self.state().source_authority.is_none_or(|authority| generation >= authority.generation)
    }
    pub(in crate::chat::runtime) fn presentation(&self, chat_id: crate::model::TerminalId, after: Option<u64>) -> Option<crate::terminal_ipc::ServerResponse> {
        let response = self.call::<Option<crate::terminal_ipc::ServerResponse>>("presentation", json!([chat_id, after])).ok().flatten()?;
        let generation = match &response {
            crate::terminal_ipc::ServerResponse::ChatSnapshot { snapshot: Some(snapshot), .. } => snapshot.generation,
            crate::terminal_ipc::ServerResponse::ChatSnapshot { snapshot: None, .. } => return Some(response),
            crate::terminal_ipc::ServerResponse::ChatUpdate { update, .. } => update.metadata.generation,
            _ => return None,
        };
        self.accepts_body(generation).then_some(response)
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
        assert!(!actor.accepts_body(51), "scalar arrives before an already queued old body");
        assert!(actor.accepts_body(52));
        let mut delayed = actor.state().as_ref().clone();
        delayed.generation = 51;
        delayed.source_authority = Some(SourceAuthority { incarnation: uuid::Uuid::new_v4(), generation: 51 });
        assert!(actor.publish_resident(delayed));
        assert_eq!(actor.state().source_authority, Some(authority));
        assert!(!actor.accepts_body(51));
        assert_eq!(observed.lock().unwrap().len(), 2, "stale metadata never emits another token");
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
        assert!(actor.replace_resident(resident, true));
        assert_eq!(actor.state().source_authority, Some(authority));
        actor.publish_source_authority(None);
        assert_eq!(actor.state().source_authority, Some(authority), "legacy final receipt means unavailable, never a replacement token");
    }
}
