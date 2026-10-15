//! Bounded ownership transfer for final frontend drops. Never inline RPC or
//! joins on the caller, and never kill an owner whose family is unconfirmed.
use super::super::CompletedRelease;
use super::{Duration, Holder, Instant};
use std::sync::OnceLock;
enum Task {
    Final(Holder),
    Completed {
        handle: super::super::TerminalHandle,
        deadline: Instant,
        reply: std::sync::mpsc::SyncSender<CompletedRelease>,
    },
}
static SERVICE: OnceLock<Result<crossbeam_channel::Sender<Task>, String>> = OnceLock::new();

// The pending host record polls only after activity. A disconnected result
// after unwinding must wake it just as a normal result or freed queue slot does.
struct CompletionWake;
impl Drop for CompletionWake {
    fn drop(&mut self) {
        crate::daemon::wake();
    }
}

fn service() -> Result<&'static crossbeam_channel::Sender<Task>, String> {
    SERVICE.get_or_init(|| {
        let (sender, receiver) = crossbeam_channel::bounded::<Task>(64);
        for index in 0..2 {
            let receiver = receiver.clone();
            std::thread::Builder::new().name(format!("terminal-holder-close-{index}")).spawn(move || {
                while let Ok(task) = receiver.recv() {
                    let _wake = CompletionWake;
                    match task {
                        Task::Final(mut holder) => {
                            if let Err(error) = holder.close_until(Instant::now()+Duration::from_secs(5)) {
                                tracing::warn!(%error,"Terminal family remains owned by its discoverable holder");
                                holder.leave_for_successor();
                            }
                        }
                        Task::Completed { handle, deadline, reply } => {
                            // Join ownership is independently retained by the
                            // join service, including a blocked TLS destructor.
                            let result = if Instant::now() >= deadline {
                                // No teardown has started. Capacity pressure
                                // must not strand an otherwise healthy record.
                                CompletedRelease::Deferred
                            } else if handle.join_worker_until(deadline, false) {
                                match handle.release_completed_holder_until(deadline) {
                                    Ok(()) => CompletedRelease::Released,
                                    Err(error) => CompletedRelease::Retained(error),
                                }
                            } else {
                                CompletedRelease::Retained("Completed terminal worker cleanup remains pending; retry its retained owner".into())
                            };
                            let _ = reply.send(result);
                        }
                    }
                }
            }).map_err(|error| error.to_string())?;
        }
        Ok(sender)
    }).as_ref().map_err(Clone::clone)
}

pub(super) fn initialize() -> Result<(), String> {
    service().map(|_| ())
}

pub(crate) fn completed(
    handle: &super::super::TerminalHandle,
) -> Result<std::sync::mpsc::Receiver<CompletedRelease>, String> {
    completed_until(handle, Instant::now() + Duration::from_secs(3))
}

/// The opaque stop receipt owns explicit whole-session admission. Unlike a
/// natural completion, root-exit uncertainty may still require the retained
/// holder's independent family proof. Never perform its I/O on the caller.
pub(crate) fn stopped_until(
    handle: &super::super::TerminalHandle,
    instance: uuid::Uuid,
    deadline: Instant,
) -> Result<std::sync::mpsc::Receiver<CompletedRelease>, String> {
    if Instant::now() >= deadline {
        return Err("Terminal stop cleanup deadline expired; retain its exact owner".into());
    }
    {
        let order =
            handle.inner.published.input_order.try_lock().map_err(|_| {
                "Terminal stop ownership is busy; retain its exact owner".to_owned()
            })?;
        if handle.execution_instance() != instance || !order.closing || order.handoff {
            return Err(
                "Terminal stop no longer owns this exact worker; its replacement was preserved"
                    .into(),
            );
        }
    }
    submit_completed(handle, deadline)
}

fn completed_until(
    handle: &super::super::TerminalHandle,
    deadline: Instant,
) -> Result<std::sync::mpsc::Receiver<CompletedRelease>, String> {
    if !handle.is_completed() {
        return Err("Terminal is still running; completed cleanup cannot retire it".into());
    }
    submit_completed(handle, deadline)
}

fn submit_completed(
    handle: &super::super::TerminalHandle,
    deadline: Instant,
) -> Result<std::sync::mpsc::Receiver<CompletedRelease>, String> {
    let (reply, result) = std::sync::mpsc::sync_channel(1);
    let task = Task::Completed {
        handle: handle.clone(),
        deadline,
        reply,
    };
    service()?.try_send(task).map_err(|_| {
        "Terminal cleanup capacity is busy; its exact record remains retained".to_owned()
    })?;
    Ok(result)
}

pub(super) fn request(mut holder: Holder) {
    match service() {
        Ok(sender) => match sender.try_send(Task::Final(holder)) {
            Ok(()) => return,
            Err(error) => match error.into_inner() {
                Task::Final(returned) => holder = returned,
                Task::Completed { .. } => {
                    unreachable!("only the submitted final holder can be returned")
                }
            },
        },
        Err(error) => tracing::warn!(%error,"Terminal cleanup service unavailable"),
    }
    // A registered helper sees daemon loss and retains discovery, directory
    // pin and setup flock for a successor. Absence is never cleanup proof.
    holder.leave_for_successor();
}

