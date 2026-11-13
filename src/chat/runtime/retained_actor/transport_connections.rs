//! Per-connection framing for the retained helper's serve loop.
//!
//! Each accepted client is read, authorized and answered on its own thread
//! within its own deadline, so a client that is slow to send its request or
//! to read its reply holds up only itself. The serve loop still runs requests
//! one at a time, in the order they finish arriving: every protocol fence
//! (epoch, proof, installation, retirement) keeps its single-writer ordering.
//! A request that arrived in full runs even if its client has since hung up:
//! an input's sender stops waiting for the reply, not for the input's receipt.
use super::{Invocation, MAX_FRAME, TIMEOUT, authorize, receive, send};
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
}
impl Call {
    pub(super) fn method(&self) -> &str {
        &self.request.method
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
    Binary(Result<crate::terminal_ipc::ServerResponse, String>),
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
        let mut workers = self.pool.lock();
        if workers.idle > 0 {
            // Sent under the lock, so a worker timing out sees it waiting.
            workers.idle -= 1;
            let _ = self.pool.handoff.send(control);
            return;
        }
        workers.total += 1;
        drop(workers);
        let pool = self.pool.clone();
        // A failed spawn drops the connection; the client sees it closed,
        // exactly as if the helper had refused it.
        if thread::Builder::new()
            .name("retained-client".into())
            .spawn(move || pool.work(control))
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
    handoff: Sender<UnixStream>,
    accepted: Receiver<UnixStream>,
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
    fn work(&self, mut control: UnixStream) {
        loop {
            connection(control, &self.calls, &self.wake);
            {
                let mut workers = self.lock();
                // Only a full helper stopped watching its listener.
                if workers.busy() == CAPACITY {
                    wake(&self.wake);
                }
                workers.idle += 1;
            }
            control = match self.accepted.recv_timeout(IDLE) {
                Ok(control) => control,
                Err(_) => {
                    let mut workers = self.lock();
                    match self.accepted.try_recv() {
                        Ok(control) => control,
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

fn connection(control: UnixStream, calls: &Sender<Call>, wakeup: &UnixStream) {
    let request = receive::<Invocation>(&control, TIMEOUT).and_then(|request| {
        authorize(&control, &request.proof)?;
        Ok(request)
    });
    let request = match request {
        Ok(request) => request,
        // Authentication failures use the same result boundary as refusals.
        Err(error) => {
            let _ = send(&control, &Err::<Value, _>(error), TIMEOUT);
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
    };
    if calls.send(call).is_err() {
        return;
    }
    wake(wakeup);
    // A dropped reply means the helper stopped serving or the client left.
    match answered.recv() {
        Ok(Answer::Value(response)) => {
            let _ = send(&control, &response, TIMEOUT);
        }
        Ok(Answer::Binary(response)) => {
            let _ = super::deadline_io::send_binary_until(
                &control,
                &response,
                Instant::now() + TIMEOUT,
            );
        }
        Ok(Answer::Watch(watch)) => {
            let response = watch.as_ref().map(|_| ()).map_err(Clone::clone);
            let fds: Vec<_> = watch
                .as_ref()
                .ok()
                .map(|watch| vec![watch.as_fd()])
                .unwrap_or_default();
            let _ = crate::descriptor_frame::send_json_with_fds_until(
                &control,
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
                &control,
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
