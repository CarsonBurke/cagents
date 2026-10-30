//! OS thread exit can follow is_finished (notably TLS destructors). Keep
//! actual joins off callers with deadlines and retain the exact actor owner.
use super::LocalHandle as Handle;
use std::sync::{OnceLock, atomic::Ordering};

struct Task {
    owner: Handle,
    worker: std::thread::JoinHandle<()>,
}
static SERVICE: OnceLock<Result<crossbeam_channel::Sender<Task>, String>> = OnceLock::new();
const CAPACITY: usize = 64;
const WORKERS: usize = 2;

pub(super) fn request(
    owner: &Handle,
    slot: &mut Option<std::thread::JoinHandle<()>>,
) -> Result<(), String> {
    let service = SERVICE
        .get_or_init(|| {
            let (sender, receiver) = crossbeam_channel::bounded::<Task>(CAPACITY);
            for index in 0..WORKERS {
                let receiver = receiver.clone();
                std::thread::Builder::new()
                    .name(format!("cagents-chat-join-{index}"))
                    .spawn(move || {
                        while let Ok(Task { owner, worker }) = receiver.recv() {
                            if worker.join().is_err() {
                                owner.0.completed.store(false, Ordering::Release);
                                owner.0.worker_join_failed.store(true, Ordering::Release);
                            }
                            owner.0.worker_joining.store(false, Ordering::Release);
                        }
                    })
                    .map_err(|error| {
                        format!("Native worker join service is unavailable: {error}")
                    })?;
            }
            Ok(sender)
        })
        .as_ref()
        .map_err(Clone::clone)?;
    let Some(worker) = slot.take() else {
        return Ok(());
    };
    owner.0.worker_joining.store(true, Ordering::Release);
    match service.try_send(Task {
        owner: owner.clone(),
        worker,
    }) {
        Ok(()) => Ok(()),
        Err(error) => {
            let task = error.into_inner();
            *slot = Some(task.worker);
            owner.0.worker_joining.store(false, Ordering::Release);
            Err("Native worker join capacity is busy; the exact owner is retained for retry".into())
        }
    }
}
