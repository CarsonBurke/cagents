//! Account files can be atomically replaced by any window or CLI.

use super::Handle;
use notify::{Event, EventKindMask, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(super) struct AccountWatch {
    files: Arc<[PathBuf]>,
    changed: Arc<AtomicBool>,
    watcher: Option<RecommendedWatcher>,
}

impl AccountWatch {
    pub(super) fn new() -> Self {
        let files = [
            crate::account::cache::Cache::installation().map(|cache| cache.path()),
            crate::account::registry::Store::installation().map(|store| store.path()),
        ];
        Self::for_files(files.into_iter().flatten().collect())
    }

    pub(super) fn for_files(files: Arc<[PathBuf]>) -> Self {
        Self {
            files,
            changed: Arc::new(AtomicBool::new(true)),
            watcher: None,
        }
    }

    pub(super) fn refresh(&mut self, courier: &Handle) -> bool {
        if self.files.is_empty() {
            return true;
        }
        if !self.changed.swap(false, Ordering::AcqRel) && self.watcher.is_some() {
            return true;
        }
        match self.install(courier) {
            Ok(watcher) => {
                self.watcher = Some(watcher);
                true
            }
            Err(error) => {
                tracing::debug!(%error, "cannot watch account files for message delivery");
                self.watcher = None;
                false
            }
        }
    }

    fn install(&self, courier: &Handle) -> notify::Result<RecommendedWatcher> {
        let files = Arc::clone(&self.files);
        let changed = Arc::clone(&self.changed);
        let notify_courier = courier.clone();
        let roots = self.roots();
        let mut watcher = RecommendedWatcher::new(
            move |event: notify::Result<Event>| {
                let mut rebuild = event.is_err();
                let mut relevant = false;
                if let Ok(event) = event {
                    rebuild = event.need_rescan();
                    for path in &event.paths {
                        relevant |= files.iter().any(|file| file == path);
                        rebuild |= files
                            .iter()
                            .any(|file| file != path && file.starts_with(path));
                    }
                }
                if rebuild {
                    changed.store(true, Ordering::Release);
                }
                if relevant || rebuild {
                    notify_courier.wake();
                }
            },
            // Reading the private registry can chmod it. A metadata
            // change from that read must not schedule another read.
            notify::Config::default()
                .with_event_kinds(EventKindMask::CORE - EventKindMask::MODIFY_META),
        )?;
        for directory in &roots {
            watcher.watch(directory, RecursiveMode::NonRecursive)?;
        }
        // A file's parent can be created between discovering the existing
        // ancestor and watching it. Re-evaluate the attachment if that won.
        if self.roots() != roots {
            self.changed.store(true, Ordering::Release);
            courier.wake();
        }
        Ok(watcher)
    }

    fn roots(&self) -> BTreeSet<&Path> {
        self.files
            .iter()
            .filter_map(|file| file.ancestors().skip(1).find(|path| path.is_dir()))
            .collect()
    }
}
