//! The daemon's Work board: one read model of every worktree and branch the
//! user is working on, shared by the CLI and the windows.
//!
//! A coordinator thread decides which projects to scan and a few workers
//! scan them, so one large checkout delays only itself. The watcher wakes the
//! coordinator for the projects within the watch budget; the rest are polled,
//! sooner while something happens in them. A separate forge thread polls
//! GitHub with bounded `gh` calls, so a slow forge delays no local scan.
//! Requests assemble a board from the latest facts without spawning a
//! process unless they ask for a fresh one. A publisher thread sends windows
//! that watch the board each changed board as soon as a scan, poll, pin or
//! session changes it.
mod board;
mod forge;
mod removal;
mod scan;
mod store;
mod tickets;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, Weak},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use super::{
    agent_execution,
    delivery::{DeliveryQueue, Outbound},
    state::{ClientId, HostLock, SharedHost},
};
use crate::{
    agent::ActivityState,
    git_watch,
    model::{ProjectId, ProjectKind},
    repository::CancellationToken,
    terminal_ipc::ServerMessage,
    work::{Board, Column, Forge, Item, Request, Response, Scope},
};

/// The shortest gap between two event-driven scans of one project; edits in
/// between coalesce into the next.
const SCAN_SPACING: Duration = Duration::from_secs(2);
/// A failed scan is retried this long after, or as soon as someone reads the
/// board; its changes wait for the retry.
const SCAN_RETRY: Duration = Duration::from_secs(30);
/// Polling for projects beyond the watch budget: soon after recent activity,
/// rarely otherwise.
const ACTIVE_POLL: Duration = Duration::from_secs(15);
const IDLE_POLL: Duration = Duration::from_secs(120);
/// A watched project is still re-read now and then; a watcher can miss what
/// happens on another filesystem or while it re-registers.
const WATCHED_POLL: Duration = Duration::from_secs(10 * 60);
const SESSION_SAMPLE: Duration = Duration::from_secs(15);
/// How often an agent's work in one worktree is written down.
const SESSION_STAMP: Duration = Duration::from_secs(60);
const MODEL_REFRESH: Duration = Duration::from_secs(30);
/// A board counts as being read for this long after a request.
const READ_WINDOW: Duration = Duration::from_secs(120);
const FIRST_SCAN_WAIT: Duration = Duration::from_secs(5);
const FRESH_WAIT: Duration = Duration::from_secs(20);
/// A fresh request accepts a scan or poll this recent rather than another.
const FRESH_ENOUGH: Duration = Duration::from_secs(5);
/// Scan workers: one slow repository holds up only its own.
const SCAN_WORKERS: usize = 4;
/// Bursts of file events coalesce into one scanner pass per interval.
const TICK_FLOOR: Duration = Duration::from_millis(250);
/// Changes arriving together, such as several workers finishing at once, are
/// sent to watchers as one board.
const PUBLISH_FLOOR: Duration = Duration::from_millis(150);
/// How often a watched board is reassembled without a change to wake it:
/// session states and the clock move cards without a scan.
const REPUBLISH: Duration = Duration::from_secs(5);
/// Unwatched, the publisher waits for a watcher's notification.
const UNWATCHED: Duration = Duration::from_secs(60 * 60);

#[derive(Default)]
struct SignalState {
    generation: u64,
    stopped: bool,
}

/// Wakes one worker thread; a notification before it waits is not lost.
#[derive(Default)]
struct Signal {
    state: Mutex<SignalState>,
    changed: Condvar,
}

impl Signal {
    fn notify(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.generation = state.generation.wrapping_add(1);
        self.changed.notify_all();
    }

    fn stop(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).stopped = true;
        self.changed.notify_all();
    }

    fn observe(&self) -> Option<u64> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        (!state.stopped).then_some(state.generation)
    }

    fn wait(&self, observed: u64, timeout: Duration) {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let _ = self
            .changed
            .wait_timeout_while(state, timeout, |state| {
                !state.stopped && state.generation == observed
            })
            .unwrap_or_else(|p| p.into_inner());
    }
}

/// One project's scan, handed from the scanner to a worker.
struct Job {
    repository: PathBuf,
    id: ProjectId,
    previous: Option<Arc<scan::Local>>,
    change: git_watch::Change,
    hints: Arc<scan::ForgeHints>,
    sequence: u64,
}

#[derive(Default)]
struct JobsState {
    queue: VecDeque<Job>,
    stopped: bool,
}

/// Scans waiting for a worker.
#[derive(Default)]
struct Jobs {
    state: Mutex<JobsState>,
    ready: Condvar,
}

impl Jobs {
    fn push(&self, jobs: Vec<Job>) {
        if jobs.is_empty() {
            return;
        }
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .queue
            .extend(jobs);
        self.ready.notify_all();
    }

    fn stop(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).stopped = true;
        self.ready.notify_all();
    }

    /// The next job, waiting for one; `None` once stopped.
    fn next(&self) -> Option<Job> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = self
            .ready
            .wait_while(state, |state| !state.stopped && state.queue.is_empty())
            .unwrap_or_else(|p| p.into_inner());
        if state.stopped {
            return None;
        }
        state.queue.pop_front()
    }
}

/// One open Git project as the board sees it.
struct Tracked {
    id: ProjectId,
    name: String,
    local: Option<Arc<scan::Local>>,
    scan_error: Option<String>,
    /// Scans handed to a worker, and the latest of them that completed. A
    /// fresh request waits for one that started after it asked.
    scans_started: u64,
    scans: u64,
    scanning: bool,
    /// What changed since the last scan; `Some` while a scan is wanted.
    pending: Option<git_watch::Change>,
    /// A request asked for this scan and waits on it.
    urgent: bool,
    last_scan: Option<Instant>,
    /// Whether the watcher covers every worktree, so polling can rest.
    covered: bool,
    hints: Arc<scan::ForgeHints>,
    forge: Forge,
    snapshot: Option<Arc<forge::Snapshot>>,
    /// Polls begun, and the latest of them that completed.
    polls_started: u64,
    polls: u64,
    poll_requested: bool,
    last_poll: Option<Instant>,
    next_poll: Instant,
    failures: u32,
    memory: forge::Memory,
}

impl Tracked {
    fn new(name: String) -> Self {
        Self {
            id: ProjectId::new(),
            name,
            local: None,
            scan_error: None,
            scans_started: 0,
            scans: 0,
            scanning: false,
            pending: Some(full_change()),
            urgent: false,
            last_scan: None,
            covered: false,
            hints: Arc::default(),
            forge: Forge::Pending,
            snapshot: None,
            polls_started: 0,
            polls: 0,
            poll_requested: false,
            last_poll: None,
            next_poll: Instant::now(),
            failures: 0,
            memory: forge::Memory::default(),
        }
    }

