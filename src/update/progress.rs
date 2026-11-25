//! Wake an update follower when the worker publishes a new snapshot.

use super::{UpdateError, cancellation, invalid, running_installation, validate_lock};
use notify::event::{ModifyKind, RenameMode};
use notify::{Config, Event, EventKind, EventKindMask, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    fs::OpenOptions,
    io,
    os::{
        fd::AsRawFd,
        unix::{fs::OpenOptionsExt, net::UnixDatagram},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Instant,
};

#[derive(Clone, Copy)]
enum Edge {
    Changed,
    Unavailable,
}

#[cfg(feature = "ui")]
pub struct Changes {
    _watcher: RecommendedWatcher,
    pending: async_channel::Receiver<()>,
}

#[cfg(feature = "ui")]
impl Changes {
    /// Watch the directory, since the worker replaces status.json atomically.
    /// Register before reading the initial snapshot, so a publication during
    /// that read is still delivered.
    pub fn watch() -> Result<Self, UpdateError> {
        #[cfg(feature = "development")]
        if let Some(root) = std::env::var_os("CAGENTS_EVIDENCE_FIXTURE") {
            return Self::watch_file(
                PathBuf::from(root).join("home/.config/fixture-update-review.json"),
                None,
            );
        }
        let directory = running_installation()?.prefix.join(".update");
        Self::watch_file(directory.join("status.json"), Some(&directory.join("lock")))
    }

    fn watch_file(snapshot: PathBuf, lease: Option<&Path>) -> Result<Self, UpdateError> {
        let (send, pending) = async_channel::bounded(1);
        let watcher = watch_file(snapshot, lease, move |edge| match edge {
            Edge::Changed => {
                let _ = send.try_send(());
            }
            Edge::Unavailable => {
                send.close();
            }
        })?;
        Ok(Self {
            _watcher: watcher,
            pending,
        })
    }

    /// A closed watcher leaves the caller free to fall back to a status read.
    pub async fn changed(&self) -> bool {
        self.pending.recv().await.is_ok()
    }
}

pub(super) struct TerminalChanges {
    _watcher: RecommendedWatcher,
    receiver: UnixDatagram,
    available: Arc<AtomicBool>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TerminalWake {
    Changed,
    Deadline,
    Unavailable,
}

impl TerminalChanges {
    pub(super) fn watch() -> Result<Self, UpdateError> {
        let directory = running_installation()?.prefix.join(".update");
        Self::watch_directory(&directory)
    }

    fn watch_directory(directory: &Path) -> Result<Self, UpdateError> {
        let (receiver, sender) = UnixDatagram::pair()?;
        receiver.set_nonblocking(true)?;
        sender.set_nonblocking(true)?;
        let sender = Arc::new(sender);
        let available = Arc::new(AtomicBool::new(true));
        let supported = available.clone();
        let watcher = watch_file(
            directory.join("status.json"),
            Some(&directory.join("lock")),
            move |edge| {
                if matches!(edge, Edge::Unavailable) {
                    supported.store(false, Ordering::Release);
                }
                let _ = sender.send(b"u");
            },
        )?;
        Ok(Self {
            _watcher: watcher,
            receiver,
            available,
        })
    }

    pub(super) fn wait(&self, deadline: Instant) -> TerminalWake {
        let mut descriptors =
            [self.receiver.as_raw_fd(), cancellation::wake_fd()].map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        // SAFETY: both descriptors remain open through the call.
        let ready = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                timeout,
            )
        };
        if ready < 0 {
            return if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                TerminalWake::Changed
            } else {
                TerminalWake::Unavailable
            };
        }
        if descriptors[0].revents != 0 {
            let mut value = [0];
            while self.receiver.recv(&mut value).is_ok() {}
        }
        if descriptors[1].revents != 0 {
            cancellation::drain_wake();
        }
        if !self.available.load(Ordering::Acquire) {
            TerminalWake::Unavailable
        } else if ready == 0 {
            TerminalWake::Deadline
        } else {
            TerminalWake::Changed
        }
    }
}

#[cfg(test)]
#[test]
fn terminal_status_follows_a_published_phase_without_waiting_for_the_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let active = super::acquire_lock(directory.path()).unwrap();
    let changes = TerminalChanges::watch_directory(directory.path()).unwrap();
    let staged = directory.path().join("status.tmp");
    std::fs::write(&staged, "new phase").unwrap();
    std::fs::rename(staged, directory.path().join("status.json")).unwrap();
    assert_eq!(
        changes.wait(Instant::now() + std::time::Duration::from_secs(5)),
        TerminalWake::Changed,
    );
    drop(active);
}

fn watch_file(
    snapshot: PathBuf,
    lease: Option<&Path>,
    report: impl Fn(Edge) + Clone + Send + 'static,
) -> Result<RecommendedWatcher, UpdateError> {
    let directory = snapshot.parent().expect("snapshot has a parent").to_owned();
    let watched_directory = directory.clone();
    let events = report.clone();
    let mut watcher = RecommendedWatcher::new(
        move |event: notify::Result<Event>| match event {
            Ok(event)
                if event.paths.contains(&watched_directory)
                    && matches!(
                        event.kind,
                        EventKind::Remove(_)
                            | EventKind::Modify(ModifyKind::Name(
                                RenameMode::From | RenameMode::Any
                            ))
                    ) =>
            {
                events(Edge::Unavailable)
            }
            Ok(event) if event.need_rescan() || event.paths.contains(&snapshot) => {
                events(Edge::Changed);
            }
            Err(_) => events(Edge::Unavailable),
            _ => (),
        },
        Config::default().with_event_kinds(EventKindMask::CORE),
    )
    .map_err(|error| invalid(format!("Could not watch update progress: {error}")))?;
    watcher
        .watch(&directory, RecursiveMode::NonRecursive)
        .map_err(|error| invalid(format!("Could not watch update progress: {error}")))?;
    if let Some(path) = lease {
        let lease = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        validate_lock(&lease)?;
        // A crashed worker has no last snapshot. Wait for its inherited
        // lock to release, then wake the existing stale-job check. This
        // thread is independent of the UI executor so the window can
        // exit even while an update still holds the lease.
        thread::Builder::new()
            .name("update-lease-watch".into())
            .spawn(
                move || match crate::instance::owned_file_lock(lease, libc::LOCK_SH) {
                    Ok(released) => {
                        drop(released);
                        report(Edge::Changed);
                    }
                    Err(_) => report(Edge::Unavailable),
                },
            )?;
    }
    Ok(watcher)
}

#[cfg(all(test, feature = "ui"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn watching_before_the_first_read_catches_an_atomic_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = directory.path().join("status.json");
        let changes = Changes::watch_file(snapshot.clone(), None).unwrap();
        let staged = directory.path().join("status.tmp");
        std::fs::write(&staged, "changed").unwrap();
        std::fs::rename(staged, snapshot).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), changes.changed())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn the_worker_lease_ending_wakes_a_reader_without_a_final_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let lease = super::super::acquire_lock(directory.path()).unwrap();
        let changes = Changes::watch_file(
            directory.path().join("status.json"),
            Some(&directory.path().join("lock")),
        )
        .unwrap();
        assert!(changes.pending.is_empty());
        drop(lease);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), changes.changed())
                .await
                .unwrap()
        );
    }
}
