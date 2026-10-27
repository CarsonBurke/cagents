//! One filesystem preparation, one latest navigation request, then retirement.
//!
//! This deliberately does not coalesce commands that create/resume sessions.
use super::*;
use notify::Watcher;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

#[derive(Default)]
pub(in crate::app) struct State {
    generation: u64,
    active: Option<(u64, u64)>,
    ticket: u64,
    pending: Option<Request>,
    failed_sync: Option<(Vec<PathBuf>, Instant)>,
    owners_dirty: bool,
    refresh_scope: bool,
    retired_readiness: Vec<Arc<crate::instance::OwnedLock>>,
    #[cfg(test)]
    local_window: bool,
    #[cfg(test)]
    runtime: Option<PathBuf>,
    #[cfg(test)]
    before_prepare: Option<Arc<dyn Fn(&Path) + Send + Sync>>,
    #[cfg(test)]
    after_acquire: Option<Arc<dyn Fn(&Path) + Send + Sync>>,
    #[cfg(test)]
    owner_read: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    after_retire: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    before_retire: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    followup_effects: Option<Vec<Task<Message>>>,
}

impl State {
    #[cfg(test)]
    pub(super) fn record_owner_read(&self) {
        if let Some(read) = &self.owner_read {
            read();
        }
    }

    pub(in crate::app) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub(in crate::app) fn needs_reconciliation(&self) -> bool {
        self.is_active() || self.owners_dirty || !self.retired_readiness.is_empty()
    }

    pub(super) fn owners_refreshed(&mut self) {
        self.owners_dirty = false;
        self.refresh_scope = false;
    }

