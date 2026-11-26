//! Watches on files the caller already holds open: a change to a watched
//! inode wakes only the subscribers watching that inode, and each one sees
//! the edge on its own descriptor.
//!
//! One kernel watch instance serves the whole process, because Linux caps
//! inotify instances per user. Linux watches the open file through inotify
//! on `/proc/self/fd/N`; Darwin attaches a kqueue `EVFILT_VNODE` filter to a
//! duplicate of the open descriptor. Either way the watch follows the inode,
//! not its path, so a rename keeps it and a replacement at the path does
//! not move it.
//!
//! A subscriber treats its wake as "look again": it rescans and, after a
//! rename, deletion or replacement, drops its watch and watches whatever the
//! path now names.

use std::{
    collections::{HashMap, hash_map::Entry},
    fs::File,
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
    sync::{Arc, Mutex, MutexGuard},
    thread,
};

#[cfg(target_os = "linux")]
mod inotify;
#[cfg(target_os = "linux")]
use inotify as kernel;
#[cfg(target_os = "macos")]
mod kqueue;
#[cfg(target_os = "macos")]
use kqueue as kernel;

/// What the kernel reported about one watch, or about all of them.
enum Event {
    /// The watched inode changed.
    Changed(kernel::Id),
    /// The kernel dropped the watch, so its subscribers must watch again.
    Gone(kernel::Id),
    /// Events were lost, so any subscriber may have missed a change. A
    /// kqueue filter accumulates its changes instead of queueing them, so
    /// only inotify overflows.
    #[cfg(target_os = "linux")]
    Overflow,
}

/// The notify ends of each watch's subscribers.
type Subscribers = HashMap<kernel::Id, Vec<Arc<OwnedFd>>>;

struct State {
    subscribers: Subscribers,
    registry: kernel::Registry,
}

/// The process's file watch service.
pub(crate) struct FileWatches {
    kernel: kernel::Kernel,
    /// `None` once the reader has failed, which ends every watch.
    state: Mutex<Option<State>>,
}

impl FileWatches {
    /// The process-wide service, started on first use.
    pub(crate) fn shared() -> Option<Arc<Self>> {
        static SERVICE: Mutex<Option<Arc<FileWatches>>> = Mutex::new(None);
        let mut service = SERVICE.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(service) = service.as_ref() {
            return Some(service.clone());
        }
        // Leave failed initialization retryable if the kernel's limit is full.
        let started = Self::start().ok()?;
        *service = Some(started.clone());
        Some(started)
    }

    fn start() -> io::Result<Arc<Self>> {
        let service = Arc::new(Self {
            kernel: kernel::Kernel::open()?,
            state: Mutex::new(Some(State {
                subscribers: HashMap::new(),
                registry: Default::default(),
            })),
        });
        let worker = service.clone();
        thread::Builder::new()
            .name("cagents-file-watches".into())
            .spawn(move || worker.dispatch())?;
        Ok(service)
    }

