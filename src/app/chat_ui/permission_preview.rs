//! One off-UI worker caches draft policy and watches its authored config files.
//! The UI publishes current launch recipes; replies carry those exact recipes
//! so a late read cannot replace a different account or a started conversation.
use super::*;
use notify::{EventKind, RecursiveMode, Watcher};
use std::sync::atomic::AtomicBool;

type Desired = HashMap<Target, (chat::Launch, u64)>;

#[derive(Clone)]
pub(super) struct Reply {
    pub(super) target: Target,
    pub(super) launch: chat::Launch,
    pub(super) mode: Option<String>,
}

pub(super) struct Monitor {
    desired: Arc<Mutex<Desired>>,
    dirty: Arc<AtomicBool>,
    watch_failed: Arc<AtomicBool>,
    watch_broken: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    wake: crossbeam_channel::Sender<()>,
    receiver: async_channel::Receiver<Reply>,
}

impl Monitor {
    pub(super) fn new() -> Self {
        Self::with_resolver(|launch| draft_permission_mode(&launch.recipe, launch.provider), true)
    }

    fn with_resolver(
        mut resolve: impl FnMut(&chat::Launch) -> Option<String> + Send + 'static,
        watch_files: bool,
    ) -> Self {
        let desired = Arc::new(Mutex::new(Desired::new()));
        let dirty = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let watch_failed = Arc::new(AtomicBool::new(false));
        let worker_watch_failed = watch_failed.clone();
        let watch_broken = Arc::new(AtomicBool::new(false));
        let worker_watch_broken = watch_broken.clone();
        let (wake, wake_rx) = crossbeam_channel::bounded(1);
        let (replies, receiver) = async_channel::unbounded();
        let worker_desired = desired.clone();
        let worker_dirty = dirty.clone();
        let worker_stopped = stopped.clone();
        let event_wake = wake.clone();
        let candidates = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
        let _ = std::thread::Builder::new().name("draft-permission-preview".into()).spawn(move || {
            let mut watcher = None;
            let mut registered = HashSet::<PathBuf>::new();
            let mut current = Desired::new();
            let mut modes = HashMap::<Target, Option<String>>::new();
            while wake_rx.recv().is_ok() {
                if worker_stopped.load(Ordering::Acquire) { break; }
                let next = worker_desired.lock().unwrap_or_else(|error| error.into_inner()).clone();
                let invalidated = worker_dirty.swap(false, Ordering::AcqRel);
                let mut watch_changed = false;
                if watch_files {
                    if worker_watch_broken.swap(false, Ordering::AcqRel) {
                        watcher = None;
                        registered.clear();
                    }
                    if watcher.is_none() {
                        watcher = config_watcher(candidates.clone(), worker_dirty.clone(),
                            event_wake.clone(), worker_watch_broken.clone()).ok();
                    }
                    let paths = next.values().flat_map(|(launch, _)| config_paths(launch)).collect::<HashSet<_>>();
                    let directories = paths.iter().filter_map(|path| {
                        path.parent()?.ancestors().find(|parent| parent.is_dir()).map(Path::to_owned)
                    }).collect::<HashSet<_>>();
                    *candidates.lock().unwrap_or_else(|error| error.into_inner()) = paths;
                    let mut failed = false;
                    if let Some(watcher) = &mut watcher {
                        for path in registered.difference(&directories) { let _ = watcher.unwatch(path); }
                        registered.retain(|path| directories.contains(path));
                        for path in directories.difference(&registered).cloned().collect::<Vec<_>>() {
                            if watcher.watch(&path, RecursiveMode::NonRecursive).is_ok() {
                                registered.insert(path);
                            } else { failed = true; }
                        }
                    } else { failed = !directories.is_empty(); }
                    watch_changed = worker_watch_failed.swap(failed, Ordering::AcqRel) != failed;
                }
                modes.retain(|target, _| next.contains_key(target));
                for (&target, entry) in &next {
                    let (launch, _) = entry;
                    let changed_recipe = current.get(&target) != Some(entry);
                    if !changed_recipe && !invalidated && !watch_changed { continue; }
                    let mode = if changed_recipe || invalidated {
                        resolve(launch)
                    } else { modes.get(&target).cloned().unwrap_or(None) };
                    if changed_recipe || modes.get(&target) != Some(&mode) || watch_changed {
                        if replies.send_blocking(Reply { target, launch: launch.clone(), mode: mode.clone() }).is_err() {
                            return;
                        }
                        watch_changed = false;
                    }
                    modes.insert(target, mode);
                }
                current = next;
            }
        });
        Self {
            desired,
            dirty,
            watch_failed,
            watch_broken,
            stopped, wake, receiver,
        }
    }

