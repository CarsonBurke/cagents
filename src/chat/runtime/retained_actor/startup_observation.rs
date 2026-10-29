//! Read-only observations of one actual startup and its existing sole waiter.
//! No test observation can transfer a Child or publish a cleanup outcome.
use super::*;
use std::sync::mpsc::{self, Receiver, SyncSender};

pub(crate) struct Spawned {
    pub process: Process,
    pub pidfd: File,
}

pub(crate) enum Event {
    Spawned(Result<Spawned, String>),
    Reaped(Result<std::process::ExitStatus, String>),
}

struct Registration {
    id: uuid::Uuid,
    source: chat::Session,
    context: Arc<ExecutionContext>,
    events: SyncSender<Event>,
}

static OBSERVER: Mutex<Option<Registration>> = Mutex::new(None);

pub(crate) struct Observation {
    id: uuid::Uuid,
    pub events: Receiver<Event>,
}

impl Observation {
    pub fn install(source: chat::Session, context: Arc<ExecutionContext>) -> Self {
        let id = uuid::Uuid::new_v4();
        let (events, observed) = mpsc::sync_channel(2);
        let mut registration = OBSERVER.lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            registration.is_none(),
            "one exact startup observer per fixture"
        );
        *registration = Some(Registration {
            id,
            source,
            context,
            events,
        });
        Self {
            id,
            events: observed,
        }
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        let mut registration = OBSERVER.lock().unwrap_or_else(|p| p.into_inner());
        if registration
            .as_ref()
            .is_some_and(|value| value.id == self.id)
        {
            registration.take();
        }
    }
}

pub(super) fn matching(
    source: &chat::Session,
    context: Option<&Arc<ExecutionContext>>,
) -> Option<SyncSender<Event>> {
    let registration = OBSERVER.lock().unwrap_or_else(|p| p.into_inner());
    registration.as_ref().and_then(|value| {
        (&value.source == source
            && context.is_some_and(|context| Arc::ptr_eq(&value.context, context)))
        .then(|| value.events.clone())
    })
}

pub(super) fn capture(child: &std::process::Child) -> Result<Spawned, String> {
    let process = Process::read(child.id() as i32).map_err(|error| error.to_string())?;
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let pidfd = unsafe { File::from_raw_fd(raw as i32) };
    if Process::read(process.pid).map_err(|error| error.to_string())? != process {
        return Err("The observed startup helper changed before pinning".into());
    }
    Ok(Spawned { process, pidfd })
}
