//! Per-connection framing for the retained helper's serve loop.
//!
//! Each accepted client is read, authorized and answered on its own thread
//! within its own deadline. A dedicated acceptor admits connections even when
//! the serve loop is busy. Writes go through that loop in arrival order. An
//! authenticated complete write runs after its client hangs up. On Darwin
//! the credentials needed for that proof can disappear at disconnect.
#[cfg(target_os = "linux")]
use super::authorize;
use super::{Invocation, MAX_FRAME, TIMEOUT, reads::Reads, receive, send};
use crate::agent_execution::DirectoryIdentity;
use crossbeam_channel::{Receiver, Sender};
use serde_json::Value;
use std::{
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsFd, AsRawFd, RawFd},
        unix::net::{UnixListener, UnixStream},
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
    /// No safe transcript publication is ready. Closing before any payload
    /// takes the same bounded retry path on old and new daemons.
    Retry,
    Value(Result<Value, String>),
    Watch(Result<UnixStream, String>),
    Binary(Result<Box<crate::terminal_ipc::ServerResponse>, String>),
    /// `pinned_directory` sends its directory descriptor beside the frame.
    Directory(Result<(DirectoryIdentity, Arc<File>), String>),
}

pub(super) struct Connections {
    calls: Receiver<Call>,
    woken: UnixStream,
    capacity_ready: UnixStream,
    pool: Arc<Pool>,
}
impl Connections {
    pub(super) fn new() -> Result<Self, String> {
        let (wake, woken) = wake_pair()?;
        let (capacity, capacity_ready) = wake_pair()?;
        let (sender, calls) = crossbeam_channel::unbounded();
        let (handoff, accepted) = crossbeam_channel::unbounded();
        Ok(Self {
            calls,
            woken,
            capacity_ready,
            pool: Arc::new(Pool {
                workers: Mutex::new(Workers { idle: 0, total: 0 }),
                handoff,
                accepted,
                calls: sender,
                wake,
                capacity,
            }),
        })
    }
    /// Readable when a call is queued or the acceptor failed.
    pub(super) fn wakeup(&self) -> RawFd {
        self.woken.as_raw_fd()
    }
    pub(super) fn listen(
        &self,
        listener: &UnixListener,
        reads: Arc<Reads>,
    ) -> Result<Listening, String> {
        let listener = listener.try_clone().map_err(|error| error.to_string())?;
        let capacity = self
            .capacity_ready
            .try_clone()
            .map_err(|error| error.to_string())?;
        let (stop, stopped) = wake_pair()?;
        let pool = self.pool.clone();
        let error = Arc::new(Mutex::new(None));
        let failure = error.clone();
        let accept_reads = reads.clone();
        let worker = thread::Builder::new()
            .name("retained-listener".into())
            .spawn(move || {
                if let Err(error) = accept(&listener, &pool, &stopped, &capacity, &accept_reads) {
                    *failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error.to_string());
                    wake(&pool.wake);
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Listening {
            stop: Some(stop),
            error,
            reads,
            worker: Some(worker),
        })
    }

    #[cfg(test)]
    pub(super) fn admit(&self, control: UnixStream) {
        self.pool.admit(control, None);
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
        drain(&self.woken);
        self.calls.try_recv().ok()
    }
}

pub(super) struct Listening {
    stop: Option<UnixStream>,
    error: Arc<Mutex<Option<String>>>,
    reads: Arc<Reads>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Listening {
    pub(super) fn check(&self) -> Result<(), String> {
        self.error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .map_or(Ok(()), Err)
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.stop.take();
        self.reads.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn accept(
    listener: &UnixListener,
    pool: &Arc<Pool>,
    stopped: &UnixStream,
    capacity: &UnixStream,
    reads: &Arc<Reads>,
) -> std::io::Result<()> {
    loop {
        drain(capacity);
        let accepting = pool.has_capacity();
        let mut events = [
            libc::pollfd {
                fd: stopped.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: capacity.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if accepting { listener.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: poll borrows the initialized array and the sockets live
        // through the call.
        if unsafe { libc::poll(events.as_mut_ptr(), events.len() as libc::nfds_t, -1) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if events[0].revents != 0 {
            return Ok(());
        }
        if events[2].revents != 0 {
            // Return to poll after each admission so a steady stream of
            // clients cannot keep shutdown from waking the acceptor.
            match crate::descriptor::accept(listener) {
                Ok((control, _)) => pool.admit(control, Some(reads.clone())),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

fn wake_pair() -> Result<(UnixStream, UnixStream), String> {
    let (writer, reader) = UnixStream::pair().map_err(|error| error.to_string())?;
    writer
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    reader
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    Ok((writer, reader))
}

fn drain(stream: &UnixStream) {
    let mut buffer = [0; 64];
    while (&*stream).read(&mut buffer).is_ok_and(|read| read > 0) {}
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
    capacity: UnixStream,
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
    fn has_capacity(&self) -> bool {
        self.lock().busy() < CAPACITY
    }
    /// Read, authorize and answer `control` on a worker of its own: an idle
    /// one if there is one, otherwise a new one.
    fn admit(self: &Arc<Self>, control: UnixStream, reads: Option<Arc<Reads>>) {
        let connection = Accepted::new(control, reads);
        let mut workers = self.lock();
        if workers.idle > 0 {
            // Sent under the lock, so a worker timing out sees it waiting.
            workers.idle -= 1;
            let _ = self.handoff.send(connection);
            return;
        }
        workers.total += 1;
        drop(workers);
        let pool = self.clone();
        // A failed spawn drops the connection; the client sees it closed,
        // exactly as if the helper had refused it.
        if thread::Builder::new()
            .name("retained-client".into())
            .spawn(move || pool.work(connection))
            .is_err()
        {
            self.lock().total -= 1;
        }
    }
    fn work(&self, mut accepted: Accepted) {
        loop {
            connection(accepted, &self.calls, &self.wake);
            {
                let mut workers = self.lock();
                // Only a full helper stopped watching its listener.
                if workers.busy() == CAPACITY {
                    wake(&self.capacity);
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
    reads: Option<Arc<Reads>>,
    #[cfg(target_os = "macos")]
    peer: std::io::Result<(crate::pinned_process::PeerCredentials, u64)>,
}

impl Accepted {
    fn new(control: UnixStream, reads: Option<Arc<Reads>>) -> Self {
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
            reads,
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
    let started = Instant::now();
    if let Some(reads) = accepted.reads
        && let Some(answer) = reads.answer(&request, || wake(wakeup))
    {
        drop(Timing {
            method: request.method,
            queued: started,
            started,
        });
        respond(control, answer);
        return;
    }
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
    if let Ok(answer) = answered.recv() {
        respond(control, answer);
    }
}

fn respond(control: &UnixStream, answer: Answer) {
    match answer {
        Answer::Retry => {}
        Answer::Value(response) => {
            let _ = send(control, &response, TIMEOUT);
        }
        Answer::Binary(response) => {
            let _ =
                super::deadline_io::send_binary_until(control, &response, Instant::now() + TIMEOUT);
        }
        Answer::Watch(watch) => {
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
        Answer::Directory(pin) => {
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
            let (client, server) = UnixStream::pair().unwrap();
            let mut accepted = Accepted::new(server, None);
            accepted.authorize(&proof).unwrap();
            accepted.peer.as_mut().unwrap().1 ^= 1;
            let refusal = accepted.authorize(&proof).unwrap_err();
            assert!(refusal.contains("does not own the current daemon lease"));
            accepted.peer.as_mut().unwrap().1 ^= 1;
            drop(client);
            accepted.authorize(&proof).unwrap();
            let unobserved = Accepted::new(accepted.control, None);
            // XNU can retain the last PID after disconnect. Where it no
            // longer supplies one, a late admission has no fallback proof.
            if unobserved.peer.is_err() {
                assert!(unobserved.authorize(&proof).is_err());
            }
        });
    }
}
