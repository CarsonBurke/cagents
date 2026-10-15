//! The source ordering clock belongs to the retained Store, not a replacement
//! provider actor's fresh Snapshot. It never derives identity from raw nonce.
use super::*;
#[derive(Default)]
pub(super) struct Clock {
    last: Option<crate::chat::runtime::SourceAuthority>,
    floor: u64,
}
impl Store {
    /// After exact old actor completion/join, replacement callers include its
    /// final metadata generation before binding this same Store to a new actor.
    pub(in crate::chat::runtime) fn note_source_generation_floor(&mut self,generation:u64) {
        self.source_authority.floor=self.source_authority.floor.max(generation);
    }
    pub(in crate::chat::runtime) fn source_authority(&self)->Option<crate::chat::runtime::SourceAuthority> {
        self.source_authority.last.filter(|authority|authority.incarnation==self.source_incarnation)
    }
    pub(super) fn source_clock_available(&self)->bool {self.source_authority.floor<=u64::MAX-2}
    pub(super) fn record_source_authority(&mut self,state:&mut crate::chat::Snapshot)->Result<(),String> {
        self.note_source_generation_floor(state.generation);
        let clock=&mut self.source_authority;
        if clock.last.is_none_or(|last|last.incarnation!=self.source_incarnation) {
            let generation=clock.floor.checked_add(1).ok_or("Source authority clock is exhausted; Reset and source publication were refused")?;
            clock.last=Some(crate::chat::runtime::SourceAuthority {incarnation:self.source_incarnation,generation});
            clock.floor=generation;
        }
        // A new LocalHandle using this retained Store cannot restart the
        // public ordering clock below generations already observed by peers.
        state.generation=state.generation.max(clock.floor);
        Ok(())
    }
}
