//! Bounded ownership transfer for final frontend drops. Never inline RPC or
//! joins on the caller, and never kill an owner whose family is unconfirmed.
use super::{Holder, Duration, Instant};
use super::super::CompletedRelease;
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
    fn drop(&mut self) { crate::daemon::wake(); }
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

pub(super) fn initialize() -> Result<(),String> {service().map(|_|())}

pub(crate) fn completed(handle: &super::super::TerminalHandle) -> Result<std::sync::mpsc::Receiver<CompletedRelease>, String> {
    completed_until(handle, Instant::now()+Duration::from_secs(3))
}

fn completed_until(handle: &super::super::TerminalHandle, deadline: Instant) -> Result<std::sync::mpsc::Receiver<CompletedRelease>, String> {
    if !handle.is_completed() {
        return Err("Terminal is still running; completed cleanup cannot retire it".into());
    }
    let (reply, result) = std::sync::mpsc::sync_channel(1);
    let task = Task::Completed {
        handle: handle.clone(),
        deadline,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession}, terminal::{TerminalConfig, TerminalHandle}};
    use std::{fs::File, sync::TryLockError, thread};

    fn completed_terminal() -> TerminalHandle {
        let launch = LaunchRecipe {program:"/bin/sh".into(), arguments:vec!["-c".into(),"exit 0".into()],working_directory:"/".into(),environment:Default::default(),unset_environment:Default::default()};
        let handle = TerminalHandle::spawn(TerminalSession {
            id:TerminalId::new(), launch:launch.clone(), transcript_path:None, resume_hint:None,
            continuity:TerminalContinuity::RelaunchAvailable {recipe:launch,last_exit:None},
            owned_codex_listener:None,recovery_origin:None,predecessor:None,
        },TerminalConfig::default(),|_|{}).unwrap();
        let deadline=Instant::now()+Duration::from_secs(5);
        while !handle.is_completed() {assert!(Instant::now()<deadline);thread::sleep(Duration::from_millis(5));}
        handle
    }

    #[test]
    fn queued_healthy_owner_is_deferred_after_capacity_expiry_and_retries() {
        const FIXTURE:&str="CAGENTS_TERMINAL_RELEASE_QUEUE_FIXTURE";
        if std::env::var_os(FIXTURE).is_none() {
            let result=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","terminal::worker::holder::release::tests::queued_healthy_owner_is_deferred_after_capacity_expiry_and_retries","--nocapture","--test-threads=1"])
                .env(FIXTURE,"1").output().unwrap();
            assert!(result.status.success(),"{}\n{}",String::from_utf8_lossy(&result.stdout),String::from_utf8_lossy(&result.stderr));
            return;
        }
        let handles=[completed_terminal(),completed_terminal(),completed_terminal()];
        struct Resume(Vec<File>);
        impl Drop for Resume {fn drop(&mut self){for fd in &self.0 {let _=super::super::super::close_processes::signal(fd,libc::SIGCONT);}}}
        let mut resume=Resume(Vec::new());
        for handle in &handles[..2] {
            let holder=handle.inner.published.retained_holder.lock().unwrap();
            let fd=holder.as_ref().unwrap().pidfd.as_ref().unwrap().try_clone().unwrap();
            super::super::super::close_processes::signal(&fd,libc::SIGSTOP).unwrap();
            resume.0.push(fd);
        }
        let first=completed_until(&handles[0],Instant::now()+Duration::from_secs(1)).unwrap();
        let second=completed_until(&handles[1],Instant::now()+Duration::from_secs(1)).unwrap();
        let deadline=Instant::now()+Duration::from_secs(1);
        while !handles[..2].iter().all(|handle|matches!(handle.inner.published.retained_holder.try_lock(),Err(TryLockError::WouldBlock))) {
            assert!(Instant::now()<deadline,"both actual holder closes must occupy the fixed workers");thread::sleep(Duration::from_millis(2));
        }
        let healthy=completed_until(&handles[2],Instant::now()+Duration::from_millis(20)).unwrap();
        assert_eq!(healthy.recv_timeout(Duration::from_secs(3)).unwrap(),CompletedRelease::Deferred);
        assert!(matches!(first.recv_timeout(Duration::from_secs(1)).unwrap(),CompletedRelease::Retained(_)));
        assert!(matches!(second.recv_timeout(Duration::from_secs(1)).unwrap(),CompletedRelease::Retained(_)));
        assert!(handles[2].inner.published.retained_holder.lock().unwrap().is_some(),"capacity alone cannot discard its exact healthy owner");
        drop(resume);
        for handle in &handles {
            assert_eq!(completed(handle).unwrap().recv_timeout(Duration::from_secs(5)).unwrap(),CompletedRelease::Released);
        }
    }
}