    /// The shortest gap before this project's next scan.
    fn spacing(&self) -> Duration {
        if self.scan_error.is_some() {
            SCAN_RETRY
        } else {
            SCAN_SPACING
        }
    }

    fn restore_snapshot(&mut self, synced_at_unix_ms: u64, snapshot: forge::Snapshot) {
        self.hints = Arc::new(snapshot.hints());
        self.snapshot = Some(Arc::new(snapshot));
        self.forge = Forge::Synced {
            at_unix_ms: synced_at_unix_ms,
            error: None,
        };
    }

    /// The pins a scan proves stale. A branch's pin follows it between
    /// worktrees, so it lasts while the branch is checked out, has a pull
    /// request in flight, or still exists without a finished one; a worktree
    /// card's lasts while the worktree exists.
    fn live_pin(&self, item: &str) -> bool {
        let Some(local) = &self.local else {
            return true;
        };
        match crate::work::ItemKey::from_storage_key(item) {
            Some(crate::work::ItemKey::Branch(branch)) => {
                let pull_requests = self
                    .snapshot
                    .iter()
                    .flat_map(|snapshot| &snapshot.pull_requests)
                    .filter(|pull_request| pull_request.head_ref_name == branch);
                let (in_flight, finished) =
                    pull_requests.fold((false, false), |(in_flight, finished), pull_request| {
                        let live = forge::in_flight(pull_request);
                        (in_flight || live, finished || !live)
                    });
                in_flight
                    || local
                        .worktrees
                        .iter()
                        .any(|worktree| worktree.branch.as_ref() == Some(&branch))
                    || (local.branch_names.contains(&branch) && !finished)
            }
            Some(crate::work::ItemKey::Worktree(path)) => {
                local.worktrees.iter().any(|worktree| worktree.path == path)
            }
            None => true,
        }
    }
}

fn full_change() -> git_watch::Change {
    git_watch::Change {
        topology: true,
        ..git_watch::Change::default()
    }
}

struct Shared {
    projects: BTreeMap<PathBuf, Tracked>,
    pins: BTreeMap<(PathBuf, String), Column>,
    session_activity: HashMap<PathBuf, u64>,
    /// Resolved Linear tickets by reference: a linked issue's id or a branch
    /// name's identifier.
    tickets: BTreeMap<String, store::Ticket>,
    /// Why Linear cannot be asked about tickets cards still want.
    linear_unavailable: Option<String>,
    stale_after_days: u16,
    read_at: Option<Instant>,
    /// Forge listings read from the store for projects not yet tracked.
    restored: BTreeMap<PathBuf, (u64, forge::Snapshot)>,
}

impl Shared {
    /// The tickets cards could show: every worktree's link, else its branch
    /// name's identifier, and the identifiers of the branches the user's open
    /// pull requests and closed drafts come from. A superset of the cards'
    /// tickets is fine; each is looked up once.
    fn wanted_tickets(&self) -> BTreeSet<tickets::Reference> {
        let mut wanted = BTreeSet::new();
        for tracked in self.projects.values() {
            for worktree in tracked
                .local
                .iter()
                .flat_map(|local| local.worktrees.iter())
            {
                let reference = match &worktree.link {
                    Some(link) => Some(tickets::Reference::linked(link)),
                    None => worktree
                        .branch
                        .as_deref()
                        .and_then(tickets::Reference::from_branch),
                };
                wanted.extend(reference);
            }
            for pull_request in tracked
                .snapshot
                .iter()
                .flat_map(|snapshot| snapshot.pull_requests.iter())
                .filter(|pull_request| forge::in_flight(pull_request))
            {
                wanted.extend(tickets::Reference::from_branch(&pull_request.head_ref_name));
            }
        }
        wanted
    }

    fn being_read(&self) -> bool {
        self.read_at
            .is_some_and(|read_at| read_at.elapsed() < READ_WINDOW)
    }

    /// How long a project goes without a re-read when no watcher event asks
    /// for one.
    fn poll_interval(&self, tracked: &Tracked) -> Duration {
        if tracked.covered {
            WATCHED_POLL
        } else if self.active(tracked) {
            ACTIVE_POLL
        } else {
            IDLE_POLL
        }
    }

    /// Whether a project's files or agents changed within the active window.
    fn active(&self, tracked: &Tracked) -> bool {
        let recent = |at: u64| now_unix_ms().saturating_sub(at) <= crate::work::ACTIVE_WINDOW_MS;
        tracked.local.as_ref().is_some_and(|local| {
            local.worktrees.iter().any(|worktree| {
                worktree
                    .dirty
                    .as_ref()
                    .and_then(|dirty| dirty.newest_modified_at_unix_ms)
                    .is_some_and(recent)
                    || self
                        .session_activity
                        .get(&worktree.path)
                        .copied()
                        .is_some_and(recent)
            })
        })
    }

    fn track(&mut self, repository: &Path, name: &str) -> &mut Tracked {
        let restored = &mut self.restored;
        let tracked = self
            .projects
            .entry(repository.to_owned())
            .or_insert_with(|| {
                let mut tracked = Tracked::new(name.to_owned());
                if let Some((at, snapshot)) = restored.remove(repository) {
                    tracked.restore_snapshot(at, snapshot);
                }
                tracked
            });
        tracked.name = name.to_owned();
        tracked
    }
}

/// The connections watching the board and what they were last sent.
#[derive(Default)]
struct Publication {
    watchers: BTreeMap<ClientId, Weak<DeliveryQueue>>,
    revision: u64,
    /// The last board sent, with its generation time cleared, so a pass that
    /// changes nothing sends nothing.
    sent: Option<Board>,
}

impl Publication {
    /// Makes `board` the watchers' board, sending it to all but `except` when
    /// it differs from the last one sent.
    fn offer(&mut self, host: &SharedHost, board: &Board, except: Option<ClientId>) {
        let mut comparable = board.clone();
        comparable.generated_at_unix_ms = 0;
        if self.sent.as_ref() == Some(&comparable) {
            return;
        }
        self.sent = Some(comparable);
        self.revision += 1;
        let proof = Arc::new(
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .proof()
                .clone(),
        );
        let revision = self.revision;
        self.watchers.retain(|client, delivery| {
            if Some(*client) == except {
                return true;
            }
            let Some(delivery) = delivery.upgrade() else {
                return false;
            };
            delivery
                .push(Outbound::broadcast(
                    &proof,
                    ServerMessage::WorkBoard {
                        revision,
                        board: Box::new(board.clone()),
                    },
                ))
                .is_ok()
        });
    }
}