    pub(in crate::app) fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
        // A blocking task is not cancelled by dropping its future. Keep its
        // slot occupied until its real receipt and capabilities have retired.
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Request {
    generation: u64,
    path: Option<PathBuf>,
    create: bool,
    origin: Option<Origin>,
    background: bool,
    refresh_channel: Option<ChannelId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) enum Origin {
    Project(ProjectId, PathBuf),
    Worktree(ProjectId, WorktreeId, PathBuf),
}

impl Origin {
    fn current(&self, app: &Cagents, path: &Path) -> bool {
        match self {
            Self::Project(id, repository) => {
                app.sidebar_model.project(*id).is_some_and(|project| {
                    project.repository == *repository
                        && app.project_entry_directory(*id, repository) == path
                })
            }
            Self::Worktree(project, id, expected) => {
                expected == path
                    && app.sidebar_model.project(*project).is_some_and(|project| {
                        project
                            .worktrees
                            .iter()
                            .any(|tree| tree.id == *id && tree.path == *expected)
                    })
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Inputs {
    request: Request,
    roots: Vec<(PathBuf, bool)>,
    cards: Vec<(CardId, u64, Option<PathBuf>, Option<PathBuf>)>,
    sidebar: Vec<(ProjectId, bool, PathBuf, Vec<(WorktreeId, PathBuf)>)>,
    protected: Vec<PathBuf>,
    window: Option<window::Id>,
    device: Option<(String, String, String)>,
    work: (bool, uuid::Uuid),
    default_pane: settings::DefaultPane,
    scope: (Option<PathBuf>, Option<ChannelId>),
    refresh_scope: bool,
}

impl Inputs {
    fn capture(app: &mut Cagents, request: Request) -> Self {
        app.collect_worktree_roots();
        let mut roots = app
            .known_worktree_roots
            .iter()
            .map(|path| (path.clone(), app.sidebar_model.worktree_at(path).is_some()))
            .collect::<Vec<_>>();
        roots.sort();
        let cards = app
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .map(|card| {
                (
                    card.id,
                    card.generation,
                    app.durable.card_scopes.get(&card.id).cloned(),
                    card_directory(card).map(Path::to_owned),
                )
            })
            .collect();
        let sidebar = app
            .sidebar_model
            .projects
            .iter()
            .map(|project| {
                (
                    project.id,
                    project.kind == ProjectKind::Folder,
                    project.repository.clone(),
                    project
                        .worktrees
                        .iter()
                        .map(|tree| (tree.id, tree.path.clone()))
                        .collect(),
                )
            })
            .collect();
        let mut protected = app.desired_worktree_protections();
        protected.sort();
        protected.dedup();
        let refresh_scope = request.background && app.worktree_selection.refresh_scope;
        Self {
            request,
            roots,
            cards,
            sidebar,
            protected,
            window: app.core.main_window_id(),
            work: (app.channel_work_is_main(), app.channel_home_generation()),
            default_pane: app.settings.default_worktree_pane,
            scope: (app.active_workspace.clone(), app.durable.active_channel),
            refresh_scope,
            device: crate::device::selected().map(|device| {
                (
                    device.name.clone(),
                    device.host.clone(),
                    device.executable.clone(),
                )
            }),
        }
    }
}

/// Watches only invalidate an active physical proof, never authorize a path.
/// Keep their potentially joining destructor in the worker retirement packet.
struct Invalidation {
    watcher: notify::RecommendedWatcher,
    targets: Arc<Mutex<HashSet<PathBuf>>>,
    registered: HashSet<PathBuf>,
    epoch: Arc<AtomicU64>,
}

impl Invalidation {
    fn new() -> Result<Self, String> {
        let epoch = Arc::new(AtomicU64::new(0));
        let targets = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
        let observed = targets.clone();
        let changed = epoch.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let invalid = match event {
                Err(_) => true,
                Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => false,
                Ok(event) => {
                    let targets = observed.lock().unwrap_or_else(|error| error.into_inner());
                    event.paths.is_empty()
                        || event
                            .paths
                            .iter()
                            .any(|path| targets.iter().any(|target| target.starts_with(path)))
                }
            };
            if invalid {
                changed.fetch_add(1, Ordering::AcqRel);
            }
        })
        .map_err(|error| error.to_string())?;
        Ok(Self {
            watcher,
            targets,
            registered: HashSet::new(),
            epoch,
        })
    }

    fn register(&mut self, paths: impl IntoIterator<Item = PathBuf>) -> Result<(), String> {
        let paths = paths.into_iter().collect::<Vec<_>>();
        self.targets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .extend(paths.clone());
        for path in paths {
            for parent in path.ancestors().skip(1).filter(|parent| parent.is_dir()) {
                if self.registered.insert(parent.to_owned()) {
                    self.watcher
                        .watch(parent, notify::RecursiveMode::NonRecursive)
                        .map_err(|error| error.to_string())?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TerminalContext {
    defaults: TerminalLaunchDefaults,
    shell: Option<std::ffi::OsString>,
    deferred: bool,
}

struct Prepared {
    inputs: Inputs,
    path: Option<PathBuf>,
    owners: HashMap<CardId, PathBuf>,
    protections: crate::worktree_cleanup::WindowProtections,
    ready: Option<crate::instance::PreparedLock>,
    invalidation: Invalidation,
    epoch: u64,
    chat: Option<chat_ui::PreparedDefaultChat>,
    terminal: Option<(TerminalContext, LaunchRecipe)>,
}

#[derive(Default)]
struct Slot {
    phase: u64,
    claimed: bool,
    result: Option<Result<Prepared, String>>,
    command: Option<Command>,
}

#[derive(Default)]
struct Shared {
    slot: Mutex<Slot>,
    cancelled: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    before_retire: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    after_retire: Option<Arc<dyn Fn() + Send + Sync>>,
}

enum Command {
    Chat(chat_ui::DefaultChatContext),
    Terminal(TerminalContext),
    Retire(Retirement),
}

#[derive(Clone, Copy)]
enum Notification {
    Ready(u64),
    Retired,
}

struct Client {
    shared: Arc<Shared>,
    wake: crossbeam_channel::Sender<()>,
    notifications: async_channel::Receiver<Notification>,
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for Client {
    fn drop(&mut self) {
        // A modal-deferred Message owns metadata only. The original worker
        // retains Shared and drains its capabilities when this last UI lease
        // disappears, including receiver/Task closure during window teardown.
        self.shared.cancelled.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
    }
}

#[derive(Clone)]
pub(in crate::app) struct Receipt {
    client: Arc<Client>,
    phase: u64,
}

impl std::fmt::Debug for Receipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedWorktreeNavigation")
    }
}

impl Receipt {
    fn command(&self, command: Command) {
        let mut slot = self
            .client
            .shared
            .slot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // One consumed notification produces exactly one command. A duplicate
        // old Message cannot replace a queued retirement packet on the UI.
        assert!(slot.command.is_none());
        slot.command = Some(command);
        drop(slot);
        let _ = self.client.wake.try_send(());
    }

    async fn ready(self, generation: u64) -> Message {
        match self.client.notifications.recv().await {
            Ok(Notification::Ready(phase)) => Message::WorktreeNavigationPrepared {
                generation,
                receipt: Self {
                    client: self.client.clone(),
                    phase,
                },
            },
            // No proof of ordinary retirement has been delivered to this task.
            // Retain the client/slot instead of declaring a completed ticket.
            _ => {
                let _held = self;
                std::future::pending::<Message>().await
            }
        }
    }

    async fn retired(self, generation: u64) -> Message {
        if !matches!(
            self.client.notifications.recv().await,
            Ok(Notification::Retired)
        ) {
            let _held = self;
            return std::future::pending::<Message>().await;
        }
        let join = self
            .client
            .join
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(join) = join {
            if join.await.is_ok() {
                return Message::WorktreeNavigationRetired(generation);
            }
        }
        let _held = self;
        std::future::pending::<Message>().await
    }
}

#[derive(Default)]
struct Retirement {
    previous: Option<crate::worktree_cleanup::WindowProtections>,
    obsolete: Vec<Arc<crate::worktree_cleanup::Protection>>,
    readiness: Vec<Arc<crate::instance::OwnedLock>>,
    #[cfg(test)]
    observed: Option<Arc<dyn Fn() + Send + Sync>>,
}

fn worker_phase(phase: impl FnOnce() -> Result<Prepared, String>) -> Result<Prepared, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(phase))
        .unwrap_or_else(|_| Err("Worktree preparation worker panicked".into()))
}

fn worker(
    shared: Arc<Shared>,
    wake: crossbeam_channel::Receiver<()>,
    notifications: async_channel::Sender<Notification>,
    initial: impl FnOnce() -> Result<Prepared, String>,
) {
    let mut result = worker_phase(initial);
    loop {
        let phase = {
            let mut slot = shared
                .slot
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            slot.phase = slot.phase.wrapping_add(1);
            slot.claimed = false;
            slot.result = Some(result);
            slot.phase
        };
        if shared.cancelled.load(Ordering::Acquire) {
            break;
        }
        if notifications
            .send_blocking(Notification::Ready(phase))
            .is_err()
        {
            break;
        }
        if wake.recv().is_err() || shared.cancelled.load(Ordering::Acquire) {
            break;
        }
        let (command, prepared) = {
            let mut slot = shared
                .slot
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            (slot.command.take(), slot.result.take())
        };
        match command {
            Some(Command::Retire(retirement)) => {
                let mut slot = shared
                    .slot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                slot.result = prepared;
                slot.command = Some(Command::Retire(retirement));
                break;
            }
            Some(Command::Terminal(context)) => {
                result = worker_phase(move || {
                    let mut prepared = prepared.ok_or("missing preparation")??;
                    if context.shell != std::env::var_os("SHELL") {
                        return Err("Terminal launch context changed".into());
                    }
                    let launch = LaunchRecipe::shell(
                        prepared
                            .path
                            .clone()
                            .ok_or("missing terminal destination")?,
                    )
                    .map_err(|error| error.to_string())?;
                    let launch = context.defaults.clone().apply(launch);
                    if context.shell != std::env::var_os("SHELL") {
                        return Err("Terminal launch context changed".into());
                    }
                    prepared.terminal = Some((context, launch));
                    prepared
                        .protections
                        .validate_navigation()
                        .map_err(|error| error.to_string())?;
                    Ok(prepared)
                });
            }
            Some(Command::Chat(context)) => {
                result = worker_phase(move || {
                    let mut prepared = prepared.ok_or("missing preparation")??;
                    let lexical = context.config_paths();
                    prepared.invalidation.register(lexical.iter().cloned())?;
                    let physical = lexical
                        .iter()
                        .map(|path| (path.clone(), path.canonicalize().ok()))
                        .collect::<Vec<_>>();
                    prepared
                        .invalidation
                        .register(physical.iter().filter_map(|(_, path)| path.clone()))?;
                    prepared.chat = Some(chat_ui::prepare_default_chat(context)?);
                    let validated = lexical
                        .iter()
                        .map(|path| (path.clone(), path.canonicalize().ok()))
                        .collect::<Vec<_>>();
                    if physical != validated {
                        return Err("Permission configuration paths changed while preparing".into());
                    }
                    prepared
                        .protections
                        .validate_navigation()
                        .map_err(|error| error.to_string())?;
                    Ok(prepared)
                });
            }
            None => {
                shared
                    .slot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .result = prepared;
                break;
            }
        }
    }
    #[cfg(test)]
    if let Some(before) = &shared.before_retire {
        before();
    }
    // Drain before dropping the worker's Arc: a surviving/deferred UI receipt
    // can never become the final owner of a watcher or uninstalled lease.
    let (result, command) = {
        let mut slot = shared
            .slot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (slot.result.take(), slot.command.take())
    };
    drop(result);
    drop(command);
    #[cfg(test)]
    if let Some(after) = &shared.after_retire {
        after();
    }
    let _ = notifications.try_send(Notification::Retired);
}

fn resolve(
    inputs: &Inputs,
) -> Result<
    (
        Option<PathBuf>,
        HashMap<CardId, PathBuf>,
        Vec<(PathBuf, PathBuf)>,
    ),
    String,
> {
    let mut physical = HashMap::<PathBuf, PathBuf>::new();
    if let Some(path) = &inputs.request.path {
        let selected = path.canonicalize().map_err(|error| error.to_string())?;
        physical.insert(path.clone(), selected);
    }
    for (root, _) in &inputs.roots {
        physical
            .entry(root.clone())
            .or_insert_with(|| root.canonicalize().unwrap_or_else(|_| root.clone()));
    }
    for (_, _, scope, directory) in &inputs.cards {
        for path in scope.iter().chain(directory) {
            physical
                .entry(path.clone())
                .or_insert_with(|| path.canonicalize().unwrap_or_else(|_| path.clone()));
        }
    }
    let owner = |path: &Path| {
        let canonical = physical.get(path).expect("captured physical path");
        inputs
            .roots
            .iter()
            .filter(|(root, _)| canonical.starts_with(&physical[root]))
            .max_by_key(|(root, visible)| (physical[root].components().count(), *visible))
            .map_or_else(|| canonical.clone(), |(root, _)| root.clone())
    };
    let path = inputs.request.path.as_deref().map(owner);
    let owners = inputs
        .cards
        .iter()
        .filter_map(|(id, _, scope, directory)| {
            scope
                .clone()
                .or_else(|| directory.as_deref().map(owner))
                .map(|path| (*id, path))
        })
        .collect();
    let mut physical = physical.into_iter().collect::<Vec<_>>();
    physical.sort();
    Ok((path, owners, physical))
}

fn prepare(
    inputs: Inputs,
    protections: crate::worktree_cleanup::WindowProtections,
    #[cfg(test)] runtime: Option<PathBuf>,
    #[cfg(test)] after_acquire: Option<Arc<dyn Fn(&Path) + Send + Sync>>,
) -> Result<Prepared, String> {
    let mut invalidation = Invalidation::new()?;
    invalidation.register(
        inputs
            .roots
            .iter()
            .map(|(path, _)| path.clone())
            .chain(
                inputs
                    .cards
                    .iter()
                    .flat_map(|(_, _, scope, directory)| scope.iter().chain(directory).cloned()),
            )
            .chain(inputs.protected.iter().cloned())
            .chain(inputs.request.path.iter().cloned()),
    )?;
    let epoch = invalidation.epoch.load(Ordering::Acquire);
    let (path, owners, physical) = resolve(&inputs)?;
    invalidation.register(physical.iter().map(|(_, physical)| physical.clone()))?;
    let paths = inputs
        .protected
        .iter()
        .cloned()
        .chain(owners.values().cloned())
        .chain(
            inputs
                .cards
                .iter()
                .filter_map(|(_, _, _, directory)| directory.clone()),
        )
        .chain(path.iter().cloned())
        .collect();
    let protections = protections
        .prepare_navigation(paths)
        .map_err(|error| error.to_string())?;
    let ready = {
        #[cfg(test)]
        if let Some(runtime) = runtime {
            Some(
                crate::instance::fixture_prepared_lock(&runtime.join("navigation-ready"))
                    .map_err(|error| error.to_string())?,
            )
        } else {
            Some(
                crate::instance::prepare_window_protection_ready()
                    .map_err(|error| error.to_string())?,
            )
        }
        #[cfg(not(test))]
        {
            Some(
                crate::instance::prepare_window_protection_ready()
                    .map_err(|error| error.to_string())?,
            )
        }
    };
    #[cfg(test)]
    if let (Some(after), Some(path)) = (after_acquire, path.as_deref()) {
        after(path);
    }
    protections
        .validate_navigation()
        .map_err(|error| error.to_string())?;
    let final_paths = resolve(&inputs)?;
    if final_paths != (path.clone(), owners.clone(), physical)
        || epoch != invalidation.epoch.load(Ordering::Acquire)
    {
        return Err("worktree paths changed while preparing; select the worktree again".into());
    }
    Ok(Prepared {
        inputs,
        path,
        owners,
        protections,
        ready,
        invalidation,
        epoch,
        chat: None,
        terminal: None,
    })
}

impl Cagents {
    pub(in crate::app) fn uses_navigation_scheduler(&self) -> bool {
        let local = crate::instance::window_is_initialized() && !crate::device::is_remote();
        #[cfg(test)]
        let local = local || self.worktree_selection.local_window;
        local
    }

    pub(super) fn refresh_automatic_worktree_owners(&mut self) -> Task<Message> {
        self.collect_worktree_roots();
        self.worktree_selection.refresh_scope = true;
        self.navigation_owners_changed();
        // This is background reconciliation, never a newer user navigation.
        // An explicit active/pending selection keeps its intent and priority.
        self.reconcile_navigation_protections()
    }

    pub(in crate::app) fn navigate_worktree(
        &mut self,
        path: PathBuf,
        create: bool,
        origin: Option<Origin>,
    ) -> Task<Message> {
        if !self.uses_navigation_scheduler() {
            return self.select_worktree(path, create);
        }
        self.worktree_selection.cancel();
        self.worktree_selection.pending = Some(Request {
            generation: self.worktree_selection.generation,
            path: Some(path),
            create,
            origin,
            background: false,
            refresh_channel: None,
        });
        self.dispatch_worktree_navigation()
    }

    pub(super) fn navigation_owners_changed(&mut self) {
        self.worktree_selection.owners_dirty = true;
        if self.worktree_selection.is_active() {
            // A new boundary can change owners without changing the OLD cached
            // desired paths. Revoke readiness immediately, even when collection
            // occurs after this update's ordinary protection tail.
            let paths = self.desired_worktree_protections();
            match self
                .worktree_protections
                .defer_navigation_sync(&paths, true)
            {
                Ok(Some(ready)) => self.worktree_selection.retired_readiness.push(ready),
                Ok(None) => {}
                Err(error) => {
                    self.status = Some(TransientNotice::warning(format!(
                        "Cannot defer worktree protection: {error}"
                    )))
                }
            }
        }
    }

    pub(in crate::app) fn reconcile_navigation_protections(&mut self) -> Task<Message> {
        let mut paths = self.desired_worktree_protections();
        paths.sort();
        paths.dedup();
        match self
            .worktree_protections
            .defer_navigation_sync(&paths, self.worktree_selection.owners_dirty)
        {
            Ok(Some(ready)) => self.worktree_selection.retired_readiness.push(ready),
            Ok(None) => {}
            Err(error) => {
                self.status = Some(TransientNotice::warning(format!(
                    "Cannot defer worktree protection: {error}"
                )));
                return Task::none();
            }
        }
        if self.worktree_selection.active.is_some() || self.worktree_selection.pending.is_some() {
            return Task::none();
        }
        if (!self.worktree_protections.navigation_sync_needed(&paths)
            && !self.worktree_selection.owners_dirty)
            || self
                .worktree_selection
                .failed_sync
                .as_ref()
                .is_some_and(|(failed, until)| *failed == paths && Instant::now() < *until)
        {
            if !self.worktree_selection.retired_readiness.is_empty() {
                self.worktree_selection.ticket = self.worktree_selection.ticket.wrapping_add(1);
                let ticket = self.worktree_selection.ticket;
                self.worktree_selection.active = Some((ticket, self.worktree_selection.generation));
                return self.retire_worktree_navigation(ticket, Retirement::default(), None);
            }
            return Task::none();
        }
        self.worktree_selection.pending = Some(Request {
            generation: self.worktree_selection.generation,
            path: None,
            create: false,
            origin: None,
            background: true,
            refresh_channel: None,
        });
        self.dispatch_worktree_navigation()
    }

    fn dispatch_worktree_navigation(&mut self) -> Task<Message> {
        if self.worktree_selection.active.is_some() {
            return Task::none();
        }
        if crate::device::is_remote() {
            self.worktree_selection.cancel();
            return Task::none();
        }
        let Some(mut request) = self.worktree_selection.pending.take() else {
            return Task::none();
        };
        if request.background && self.worktree_selection.refresh_scope {
            request.refresh_channel = self.durable.active_channel;
            request.path = request
                .refresh_channel
                .and_then(|channel| self.channel_directory(channel))
                .or_else(|| self.active_workspace.clone());
        }
        if request.origin.as_ref().is_some_and(|origin| {
            !origin.current(self, request.path.as_deref().expect("navigation origin"))
        }) {
            return Task::none();
        }
        self.worktree_selection.ticket = self.worktree_selection.ticket.wrapping_add(1);
        let generation = self.worktree_selection.ticket;
        self.worktree_selection.active = Some((generation, request.generation));
        let inputs = Inputs::capture(self, request);
        let protections = self.worktree_protections.navigation_snapshot();
        #[cfg(test)]
        let (runtime, before, after_acquire) = (
            self.worktree_selection.runtime.clone(),
            self.worktree_selection.before_prepare.clone(),
            self.worktree_selection.after_acquire.clone(),
        );
        let shared = Arc::new(Shared {
            #[cfg(test)]
            before_retire: self.worktree_selection.before_retire.clone(),
            #[cfg(test)]
            after_retire: self.worktree_selection.after_retire.clone(),
            ..Shared::default()
        });
        let (signal, wake) = crossbeam_channel::bounded(1);
        let (notifications, receive) = async_channel::bounded(1);
        let client = Arc::new(Client {
            shared: shared.clone(),
            wake: signal,
            notifications: receive,
            join: Mutex::new(None),
        });
        let receipt = Receipt { client, phase: 0 };
        cosmic::task::future(async move {
            let join = tokio::task::spawn_blocking(move || {
                worker(shared, wake, notifications, move || {
                    #[cfg(test)]
                    if let Some(before) = before {
                        before(inputs.request.path.as_deref().unwrap_or(Path::new("")));
                    }
                    #[cfg(test)]
                    {
                        prepare(inputs, protections, runtime, after_acquire)
                    }
                    #[cfg(not(test))]
                    {
                        prepare(inputs, protections)
                    }
                })
            });
            *receipt
                .client
                .join
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(join);
            receipt.ready(generation).await
        })
    }

    fn navigation_sync_failed(&mut self) {
        let mut paths = self.desired_worktree_protections();
        paths.sort();
        paths.dedup();
        self.worktree_selection.failed_sync =
            Some((paths, Instant::now() + Duration::from_secs(30)));
    }

    fn navigation_default_needed(&self, path: &Path, owners: &HashMap<CardId, PathBuf>) -> bool {
        let roots = self
            .known_worktree_roots
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        !self.durable.cards.iter().any(|card| {
            matches!(card.kind, CardKind::Terminal(_) | CardKind::Chat(_))
                && !self
                    .durable
                    .channels
                    .iter()
                    .any(|channel| channel.cards.contains(&card.id))
                && belongs_to_scope(card, path, &roots, owners)
        })
    }

    pub(in crate::app) fn navigation_default_provider(
        &self,
        path: &Path,
        owners: &HashMap<CardId, PathBuf>,
    ) -> crate::agent::Provider {
        let was_channel = self.durable.active_channel.is_some();
        let promoted = !was_channel
            && self.active_workspace.as_ref().is_some_and(|previous| {
                previous != path
                    && self.durable.cards.iter().any(|card| {
                        card.existence.is_open()
                            && card_directory(card) == Some(previous.as_path())
                            && owners.get(&card.id).is_some_and(|owner| owner == path)
                    })
            });
        let discard = !promoted && (was_channel || self.active_workspace.as_deref() != Some(path));
        // Called only when destination has no terminal/chat. Its post-commit
        // focused card cannot be a chat, so the ordinary constructor's fallback
        // is the last surviving durable chat after current unused defaults go.
        self.durable
            .cards
            .iter()
            .rev()
            .filter(|card| {
                card.existence.is_open()
                    && !(discard
                        && self.workspace.order().any(|id| id == card.id)
                        && self.chat_is_unused_default(card.id))
            })
            .find_map(|card| match &card.kind {
                CardKind::Chat(session) => Some(session.launch.provider),
                _ => None,
            })
            .unwrap_or(crate::agent::Provider::Codex)
    }

    fn navigation_terminal_context(&self, directory: &Path) -> TerminalContext {
        TerminalContext {
            defaults: self.terminal_launch_defaults(directory),
            shell: std::env::var_os("SHELL"),
            deferred: self.default_terminal_is_deferred(directory),
        }
    }

    fn prepare_navigation_terminal(
        &mut self,
        generation: u64,
        receipt: Receipt,
        context: TerminalContext,
    ) -> Task<Message> {
        receipt.command(Command::Terminal(context));
        cosmic::task::future(receipt.ready(generation))
    }

    fn prepare_navigation_chat(
        &mut self,
        generation: u64,
        receipt: Receipt,
        context: chat_ui::DefaultChatContext,
    ) -> Task<Message> {
        receipt.command(Command::Chat(context));
        cosmic::task::future(receipt.ready(generation))
    }

    pub(in crate::app) fn worktree_navigation_prepared(
        &mut self,
        generation: u64,
        receipt: Receipt,
    ) -> Task<Message> {
        let client = receipt.client.clone();
        let mut slot = client
            .shared
            .slot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot.phase != receipt.phase || slot.claimed {
            return Task::none();
        }
        slot.claimed = true;
        let Some(result) = slot.result.as_mut() else {
            return Task::none();
        };
        let mut retirement = Retirement::default();
        match result {
            Ok(prepared) => {
                let current = prepared.inputs.request.generation
                    == self.worktree_selection.generation
                    && self.worktree_selection.active
                        == Some((generation, prepared.inputs.request.generation))
                    && self.worktree_selection.pending.is_none();
                if current {
                    let inputs = Inputs::capture(self, prepared.inputs.request.clone());
                    if inputs.device != prepared.inputs.device
                        || inputs.window != prepared.inputs.window
                        || inputs.work != prepared.inputs.work
                    {
                        // This is a different presentation/caller context, not
                        // a request to reinterpret an old local destination.
                        self.worktree_selection.cancel();
                    } else if inputs != prepared.inputs {
                        // Latest desired selection, fresh snapshot, still behind
                        // this receipt's actual capability retirement.
                        self.worktree_selection.pending = Some(prepared.inputs.request.clone());
                    } else if prepared.epoch != prepared.invalidation.epoch.load(Ordering::Acquire)
                    {
                        self.navigation_sync_failed();
                        self.status = Some(TransientNotice::warning(
                            "Worktree paths changed; select the worktree again.",
                        ));
                    } else if !prepared
                        .path
                        .as_deref()
                        .is_some_and(|path| self.refuse_in_deleting_worktree(path))
                    {
                        let needs_default = prepared.path.as_deref().is_some_and(|path| {
                            prepared.inputs.request.create
                                && self.navigation_default_needed(path, &prepared.owners)
                        });
                        if needs_default
                            && self.settings.default_worktree_pane
                                == settings::DefaultPane::NativeChat
                        {
                            let context = self.navigation_default_chat_context(
                                prepared.path.as_deref().unwrap(),
                                self.navigation_default_provider(
                                    prepared.path.as_deref().unwrap(),
                                    &prepared.owners,
                                ),
                            );
                            if let Some(chat) = &prepared.chat {
                                if !chat.current(&context) {
                                    self.worktree_selection.pending =
                                        Some(prepared.inputs.request.clone());
                                    drop(slot);
                                    return self.retire_worktree_navigation(
                                        generation,
                                        retirement,
                                        Some(receipt),
                                    );
                                }
                            } else {
                                // Same active admission, sequential worker phase.
                                // No card/session/focus is installed speculatively.
                                drop(slot);
                                return self.prepare_navigation_chat(generation, receipt, context);
                            }
                        }
                        if needs_default
                            && self.settings.default_worktree_pane
                                == settings::DefaultPane::Terminal
                        {
                            let context =
                                self.navigation_terminal_context(prepared.path.as_deref().unwrap());
                            if let Some((captured, _)) = &prepared.terminal {
                                if *captured != context {
                                    self.worktree_selection.pending =
                                        Some(prepared.inputs.request.clone());
                                    drop(slot);
                                    return self.retire_worktree_navigation(
                                        generation,
                                        retirement,
                                        Some(receipt),
                                    );
                                }
                            } else if !context.deferred {
                                drop(slot);
                                return self
                                    .prepare_navigation_terminal(generation, receipt, context);
                            }
                        }
                        // This conservative set includes the future active root
                        // and all current editor scopes; default panes can only
                        // add the already-prepared destination root.
                        let desired = inputs
                            .protected
                            .iter()
                            .cloned()
                            .chain(prepared.owners.values().cloned())
                            .chain(prepared.path.iter().cloned())
                            .collect::<Vec<_>>();
                        match prepared.protections.accept_navigation(
                            &self.worktree_protections,
                            desired,
                            &mut prepared.ready,
                        ) {
                            Err(error) => {
                                self.navigation_sync_failed();
                                self.status = Some(TransientNotice::warning(format!(
                                    "Cannot open this worktree yet: {error}"
                                )))
                            }
                            Ok(obsolete) => {
                                retirement.obsolete = obsolete;
                                let replacement = std::mem::take(&mut prepared.protections);
                                retirement.previous = Some(std::mem::replace(
                                    &mut self.worktree_protections,
                                    replacement,
                                ));
                                self.worktree_selection.failed_sync = None;
                                self.worktree_owners = std::mem::take(&mut prepared.owners);
                                self.worktree_selection.owners_refreshed();
                                let task = if let Some(path) = &prepared.path {
                                    let scope = if let Some(channel) =
                                        prepared.inputs.request.refresh_channel
                                    {
                                        self.commit_channel_selection(channel, path.clone())
                                    } else {
                                        self.commit_worktree_selection(path.clone(), false).1
                                    };
                                    let pane = if needs_default {
                                        match self.settings.default_worktree_pane {
                                            settings::DefaultPane::NativeChat => self
                                                .insert_prepared_default_chat(
                                                    prepared
                                                        .chat
                                                        .take()
                                                        .expect("prepared current default chat"),
                                                ),
                                            settings::DefaultPane::Terminal => {
                                                if let Some((_, launch)) = prepared.terminal.take()
                                                {
                                                    self.add_prepared_default_terminal(launch)
                                                } else {
                                                    self.shared_views.deferred_default =
                                                        Some(path.clone());
                                                    Task::none()
                                                }
                                            }
                                        }
                                    } else {
                                        Task::none()
                                    };
                                    Task::batch([scope, pane])
                                } else {
                                    Task::none()
                                };
                                // Discarding unused drafts may narrow desired
                                // paths. Move obsolete guards to the same worker;
                                // no canonicalization or last-owner drop here.
                                let actual = self.desired_worktree_protections();
                                match self.worktree_protections.accept_navigation(
                                    retirement.previous.as_ref().unwrap(),
                                    actual,
                                    &mut prepared.ready,
                                ) {
                                    Ok(obsolete) => retirement.obsolete.extend(obsolete),
                                    Err(error) => {
                                        self.status = Some(TransientNotice::warning(format!(
                                            "Cannot synchronize worktree protection: {error}"
                                        )))
                                    }
                                }
                                drop(slot);
                                return self.finish_worktree_navigation(
                                    generation, retirement, receipt, task,
                                );
                            }
                        }
                    }
                }
            }
            Err(error)
                if self.worktree_selection.active
                    == Some((generation, self.worktree_selection.generation))
                    && self.worktree_selection.pending.is_none() =>
            {
                let mut paths = self.desired_worktree_protections();
                paths.sort();
                paths.dedup();
                self.worktree_selection.failed_sync =
                    Some((paths, Instant::now() + Duration::from_secs(30)));
                self.status = Some(TransientNotice::warning(format!(
                    "Cannot open this worktree yet: {error}"
                )));
            }
            Err(_) => {}
        }
        drop(slot);
        self.retire_worktree_navigation(generation, retirement, Some(receipt))
    }

    fn finish_worktree_navigation(
        &mut self,
        generation: u64,
        retirement: Retirement,
        receipt: Receipt,
        followup: Task<Message>,
    ) -> Task<Message> {
        let retirement = self.retire_worktree_navigation(generation, retirement, Some(receipt));
        #[cfg(test)]
        if let Some(effects) = &mut self.worktree_selection.followup_effects {
            // Owning headless fixtures record the real independent effects
            // without starting a terminal/daemon or provider. The ordinary
            // application and other fixtures retain concurrent Task semantics.
            effects.push(followup);
            return retirement;
        }
        // New guards already protect the installed scope. Slow old-FD/watcher
        // retirement must not postpone hydration, focus, skills or rebind work.
        // Only the next filesystem admission waits for original-worker join.
        Task::batch([retirement, followup])
    }

    fn retire_worktree_navigation(
        &mut self,
        generation: u64,
        mut retirement: Retirement,
        receipt: Option<Receipt>,
    ) -> Task<Message> {
        retirement
            .readiness
            .append(&mut self.worktree_selection.retired_readiness);
        #[cfg(test)]
        {
            retirement.observed = self.worktree_selection.after_retire.clone();
        }
        if let Some(receipt) = receipt {
            receipt.command(Command::Retire(retirement));
            return cosmic::task::future(receipt.retired(generation));
        }
        // A cleanup-only ticket for a previously accepted ready description.
        // It never owns an uninstalled preparation or watcher.
        cosmic::task::future(async move {
            let joined = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                let observed = retirement.observed.clone();
                drop(retirement);
                #[cfg(test)]
                if let Some(observed) = observed {
                    observed();
                }
            })
            .await;
            if joined.is_err() {
                std::future::pending::<()>().await;
            }
            Message::WorktreeNavigationRetired(generation)
        })
    }

    pub(in crate::app) fn worktree_navigation_retired(&mut self, generation: u64) -> Task<Message> {
        if self.worktree_selection.active.map(|(ticket, _)| ticket) != Some(generation) {
            return Task::none();
        }
        self.worktree_selection.active = None;
        self.dispatch_worktree_navigation()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct Fixture {
        app: Cagents,
        _root: tempfile::TempDir,
        runtime: PathBuf,
        old: PathBuf,
        next: PathBuf,
    }

    fn folder(app: &mut Cagents, path: &Path) -> ProjectId {
        let id = ProjectId::new();
        let mut project = crate::sidebar::SidebarProject::new(
            id,
            RepositoryId::new(),
            "navigation fixture".into(),
            path.to_owned(),
            Vec::new(),
            Vec::new(),
        );
        project.kind = ProjectKind::Folder;
        app.sidebar_model.projects.push(project);
        id
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let runtime = base.join("runtime");
        let old = base.join("old");
        let next = base.join("next");
        for path in [&runtime, &old, &next] {
            std::fs::create_dir(path).unwrap();
        }
        let mut app = crate::app::tests::context_inspector_test_app(false);
        folder(&mut app, &old);
        folder(&mut app, &next);
        app.worktree_protections
            .use_fixture_runtime(runtime.clone());
        drop(app.select_worktree(old.clone(), false));
        app.worktree_selection.local_window = true;
        app.worktree_selection.followup_effects = Some(Vec::new());
        app.worktree_selection.runtime = Some(runtime.clone());
        app.settings.default_worktree_pane = settings::DefaultPane::Terminal;
        let initial = app.reconcile_navigation_protections();
        let retirement = app.update(prepared(initial));
        no_effect(app.update(retired(retirement)));
        Fixture {
            app,
            _root: root,
            runtime,
            old,
            next,
        }
    }

    fn open(app: &mut Cagents, path: &Path) -> Task<Message> {
        let project_id = app
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.repository == path)
            .unwrap()
            .id;
        app.update(Message::Sidebar(SidebarAction::OpenRepository {
            project_id,
            repository: path.to_owned(),
        }))
    }

    fn prepared(task: Task<Message>) -> Message {
        matching(task, |message| {
            matches!(message, Message::WorktreeNavigationPrepared { .. })
        })
    }

    fn retired(task: Task<Message>) -> Message {
        matching(task, |message| {
            matches!(message, Message::WorktreeNavigationRetired(_))
        })
    }

    // A receipt deliberately outlives the Task that delivers it. Keep the test
    // executor alive across preparation, modal deferral, and original-worker
    // retirement, just as the application's executor does.
    fn executor() -> &'static tokio::runtime::Runtime {
        static EXECUTOR: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        EXECUTOR.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap()
        })
    }

