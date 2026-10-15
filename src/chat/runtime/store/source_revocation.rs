//! One actual kernel epoch for each Store-issued public source incarnation.
//! Export readers never invent their own revocation FD. All public token
//! changes pass this barrier before either metadata or SQL body mutation.
use super::*;
impl Store {
    #[cfg(test)]
    pub(in crate::chat::runtime) fn fail_source_revocation_for_test(&mut self) {
        self.source_revocation.inject_revoke_failure_for_test().unwrap();
    }

    pub(super) fn rotate_source_incarnation(&mut self) -> Result<(), String> {
        let incarnation = uuid::Uuid::new_v4();
        let next = match crate::transcript_cache::export::SourceRevocationEpoch::new(incarnation) {
            Ok(next) => next,
            Err(error) => {
                self.refuse_source_revocation();
                return Err(format!("Source revocation epoch could not be established; no source identity or body was changed: {error}"));
            }
        };
        // Allocate first. If allocation or signaling fails, no token, reset
        // intent, canonical generation or body mutation has taken place.
        if let Err(error) = self.source_revocation.revoke() {
            self.refuse_source_revocation();
            return Err(format!("Source revocation was not confirmed; Reset/source mutation was refused: {error}"));
        }
        self.source_revocation = next;
        self.source_incarnation = incarnation;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript_cache::export::Error;
    fn populate(store:&mut Store) {
        store.apply(Event::Item(Item {id:"private".into(),role:Role::Assistant,at:None,started_at:None,
            content:ItemContent::Text {text:"old private source".into(),thinking:false,phase:Phase::Final}})).unwrap();
    }
    #[test]
    fn failed_reset_intent_revokes_the_actual_previous_kernel_epoch_before_mutation() {
        let mut store=Store::new().unwrap();populate(&mut store);
        let incarnation=store.source_incarnation;
        let previous=store.source_revocation.probe().unwrap();
        let body:Vec<u8>=store.db.query_row("SELECT data FROM chunks WHERE id='private'",[],|r|r.get(0)).unwrap();
        store.refuse_source_reset_for_test(true);
        assert!(store.apply(Event::Reset {reason:"intent refused".into()}).is_err());
        assert!(matches!(previous.check(),Err(Error::Revoked)));
        assert_ne!(store.source_incarnation,incarnation);
        let next=store.source_revocation.probe().unwrap();
        assert_eq!(next.incarnation(),store.source_incarnation);next.check().unwrap();
        assert_eq!(store.db.query_row::<Vec<u8>,_,_>("SELECT data FROM chunks WHERE id='private'",[],|r|r.get(0)).unwrap(),body);
        assert_eq!(store.db.query_row::<i64,_,_>("SELECT pending FROM transcript_reset_schema",[],|r|r.get(0)).unwrap(),0);
    }
    #[test]
    fn kernel_signal_failure_refuses_reset_without_token_intent_or_body_mutation() {
        let mut store=Store::new().unwrap();populate(&mut store);
        let incarnation=store.source_incarnation;
        let canonical=store.canonical_generation;
        let owner=store.raw_sources.nonce;
        let previous=store.source_revocation.probe().unwrap();
        let body:Vec<u8>=store.db.query_row("SELECT data FROM chunks WHERE id='private'",[],|r|r.get(0)).unwrap();
        store.source_revocation.inject_revoke_failure_for_test().unwrap();
        let error=store.apply(Event::Reset {reason:"must not mutate".into()}).unwrap_err();
        assert!(error.contains("revocation was not confirmed"));
        assert_eq!(store.source_incarnation,incarnation);
        assert_eq!(store.canonical_generation,canonical);
        assert_eq!(store.raw_sources.nonce,owner);
        assert!(!store.reset_pending);
        assert_eq!(store.db.query_row::<Vec<u8>,_,_>("SELECT data FROM chunks WHERE id='private'",[],|r|r.get(0)).unwrap(),body);
        assert_eq!(store.db.query_row::<i64,_,_>("SELECT pending FROM transcript_reset_schema",[],|r|r.get(0)).unwrap(),0);
        previous.check().unwrap(); // Honest refusal: the old exported FD was NOT revoked.
        assert!(store.source_guard().is_err());
        assert!(store.source_authority().is_none());
        assert_eq!(store.source_authority_unavailable(),Some(crate::chat::SourceAuthorityUnavailableCause::RevocationFailed));
    }
    #[test]
    fn reader_originated_poison_revokes_the_actual_epoch_or_reports_exact_failure() {
        for fail in [false,true] {
            let mut store=Store::new().unwrap();populate(&mut store);
            let mut state=crate::chat::Snapshot::default();store.publish_source_authority(&mut state);
            let original=store.source_authority().unwrap();
            let previous=store.source_revocation.probe().unwrap();
            if fail {store.source_revocation.inject_revoke_failure_for_test().unwrap();}
            store.deny_source_release_for_test();
            let original_error=store.source_guard().unwrap_err();
            store.publish_source_authority(&mut state);
            assert_eq!(state.error,Some(original_error),"kernel disposition must not overwrite the actual cache error");
            assert!(state.items.is_empty());
            if fail {
                assert_eq!(store.source_incarnation,original.incarnation);
                assert!(store.source_authority().is_none());
                assert_eq!(store.source_authority_unavailable(),Some(crate::chat::SourceAuthorityUnavailableCause::RevocationFailed));
                previous.check().unwrap();
            } else {
                assert!(matches!(previous.check(),Err(Error::Revoked)));
                assert_ne!(store.source_authority().unwrap().incarnation,original.incarnation);
            }
        }
    }
}