struct Inner {
    host: Weak<HostLock>,
    shared: Mutex<Shared>,
    /// Signalled whenever a scan or poll completes; requests wait on it.
    updated: Condvar,
    scanner: Signal,
    forge: Signal,
    /// Wakes the publisher when the board may have changed.
    publisher: Signal,
    /// Held while a board is assembled and sent, so watchers receive boards
    /// in the order their facts were read.
    publication: Mutex<Publication>,
    jobs: Jobs,
    /// `None` when the store could not be opened: the board still works, but
    /// pins and session history last only as long as the daemon.
    store: Mutex<Option<store::Store>>,
    cancellation: CancellationToken,
}

#[derive(Clone)]
pub(in crate::host) struct Handle(Arc<Inner>);

pub(in crate::host) struct Service {
    handle: Handle,
    threads: Vec<JoinHandle<()>>,
}

impl Service {
    pub(super) fn start(host: &SharedHost) -> Result<Self, String> {
        let path = crate::instance::project_dirs()
            .ok_or("the data directory is unavailable")?
            .data_dir()
            .join("work.sqlite3");
        Self::start_with(host, store::Store::open(&path))
    }

    fn start_with(host: &SharedHost, store: Result<store::Store, String>) -> Result<Self, String> {
        let mut store = store
            .inspect_err(|error| {
                tracing::warn!(%error, "Work board pins and session history will not persist");
            })
            .ok();
        let mut shared = Shared {
            projects: BTreeMap::new(),
            pins: BTreeMap::new(),
            session_activity: HashMap::new(),
            tickets: BTreeMap::new(),
            linear_unavailable: None,
            stale_after_days: stale_after_days(),
            read_at: None,
            restored: BTreeMap::new(),
        };
        if let Some(store) = &mut store {
            let restored = (|| -> Result<(), String> {
                shared.pins = store.pins()?;
                shared.session_activity = store.session_activity()?;
                shared.tickets = store.tickets()?;
                for (repository, (at, snapshot)) in store.forge_snapshots()? {
                    // A listing an older layout wrote is simply polled again.
                    if let Ok(snapshot) = serde_json::from_str(&snapshot) {
                        shared.restored.insert(repository, (at, snapshot));
                    }
                }
                Ok(())
            })();
            if let Err(error) = restored {
                tracing::warn!(%error, "the Work board store could not be read");
            }
        }
        let handle = Handle(Arc::new(Inner {
            host: Arc::downgrade(host),
            shared: Mutex::new(shared),
            updated: Condvar::new(),
            scanner: Signal::default(),
            forge: Signal::default(),
            publisher: Signal::default(),
            publication: Mutex::new(Publication::default()),
            jobs: Jobs::default(),
            store: Mutex::new(store),
            cancellation: CancellationToken::new(),
        }));
        let mut service = Self {
            handle: handle.clone(),
            threads: Vec::new(),
        };
        let scanner = handle.clone();
        service.threads.push(
            std::thread::Builder::new()
                .name("work-scanner".into())
                .spawn(move || scanner.scan_loop())
                .map_err(|error| error.to_string())?,
        );
        let forge = handle.clone();
        service.threads.push(
            std::thread::Builder::new()
                .name("work-forge".into())
                .spawn(move || forge.forge_loop())
                .map_err(|error| error.to_string())?,
        );
        let publisher = handle.clone();
        service.threads.push(
            std::thread::Builder::new()
                .name("work-publisher".into())
                .spawn(move || publisher.publish_loop())
                .map_err(|error| error.to_string())?,
        );
        for _ in 0..SCAN_WORKERS {
            let worker = handle.clone();
            service.threads.push(
                std::thread::Builder::new()
                    .name("work-scan".into())
                    .spawn(move || worker.scan_worker())
                    .map_err(|error| error.to_string())?,
            );
        }
        Ok(service)
    }

