//! Nonblocking, per-session exclusion between a private console spawn and
//! changes to its durable accepted authority. This is synchronization only;
//! possession of a session ID never grants permission to execute anything.
//! This process-local guard does not replace durable startup/census evidence
//! across daemon death. Production remapping remains disabled until that
//! separate recovery owner and the exact actor/pair publication are composed.
use std::{collections::BTreeMap,sync::{Arc,Mutex,OnceLock,atomic::{AtomicBool,Ordering}}};
use crate::model::TerminalId;
const CAPACITY:usize=4096;
type Fences=BTreeMap<TerminalId,Arc<AtomicBool>>;
static FENCES:OnceLock<Mutex<Fences>>=OnceLock::new();

#[derive(Debug)]
pub(crate) struct Lease {held:Arc<AtomicBool>,requested:bool,confirmed:bool}
pub(crate) fn acquire(id:TerminalId)->Result<Lease,String> {
    let held={
        let mut fences=FENCES.get_or_init(||Mutex::new(BTreeMap::new())).lock().unwrap_or_else(|p|p.into_inner());
        fences.retain(|_,held|held.load(Ordering::Acquire));
        if let Some(held)=fences.get(&id).cloned() {held}
        else {
            if fences.len()>=CAPACITY {return Err("Console authority transition capacity is full; retry the existing session".into());}
            let held=Arc::new(AtomicBool::new(false));fences.insert(id,held.clone());held
        }
    };
    held.compare_exchange(false,true,Ordering::AcqRel,Ordering::Acquire)
        .map_err(|_|"The exact session has a console spawn or authority transition in progress; retry after its outcome is confirmed".to_owned())?;
    Ok(Lease {held,requested:false,confirmed:false})
}
impl Lease {
    pub(crate) fn mark_requested(&mut self) {self.requested=true;}
    pub(crate) fn confirm_started(&mut self) {self.confirmed=true;}
    pub(crate) fn confirm_family_closed(&mut self) {self.confirmed=true;}
}
impl Drop for Lease {
    fn drop(&mut self) {
        // Reaping the helper does not prove a root never escaped before a lost
        // Started response. Keep this session unavailable, even after the OS
        // child owner settles. No absence or timeout clears this disposition.
        if !self.requested || self.confirmed {self.held.store(false,Ordering::Release);}
        else {tracing::error!("Console spawn outcome remains unknown; its session authority transition stays quarantined");}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helper_owner_drop_after_a_requested_spawn_does_not_prove_family_cleanup() {
        let id=TerminalId::new();let mut lease=acquire(id).unwrap();lease.mark_requested();drop(lease);
        assert!(acquire(id).is_err(),"helper status alone cannot release a logical unknown-start fence");
    }
    #[test]
    fn positive_started_or_typed_family_close_releases_requested_transition() {
        for started in [true,false] {
            let id=TerminalId::new();let mut lease=acquire(id).unwrap();lease.mark_requested();
            if started {lease.confirm_started();} else {lease.confirm_family_closed();}
            drop(lease);drop(acquire(id).unwrap());
        }
    }
    #[test]
    fn same_session_refuses_without_blocking_and_unrelated_sessions_continue() {
        let id=TerminalId::new();let lease=acquire(id).unwrap();
        assert!(acquire(id).is_err());
        let other=acquire(TerminalId::new()).unwrap();drop(other);
        drop(lease);drop(acquire(id).unwrap());
    }
}