    /// Watches the inode `file` names. The caller may close `file` afterwards.
    pub(crate) fn watch(self: &Arc<Self>, file: &File) -> Option<FileWatch> {
        let wake = crate::descriptor::wake().ok()?;
        let mut state = self.lock();
        let State {
            subscribers,
            registry,
        } = state.as_mut()?;
        let id = self.kernel.add(registry, file)?;
        subscribers.entry(id).or_default().push(wake.notify.clone());
        Some(FileWatch {
            service: self.clone(),
            id,
            wake,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Option<State>> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn dispatch(&self) {
        let mut batch = kernel::Batch::default();
        loop {
            match self.kernel.wait(&mut batch) {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
            let mut state = self.lock();
            let Some(State {
                subscribers,
                registry,
            }) = state.as_mut()
            else {
                break;
            };
            batch.events(registry, |event| match event {
                #[cfg(target_os = "linux")]
                Event::Overflow => {
                    for wake in subscribers.values().flatten() {
                        signal(wake);
                    }
                }
                Event::Gone(id) => {
                    for wake in subscribers.remove(&id).into_iter().flatten() {
                        signal(&wake);
                    }
                }
                Event::Changed(id) => {
                    for wake in subscribers.get(&id).into_iter().flatten() {
                        signal(wake);
                    }
                }
            });
        }
        // A reader failure leaves subscribers on their ordinary retry.
        if let Some(State {
            subscribers,
            mut registry,
        }) = self.lock().take()
        {
            for (id, wakes) in subscribers {
                self.kernel.remove(&mut registry, id);
                for wake in wakes {
                    signal(&wake);
                }
            }
        }
    }
}

fn signal(wake: &OwnedFd) {
    let one = 1_u64;
    // SAFETY: eight readable bytes, live for the call, into a descriptor the
    // caller holds. A full nonblocking wake already has a wake pending, so a
    // failed write needs no handling.
    unsafe { libc::write(wake.as_raw_fd(), (&raw const one).cast(), size_of::<u64>()) };
}

/// One subscriber's watch. Its descriptor becomes readable once the watched
/// inode changes, and stays readable; drop the watch to discard the edge.
pub(crate) struct FileWatch {
    service: Arc<FileWatches>,
    id: kernel::Id,
    wake: crate::descriptor::Wake,
}

impl AsFd for FileWatch {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.wake.watch.as_fd()
    }
}

impl Drop for FileWatch {
    fn drop(&mut self) {
        let mut state = self.service.lock();
        let Some(State {
            subscribers,
            registry,
        }) = state.as_mut()
        else {
            return;
        };
        if let Entry::Occupied(mut entry) = subscribers.entry(self.id) {
            let wakes = entry.get_mut();
            wakes.retain(|wake| !Arc::ptr_eq(wake, &self.wake.notify));
            if wakes.is_empty() {
                entry.remove();
                self.service.kernel.remove(registry, self.id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::fs::{FileExt, PermissionsExt},
        path::Path,
    };

    fn ready(watch: &FileWatch, timeout: i32) -> bool {
        let mut poll = libc::pollfd {
            fd: watch.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll borrows the live watch for this call.
        assert!(unsafe { libc::poll(&mut poll, 1, timeout) } >= 0);
        poll.revents & libc::POLLIN != 0
    }

    /// Watches `path` the way a caller does: open, watch, close.
    fn watch(path: &Path) -> FileWatch {
        let file = File::open(path).unwrap();
        FileWatches::shared().unwrap().watch(&file).unwrap()
    }

    fn watched(watch: &FileWatch) -> bool {
        let state = watch.service.lock();
        state
            .as_ref()
            .unwrap()
            .subscribers
            .get(&watch.id)
            .is_some_and(|wakes| wakes.iter().any(|w| Arc::ptr_eq(w, &watch.wake.notify)))
    }

    #[test]
    fn one_kernel_watch_dispatches_to_each_subscriber_of_its_inode() {
        let mut first = tempfile::NamedTempFile::new().unwrap();
        let mut second = tempfile::NamedTempFile::new().unwrap();
        let first_watch = watch(first.path());
        let duplicate = watch(first.path());
        let second_watch = watch(second.path());

        assert!(Arc::ptr_eq(&first_watch.service, &second_watch.service));
        assert_eq!(first_watch.id, duplicate.id);
        assert_ne!(first_watch.id, second_watch.id);

        // One write each, so one change raises one wake.
        first.write_all(b"first change\n").unwrap();
        assert!(ready(&first_watch, 2000));
        assert!(ready(&duplicate, 2000));
        assert!(!ready(&second_watch, 0));

        drop(first_watch);
        File::from(duplicate.wake.watch.try_clone().unwrap())
            .read_exact(&mut [0; 8])
            .unwrap();
        assert!(!ready(&duplicate, 0), "the wake was drained");
        first.write_all(b"another change\n").unwrap();
        assert!(ready(&duplicate, 2000));

        let id = duplicate.id;
        drop(duplicate);
        assert!(
            !second_watch
                .service
                .lock()
                .as_ref()
                .unwrap()
                .subscribers
                .contains_key(&id)
        );

        second.write_all(b"other transcript\n").unwrap();
        assert!(ready(&second_watch, 2000));
    }

    /// Each kind of change to a watched inode wakes its watcher and not the
    /// watcher of an unrelated file beside it. `gone` says whether the
    /// kernel drops the watch, so its subscriber must watch again.
    #[test]
    fn each_change_to_a_watched_inode_wakes_only_its_watchers() {
        type Change = fn(&Path, &Path);
        let changes: [(&str, Change, bool); 6] = [
            (
                "write",
                |path, _| {
                    File::options()
                        .write(true)
                        .open(path)
                        .unwrap()
                        .write_all_at(b"overwritten", 0)
                        .unwrap();
                },
                false,
            ),
            (
                "append",
                |path, _| {
                    writeln!(File::options().append(true).open(path).unwrap(), "more").unwrap();
                },
                false,
            ),
            (
                "attributes",
                |path, _| {
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
                },
                false,
            ),
            (
                "rename",
                |path, directory| {
                    std::fs::rename(path, directory.join("renamed")).unwrap();
                },
                false,
            ),
            (
                "delete",
                |path, _| std::fs::remove_file(path).unwrap(),
                true,
            ),
            (
                "replace",
                |path, directory| {
                    let replacement = directory.join("replacement");
                    std::fs::write(&replacement, "new\n").unwrap();
                    std::fs::rename(replacement, path).unwrap();
                },
                true,
            ),
        ];
        for (name, change, gone) in changes {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("watched");
            let unrelated = directory.path().join("unrelated");
            std::fs::write(&path, "original\n").unwrap();
            std::fs::write(&unrelated, "unrelated\n").unwrap();
            let target = watch(&path);
            let bystander = watch(&unrelated);

            change(&path, directory.path());
            assert!(ready(&target, 2000), "{name} wakes the watcher");
            assert!(
                !ready(&bystander, 100),
                "{name} leaves an unrelated watcher"
            );
            // Linux reports an unlink's link-count change before it drops
            // the watch, so a dropped watch may follow its first wake.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while gone && watched(&target) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert_eq!(watched(&target), !gone, "{name} keeps or drops the watch");
            assert!(watched(&bystander), "{name} keeps the unrelated watch");
        }
    }

    #[test]
    fn a_change_to_an_unrelated_file_wakes_no_watcher() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("watched");
        std::fs::write(&path, "original\n").unwrap();
        let target = watch(&path);
        std::fs::write(directory.path().join("unrelated"), "created\n").unwrap();
        std::fs::write(directory.path().join("unrelated"), "rewritten\n").unwrap();
        assert!(!ready(&target, 200));
    }
}