    #[cfg(test)]
    pub(super) fn next_for_test(&self) -> Reply {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(3), self.receiver.recv()).await.unwrap().unwrap()
        })
    }

    pub(super) fn sync<'a>(&self, requests: impl Iterator<Item = (Target, &'a chat::Launch)>) {
        let requests = requests.collect::<HashMap<_, _>>();
        let mut desired = self.desired.lock().unwrap_or_else(|error| error.into_inner());
        if desired.len() == requests.len() && requests.iter().all(|(target, launch)| {
            desired.get(target).is_some_and(|(current, _)| current == *launch)
        }) { return; }
        let next = requests.into_iter().map(|(target, launch)| {
            let revision = desired.get(&target).map_or(0, |(previous, revision)| {
                if previous == launch { *revision } else { revision.wrapping_add(1) }
            });
            (target, (launch.clone(), revision))
        }).collect();
        *desired = next;
        drop(desired);
        let _ = self.wake.try_send(());
    }

    pub(super) fn invalidate(&self) {
        if self.watch_failed.load(Ordering::Acquire) {
            self.watch_broken.store(true, Ordering::Release);
        }
        self.dirty.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
    }

    pub(super) fn subscription(&self, focused: bool) -> Subscription<Message> {
        let replies = Subscription::run_with(Source(self.receiver.clone()), stream)
            .map(|reply| Message::ChatUi(Action::DraftPermissionRead(reply)));
        let fallback = if focused && self.watch_failed.load(Ordering::Acquire)
            && !self.desired.lock().unwrap_or_else(|error| error.into_inner()).is_empty() {
            // Filesystems without notification support still converge while
            // the reader uses the window. This timer does no filesystem I/O.
            iced::time::every(Duration::from_secs(60))
                .map(|_| Message::ChatUi(Action::RefreshDraftPermissionPreview))
        } else { Subscription::none() };
        Subscription::batch([replies, fallback])
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // No join on the UI thread: a slow filesystem read owns no window or
        // widget resources and may finish after the window has closed.
        self.stopped.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
    }
}

fn config_watcher(
    candidates: Arc<Mutex<HashSet<PathBuf>>>,
    dirty: Arc<AtomicBool>,
    wake: crossbeam_channel::Sender<()>,
    broken: Arc<AtomicBool>,
) -> notify::Result<notify::RecommendedWatcher> {
    notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let relevant = match event {
            Ok(event) if matches!(event.kind, EventKind::Access(_)) => false,
            Ok(event) => {
                let candidates = candidates.lock().unwrap_or_else(|error| error.into_inner());
                event.paths.iter().any(|path| candidates.iter().any(|candidate| {
                    candidate == path || candidate.starts_with(path)
                }))
            }
            Err(_) => {
                broken.store(true, Ordering::Release);
                true
            },
        };
        if relevant {
            dirty.store(true, Ordering::Release);
            let _ = wake.try_send(());
        }
    })
}

fn config_paths(launch: &chat::Launch) -> Vec<PathBuf> {
    if crate::device::is_remote() { return Vec::new(); }
    let recipe = &launch.recipe;
    let environment = |name: &str| {
        (!recipe.unset_environment.contains(name)).then(|| {
            recipe.environment.get(name).cloned().or_else(|| std::env::var(name).ok())
        }).flatten().filter(|value| !value.is_empty())
    };
    let absolute = |path: PathBuf| if path.is_absolute() { path } else { recipe.working_directory.join(path) };
    let home = environment("HOME").map(PathBuf::from).map(absolute);
    let mut paths = Vec::new();
    match launch.provider {
        Provider::Codex => {
            if let Some(root) = environment("CODEX_HOME").map(PathBuf::from).map(absolute)
                .or_else(|| home.as_ref().map(|home| home.join(".codex"))) {
                paths.push(root.join("config.toml"));
            }
            for directory in recipe.working_directory.ancestors() {
                if home.as_deref() == Some(directory) { break; }
                paths.push(directory.join(".codex/config.toml"));
            }
        }
        Provider::Claude => {
            if let Some(root) = environment("CLAUDE_CONFIG_DIR").map(PathBuf::from).map(absolute)
                .or_else(|| home.as_ref().map(|home| home.join(".claude"))) {
                paths.push(root.join("settings.json"));
            }
            if let Some(system) = Account::system(Provider::Claude).resolved_home() {
                paths.push(system.join("settings.json"));
            }
            for directory in recipe.working_directory.ancestors() {
                for name in ["settings.json", "settings.local.json"] {
                    paths.push(directory.join(".claude").join(name));
                }
                if home.as_deref() == Some(directory) { break; }
            }
        }
        Provider::Omp => {}
    }
    // Watch the target of linked account configuration as well as the link.
    let targets = paths.iter().filter_map(|path| path.canonicalize().ok()).collect::<Vec<_>>();
    paths.extend(targets);
    paths
}

