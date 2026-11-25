//! Per-connection framing for the retained helper's serve loop.
//!
//! Each accepted client is read, authorized and answered on its own thread
//! within its own deadline, so a client that is slow to send its request or
//! to read its reply holds up only itself. The serve loop still runs requests
//! one at a time, in the order they finish arriving: every protocol fence
//! (epoch, proof, installation, retirement) keeps its single-writer ordering.
//! An authenticated complete request runs after its client hangs up. On
//! Darwin the credentials needed for that proof can disappear at disconnect.
#[cfg(target_os = "linux")]
use super::authorize;
use super::{Invocation, MAX_FRAME, TIMEOUT, receive, send};
use crate::agent_execution::DirectoryIdentity;
use crossbeam_channel::{Receiver, Sender};
use serde_json::Value;
use std::{
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsFd, AsRawFd, RawFd},
        unix::net::UnixStream,
    },
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

/// Clients served at once. Beyond this, new clients wait in the listen
/// backlog until a connection finishes.
pub(super) const CAPACITY: usize = 32;
/// How long a worker waits for another client before exiting.
const IDLE: Duration = Duration::from_secs(30);

/// One authorized request waiting for the serve loop.
pub(super) struct Call {
    request: Invocation,
    reply: Reply,
    queued: Instant,
}
impl Call {
    pub(super) fn method(&self) -> &str {
        &self.request.method
    }
    /// Start timing the call's turn in the serve loop. Its client's deadline
    /// covers the wait in the queue as well as the run, so a slow call is
    /// reported with both when the returned guard is dropped.
    pub(super) fn timing(&self) -> Timing {
        Timing {
            method: self.request.method.clone(),
            queued: self.queued,
            started: Instant::now(),
        }
    }
    pub(super) fn into_parts(self) -> (Invocation, Reply) {
        (self.request, self.reply)
    }
}

/// Where a call's result goes. Dropping it unanswered closes the connection.
pub(super) struct Reply {
    answer: Sender<Answer>,
    delivered: Receiver<()>,
}
impl Reply {
    /// Hand the result to the connection's thread to send. The returned
    /// [`Delivery`] waits until that send has finished or failed.
    pub(super) fn send(self, answer: Answer) -> Delivery {
        let _ = self.answer.send(answer);
        Delivery(self.delivered)
    }
}

/// A call whose wait and run together take this much of its client's
/// `TIMEOUT` is logged, so a missed deadline can be traced to the queue or to
/// the request itself.
const SLOW_CALL: Duration = Duration::from_secs(1);

/// Reports a slow call when dropped. See [`Call::timing`].
pub(super) struct Timing {
    method: String,
    queued: Instant,
    started: Instant,
}
impl Drop for Timing {
    fn drop(&mut self) {
        let waited = self.started.duration_since(self.queued);
        let ran = self.started.elapsed();
        if waited + ran >= SLOW_CALL {
            tracing::warn!(
                method = %self.method,
                waited_ms = waited.as_millis() as u64,
                ran_ms = ran.as_millis() as u64,
                "a retained request was slow to answer"
            );
        }
    }
}

pub(super) struct Delivery(Receiver<()>);
impl Delivery {
    /// Bounded by the connection's own send deadline.
    pub(super) fn wait(self) {
        let _ = self.0.recv();
    }
}

pub(super) enum Answer {
    Value(Result<Value, String>),
    Watch(Result<UnixStream, String>),
    Binary(Result<Box<crate::terminal_ipc::ServerResponse>, String>),
    /// `pinned_directory` sends its directory descriptor beside the frame.
    Directory(Result<(DirectoryIdentity, Arc<File>), String>),
}