    fn matching(task: Task<Message>, accept: impl Fn(&Message) -> bool) -> Message {
        use iced::futures::StreamExt;
        let mut stream = iced::runtime::task::into_stream(task).expect("owning task");
        executor().block_on(async {
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                    if accept(&message) {
                        return message;
                    }
                }
            }
            panic!("owning receipt missing")
        })
    }

    fn commit(app: &mut Cagents, mut message: Message, expected: &Path) -> Task<Message> {
        for _ in 0..3 {
            let task = app.update(message);
            if app.active_workspace.as_deref() == Some(expected) {
                return task;
            }
            message = prepared(task);
        }
        panic!("prepared navigation did not commit")
    }

    fn no_effect(task: Task<Message>) {
        use iced::futures::StreamExt;
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            while let Some(action) = executor().block_on(stream.next()) {
                assert!(
                    !matches!(
                        action,
                        iced::runtime::Action::Output(cosmic::Action::App(
                            Message::WorktreeNavigationPrepared { .. }
                                | Message::WorktreeNavigationRetired(_)
                        ))
                    ),
                    "unexpected second admission"
                );
            }
        }
    }

    fn blocked(
        app: &mut Cagents,
    ) -> (
        crossbeam_channel::Receiver<()>,
        crossbeam_channel::Sender<()>,
        Arc<AtomicUsize>,
    ) {
        let (entered, receive) = crossbeam_channel::bounded(1);
        let (release, wait) = crossbeam_channel::bounded(1);
        let starts = Arc::new(AtomicUsize::new(0));
        let counted = starts.clone();
        app.worktree_selection.before_prepare = Some(Arc::new(move |_| {
            if counted.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(10)).unwrap();
            }
        }));
        (receive, release, starts)
    }

    #[test]
    fn blocked_multi_selection_prepares_only_active_and_latest_and_retires_before_dispatch() {
        let mut f = fixture();
        let (entered, release, starts) = blocked(&mut f.app);
        let first = open(&mut f.app, &f.next);
        let worker = std::thread::spawn(move || prepared(first));
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let initial_cards = f.app.durable.cards.len();
        let mut last = f.next.clone();
        for index in 0..11 {
            last = f.old.parent().unwrap().join(format!("choice-{index}"));
            std::fs::create_dir(&last).unwrap();
            folder(&mut f.app, &last);
            no_effect(open(&mut f.app, &last));
        }
        assert_eq!(starts.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(
            f.app
                .worktree_selection
                .pending
                .as_ref()
                .unwrap()
                .path
                .as_ref(),
            Some(&last)
        );
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        assert_eq!(f.app.durable.cards.len(), initial_cards);
        assert!(crate::worktree_cleanup::fixture_protection(&f.runtime, &f.old, true).is_err());
        // Ordinary input still reaches the old strip while filesystem work is held.
        drop(
            f.app
                .update(Message::SettingsSearchChanged("still editable".into())),
        );
        assert_eq!(f.app.settings_search, "still editable");
        release.send(()).unwrap();
        let old_receipt = worker.join().unwrap();
        let duplicate = old_receipt.clone();
        let retirement = f.app.update(old_receipt);
        no_effect(f.app.update(duplicate));
        assert_eq!(starts.load(AtomicOrdering::SeqCst), 1);
        assert!(
            f.app.worktree_selection.active.is_some(),
            "completed receipt still owns admission"
        );
        let retired_message = retired(retirement);
        let next = f.app.update(retired_message.clone());
        let next_receipt = prepared(next);
        // Old retirement cannot clear the new job, even after intent-generation
        // and active-ticket counters have diverged.
        no_effect(f.app.update(retired_message));
        assert_eq!(starts.load(AtomicOrdering::SeqCst), 2);
        let retirement = commit(&mut f.app, next_receipt, &last);
        assert_eq!(f.app.active_workspace.as_ref(), Some(&last));
        assert_eq!(f.app.durable.cards.len(), initial_cards + 1);
        let done = retired(retirement);
        no_effect(f.app.update(done));
        assert!(f.app.worktree_selection.active.is_none());
    }

    #[test]
    fn accepted_current_scope_and_utility_supersede_but_refused_selection_does_not() {
        for accepted in [false, true] {
            let mut f = fixture();
            let message = prepared(f.app.navigate_worktree(f.next.clone(), false, None));
            if accepted {
                drop(f.app.open_utility_card(CardKind::Settings));
            } else {
                let missing = f.old.parent().unwrap().join("missing");
                let (selected, task) = f.app.select_worktree_checked(missing, false);
                assert!(!selected);
                no_effect(task);
            }
            let retirement = f.app.update(message);
            assert_eq!(
                f.app.active_workspace.as_ref(),
                Some(if accepted { &f.old } else { &f.next })
            );
            let done = retired(retirement);
            // A refused legacy path can alter its cached owner inputs; allow
            // the one current reprepare when required, never an old UI commit.
            let followup = f.app.update(done);
            drop(followup);
        }
        let mut f = fixture();
        let message = prepared(f.app.navigate_worktree(f.next.clone(), false, None));
        drop(f.app.select_worktree(f.old.clone(), false));
        let retirement = f.app.update(message);
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        let done = retired(retirement);
        no_effect(f.app.update(done));
    }

    #[test]
    fn removed_original_sidebar_destination_is_not_reinterpreted_as_a_known_root() {
        let mut f = fixture();
        let message = prepared(open(&mut f.app, &f.next));
        f.app
            .sidebar_model
            .projects
            .retain(|project| project.repository != f.next);
        let retirement = f.app.update(message);
        let done = retired(retirement);
        no_effect(f.app.worktree_navigation_retired(match done {
            Message::WorktreeNavigationRetired(ticket) => ticket,
            _ => unreachable!(),
        }));
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        assert!(f.app.worktree_selection.active.is_none());
    }

    #[test]
    fn prepared_commit_and_full_update_tail_keep_owner_and_protection_io_off_ui() {
        let mut f = fixture();
        f.app.settings.default_worktree_pane = settings::DefaultPane::Terminal;
        let ui = std::thread::current().id();
        let reads = Arc::new(Mutex::new(Vec::new()));
        let capture = reads.clone();
        f.app
            .worktree_protections
            .observe_navigation_io(Arc::new(move || {
                capture.lock().unwrap().push(std::thread::current().id())
            }));
        let capture = reads.clone();
        f.app.worktree_selection.owner_read = Some(Arc::new(move || {
            capture.lock().unwrap().push(std::thread::current().id())
        }));
        let retired_on = Arc::new(Mutex::new(Vec::new()));
        let capture = retired_on.clone();
        f.app.worktree_selection.after_retire = Some(Arc::new(move || {
            capture.lock().unwrap().push(std::thread::current().id())
        }));
        f.app.worktree_protections.expire_navigation_retry();
        let first = open(&mut f.app, &f.next);
        let message = prepared(first);
        // This first receipt schedules actual shell/NSS preparation, not a UI
        // LaunchRecipe::shell call and not a second navigation admission.
        let message = prepared(f.app.update(message));
        let before = f.app.durable.cards.len();
        let retirement = f.app.update(message);
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.next));
        assert_eq!(f.app.durable.cards.len(), before + 1);
        assert!(!reads.lock().unwrap().is_empty());
        assert!(reads.lock().unwrap().iter().all(|thread| *thread != ui));
        assert!(
            crate::worktree_cleanup::fixture_protection(&f.runtime, &f.old, true).is_err(),
            "old guards remain until retirement"
        );
        let done = retired(retirement);
        no_effect(f.app.update(done));
        assert!(
            retired_on
                .lock()
                .unwrap()
                .iter()
                .all(|thread| *thread != ui)
        );
        assert!(crate::worktree_cleanup::fixture_protection(&f.runtime, &f.old, true).is_ok());
        assert!(crate::worktree_cleanup::fixture_protection(&f.runtime, &f.next, true).is_err());
        assert!(reads.lock().unwrap().iter().all(|thread| *thread != ui));
    }

    #[test]
    fn a_folder_loaded_during_blocked_preparation_requeues_without_ui_canonicalization() {
        let mut f = fixture();
        let (entered, release, starts) = blocked(&mut f.app);
        let ui_reads = Arc::new(AtomicUsize::new(0));
        let capture = ui_reads.clone();
        f.app.worktree_selection.owner_read = Some(Arc::new(move || {
            capture.fetch_add(1, AtomicOrdering::SeqCst);
        }));
        let first = f.app.navigate_worktree(f.next.clone(), false, None);
        let worker = std::thread::spawn(move || prepared(first));
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let extra = f.old.parent().unwrap().join("arrived-folder");
        std::fs::create_dir(&extra).unwrap();
        folder(&mut f.app, &extra);
        // Exercise the same ordinary structural tail after a repository reply.
        drop(
            f.app
                .update(Message::SettingsSearchChanged("loaded".into())),
        );
        assert!(f.app.known_worktree_roots.contains(&extra));
        assert_eq!(ui_reads.load(AtomicOrdering::SeqCst), 0);
        release.send(()).unwrap();
        let retirement = f.app.update(worker.join().unwrap());
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        let next = f.app.update(retired(retirement));
        let retirement = f.app.update(prepared(next));
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.next));
        assert_eq!(starts.load(AtomicOrdering::SeqCst), 2);
        no_effect(f.app.update(retired(retirement)));
        assert_eq!(ui_reads.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn automatic_repository_reload_preserves_explicit_navigation_without_ui_owner_io() {
        let mut f = fixture();
        let (entered, release, starts) = blocked(&mut f.app);
        let ui_reads = Arc::new(AtomicUsize::new(0));
        let reads = ui_reads.clone();
        f.app.worktree_selection.owner_read = Some(Arc::new(move || {
            reads.fetch_add(1, AtomicOrdering::SeqCst);
        }));
        let first = f.app.navigate_worktree(f.next.clone(), false, None);
        let worker = std::thread::spawn(move || prepared(first));
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let generation = f.app.worktree_selection.generation;
        let project = f
            .app
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.repository == f.old)
            .unwrap()
            .id;
        f.app.repository_generations.insert(project, 1);
        let loaded = LoadedRepository {
            project: ProjectState {
                id: project,
                generation: 0,
                name: "refreshed folder".into(),
                repository: f.old.clone(),
                kind: ProjectKind::Folder,
                existence: Existence::Open,
            },
            repository: None,
            worktrees: Vec::new(),
            github: GitHubAvailability::NotAssociated,
            review_repository: None,
            head_repositories: HashMap::new(),
        };
        // Exercise the real accepted automatic-load branch and its full tail.
        // Returned unrelated enrichment tasks are not run by this owning test.
        drop(f.app.update(Message::RepositoryLoaded {
            open_cards: false,
            project_id: project,
            generation: 1,
            result: Ok(Box::new(loaded)),
        }));
        assert_eq!(f.app.worktree_selection.generation, generation);
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        assert_eq!(ui_reads.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(starts.load(AtomicOrdering::SeqCst), 1);
        release.send(()).unwrap();
        let retirement = f.app.update(worker.join().unwrap());
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.next));
        no_effect(f.app.update(retired(retirement)));
        assert_eq!(starts.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(ui_reads.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn cancelled_navigation_reconciles_new_nested_folder_owners_before_readiness() {
        let mut f = fixture();
        let nested = f.old.join("nested");
        let launch_directory = nested.join("app");
        std::fs::create_dir_all(&launch_directory).unwrap();
        let mut terminal = crate::app::tests::terminal_card_state();
        let CardKind::Terminal(session) = &mut terminal.kind else {
            unreachable!()
        };
        session.launch.working_directory = launch_directory;
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: session.launch.clone(),
            last_exit: None,
        };
        let card = terminal.id;
        Arc::make_mut(&mut f.app.durable).cards.push(terminal);
        f.app.cards.insert(card, CardRuntime::Loading);
        drop(f.app.select_worktree(f.old.clone(), false));
        assert_eq!(f.app.worktree_owners[&card], f.old);
        let (entered, release, starts) = blocked(&mut f.app);
        let first = f.app.navigate_worktree(f.next.clone(), false, None);
        let worker = std::thread::spawn(move || prepared(first));
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let project = ProjectId::new();
        f.app.repository_generations.insert(project, 1);
        drop(f.app.update(Message::RepositoryLoaded {
            open_cards: false,
            project_id: project,
            generation: 1,
            result: Ok(Box::new(LoadedRepository {
                project: ProjectState {
                    id: project,
                    generation: 0,
                    name: "new nested folder".into(),
                    repository: nested.clone(),
                    kind: ProjectKind::Folder,
                    existence: Existence::Open,
                },
                repository: None,
                worktrees: Vec::new(),
                github: GitHubAvailability::NotAssociated,
                review_repository: None,
                head_repositories: HashMap::new(),
            })),
        }));
        assert!(f.app.known_worktree_roots.contains(&nested));
        assert!(f.app.worktree_selection.owners_dirty);
        assert_eq!(f.app.worktree_owners[&card], f.old);
        assert!(
            crate::instance::owned_lock(
                &f.runtime.join("navigation-ready"),
                libc::LOCK_EX | libc::LOCK_NB
            )
            .is_ok(),
            "old owner map cannot advertise readiness"
        );
        drop(f.app.open_utility_card(CardKind::Settings));
        let focused = f.app.workspace.focused();
        release.send(()).unwrap();
        let retirement = f.app.update(worker.join().unwrap());
        let reconcile = f.app.update(retired(retirement));
        assert!(
            f.app.worktree_selection.is_active(),
            "owner-only work survives cancellation"
        );
        let retirement = f.app.update(prepared(reconcile));
        assert_eq!(f.app.worktree_owners[&card], nested);
        assert!(!f.app.worktree_selection.owners_dirty);
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        assert_eq!(f.app.workspace.focused(), focused);
        no_effect(f.app.update(retired(retirement)));
        assert_eq!(starts.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(f.app.worktree_owners[&card], nested);
        assert_eq!(f.app.workspace.focused(), focused);
        assert!(crate::worktree_cleanup::fixture_protection(&f.runtime, &nested, true).is_err());
        assert!(
            crate::instance::owned_lock(
                &f.runtime.join("navigation-ready"),
                libc::LOCK_EX | libc::LOCK_NB
            )
            .is_err()
        );
    }

    fn automatic_folder_load(app: &mut Cagents, project: ProjectId, path: &Path) -> Task<Message> {
        app.repository_generations.insert(project, 1);
        app.update(Message::RepositoryLoaded {
            open_cards: false,
            project_id: project,
            generation: 1,
            result: Ok(Box::new(LoadedRepository {
                project: ProjectState {
                    id: project,
                    generation: 0,
                    name: "background folder".into(),
                    repository: path.to_owned(),
                    kind: ProjectKind::Folder,
                    existence: Existence::Open,
                },
                repository: None,
                worktrees: Vec::new(),
                github: GitHubAvailability::NotAssociated,
                review_repository: None,
                head_repositories: HashMap::new(),
            })),
        })
    }

    #[test]
    fn inactive_repository_reload_prepares_owners_and_guards_without_blocking_input() {
        let mut f = fixture();
        let project = f
            .app
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.repository == f.old)
            .unwrap()
            .id;
        let reads = Arc::new(AtomicUsize::new(0));
        let record = reads.clone();
        f.app.worktree_selection.owner_read = Some(Arc::new(move || {
            record.fetch_add(1, AtomicOrdering::SeqCst);
        }));
        let (entered, release, starts) = blocked(&mut f.app);
        let task = automatic_folder_load(&mut f.app, project, &f.old);
        let worker = std::thread::spawn(move || prepared(task));
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        no_effect(f.app.update(Message::SettingsSearchChanged(
            "input while reload waits".into(),
        )));
        assert_eq!(f.app.settings_search, "input while reload waits");
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        assert_eq!(reads.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(starts.load(AtomicOrdering::SeqCst), 1);
        release.send(()).unwrap();
        let retirement = f.app.update(worker.join().unwrap());
        no_effect(f.app.update(retired(retirement)));
        assert_eq!(reads.load(AtomicOrdering::SeqCst), 0);
        assert!(f.app.worktree_selection.active.is_none());
        assert!(!f.app.worktree_selection.owners_dirty);
        assert!(!f.app.worktree_selection.refresh_scope);
    }

    #[test]
    fn due_protection_retry_uses_the_one_worker_without_ui_path_io() {
        let mut f = fixture();
        let ui = std::thread::current().id();
        let reads = Arc::new(Mutex::new(Vec::new()));
        let record = reads.clone();
        f.app
            .worktree_protections
            .observe_navigation_io(Arc::new(move || {
                record.lock().unwrap().push(std::thread::current().id());
            }));
        f.app.worktree_protections.expire_navigation_retry();
        let (entered, release, starts) = blocked(&mut f.app);
        let task = f
            .app
            .update(Message::SettingsSearchChanged("retry admitted".into()));
        let worker = std::thread::spawn(move || prepared(task));
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        no_effect(
            f.app
                .update(Message::SettingsSearchChanged("still editable".into())),
        );
        assert_eq!(f.app.settings_search, "still editable");
        assert_eq!(starts.load(AtomicOrdering::SeqCst), 1);
        assert!(reads.lock().unwrap().is_empty());
        release.send(()).unwrap();
        let retirement = f.app.update(worker.join().unwrap());
        no_effect(f.app.update(retired(retirement)));
        assert!(!reads.lock().unwrap().is_empty());
        assert!(reads.lock().unwrap().iter().all(|thread| *thread != ui));
    }

    #[test]
    fn explicit_selection_supersedes_blocked_background_reload_without_a_second_worker() {
        let mut f = fixture();
        let project = f
            .app
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.repository == f.old)
            .unwrap()
            .id;
        let (entered, release, starts) = blocked(&mut f.app);
        let task = automatic_folder_load(&mut f.app, project, &f.old);
        let worker = std::thread::spawn(move || prepared(task));
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        no_effect(open(&mut f.app, &f.next));
        assert_eq!(starts.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        release.send(()).unwrap();
        let retirement = f.app.update(worker.join().unwrap());
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        let next = f.app.update(retired(retirement));
        let retirement = commit(&mut f.app, prepared(next), &f.next);
        no_effect(f.app.update(retired(retirement)));
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.next));
        assert_eq!(starts.load(AtomicOrdering::SeqCst), 2);
        assert!(!f.app.worktree_selection.refresh_scope);
    }

    #[test]
    fn background_reload_preserves_current_channel_and_its_focused_pane() {
        let mut f = fixture();
        let project = f
            .app
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.repository == f.old)
            .unwrap()
            .id;
        let mut card = crate::app::tests::terminal_card_state();
        let CardKind::Terminal(session) = &mut card.kind else {
            unreachable!()
        };
        session.launch.working_directory = f.old.clone();
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: session.launch.clone(),
            last_exit: None,
        };
        let focused = card.id;
        let channel = ChannelId::new();
        Arc::make_mut(&mut f.app.durable).cards.push(card);
        Arc::make_mut(&mut f.app.durable)
            .channels
            .push(crate::model::ProjectChannel {
                id: channel,
                project_id: project,
                title: "kept channel".into(),
                title_source: Default::default(),
                ordinal: 0,
                shared: false,
                cards: vec![focused],
                focused: Some(focused),
            });
        f.app.cards.insert(focused, CardRuntime::Loading);
        drop(f.app.select_channel(channel));
        let generation = f.app.worktree_selection.generation;
        let count = f.app.durable.cards.len();
        let task = automatic_folder_load(&mut f.app, project, &f.old);
        let retirement = f.app.update(prepared(task));
        assert_eq!(f.app.durable.active_channel, Some(channel));
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        assert_eq!(f.app.workspace.focused(), Some(focused));
        assert_eq!(f.app.worktree_selection.generation, generation);
        assert_eq!(f.app.durable.cards.len(), count);
        no_effect(f.app.update(retired(retirement)));
    }

    #[test]
    fn background_discovery_promotes_loose_scope_without_rewriting_launch_or_focus() {
        let mut f = fixture();
        let parent = f.old.parent().unwrap().to_owned();
        let project = f
            .app
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.repository == f.old)
            .unwrap()
            .id;
        // Before discovery the project is known, but has no owning root row.
        let row = f
            .app
            .sidebar_model
            .projects
            .iter_mut()
            .find(|p| p.id == project)
            .unwrap();
        row.kind = ProjectKind::Git;
        row.repository = parent.clone();
        f.app.known_worktree_roots.remove(&f.old);
        let mut card = crate::app::tests::terminal_card_state();
        let CardKind::Terminal(session) = &mut card.kind else {
            unreachable!()
        };
        session.launch.working_directory = f.old.clone();
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: session.launch.clone(),
            last_exit: None,
        };
        let launch = session.launch.clone();
        let focused = card.id;
        Arc::make_mut(&mut f.app.durable).cards.push(card);
        f.app.cards.insert(focused, CardRuntime::Loading);
        drop(f.app.select_worktree(f.old.clone(), false));
        assert_eq!(f.app.worktree_owners[&focused], f.old);
        assert_eq!(f.app.workspace.focused(), Some(focused));
        let task = automatic_folder_load(&mut f.app, project, &parent);
        let retirement = f.app.update(prepared(task));
        assert_eq!(f.app.active_workspace.as_ref(), Some(&parent));
        assert_eq!(f.app.worktree_owners[&focused], parent);
        assert_eq!(f.app.workspace.focused(), Some(focused));
        let CardKind::Terminal(session) = &f.app.card_state(focused).unwrap().kind else {
            unreachable!()
        };
        assert_eq!(session.launch, launch);
        no_effect(f.app.update(retired(retirement)));
    }

    #[test]
    fn same_named_directory_replacement_after_acquisition_refuses_the_receipt() {
        let mut f = fixture();
        let moved = f.old.parent().unwrap().join("old-next-inode");
        let replacement = f.next.clone();
        f.app.worktree_selection.after_acquire = Some(Arc::new(move |path| {
            assert_eq!(path, replacement);
            std::fs::rename(path, &moved).unwrap();
            std::fs::create_dir(path).unwrap();
        }));
        let message = prepared(f.app.navigate_worktree(f.next.clone(), false, None));
        let Message::WorktreeNavigationPrepared { receipt, .. } = &message else {
            unreachable!()
        };
        assert!(
            receipt
                .client
                .shared
                .slot
                .lock()
                .unwrap()
                .result
                .as_ref()
                .unwrap()
                .is_err(),
            "canonical text equality cannot certify an inode"
        );
        let retirement = f.app.update(message);
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        let done = retired(retirement);
        no_effect(f.app.update(done));
        assert!(crate::worktree_cleanup::fixture_protection(&f.runtime, &f.old, true).is_err());
    }

    #[test]
    fn current_failure_uses_active_ticket_and_cooldown_after_ticket_generation_diverge() {
        let mut f = fixture();
        // A synchronization ticket has no navigation generation of its own.
        f.app.worktree_protections.expire_navigation_retry();
        let first = f.app.reconcile_navigation_protections();
        let retirement = f.app.update(prepared(first));
        no_effect(f.app.update(retired(retirement)));
        let missing = f.old.parent().unwrap().join("missing");
        let message = prepared(f.app.navigate_worktree(missing, false, None));
        let retirement = f.app.update(message);
        assert!(f.app.worktree_selection.failed_sync.is_some());
        let done = retired(retirement);
        no_effect(f.app.update(done));
        assert!(f.app.worktree_selection.active.is_none());
        no_effect(f.app.reconcile_navigation_protections());
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
    }

    #[test]
    fn modal_deferred_receipt_owns_admission_until_actual_consumption_and_retirement() {
        let mut f = fixture();
        let message = prepared(f.app.navigate_worktree(f.next.clone(), false, None));
        let active = f.app.worktree_selection.active;
        f.app.settings_reset_confirm = true;
        no_effect(f.app.update(message));
        assert_eq!(f.app.deferred_navigation.len(), 1);
        assert_eq!(f.app.worktree_selection.active, active);
        let generation = f.app.worktree_selection.generation;
        no_effect(open(&mut f.app, &f.old));
        assert_eq!(
            f.app.worktree_selection.generation, generation,
            "refused modal input must not cancel valid navigation"
        );
        assert!(f.app.worktree_selection.pending.is_none());
        let message = prepared(f.app.update(Message::CancelResetSettings));
        let retirement = f.app.update(message);
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.next));
        assert_eq!(f.app.worktree_selection.active, active);
        no_effect(f.app.update(retired(retirement)));
        assert!(f.app.worktree_selection.active.is_none());
    }

    #[test]
    fn accepted_followup_runs_while_original_filesystem_retirement_is_blocked() {
        let mut f = fixture();
        f.app.worktree_selection.followup_effects = None;
        let (entered, receive) = crossbeam_channel::bounded(1);
        let (release, wait) = crossbeam_channel::bounded(1);
        f.app.worktree_selection.before_retire = Some(Arc::new(move || {
            entered.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(10)).unwrap();
        }));
        let message = prepared(f.app.navigate_worktree(f.next.clone(), false, None));
        let Message::WorktreeNavigationPrepared {
            generation,
            receipt,
        } = message
        else {
            unreachable!()
        };
        let worker = receipt.client.join.lock().unwrap().take().unwrap();
        let shared = receipt.client.shared.clone();
        let task = f.app.finish_worktree_navigation(
            generation,
            Retirement::default(),
            receipt,
            cosmic::task::message(Message::SettingsSearchChanged("independent effect".into())),
        );
        let delivered = matching(task, |message| {
            matches!(message,
            Message::SettingsSearchChanged(text) if text == "independent effect")
        });
        assert!(matches!(delivered, Message::SettingsSearchChanged(_)));
        receive.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!worker.is_finished());
        assert!(f.app.worktree_selection.is_active());
        assert!(shared.slot.lock().unwrap().result.is_some());
        release.send(()).unwrap();
        executor().block_on(worker).unwrap();
        assert!(shared.slot.lock().unwrap().result.is_none());
    }

    #[test]
    fn modal_deferred_close_cancels_metadata_without_ui_retirement_or_false_settlement() {
        deferred_close(false);
    }

    #[test]
    fn deferred_background_reload_close_retires_original_worker_without_a_scope_commit() {
        deferred_close(true);
    }

    fn deferred_close(background: bool) {
        let mut f = fixture();
        let protected = if background {
            f.old.clone()
        } else {
            f.next.clone()
        };
        let ui = std::thread::current().id();
        let (entered, receive) = crossbeam_channel::bounded(1);
        let (release, wait) = crossbeam_channel::bounded(1);
        f.app.worktree_selection.before_retire = Some(Arc::new(move || {
            entered.send(std::thread::current().id()).unwrap();
            wait.recv_timeout(Duration::from_secs(10)).unwrap();
        }));
        let retired_on = Arc::new(Mutex::new(Vec::new()));
        let observed = retired_on.clone();
        f.app.worktree_selection.after_retire = Some(Arc::new(move || {
            observed.lock().unwrap().push(std::thread::current().id());
        }));
        let task = if background {
            let project = f
                .app
                .sidebar_model
                .projects
                .iter()
                .find(|project| project.repository == f.old)
                .unwrap()
                .id;
            automatic_folder_load(&mut f.app, project, &f.old)
        } else {
            f.app.navigate_worktree(f.next.clone(), false, None)
        };
        let message = prepared(task);
        let Message::WorktreeNavigationPrepared { receipt, .. } = &message else {
            unreachable!()
        };
        // Retain the actual join proof, not another UI receipt/client lease.
        let worker = receipt.client.join.lock().unwrap().take().unwrap();
        let shared = receipt.client.shared.clone();
        let client = Arc::downgrade(&receipt.client);
        f.app.settings_reset_confirm = true;
        no_effect(f.app.update(message));
        assert_eq!(f.app.deferred_navigation.len(), 1);
        assert!(crate::worktree_cleanup::fixture_protection(&f.runtime, &protected, true).is_err());
        drop(f.app); // No close-policy hook: the ordinary field teardown cancels metadata.
        let retirement_thread = receive.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_ne!(retirement_thread, ui);
        assert!(client.upgrade().is_none());
        assert!(shared.cancelled.load(Ordering::Acquire));
        assert!(
            !worker.is_finished(),
            "blocked retirement is not settlement"
        );
        assert!(shared.slot.lock().unwrap().result.is_some());
        assert!(retired_on.lock().unwrap().is_empty());
        assert!(crate::worktree_cleanup::fixture_protection(&f.runtime, &protected, true).is_err());
        release.send(()).unwrap();
        executor().block_on(worker).unwrap();
        assert!(shared.slot.lock().unwrap().result.is_none());
        assert_eq!(*retired_on.lock().unwrap(), vec![retirement_thread]);
        assert!(crate::worktree_cleanup::fixture_protection(&f.runtime, &protected, true).is_ok());
    }

    #[test]
    fn accepted_channel_work_presentation_supersedes_but_invalid_channel_does_not() {
        for accepted in [false, true] {
            let mut f = fixture();
            let channel = crate::model::ChannelId::new();
            f.app.workspace_model.identity = uuid::Uuid::new_v4();
            f.app
                .workspace_model
                .channels
                .push(crate::workspace_model::ModelChannel {
                    id: channel,
                    repository: f.old.clone(),
                    title: "Work fixture".into(),
                    title_source: Default::default(),
                    ordinal: 1,
                });
            let message = prepared(f.app.navigate_worktree(f.next.clone(), false, None));
            drop(f.app.open_channel_work(if accepted {
                channel
            } else {
                crate::model::ChannelId::new()
            }));
            assert_eq!(f.app.channel_work_is_main(), accepted);
            let retirement = f.app.update(message);
            assert_eq!(
                f.app.active_workspace.as_ref(),
                Some(if accepted { &f.old } else { &f.next })
            );
            no_effect(f.app.update(retired(retirement)));
            assert_eq!(f.app.channel_work_is_main(), accepted);
        }
    }
}