#[derive(Clone)]
struct Source(async_channel::Receiver<Reply>);
impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool { self.0.same_channel(&other.0) }
}
impl Eq for Source {}
impl std::hash::Hash for Source {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) { std::any::TypeId::of::<Self>().hash(state); }
}
fn stream(source: &Source) -> Pin<Box<dyn iced::futures::Stream<Item = Reply> + Send>> {
    Box::pin(iced::futures::stream::unfold(source.0.clone(), |receiver| async move {
        receiver.recv().await.ok().map(|reply| (reply, receiver))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn launch() -> chat::Launch {
        let CardKind::Terminal(session) = crate::app::tests::terminal_card_state().kind else { unreachable!() };
        chat::Launch { provider: Provider::Codex, recipe: session.launch, session_id: None }
    }

    #[test]
    fn repeated_draft_updates_read_config_once_and_recipe_changes_invalidate() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let monitor = Monitor::with_resolver(move |launch| {
            observed.fetch_add(1, Ordering::AcqRel);
            Some(launch.recipe.arguments.join(" "))
        }, false);
        let target = Target { card: CardId::new(), generation: 1 };
        let mut launch = launch();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let receive = || runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), monitor.receiver.recv()).await.unwrap().unwrap()
        });
        monitor.sync(std::iter::once((target, &launch)));
        assert_eq!(receive().launch, launch);
        for _ in 0..100 { monitor.sync(std::iter::once((target, &launch))); }
        assert_eq!(calls.load(Ordering::Acquire), 1);
        launch.recipe.arguments.push("--sandbox=read-only".into());
        monitor.sync(std::iter::once((target, &launch)));
        assert_eq!(receive().launch, launch);
        assert_eq!(calls.load(Ordering::Acquire), 2);
        monitor.dirty.store(true, Ordering::Release);
        let _ = monitor.wake.try_send(());
        // A real config invalidation re-reads, but an unchanged result emits no
        // UI message. Fence its completion by changing the recipe afterward.
        launch.recipe.arguments.push("--ask-for-approval=on-request".into());
        monitor.sync(std::iter::once((target, &launch)));
        assert_eq!(receive().launch, launch);
        assert!(calls.load(Ordering::Acquire) >= 3);
    }
    #[test]
    fn input_updates_complete_while_config_resolution_is_deliberately_blocked() {
        use cosmic::Application as _;
        let (entered, entered_rx) = crossbeam_channel::bounded(1);
        let (release, release_rx) = crossbeam_channel::bounded(1);
        let monitor = Monitor::with_resolver(move |_| {
            let _ = entered.send(());
            let _ = release_rx.recv();
            Some("workspaceWrite".into())
        }, false);
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.chat_ui.permission_preview = Some(monitor);
        drop(app.new_native_chat(Provider::Codex, Some("/tmp".into())));
        app.refresh_draft_permission_previews();
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        // The resolver cannot finish until after this actual UI input update.
        drop(app.update(Message::SettingsSearchChanged("input remains responsive".into())));
        assert_eq!(app.settings_search, "input remains responsive");
        release.send(()).unwrap();
    }

    #[test]
    fn a_broken_watch_is_rebuilt_and_still_observes_later_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "first").unwrap();
        let source = path.clone();
        let monitor = Monitor::with_resolver(move |_| std::fs::read_to_string(&source).ok(), true);
        let target = Target { card: CardId::new(), generation: 1 };
        let mut launch = launch();
        launch.recipe.working_directory = directory.path().into();
        launch.recipe.environment.insert("CODEX_HOME".into(), directory.path().to_string_lossy().into_owned());
        monitor.sync(std::iter::once((target, &launch)));
        assert_eq!(monitor.next_for_test().mode.as_deref(), Some("first"));
        // This is the signal produced by a notify transport failure. An
        // invalidation forces watcher replacement before the resolver runs.
        monitor.watch_broken.store(true, Ordering::Release);
        monitor.watch_failed.store(true, Ordering::Release);
        monitor.invalidate();
        assert_eq!(monitor.next_for_test().mode.as_deref(), Some("first"));
        assert!(!monitor.watch_failed.load(Ordering::Acquire));
        std::fs::write(path, "after recovery").unwrap();
        assert_eq!(monitor.next_for_test().mode.as_deref(), Some("after recovery"));
    }

    #[test]
    fn a_config_file_change_invalidates_the_cached_preview() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "first").unwrap();
        let source = path.clone();
        let monitor = Monitor::with_resolver(move |_| std::fs::read_to_string(&source).ok(), true);
        let target = Target { card: CardId::new(), generation: 1 };
        let mut launch = launch();
        launch.recipe.working_directory = directory.path().into();
        launch.recipe.environment.insert("CODEX_HOME".into(), directory.path().to_string_lossy().into_owned());
        monitor.sync(std::iter::once((target, &launch)));
        assert_eq!(monitor.next_for_test().mode.as_deref(), Some("first"));
        std::fs::write(path, "second").unwrap();
        assert_eq!(monitor.next_for_test().mode.as_deref(), Some("second"));
    }

}
