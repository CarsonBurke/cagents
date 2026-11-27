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
pub(super) enum Edge {
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
    #[cfg(feature = "development")]
    pub fn has_review_fixture() -> bool {
        std::env::var_os("CAGENTS_EVIDENCE_FIXTURE")
            .is_some_and(|root| super::review_fixture::path(Path::new(&root)).is_file())
    }

    /// Watch the directory, since the worker replaces status.json atomically.
    /// Register before reading the initial snapshot, so a publication during
    /// that read is still delivered. Only follow the one-shot lease when the
    /// window has already learned that a job started.
    pub fn watch(active_job: bool) -> Result<Self, UpdateError> {
        #[cfg(feature = "development")]
        if let Some(root) = std::env::var_os("CAGENTS_EVIDENCE_FIXTURE") {
            return Self::watch_file(super::review_fixture::path(Path::new(&root)), None);
        }
        let directory = running_installation()?.state_dir()?;
        let lease = active_job.then(|| directory.join("lock"));
        Self::watch_file(directory.join("status.json"), lease.as_deref())
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
    sender: Arc<UnixDatagram>,
    available: Arc<AtomicBool>,
    lease: PathBuf,
    /// Whether a lease watch is armed. Each one reports a single release.
    following_lease: Arc<AtomicBool>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TerminalWake {
    Changed,
    Input,
    Deadline,
    Unavailable,
}

impl TerminalChanges {
    pub(super) fn watch() -> Result<Self, UpdateError> {
        let directory = running_installation()?.prefix.join(".update");
        Self::watch_directory(&directory)
    }

    pub(super) fn watch_directory(directory: &Path) -> Result<Self, UpdateError> {
        let (receiver, sender) = UnixDatagram::pair()?;
        receiver.set_nonblocking(true)?;
        sender.set_nonblocking(true)?;
        let sender = Arc::new(sender);
        let available = Arc::new(AtomicBool::new(true));
        let watcher = watch_file(
            directory.join("status.json"),
            None,
            Self::reporter(sender.clone(), available.clone()),
        )?;
        let changes = Self {
            _watcher: watcher,
            receiver,
            sender,
            available,
            lease: directory.join("lock"),
            following_lease: Arc::new(AtomicBool::new(false)),
        };
        changes.arm_lease()?;
        Ok(changes)
    }

    fn reporter(sender: Arc<UnixDatagram>, available: Arc<AtomicBool>) -> impl Fn(Edge) + Clone {
        move |edge| {
            if matches!(edge, Edge::Unavailable) {
                available.store(false, Ordering::Release);
            }
            let _ = sender.send(b"u");
        }
    }

    /// Wakes a later wait when the worker that holds the lease now lets go.
    ///
    /// A worker can finish without publishing again: it records its final
    /// phase and then releases the lease, which changes no file. The watch
    /// armed at registration reports only the first release, which may be an
    /// earlier holder's, or no holder at all before the worker took the lease,
    /// so a follower that finds the lease held again arms another.
    pub(super) fn follow_lease(&self) {
        if self.arm_lease().is_err() {
            // The next wait reports the watcher unavailable, and its caller
            // falls back to reading the status on a timer.
            self.available.store(false, Ordering::Release);
            let _ = self.sender.send(b"u");
        }
    }

    fn arm_lease(&self) -> Result<(), UpdateError> {
        if self.following_lease.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let following = self.following_lease.clone();
        let report = Self::reporter(self.sender.clone(), self.available.clone());
        watch_lease(&self.lease, move |edge| {
            following.store(false, Ordering::Release);
            report(edge);
        })
        .inspect_err(|_| self.following_lease.store(false, Ordering::Release))
    }

    pub(super) fn wait(&self, deadline: Instant) -> TerminalWake {
        Self::wait_with_input(Some(self), None, Some(deadline))
    }

    pub(super) fn wait_with_input(
        observer: Option<&Self>,
        input: Option<i32>,
        deadline: Option<Instant>,
    ) -> TerminalWake {
        let mut descriptors = [
            observer.map_or(-1, |observer| observer.receiver.as_raw_fd()),
            cancellation::wake_fd(),
            input.unwrap_or(-1),
        ]
        .map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        let timeout = deadline.map_or(-1, |deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32
        });
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
        if descriptors[0].revents != 0
            && let Some(observer) = observer
        {
            let mut value = [0];
            while observer.receiver.recv(&mut value).is_ok() {}
        }
        if descriptors[1].revents != 0 {
            cancellation::drain_wake();
        }
        if observer.is_some_and(|observer| !observer.available.load(Ordering::Acquire)) {
            TerminalWake::Unavailable
        } else if ready == 0 {
            TerminalWake::Deadline
        } else if descriptors[0].revents != 0 || descriptors[1].revents != 0 {
            TerminalWake::Changed
        } else {
            TerminalWake::Input
        }
    }
}

#[cfg(test)]
#[test]
fn terminal_status_follows_a_published_phase_without_waiting_for_the_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let active = super::acquire_lock(directory.path()).unwrap();
    let changes = TerminalChanges::watch_directory(directory.path()).unwrap();
    let (input, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
    let staged = directory.path().join("status.tmp");
    std::fs::write(&staged, "new phase").unwrap();
    std::fs::rename(staged, directory.path().join("status.json")).unwrap();
    assert_eq!(
        TerminalChanges::wait_with_input(
            Some(&changes),
            Some(input.as_raw_fd()),
            Some(Instant::now() + std::time::Duration::from_secs(5)),
        ),
        TerminalWake::Changed,
    );
    use std::io::Write;
    writer.write_all(b"i\n").unwrap();
    assert_eq!(
        TerminalChanges::wait_with_input(
            Some(&changes),
            Some(input.as_raw_fd()),
            Some(Instant::now() + std::time::Duration::from_secs(5)),
        ),
        TerminalWake::Input,
    );
    drop(active);
}

#[cfg(test)]
#[test]
fn a_follower_wakes_when_a_later_worker_releases_the_lease() {
    let directory = tempfile::tempdir().unwrap();
    let wait = |changes: &TerminalChanges| {
        changes.wait(Instant::now() + std::time::Duration::from_secs(5))
    };
    let quiet = |changes: &TerminalChanges| {
        changes.wait(Instant::now() + std::time::Duration::from_millis(300))
            == TerminalWake::Deadline
    };
    // Registered while no worker held the lease, so the first watch has
    // already reported.
    drop(super::acquire_lock(directory.path()).unwrap());
    let changes = TerminalChanges::watch_directory(directory.path()).unwrap();
    assert_eq!(wait(&changes), TerminalWake::Changed);
    // The worker takes the lease, publishes its final phase, and only then
    // lets go, which changes no file.
    let worker = super::acquire_lock(directory.path()).unwrap();
    let staged = directory.path().join("status.tmp");
    std::fs::write(&staged, "complete").unwrap();
    std::fs::rename(staged, directory.path().join("status.json")).unwrap();
    assert_eq!(wait(&changes), TerminalWake::Changed);
    // The publication's events may arrive one at a time.
    for _ in 0..10 {
        if quiet(&changes) {
            break;
        }
    }
    changes.follow_lease();
    changes.follow_lease();
    assert!(changes.following_lease.load(Ordering::Acquire));
    assert!(quiet(&changes), "nothing reports while the worker holds on");
    drop(worker);
    let started = Instant::now();
    assert_eq!(wait(&changes), TerminalWake::Changed);
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
}

fn watch_file(
    snapshot: PathBuf,
    lease: Option<&Path>,
    report: impl Fn(Edge) + Clone + Send + 'static,
) -> Result<RecommendedWatcher, UpdateError> {
    let directory = snapshot.parent().expect("snapshot has a parent").to_owned();
    let watcher = watch_paths(vec![snapshot], &[directory], report.clone())?;
    if let Some(path) = lease {
        watch_lease(path, report)?;
    }
    Ok(watcher)
}

pub(super) fn watch_paths(
    paths: Vec<PathBuf>,
    directories: &[PathBuf],
    report: impl Fn(Edge) + Send + 'static,
) -> Result<RecommendedWatcher, UpdateError> {
    let watched = directories.to_vec();
    let mut watcher = RecommendedWatcher::new(
        move |event: notify::Result<Event>| match event {
            Ok(event)
                if event.paths.iter().any(|path| watched.contains(path))
                    && matches!(
                        event.kind,
                        EventKind::Remove(_)
                            | EventKind::Modify(ModifyKind::Name(
                                RenameMode::From | RenameMode::Any
                            ))
                    ) =>
            {
                report(Edge::Unavailable)
            }
            Ok(event)
                if event.need_rescan() || event.paths.iter().any(|path| paths.contains(path)) =>
            {
                report(Edge::Changed);
            }
            Err(_) => report(Edge::Unavailable),
            _ => (),
        },
        Config::default().with_event_kinds(EventKindMask::CORE),
    )
    .map_err(|error| invalid(format!("Could not watch update progress: {error}")))?;
    for directory in directories {
        watcher
            .watch(directory, RecursiveMode::NonRecursive)
            .map_err(|error| invalid(format!("Could not watch update progress: {error}")))?;
    }
    Ok(watcher)
}

pub(super) fn watch_lease(
    path: &Path,
    report: impl FnOnce(Edge) + Send + 'static,
) -> Result<(), UpdateError> {
    let lease = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    validate_lock(&lease)?;
    // A crashed worker has no last snapshot. Wait for its inherited lease
    // without tying up the caller or the UI executor.
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
    Ok(())
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
