//! Keep one local daemon connection parked on bridge edges for the closed
//! Phone control. A replacement daemon starts with a new count.

use std::{
    hash::{Hash, Hasher},
    io::{self, Read},
    net::Shutdown,
    os::{
        fd::{AsFd, AsRawFd, RawFd},
        unix::net::UnixStream,
    },
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use cagents::control::{
    ControlClient, ControlError, Hangup, is_timeout_error, watch_control_directory,
};
use cagents::daemon::{HostLease, RuntimePaths};
use cosmic::iced::futures::{SinkExt, Stream};
use tokio::sync::watch;

use super::{Action, host, sweep_stale_pairings};

pub(super) struct Source {
    generation: u64,
    shown: usize,
}

impl Source {
    pub(super) fn new(generation: u64, shown: usize) -> Self {
        Self { generation, shown }
    }
}

impl Hash for Source {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Updating the count must not reconnect this subscription. A new
        // generation takes a fresh snapshot after a detailed phone probe.
        self.generation.hash(state);
    }
}

pub(super) fn watch(source: &Source) -> impl Stream<Item = Action> + use<> {
    let Source { generation, shown } = *source;
    cosmic::iced::stream::channel(1, async move |mut output| {
        let (counts, mut next) = watch::channel(shown);
        let (wake, canceled) = match UnixStream::pair() {
            Ok(pair) => pair,
            Err(error) => {
                tracing::debug!(%error, "could not create the phone presence cancellation socket");
                return;
            }
        };
        let stop = Arc::new(Stop {
            closed: AtomicBool::new(false),
            hangup: Mutex::new(None),
        });
        let _cancel = CancelOnDrop {
            stop: stop.clone(),
            wake,
        };
        if let Err(error) = thread::Builder::new()
            .name("cagents-phone-presence".into())
            .spawn(move || run(&stop, &canceled, &counts))
        {
            tracing::debug!(%error, "could not start the phone presence observer");
            return;
        }

        let mut displayed = shown;
        while next.changed().await.is_ok() {
            let count = *next.borrow_and_update();
            if count != displayed {
                if output
                    .send(Action::ConnectionsObserved { generation, count })
                    .await
                    .is_err()
                {
                    break;
                }
                displayed = count;
            }
        }
    })
}

struct Stop {
    closed: AtomicBool,
    hangup: Mutex<Option<Hangup>>,
}

impl Stop {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn hold(&self, connected: Hangup) -> io::Result<()> {
        let mut hangup = self.hangup.lock().unwrap_or_else(|p| p.into_inner());
        if self.is_closed() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        *hangup = Some(connected);
        Ok(())
    }

    fn release(&self) {
        if let Some(hangup) = self.hangup.lock().unwrap_or_else(|p| p.into_inner()).take() {
            hangup.hang_up();
        }
    }
}

struct CancelOnDrop {
    stop: Arc<Stop>,
    wake: UnixStream,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.stop.closed.store(true, Ordering::Release);
        self.stop.release();
        let _ = self.wake.shutdown(Shutdown::Both);
    }
}

fn publish(counts: &watch::Sender<usize>, count: usize) {
    counts.send_if_modified(|present| {
        if *present == count {
            return false;
        }
        *present = count;
        true
    });
}

fn run(stop: &Stop, mut canceled: &UnixStream, counts: &watch::Sender<usize>) {
    sweep_stale_pairings();
    if let Some(count) = host::fixture_connection_count() {
        publish(counts, count);
        let _ = canceled.read(&mut [0]);
        return;
    }
    let Ok(paths) = RuntimePaths::discover() else {
        publish(counts, 0);
        return;
    };

    while !stop.is_closed() {
        let (mut client, count) = match connect(&paths.socket, stop) {
            Ok(client) => client,
            Err(_) => {
                publish(counts, 0);
                match wait_for_daemon(&paths, stop, canceled) {
                    Ok(Some(client)) => client,
                    Ok(None) => return,
                    Err(error) => {
                        tracing::debug!(%error, "could not watch for the local daemon socket");
                        return;
                    }
                }
            }
        };
        publish(counts, count);
        let result = observe(&mut client, counts);
        stop.release();
        if !stop.is_closed() {
            publish(counts, 0);
            if let Err(error) = result {
                tracing::debug!(%error, "phone presence connection ended");
            }
        }
    }
}

