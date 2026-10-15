//! The source ordering clock belongs to the retained Store, not a replacement
//! provider actor's fresh Snapshot. It never derives identity from raw nonce.
use super::*;
#[derive(Default)]
pub(super) struct Clock {
    last: Option<crate::chat::runtime::SourceAuthority>,
    floor: u64,
    unavailable:Option<crate::chat::SourceAuthorityUnavailableCause>,
    // A retained Store can outlive and replace its LocalHandle. Readers sample
    // the current observer while holding Store, then invoke it after unlock.
    observer:std::sync::Weak<crate::chat::runtime::Inner>,
}
impl Store {
    pub(super) fn refuse_source_revocation(&mut self) {
        self.source_authority.unavailable.get_or_insert(crate::chat::SourceAuthorityUnavailableCause::RevocationFailed);
        self.source_reads.poison();
    }

    pub(in crate::chat::runtime) fn register_source_authority_observer(&mut self,observer:std::sync::Weak<crate::chat::runtime::Inner>) {
        self.source_authority.observer=observer;
    }
    pub(in crate::chat::runtime) fn source_authority_observer(&self)->std::sync::Weak<crate::chat::runtime::Inner> {
        self.source_authority.observer.clone()
    }
    /// After exact old actor completion/join, replacement callers include its
    /// final metadata generation before binding this same Store to a new actor.
    pub(in crate::chat::runtime) fn note_source_generation_floor(&mut self,generation:u64) {
        self.source_authority.floor=self.source_authority.floor.max(generation);
    }
    pub(in crate::chat::runtime) fn source_authority(&self)->Option<crate::chat::runtime::SourceAuthority> {
        self.source_authority.last.filter(|authority|self.source_authority.unavailable.is_none() && authority.incarnation==self.source_incarnation)
    }
    pub(in crate::chat::runtime) fn source_authority_unavailable(&self)->Option<crate::chat::SourceAuthorityUnavailableCause> {self.source_authority.unavailable}
    pub(super) fn source_clock_available(&self)->bool {self.source_authority.unavailable.is_none() && self.source_authority.floor<=u64::MAX-2}
    pub(super) fn record_source_authority(&mut self,state:&mut crate::chat::Snapshot)->Result<(),String> {
        if self.source_authority.unavailable.is_some() {return Err("Source authority is unavailable".into());}
        self.note_source_generation_floor(state.generation);
        let clock=&mut self.source_authority;
        if clock.last.is_none_or(|last|last.incarnation!=self.source_incarnation) {
            let Some(generation)=clock.floor.checked_add(1) else {
                clock.unavailable=Some(crate::chat::SourceAuthorityUnavailableCause::ClockExhausted);
                return Err("Source authority clock is exhausted; Reset and source publication were refused".into());
            };
            clock.last=Some(crate::chat::runtime::SourceAuthority {incarnation:self.source_incarnation,generation});
            clock.floor=generation;
        }
        // A new LocalHandle using this retained Store cannot restart the
        // public ordering clock below generations already observed by peers.
        state.generation=state.generation.max(clock.floor);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_store_replacement_keeps_exact_pair_then_fences_final_generation() {
        let mut store=Store::new().unwrap();
        let mut old=crate::chat::Snapshot::default();
        store.publish_source_authority(&mut old);
        let pair=store.source_authority().unwrap();
        old.generation=900;
        store.note_source_generation_floor(old.generation);
        let mut replacement=crate::chat::Snapshot::default();
        store.publish_source_authority(&mut replacement);
        assert_eq!(store.source_authority(),Some(pair));
        assert!(replacement.generation>=900);
        store.rotate_source_incarnation().unwrap();
        store.publish_source_authority(&mut replacement);
        let next=store.source_authority().unwrap();
        assert_ne!(next.incarnation,pair.incarnation);
        assert!(next.generation>900);
    }
    #[test]
    fn clock_exhaustion_revokes_authority_without_fabricating_a_pair_or_reset() {
        let mut store=Store::new().unwrap();
        let mut state=crate::chat::Snapshot::default();
        store.publish_source_authority(&mut state);
        let original=store.source_authority().unwrap();
        store.note_source_generation_floor(u64::MAX);
        store.source_reads.poison();
        store.publish_source_authority(&mut state);
        assert!(store.source_authority().is_none());
        assert_eq!(store.source_authority_unavailable(),Some(crate::chat::SourceAuthorityUnavailableCause::ClockExhausted));
        assert_eq!(store.source_authority.last,Some(original),"no new ordered proof was fabricated");
        assert!(store.apply(Event::Reset {reason:"cannot order this reset".into()}).is_err());
        store.publish_source_authority(&mut state);
        assert!(store.source_authority().is_none());
    }
    #[test]
    fn repeated_failed_resets_each_issue_a_strictly_newer_exact_token() {
        let mut store=Store::new().unwrap();
        let mut state=crate::chat::Snapshot::default();
        store.publish_source_authority(&mut state);
        let initial=store.source_authority().unwrap();
        store.refuse_source_reset_for_test(true);
        let mut previous=initial;
        for _ in 0..3 {
            assert!(store.apply(Event::Reset {reason:"refused but invalidated".into()}).is_err());
            store.publish_source_authority(&mut state);
            let pair=store.source_authority().unwrap();
            assert_ne!(pair.incarnation,previous.incarnation);
            assert!(pair.generation>previous.generation);
            assert_eq!(state.source_incarnation,Some(pair.incarnation));
            previous=pair;
        }
    }
}
