//! Watch the directories that contain the transcripts of open inspectors.
//! Watching the containing directory keeps file replacement observable.

use super::{Located, TerminalId};
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _, event::ModifyKind,
};
use std::{
    collections::BTreeMap,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

type Notify = dyn Fn(TerminalId) + Send + Sync;

pub(super) struct Watches {
    watcher: Option<RecommendedWatcher>,
    sources: Arc<Mutex<BTreeMap<TerminalId, Located>>>,
    registered: BTreeMap<PathBuf, (u64, u64)>,
    wake: Arc<Notify>,
    faulted: Arc<AtomicBool>,
}

impl Watches {
    pub(super) fn new(wake: impl Fn(TerminalId) + Send + Sync + 'static) -> Self {
        Self {
            watcher: None,
            sources: Arc::default(),
            registered: BTreeMap::new(),
            wake: Arc::new(wake),
            faulted: Arc::default(),
        }
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(TerminalId) -> bool) {
        let mut sources = self.sources.lock().unwrap_or_else(|p| p.into_inner());
        let before = sources.len();
        sources.retain(|id, _| keep(*id));
        let changed = sources.len() != before;
        drop(sources);
        if changed {
            self.reconcile();
        }
    }

    /// Register before scanning so writes racing watch admission are seen
    /// either in that read or in the next notification.
    pub(super) fn cover(&mut self, id: TerminalId, source: &Located) -> bool {
        self.sources
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, source.clone());
        self.reconcile();
        self.watcher.is_some() && directories(source).all(|path| self.registered.contains_key(path))
    }

    fn reconcile(&mut self) {
        let desired = self
            .sources
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .flat_map(directories)
            .filter_map(|path| identity(path).map(|id| (path.to_owned(), id)))
            .collect::<BTreeMap<_, _>>();
        if desired.is_empty() {
            self.watcher = None;
            self.registered.clear();
            return;
        }
        if self.faulted.swap(false, Ordering::AcqRel) {
            self.watcher = None;
            self.registered.clear();
        }
        if self.watcher.is_none() {
            let faulted = self.faulted.clone();
            let sources = self.sources.clone();
            let wake = self.wake.clone();
            self.watcher = RecommendedWatcher::new(
                move |event: notify::Result<Event>| {
                    if event.is_err() {
                        faulted.store(true, Ordering::Release);
                    }
                    let sources = sources.lock().unwrap_or_else(|p| p.into_inner());
                    for (id, source) in sources.iter() {
                        let changed = match &event {
                            Ok(event) => {
                                // A backend can drop a descriptor on directory
                                // removal, even if the path already exists again.
                                if matches!(
                                    event.kind,
                                    EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
                                ) && event
                                    .paths
                                    .iter()
                                    .any(|path| directory_ancestor(source, path))
                                {
                                    faulted.store(true, Ordering::Release);
                                }
                                event.need_rescan()
                                    || event.paths.iter().any(|path| relevant(source, path))
                            }
                            Err(_) => true,
                        };
                        if changed {
                            wake(*id);
                        }
                    }
                },
                notify::Config::default().with_event_kinds(notify::EventKindMask::CORE),
            )
            .ok();
        }
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        self.registered.retain(|path, before| {
            let keep = desired.get(path) == Some(before);
            if !keep {
                let _ = watcher.unwatch(path);
            }
            keep
        });
        for (path, before) in desired {
            if let std::collections::btree_map::Entry::Vacant(entry) = self.registered.entry(path)
                && watcher
                    .watch(entry.key(), RecursiveMode::NonRecursive)
                    .is_ok()
            {
                if identity(entry.key()) == Some(before) {
                    entry.insert(before);
                } else {
                    let _ = watcher.unwatch(entry.key());
                }
            }
        }
    }
}

fn directories(source: &Located) -> impl Iterator<Item = &Path> {
    std::iter::once(source.transcript.as_path())
        .chain(source.subagents.as_deref())
        .filter_map(|path| path.parent()?.ancestors().find(|dir| dir.is_dir()))
        .chain(source.subagents.as_deref().filter(|dir| dir.is_dir()))
}

fn identity(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    metadata.is_dir().then(|| (metadata.dev(), metadata.ino()))
}

fn relevant(source: &Located, path: &Path) -> bool {
    source.transcript.starts_with(path)
        || source
            .subagents
            .as_ref()
            .is_some_and(|dir| dir.starts_with(path) || path.starts_with(dir))
}

fn directory_ancestor(source: &Located, path: &Path) -> bool {
    source
        .transcript
        .parent()
        .is_some_and(|parent| parent.starts_with(path))
        || source
            .subagents
            .as_ref()
            .is_some_and(|dir| dir.starts_with(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::work::conversations::Format;
    use std::{fs, sync::mpsc, time::Duration};

    #[test]
    fn transcript_replacement_and_late_subagents_wake_their_inspector() {
        let root = tempfile::tempdir().unwrap();
        let transcript = root.path().join("conversation.jsonl");
        let subagents = root.path().join("conversation/subagents");
        fs::write(&transcript, "").unwrap();
        let id = TerminalId::new();
        let (send, recv) = mpsc::channel();
        let mut watches = Watches::new(move |id| {
            let _ = send.send(id);
        });
        let source = Located {
            format: Format::Claude,
            directory: root.path().to_owned(),
            transcript: transcript.clone(),
            subagents: Some(subagents.clone()),
        };
        let wake = || assert_eq!(recv.recv_timeout(Duration::from_secs(3)).unwrap(), id);

        assert!(watches.cover(id, &source));
        let replacement = root.path().join("new.jsonl");
        fs::write(&replacement, "replacement").unwrap();
        fs::rename(replacement, &transcript).unwrap();
        wake();

        fs::create_dir_all(&subagents).unwrap();
        wake();
        assert!(watches.cover(id, &source));
        // Drain any further topology notifications from this change.
        while recv.try_recv().is_ok() {}
        fs::write(subagents.join("agent-1.jsonl"), "new subagent output").unwrap();
        wake();
    }
}