pub(super) struct Connections {
    calls: Receiver<Call>,
    woken: UnixStream,
    pool: Arc<Pool>,
}
impl Connections {
    pub(super) fn new() -> Result<Self, String> {
        let (wake, woken) = UnixStream::pair().map_err(|error| error.to_string())?;
        for end in [&wake, &woken] {
            end.set_nonblocking(true)
                .map_err(|error| error.to_string())?;
        }
        let (sender, calls) = crossbeam_channel::unbounded();
        let (handoff, accepted) = crossbeam_channel::unbounded();
        Ok(Self {
            calls,
            woken,
            pool: Arc::new(Pool {
                workers: Mutex::new(Workers { idle: 0, total: 0 }),
                handoff,
                accepted,
                calls: sender,
                wake,
            }),
        })
    }
    /// Readable when a call is queued or a full helper has room again.
    pub(super) fn wakeup(&self) -> RawFd {
        self.woken.as_raw_fd()
    }
    pub(super) fn has_capacity(&self) -> bool {
        self.pool.lock().busy() < CAPACITY
    }
    /// Read, authorize and answer `control` on a worker of its own: an idle
    /// one if there is one, otherwise a new one.
    pub(super) fn admit(&self, control: UnixStream) {
        let connection = Accepted::new(control);
        let mut workers = self.pool.lock();
        if workers.idle > 0 {
            // Sent under the lock, so a worker timing out sees it waiting.
            workers.idle -= 1;
            let _ = self.pool.handoff.send(connection);
            return;
        }
        workers.total += 1;
        drop(workers);
        let pool = self.pool.clone();
        // A failed spawn drops the connection; the client sees it closed,
        // exactly as if the helper had refused it.
        if thread::Builder::new()
            .name("retained-client".into())
            .spawn(move || pool.work(connection))
            .is_err()
        {
            self.pool.lock().total -= 1;
        }
    }
    /// Idle and total workers.
    #[cfg(test)]
    pub(super) fn workers(&self) -> (usize, usize) {
        let workers = self.pool.lock();
        (workers.idle, workers.total)
    }
    /// The next queued call, if any. Clears pending wakeups first, so a call
    /// queued after this returns `None` makes the wakeup readable again.
    pub(super) fn next(&self) -> Option<Call> {
        let mut buffer = [0; 64];
        while (&self.woken).read(&mut buffer).is_ok_and(|read| read > 0) {}
        self.calls.try_recv().ok()
    }
}

fn wake(wake: &UnixStream) {
    // A full buffer already holds a pending wakeup.
    let _ = (&*wake).write(&[1]);
}

/// Workers kept between clients, so a helper that answers several polls a
/// second does not start a thread for each.
struct Pool {
    workers: Mutex<Workers>,
    handoff: Sender<Accepted>,
    accepted: Receiver<Accepted>,
    calls: Sender<Call>,
    wake: UnixStream,
}
struct Workers {
    idle: usize,
    total: usize,
}
impl Workers {
    fn busy(&self) -> usize {
        self.total - self.idle
    }
}
impl Pool {
    fn lock(&self) -> std::sync::MutexGuard<'_, Workers> {
        self.workers.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn work(&self, mut accepted: Accepted) {
        loop {
            connection(accepted, &self.calls, &self.wake);
            {
                let mut workers = self.lock();
                // Only a full helper stopped watching its listener.
                if workers.busy() == CAPACITY {
                    wake(&self.wake);
                }
                workers.idle += 1;
            }
            accepted = match self.accepted.recv_timeout(IDLE) {
                Ok(accepted) => accepted,
                Err(_) => {
                    let mut workers = self.lock();
                    match self.accepted.try_recv() {
                        Ok(accepted) => accepted,
                        Err(_) => {
                            workers.idle -= 1;
                            workers.total -= 1;
                            return;
                        }
                    }
                }
            };
        }
    }
}

struct Accepted {
    control: UnixStream,
    #[cfg(target_os = "macos")]
    peer: std::io::Result<(crate::pinned_process::PeerCredentials, u64)>,
}

impl Accepted {
    fn new(control: UnixStream) -> Self {
        // Save the kernel's candidate before handing this socket to a
        // worker. XNU does not give the peer PID an atomic epoch, but a
        // start sample prevents reuse after admission from gaining authority.
        #[cfg(target_os = "macos")]
        let peer = crate::pinned_process::peer_credentials(&control).and_then(|credentials| {
            crate::pinned_process::started_ticks_of(credentials.pid)
                .map(|start| (credentials, start))
        });
        Self {
            control,
            #[cfg(target_os = "macos")]
            peer,
        }
    }

    #[cfg(target_os = "linux")]
    fn authorize(&self, proof: &crate::model::HostSessionProof) -> Result<(), String> {
        authorize(&self.control, proof)
    }

