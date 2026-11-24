use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

const WATCH_RETRY: Duration = Duration::from_secs(60);

pub(super) fn enabled(path: &Path) -> bool {
    crate::settings::provider_update_preference(path).unwrap_or_else(|error| {
        tracing::warn!(%error, "automatic provider updates could not read settings");
        false
    })
}

pub(super) struct SettingsWatch {
    path: PathBuf,
    previous: Option<bool>,
    changed: Receiver<()>,
    signal: Sender<()>,
    watch: Option<DirectoryWatch>,
    warned: bool,
}

struct DirectoryWatch {
    directory: PathBuf,
    identity: (u64, u64),
    _watcher: RecommendedWatcher,
}

impl SettingsWatch {
    pub(super) fn new(path: PathBuf) -> Self {
        let (signal, changed) = crossbeam_channel::bounded(1);
        Self {
            path,
            previous: None,
            changed,
            signal,
            watch: None,
            warned: false,
        }
    }

    pub(super) fn wait_for_check(
        &mut self,
        stopped: &Receiver<()>,
        deadline: Instant,
    ) -> Option<bool> {
        loop {
            crossbeam_channel::select_biased! {
                recv(stopped) -> _ => return None,
                default => {}
            }
            self.refresh_watch();
            let current = enabled(&self.path);
            if self.previous.replace(current) != Some(current) || Instant::now() >= deadline {
                return Some(current);
            }
            // With no direct parent watch, periodically try to arm one.
            let wake_at = if self
                .watch
                .as_ref()
                .is_some_and(|watch| Some(watch.directory.as_path()) == self.path.parent())
            {
                deadline
            } else {
                deadline.min(Instant::now() + WATCH_RETRY)
            };
            crossbeam_channel::select_biased! {
                recv(stopped) -> _ => return None,
                recv(self.changed) -> _ => {},
                recv(crossbeam_channel::at(wake_at)) -> _ => {},
            }
        }
    }

    fn refresh_watch(&mut self) {
        // A settings save atomically replaces the file. Watch its directory,
        // or an ancestor while the configuration directory is still absent.
        let Some((directory, identity)) = self.path.ancestors().skip(1).find_map(|directory| {
            let metadata = fs::metadata(directory).ok()?;
            metadata
                .is_dir()
                .then(|| (directory, (metadata.dev(), metadata.ino())))
        }) else {
            self.watch = None;
            return;
        };
        if self
            .watch
            .as_ref()
            .is_some_and(|watch| watch.directory == directory && watch.identity == identity)
        {
            return;
        }
        let path = self.path.clone();
        let signal = self.signal.clone();
        let create = || {
            let mut watcher = RecommendedWatcher::new(
                move |event: notify::Result<notify::Event>| {
                    if event.map_or(true, |event| {
                        event.need_rescan()
                            || event.paths.iter().any(|changed| path.starts_with(changed))
                    }) {
                        let _ = signal.try_send(());
                    }
                },
                notify::Config::default().with_event_kinds(notify::EventKindMask::CORE),
            )?;
            watcher.watch(directory, RecursiveMode::NonRecursive)?;
            Ok::<_, notify::Error>(DirectoryWatch {
                directory: directory.to_path_buf(),
                identity,
                _watcher: watcher,
            })
        };
        self.watch = match create() {
            Ok(watch) => Some(watch),
            Err(error) => {
                if !std::mem::replace(&mut self.warned, true) {
                    tracing::warn!(%error, path = %directory.display(), "could not watch automatic provider update settings");
                }
                None
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_replacements_wake_the_scheduler_on_both_preference_changes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config/settings.toml");
        let mut watch = SettingsWatch::new(path.clone());
        let (stop, stopped) = crossbeam_channel::bounded(1);
        let (publish, changes) = crossbeam_channel::bounded(1);
        let deadline = Instant::now() + Duration::from_secs(3600);
        let worker = std::thread::spawn(move || {
            while let Some(current) = watch.wait_for_check(&stopped, deadline) {
                if publish.send(current).is_err() {
                    break;
                }
            }
        });

        let next = || changes.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(next());
        for update_providers_automatically in [false, true] {
            crate::settings::save(
                &path,
                &crate::settings::AppSettings {
                    update_providers_automatically,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(next(), update_providers_automatically);
        }
        stop.send(()).unwrap();
        worker.join().unwrap();
    }
}