    pub(super) fn handle(&self) -> Handle {
        self.handle.clone()
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.handle.0.cancellation.cancel();
        self.handle.0.scanner.stop();
        self.handle.0.forge.stop();
        self.handle.0.publisher.stop();
        self.handle.0.jobs.stop();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn stale_after_days() -> u16 {
    let Some(path) = crate::instance::settings_path() else {
        return crate::work::DEFAULT_STALE_AFTER_DAYS;
    };
    crate::work::stale_after_days(&path).unwrap_or_else(|error| {
        tracing::warn!(%error, "the Work board's stale window falls back to its default");
        crate::work::DEFAULT_STALE_AFTER_DAYS
    })
}

fn now_unix_ms() -> u64 {
    crate::chat::now_ms().unwrap_or_default()
}

/// The open Git projects: repository and name.
fn open_projects(host: &SharedHost) -> Result<Vec<(PathBuf, String)>, String> {
    let model = agent_execution::registry(host)?
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .model()?;
    Ok(model
        .projects
        .into_iter()
        .filter(|project| project.kind == ProjectKind::Git)
        .map(|project| (project.repository, project.name))
        .collect())
}

/// Live sessions with their directories canonicalized, so a session started
/// through a symlink still lands on its worktree.
fn live_sessions(host: &SharedHost) -> Vec<board::LiveSession> {
    agent_execution::live_sessions(host)
        .into_iter()
        .map(|live| board::LiveSession {
            id: live.id,
            provider: live.provider,
            title: live.title,
            state: live.state,
            directory: std::fs::canonicalize(&live.directory).unwrap_or(live.directory),
        })
        .collect()
}

impl Handle {
    fn shared(&self) -> MutexGuard<'_, Shared> {
        self.0.shared.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn store<T>(&self, write: impl FnOnce(&mut store::Store) -> Result<T, String>) -> Option<T> {
        let mut store = self.0.store.lock().unwrap_or_else(|p| p.into_inner());
        let result = write(store.as_mut()?);
        result
            .inspect_err(|error| tracing::warn!(%error, "the Work board store refused a write"))
            .ok()
    }

    pub(super) fn process(&self, host: &SharedHost, request: Request) -> Result<Response, String> {
        match request {
            Request::Board { scope, fresh } => self.board(host, &scope, fresh).map(Response::Board),
            Request::Pin {
                scope,
                item,
                column,
            } => self
                .pin(host, &scope, &item, column)
                .map(|item| Response::Pinned(Box::new(item))),
            Request::RemovalPlan { scope, item, fetch } => self
                .removal_plan(host, &scope, &item, fetch)
                .map(|plan| Response::RemovalPlan(Box::new(plan))),
            Request::Remove(removal) => self
                .remove(host, &removal)
                .map(|removed| Response::Removed(Box::new(removed))),
            Request::Snapshots { scope } => self.snapshots(host, &scope).map(Response::Snapshots),
        }
    }

    /// Rescans `repository` soon, so the board drops what was removed.
    fn rescan(&self, repository: &Path) {
        if let Some(tracked) = self.shared().projects.get_mut(repository) {
            tracked
                .pending
                .get_or_insert_with(Default::default)
                .merge(full_change());
            tracked.urgent = true;
        }
        self.0.scanner.notify();
    }

    /// The repositories `scope` covers, read from the shared workspace model.
    fn resolve(&self, host: &SharedHost, scope: &Scope) -> Result<Vec<(PathBuf, String)>, String> {
        let model = agent_execution::registry(host)?
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .model()?;
        let git = |project: &crate::workspace_model::ModelProject| {
            (project.kind == ProjectKind::Git)
                .then(|| (project.repository.clone(), project.name.clone()))
        };
        let path = match scope {
            Scope::All => return Ok(model.projects.iter().filter_map(git).collect()),
            Scope::Project(path) => path,
        };
        let repository = if model.project(path).is_some() {
            path.clone()
        } else if path.is_relative() {
            agent_execution::named(&model, path)?
        } else {
            crate::project_location::place(path, &model)
                .project
                .ok_or_else(|| format!("No open project contains {}", path.display()))?
        };
        let project = model
            .project(&repository)
            .ok_or_else(|| format!("No open project contains {}", path.display()))?;
        git(project)
            .map(|project| vec![project])
            .ok_or_else(|| format!("{} is not a Git project", project.name))
    }

    fn board(&self, host: &SharedHost, scope: &Scope, fresh: bool) -> Result<Board, String> {
        let selected = self.resolve(host, scope)?;
        self.settle(&selected, fresh);
        Ok(self.assemble(host, &selected))
    }

    /// Waits, within a bound, for what a board of `selected` needs: each
    /// project's first scan, and with `fresh` a scan and a forge poll that
    /// start after this call.
    fn settle(&self, selected: &[(PathBuf, String)], fresh: bool) {
        let deadline = Instant::now() + if fresh { FRESH_WAIT } else { FIRST_SCAN_WAIT };
        let mut targets = Vec::new();
        {
            let mut shared = self.shared();
            shared.read_at = Some(Instant::now());
            for (repository, name) in selected {
                let tracked = shared.track(repository, name);
                let recent = |at: Option<Instant>| at.is_some_and(|at| at.elapsed() < FRESH_ENOUGH);
                // Waits for a scan that starts after this request: one already
                // running may have read the worktrees before the change the
                // caller wants to see.
                let rescan = fresh && !recent(tracked.last_scan);
                // A first scan that just failed answers with its failure.
                let first = tracked.local.is_none()
                    && !(tracked.scan_error.is_some() && recent(tracked.last_scan));
                let scan = (rescan || first).then(|| {
                    if rescan || !tracked.scanning {
                        tracked
                            .pending
                            .get_or_insert_with(Default::default)
                            .merge(full_change());
                    }
                    tracked.urgent = true;
                    // A first scan answers whenever it completes.
                    if rescan {
                        tracked.scans_started + 1
                    } else {
                        tracked.scans + 1
                    }
                });
                let poll =
                    (fresh && tracked.forge != Forge::NotGitHub && !recent(tracked.last_poll))
                        .then(|| {
                            tracked.poll_requested = true;
                            tracked.polls_started + 1
                        });
                // Unread, listings go stale for minutes; the first read in a
                // while refreshes them without waiting for the answer.
                if tracked.forge != Forge::NotGitHub
                    && tracked
                        .last_poll
                        .is_some_and(|at| at.elapsed() >= forge::WATCHED_INTERVAL)
                {
                    tracked.poll_requested = true;
                }
                targets.push((repository.clone(), scan, poll));
            }
        }
        self.0.scanner.notify();
        self.0.forge.notify();
        let shared = self.shared();
        let _settled = self
            .0
            .updated
            .wait_timeout_while(
                shared,
                deadline.saturating_duration_since(Instant::now()),
                |shared| {
                    targets.iter().any(|(repository, scan, poll)| {
                        shared.projects.get(repository).is_some_and(|tracked| {
                            scan.is_some_and(|scan| tracked.scans < scan)
                                // The forge reads a project only once it has
                                // scanned it.
                                || poll.is_some_and(|poll| {
                                    tracked.polls < poll
                                        && tracked.forge != Forge::NotGitHub
                                        && (tracked.local.is_some() || tracked.scan_error.is_none())
                                })
                        })
                    })
                },
            )
            .unwrap_or_else(|p| p.into_inner());
    }

    /// The board of `selected` from the latest facts, without waiting.
    fn assemble(&self, host: &SharedHost, selected: &[(PathBuf, String)]) -> Board {
        let shared = self.shared();
        // Take what the board needs and release the lock before reading the
        // live sessions, which takes the host lock.
        let projects = selected
            .iter()
            .filter_map(|(repository, _)| {
                let tracked = shared.projects.get(repository)?;
                Some((
                    repository.clone(),
                    tracked.name.clone(),
                    tracked.local.clone(),
                    tracked.scan_error.clone(),
                    tracked.forge.clone(),
                    tracked.snapshot.clone(),
                ))
            })
            .collect::<Vec<_>>();
        let pins = shared.pins.clone();
        let session_activity = shared.session_activity.clone();
        let tickets = shared.tickets.clone();
        let linear_unavailable = shared.linear_unavailable.clone();
        let stale_after_days = shared.stale_after_days;
        drop(shared);
        let sessions = live_sessions(host);
        let views = projects
            .iter()
            .map(
                |(repository, name, local, scan_error, forge, snapshot)| board::ProjectView {
                    repository,
                    name,
                    local: local.as_deref(),
                    scan_error: scan_error.as_deref(),
                    forge,
                    snapshot: snapshot.as_deref(),
                },
            )
            .collect::<Vec<_>>();
        board::assemble(
            &views,
            &board::Context {
                now_unix_ms: now_unix_ms(),
                stale_after_days,
                pins: &pins,
                session_activity: &session_activity,
                sessions: &sessions,
                tickets: &tickets,
                linear_unavailable: linear_unavailable.as_deref(),
            },
        )
    }

    /// Subscribes `client` to every board change and returns the whole board
    /// as of the revision it starts from.
    pub(super) fn watch(
        &self,
        host: &SharedHost,
        client: ClientId,
        delivery: Weak<DeliveryQueue>,
    ) -> Result<(u64, Board), String> {
        let selected = self.resolve(host, &Scope::All)?;
        self.settle(&selected, false);
        let mut publication = self.publication();
        publication.watchers.insert(client, delivery);
        // Assembled while subscribed, so no later change can miss this
        // client; the others are sent the same board if it is new to them.
        let board = self.assemble(host, &selected);
        publication.offer(host, &board, Some(client));
        let revision = publication.revision;
        drop(publication);
        // The publisher now has a watcher to republish for.
        self.0.publisher.notify();
        Ok((revision, board))
    }

    pub(super) fn unwatch(&self, client: ClientId) {
        self.publication().watchers.remove(&client);
    }

    fn publication(&self) -> MutexGuard<'_, Publication> {
        self.0.publication.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Something the board shows may have changed: wakes requests waiting on
    /// facts and the publisher.
    fn changed(&self) {
        self.0.updated.notify_all();
        self.0.publisher.notify();
    }

    fn publish_loop(&self) {
        while let Some(observed) = self.0.publisher.observe() {
            let Some(host) = self.0.host.upgrade() else {
                break;
            };
            let started = Instant::now();
            let watched =
                super::service::attempt("Work board publishing", || Ok(self.republish(&host)))
                    .unwrap_or_else(|error| {
                        tracing::warn!(%error, "Work board publishing is temporarily unavailable");
                        true
                    });
            drop(host);
            // Unwatched, only a new watcher's notification wakes this thread.
            self.0
                .publisher
                .wait(observed, if watched { REPUBLISH } else { UNWATCHED });
            std::thread::sleep(PUBLISH_FLOOR.saturating_sub(started.elapsed()));
        }
    }

    /// Sends watchers the board when it changed, and reports whether anyone
    /// is watching.
    fn republish(&self, host: &SharedHost) -> bool {
        let mut publication = self.publication();
        publication
            .watchers
            .retain(|_, delivery| delivery.strong_count() > 0);
        if publication.watchers.is_empty() {
            publication.sent = None;
            return false;
        }
        let selected = match self.resolve(host, &Scope::All) {
            Ok(selected) => selected,
            Err(error) => {
                tracing::debug!(%error, "the Work board could not read the open projects");
                return true;
            }
        };
        let untracked = {
            let mut shared = self.shared();
            // A watched board is a read, which keeps polls and tickets current.
            shared.read_at = Some(Instant::now());
            let untracked = selected
                .iter()
                .any(|(repository, _)| !shared.projects.contains_key(repository));
            for (repository, name) in &selected {
                shared.track(repository, name);
            }
            untracked
        };
        if untracked {
            // A project opened since the last pass is scanned now rather than
            // at the next model refresh.
            self.0.scanner.notify();
        }
        let board = self.assemble(host, &selected);
        publication.offer(host, &board, None);
        true
    }

    /// The one card on `board` named by `name`: its branch, its storage key or
    /// its worktree's path.
    fn card(board: &Board, name: &str) -> Result<Item, String> {
        // A path names a worktree however it is spelled: through a symlink,
        // or as Git recorded it.
        let path = Path::new(name)
            .is_absolute()
            .then(|| std::fs::canonicalize(name).ok())
            .flatten();
        let mut matches = board
            .items
            .iter()
            .filter(|item| {
                item.branch.as_deref() == Some(name)
                    || item.key.storage_key() == name
                    || item.worktree.as_ref().is_some_and(|worktree| {
                        worktree.path == Path::new(name)
                            || path.as_ref().is_some_and(|path| {
                                std::fs::canonicalize(&worktree.path)
                                    .is_ok_and(|card| card == *path)
                            })
                    })
            })
            .collect::<Vec<_>>();
        match matches.len() {
            0 => Err(format!("No card on the board is named {name}")),
            1 => Ok(matches.remove(0).clone()),
            count => Err(format!(
                "{count} cards are named {name}, in {}; choose one with --project",
                matches
                    .iter()
                    .map(|item| item.project_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    fn pin(
        &self,
        host: &SharedHost,
        scope: &Scope,
        name: &str,
        column: Option<Column>,
    ) -> Result<Item, String> {
        let board = self.board(host, scope, false)?;
        let mut item = Self::card(&board, name)?;
        let key = item.key.storage_key();
        self.store(|store| store.set_pin(&item.project, &key, column))
            .ok_or("The Work board store is unavailable, so pins cannot be saved")?;
        {
            let mut shared = self.shared();
            let entry = (item.project.clone(), key);
            match column {
                Some(column) => shared.pins.insert(entry, column),
                None => shared.pins.remove(&entry),
            };
        }
        self.0.publisher.notify();
        crate::work::place(&mut item, column, now_unix_ms(), board.stale_after_days);
        Ok(item)
    }

    fn scan_worker(&self) {
        while let Some(job) = self.0.jobs.next() {
            let started = Instant::now();
            let result = super::service::attempt("Work board scan", || {
                scan::scan(scan::Input {
                    id: job.id,
                    repository: &job.repository,
                    previous: job.previous.as_deref(),
                    change: &job.change,
                    hints: &job.hints,
                    now_unix_ms: now_unix_ms(),
                    cancellation: &self.0.cancellation,
                })
            });
            tracing::debug!(
                repository = %job.repository.display(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                ok = result.is_ok(),
                "Work board scan"
            );
            self.publish(job, result);
        }
    }

    /// Makes a scan's result the project's, then forgets the pins of cards it
    /// proves gone once Git and the forge both answered.
    fn publish(&self, job: Job, result: Result<scan::Local, String>) {
        let mut shared = self.shared();
        // A project closed, or closed and reopened, while it was scanned.
        let Some(tracked) = shared
            .projects
            .get_mut(&job.repository)
            .filter(|tracked| tracked.id == job.id)
        else {
            return;
        };
        let first = tracked.local.is_none();
        tracked.scanning = false;
        tracked.scans = job.sequence;
        tracked.last_scan = Some(Instant::now());
        match result {
            Ok(local) => {
                tracked.scan_error = None;
                tracked.local = Some(Arc::new(local));
            }
            Err(error) => {
                tracing::debug!(%error, project = %tracked.name, "Work board scan failed");
                tracked.scan_error = Some(error);
                tracked
                    .pending
                    .get_or_insert_with(Default::default)
                    .merge(job.change);
            }
        }
        let settled = tracked.scan_error.is_none()
            && matches!(
                tracked.forge,
                Forge::Synced { error: None, .. } | Forge::NotGitHub
            );
        let tracked = &shared.projects[&job.repository];
        let stale = if settled {
            shared
                .pins
                .keys()
                .filter(|(project, item)| *project == job.repository && !tracked.live_pin(item))
                .map(|(_, item)| item.clone())
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        shared
            .pins
            .retain(|(project, item), _| *project != job.repository || !stale.contains(item));
        drop(shared);
        if !stale.is_empty() {
            self.store(|store| store.retain_pins(&job.repository, |item| !stale.contains(item)));
        }
        self.changed();
        if first {
            // The forge needs a checkout to resolve the repository from.
            self.0.forge.notify();
        }
        // New plans for the watcher, and changes that waited for this scan.
        self.0.scanner.notify();
    }

    fn scan_loop(&self) {
        let signal = Arc::downgrade(&self.0);
        let monitor = git_watch::Monitor::start(move || {
            if let Some(inner) = signal.upgrade() {
                inner.scanner.notify();
            }
        })
        .inspect_err(|error| {
            tracing::warn!(%error, "the Work board will poll every project instead of watching");
        })
        .ok();
        let mut scanner = Scanner {
            monitor,
            configured: BTreeMap::new(),
            stamped: HashMap::new(),
            sampled: None,
            refreshed: None,
        };
        while let Some(observed) = self.0.scanner.observe() {
            let Some(host) = self.0.host.upgrade() else {
                break;
            };
            let started = Instant::now();
            let delay =
                super::service::attempt("Work board scanning", || Ok(scanner.tick(self, &host)))
                    .unwrap_or_else(|error| {
                        tracing::warn!(%error, "Work board scanning is temporarily unavailable");
                        Duration::from_secs(5)
                    });
            drop(host);
            self.0.scanner.wait(observed, delay);
            // The watcher wakes this thread per event batch; a build writing
            // thousands of files costs one pass per floor, not one per batch.
            std::thread::sleep(TICK_FLOOR.saturating_sub(started.elapsed()));
        }
    }

    fn forge_loop(&self) {
        let mut tickets = tickets::Memory::default();
        while let Some(observed) = self.0.forge.observe() {
            let delay = super::service::attempt("Work board forge polling", || {
                Ok(self.forge_tick(&mut tickets))
            })
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "Work board forge polling is temporarily unavailable");
                Duration::from_secs(30)
            });
            self.0.forge.wait(observed, delay);
        }
    }

    fn forge_tick(&self, tickets: &mut tickets::Memory) -> Duration {
        let now = Instant::now();
        let due = {
            let mut shared = self.shared();
            shared
                .projects
                .iter_mut()
                .filter(|(_, tracked)| tracked.poll_requested || tracked.next_poll <= now)
                .filter_map(|(repository, tracked)| {
                    // The checkout `gh` resolves from comes from a scan.
                    let local = tracked.local.as_ref()?;
                    let target = forge::Target {
                        repository: repository.clone(),
                        root: local.root.clone().or_else(|| {
                            local
                                .worktrees
                                .first()
                                .map(|worktree| worktree.path.clone())
                        }),
                        checked_out: local
                            .worktrees
                            .iter()
                            .filter_map(|worktree| {
                                Some((
                                    worktree.path.clone(),
                                    worktree.branch.clone()?,
                                    worktree.head.as_ref()?.oid.clone(),
                                ))
                            })
                            .collect(),
                    };
                    // A request made while this poll runs wants the next one.
                    tracked.poll_requested = false;
                    tracked.polls_started += 1;
                    Some((
                        tracked.id,
                        target,
                        std::mem::take(&mut tracked.memory),
                        tracked.polls_started,
                    ))
                })
                .collect::<Vec<_>>()
        };
        for (id, target, mut memory, sequence) in due {
            if self.0.cancellation.is_cancelled() {
                break;
            }
            let outcome = forge::poll(&target, &mut memory, &self.0.cancellation);
            let synced_at = now_unix_ms();
            // A listing is written when it changes, not every poll; until the
            // first poll after a restart its age reads as when it last did.
            let changed = |snapshot: &forge::Snapshot| {
                self.shared()
                    .projects
                    .get(&target.repository)
                    .and_then(|tracked| tracked.snapshot.as_deref())
                    != Some(snapshot)
            };
            if let forge::Outcome::Synced(snapshot) = &outcome
                && changed(snapshot)
                && let Ok(serialized) = serde_json::to_string(snapshot)
            {
                self.store(|store| {
                    store.save_forge_snapshot(&target.repository, synced_at, &serialized)
                });
            }
            let mut shared = self.shared();
            let watched = shared.being_read();
            // A project closed, or closed and reopened, while it was polled.
            let Some(tracked) = shared
                .projects
                .get_mut(&target.repository)
                .filter(|tracked| tracked.id == id)
            else {
                continue;
            };
            tracked.memory = memory;
            tracked.polls = sequence;
            tracked.last_poll = Some(Instant::now());
            match outcome {
                forge::Outcome::Synced(snapshot) => {
                    tracked.failures = 0;
                    let hints = snapshot.hints();
                    tracked.forge = Forge::Synced {
                        at_unix_ms: synced_at,
                        error: None,
                    };
                    tracked.snapshot = Some(Arc::new(snapshot));
                    // New branches or merges need measuring before the cards
                    // they belong to are right.
                    if *tracked.hints != hints {
                        tracked.hints = Arc::new(hints);
                        tracked.pending.get_or_insert_with(Default::default);
                        self.0.scanner.notify();
                    }
                }
                forge::Outcome::NotGitHub => {
                    tracked.failures = 0;
                    tracked.forge = Forge::NotGitHub;
                    tracked.snapshot = None;
                }
                forge::Outcome::Failed(error) => {
                    tracked.failures = tracked.failures.saturating_add(1);
                    tracked.forge = match (&tracked.forge, &tracked.snapshot) {
                        (Forge::Synced { at_unix_ms, .. }, Some(_)) => Forge::Synced {
                            at_unix_ms: *at_unix_ms,
                            error: Some(error),
                        },
                        _ => Forge::Unavailable { reason: error },
                    };
                }
            }
            tracked.next_poll = Instant::now()
                + if tracked.forge == Forge::NotGitHub {
                    forge::NOT_GITHUB_INTERVAL
                } else {
                    forge::interval(watched, tracked.failures)
                };
            drop(shared);
            self.changed();
        }

        // Tickets are resolved only while someone reads the board, so the
        // daemon never reaches for Linear credentials unattended.
        let wanted = {
            let shared = self.shared();
            shared.being_read().then(|| {
                shared
                    .wanted_tickets()
                    .into_iter()
                    .filter(|reference| !shared.tickets.contains_key(&reference.id))
                    .collect::<BTreeSet<_>>()
            })
        };
        if let Some(wanted) = wanted {
            let resolved = tickets::resolve(&wanted, tickets, &self.0.cancellation);
            for (reference, ticket) in &resolved {
                self.store(|store| store.save_ticket(reference, ticket));
            }
            let unavailable = tickets.unavailable_for(&wanted);
            let mut shared = self.shared();
            let changed = !resolved.is_empty() || shared.linear_unavailable != unavailable;
            shared.tickets.extend(resolved);
            shared.linear_unavailable = unavailable;
            drop(shared);
            if changed {
                self.changed();
            }
        }

        let shared = self.shared();
        shared
            .projects
            .values()
            .map(|tracked| {
                if tracked.local.is_none() {
                    // Waits for the scan, which wakes this thread.
                    Duration::from_secs(60)
                } else {
                    tracked.next_poll.saturating_duration_since(Instant::now())
                }
            })
            .min()
            .unwrap_or(Duration::from_secs(60))
            .clamp(Duration::from_secs(1), Duration::from_secs(30 * 60))
    }
}

/// State only the scanner thread touches.
struct Scanner {
    monitor: Option<git_watch::Monitor>,
    configured: BTreeMap<ProjectId, Arc<git_watch::Project>>,
    /// When each worktree's agent activity was last written down.
    stamped: HashMap<PathBuf, Instant>,
    sampled: Option<Instant>,
    refreshed: Option<Instant>,
}

impl Scanner {
    fn tick(&mut self, handle: &Handle, host: &SharedHost) -> Duration {
        let now = Instant::now();
        if self
            .refreshed
            .is_none_or(|refreshed| refreshed.elapsed() >= MODEL_REFRESH)
        {
            self.refreshed = Some(now);
            self.refresh(handle, host);
        }
        self.drain(handle);
        if self
            .sampled
            .is_none_or(|sampled| sampled.elapsed() >= SESSION_SAMPLE)
        {
            self.sampled = Some(now);
            self.sample(handle, host);
        }
        self.dispatch(handle);
        self.configure(handle);
        self.next(handle)
    }

    /// Tracks the open Git projects, forgetting closed ones.
    fn refresh(&mut self, handle: &Handle, host: &SharedHost) {
        let projects = match open_projects(host) {
            Ok(projects) => projects,
            Err(error) => {
                tracing::debug!(%error, "the Work board could not read the open projects");
                return;
            }
        };
        let stale_after_days = stale_after_days();
        let mut shared = handle.shared();
        let unchanged = shared.stale_after_days == stale_after_days
            && shared.projects.len() == projects.len()
            && projects.iter().all(|(repository, name)| {
                shared
                    .projects
                    .get(repository)
                    .is_some_and(|tracked| tracked.name == *name)
            });
        shared.stale_after_days = stale_after_days;
        shared
            .projects
            .retain(|repository, _| projects.iter().any(|(open, _)| open == repository));
        for (repository, name) in &projects {
            shared.track(repository, name);
        }
        drop(shared);
        if !unchanged {
            handle.0.publisher.notify();
        }
    }

    fn drain(&mut self, handle: &Handle) {
        let Some(monitor) = &self.monitor else {
            return;
        };
        let changes = monitor.drain();
        if changes.is_empty() {
            return;
        }
        let mut shared = handle.shared();
        for tracked in shared.projects.values_mut() {
            let Some(change) = changes.get(&tracked.id) else {
                continue;
            };
            if let Some(covered) = change.coverage {
                tracked.covered = covered;
            }
            if let Some(error) = &change.error {
                tracing::debug!(%error, project = %tracked.name, "the Work board polls this project");
            }
            if !change.worktrees.is_empty()
                || change.references
                || change.topology
                || change.membership
            {
                let mut change = change.clone();
                change.coverage = None;
                change.error = None;
                tracked
                    .pending
                    .get_or_insert_with(Default::default)
                    .merge(change);
            }
        }
    }

    /// Notes which worktrees agents are working in now. The stamp outlives
    /// the session, so a card keeps its place after the agent finishes.
    fn sample(&mut self, handle: &Handle, host: &SharedHost) {
        let sessions = live_sessions(host);
        let now = now_unix_ms();
        let mut shared = handle.shared();
        let worktrees = shared
            .projects
            .values()
            .filter_map(|tracked| tracked.local.as_ref())
            .flat_map(|local| local.worktrees.iter().map(|worktree| worktree.path.clone()))
            .collect::<Vec<_>>();
        let mut stamps = Vec::new();
        for session in sessions.iter().filter(|session| {
            matches!(
                session.state,
                ActivityState::Working | ActivityState::Waiting
            )
        }) {
            let Some(worktree) = worktrees
                .iter()
                .filter(|worktree| session.directory.starts_with(worktree))
                .max_by_key(|worktree| worktree.components().count())
            else {
                continue;
            };
            shared.session_activity.insert(worktree.clone(), now);
            if self
                .stamped
                .get(worktree)
                .is_none_or(|stamped| stamped.elapsed() >= SESSION_STAMP)
            {
                self.stamped.insert(worktree.clone(), Instant::now());
                stamps.push((worktree.clone(), now));
            }
        }
        let horizon =
            now.saturating_sub(u64::from(crate::work::MAX_STALE_AFTER_DAYS) * crate::work::DAY_MS);
        shared.session_activity.retain(|_, at| *at >= horizon);
        drop(shared);
        self.stamped
            .retain(|_, stamped| stamped.elapsed() < SESSION_STAMP);
        if !stamps.is_empty() {
            handle.store(|store| store.record_session_activity(&stamps, horizon));
        }
    }

    /// Hands due scans to the workers. A project already scanning keeps its
    /// changes pending for the next one.
    fn dispatch(&mut self, handle: &Handle) {
        let jobs = {
            let mut shared = handle.shared();
            let polls = shared
                .projects
                .iter()
                .filter(|(_, tracked)| {
                    tracked
                        .last_scan
                        .is_some_and(|last| last.elapsed() >= shared.poll_interval(tracked))
                })
                .map(|(repository, _)| repository.clone())
                .collect::<Vec<_>>();
            for repository in polls {
                let tracked = shared.projects.get_mut(&repository).expect("listed above");
                let worktrees = tracked
                    .local
                    .iter()
                    .flat_map(|local| local.worktrees.iter().map(|worktree| worktree.id))
                    .collect();
                tracked
                    .pending
                    .get_or_insert_with(Default::default)
                    .merge(git_watch::Change {
                        worktrees,
                        ..git_watch::Change::default()
                    });
            }
            shared
                .projects
                .iter_mut()
                .filter(|(_, tracked)| {
                    !tracked.scanning
                        && tracked.pending.is_some()
                        && (tracked.urgent
                            || tracked
                                .last_scan
                                .is_none_or(|last| last.elapsed() >= tracked.spacing()))
                })
                .map(|(repository, tracked)| {
                    tracked.scanning = true;
                    tracked.urgent = false;
                    tracked.scans_started += 1;
                    Job {
                        repository: repository.clone(),
                        id: tracked.id,
                        previous: tracked.local.clone(),
                        change: tracked.pending.take().unwrap_or_default(),
                        hints: tracked.hints.clone(),
                        sequence: tracked.scans_started,
                    }
                })
                .collect::<Vec<_>>()
        };
        handle.0.jobs.push(jobs);
    }

    /// Hands the watcher every project's current plan.
    fn configure(&mut self, handle: &Handle) {
        let Some(monitor) = &self.monitor else {
            return;
        };
        let plans = handle
            .shared()
            .projects
            .values()
            .filter_map(|tracked| {
                let plan = tracked.local.as_ref()?.plan.clone()?;
                Some((tracked.id, plan))
            })
            .collect::<BTreeMap<_, _>>();
        let unchanged = plans.len() == self.configured.len()
            && plans.iter().all(|(id, plan)| {
                self.configured
                    .get(id)
                    .is_some_and(|configured| Arc::ptr_eq(configured, plan))
            });
        if !unchanged {
            monitor.configure(plans.values().cloned().collect());
            self.configured = plans;
        }
    }

    /// How long the scanner may sleep before something is due.
    fn next(&self, handle: &Handle) -> Duration {
        let shared = handle.shared();
        let mut next = SESSION_SAMPLE.saturating_sub(
            self.sampled
                .map_or(Duration::ZERO, |sampled| sampled.elapsed()),
        );
        next = next.min(
            MODEL_REFRESH.saturating_sub(
                self.refreshed
                    .map_or(Duration::ZERO, |refreshed| refreshed.elapsed()),
            ),
        );
        for tracked in shared.projects.values() {
            let since = tracked
                .last_scan
                .map_or(Duration::MAX, |last| last.elapsed());
            // A scanning project's worker wakes this thread when it is done.
            if tracked.pending.is_some() && !tracked.scanning {
                next = next.min(tracked.spacing().saturating_sub(since));
            }
            if !tracked.scanning {
                next = next.min(shared.poll_interval(tracked).saturating_sub(since));
            }
        }
        next.max(Duration::from_millis(100))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_keep_a_notification_sent_before_the_wait() {
        let signal = Signal::default();
        let observed = signal.observe().unwrap();
        signal.notify();
        let started = Instant::now();
        signal.wait(observed, Duration::from_secs(10));
        assert!(started.elapsed() < Duration::from_secs(5));
        signal.stop();
        assert_eq!(signal.observe(), None);
    }

    #[test]
    fn watchers_keep_only_the_latest_pending_board_revision() {
        let host = super::super::service::test_host();
        let board = |generated_at_unix_ms, stale_after_days| Board {
            generated_at_unix_ms,
            stale_after_days,
            projects: Vec::new(),
            items: Vec::new(),
            linear_unavailable: None,
        };
        // Everything a queue was sent; `pop` blocks while one is open.
        let received = |delivery: &DeliveryQueue| {
            delivery.close_after_drain();
            std::iter::from_fn(|| delivery.pop())
                .map(|outbound| match outbound.message {
                    ServerMessage::WorkBoard { revision, board } => {
                        (revision, board.stale_after_days)
                    }
                    message => panic!("unexpected {message:?}"),
                })
                .collect::<Vec<_>>()
        };
        let first = Arc::new(DeliveryQueue::new(1, 16));
        let second = Arc::new(DeliveryQueue::new(2, 16));
        let gone = Arc::new(DeliveryQueue::new(3, 16));
        let mut publication = Publication::default();
        publication.watchers.insert(1, Arc::downgrade(&first));
        publication.watchers.insert(2, Arc::downgrade(&second));
        publication.watchers.insert(3, Arc::downgrade(&gone));
        drop(gone);

        publication.offer(&host, &board(1, 7), None);
        assert!(!publication.watchers.contains_key(&3));
        // Only the clock moved: nothing to send.
        publication.offer(&host, &board(2, 7), None);
        // A subscriber's own board is its answer, not a push.
        publication.offer(&host, &board(3, 14), Some(1));
        assert_eq!(publication.revision, 2);
        assert_eq!(received(&first), [(1, 7)]);
        assert_eq!(received(&second), [(2, 14)]);
    }

    #[test]
    fn pins_outlive_their_card_only_until_git_and_the_forge_disagree() {
        let mut tracked = Tracked::new("repo".into());
        assert!(tracked.live_pin("branch:gone"), "nothing is known yet");
        let mut local = scan::Local::default();
        local.worktrees.push(scan::Worktree {
            id: crate::model::WorktreeId::new(),
            path: "/repo".into(),
            primary: true,
            locked: false,
            created_at_unix_ms: None,
            branch: Some("checked-out".into()),
            head: None,
            comparison: None,
            dirty: None,
            link: None,
            error: None,
        });
        // A branch between worktrees keeps its pin until its pull request
        // finishes.
        local
            .branch_names
            .extend(["checked-out", "open", "draft", "merged", "switched-away"].map(String::from));
        tracked.local = Some(Arc::new(local));
        let pull_request = |branch: &str, state: &str, draft: bool| {
            serde_json::from_value(serde_json::json!({
                "number": 1,
                "title": branch,
                "state": state,
                "isDraft": draft,
                "headRefName": branch,
                "headRefOid": "oid",
                "baseRefName": "main",
                "updatedAt": "2026-10-01T00:00:00Z",
                "url": "https://github.com/me/repo/pull/1",
            }))
            .unwrap()
        };
        tracked.snapshot = Some(Arc::new(forge::Snapshot {
            repository: crate::github::GitHubRepoKey::new("github.com", None, "me", "repo")
                .unwrap(),
            remotes: Vec::new(),
            pull_requests: vec![
                pull_request("open", "OPEN", false),
                pull_request("draft", "CLOSED", true),
                pull_request("merged", "MERGED", false),
            ],
            others: BTreeMap::new(),
        }));
        assert!(tracked.live_pin("branch:checked-out"));
        assert!(tracked.live_pin("branch:open"));
        assert!(tracked.live_pin("branch:draft"));
        assert!(!tracked.live_pin("branch:merged"));
        assert!(tracked.live_pin("branch:switched-away"));
        assert!(!tracked.live_pin("branch:gone"));
        assert!(tracked.live_pin("worktree:/repo"));
        assert!(!tracked.live_pin("worktree:/repo/.worktrees/gone"));
        assert!(tracked.live_pin("unknown-layout"));
    }
}
