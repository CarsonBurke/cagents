//! A fixed, bounded owner for retained-provider cleanup. Caller deadlines do
//! not abandon holders or allocate another thread for each close attempt.
use super::LocalHandle;
use std::sync::{OnceLock, atomic::Ordering};

const CAPACITY:usize=64;
const WORKERS:usize=2;
static SERVICE:OnceLock<Result<crossbeam_channel::Sender<LocalHandle>,String>>=OnceLock::new();

pub(super) fn request(handle:&LocalHandle)->Result<(),String> {
    if handle.0.native_reconciling.compare_exchange(false,true,Ordering::AcqRel,Ordering::Acquire).is_err() {
        return Ok(());
    }
    let service=SERVICE.get_or_init(|| {
        let (sender,receiver)=crossbeam_channel::bounded::<LocalHandle>(CAPACITY);
        for index in 0..WORKERS {
            let receiver=receiver.clone();
            std::thread::Builder::new().name(format!("cagents-native-close-{index}")).spawn(move|| {
                while let Ok(handle)=receiver.recv() {
                    // The exact LocalHandle owns all teardown resources. No host or
                    // control guard enters this queue, including on panic.
                    let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||handle.close()))
                        .unwrap_or_else(|_|Err("Native cleanup panicked; its owner remains unconfirmed".into()));
                    *handle.0.native_reconcile_error.lock().unwrap_or_else(|p|p.into_inner())=
                        result.err().map(|error|crate::chat::protocol::preview(error.as_bytes(),1024));
                    handle.0.native_reconciling.store(false,Ordering::Release);
                }
            }).map_err(|error|format!("Native cleanup service is unavailable: {error}"))?;
        }
        Ok(sender)
    });
    let queued=service.as_ref().map_err(Clone::clone).and_then(|service|service.try_send(handle.clone())
        .map_err(|_|"Native cleanup capacity is busy; its exact owner is retained for retry".to_owned()));
    if queued.is_err() {handle.0.native_reconciling.store(false,Ordering::Release);}
    queued
}