    #[cfg(target_os = "macos")]
    fn authorize(&self, proof: &crate::model::HostSessionProof) -> Result<(), String> {
        use crate::pinned_process::peer_credentials;

        let &(credentials, start) = super::authority_result(self.peer.as_ref())?;
        match peer_credentials(&self.control) {
            Ok(current) if current == credentials => {}
            Ok(_) => return Err("CAGENTS_ACTOR_AUTHORITY_UNCONFIRMED: Retained actor socket peer changed after admission".into()),
            Err(error)
                if error.raw_os_error() == Some(libc::ENOTCONN)
                    && self.read_side_ended() => {}
            Err(error) => return super::authority_result(Err(error)),
        }
        let paths = crate::daemon::RuntimePaths::discover().map_err(|error| error.to_string())?;
        let (pid, owner) =
            super::authority_result(crate::daemon::lease_owner_process(&paths.lock, proof))?;
        if credentials.uid != crate::daemon::current_uid()
            || credentials.pid != pid
            || owner.started_ticks() != start
            || super::authority_result(owner.exited())?
        {
            return Err("CAGENTS_ACTOR_AUTHORITY_UNCONFIRMED: Retained actor caller does not own the current daemon lease".into());
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn read_side_ended(&self) -> bool {
        let mut byte = 0u8;
        // SAFETY: the descriptor is live, and the read-only peek writes at
        // most one byte. The flag prevents waiting for a client still open.
        unsafe {
            libc::recv(
                self.control.as_raw_fd(),
                (&raw mut byte).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            ) == 0
        }
    }
}

fn connection(accepted: Accepted, calls: &Sender<Call>, wakeup: &UnixStream) {
    let control = &accepted.control;
    let request = receive::<Invocation>(control, TIMEOUT).and_then(|request| {
        accepted.authorize(&request.proof)?;
        Ok(request)
    });
    let request = match request {
        Ok(request) => request,
        // Authentication failures use the same result boundary as refusals.
        Err(error) => {
            let _ = send(control, &Err::<Value, _>(error), TIMEOUT);
            return;
        }
    };
    let (answer, answered) = crossbeam_channel::bounded(1);
    // Held until this thread returns, so the serve loop can wait for a
    // retirement reply to reach the client before the helper exits.
    let (_delivering, delivered) = crossbeam_channel::bounded::<()>(0);
    let call = Call {
        request,
        reply: Reply { answer, delivered },
        queued: Instant::now(),
    };
    if calls.send(call).is_err() {
        return;
    }
    wake(wakeup);
    // A dropped reply means the helper stopped serving or the client left.
    match answered.recv() {
        Ok(Answer::Value(response)) => {
            let _ = send(control, &response, TIMEOUT);
        }
        Ok(Answer::Binary(response)) => {
            let _ =
                super::deadline_io::send_binary_until(control, &response, Instant::now() + TIMEOUT);
        }
        Ok(Answer::Watch(watch)) => {
            let response = watch.as_ref().map(|_| ()).map_err(Clone::clone);
            let fds: Vec<_> = watch
                .as_ref()
                .ok()
                .map(|watch| vec![watch.as_fd()])
                .unwrap_or_default();
            let _ = crate::descriptor_frame::send_json_with_fds_until(
                control,
                &response,
                &fds,
                Instant::now() + TIMEOUT,
                MAX_FRAME,
                1,
                crate::descriptor_frame::FrameByteOrder::LittleEndian,
            );
        }
        Ok(Answer::Directory(pin)) => {
            let response = pin
                .as_ref()
                .map(|(identity, _)| *identity)
                .map_err(Clone::clone);
            let descriptors: Vec<_> = pin
                .as_ref()
                .ok()
                .map(|(_, file)| vec![file.as_fd()])
                .unwrap_or_default();
            let _ = crate::descriptor_frame::send_json_with_fds_until(
                control,
                &response,
                &descriptors,
                Instant::now() + TIMEOUT,
                MAX_FRAME,
                1,
                crate::descriptor_frame::FrameByteOrder::LittleEndian,
            );
        }
        Err(_) => {}
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::chat::runtime::retained_actor::transport::connection_tests::{
        isolated, leased_proof,
    };

    #[test]
    fn a_captured_peer_epoch_cannot_authorize_a_replacement_lease_owner() {
        isolated("CAGENTS_RETAINED_PEER_EPOCH_TEST", || {
            let mut proof = crate::model::HostSessionProof {
                host_session_id: "peer-epoch-session".into(),
                boot_id: crate::daemon::read_boot_id().unwrap(),
                host_incarnation: "peer-epoch-owner".into(),
            };
            let _lease = leased_proof(&mut proof);
            let (_client, server) = UnixStream::pair().unwrap();
            let mut accepted = Accepted::new(server);
            accepted.authorize(&proof).unwrap();
            accepted.peer.as_mut().unwrap().1 ^= 1;
            let refusal = accepted.authorize(&proof).unwrap_err();
            assert!(refusal.contains("does not own the current daemon lease"));
        });
    }
}
