//! Watches lexical aliases and their existing ancestors without UI filesystem I/O.
use super::*;
use notify::{EventKind, RecursiveMode, Watcher};
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
struct Registration {
    watcher: Option<notify::RecommendedWatcher>,
    paths: HashSet<PathBuf>,
}

pub(super) struct Monitor {
    registration: Mutex<Registration>,
    observed: Arc<Mutex<HashMap<PathBuf, PathBuf>>>,
    dirty: Arc<Mutex<HashSet<PathBuf>>>,
    broken: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    sender: async_channel::Sender<()>,
    receiver: async_channel::Receiver<()>,
}

impl Default for Monitor {
    fn default() -> Self {
        let (sender, receiver) = async_channel::bounded(1);
        Self {
            registration: Mutex::new(Registration::default()),
            observed: Arc::new(Mutex::new(HashMap::new())),
            dirty: Arc::new(Mutex::new(HashSet::new())),
            broken: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            sender, receiver,
        }
    }
}

impl Monitor {
    /// Called only inside the existing blocking canonicalization task.
    pub(super) fn register(&self, results: &[(PathBuf, PathBuf)]) {
        let mut registration = self.registration.lock().unwrap_or_else(|error| error.into_inner());
        if self.broken.swap(false, Ordering::AcqRel) {
            registration.watcher = None;
            registration.paths.clear();
        }
        {
            let mut observed = self.observed.lock().unwrap_or_else(|error| error.into_inner());
            observed.extend(results.iter().cloned());
        }
        if registration.watcher.is_none() {
            let observed = self.observed.clone();
            let dirty = self.dirty.clone();
            let broken = self.broken.clone();
            let sender = self.sender.clone();
            registration.watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let observed = observed.lock().unwrap_or_else(|error| error.into_inner());
                let changed = match event {
                    Ok(event) if matches!(event.kind, EventKind::Access(_)) => Vec::new(),
                    Ok(event) => observed.iter().filter(|(lexical, physical)| {
                        event.paths.iter().any(|path| lexical.starts_with(path) || physical.starts_with(path))
                    }).map(|(lexical, _)| lexical.clone()).collect(),
                    Err(_) => {
                        broken.store(true, Ordering::Release);
                        observed.keys().cloned().collect()
                    }
                };
                if !changed.is_empty() {
                    dirty.lock().unwrap_or_else(|error| error.into_inner()).extend(changed);
                    let _ = sender.try_send(());
                }
            }).ok();
        }
        // Watching each existing ancestor also observes replacement of a
        // symlink in the middle of a launch directory, and creation of a
        // directory that did not exist when canonicalization first ran.
        let observed_paths = {
            let observed = self.observed.lock().unwrap_or_else(|error| error.into_inner());
            observed.iter().flat_map(|(lexical, physical)| [lexical.clone(), physical.clone()])
                .collect::<Vec<_>>()
        };
        // UI fallback invalidation also reads `observed`. Never hold that
        // mutex across even metadata calls on a potentially slow filesystem.
        let directories = observed_paths.iter().flat_map(|path| path.ancestors().skip(1))
            .filter(|path| path.is_dir()).map(Path::to_owned).collect::<HashSet<_>>();
        let removed = registration.paths.difference(&directories).cloned().collect::<Vec<_>>();
        for path in removed {
            if let Some(watcher) = &mut registration.watcher { let _ = watcher.unwatch(&path); }
            registration.paths.remove(&path);
        }
        let mut failed = registration.watcher.is_none() && !directories.is_empty();
        for path in directories {
            if registration.paths.contains(&path) { continue; }
            if registration.watcher.as_mut().is_some_and(|watcher| {
                watcher.watch(&path, RecursiveMode::NonRecursive).is_ok()
            }) {
                registration.paths.insert(path);
            } else { failed = true; }
        }
        if self.failed.swap(failed, Ordering::AcqRel) != failed {
            let _ = self.sender.try_send(());
        }
    }

    /// A cheap UI operation: registration cleanup itself runs in the next
    /// blocking task, including a task with no new canonical paths.
    pub(super) fn retain(&self, active: &HashSet<PathBuf>) -> bool {
        let mut observed = self.observed.lock().unwrap_or_else(|error| error.into_inner());
        let previous = observed.len();
        observed.retain(|path, _| active.contains(path));
        self.dirty.lock().unwrap_or_else(|error| error.into_inner()).retain(|path| active.contains(path));
        observed.len() != previous
    }

    pub(super) fn take_invalidated(&self) -> HashSet<PathBuf> {
        std::mem::take(&mut *self.dirty.lock().unwrap_or_else(|error| error.into_inner()))
    }

    pub(super) fn invalidate_failed(&self) {
        if !self.failed.load(Ordering::Acquire) { return; }
        self.broken.store(true, Ordering::Release);
        let observed = self.observed.lock().unwrap_or_else(|error| error.into_inner());
        self.dirty.lock().unwrap_or_else(|error| error.into_inner()).extend(observed.keys().cloned());
    }

    pub(super) fn subscription(&self, focused: bool) -> Subscription<Message> {
        let changes = Subscription::run_with(Source(self.receiver.clone()), stream)
            .map(|_| Message::WorktreeOwnerPathsInvalidated);
        let fallback = if focused && self.failed.load(Ordering::Acquire) {
            iced::time::every(Duration::from_secs(60)).map(|_| Message::WorktreeOwnerPathsInvalidated)
        } else { Subscription::none() };
        Subscription::batch([changes, fallback])
    }
}

#[derive(Clone)]
struct Source(async_channel::Receiver<()>);
impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool { self.0.same_channel(&other.0) }
}
impl Eq for Source {}
impl std::hash::Hash for Source {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) { std::any::TypeId::of::<Self>().hash(state); }
}
fn stream(source: &Source) -> Pin<Box<dyn iced::futures::Stream<Item = ()> + Send>> {
    Box::pin(iced::futures::stream::unfold(source.0.clone(), |receiver| async move {
        receiver.recv().await.ok().map(|item| (item, receiver))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next_change(monitor: &Monitor) {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(3), monitor.receiver.recv()).await.unwrap().unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn retargeting_the_same_lexical_symlink_invalidates_its_cached_owner() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let monitor = Monitor::default();
        monitor.register(&[(alias.clone(), alias.canonicalize().unwrap())]);
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&second, &alias).unwrap();
        next_change(&monitor);
        assert!(monitor.take_invalidated().contains(&alias));
        assert_eq!(alias.canonicalize().unwrap(), second);
    }

    #[test]
    fn obsolete_paths_release_their_watch_registrations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("card");
        std::fs::create_dir(&path).unwrap();
        let monitor = Monitor::default();
        monitor.register(&[(path.clone(), path)]);
        assert!(!monitor.registration.lock().unwrap().paths.is_empty());
        assert!(monitor.retain(&HashSet::new()));
        monitor.register(&[]);
        assert!(monitor.registration.lock().unwrap().paths.is_empty());
        assert!(monitor.observed.lock().unwrap().is_empty());
    }

    #[test]
    fn creating_a_formerly_missing_ancestor_invalidates_the_lexical_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing/nested");
        let monitor = Monitor::default();
        monitor.register(&[(missing.clone(), missing.clone())]);
        std::fs::create_dir_all(&missing).unwrap();
        next_change(&monitor);
        assert!(monitor.take_invalidated().contains(&missing));
    }
}
