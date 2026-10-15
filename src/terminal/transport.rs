//! Fixed transport resources, independent from PTY parsing and UI drawing.
//!
//! Socket tasks never block on I/O or host requests. CPU/blocking work uses
//! bounded admission queues and fixed pools. Input, snapshots, serialization
//! and slow domain work have independent capacity; adding a connection adds no
//! OS thread. Each connection submits at most one request and one encode at once.
use std::{
    io::{self, Read},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{Arc, OnceLock, atomic::{AtomicUsize, Ordering}},
};

use serde::de::DeserializeOwned;
use tokio::{io::unix::AsyncFd, sync::{Notify, Semaphore, oneshot}};

use super::ipc::{FrameCodec, FrameError};

type Job = Box<dyn FnOnce() + Send>;
const QUEUED_WORK: usize = 64;
#[derive(Clone, Copy)]
pub(crate) enum WorkClass { Input, Snapshot, Encode, Domain }

struct Pool { work: crossbeam_channel::Sender<Job>, admission: Arc<Semaphore> }
struct Resources {
    runtime: tokio::runtime::Runtime,
    input: OnceLock<Result<Pool, String>>,
    snapshot: OnceLock<Result<Pool, String>>,
    encode: OnceLock<Result<Pool, String>>,
    domain: OnceLock<Result<Pool, String>>,
    retire: crossbeam_channel::Sender<Job>,
    retirement: crossbeam_channel::Receiver<Job>,
    retiring: AtomicUsize,
    retired: Notify,
}

fn resources() -> Result<&'static Resources, String> {
    static RESOURCES: OnceLock<Result<Resources, String>> = OnceLock::new();
    RESOURCES.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2).thread_name("terminal-transport")
            .enable_io().enable_time().build().map_err(|error| error.to_string())?;
        let (retire, retirement) = crossbeam_channel::unbounded::<Job>();
        Ok(Resources { runtime, input: OnceLock::new(), snapshot: OnceLock::new(), encode: OnceLock::new(), domain: OnceLock::new(),
            retire, retirement, retiring: AtomicUsize::new(0), retired: Notify::new() })
    }).as_ref().map_err(Clone::clone)
}

fn pool(resources: &'static Resources, class: WorkClass) -> Result<&'static Pool, String> {
    let (slot, name, workers) = match class {
        WorkClass::Input => (&resources.input, "input", 2),
        WorkClass::Snapshot => (&resources.snapshot, "snapshot", 2),
        WorkClass::Encode => (&resources.encode, "encode", 2),
        WorkClass::Domain => (&resources.domain, "domain", 4),
    };
    slot.get_or_init(|| {
        let (work, jobs) = crossbeam_channel::unbounded::<Job>();
        let retirement = if matches!(class, WorkClass::Domain) { resources.retirement.clone() } else { crossbeam_channel::never() };
        for index in 0..workers {
            let jobs = jobs.clone();
            let retirement = retirement.clone();
            std::thread::Builder::new().name(format!("terminal-{name}-{index}"))
                .spawn(move || loop {
                    let job = crossbeam_channel::select_biased! {
                        recv(retirement) -> job => job,
                        recv(jobs) -> job => job,
                    };
                    let Ok(job) = job else { return; };
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                }).map_err(|error| error.to_string())?;
        }
        Ok(Pool { work, admission: Arc::new(Semaphore::new(QUEUED_WORK)) })
    }).as_ref().map_err(Clone::clone)
}

pub(crate) fn runtime() -> Result<&'static tokio::runtime::Runtime, String> { resources().map(|resources| &resources.runtime) }

/// Warm host resources before publishing daemon readiness or sampling its idle
/// thread count. The UI initializes only its two I/O threads.
pub(crate) fn initialize_host() -> Result<(), String> {
    let resources = resources()?;
    for class in [WorkClass::Input, WorkClass::Snapshot, WorkClass::Encode, WorkClass::Domain] { pool(resources, class)?; }
    Ok(())
}

pub(crate) async fn dispatch<T: Send + 'static>(class: WorkClass, operation: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    let pool = pool(resources()?, class)?;
    let permit = pool.admission.clone().acquire_owned().await.map_err(|error| error.to_string())?;
    let (reply, result) = oneshot::channel();
    pool.work.send(Box::new(move || {
        let _permit = permit;
        let value = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation));
        let _ = reply.send(value.map_err(|_| "transport dispatcher request panicked".to_owned()));
    })).map_err(|_| "transport dispatcher stopped".to_owned())?;
    result.await.map_err(|_| "transport dispatcher lost its reply".to_owned())?
}

/// Retirement never waits on the I/O reactor. The caller retains its bounded
/// connection permit until this job finishes, so the queue is bounded by the
/// number of admitted clients, including clients whose cleanup is pending.
pub(crate) fn retire(operation: impl FnOnce() + Send + 'static) {
    let resources = resources().expect("transport initialized before client registration");
    pool(resources, WorkClass::Domain).expect("initialized retirement pool");
    resources.retiring.fetch_add(1, Ordering::AcqRel);
    let _ = resources.retire.send(Box::new(move || {
        struct Finished(&'static Resources);
        impl Drop for Finished {
            fn drop(&mut self) {
                self.0.retiring.fetch_sub(1, Ordering::AcqRel);
                self.0.retired.notify_waiters();
            }
        }
        let _finished = Finished(resources);
        operation();
    }));
}

pub(crate) async fn flush_retired() {
    let resources = resources().expect("transport initialized");
    loop {
        let changed = resources.retired.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if resources.retiring.load(Ordering::Acquire) == 0 { return; }
        changed.await;
    }
}

struct SocketRead<'a>(&'a UnixStream);
impl Read for SocketRead<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        // MSG_DONTWAIT leaves the shared descriptor's blocking mode unchanged
        // for legacy control callers and never blocks this reactor.
        let count = unsafe { libc::recv(self.0.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len(), libc::MSG_DONTWAIT) };
        if count < 0 { Err(io::Error::last_os_error()) } else { Ok(count as usize) }
    }
}

pub(crate) async fn read<T: DeserializeOwned>(socket: &AsyncFd<UnixStream>, codec: &mut FrameCodec) -> Result<Option<T>, FrameError> {
    loop {
        let mut ready = socket.readable().await?;
        match ready.try_io(|socket| match codec.read(&mut SocketRead(socket.get_ref())) {
            Err(FrameError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => Err(error),
            result => Ok(result),
        }) {
            Ok(Ok(result)) => return result,
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => continue,
        }
    }
}

pub(crate) async fn write(socket: &AsyncFd<UnixStream>, bytes: &[u8]) -> io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        let mut ready = socket.writable().await?;
        match ready.try_io(|socket| {
            let count = unsafe { libc::send(socket.get_ref().as_raw_fd(), bytes[written..].as_ptr().cast(), bytes.len() - written, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
            if count < 0 { Err(io::Error::last_os_error()) } else { Ok(count as usize) }
        }) {
            Err(_) => continue,
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Ok(Err(error)) => return Err(error),
            Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(Ok(count)) => written += count,
        }
    }
    Ok(())
}
