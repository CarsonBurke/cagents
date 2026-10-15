//! Bounded ownership transfer for final frontend drops. Never inline RPC or
//! joins on the caller, and never kill an owner whose family is unconfirmed.
use super::{Holder, Duration, Instant};
use std::sync::OnceLock;
enum Task {
    Final(Holder),
    Completed {
        handle: super::super::TerminalHandle,
        deadline: Instant,
        reply: std::sync::mpsc::SyncSender<Result<(), String>>,
    },
}
static SERVICE: OnceLock<Result<crossbeam_channel::Sender<Task>, String>> = OnceLock::new();

fn service() -> Result<&'static crossbeam_channel::Sender<Task>, String> {
    SERVICE.get_or_init(|| {
        let (sender, receiver) = crossbeam_channel::bounded::<Task>(64);
        for index in 0..2 {
            let receiver = receiver.clone();
            std::thread::Builder::new().name(format!("terminal-holder-close-{index}")).spawn(move || {
                while let Ok(task) = receiver.recv() {
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
                            let result = if handle.join_worker_until(deadline, false) {
                                handle.release_completed_holder_until(deadline)
                            } else {
                                Err("Completed terminal worker cleanup remains pending; retry its retained owner".into())
                            };
                            let _ = reply.send(result);
                        }
                    }
                    crate::daemon::wake();
                }
            }).map_err(|error| error.to_string())?;
        }
        Ok(sender)
    }).as_ref().map_err(Clone::clone)
}

pub(crate) fn completed(handle: &super::super::TerminalHandle) -> Result<std::sync::mpsc::Receiver<Result<(), String>>, String> {
    if !handle.is_completed() {
        return Err("Terminal is still running; completed cleanup cannot retire it".into());
    }
    let (reply, result) = std::sync::mpsc::sync_channel(1);
    let task = Task::Completed {
        handle: handle.clone(),
        deadline: Instant::now()+Duration::from_secs(3),
        reply,
    };
    service()?.try_send(task).map_err(|_| "Terminal cleanup capacity is busy; its exact record remains retained".to_owned())?;
    Ok(result)
}

pub(super) fn request(mut holder: Holder) {
    match service() {
        Ok(sender) => match sender.try_send(Task::Final(holder)) {
            Ok(()) => return,
            Err(error) => match error.into_inner() {
                Task::Final(returned) => holder = returned,
                Task::Completed { .. } => unreachable!("only the submitted final holder can be returned"),
            },
        },
        Err(error) => tracing::warn!(%error,"Terminal cleanup service unavailable"),
    }
    // A registered helper sees daemon loss and retains discovery, directory
    // pin and setup flock for a successor. Absence is never cleanup proof.
    holder.leave_for_successor();
}
