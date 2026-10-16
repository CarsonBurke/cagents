//! A fixed, bounded owner for retained-provider cleanup. Caller deadlines do
//! not abandon holders or allocate another thread for each close attempt.
use super::Handle;
use std::sync::{OnceLock, atomic::Ordering};

const CAPACITY: usize = 64;
const WORKERS: usize = 2;
enum Task {
    Retained(Handle),
    FinalHolder(crate::chat::native_holder::Holder),
}
static SERVICE: OnceLock<Result<crossbeam_channel::Sender<Task>, String>> = OnceLock::new();

pub(super) fn request(handle: &Handle) -> Result<(), String> {
    if handle
        .0
        .native_reconciling
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(());
    }
    let service = service();
    let queued = service.as_ref().map_err(Clone::clone).and_then(|service| {
        service
            .try_send(Task::Retained(handle.clone()))
            .map_err(|_| {
                "Native cleanup capacity is busy; its exact owner is retained for retry".to_owned()
            })
    });
    if queued.is_err() {
        handle.0.native_reconciling.store(false, Ordering::Release);
    }
    queued
}

fn service() -> &'static Result<crossbeam_channel::Sender<Task>, String> {
    SERVICE.get_or_init(|| {
        let (sender,receiver)=crossbeam_channel::bounded::<Task>(CAPACITY);
        for index in 0..WORKERS {
            let receiver=receiver.clone();
            std::thread::Builder::new().name(format!("cagents-native-close-{index}")).spawn(move|| {
                while let Ok(task)=receiver.recv() {
                    let handle=match task {
                        Task::Retained(handle)=>handle,
                        Task::FinalHolder(mut holder)=> {
                            let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||holder.close()));
                            if !matches!(result,Ok(Ok(()))) {
                                // The registered exact identity remains discoverable;
                                // no last-handle destructor may retry RPC inline.
                                holder.preserve();
                                tracing::warn!("Final native owner cleanup is unconfirmed; exact registry identity retained");
                            }
                            continue;
                        }
                    };
                    // The exact Handle owns all teardown resources. No host or
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
    })
}

/// Final Handle destruction has no caller that can retain/retry it. Transfer
/// its exact holder without waiting; if capacity is unavailable, keep its
/// durable registry identity and let the helper's orphan policy own shutdown.
pub(super) fn retire_final(holder: crate::chat::native_holder::Holder) {
    let retained = match service() {
        Ok(service) => service
            .try_send(Task::FinalHolder(holder))
            .err()
            .map(|error| error.into_inner()),
        Err(_) => Some(Task::FinalHolder(holder)),
    };
    if let Some(Task::FinalHolder(holder)) = retained {
        holder.preserve();
        tracing::warn!(
            "Final native cleanup capacity unavailable; exact registry identity retained without cleanup receipt"
        );
    }
}
