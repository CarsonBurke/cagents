//! Bounded final semantic work after every provider received close admission.
//! This service only revisits providers with successful close admission. It
//! never removes an unproved owner,
//! or creates a worker per pane. A batch retains the full snapshot, including
//! providers whose close admission returned uncertainty.
use super::{SharedHost, agent_execution, chat};
use crate::chat::{Session, runtime::Handle};
use std::{collections::VecDeque, sync::{Arc, Mutex, OnceLock}, time::Instant};

const WORKERS: usize = 2;
// This bounds queued worker tickets, not the number of sources in one daemon.
const BATCH_TICKETS: usize = 64;
type Providers = Vec<(Session, Handle)>;
type Reply = (usize, Result<(), String>);
struct Batch {
    host: SharedHost,
    providers: Providers,
    pending: Mutex<VecDeque<usize>>,
    replies: crossbeam_channel::Sender<Reply>,
}
static SERVICE: OnceLock<Result<crossbeam_channel::Sender<Arc<Batch>>, String>> = OnceLock::new();

fn service() -> &'static Result<crossbeam_channel::Sender<Arc<Batch>>, String> {
    SERVICE.get_or_init(|| {
        let (sender, receiver) = crossbeam_channel::bounded::<Arc<Batch>>(BATCH_TICKETS);
        for index in 0..WORKERS {
            let receiver = receiver.clone();
            std::thread::Builder::new().name(format!("cagentsd-final-outcomes-{index}"))
                .spawn(move || {
                    while let Ok(batch) = receiver.recv() {
                        loop {
                            let next = batch.pending.lock().unwrap_or_else(|p| p.into_inner()).pop_front();
                            let Some(index) = next else { break; };
                            let (source, handle) = &batch.providers[index];
                            // No queue or HostState guard crosses semantic IO.
                            // The original record/handle stays in the batch and
                            // HostState even if a caller's deadline expires.
                            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                // Direct admission proved provider cleanup without
                                // waiting on proxy metadata. Publish the original
                                // final snapshot/RetainedOutcomes before draining
                                // and acknowledging the helper's durable journal.
                                handle.close()?;
                                chat::retained_actor::finish_close(&batch.host, handle, source)?;
                                agent_execution::retired(&batch.host, source)
                            })).unwrap_or_else(|_| Err("Final provider retirement panicked; its exact owner remains unconfirmed".into()));
                            let _ = batch.replies.try_send((index, result));
                        }
                        crate::daemon::wake();
                    }
                }).map_err(|error| format!("Final provider cleanup service is unavailable: {error}"))?;
        }
        Ok(sender)
    })
}

pub(super) fn finish_until(
    host: SharedHost,
    providers: Providers,
    ready: Vec<usize>,
    deadline: Instant,
) -> Result<(), String> {
    if ready.is_empty() { return Ok(()); }
    let service = service().as_ref().map_err(Clone::clone)?;
    let mut waiting = ready.iter().copied().collect::<std::collections::BTreeSet<_>>();
    let (replies, receive) = crossbeam_channel::bounded(ready.len());
    let batch = Arc::new(Batch { host, providers, pending: Mutex::new(ready.into()), replies });
    let mut admitted = 0;
    for _ in 0..WORKERS {
        if service.try_send(batch.clone()).is_ok() { admitted += 1; }
    }
    if admitted == 0 {
        return Err("Final provider cleanup capacity is busy; all exact session records and helper journals remain retained".into());
    }
    let mut failure = None;
    while !waiting.is_empty() {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else { break; };
        match receive.recv_timeout(remaining) {
            Ok((index, result)) => {
                waiting.remove(&index);
                if let Err(error) = result {
                    tracing::warn!(provider = %batch.providers[index].0.id, %error, "final provider retirement remains unconfirmed");
                    if failure.is_none() {
                        failure = Some(format!("Provider {} final retirement remains unconfirmed: {error}", batch.providers[index].0.id));
                    }
                }
            }
            Err(_) => break,
        }
    }
    if !waiting.is_empty() {
        let pending = waiting.into_iter().map(|index| batch.providers[index].0.id.to_string())
            .collect::<Vec<_>>().join(", ");
        let error = format!("Final provider retirement is still pending for {pending}; exact cleanup owners and helper journals remain retained");
        return Err(failure.map_or(error.clone(), |first| format!("{first}; {error}")));
    }
    match failure { Some(error) => Err(error), None => Ok(()) }
}