fn connect(socket: &Path, stop: &Stop) -> Result<(ControlClient, usize), ControlError> {
    let result = (|| {
        let mut client = ControlClient::connect_with_hangup(socket, |handle| stop.hold(handle))?;
        client.watch_mobile_bridges()?;
        let (count, _) = client.mobile_bridges(false)?;
        Ok((client, count))
    })();
    if result.is_err() {
        stop.release();
    }
    result
}

fn observe(client: &mut ControlClient, counts: &watch::Sender<usize>) -> Result<(), ControlError> {
    loop {
        client.next_mobile_bridge_change()?;
        let (count, _) = client.mobile_bridges(false)?;
        publish(counts, count);
    }
}

fn wait_for_daemon(
    paths: &RuntimePaths,
    stop: &Stop,
    canceled: &UnixStream,
) -> io::Result<Option<(ControlClient, usize)>> {
    let runtime = paths
        .directory
        .parent()
        .expect("the instance has a runtime directory");

    while !stop.is_closed() {
        // Watch the runtime before opening the instance so replacing that
        // directory, or binding the socket before connect, cannot lose an edge.
        let runtime = watch_control_directory(runtime)?;
        let instance = match watch_control_directory(&paths.directory) {
            Ok(watch) => Some(watch),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let delay = match connect(&paths.socket, stop) {
            Ok(client) => return Ok(Some(client)),
            // bind publishes the path before listen, which emits no edge.
            // A starting daemon holds the lease even if it pauses there.
            Err(ControlError::Connect(error)) => (matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
            ) && HostLease::is_held(paths).unwrap_or(false))
            .then_some(Duration::from_millis(250)),
            // A live daemon can become ready without replacing its socket.
            Err(
                ControlError::Remote {
                    retryable: true, ..
                }
                | ControlError::ConnectionRejected {
                    retryable: true, ..
                },
            ) => Some(Duration::from_secs(2)),
            Err(error) if is_timeout_error(&error) => Some(Duration::from_secs(2)),
            Err(_) => None,
        };
        let descriptors = [
            canceled.as_raw_fd(),
            runtime.as_fd().as_raw_fd(),
            instance
                .as_ref()
                .map_or(-1, |watch| watch.as_fd().as_raw_fd()),
        ];
        wait_for_socket_edge(descriptors, delay)?;
    }
    Ok(None)
}

fn wait_for_socket_edge(descriptors: [RawFd; 3], delay: Option<Duration>) -> io::Result<()> {
    let mut watched = descriptors.map(|fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    });
    let timeout = delay.map_or(-1, |delay| delay.as_millis() as libc::c_int);
    loop {
        // SAFETY: the caller holds these descriptors and poll borrows the array.
        let ready =
            unsafe { libc::poll(watched.as_mut_ptr(), watched.len() as libc::nfds_t, timeout) };
        if ready >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{hash::DefaultHasher, sync::mpsc};

    #[test]
    fn a_changed_count_keeps_the_same_subscription() {
        let identity = |generation, shown| {
            let mut hasher = DefaultHasher::new();
            Source::new(generation, shown).hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(identity(1, 0), identity(1, 1));
        assert_ne!(identity(1, 1), identity(2, 1));
    }

    #[test]
    fn dropping_the_observer_cancels_a_daemon_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let daemon = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (wake, _canceled) = UnixStream::pair().unwrap();
        let (installed, installation) = mpsc::channel();
        let (finished, result) = mpsc::channel();
        let stop = Arc::new(Stop {
            closed: AtomicBool::new(false),
            hangup: Mutex::new(None),
        });
        let cancel = CancelOnDrop {
            stop: stop.clone(),
            wake,
        };
        let worker = thread::spawn(move || {
            let outcome = ControlClient::connect_with_hangup(&socket, |handle| {
                stop.hold(handle)?;
                let _ = installed.send(());
                Ok(())
            });
            let _ = finished.send(outcome.is_err());
        });
        installation.recv_timeout(Duration::from_secs(2)).unwrap();
        let _unresponsive_peer = daemon.accept().unwrap();

        drop(cancel);
        assert!(result.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn a_quiet_count_does_not_wake_the_window() {
        let (counts, mut observer) = watch::channel(0);
        publish(&counts, 0);
        assert!(!observer.has_changed().unwrap());
        publish(&counts, 1);
        assert_eq!(*observer.borrow_and_update(), 1);
        publish(&counts, 1);
        assert!(!observer.has_changed().unwrap());
        publish(&counts, 0);
        assert_eq!(*observer.borrow_and_update(), 0);
    }
}
