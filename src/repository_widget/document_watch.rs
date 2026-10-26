//! One native watcher per process, shared by open documents. No scans or timer.
//! Parent/ancestor watches survive atomic saves and missing path components.
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak, mpsc},
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
        if let Some(Ok(service)) = SERVICE.get() {
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
        let service = SERVICE.get_or_init(start);
        let result = match service {
            Ok(service) => {
                let (reply, response) = mpsc::sync_channel(1);
                service
                    .send(Command::Register(Arc::downgrade(&self.0), targets, reply))
                    .map_err(|e| e.to_string())
                    .and_then(|()| response.recv().map_err(|e| e.to_string()))
            }
            Err(error) => Err(error.clone()),
        };
        if let Err(error) = result {
            if self.error().as_ref() != Some(&error) {
                self.0.signal(Some(error));
            }
        }
    }
}
static SERVICE: OnceLock<Result<mpsc::Sender<Command>, String>> = OnceLock::new();
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
    std::thread::Builder::new().name("document-watch".into()).spawn(move || {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let create = || {
            let pending = pending.clone();
            let wake = wake.clone();
            notify::recommended_watcher(move |event: notify::Result<Event>| {
                if event.as_ref().is_ok_and(|event| matches!(event.kind, EventKind::Access(kind) if !matches!(kind, notify::event::AccessKind::Close(notify::event::AccessMode::Write)))) { return; }
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
        let mut registered = BTreeSet::<PathBuf>::new();
        while let Ok(command) = commands.recv() {
            let mut reply = None;
            let mut added = None;
            let mut changes = None;
            match command {
                Command::Register(state, targets, ack) => {
                    if let Some(live) = state.upgrade() {
                        added = Some(live.id);
                        entries.insert(live.id, Entry { state, targets });
                    }
                    reply = Some(ack);
                }
                Command::Remove(id) => { entries.remove(&id); }
                Command::Events => {
                    changes = Some(std::mem::take(&mut *pending.lock().unwrap_or_else(|p| p.into_inner())));
                }
            }
            entries.retain(|_, entry| entry.state.strong_count() > 0);
            let fault = changes.as_ref().and_then(|p| p.fault.clone());
            if watcher.is_err() || fault.is_some() {
                // A native failure is visible. Recreate only on a real event or
                // explicit registration/reload, never a periodic retry.
                watcher = create();
                registered.clear();
            }
            let desired: BTreeSet<_> = entries.values().flat_map(|entry| entry.targets.iter())
                .flat_map(|target| target.ancestors().skip(1))
                .filter(|path| path.is_dir()).map(Path::to_path_buf).collect();
            let mut failures = BTreeMap::new();
            match &mut watcher {
                Ok(watcher) => {
                    let removed: Vec<_> = registered.iter().filter(|path| !desired.contains(*path) || changes.as_ref().is_some_and(|pending| pending.events.iter().any(|event| {
                        matches!(event.kind, EventKind::Remove(_) | EventKind::Create(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))) && event.paths.iter().any(|changed| path.starts_with(changed))
                    }))).cloned().collect();
                    for path in removed { let _ = watcher.unwatch(&path); registered.remove(&path); }
                    for path in desired.difference(&registered).cloned().collect::<Vec<_>>() {
                        match watcher.watch(&path, RecursiveMode::NonRecursive) {
                            Ok(()) => { registered.insert(path); }
                            Err(error) => { failures.insert(path, error.to_string()); }
                        }
                    }
                }
                Err(error) => { failures.insert(PathBuf::from("/"), error.to_string()); }
            }
            for (&id, entry) in &entries {
                let error = entry.targets.iter().find_map(|target| failures.iter().find(|(path, _)| target.starts_with(path)).map(|(_, error)| error.clone())).or_else(|| fault.clone());
                if let Some(state) = entry.state.upgrade() {
                    let changed = changes.as_ref().is_some_and(|pending| pending.overflow || pending.fault.is_some() || pending.events.iter().any(|event| relevant(event, &entry.targets)));
                    // Clear a previous watch failure when an event or explicit
                    // registration successfully rearms it. Preserve failure
                    // visibility until then.
                    if changed || (added == Some(id) && state.status.lock().unwrap_or_else(|p| p.into_inner()).error != error) {
                        state.signal(error);
                    }
                }
            }
            if let Some(reply) = reply { let _ = reply.send(()); }
        }
    }).map_err(|error| error.to_string())?;
    Ok(sender)
}
