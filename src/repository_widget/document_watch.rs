//! One native watcher per process, shared by open documents. No scans or timer.
//! Parent/ancestor watches survive atomic saves and missing path components.
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak, mpsc},
};

#[derive(Clone, Debug)]
pub(super) struct DocumentWatch(Arc<State>);
#[derive(Debug)]
struct State {
    id: uuid::Uuid,
    changed: async_channel::Sender<()>,
    receiver: async_channel::Receiver<()>,
    status: Mutex<Status>,
}
#[derive(Debug, Default)]
struct Status {
    revision: u64,
    error: Option<String>,
}
impl State {
    fn signal(&self, error: Option<String>) {
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        status.revision = status.revision.wrapping_add(1);
        status.error = error;
        let _ = self.changed.try_send(());
    }
}
impl Drop for State {
    fn drop(&mut self) {
        if let Some(service) = SERVICE.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            let _ = service.send(Command::Remove(self.id));
        }
    }
}
impl DocumentWatch {
    pub fn new() -> Self {
        let (changed, receiver) = async_channel::bounded(1);
        Self(Arc::new(State {
            id: uuid::Uuid::new_v4(),
            changed,
            receiver,
            status: Mutex::new(Status::default()),
        }))
    }
    pub fn id(&self) -> uuid::Uuid {
        self.0.id
    }
    pub fn revision(&self) -> u64 {
        self.0
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .revision
    }
    pub fn error(&self) -> Option<String> {
        self.0
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .error
            .clone()
    }
    pub fn receiver(&self) -> async_channel::Receiver<()> {
        self.0.receiver.clone()
    }
    /// Called on the file worker, and acknowledged before its first read.
    pub fn arm(&self, targets: Vec<PathBuf>) {
        let service = {
            let mut slot = SERVICE.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_none() {
                match start() {
                    Ok(service) => *slot = Some(service),
                    Err(error) => {
                        if self.error().as_ref() != Some(&error) {
                            self.0.signal(Some(error));
                        }
                        return;
                    }
                }
            }
            slot.as_ref().unwrap().clone()
        };
        let (reply, response) = mpsc::sync_channel(1);
        let result = service
            .send(Command::Register(Arc::downgrade(&self.0), targets, reply))
            .map_err(|error| error.to_string())
            .and_then(|()| response.recv().map_err(|error| error.to_string()));
        if let Err(error) = result {
            // A terminated coordinator must be restartable on explicit retry.
            *SERVICE.lock().unwrap_or_else(|p| p.into_inner()) = None;
            if self.error().as_ref() != Some(&error) {
                self.0.signal(Some(error));
            }
        }
    }
}
static SERVICE: Mutex<Option<mpsc::Sender<Command>>> = Mutex::new(None);
enum Command {
    Register(Weak<State>, Vec<PathBuf>, mpsc::SyncSender<()>),
    Remove(uuid::Uuid),
    Events,
}
#[derive(Default)]
struct Pending {
    queued: bool,
    events: Vec<Event>,
    fault: Option<String>,
    overflow: bool,
}
struct Entry {
    state: Weak<State>,
    targets: Vec<PathBuf>,
    matched: Vec<PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum DirectoryKey {
    #[cfg(unix)]
    Native(u64, u64),
    #[cfg(not(unix))]
    Path(PathBuf),
}
fn directory_key(path: &Path) -> Option<DirectoryKey> {
    let metadata = path.metadata().ok().filter(|metadata| metadata.is_dir())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(DirectoryKey::Native(metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Some(DirectoryKey::Path(path.canonicalize().ok()?))
    }
}
struct Directory {
    path: PathBuf,
    owners: BTreeSet<uuid::Uuid>,
}
fn plan(
    entries: &mut BTreeMap<uuid::Uuid, Entry>,
    registered: &BTreeMap<DirectoryKey, PathBuf>,
) -> BTreeMap<DirectoryKey, Directory> {
    let mut desired: BTreeMap<DirectoryKey, Directory> = BTreeMap::new();
    for (&id, entry) in entries {
        entry.matched.clear();
        for target in &entry.targets {
            for ancestor in target.ancestors().skip(1) {
                let Ok(canonical) = ancestor.canonicalize() else {
                    continue;
                };
                let Some(key) = directory_key(&canonical) else {
                    continue;
                };
                // notify indexes native descriptors by path: two aliases of
                // one directory would steal routing and unwatch each other.
                // Keep one representative per physical directory, including
                // bind mounts, and project every owner's path onto it.
                let representative = registered
                    .get(&key)
                    .filter(|path| directory_key(path).as_ref() == Some(&key))
                    .cloned()
                    .unwrap_or(canonical);
                let directory = desired.entry(key).or_insert_with(|| Directory {
                    path: representative,
                    owners: BTreeSet::new(),
                });
                directory.owners.insert(id);
                if let Ok(suffix) = target.strip_prefix(ancestor) {
                    entry.matched.push(directory.path.join(suffix));
                }
            }
        }
    }
    desired
}
fn relevant(event: &Event, targets: &[PathBuf]) -> bool {
    event.need_rescan()
        || event
            .paths
            .iter()
            .any(|path| targets.iter().any(|target| target.starts_with(path)))
}
fn start() -> Result<mpsc::Sender<Command>, String> {
    let (sender, commands) = mpsc::channel();
    let wake = sender.clone();
    std::thread::Builder::new()
        .name("document-watch".into())
        .spawn(move || run(commands, wake))
        .map_err(|error| error.to_string())?;
    Ok(sender)
}
fn run(commands: mpsc::Receiver<Command>, wake: mpsc::Sender<Command>) {
    let pending = Arc::new(Mutex::new(Pending::default()));
    let create = || {
        let pending = pending.clone();
        let wake = wake.clone();
        notify::recommended_watcher(move |event: notify::Result<Event>| {
            if event.as_ref().is_ok_and(|event| {
                !event.need_rescan()
                    && matches!(event.kind,
                EventKind::Access(kind) if !matches!(kind,
                    notify::event::AccessKind::Close(notify::event::AccessMode::Write)))
            }) {
                return;
            }
            let mut pending = pending.lock().unwrap_or_else(|p| p.into_inner());
            match event {
                Ok(event) if pending.events.len() < 256 => pending.events.push(event),
                Ok(_) => pending.overflow = true,
                Err(error) => pending.fault = Some(error.to_string()),
            }
            if !pending.queued {
                pending.queued = true;
                let _ = wake.send(Command::Events);
            }
        })
    };
    let mut watcher = create();
    let mut entries: BTreeMap<uuid::Uuid, Entry> = BTreeMap::new();
    let mut registered = BTreeMap::<DirectoryKey, PathBuf>::new();
    while let Ok(command) = commands.recv() {
        let mut reply = None;
        let mut changes = None;
        match command {
            Command::Register(state, targets, ack) => {
                if let Some(live) = state.upgrade() {
                    let matched = entries
                        .remove(&live.id)
                        .map(|entry| entry.matched)
                        .unwrap_or_default();
                    entries.insert(
                        live.id,
                        Entry {
                            state,
                            targets,
                            matched,
                        },
                    );
                }
                reply = Some(ack);
            }
            Command::Remove(id) => {
                entries.remove(&id);
            }
            Command::Events => {
                changes = Some(std::mem::take(
                    &mut *pending.lock().unwrap_or_else(|p| p.into_inner()),
                ));
            }
        }
        entries.retain(|_, entry| entry.state.strong_count() > 0);
        let changed: BTreeSet<_> = entries
            .iter()
            .filter(|(_, entry)| {
                changes.as_ref().is_some_and(|pending| {
                    pending.overflow
                        || pending.fault.is_some()
                        || pending
                            .events
                            .iter()
                            .any(|event| relevant(event, &entry.matched))
                })
            })
            .map(|(&id, _)| id)
            .collect();
        if changes.as_ref().is_some_and(|pending| {
            !pending.overflow
                && pending.fault.is_none()
                && !pending.events.iter().any(Event::need_rescan)
        }) && changed.is_empty()
        {
            // Ancestor watches also see siblings. Unrelated repository or
            // temporary-file traffic must not cause path probes or reads.
            continue;
        }
        let fault = changes.as_ref().and_then(|pending| pending.fault.clone());
        let lost = changes.as_ref().is_some_and(|pending| {
            pending.overflow || pending.events.iter().any(Event::need_rescan)
        });
        if watcher.is_err() || fault.is_some() || lost {
            // Release the old instance first: opening a replacement while
            // still holding it can itself exceed the native instance quota.
            drop(watcher);
            watcher = create();
            registered.clear();
        }
        let desired = plan(&mut entries, &registered);
        let mut failures: BTreeMap<uuid::Uuid, String> = BTreeMap::new();
        match &mut watcher {
            Ok(watcher) => {
                let removed: Vec<_> = registered
                    .iter()
                    .filter(|(key, path)| {
                        desired
                            .get(*key)
                            .is_none_or(|directory| directory.path != **path)
                            || changes.as_ref().is_some_and(|pending| {
                                pending.events.iter().any(|event| {
                                    matches!(
                                        event.kind,
                                        EventKind::Create(_)
                                            | EventKind::Remove(_)
                                            | EventKind::Modify(notify::event::ModifyKind::Name(_))
                                    ) && event.paths.iter().any(|changed| path.starts_with(changed))
                                })
                            })
                    })
                    .map(|(key, path)| (key.clone(), path.clone()))
                    .collect();
                for (key, path) in removed {
                    let _ = watcher.unwatch(&path);
                    registered.remove(&key);
                }
                for (key, directory) in &desired {
                    if registered.contains_key(key) {
                        continue;
                    }
                    match watcher.watch(&directory.path, RecursiveMode::NonRecursive) {
                        Ok(()) => {
                            registered.insert(key.clone(), directory.path.clone());
                        }
                        Err(error) => {
                            for &id in &directory.owners {
                                failures.insert(id, error.to_string());
                            }
                        }
                    }
                }
            }
            Err(error) => {
                for &id in entries.keys() {
                    failures.insert(id, error.to_string());
                }
            }
        }
        for (&id, entry) in &entries {
            let error = failures.get(&id).cloned().or_else(|| fault.clone());
            if let Some(state) = entry.state.upgrade() {
                let relevant_now = changes.as_ref().is_some_and(|pending| {
                    pending
                        .events
                        .iter()
                        .any(|event| relevant(event, &entry.matched))
                });
                let status_changed =
                    state.status.lock().unwrap_or_else(|p| p.into_inner()).error != error;
                if changed.contains(&id) || relevant_now || status_changed {
                    state.signal(error);
                }
            }
        }
        if let Some(reply) = reply {
            let _ = reply.send(());
        }
    }
}
