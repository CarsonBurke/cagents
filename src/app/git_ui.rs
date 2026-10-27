//! Local Git invalidation has its own queue and generations. GitHub reads never
//! replace local rows, and filesystem events never schedule a GitHub process.
use super::*;
use crate::{git_watch, model::WorktreeId, repository::HeadState};
use std::collections::{BTreeMap, BTreeSet};

fn local_workers() -> usize {
    std::thread::available_parallelism().map_or(2, |count| count.get().clamp(2, 8))
}
const FALLBACK_INTERVAL: Duration = Duration::from_secs(60);

/// Both UI read streams allocate from one process-local order at worker start.
/// No wall-clock ordering or serialized identity is involved.
pub(super) fn next_status_read() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    repository: RepositoryInfo,
    worktrees: Vec<(WorktreeRecord, Option<StatusSnapshot>)>,
    heads: HashMap<WorktreeId, GitHubRepoKey>,

    status_errors: HashMap<WorktreeId, String>,
    /// Only these rows were actually attempted by this read. Retained sibling
    /// snapshots cannot supersede a newer successful bound Changes read.
    status_updates: HashSet<WorktreeId>,
    status_reads: HashMap<WorktreeId, u64>,
}

/// Initial/reload reads participate in the same per-row order as both live
/// streams. The listing timestamp is only authority for omitted rows.
#[derive(Debug, Clone, Default)]
pub(super) struct LoadedStatusOrder {
    listing: u64,
    pub(super) reads: HashMap<WorktreeId, u64>,
    pub(super) errors: HashMap<WorktreeId, String>,
}
impl LoadedStatusOrder {
    pub(super) fn new() -> Self {
        Self {
            listing: next_status_read(),
            ..Self::default()
        }
    }
}

#[derive(Clone)]
struct BoundLoadUpdate {
    path: PathBuf,
    status: Option<StatusSnapshot>,
    head: Option<GitHubRepoKey>,
    error: Option<String>,
    read: u64,
}
#[derive(Default)]
struct ReloadStatus {
    previous: Option<Snapshot>,
    // Only an already-bound row can supply these proofs; at most one scalar
    // update per worktree in this live project load. No path listings retained.
    bound: HashMap<WorktreeId, BoundLoadUpdate>,
}

/// Sidebar snapshots retain scalar row facts, never the Changes panel's file
/// listing. Build the projection directly so the UI does not first clone a
/// potentially large entries vector merely to discard it.
fn display_worktrees(
    worktrees: &[(WorktreeRecord, Option<StatusSnapshot>)],
) -> Vec<(WorktreeRecord, Option<StatusSnapshot>)> {
    worktrees
        .iter()
        .map(|(record, status)| {
            (
                record.clone(),
                status.as_ref().map(StatusSnapshot::display_summary),
            )
        })
        .collect()
}

impl Snapshot {
    /// Independent read streams order row authority by read start, rather than
    /// callback delivery. A delayed result cannot overwrite newer accepted facts.
    fn preserve_newer_status(&mut self, current: &Self) -> HashSet<WorktreeId> {
        let mut preserved = HashSet::new();
        let current_rows = current
            .worktrees
            .iter()
            .map(|row| (row.0.id, row))
            .collect::<HashMap<_, _>>();
        let incoming_ids = self
            .worktrees
            .iter()
            .map(|row| row.0.id)
            .collect::<HashSet<_>>();
        self.worktrees.retain_mut(|row| {
            let id = row.0.id;
            let Some(&latest_at) = current.status_reads.get(&id) else {
                return true;
            };
            if self
                .status_reads
                .get(&id)
                .is_some_and(|captured| *captured >= latest_at)
            {
                return true;
            }
            preserved.insert(id);
            self.status_updates.remove(&id);
            self.status_reads.insert(id, latest_at);
            if let Some(error) = current.status_errors.get(&id) {
                self.status_errors.insert(id, error.clone());
            } else {
                self.status_errors.remove(&id);
            }
            if let Some(head) = current.heads.get(&id) {
                self.heads.insert(id, head.clone());
            } else {
                self.heads.remove(&id);
            }
            if let Some(latest) = current_rows.get(&id) {
                *row = (**latest).clone();
                true
            } else {
                self.status_errors.remove(&id);
                self.heads.remove(&id);
                false
            }
        });
        // An omission is an attempted removal, not authority over a row whose
        // newer bound read already proved it present again.
        for row in &current.worktrees {
            let id = row.0.id;
            // A later failure retains last-known display data; it does not
            // prove that an omitted checkout is still present.
            if current.status_errors.contains_key(&id) {
                continue;
            }
            if incoming_ids.contains(&id) {
                continue;
            }
            let Some(&latest) = current.status_reads.get(&id) else {
                continue;
            };
            if self
                .status_reads
                .get(&id)
                .is_some_and(|removed| *removed >= latest)
            {
                continue;
            }
            self.worktrees.push(row.clone());
            preserved.insert(id);
            self.status_updates.remove(&id);
            self.status_reads.insert(id, latest);
            if let Some(error) = current.status_errors.get(&id) {
                self.status_errors.insert(id, error.clone());
            } else {
                self.status_errors.remove(&id);
            }
            if let Some(head) = current.heads.get(&id) {
                self.heads.insert(id, head.clone());
            } else {
                self.heads.remove(&id);
            }
        }
        preserved
    }
}

/// A stale listing can also carry stale watch coverage. Keep previously planned
/// coverage for rows whose newer status won, without doing filesystem work on
/// the UI thread. Only missing coverage needs one coalesced worker replan.
fn reconcile_watch_plan(
    plan: &mut git_watch::Project,
    previous: Option<&git_watch::Project>,
    snapshot: &Snapshot,
    preserved: &HashSet<WorktreeId>,
) -> BTreeSet<WorktreeId> {
    let upstreams = snapshot
        .worktrees
        .iter()
        .filter_map(|(record, status)| {
            let upstream = match &status.as_ref()?.tracking {
                crate::repository::TrackingState::Compared { upstream, .. } => Some(upstream),
                crate::repository::TrackingState::Unknown { upstream } => upstream.as_ref(),
                crate::repository::TrackingState::NoUpstream => None,
            }?;
            Some((record.id, upstream.clone()))
        })
        .collect();
    plan.reconcile_coverage(
        previous,
        snapshot.worktrees.iter().map(|(record, _)| record),
        &preserved.iter().copied().collect(),
        &upstreams,
    )
}

struct Entry {
    snapshot: Snapshot,
    generation: u64,
    plan: Option<Arc<git_watch::Project>>,
    archived: Vec<crate::github::PullRequest>,
    remote_started: bool,
    cancellation: Option<CancellationToken>,
    pending: git_watch::Change,
    fallback: Option<String>,
    read_failure: Option<String>,
    read_full_scan: bool,
    /// A baseline was offered to this window's SessionRuntime. This is not a
    /// save acknowledgement: retry/quarantine remain owned by the writer.
    catalog_requested: bool,
    /// When the worktree list was last re-read. Polling is paced from this
    /// rather than from any read, since the edits a partial watch still
    /// reports would otherwise postpone the scan it stands in for.
    last_full_scan: Instant,
    /// Failed siblings have independent retry pacing; attempts for one row
    /// cannot postpone another row's recovery or force a topology rescan.
    failed_status_attempts: HashMap<WorktreeId, Instant>,
}

/// The window's watcher and the channel its wakes arrive on.
struct Watch {
    monitor: git_watch::Monitor,
    receiver: async_channel::Receiver<()>,
}

impl Watch {
    fn start() -> std::io::Result<Self> {
        let (wake, receiver) = async_channel::bounded(1);
        let monitor = git_watch::Monitor::start(move || {
            let _ = wake.try_send(());
        })?;
        Ok(Self { monitor, receiver })
    }
}

impl std::ops::Deref for Watch {
    type Target = git_watch::Monitor;
    fn deref(&self) -> &Self::Target {
        &self.monitor
    }
}

#[derive(Default)]
pub(super) struct State {
    watch: Option<Watch>,
    entries: HashMap<ProjectId, Entry>,
    queue: VecDeque<ProjectId>,
    activity: Vec<ProjectId>,
    reloads: HashMap<ProjectId, ReloadStatus>,
}

impl State {
    /// Stop the old watcher worker, retaining its row frontier until this
    /// project's generation-checked loader publishes or the project closes.
    pub(super) fn begin_repository_load(&mut self, project: ProjectId) {
        let retained = self.reloads.remove(&project).unwrap_or_default();
        let previous = self
            .entries
            .get(&project)
            .map(|entry| entry.snapshot.clone())
            .or(retained.previous);
        self.forget(project);
        self.reloads.insert(
            project,
            ReloadStatus {
                previous,
                bound: retained.bound,
            },
        );
    }

    fn accept_during_load(&mut self, key: &ActiveWorktreeKey, update: BoundLoadUpdate) -> bool {
        let Some(reload) = self.reloads.get_mut(&key.project_id) else {
            return true;
        };
        if let Some(previous) = &reload.previous {
            if !previous.worktrees.iter().any(|(record, _)| {
                record.id == key.worktree_id && record.path == key.canonical_path
            }) || previous
                .status_reads
                .get(&key.worktree_id)
                .is_some_and(|current| *current > update.read)
            {
                return false;
            }
        }
        if reload
            .bound
            .get(&key.worktree_id)
            .is_some_and(|current| current.read > update.read)
        {
            return false;
        }
        // A failure retains the preceding last-known values; its read order
        // still supersedes an older loader success.
        let mut update = update;
        if update.error.is_some() {
            if let Some(previous) = reload.bound.get(&key.worktree_id) {
                update.status = previous.status.clone();
                update.head = previous.head.clone();
            } else if let Some(previous) = &reload.previous {
                update.status = previous
                    .worktrees
                    .iter()
                    .find(|(record, _)| record.id == key.worktree_id)
                    .and_then(|(_, status)| status.clone());
                update.head = previous.heads.get(&key.worktree_id).cloned();
            }
        }
        reload.bound.insert(key.worktree_id, update);
        true
    }

    /// Reconcile before the App creates visible rows, persists records or
    /// installs the new watcher entry. All three see exactly the same facts.
    pub(super) fn reconcile_repository_load(&mut self, loaded: &mut LoadedRepository) {
        let Some(repository) = &loaded.repository else {
            self.reloads.remove(&loaded.project.id);
            return;
        };
        let mut snapshot = Snapshot {
            repository: repository.clone(),
            worktrees: display_worktrees(&loaded.worktrees),
            heads: loaded.head_repositories.clone(),
            status_errors: loaded.status_order.errors.clone(),
            status_reads: loaded.status_order.reads.clone(),
            status_updates: loaded
                .worktrees
                .iter()
                .map(|(record, _)| record.id)
                .collect(),
        };
        if let Some(reload) = self.reloads.remove(&loaded.project.id).or_else(|| {
            self.entries
                .get(&loaded.project.id)
                .map(|entry| ReloadStatus {
                    previous: Some(entry.snapshot.clone()),
                    bound: HashMap::new(),
                })
        }) {
            if reload.previous.as_ref().is_some_and(|previous| {
                previous.repository.id != repository.id
                    || previous.repository.common_dir != repository.common_dir
            }) {
                return;
            }
            let previous = reload.previous;
            if let Some(previous) = &previous {
                for (record, status) in &mut snapshot.worktrees {
                    if status.is_none() && snapshot.status_errors.contains_key(&record.id) {
                        *status = previous
                            .worktrees
                            .iter()
                            .find(|(old, _)| old.id == record.id && old.path == record.path)
                            .and_then(|(_, status)| status.clone());
                    }
                }
                for (record, _) in &previous.worktrees {
                    snapshot
                        .status_reads
                        .entry(record.id)
                        .or_insert(loaded.status_order.listing);
                }
                snapshot.preserve_newer_status(previous);
                // Status authority covers head/counts/tracking, not the
                // loader's independently sampled lock or inventory metadata.
                for (record, _) in &mut snapshot.worktrees {
                    if let Some((listed, _)) = loaded
                        .worktrees
                        .iter()
                        .find(|(listed, _)| listed.id == record.id && listed.path == record.path)
                    {
                        record.lock = listed.lock.clone();
                        record.prunable_reason = listed.prunable_reason.clone();
                        record.created_at_unix_ms = listed.created_at_unix_ms;
                        record.bare = listed.bare;
                    }
                }
            }
            for (id, update) in reload.bound {
                if snapshot
                    .status_reads
                    .get(&id)
                    .is_some_and(|read| *read > update.read)
                {
                    // A newer failed attempt owns the unavailable marker, but
                    // an earlier accepted success is still useful last-known
                    // display data, including during initial Entry absence.
                    if snapshot.status_errors.contains_key(&id)
                        && let Some((_, status)) = snapshot
                            .worktrees
                            .iter_mut()
                            .find(|(record, _)| record.id == id && record.path == update.path)
                        && status.is_none()
                    {
                        *status = update.status;
                    }
                    continue;
                }
                // A successful bound read also proves a previously listed row
                // survived an older omission. Preserve its original record.
                if update.error.is_none()
                    && !snapshot.worktrees.iter().any(|(record, _)| record.id == id)
                    && let Some(row) = previous.as_ref().and_then(|previous| {
                        previous
                            .worktrees
                            .iter()
                            .find(|(record, _)| record.id == id && record.path == update.path)
                    })
                {
                    snapshot.worktrees.push(row.clone());
                }
                let Some((record, status)) = snapshot
                    .worktrees
                    .iter_mut()
                    .find(|(record, _)| record.id == id && record.path == update.path)
                else {
                    continue;
                };
                if let Some(current) = &update.status {
                    *status = Some(current.clone());
                    apply_status_head(record, current);
                }
                if let Some(error) = update.error {
                    snapshot.status_errors.insert(id, error);
                } else {
                    snapshot.status_errors.remove(&id);
                }
                if let Some(head) = update.head {
                    snapshot.heads.insert(id, head);
                } else {
                    snapshot.heads.remove(&id);
                }
                snapshot.status_reads.insert(id, update.read);
            }
        }
        loaded.worktrees = snapshot.worktrees;
        loaded.head_repositories = snapshot.heads;
        loaded.status_order.reads = snapshot.status_reads;
        loaded.status_order.errors = snapshot.status_errors;
    }

    pub(super) fn remember_archived(
        &mut self,
        project: ProjectId,
        generation: u64,
        archived: &[crate::github::PullRequest],
    ) {
        if let Some(entry) = self
            .entries
            .get_mut(&project)
            .filter(|entry| entry.generation == generation && !entry.remote_started)
        {
            entry.archived = archived.to_vec();
        }
    }
    /// A bound full read shares its row freshness with watcher reads. Keep only
    /// the small status projection here, not the Changes panel's path entries.
    pub(super) fn accept_bound_status(
        &mut self,
        key: &ActiveWorktreeKey,
        status: &StatusSnapshot,
        head_repository: Option<&GitHubRepoKey>,
        read: u64,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(&key.project_id) else {
            return self.accept_during_load(
                key,
                BoundLoadUpdate {
                    path: key.canonical_path.clone(),
                    status: Some(status.display_summary()),
                    head: head_repository.cloned(),
                    error: None,
                    read,
                },
            );
        };
        if entry
            .snapshot
            .status_reads
            .get(&key.worktree_id)
            .is_some_and(|current| *current > read)
        {
            return false;
        }
        let Some((record, cached)) =
            entry.snapshot.worktrees.iter_mut().find(|(record, _)| {
                record.id == key.worktree_id && record.path == key.canonical_path
            })
        else {
            return false;
        };
        *cached = Some(status.display_summary());
        apply_status_head(record, status);
        let upstream = match &status.tracking {
            crate::repository::TrackingState::Compared { upstream, .. } => Some(upstream.as_str()),
            crate::repository::TrackingState::Unknown { upstream } => upstream.as_deref(),
            crate::repository::TrackingState::NoUpstream => None,
        };
        // The worker selected this fork from the same successful snapshot's
        // configuration. Authoritative None also revokes an old attachment.
        if let Some(repository) = head_repository {
            entry
                .snapshot
                .heads
                .insert(key.worktree_id, repository.clone());
        } else {
            entry.snapshot.heads.remove(&key.worktree_id);
        }
        let routed = entry
            .plan
            .as_ref()
            .and_then(|plan| plan.refresh_worktree_routing(record, upstream));
        let reconfigure = routed.is_some();
        if let Some(plan) = routed {
            entry.plan = Some(Arc::new(plan));
        }
        entry.snapshot.status_errors.remove(&key.worktree_id);
        entry.failed_status_attempts.remove(&key.worktree_id);
        entry.snapshot.status_reads.insert(key.worktree_id, read);
        if reconfigure {
            self.configure();
        }
        true
    }

    /// Actual failed status attempts share the same row order as successful
    /// bound/watch proofs. A later successful sibling/read must not be undone.
    pub(super) fn accept_bound_status_error(
        &mut self,
        key: &ActiveWorktreeKey,
        error: &str,
        read: u64,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(&key.project_id) else {
            return self.accept_during_load(
                key,
                BoundLoadUpdate {
                    path: key.canonical_path.clone(),
                    status: None,
                    head: None,
                    error: Some(error.to_owned()),
                    read,
                },
            );
        };
        if entry
            .snapshot
            .status_reads
            .get(&key.worktree_id)
            .is_some_and(|current| *current > read)
            || !entry.snapshot.worktrees.iter().any(|(record, _)| {
                record.id == key.worktree_id && record.path == key.canonical_path
            })
        {
            return false;
        }
        entry.snapshot.status_reads.insert(key.worktree_id, read);
        entry
            .snapshot
            .status_errors
            .insert(key.worktree_id, error.to_owned());
        entry
            .failed_status_attempts
            .insert(key.worktree_id, Instant::now());
        true
    }

    pub(super) fn cached_head_repository(&self, key: &ActiveWorktreeKey) -> Option<GitHubRepoKey> {
        self.entries
            .get(&key.project_id)?
            .snapshot
            .heads
            .get(&key.worktree_id)
            .cloned()
    }

    pub fn forget(&mut self, project: ProjectId) {
        self.reloads.remove(&project);
        if let Some(entry) = self.entries.remove(&project)
            && let Some(cancel) = entry.cancellation
        {
            cancel.cancel();
        }
        self.queue.retain(|id| *id != project);
        self.configure();
    }

    /// Take a worktree the application deleted out of the last listing, so its
    /// absence from the next one is not read as a removal made elsewhere.
    pub(super) fn forget_worktree(&mut self, project: ProjectId, worktree: WorktreeId) {
        if let Some(entry) = self.entries.get_mut(&project) {
            entry
                .snapshot
                .worktrees
                .retain(|(record, _)| record.id != worktree);
            entry.snapshot.status_errors.remove(&worktree);
            entry.failed_status_attempts.remove(&worktree);
            entry.snapshot.status_updates.remove(&worktree);
            entry
                .snapshot
                .status_reads
                .insert(worktree, next_status_read());
        }
    }

    fn configure(&self) {
        if let Some(watch) = &self.watch {
            watch.monitor.configure(
                self.entries
                    .values()
                    .filter_map(|entry| entry.plan.clone())
                    .collect(),
            );
        }
    }

    fn changed(&mut self, project: ProjectId, change: git_watch::Change) {
        let Some(entry) = self.entries.get_mut(&project) else {
            return;
        };
        if change.coverage == Some(true) {
            entry.fallback = None;
        }
        if let Some(error) = &change.error {
            entry.fallback = Some(error.clone());
        }
        if change.worktrees.is_empty() && !change.topology && !change.membership {
            return;
        }
        entry.pending.merge(change);
        if !self.queue.contains(&project) {
            self.queue.push_back(project);
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        for entry in self.entries.values() {
            if let Some(cancel) = &entry.cancellation {
                cancel.cancel();
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Remote {
        project: ProjectId,
        generation: u64,
        result: Result<(GitHubAvailability, Vec<crate::github::PullRequest>), String>,
    },
    Changed,
    Fallback,
    Read {
        project: ProjectId,
        generation: u64,
        result: Result<(Snapshot, Option<git_watch::Project>), String>,
    },
}

fn panel_invalidation(
    project: ProjectId,
    change: &git_watch::Change,
    key: &ActiveWorktreeKey,
) -> Option<u8> {
    (key.project_id == project
        && (change.topology
            || change.shared_snapshot
            || change.worktrees.contains(&key.worktree_id)))
    .then_some(if change.metadata || change.membership {
        3
    } else {
        1
    })
}

impl Cagents {
    pub(super) fn observe_git_repository(
        &mut self,
        loaded: &LoadedRepository,
        generation: u64,
    ) -> Task<Message> {
        let Some(repository) = &loaded.repository else {
            return Task::none();
        };
        let project = loaded.project.id;
        let state = &mut self.refresh_ui.git;
        let old = state.entries.remove(&project);
        let records = loaded
            .worktrees
            .iter()
            .map(|(record, _)| record.clone())
            .collect::<Vec<_>>();
        let reusable_plan = old
            .as_ref()
            .and_then(|entry| entry.plan.clone())
            .filter(|plan| plan.matches(&repository.common_dir, &records));
        if let Some(old) = old {
            if let Some(cancel) = &old.cancellation {
                cancel.cancel();
            }
            // A reload replaces the listing wholesale, so what it dropped is
            // settled here rather than by the read that follows it.
            if let Some(session) = &self.session {
                session.save_worktrees(records_removed_outside(
                    project,
                    &old.snapshot,
                    &loaded.worktrees,
                ));
            }
        }
        let failure = if state.watch.is_none() {
            match Watch::start() {
                Ok(watch) => {
                    state.watch = Some(watch);
                    None
                }
                Err(error) => Some(error.to_string()),
            }
        } else {
            None
        };
        state.entries.insert(
            project,
            Entry {
                snapshot: Snapshot {
                    repository: repository.clone(),
                    worktrees: display_worktrees(&loaded.worktrees),
                    heads: loaded.head_repositories.clone(),

                    status_errors: loaded.status_order.errors.clone(),
                    status_updates: HashSet::new(),
                    status_reads: loaded.status_order.reads.clone(),
                },
                generation,
                plan: reusable_plan.clone(),
                archived: Vec::new(),
                remote_started: false,
                cancellation: None,
                read_failure: None,
                read_full_scan: false,
                catalog_requested: false,
                pending: git_watch::Change::default(),
                fallback: failure,
                last_full_scan: Instant::now(),
                failed_status_attempts: loaded
                    .status_order
                    .errors
                    .keys()
                    .map(|id| (*id, Instant::now()))
                    .collect(),
            },
        );
        state.changed(
            project,
            git_watch::Change {
                membership: reusable_plan.is_none(),
                ..Default::default()
            },
        );
        self.refresh_ui.git.configure();
        self.start_git_reads()
    }

    pub(super) fn git_subscription(&self) -> Subscription<Message> {
        let events = self
            .refresh_ui
            .git
            .watch
            .as_ref()
            .map_or_else(Subscription::none, |watch| {
                Subscription::run_with(Source(watch.receiver.clone()), stream)
                    .map(|()| Message::GitUi(Action::Changed))
            });
        Subscription::batch([
            events,
            iced::time::every(FALLBACK_INTERVAL).map(|_| Message::GitUi(Action::Fallback)),
        ])
    }

    pub(super) fn git_fallback_tick(&mut self) -> Task<Message> {
        if self.refresh_ui.git.watch.is_none()
            && !self.refresh_ui.git.entries.is_empty()
            && let Ok(watch) = Watch::start()
        {
            self.refresh_ui.git.watch = Some(watch);
            self.refresh_ui.git.configure();
        }
        let open = self
            .sidebar_model
            .projects
            .iter()
            .map(|project| project.id)
            .collect::<HashSet<_>>();
        let removed = self
            .refresh_ui
            .git
            .entries
            .keys()
            .filter(|id| !open.contains(id))
            .copied()
            .collect::<Vec<_>>();
        for project in removed {
            self.refresh_ui.git.forget(project);
        }
        let due = self
            .refresh_ui
            .git
            .entries
            .iter()
            .filter_map(|(&project, entry)| {
                if (entry.fallback.is_some() || entry.read_failure.is_some())
                    && entry.last_full_scan.elapsed() >= FALLBACK_INTERVAL
                {
                    return Some((
                        project,
                        git_watch::Change {
                            worktrees: entry
                                .snapshot
                                .worktrees
                                .iter()
                                .map(|(record, _)| record.id)
                                .collect(),
                            topology: true,
                            membership: true,
                            ..Default::default()
                        },
                    ));
                }
                let failed = entry
                    .snapshot
                    .status_errors
                    .keys()
                    .copied()
                    .filter(|id| {
                        entry
                            .failed_status_attempts
                            .get(id)
                            .is_none_or(|last| last.elapsed() >= FALLBACK_INTERVAL)
                    })
                    .collect::<BTreeSet<_>>();
                if !failed.is_empty() {
                    return Some((
                        project,
                        git_watch::Change {
                            worktrees: failed,
                            untracked: true,
                            ..Default::default()
                        },
                    ));
                }
                None
            })
            .collect::<Vec<_>>();
        for (project, change) in due {
            self.refresh_ui.git.changed(project, change);
        }
        let mut tasks = vec![self.start_git_reads()];
        if self.refresh_ui.source_last_refresh.elapsed() >= FALLBACK_INTERVAL
            && self.refresh_ui.source_cancel.is_none()
            && let Some(key) = self
                .sidebar_model
                .source_control
                .as_ref()
                .filter(|source| {
                    !source.busy()
                        && !self
                            .source_control_mutations
                            .contains_key(&source.key.canonical_path)
                })
                .map(|source| source.key.clone())
        {
            tasks.push(self.reload_source_control(&key));
        }
        Task::batch(tasks)
    }

    pub(super) fn update_git(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Fallback => return self.git_fallback_tick(),
            Action::Remote {
                project,
                generation,
                result,
            } => {
                if self.repository_generations.get(&project) != Some(&generation) {
                    return Task::none();
                }
                self.refresh_ui.projects.remove(&project);
                let (availability, archived) = result
                    .unwrap_or_else(|detail| (GitHubAvailability::Failed { detail }, Vec::new()));
                self.refresh_ui.finished(
                    project,
                    !matches!(availability, GitHubAvailability::Failed { .. }),
                );
                if matches!(availability, GitHubAvailability::Ready { .. })
                    && let Some(entry) = self.refresh_ui.git.entries.get_mut(&project)
                {
                    entry.archived = archived.clone();
                }
                let Some(row) = self
                    .sidebar_model
                    .projects
                    .iter_mut()
                    .find(|row| row.id == project)
                else {
                    return Task::none();
                };
                if let GitHubAvailability::Ready { pull_requests, .. } = &availability {
                    row.replace_pull_requests(
                        pull_requests
                            .iter()
                            .map(|(pr, checks)| {
                                SidebarPullRequest::from_github(project, pr, checks)
                            })
                            .collect(),
                    );
                    if let Some(entry) = self.refresh_ui.git.entries.get(&project) {
                        apply_attached_worktree_prs(
                            row,
                            &entry.snapshot.worktrees,
                            &pull_requests
                                .iter()
                                .map(|(pr, _)| pr.clone())
                                .collect::<Vec<_>>(),
                            &archived,
                        );
                    }
                }
                row.github_ready = availability.repository_key().is_some();
                if !row.github_ready {
                    row.revoke_github();
                }
                row.github_status = availability.label().into();
                self.github_availability.insert(project, availability);
                return Task::batch([
                    self.open_pending_pull_requests(project),
                    self.update_review_requests(review_request_ui::Action::Tick),
                ]);
            }
            Action::Changed => {
                let changes = self
                    .refresh_ui
                    .git
                    .watch
                    .as_ref()
                    .map(|watch| watch.monitor.drain())
                    .unwrap_or_default();
                let mut reload = None;
                for (project, change) in changes {
                    if change.metadata
                        && self
                            .sidebar_model
                            .source_control
                            .as_ref()
                            .is_some_and(|source| source.key.project_id == project)
                    {
                        // Shared refs can change a future branch picker without
                        // requiring status reads in unrelated checkouts.
                        self.refresh_ui
                            .source_invalidated
                            .fetch_or(3, std::sync::atomic::Ordering::AcqRel);
                    }
                    if let Some(source) = self.sidebar_model.source_control.as_ref()
                        && let Some(invalidation) =
                            panel_invalidation(project, &change, &source.key)
                    {
                        self.refresh_ui
                            .source_invalidated
                            .fetch_or(invalidation, std::sync::atomic::Ordering::AcqRel);
                        if !self
                            .source_control_mutations
                            .contains_key(&source.key.canonical_path)
                        {
                            reload = Some(source.key.clone());
                        }
                    }
                    if let Some(error) = &change.error
                        && error != "directory watch limit reached"
                        && error != "submodule contents require a periodic local Git check"
                        && self
                            .refresh_ui
                            .git
                            .entries
                            .get(&project)
                            .is_some_and(|entry| entry.fallback.as_ref() != Some(error))
                    {
                        self.status = Some(TransientNotice::warning(format!(
                            "Local Git watching unavailable; checking once a minute: {error}"
                        )));
                    }
                    self.refresh_ui.git.changed(project, change);
                }
                if let Some(key) = reload {
                    return Task::batch([
                        self.reload_source_control_from_event(&key),
                        self.start_git_reads(),
                    ]);
                }
            }
            Action::Read {
                project,
                generation,
                result,
            } => {
                let Some(entry) = self
                    .refresh_ui
                    .git
                    .entries
                    .get_mut(&project)
                    .filter(|entry| entry.generation == generation)
                else {
                    return Task::none();
                };
                entry.cancellation = None;
                if self.repository_generations.get(&project) != Some(&generation) {
                    return self.start_git_reads();
                }
                let mut repair = BTreeSet::new();
                if self.session.is_none() {
                    entry.catalog_requested = false;
                }
                match result {
                    Ok((mut snapshot, mut plan)) => {
                        let preserved = snapshot.preserve_newer_status(&entry.snapshot);
                        if let Some(plan) = plan.as_mut() {
                            repair = reconcile_watch_plan(
                                plan,
                                entry.plan.as_deref(),
                                &snapshot,
                                &preserved,
                            );
                        }
                        if entry.read_full_scan {
                            entry.read_failure = None;
                        }
                        entry
                            .failed_status_attempts
                            .retain(|id, _| snapshot.status_errors.contains_key(id));
                        for id in snapshot.status_errors.keys() {
                            entry
                                .failed_status_attempts
                                .entry(*id)
                                .or_insert_with(Instant::now);
                        }
                        let previous = std::mem::replace(&mut entry.snapshot, snapshot.clone());
                        let removed =
                            records_removed_outside(project, &previous, &snapshot.worktrees);
                        let reconfigure = plan.is_some();
                        let full_catalog = !entry.catalog_requested
                            || entry.plan.is_none()
                            || reconfigure
                            || entry.read_full_scan;
                        let archived = entry.archived.clone();
                        if let Some(plan) = plan {
                            entry.plan = Some(Arc::new(plan));
                        }
                        let Some(row) = self
                            .sidebar_model
                            .projects
                            .iter_mut()
                            .find(|row| row.id == project)
                        else {
                            self.refresh_ui.git.forget(project);
                            return Task::none();
                        };
                        apply_local(row, &snapshot);
                        if row.github_ready {
                            row.refresh_worktree_pull_requests();
                        }
                        if let Some(GitHubAvailability::Ready { pull_requests, .. }) =
                            self.github_availability.get(&project)
                        {
                            apply_attached_worktree_prs(
                                row,
                                &snapshot.worktrees,
                                &pull_requests
                                    .iter()
                                    .map(|(pr, _)| pr.clone())
                                    .collect::<Vec<_>>(),
                                &archived,
                            );
                        }
                        if let Some(session) = &self.session {
                            let records = catalog_changes(
                                project,
                                &previous,
                                &snapshot.worktrees,
                                removed,
                                full_catalog,
                            );
                            if !records.is_empty() {
                                session.save_worktrees(records);
                            }
                            entry.catalog_requested = true;
                        }
                        if reconfigure {
                            self.refresh_ui.git.configure();
                        }
                        self.sync_worktree_operation_rows();
                        self.refresh_sidebar_channels();
                        self.refresh_sidebar_worktree_panes();
                    }
                    Err(error) => {
                        if entry.read_failure.as_ref() != Some(&error) {
                            self.status = Some(TransientNotice::warning(format!(
                                "Local Git watching unavailable; checking once a minute: {error}"
                            )));
                        }
                        entry.read_failure = Some(error);
                    }
                }
                if !repair.is_empty() {
                    self.refresh_ui.git.changed(
                        project,
                        git_watch::Change {
                            worktrees: repair,
                            membership: true,
                            ..Default::default()
                        },
                    );
                }
            }
        }
        self.start_git_reads()
    }

    pub(super) fn prioritize_git_watches(&mut self) {
        let active = self.sidebar_state.active_worktree().cloned();
        let state = &mut self.refresh_ui.git;
        if let Some(active) = &active {
            state
                .activity
                .retain(|id| *id != active.project_id && state.entries.contains_key(id));
            state.activity.insert(0, active.project_id);
        }
        let mut targets = self.sidebar_state.git_watch_targets();
        if let Some(active) = active {
            targets.insert(0, (active.project_id, Some(active.worktree_id)));
        }
        let mut priority = Vec::new();
        let mut selected = Vec::new();
        for (project_id, worktree_id) in targets {
            if !priority.contains(&project_id) {
                priority.push(project_id);
            }
            let worktree_id = worktree_id.or_else(|| {
                let plan = state.entries.get(&project_id)?.plan.as_ref()?;
                plan.worktrees
                    .iter()
                    .find(|worktree| worktree.git_dir == plan.common_dir)
                    .or_else(|| plan.worktrees.first())
                    .map(|worktree| worktree.id)
            });
            if let Some(id) = worktree_id {
                if !selected.contains(&id) {
                    selected.push(id);
                }
            }
        }
        for project_id in state
            .activity
            .iter()
            .copied()
            .chain(self.sidebar_model.projects.iter().map(|project| project.id))
        {
            if !priority.contains(&project_id) {
                priority.push(project_id);
            }
        }
        if let Some(monitor) = &state.watch {
            monitor.monitor.prioritize(priority, selected);
        }
    }

    fn start_git_reads(&mut self) -> Task<Message> {
        self.prioritize_git_watches();
        let state = &mut self.refresh_ui.git;
        let mut running = state
            .entries
            .values()
            .filter(|entry| entry.cancellation.is_some())
            .count();
        let mut tasks = Vec::new();
        let mut inspected = state.queue.len();
        while running < local_workers() && inspected > 0 {
            inspected -= 1;
            let Some(project) = state.queue.pop_front() else {
                break;
            };
            let Some(entry) = state.entries.get_mut(&project) else {
                continue;
            };
            if entry.cancellation.is_some() {
                state.queue.push_back(project);
                continue;
            }
            let change = std::mem::take(&mut entry.pending);
            for id in entry
                .snapshot
                .status_errors
                .keys()
                .filter(|id| change.topology || change.worktrees.contains(*id))
            {
                entry.failed_status_attempts.insert(*id, Instant::now());
            }
            entry.read_full_scan = change.topology;
            let snapshot = entry.snapshot.clone();
            let plan = entry.plan.clone();
            let generation = entry.generation;
            let cancellation = CancellationToken::default();
            entry.cancellation = Some(cancellation.clone());
            if change.topology {
                entry.last_full_scan = Instant::now();
            }
            running += 1;
            tasks.push(cosmic::task::future(async move {
                let result = tokio::task::spawn_blocking(move || {
                    read_local_with_plan(project, snapshot, change, cancellation, plan.as_deref())
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
                Message::GitUi(Action::Read {
                    project,
                    generation,
                    result,
                })
            }));
        }
        Task::batch(tasks)
    }
}

#[cfg(test)]
fn read_local(
    project: ProjectId,
    snapshot: Snapshot,
    change: git_watch::Change,
    cancellation: CancellationToken,
) -> Result<(Snapshot, Option<git_watch::Project>), String> {
    read_local_with_plan(project, snapshot, change, cancellation, None)
}

fn read_local_with_plan(
    project: ProjectId,
    mut snapshot: Snapshot,
    change: git_watch::Change,
    cancellation: CancellationToken,
    previous_plan: Option<&git_watch::Project>,
) -> Result<(Snapshot, Option<git_watch::Project>), String> {
    let git = RepositoryService::system("git");
    snapshot.status_updates.clear();
    let mut removed = HashSet::new();
    if change.topology {
        let previous_ids = snapshot
            .worktrees
            .iter()
            .map(|(record, _)| record.id)
            .collect::<Vec<_>>();
        let listing_read = next_status_read();
        snapshot.worktrees = git
            .list_worktrees(&snapshot.repository.common_dir, &cancellation)
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|record| record.bare || worktree_ui::worktree_directory_available(&record.path))
            .map(|record| (record, None))
            .collect();
        let listed = snapshot
            .worktrees
            .iter()
            .map(|(record, _)| record.id)
            .collect::<HashSet<_>>();
        for id in previous_ids.into_iter().filter(|id| !listed.contains(id)) {
            snapshot.status_reads.insert(id, listing_read);
            removed.insert(id);
        }
    }
    let mut vanished = HashSet::new();
    let mut coverage_updates = BTreeSet::new();
    for (record, status) in &mut snapshot.worktrees {
        // `references` also wakes the host's checkout-free branch inventory.
        // The UI's metadata/upstream routing already selects affected rows.
        if record.bare || (!change.topology && !change.worktrees.contains(&record.id)) {
            continue;
        }
        // A membership edge replaces the cached full count in this same
        // coalesced status process. Tracked-only writes do not rescan untracked
        // trees; the bounded topology fallback repairs incomplete watch coverage.
        let include_untracked = change.topology || change.membership || change.untracked;
        snapshot.status_updates.insert(record.id);
        snapshot.status_reads.insert(record.id, next_status_read());
        let current = match git.status_projection(
            &record.path,
            include_untracked,
            sidebar::STATUS_DISPLAY_LIMIT,
            &cancellation,
        ) {
            Ok(current) => current,
            // Its own removal is often the event being read, and one absent
            // checkout must not fail its siblings.
            Err(_) if checkout_removed(&record.path) => {
                vanished.insert(record.id);
                continue;
            }
            Err(error) => {
                tracing::warn!(worktree = %record.path.display(), %error, "local Git worktree read failed; retaining previous state");
                snapshot
                    .status_errors
                    .insert(record.id, error.to_string().chars().take(512).collect());
                continue;
            }
        };
        // Existing file edits cannot introduce index membership. On an index
        // edge, ordinary covered paths keep their plan; a force-added ignored
        // path or incomplete status needs a fresh complete index census.
        if change.metadata
            && change.untracked
            && (previous_plan.is_some_and(|plan| plan.status_needs_replan(record, &current))
                || current.listing_truncated)
        {
            coverage_updates.insert(record.id);
        }
        snapshot.status_errors.remove(&record.id);
        apply_status_head(record, &current);
        let head_changed = status
            .as_ref()
            .is_none_or(|previous| previous.head != current.head);
        if head_changed || change.metadata {
            snapshot.heads.remove(&record.id);
            if let Some(branch) = current.branch()
                && let Some(head) =
                    crate::github::worktree_head_repository(&record.path, branch, &cancellation)
            {
                snapshot.heads.insert(record.id, head);
            }
        }
        *status = Some(current.display_summary());
    }
    snapshot
        .worktrees
        .retain(|(record, _)| !vanished.contains(&record.id));
    let retained: HashSet<_> = snapshot
        .worktrees
        .iter()
        .map(|(record, _)| record.id)
        .collect();
    snapshot.status_errors.retain(|id, _| retained.contains(id));
    snapshot.status_updates.retain(|id| retained.contains(id));
    snapshot
        .status_reads
        .retain(|id, _| retained.contains(id) || removed.contains(id) || vanished.contains(id));
    snapshot.heads.retain(|id, _| retained.contains(id));
    let upstreams = snapshot
        .worktrees
        .iter()
        .filter_map(|(record, status)| {
            let upstream = match &status.as_ref()?.tracking {
                crate::repository::TrackingState::Compared { upstream, .. } => Some(upstream),
                crate::repository::TrackingState::Unknown { upstream } => upstream.as_ref(),
                crate::repository::TrackingState::NoUpstream => None,
            }?;
            Some((record.id, upstream.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut changed_coverage = if change.membership || change.topology {
        change.worktrees.clone()
    } else {
        BTreeSet::new()
    };
    changed_coverage.extend(coverage_updates.iter().copied());
    let plan = (change.membership
        || change.topology
        || !vanished.is_empty()
        || !coverage_updates.is_empty())
    .then(|| {
        git_watch::Project::read_changed_with_upstreams(
            project,
            snapshot.repository.common_dir.clone(),
            &snapshot
                .worktrees
                .iter()
                .map(|(record, _)| record.clone())
                .collect::<Vec<_>>(),
            &cancellation,
            previous_plan,
            &changed_coverage,
            &upstreams,
        )
    })
    .transpose()?
    .or_else(|| {
        previous_plan.and_then(|plan| {
            plan.refresh_routing(
                snapshot.worktrees.iter().map(|(record, _)| record),
                &upstreams,
            )
        })
    });
    Ok((snapshot, plan))
}

pub(super) fn apply_status_head(record: &mut WorktreeRecord, status: &StatusSnapshot) {
    match &status.head {
        HeadState::Branch { name, oid } => {
            record.branch = Some(format!("refs/heads/{name}"));
            record.head_oid = Some(oid.clone());
            record.detached = false;
        }
        HeadState::Unborn { branch } => {
            record.branch = branch.as_ref().map(|name| format!("refs/heads/{name}"));
            record.head_oid = None;
            record.detached = false;
        }
        HeadState::Detached { oid } => {
            record.branch = None;
            record.head_oid = Some(oid.clone());
            record.detached = true;
        }
        HeadState::Unknown => {}
    }
}

/// Whether a failed read is explained by the directory no longer being a
/// checkout: deleted outright, or part-way through `git worktree remove`,
/// which takes `.git` before the files and the listing that names them. A
/// permission or transient I/O failure is not evidence of either.
fn checkout_removed(root: &Path) -> bool {
    std::fs::symlink_metadata(root.join(".git")).is_err_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        )
    })
}

/// The records to close for worktrees Git listed in `previous` and no longer
/// does. One the application deleted has already left the snapshot, so its own
/// tombstone stands.
fn records_removed_outside(
    project: ProjectId,
    previous: &Snapshot,
    listed: &[(WorktreeRecord, Option<StatusSnapshot>)],
) -> Vec<WorktreeState> {
    previous
        .worktrees
        .iter()
        .filter(|(old, _)| !listed.iter().any(|(new, _)| new.id == old.id))
        .map(|(record, _)| {
            worktree_ui::worktree_state(project, record, worktree_ui::removed_outside())
        })
        .collect()
}

/// Only catalog fields belong in this comparison. Status, locks, commit IDs,
/// and PR authority still flow through the ordinary UI application below.
/// Structural/first reads always reassert open rows, including tombstone reopen.
fn catalog_changes(
    project: ProjectId,
    previous: &Snapshot,
    listed: &[(WorktreeRecord, Option<StatusSnapshot>)],
    mut removed: Vec<WorktreeState>,
    full: bool,
) -> Vec<WorktreeState> {
    let open =
        |record: &WorktreeRecord| worktree_ui::worktree_state(project, record, Existence::Open);
    if full {
        removed.extend(listed.iter().map(|(record, _)| open(record)));
        return removed;
    }
    let previous = previous
        .worktrees
        .iter()
        .map(|(record, _)| (record.id, open(record)))
        .collect::<HashMap<_, _>>();
    removed.extend(listed.iter().filter_map(|(record, _)| {
        let current = open(record);
        (previous.get(&record.id) != Some(&current)).then_some(current)
    }));
    removed
}

fn apply_local(project: &mut SidebarProject, snapshot: &Snapshot) {
    let mut previous = std::mem::take(&mut project.worktrees)
        .into_iter()
        .map(|row| (row.id, row))
        .collect::<HashMap<_, _>>();
    project.worktrees = snapshot
        .worktrees
        .iter()
        .map(|(record, status)| {
            let previous_row = previous.remove(&record.id);
            let apply_status =
                previous_row.is_none() || snapshot.status_updates.contains(&record.id);
            let mut row = previous_row
                .unwrap_or_else(|| SidebarWorktree::from_repository(record, status.as_ref()));
            let error = snapshot.status_errors.get(&record.id);
            row.apply_repository(
                record,
                if apply_status && error.is_none() {
                    status.as_ref()
                } else {
                    None
                },
            );
            if apply_status {
                row.apply_status_error(error.map(String::as_str));
            }
            let head = snapshot.heads.get(&record.id).cloned();
            if row.head_repository != head {
                row.clear_pull_request();
            }
            row.head_repository = head;
            row
        })
        .collect();
}

struct Source(async_channel::Receiver<()>);
impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.0.same_channel(&other.0)
    }
}
impl Eq for Source {}
impl std::hash::Hash for Source {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::any::TypeId::of::<Self>().hash(state);
    }
}
fn stream(source: &Source) -> Pin<Box<dyn iced::futures::Stream<Item = ()> + Send>> {
    Box::pin(iced::futures::stream::unfold(
        source.0.clone(),
        |receiver| async move {
            receiver.recv().await.ok()?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            Some(((), receiver))
        },
    ))
}

impl Cagents {
    pub(super) fn github_read_task(&mut self, project: ProjectId) -> Task<Message> {
        let Some(entry) = self.refresh_ui.git.entries.get_mut(&project) else {
            // Initial loads and explicit retries still discover the project;
            // subsequent remote ticks need no local census.
            return self
                .sidebar_model
                .project(project)
                .map(|row| (row.repository.clone(), row.kind))
                .map_or_else(Task::none, |(path, _)| {
                    self.repository_load_task(path, Some(project), false)
                });
        };
        entry.remote_started = true;
        let snapshot = entry.snapshot.clone();
        let generation = entry.generation;
        let cancellation = CancellationToken::default();
        self.refresh_ui
            .projects
            .insert(project, cancellation.clone());
        self.refresh_ui.started(project);
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let availability = load_repository_github(&snapshot.repository, &cancellation);
                let archived = if let GitHubAvailability::Ready {
                    repository_key,
                    pull_requests,
                    ..
                } = &availability
                {
                    let recent = pull_requests
                        .iter()
                        .map(|(pr, _)| pr.clone())
                        .collect::<Vec<_>>();
                    attached_worktree_pull_requests(
                        &GitHubService::system("gh"),
                        repository_key,
                        &snapshot.worktrees,
                        &recent,
                        &snapshot.heads,
                        &cancellation,
                    )
                } else {
                    Vec::new()
                };
                (availability, archived)
            })
            .await
            .map_err(|error| error.to_string());
            Message::GitUi(Action::Remote {
                project,
                generation,
                result,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};

    fn git(directory: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        for (name, _) in
            std::env::vars_os().filter(|(name, _)| name.as_encoded_bytes().starts_with(b"GIT_"))
        {
            command.env_remove(name);
        }
        let output = command
            .current_dir(directory)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.templateDir=",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture(directory: &Path) -> Snapshot {
        fs::create_dir_all(directory).unwrap();
        git(directory, &["init", "-b", "main"]);
        git(directory, &["config", "user.name", "Fixture"]);
        git(
            directory,
            &["config", "user.email", "fixture@example.invalid"],
        );
        fs::write(directory.join("tracked.txt"), "original\n").unwrap();
        git(directory, &["add", "tracked.txt"]);
        git(directory, &["commit", "-m", "fixture"]);
        let service = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        let repository = service.discover(directory, &cancellation).unwrap();
        let worktrees: Vec<_> = service
            .list_worktrees(&repository.common_dir, &cancellation)
            .unwrap()
            .into_iter()
            .map(|record| {
                let status = service
                    .status_with_untracked(&record.path, false, &cancellation)
                    .unwrap();
                (record, Some(status))
            })
            .collect();
        let status_updates = worktrees.iter().map(|(record, _)| record.id).collect();
        Snapshot {
            repository,
            worktrees,
            heads: HashMap::new(),

            status_errors: HashMap::new(),
            status_updates,
            status_reads: HashMap::new(),
        }
    }

    #[test]
    fn covered_index_updates_keep_plan_but_force_added_ignored_paths_rearm_watches() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("repo");
        let mut snapshot = fixture(&root);
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir_all(root.join("ignored/deep")).unwrap();
        fs::write(root.join("ignored/deep/new.txt"), "initial\n").unwrap();
        let project = ProjectId::new();
        let id = snapshot.worktrees[0].0.id;
        let token = CancellationToken::default();
        let plan = git_watch::Project::read_changed_with_upstreams(
            project,
            snapshot.repository.common_dir.clone(),
            &snapshot
                .worktrees
                .iter()
                .map(|(record, _)| record.clone())
                .collect::<Vec<_>>(),
            &token,
            None,
            &[id].into_iter().collect(),
            &BTreeMap::new(),
        )
        .unwrap();
        fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        let index_edge = || git_watch::Change {
            worktrees: [id].into_iter().collect(),
            metadata: true,
            untracked: true,
            ..Default::default()
        };
        let (updated, unchanged_plan) =
            read_local_with_plan(project, snapshot, index_edge(), token.clone(), Some(&plan))
                .unwrap();
        assert!(
            unchanged_plan.is_none(),
            "covered ordinary git add does not repeat index inventory or directory planning"
        );
        snapshot = updated;
        git(&root, &["add", "-f", "ignored/deep/new.txt"]);
        let (_, repaired) =
            read_local_with_plan(project, snapshot, index_edge(), token, Some(&plan)).unwrap();
        let repaired =
            repaired.expect("a force-added ignored parent needs a selective complete census");
        let (wake, _receiver) = async_channel::bounded(1);
        let monitor = git_watch::Monitor::start(move || {
            let _ = wake.try_send(());
        })
        .unwrap();
        monitor.configure(vec![Arc::new(repaired)]);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if monitor
                .drain()
                .get(&project)
                .is_some_and(|change| change.coverage == Some(true))
            {
                break;
            }
            assert!(Instant::now() < deadline, "repaired watches settle");
            std::thread::sleep(Duration::from_millis(10));
        }
        monitor.drain();
        fs::write(
            root.join("ignored/deep/new.txt"),
            "edited after admission\n",
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if monitor
                .drain()
                .get(&project)
                .is_some_and(|change| change.worktrees.contains(&id) && !change.topology)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "new forced-tracked content is watched without another index write or poll"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn app_with_status_snapshot(snapshot: Snapshot, key: &ActiveWorktreeKey) -> Cagents {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.sidebar_model.projects = vec![SidebarProject::new(
            key.project_id,
            snapshot.repository.id,
            "Fixture".into(),
            snapshot.repository.common_dir.clone(),
            snapshot
                .worktrees
                .iter()
                .map(|(record, status)| SidebarWorktree::from_repository(record, status.as_ref()))
                .collect(),
            Vec::new(),
        )];
        apply_local(&mut app.sidebar_model.projects[0], &snapshot);
        app.repository_generations.insert(key.project_id, 9);
        app.refresh_ui.git.entries.insert(
            key.project_id,
            Entry {
                snapshot,
                generation: 9,
                plan: None,
                archived: Vec::new(),
                remote_started: false,
                cancellation: Some(CancellationToken::default()),
                pending: Default::default(),
                fallback: None,
                read_failure: None,
                read_full_scan: false,
                catalog_requested: false,
                last_full_scan: Instant::now(),
                failed_status_attempts: HashMap::new(),
            },
        );
        app.sidebar_model.source_control = Some(SourceControlModel::loading(key.clone()));
        app.refresh_ui.source_generation = 7;
        app
    }

    fn bound_status_message(
        key: &ActiveWorktreeKey,
        request: u64,
        snapshot: RepositorySnapshot,
        read: u64,
    ) -> Message {
        let status = snapshot.status.display_summary();
        let model = SourceControlModel::ready(key.clone(), &snapshot);
        Message::SourceControlLoaded {
            key: key.clone(),
            request,
            result: Ok(SourceControlRead {
                prepared: Some(Box::new((snapshot, model))),
                status,
                head_repository: None,
                read,
            }),
        }
    }

    #[test]
    fn failed_worktree_status_is_last_known_and_sibling_reads_cannot_replay_it_after_recovery() {
        const TEST: &str = "app::git_ui::tests::failed_worktree_status_is_last_known_and_sibling_reads_cannot_replay_it_after_recovery";
        if !crate::git_watch::tests::fixture_isolated(TEST) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let primary = directory.path().join("primary");
        let mut snapshot = fixture(&primary);
        let linked = directory.path().join("linked");
        git(
            &primary,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::default();
        snapshot.worktrees = service
            .list_worktrees(&snapshot.repository.common_dir, &cancel)
            .unwrap()
            .into_iter()
            .map(|record| {
                let status = service
                    .status_with_untracked(&record.path, true, &cancel)
                    .unwrap();
                (record, Some(status))
            })
            .collect();
        snapshot.status_updates = snapshot
            .worktrees
            .iter()
            .map(|(record, _)| record.id)
            .collect();
        let linked_id = snapshot
            .worktrees
            .iter()
            .find(|(record, _)| record.path == linked)
            .unwrap()
            .0
            .id;
        let primary_id = snapshot
            .worktrees
            .iter()
            .find(|(record, _)| record.path == primary)
            .unwrap()
            .0
            .id;
        let mut project = SidebarProject::new(
            ProjectId::new(),
            snapshot.repository.id,
            "Fixture".into(),
            snapshot.repository.common_dir.clone(),
            snapshot
                .worktrees
                .iter()
                .map(|(record, status)| SidebarWorktree::from_repository(record, status.as_ref()))
                .collect(),
            Vec::new(),
        );
        apply_local(&mut project, &snapshot);
        let private = service.git_dir(&linked, &cancel).unwrap();
        let index = fs::read(private.join("index")).unwrap();
        fs::write(linked.join("new.txt"), "new untracked file\n").unwrap();
        fs::write(private.join("index"), b"invalid index\n").unwrap();
        let change = || git_watch::Change {
            worktrees: [linked_id].into(),
            untracked: true,
            ..Default::default()
        };
        let expected_failure = service
            .status_projection(&linked, true, sidebar::STATUS_DISPLAY_LIMIT, &cancel)
            .unwrap_err();
        let crate::repository::RepositoryError::Process(process) = &expected_failure else {
            panic!("expected corrupt-index process failure, got {expected_failure:?}");
        };
        assert_eq!(process.kind, crate::process::ProcessFailureKind::Exit);
        assert!(process.stderr_line().unwrap().contains("index"));
        let (failed, _) = read_local(project.id, snapshot, change(), cancel.clone()).unwrap();
        assert_eq!(
            failed.status_errors[&linked_id],
            expected_failure.to_string()
        );
        apply_local(&mut project, &failed);
        let row = project
            .worktrees
            .iter()
            .find(|row| row.id == linked_id)
            .unwrap();
        assert!(row.status_error.is_some());
        assert_eq!(
            row.dirty.as_ref().unwrap().untracked,
            0,
            "retain the explicitly last-known count"
        );
        let sibling_change = || git_watch::Change {
            worktrees: [primary_id].into(),
            ..Default::default()
        };
        let (sibling, _) =
            read_local(project.id, failed, sibling_change(), cancel.clone()).unwrap();
        apply_local(&mut project, &sibling);
        assert!(
            project
                .worktrees
                .iter()
                .find(|row| row.id == linked_id)
                .unwrap()
                .status_error
                .is_some()
        );

        fs::write(private.join("index"), index).unwrap();
        let current = service
            .status_with_untracked(&linked, true, &cancel)
            .unwrap();
        let row = project
            .worktrees
            .iter_mut()
            .find(|row| row.id == linked_id)
            .unwrap();
        // These are the same successful-row methods SourceControlLoaded uses.
        row.apply_status(&current);
        assert!(row.status_error.is_none());
        assert_eq!(row.dirty.as_ref().unwrap().untracked, 1);
        let (sibling, _) =
            read_local(project.id, sibling, sibling_change(), cancel.clone()).unwrap();
        apply_local(&mut project, &sibling);
        let row = project
            .worktrees
            .iter()
            .find(|row| row.id == linked_id)
            .unwrap();
        assert!(
            row.status_error.is_none(),
            "A-only completion cannot replay B's old error"
        );
        assert_eq!(
            row.dirty.as_ref().unwrap().untracked,
            1,
            "nor B's old cached clean status"
        );

        let (recovered, _) = read_local(project.id, sibling, change(), cancel).unwrap();
        assert!(!recovered.status_errors.contains_key(&linked_id));
        apply_local(&mut project, &recovered);
        let row = project
            .worktrees
            .iter()
            .find(|row| row.id == linked_id)
            .unwrap();
        assert!(row.status_error.is_none());
        assert_eq!(row.dirty.as_ref().unwrap().untracked, 1);
    }

    #[test]
    fn bound_and_watcher_status_results_keep_read_order_across_actual_updater_messages() {
        for scenario in [
            "failed-watch-bound-first",
            "failed-watch-watch-first",
            "successful-watch-bound-first",
            "successful-watch-watch-first",
            "older-bound-after-watch",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("repo");
            let initial = fixture(&root);
            let service = RepositoryService::system("git");
            let cancel = CancellationToken::default();
            let worktree = initial.worktrees[0].0.id;
            let private = service.git_dir(&root, &cancel).unwrap();
            let head = initial.worktrees[0].0.head_oid.as_ref().unwrap().as_str();
            fs::write(private.join("MERGE_HEAD"), format!("{head}\n")).unwrap();
            fs::write(private.join("MERGE_MSG"), "pending fixture merge\n").unwrap();
            let project_id = ProjectId::new();
            let change = || git_watch::Change {
                worktrees: [worktree].into(),
                untracked: true,
                ..Default::default()
            };
            let (baseline, _) = read_local(project_id, initial, change(), cancel.clone()).unwrap();
            let index = fs::read(private.join("index")).unwrap();
            let older_bound = if scenario == "older-bound-after-watch" {
                let read = next_status_read();
                Some((service.snapshot(&root, &cancel).unwrap(), read))
            } else {
                None
            };
            if scenario.starts_with("failed-watch") {
                fs::write(private.join("index"), b"invalid index\n").unwrap();
            }
            let delayed_watch = if older_bound.is_none() {
                let read = read_local(project_id, baseline.clone(), change(), cancel.clone())
                    .unwrap()
                    .0;
                if scenario.starts_with("failed-watch") {
                    assert!(read.status_errors.contains_key(&worktree));
                }
                Some(read)
            } else {
                None
            };
            fs::write(private.join("index"), index).unwrap();
            fs::remove_file(private.join("MERGE_HEAD")).unwrap();
            fs::remove_file(private.join("MERGE_MSG")).unwrap();
            fs::write(root.join("new.txt"), "newer untracked file\n").unwrap();
            let (prepared, status_read) = older_bound.unwrap_or_else(|| {
                let read = next_status_read();
                (service.snapshot(&root, &cancel).unwrap(), read)
            });
            let watch = delayed_watch.unwrap_or_else(|| {
                read_local(project_id, baseline.clone(), change(), cancel.clone())
                    .unwrap()
                    .0
            });
            let key = ActiveWorktreeKey {
                project_id,
                worktree_id: worktree,
                canonical_path: root.clone(),
            };
            let mut app = super::super::tests::context_inspector_test_app(false);
            app.sidebar_model.projects = vec![SidebarProject::new(
                project_id,
                baseline.repository.id,
                "Fixture".into(),
                baseline.repository.common_dir.clone(),
                baseline
                    .worktrees
                    .iter()
                    .map(|(record, status)| {
                        SidebarWorktree::from_repository(record, status.as_ref())
                    })
                    .collect(),
                Vec::new(),
            )];
            apply_local(&mut app.sidebar_model.projects[0], &baseline);
            app.repository_generations.insert(project_id, 9);
            app.refresh_ui.git.entries.insert(
                project_id,
                Entry {
                    snapshot: baseline,
                    generation: 9,
                    plan: None,
                    archived: Vec::new(),
                    remote_started: false,
                    cancellation: Some(cancel.clone()),
                    pending: Default::default(),
                    fallback: None,
                    read_failure: None,
                    read_full_scan: false,
                    catalog_requested: false,
                    last_full_scan: Instant::now(),
                    failed_status_attempts: HashMap::new(),
                },
            );
            app.sidebar_model.source_control = Some(SourceControlModel::loading(key.clone()));
            app.refresh_ui.source_generation = 7;
            let bound_message = Message::SourceControlLoaded {
                key: key.clone(),
                request: 7,
                result: Ok(SourceControlRead {
                    status: prepared.status.display_summary(),
                    head_repository: None,
                    read: status_read,
                    prepared: Some(Box::new((
                        prepared.clone(),
                        SourceControlModel::ready(key.clone(), &prepared),
                    ))),
                }),
            };
            let watch_message = Message::GitUi(Action::Read {
                project: project_id,
                generation: 9,
                result: Ok((watch, None)),
            });
            if scenario.ends_with("bound-first") {
                drop(app.update(bound_message));
                drop(app.update(watch_message));
            } else {
                drop(app.update(watch_message));
                drop(app.update(bound_message));
                assert_eq!(
                    app.refresh_ui.source_generation, 7,
                    "watcher progress must not restart the full model"
                );
                assert_eq!(
                    app.sidebar_model
                        .source_control
                        .as_ref()
                        .unwrap()
                        .data_state,
                    sidebar::SidebarDataState::Ready
                );
            }
            let row = app.sidebar_model.projects[0]
                .worktrees
                .iter()
                .find(|row| row.id == worktree)
                .unwrap();
            assert!(row.status_error.is_none(), "{scenario}");
            assert_eq!(row.dirty.as_ref().unwrap().untracked, 1, "{scenario}");

            let cached = &app.refresh_ui.git.entries[&project_id].snapshot;
            assert!(!cached.status_errors.contains_key(&worktree), "{scenario}");

            assert_eq!(
                cached.worktrees[0].1.as_ref().unwrap().dirty.untracked,
                1,
                "{scenario}"
            );
        }
    }

    #[test]
    fn a_newer_failed_bound_read_cannot_resurrect_a_moved_checkout() {
        for reload in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let primary = directory.path().join("primary");
            let initial = fixture(&primary);
            let linked = directory.path().join("linked");
            let moved = directory.path().join("moved");
            git(
                &primary,
                &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
            );
            let project = ProjectId::new();
            let (baseline, _) = read_local(
                project,
                initial,
                git_watch::Change {
                    topology: true,
                    ..Default::default()
                },
                CancellationToken::default(),
            )
            .unwrap();
            let id = baseline
                .worktrees
                .iter()
                .find(|(record, _)| record.path == linked)
                .unwrap()
                .0
                .id;
            let key = ActiveWorktreeKey {
                project_id: project,
                worktree_id: id,
                canonical_path: linked.clone(),
            };
            let mut app = app_with_status_snapshot(baseline.clone(), &key);
            app.active_workspace = Some(linked.clone());
            app.sidebar_state.sync_active_worktree(
                &app.sidebar_model,
                &SidebarFocus::Repository {
                    worktree: linked.clone(),
                },
            );
            let load = if reload {
                app.repository_generations.insert(project, 10);
                Some(app.repository_read_task(
                    primary.clone(),
                    project,
                    10,
                    false,
                    Some(ProjectKind::Git),
                ))
            } else {
                None
            };
            git(
                &primary,
                &[
                    "worktree",
                    "move",
                    linked.to_str().unwrap(),
                    moved.to_str().unwrap(),
                ],
            );
            let omitted = if let Some(load) = load {
                super::super::tests::task_app_message(load)
            } else {
                let (snapshot, plan) = read_local(
                    project,
                    baseline,
                    git_watch::Change {
                        topology: true,
                        ..Default::default()
                    },
                    CancellationToken::default(),
                )
                .unwrap();
                Message::GitUi(Action::Read {
                    project,
                    generation: 9,
                    result: Ok((snapshot, plan)),
                })
            };
            let failure =
                super::super::tests::task_app_message(app.source_control_snapshot_task(key));
            assert!(
                matches!(&failure, Message::SourceControlLoaded { result: Err(failure), .. } if failure.status_read.is_some()),
                "actual later Git status failure owns only an error marker"
            );
            drop(app.update(failure));
            drop(app.update(omitted));
            let rows = &app.sidebar_model.project(project).unwrap().worktrees;
            assert!(
                !rows.iter().any(|row| row.id == id),
                "failed last-known data cannot revive old path, reload={reload}"
            );
            let current = rows
                .iter()
                .find(|row| row.path == moved)
                .expect("the moved checkout remains listed");
            assert_ne!(
                current.id, id,
                "worktree IDs derive from their current paths"
            );
            let entry = &app.refresh_ui.git.entries[&project];
            assert!(
                !entry
                    .snapshot
                    .worktrees
                    .iter()
                    .any(|(record, _)| record.id == id)
            );
            assert!(!entry.snapshot.status_errors.contains_key(&id));
        }
    }

    #[test]
    fn delayed_removal_cannot_close_a_worktree_recovered_by_a_newer_bound_read() {
        for topology in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let primary = directory.path().join("primary");
            let initial = fixture(&primary);
            let linked = directory.path().join("linked");
            git(
                &primary,
                &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
            );
            let project_id = ProjectId::new();
            let cancel = CancellationToken::default();
            let (baseline, baseline_plan) = read_local(
                project_id,
                initial,
                git_watch::Change {
                    topology: true,
                    ..Default::default()
                },
                cancel.clone(),
            )
            .unwrap();
            let worktree = baseline
                .worktrees
                .iter()
                .find(|(record, _)| record.path == linked)
                .unwrap()
                .0
                .id;
            let key = ActiveWorktreeKey {
                project_id,
                worktree_id: worktree,
                canonical_path: linked.clone(),
            };
            let parked = directory.path().join("parked");
            fs::rename(&linked, &parked).unwrap();
            let (removed, removed_plan) = read_local(
                project_id,
                baseline.clone(),
                git_watch::Change {
                    topology,
                    worktrees: [worktree].into(),
                    ..Default::default()
                },
                cancel.clone(),
            )
            .unwrap();
            assert!(
                !removed
                    .worktrees
                    .iter()
                    .any(|(record, _)| record.id == worktree)
            );
            assert!(
                removed.status_reads.contains_key(&worktree),
                "omission has its own read authority"
            );
            let baseline_plan = baseline_plan.unwrap();
            let removed_plan = removed_plan.unwrap();
            assert!(
                baseline_plan
                    .worktrees
                    .iter()
                    .any(|record| record.id == worktree)
            );
            assert!(
                !removed_plan
                    .worktrees
                    .iter()
                    .any(|record| record.id == worktree)
            );
            fs::rename(&parked, &linked).unwrap();
            fs::write(linked.join("new.txt"), "recovered content\n").unwrap();
            let read = next_status_read();
            let snapshot = RepositoryService::system("git")
                .snapshot(&linked, &cancel)
                .unwrap();
            let mut app = app_with_status_snapshot(baseline, &key);
            app.refresh_ui
                .git
                .entries
                .get_mut(&project_id)
                .unwrap()
                .plan = Some(Arc::new(baseline_plan));
            app.refresh_ui.git.watch = Some(Watch::start().unwrap());
            drop(app.update(bound_status_message(&key, 7, snapshot, read)));
            drop(app.update(Message::GitUi(Action::Read {
                project: project_id,
                generation: 9,
                result: Ok((removed, Some(removed_plan))),
            })));
            let row = app.sidebar_model.projects[0]
                .worktrees
                .iter()
                .find(|row| row.id == worktree)
                .expect("newer successful B read must survive old omitted B result");
            assert!(row.status_error.is_none());
            assert_eq!(row.dirty.as_ref().unwrap().untracked, 1);
            assert!(
                app.refresh_ui.git.entries[&project_id]
                    .snapshot
                    .worktrees
                    .iter()
                    .any(|(record, _)| record.id == worktree)
            );
            let plan = app.refresh_ui.git.entries[&project_id]
                .plan
                .as_ref()
                .unwrap();
            assert!(
                plan.worktrees
                    .iter()
                    .any(|record| record.id == worktree && record.root == linked),
                "recovered B keeps its content and private metadata coverage"
            );
            // No prior coverage must request one worker repair; a newer
            // removal must also strip any stale incoming registration.
            let current = &app.refresh_ui.git.entries[&project_id].snapshot;
            let preserved = [worktree].into_iter().collect();
            let mut absent_plan = (**plan).clone();
            absent_plan.worktrees.retain(|row| row.id != worktree);
            assert_eq!(
                reconcile_watch_plan(&mut absent_plan, None, current, &preserved),
                [worktree].into()
            );
            let mut without_b = current.clone();
            without_b
                .worktrees
                .retain(|(record, _)| record.id != worktree);
            let mut stale_plan = (**plan).clone();
            assert!(
                reconcile_watch_plan(&mut stale_plan, Some(plan), &without_b, &preserved)
                    .is_empty()
            );
            assert!(
                !stale_plan
                    .worktrees
                    .iter()
                    .any(|record| record.id == worktree)
            );
            let monitor = app.refresh_ui.git.watch.as_ref().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if monitor
                    .drain()
                    .get(&project_id)
                    .is_some_and(|change| change.coverage == Some(true))
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "reconciled native watch coverage was not acknowledged"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            monitor.drain();
            fs::write(linked.join("new.txt"), "edit after recovery\n").unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if monitor
                    .drain()
                    .get(&project_id)
                    .is_some_and(|change| change.worktrees.contains(&worktree))
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "recovered worktree lost subsequent native events"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn bound_success_repairs_missed_branch_and_upstream_watch_routing_without_a_census() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let initial = fixture(&root);
        let project = ProjectId::new();
        let cancel = CancellationToken::default();
        let (baseline, plan) = read_local(
            project,
            initial,
            git_watch::Change {
                topology: true,
                ..Default::default()
            },
            cancel.clone(),
        )
        .unwrap();
        let key = ActiveWorktreeKey {
            project_id: project,
            worktree_id: baseline.worktrees[0].0.id,
            canonical_path: root.clone(),
        };
        let old = Arc::new(plan.unwrap());
        git(&root, &["checkout", "-b", "learned"]);
        git(&root, &["remote", "add", "origin", root.to_str().unwrap()]);
        git(
            &root,
            &["update-ref", "refs/remotes/origin/learned", "HEAD"],
        );
        git(&root, &["config", "branch.learned.remote", "origin"]);
        git(
            &root,
            &["config", "branch.learned.merge", "refs/heads/learned"],
        );
        let read = next_status_read();
        let full = RepositoryService::system("git")
            .snapshot(&root, &cancel)
            .unwrap();
        let mut app = app_with_status_snapshot(baseline, &key);
        app.refresh_ui.git.entries.get_mut(&project).unwrap().plan = Some(old.clone());
        app.refresh_ui.git.watch = Some(Watch::start().unwrap());
        let prior_repository = GitHubRepoKey::new("github.com", None, "old-fork", "repo").unwrap();
        app.refresh_ui
            .git
            .entries
            .get_mut(&project)
            .unwrap()
            .snapshot
            .heads
            .insert(key.worktree_id, prior_repository.clone());
        app.sidebar_model.projects[0].worktrees[0].head_repository = Some(prior_repository);
        app.sidebar_model.projects[0].worktrees[0].pull_request_state =
            Some(sidebar::WorktreePullRequestState::Open);
        let delayed_watch = app.refresh_ui.git.entries[&project].snapshot.clone();
        drop(app.update(bound_status_message(&key, 7, full, read)));
        let row = &app.sidebar_model.projects[0].worktrees[0];
        assert_eq!(row.branch.as_deref(), Some("learned"));
        assert_eq!(
            row.label(),
            "learned",
            "bound-only success repairs the visible branch too"
        );
        assert_eq!(row.id, key.worktree_id);
        assert_eq!(row.path, key.canonical_path);
        assert!(row.head_repository.is_none());
        assert!(row.pull_request_state.is_none());
        assert!(
            !app.refresh_ui.git.entries[&project]
                .snapshot
                .heads
                .contains_key(&key.worktree_id)
        );
        let entry = &app.refresh_ui.git.entries[&project];
        let current = entry.plan.as_ref().unwrap().clone();
        assert!(
            !Arc::ptr_eq(&old, &current),
            "changed routing must replace the plan"
        );
        assert!(
            current.matches(
                &entry.snapshot.repository.common_dir,
                &entry
                    .snapshot
                    .worktrees
                    .iter()
                    .map(|(record, _)| record.clone())
                    .collect::<Vec<_>>()
            )
        );
        let status = entry.snapshot.worktrees[0]
            .1
            .as_ref()
            .unwrap()
            .display_summary();
        let mut stale_status = status.clone();
        stale_status.head = HeadState::Branch {
            name: "stale-branch".into(),
            oid: "a".repeat(40),
        };
        drop(app.update(Message::SourceControlLoaded {
            key: key.clone(),
            request: 7,
            result: Ok(SourceControlRead {
                prepared: None,
                status,
                head_repository: None,
                read: next_status_read(),
            }),
        }));
        assert!(
            Arc::ptr_eq(
                &current,
                app.refresh_ui.git.entries[&project].plan.as_ref().unwrap()
            ),
            "unchanged proof must not clone/rebuild coverage"
        );
        drop(app.update(Message::SourceControlLoaded {
            key: key.clone(),
            request: 7,
            result: Ok(SourceControlRead {
                prepared: None,
                status: stale_status,
                head_repository: None,
                read: read.saturating_sub(1),
            }),
        }));
        assert_eq!(
            app.sidebar_model.projects[0].worktrees[0].label(),
            "learned",
            "an older accepted-generation callback cannot rewind the visible head"
        );
        drop(app.update(Message::GitUi(Action::Read {
            project,
            generation: 9,
            result: Ok((delayed_watch, None)),
        })));
        assert_eq!(
            app.sidebar_model.projects[0].worktrees[0].label(),
            "learned"
        );
        assert!(
            app.sidebar_model.projects[0].worktrees[0]
                .head_repository
                .is_none(),
            "old watcher cannot restore revoked fork/PR identity"
        );
        assert!(
            !app.refresh_ui.git.entries[&project]
                .snapshot
                .heads
                .contains_key(&key.worktree_id)
        );
        let monitor = app.refresh_ui.git.watch.as_ref().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if monitor
                .drain()
                .get(&project)
                .is_some_and(|change| change.coverage == Some(true))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "bound routing configuration not acknowledged"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        for name in ["refs/heads/learned", "refs/remotes/origin/learned"] {
            monitor.drain();
            let path = root.join(".git").join(name);
            let bytes = fs::read(&path).unwrap();
            fs::write(&path, bytes).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if monitor
                    .drain()
                    .get(&project)
                    .is_some_and(|change| change.worktrees.contains(&key.worktree_id))
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "bound-only new ref routing omitted {name}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn partial_watcher_recovery_repairs_missed_branch_routing_without_a_census() {
        const TEST: &str = "app::git_ui::tests::partial_watcher_recovery_repairs_missed_branch_routing_without_a_census";
        if !crate::git_watch::tests::fixture_isolated(TEST) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let project = ProjectId::new();
        let cancel = CancellationToken::default();
        let (snapshot, plan) = read_local(
            project,
            fixture(&root),
            git_watch::Change {
                topology: true,
                ..Default::default()
            },
            cancel.clone(),
        )
        .unwrap();
        let old_plan = plan.unwrap();
        let id = snapshot.worktrees[0].0.id;
        let index = fs::read(root.join(".git/index")).unwrap();
        fs::write(root.join(".git/index"), "corrupt index").unwrap();
        let partial = git_watch::Change {
            worktrees: [id].into(),
            untracked: true,
            ..Default::default()
        };
        let (failed, plan) = read_local_with_plan(
            project,
            snapshot,
            partial.clone(),
            cancel.clone(),
            Some(&old_plan),
        )
        .unwrap();
        assert!(failed.status_errors.contains_key(&id));
        assert!(plan.is_none(), "failed row retains prior watch plan");
        fs::write(root.join(".git/index"), index).unwrap();
        git(&root, &["switch", "-c", "learned"]);
        let (recovered, plan) = read_local_with_plan(
            project,
            failed,
            partial.clone(),
            cancel.clone(),
            Some(&old_plan),
        )
        .unwrap();
        assert!(!recovered.status_errors.contains_key(&id));
        assert_eq!(
            recovered.worktrees[0].0.branch.as_deref(),
            Some("refs/heads/learned")
        );
        let plan = plan.expect("a new branch needs a pure routing refresh");
        assert!(plan.matches(
            &recovered.repository.common_dir,
            &[recovered.worktrees[0].0.clone()]
        ));
        let (_, unchanged) =
            read_local_with_plan(project, recovered, partial, cancel, Some(&plan)).unwrap();
        assert!(
            unchanged.is_none(),
            "unchanged partial success reuses existing coverage"
        );
        let monitor = Watch::start().unwrap();
        monitor.configure(vec![Arc::new(plan)]);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if monitor
                .drain()
                .get(&project)
                .is_some_and(|change| change.coverage == Some(true))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "routing configuration was not acknowledged"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        monitor.drain();
        let path = root.join(".git/refs/heads/learned");
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, bytes).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if monitor
                .drain()
                .get(&project)
                .is_some_and(|change| change.worktrees.contains(&id))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "new-branch event omitted the recovered worktree"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn continuous_watcher_progress_does_not_restart_or_starve_the_full_changes_model() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let baseline = fixture(&root);
        let key = ActiveWorktreeKey {
            project_id: ProjectId::new(),
            worktree_id: baseline.worktrees[0].0.id,
            canonical_path: root.clone(),
        };
        let mut app = app_with_status_snapshot(baseline, &key);
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::default();
        for step in 0..5usize {
            let request = 7 + step as u64;
            app.refresh_ui.source_generation = request;
            let read = next_status_read();
            let full = service.snapshot(&root, &cancel).unwrap();
            fs::write(root.join(format!("new-{step}.txt")), "continuous change\n").unwrap();
            let (watch, _) = read_local(
                key.project_id,
                app.refresh_ui.git.entries[&key.project_id].snapshot.clone(),
                git_watch::Change {
                    worktrees: [key.worktree_id].into(),
                    untracked: true,
                    ..Default::default()
                },
                cancel.clone(),
            )
            .unwrap();
            drop(app.update(Message::GitUi(Action::Read {
                project: key.project_id,
                generation: 9,
                result: Ok((watch, None)),
            })));
            drop(app.update(bound_status_message(&key, request, full, read)));
            let model = app.sidebar_model.source_control.as_ref().unwrap();
            assert_eq!(model.data_state, sidebar::SidebarDataState::Ready);
            assert_eq!(
                model
                    .group(crate::repository::ChangeArea::Untracked)
                    .unwrap()
                    .rows()
                    .len(),
                step,
                "each full generation completes and advances despite newer scalar watcher data"
            );
            assert_eq!(
                app.refresh_ui.source_generation, request,
                "no wholesale retry loop"
            );
            assert!(app.refresh_ui.source_cancel.is_none());
            let row = &app.sidebar_model.projects[0].worktrees[0];
            assert_eq!(row.dirty.as_ref().unwrap().untracked, (step + 1) as u32);
        }
    }

    #[test]
    fn unchanged_bound_status_clears_error_and_fences_delayed_watch_without_rebuilding_model() {
        use crate::repository::{
            CommandSpec, ProcessExecutor, ProcessFailure, ProcessOutput, SystemProcessExecutor,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Count(Arc<AtomicUsize>);
        impl ProcessExecutor for Count {
            fn execute(
                &self,
                command: &CommandSpec,
                cancel: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                self.0.fetch_add(1, Ordering::Relaxed);
                SystemProcessExecutor.execute(command, cancel)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let baseline = fixture(&root);
        fs::write(root.join("one.txt"), "one change\n").unwrap();
        let key = ActiveWorktreeKey {
            project_id: ProjectId::new(),
            worktree_id: baseline.worktrees[0].0.id,
            canonical_path: root.clone(),
        };
        let mut app = app_with_status_snapshot(baseline, &key);
        let count = Arc::new(AtomicUsize::new(0));
        let service = RepositoryService::new("git", Arc::new(Count(count.clone())));
        let mut cache = crate::repository::SnapshotCache::default();
        let options = crate::repository::SnapshotOptions::default();
        let cancel = CancellationToken::default();
        let read = next_status_read();
        let full = service
            .snapshot_cached(&root, options, &cancel, &mut cache)
            .unwrap()
            .unwrap();
        drop(app.update(bound_status_message(&key, 7, full, read)));
        let original_rows = app
            .sidebar_model
            .source_control
            .as_ref()
            .unwrap()
            .group(crate::repository::ChangeArea::Untracked)
            .unwrap()
            .rows()
            .as_ptr();
        let mut prior = app.refresh_ui.git.entries[&key.project_id].snapshot.clone();
        prior
            .status_reads
            .insert(key.worktree_id, next_status_read());
        prior.status_updates = [key.worktree_id].into();
        prior
            .status_errors
            .insert(key.worktree_id, "temporary status failure".into());
        drop(app.update(Message::GitUi(Action::Read {
            project: key.project_id,
            generation: 9,
            result: Ok((prior.clone(), None)),
        })));
        assert!(
            app.sidebar_model.projects[0].worktrees[0]
                .status_error
                .is_some()
        );
        let mut delayed = prior;
        delayed
            .status_reads
            .insert(key.worktree_id, next_status_read());
        let read = next_status_read();
        let before = count.load(Ordering::Relaxed);
        assert!(
            service
                .snapshot_cached(&root, options, &cancel, &mut cache)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            count.load(Ordering::Relaxed) - before,
            1,
            "unchanged path still uses only the existing status process"
        );
        let status = cache.row_status().unwrap();
        assert!(
            status.entries.is_empty(),
            "cached proof retains no path listing"
        );
        drop(app.update(Message::SourceControlLoaded {
            key: key.clone(),
            request: 7,
            result: Ok(SourceControlRead {
                prepared: None,
                status: status.clone(),
                head_repository: None,
                read,
            }),
        }));
        drop(app.update(Message::GitUi(Action::Read {
            project: key.project_id,
            generation: 9,
            result: Ok((delayed, None)),
        })));
        let row = &app.sidebar_model.projects[0].worktrees[0];
        assert!(row.status_error.is_none());
        assert_eq!(row.dirty.as_ref().unwrap().untracked, 1);
        assert_eq!(
            app.sidebar_model
                .source_control
                .as_ref()
                .unwrap()
                .group(crate::repository::ChangeArea::Untracked)
                .unwrap()
                .rows()
                .as_ptr(),
            original_rows,
            "unchanged read does not rebuild the Changes rows"
        );
        assert_eq!(app.refresh_ui.source_generation, 7);
    }

    #[test]
    fn bound_panel_invalidation_ignores_unrelated_worktrees_and_projects() {
        let key = ActiveWorktreeKey {
            project_id: ProjectId::new(),
            worktree_id: WorktreeId::new(),
            canonical_path: "/repo".into(),
        };
        let mut change = git_watch::Change {
            worktrees: [key.worktree_id].into(),
            ..Default::default()
        };
        assert_eq!(panel_invalidation(key.project_id, &change, &key), Some(1));
        assert_eq!(panel_invalidation(ProjectId::new(), &change, &key), None);
        change.worktrees = [WorktreeId::new()].into();
        assert_eq!(panel_invalidation(key.project_id, &change, &key), None);
        change.topology = true;
        change.metadata = true;
        assert_eq!(panel_invalidation(key.project_id, &change, &key), Some(3));
    }

    #[test]
    fn unchanged_topology_reuses_watch_plan_and_index_events_do_not_rescan_membership() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let snapshot = fixture(&root);
        let id = snapshot.worktrees[0].0.id;
        let project = ProjectId::new();
        let (_, plan) = read_local(
            project,
            snapshot.clone(),
            git_watch::Change {
                membership: true,
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        let plan = plan.unwrap();
        assert!(
            plan.matches(
                &snapshot.repository.common_dir,
                &snapshot
                    .worktrees
                    .iter()
                    .map(|(record, _)| record.clone())
                    .collect::<Vec<_>>()
            )
        );
        fs::write(root.join("new.txt"), "new\n").unwrap();
        git(&root, &["add", "new.txt"]);
        let (_, next) = read_local_with_plan(
            project,
            snapshot,
            git_watch::Change {
                worktrees: [id].into(),
                metadata: true,
                ..Default::default()
            },
            CancellationToken::default(),
            Some(&plan),
        )
        .unwrap();
        assert!(next.is_none());
    }

    #[test]
    fn broken_worktree_does_not_block_sibling_status_updates() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let linked = directory.path().join("linked");
        let snapshot = fixture(&root);
        git(
            &root,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let project = ProjectId::new();
        let (snapshot, _) = read_local(
            project,
            snapshot,
            git_watch::Change {
                topology: true,
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        let gitdir = RepositoryService::system("git")
            .git_dir(&linked, &CancellationToken::default())
            .unwrap();
        fs::write(gitdir.join("HEAD"), "invalid HEAD\n").unwrap();
        fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        let changed = snapshot
            .worktrees
            .iter()
            .map(|(record, _)| record.id)
            .collect();
        let (next, _) = read_local(
            project,
            snapshot,
            git_watch::Change {
                worktrees: changed,
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(next.worktrees.len(), 2);
        assert!(
            next.worktrees
                .iter()
                .find(|(record, _)| record.path == root)
                .unwrap()
                .1
                .as_ref()
                .unwrap()
                .dirty
                .is_dirty()
        );
    }

    #[test]
    fn local_reads_target_changed_worktrees_and_never_invoke_github() {
        const CHILD: &str = "CAGENTS_LOCAL_GIT_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let executable = directory.path().join("gh");
            fs::write(&executable, "#!/bin/sh\n: > \"$HOME/gh-called\"\nexit 19\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            // Keep the configured Git toolchain while intercepting every gh
            // lookup. The distro Git can be older than our required version.
            let configured_path =
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
            let isolated_path = std::env::join_paths(
                std::iter::once(directory.path().to_path_buf())
                    .chain(std::env::split_paths(&configured_path)),
            )
            .unwrap();
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "app::git_ui::tests::local_reads_target_changed_worktrees_and_never_invoke_github",
                    "--nocapture",
                ])
                .env_clear()
                .env(CHILD, "1")
                .env("HOME", directory.path())
                .env("PATH", isolated_path)
                .status()
                .unwrap();
            assert!(status.success());
            assert!(
                directory.path().join("local-git-tested").exists(),
                "isolated test body did not execute"
            );
            assert!(
                !directory.path().join("gh-called").exists(),
                "a local event invoked gh"
            );
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let mut snapshot = fixture(&first);
        git(
            &first,
            &["worktree", "add", "-b", "second", second.to_str().unwrap()],
        );
        let project = ProjectId::new();
        snapshot = read_local(
            project,
            snapshot,
            git_watch::Change {
                topology: true,
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap()
        .0;
        let first_id = snapshot
            .worktrees
            .iter()
            .find(|(record, _)| record.path == first)
            .unwrap()
            .0
            .id;
        fs::write(first.join("tracked.txt"), "first edit\n").unwrap();
        fs::write(second.join("tracked.txt"), "second edit\n").unwrap();
        let change = git_watch::Change {
            worktrees: [first_id].into(),
            ..Default::default()
        };
        let (updated, plan) = read_local(
            project,
            snapshot,
            change.clone(),
            CancellationToken::default(),
        )
        .unwrap();
        assert!(
            plan.is_none(),
            "ordinary edits must not re-enumerate watch membership"
        );
        for (record, status) in &updated.worktrees {
            assert_eq!(
                status.as_ref().unwrap().dirty.is_dirty(),
                record.id == first_id,
                "only the changed worktree is reread"
            );
        }
        git(&first, &["switch", "-c", "renamed"]);
        let (updated, _) =
            read_local(project, updated, change, CancellationToken::default()).unwrap();
        assert_eq!(
            updated
                .worktrees
                .iter()
                .find(|(record, _)| record.id == first_id)
                .unwrap()
                .0
                .branch
                .as_deref(),
            Some("refs/heads/renamed")
        );
        git(
            &first,
            &["worktree", "remove", "--force", second.to_str().unwrap()],
        );
        let (updated, _) = read_local(
            project,
            updated,
            git_watch::Change {
                topology: true,
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(updated.worktrees.len(), 1);
        assert_eq!(updated.worktrees[0].0.id, first_id);
        fs::write(
            PathBuf::from(std::env::var_os("HOME").unwrap()).join("local-git-tested"),
            "passed",
        )
        .unwrap();
    }

    #[test]
    fn local_apply_preserves_project_and_pending_row_state() {
        let directory = tempfile::tempdir().unwrap();
        let mut snapshot = fixture(&directory.path().join("repo"));
        let (record, status) = &snapshot.worktrees[0];
        let mut row = SidebarWorktree::from_repository(record, status.as_ref());
        row.deletion = Some(sidebar::RowDeletion::Deleting);
        let mut project = SidebarProject::new(
            ProjectId::new(),
            snapshot.repository.id,
            "Stable project".into(),
            snapshot.repository.common_dir.clone(),
            vec![row],
            Vec::new(),
        );
        project.github_status = "Remote authority unchanged".into();
        project.github_ready = true;
        snapshot.worktrees[0].0.branch = Some("refs/heads/changed".into());
        apply_local(&mut project, &snapshot);
        assert_eq!(project.name, "Stable project");
        assert_eq!(project.github_status, "Remote authority unchanged");
        assert!(project.github_ready);
        assert_eq!(
            project.worktrees[0].deletion,
            Some(sidebar::RowDeletion::Deleting)
        );
        assert_eq!(project.worktrees[0].branch.as_deref(), Some("changed"));
    }

    #[test]
    fn a_worktree_gone_from_the_listing_is_closed_unless_the_application_deleted_it() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let snapshot = fixture(&first);
        for name in ["outside", "inside"] {
            let path = directory.path().join(name);
            git(
                &first,
                &["worktree", "add", "-b", name, path.to_str().unwrap()],
            );
        }
        let project = ProjectId::new();
        let read = |snapshot| {
            read_local(
                project,
                snapshot,
                git_watch::Change {
                    topology: true,
                    ..Default::default()
                },
                CancellationToken::default(),
            )
            .unwrap()
            .0
        };
        let previous = read(snapshot);
        let inside = previous
            .worktrees
            .iter()
            .find(|(record, _)| record.path.ends_with("inside"))
            .unwrap()
            .0
            .id;
        let mut state = State::default();
        state.entries.insert(
            project,
            Entry {
                snapshot: previous.clone(),
                generation: 1,
                plan: None,
                archived: Vec::new(),
                remote_started: false,
                cancellation: None,
                read_failure: None,
                read_full_scan: false,
                catalog_requested: false,
                pending: Default::default(),
                fallback: None,
                last_full_scan: Instant::now(),
                failed_status_attempts: HashMap::new(),
            },
        );
        for name in ["outside", "inside"] {
            git(&first, &["worktree", "remove", "--force", name]);
        }
        state.forget_worktree(project, inside);
        let listed = read(previous).worktrees;
        assert_eq!(listed.len(), 1);
        let closed = records_removed_outside(project, &state.entries[&project].snapshot, &listed);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].project_id, project);
        assert_eq!(closed[0].path, directory.path().join("outside"));
        assert_eq!(closed[0].branch.as_deref(), Some("outside"));
        assert!(!closed[0].existence.is_open());
    }

    #[test]
    fn stale_local_completion_cannot_resurrect_a_closed_project() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = fixture(&directory.path().join("repo"));
        let mut app = super::super::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        app.refresh_ui.git.entries.insert(
            project,
            Entry {
                snapshot: snapshot.clone(),
                generation: 2,
                plan: None,
                archived: Vec::new(),
                remote_started: false,
                cancellation: None,
                read_failure: None,
                read_full_scan: false,
                catalog_requested: false,
                pending: Default::default(),
                fallback: None,
                last_full_scan: Instant::now(),
                failed_status_attempts: HashMap::new(),
            },
        );
        app.repository_generations.insert(project, 2);
        app.refresh_ui.git.forget(project);
        drop(app.update_git(Action::Read {
            project,
            generation: 2,
            result: Ok((snapshot, None)),
        }));
        assert!(!app.refresh_ui.git.entries.contains_key(&project));
        assert!(app.sidebar_model.project(project).is_none());
    }
    #[test]
    fn failed_row_recovery_is_paced_per_row_and_not_postponed_by_failed_siblings() {
        fn run(task: Task<Message>) -> Message {
            use cosmic::iced::futures::StreamExt;
            let mut stream =
                cosmic::iced::runtime::task::into_stream(task).expect("retry worker task");
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    while let Some(action) = stream.next().await {
                        if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(message)) =
                            action
                        {
                            return message;
                        }
                    }
                    panic!("retry worker did not publish a result");
                })
        }
        let directory = tempfile::tempdir().unwrap();
        let primary = directory.path().join("primary");
        let initial = fixture(&primary);
        let linked_b = directory.path().join("linked-b");
        let linked_c = directory.path().join("linked-c");
        for (name, path) in [("b", &linked_b), ("c", &linked_c)] {
            git(
                &primary,
                &["worktree", "add", "-b", name, path.to_str().unwrap()],
            );
        }
        let project = ProjectId::new();
        let cancel = CancellationToken::default();
        let (baseline, _) = read_local(
            project,
            initial,
            git_watch::Change {
                topology: true,
                ..Default::default()
            },
            cancel.clone(),
        )
        .unwrap();
        let id = |path: &Path| {
            baseline
                .worktrees
                .iter()
                .find(|(record, _)| record.path == path)
                .unwrap()
                .0
                .id
        };
        let (a, b, c) = (id(&primary), id(&linked_b), id(&linked_c));
        let git = RepositoryService::system("git");
        let private_b = git.git_dir(&linked_b, &cancel).unwrap();
        let private_c = git.git_dir(&linked_c, &cancel).unwrap();
        let index_c = fs::read(private_c.join("index")).unwrap();
        for private in [&private_b, &private_c] {
            fs::write(private.join("index"), "broken index").unwrap();
        }
        let (failed, _) = read_local(
            project,
            baseline.clone(),
            git_watch::Change {
                worktrees: [b, c].into(),
                ..Default::default()
            },
            cancel.clone(),
        )
        .unwrap();
        let key = ActiveWorktreeKey {
            project_id: project,
            worktree_id: a,
            canonical_path: primary,
        };
        let mut app = app_with_status_snapshot(baseline, &key);
        drop(app.update(Message::GitUi(Action::Read {
            project,
            generation: 9,
            result: Ok((failed, None)),
        })));
        assert_eq!(
            app.refresh_ui.git.entries[&project]
                .failed_status_attempts
                .len(),
            2
        );
        let due = Instant::now() - FALLBACK_INTERVAL;
        app.refresh_ui
            .git
            .entries
            .get_mut(&project)
            .unwrap()
            .failed_status_attempts
            .insert(c, due);
        let (healthy, _) = read_local(
            project,
            app.refresh_ui.git.entries[&project].snapshot.clone(),
            git_watch::Change {
                worktrees: [a].into(),
                ..Default::default()
            },
            cancel.clone(),
        )
        .unwrap();
        drop(app.update(Message::GitUi(Action::Read {
            project,
            generation: 9,
            result: Ok((healthy, None)),
        })));
        assert_eq!(
            app.refresh_ui.git.entries[&project].failed_status_attempts[&c], due,
            "healthy A cannot postpone C"
        );
        // Continuing failed B traffic must not move C's independent deadline.
        for _ in 0..3 {
            app.refresh_ui.git.changed(
                project,
                git_watch::Change {
                    worktrees: [b].into(),
                    ..Default::default()
                },
            );
            let message = run(app.start_git_reads());
            drop(app.update(message));
            assert_eq!(
                app.refresh_ui.git.entries[&project].failed_status_attempts[&c], due,
                "failed B cannot postpone C"
            );
        }
        let message = run(app.git_fallback_tick());
        let Message::GitUi(Action::Read {
            result: Ok((snapshot, _)),
            ..
        }) = &message
        else {
            panic!("wrong retry result");
        };
        assert_eq!(
            snapshot.status_updates,
            HashSet::from([c]),
            "only C is due; no B or healthy A process"
        );
        assert!(snapshot.status_errors.contains_key(&c));
        assert!(!app.refresh_ui.git.entries[&project].read_full_scan);
        drop(app.update(message));
        use cosmic::iced::futures::StreamExt;
        let immediate = cosmic::iced::runtime::task::into_stream(app.git_fallback_tick());
        let action = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                match immediate {
                    Some(mut stream) => stream.next().await,
                    None => None,
                }
            });
        assert!(action.is_none(), "failure cannot retry before its interval");
        fs::write(private_c.join("index"), index_c).unwrap();
        fs::write(
            linked_c.join("new.txt"),
            "repaired without delivering its event\n",
        )
        .unwrap();
        app.refresh_ui
            .git
            .entries
            .get_mut(&project)
            .unwrap()
            .failed_status_attempts
            .insert(c, due);
        for _ in 0..3 {
            app.refresh_ui.git.changed(
                project,
                git_watch::Change {
                    worktrees: [b].into(),
                    ..Default::default()
                },
            );
            let message = run(app.start_git_reads());
            drop(app.update(message));
            assert_eq!(
                app.refresh_ui.git.entries[&project].failed_status_attempts[&c],
                due
            );
        }
        let message = run(app.git_fallback_tick());
        let Message::GitUi(Action::Read {
            result: Ok((snapshot, _)),
            ..
        }) = &message
        else {
            panic!("wrong repaired retry result");
        };
        assert_eq!(snapshot.status_updates, HashSet::from([c]));
        assert!(!snapshot.status_errors.contains_key(&c));
        assert!(snapshot.status_errors.contains_key(&b));
        drop(app.update(message));
        let entry = &app.refresh_ui.git.entries[&project];
        assert!(!entry.failed_status_attempts.contains_key(&c));
        assert!(entry.failed_status_attempts.contains_key(&b));
        assert_eq!(
            app.sidebar_model.projects[0]
                .worktrees
                .iter()
                .find(|row| row.id == c)
                .unwrap()
                .dirty
                .as_ref()
                .unwrap()
                .untracked,
            1
        );
        assert!(
            app.refresh_ui.projects.is_empty(),
            "row recovery never schedules GitHub"
        );
    }

    fn dense_display_rows(
        worktrees: usize,
        entries: usize,
    ) -> Vec<(WorktreeRecord, Option<StatusSnapshot>)> {
        let mut porcelain = b"# branch.oid (initial)\0# branch.head main\0".to_vec();
        for index in 0..entries {
            porcelain.extend_from_slice(format!("? generated/file-{index:05}.txt\0").as_bytes());
        }
        let status = crate::repository::parse_status(&porcelain).unwrap();
        (0..worktrees)
            .map(|index| {
                (
                    WorktreeRecord {
                        id: WorktreeId::new(),
                        path: PathBuf::from(format!("/synthetic/sidebar/worktree-{index}")),
                        created_at_unix_ms: None,
                        head_oid: None,
                        branch: Some("refs/heads/main".into()),
                        detached: false,
                        bare: false,
                        lock: Default::default(),
                        prunable_reason: None,
                    },
                    Some(status.clone()),
                )
            })
            .collect()
    }

    #[test]
    fn sidebar_status_projection_drops_dense_path_storage_before_ui_clones() {
        let mut source = dense_display_rows(1, 40_000);
        let mut record = source[0].0.clone();
        record.id = WorktreeId::new();
        record.path = PathBuf::from("/synthetic/sidebar/unread");
        source.push((record, None));
        let projected = display_worktrees(&source);
        let original = source[0].1.as_ref().unwrap();
        let summary = projected[0].1.as_ref().unwrap();
        assert_eq!(original.entries.len(), 40_000);
        assert!(original.entries.capacity() >= 40_000);
        assert_eq!(summary.entries.capacity(), 0);
        assert_eq!(summary.dirty.untracked, 40_000);
        assert_eq!(summary.head, original.head);
        assert_eq!(summary.tracking, original.tracking);
        assert_eq!(summary.untracked_omitted, original.untracked_omitted);
        assert!(projected[1].1.is_none());
        let cloned = projected.clone();
        assert!(cloned.iter().all(|(_, status)| {
            status
                .as_ref()
                .is_none_or(|status| status.entries.capacity() == 0)
        }));
        assert_eq!(
            source[0].1.as_ref().unwrap().entries.len(),
            40_000,
            "the independent full listing remains available to its owner"
        );
    }

    #[test]
    fn initial_and_watcher_sidebar_statuses_keep_counts_without_retaining_paths() {
        const TEST: &str = "app::git_ui::tests::initial_and_watcher_sidebar_statuses_keep_counts_without_retaining_paths";
        if !crate::git_watch::tests::fixture_isolated(TEST) {
            return;
        }
        let files = tempfile::tempdir().unwrap();
        let root = files.path().join("repo");
        let baseline = fixture(&root);
        let id = baseline.worktrees[0].0.id;
        for index in 0..200 {
            fs::write(root.join(format!("untracked-{index}.txt")), "new\n").unwrap();
        }
        let cancel = CancellationToken::new();
        let full = RepositoryService::system("git")
            .status(&root, &cancel)
            .unwrap();
        assert_eq!(
            full.entries.len(),
            200,
            "fresh safety reads keep their complete listing"
        );
        let loaded = super::super::load_repository(&root, None, cancel.clone()).unwrap();
        let initial = loaded.worktrees[0].1.as_ref().unwrap();
        assert_eq!(initial.dirty, full.dirty);
        assert_eq!(initial.untracked_omitted, full.untracked_omitted);
        assert_eq!(
            initial.entries.capacity(),
            0,
            "initial publication is already compact"
        );
        let (read, _) = read_local(
            ProjectId::new(),
            baseline,
            git_watch::Change {
                worktrees: [id].into(),
                untracked: true,
                ..Default::default()
            },
            cancel,
        )
        .unwrap();
        let watched = read.worktrees[0].1.as_ref().unwrap();
        assert_eq!(watched.dirty, full.dirty);
        assert_eq!(watched.head, full.head);
        assert_eq!(watched.tracking, full.tracking);
        assert_eq!(watched.untracked_omitted, full.untracked_omitted);
        assert_eq!(
            watched.entries.capacity(),
            0,
            "watcher publication drops file paths on its worker"
        );
        assert_eq!(full.entries.len(), 200);
    }

    #[test]
    fn duplicate_repository_load_preserves_new_linked_row_status_error_detail() {
        const TEST: &str = "app::git_ui::tests::duplicate_repository_load_preserves_new_linked_row_status_error_detail";
        if !crate::git_watch::tests::fixture_isolated(TEST) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repository");
        let baseline = fixture(&root);
        let key = ActiveWorktreeKey {
            project_id: ProjectId::new(),
            worktree_id: baseline.worktrees[0].0.id,
            canonical_path: root.clone(),
        };
        let mut app = app_with_status_snapshot(baseline, &key);
        let original = app.sidebar_model.projects[0].worktrees[0].clone();
        let linked = directory.path().join("linked");
        git(
            &root,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let binding = RepositoryService::system("git")
            .discover(&linked, &CancellationToken::default())
            .unwrap();
        fs::write(binding.git_dir.join("index"), "broken index").unwrap();
        let provisional = ProjectId::new();
        app.repository_generations.insert(provisional, 20);
        let loaded = super::super::tests::task_app_message(app.repository_read_task(
            linked.clone(),
            provisional,
            20,
            false,
            Some(ProjectKind::Git),
        ));
        let (id, expected_error) = match &loaded {
            Message::RepositoryLoaded {
                result: Ok(loaded), ..
            } => {
                let id = loaded
                    .worktrees
                    .iter()
                    .find(|(record, _)| record.path == linked)
                    .unwrap()
                    .0
                    .id;
                (id, loaded.status_order.errors[&id].clone())
            }
            _ => panic!("actual duplicate loader did not complete"),
        };
        drop(app.update(loaded));
        assert_eq!(app.sidebar_model.projects.len(), 1);
        let project = &app.sidebar_model.projects[0];
        assert_eq!(project.id, key.project_id);
        assert_eq!(
            project
                .worktrees
                .iter()
                .find(|row| row.id == key.worktree_id)
                .unwrap(),
            &original
        );
        let row = project.worktrees.iter().find(|row| row.id == id).unwrap();
        assert_eq!(row.status_error.as_deref(), Some(expected_error.as_str()));
        assert!(row.status_tooltip_for_test().contains(&expected_error));
        let entry = &app.refresh_ui.git.entries[&key.project_id];
        assert_eq!(
            entry.generation, 9,
            "duplicate load does not replace the existing watcher owner"
        );
        assert_eq!(entry.snapshot.worktrees.len(), 1);
        assert!(!app.repository_generations.contains_key(&provisional));
    }

    #[test]
    fn repository_reload_and_bound_tasks_publish_the_newest_sample_in_either_delivery_order() {
        const TEST: &str = "app::git_ui::tests::repository_reload_and_bound_tasks_publish_the_newest_sample_in_either_delivery_order";
        if !crate::git_watch::tests::fixture_isolated(TEST) {
            return;
        }
        for no_entry in [false, true] {
            for bound_sample_first in [false, true] {
                for bound_delivered_first in [false, true] {
                    let directory = tempfile::tempdir().unwrap();
                    let root = directory.path().join("repository");
                    let baseline = fixture(&root);
                    let key = ActiveWorktreeKey {
                        project_id: ProjectId::new(),
                        worktree_id: baseline.worktrees[0].0.id,
                        canonical_path: root.clone(),
                    };
                    let mut app = app_with_status_snapshot(baseline, &key);
                    app.active_workspace = Some(root.clone());
                    app.sidebar_state.sync_active_worktree(
                        &app.sidebar_model,
                        &SidebarFocus::Repository {
                            worktree: root.clone(),
                        },
                    );
                    if no_entry {
                        app.refresh_ui.git.forget(key.project_id);
                    }
                    app.repository_generations.insert(key.project_id, 10);
                    // This is the production task, including its read-start
                    // cache retirement. Hold its actual callback during the
                    // other worker and then exercise both delivery orders.
                    let load = app.repository_read_task(
                        root.clone(),
                        key.project_id,
                        10,
                        false,
                        Some(ProjectKind::Git),
                    );
                    assert!(!app.refresh_ui.git.entries.contains_key(&key.project_id));
                    let mut load = Some(load);
                    let first = if bound_sample_first {
                        super::super::tests::task_app_message(
                            app.source_control_snapshot_task(key.clone()),
                        )
                    } else {
                        super::super::tests::task_app_message(load.take().unwrap())
                    };
                    git(&root, &["checkout", "-b", "learned"]);
                    fs::write(root.join("new.txt"), "new\n").unwrap();
                    let second = if bound_sample_first {
                        super::super::tests::task_app_message(load.take().unwrap())
                    } else {
                        super::super::tests::task_app_message(
                            app.source_control_snapshot_task(key.clone()),
                        )
                    };
                    let (bound, loaded) = if bound_sample_first {
                        (first, second)
                    } else {
                        (second, first)
                    };
                    assert!(matches!(
                        &bound,
                        Message::SourceControlLoaded { result: Ok(_), .. }
                    ));
                    assert!(matches!(
                        &loaded,
                        Message::RepositoryLoaded { result: Ok(_), .. }
                    ));
                    for message in if bound_delivered_first {
                        [bound, loaded]
                    } else {
                        [loaded, bound]
                    } {
                        drop(app.update(message));
                    }
                    let row = &app.sidebar_model.projects[0].worktrees[0];
                    assert_eq!(row.branch.as_deref(), Some("learned"));
                    assert_eq!(row.dirty.as_ref().unwrap().untracked, 1);
                    assert!(row.status_error.is_none());
                    let entry = &app.refresh_ui.git.entries[&key.project_id];
                    assert_eq!(
                        entry.snapshot.worktrees[0].0.branch.as_deref(),
                        Some("refs/heads/learned")
                    );
                    assert_eq!(
                        entry.snapshot.worktrees[0]
                            .1
                            .as_ref()
                            .unwrap()
                            .dirty
                            .untracked,
                        1
                    );
                    assert!(entry.snapshot.status_reads[&key.worktree_id] > 0);
                    assert!(!app.refresh_ui.git.reloads.contains_key(&key.project_id));
                }
            }
        }
    }

    #[test]
    fn repository_reload_keeps_listed_lock_metadata_and_newer_recovered_checkout() {
        const TEST: &str = "app::git_ui::tests::repository_reload_keeps_listed_lock_metadata_and_newer_recovered_checkout";
        if !crate::git_watch::tests::fixture_isolated(TEST) {
            return;
        }
        for omitted in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("repository");
            let baseline = fixture(&root);
            let linked = directory.path().join("linked");
            git(
                &root,
                &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
            );
            let project = ProjectId::new();
            let (baseline, _) = read_local(
                project,
                baseline,
                git_watch::Change {
                    membership: true,
                    topology: true,
                    untracked: true,
                    ..Default::default()
                },
                CancellationToken::default(),
            )
            .unwrap();
            let id = baseline
                .worktrees
                .iter()
                .find(|(record, _)| record.path == linked)
                .unwrap()
                .0
                .id;
            let key = ActiveWorktreeKey {
                project_id: project,
                worktree_id: id,
                canonical_path: linked.clone(),
            };
            let mut app = app_with_status_snapshot(baseline, &key);
            app.active_workspace = Some(linked.clone());
            app.sidebar_state.sync_active_worktree(
                &app.sidebar_model,
                &SidebarFocus::Repository {
                    worktree: linked.clone(),
                },
            );
            let hidden = directory.path().join("held-linked");
            if omitted {
                fs::rename(&linked, &hidden).unwrap();
            } else {
                git(
                    &root,
                    &[
                        "worktree",
                        "lock",
                        "--reason",
                        "fresh lock",
                        linked.to_str().unwrap(),
                    ],
                );
            }
            app.repository_generations.insert(project, 10);
            let loaded = super::super::tests::task_app_message(app.repository_read_task(
                root,
                project,
                10,
                false,
                Some(ProjectKind::Git),
            ));
            if omitted {
                fs::rename(hidden, &linked).unwrap();
            }
            fs::write(linked.join("new.txt"), "new\n").unwrap();
            let bound = super::super::tests::task_app_message(
                app.source_control_snapshot_task(key.clone()),
            );
            assert!(matches!(
                &bound,
                Message::SourceControlLoaded { result: Ok(_), .. }
            ));
            drop(app.update(bound));
            drop(app.update(loaded));
            let row = app.sidebar_model.projects[0]
                .worktrees
                .iter()
                .find(|row| row.id == id)
                .unwrap();
            assert_eq!(row.dirty.as_ref().unwrap().untracked, 1);
            assert_eq!(row.locked, !omitted);
            let entry = &app.refresh_ui.git.entries[&project];
            let (record, status) = entry
                .snapshot
                .worktrees
                .iter()
                .find(|(record, _)| record.id == id)
                .unwrap();
            assert_eq!(record.lock.locked, !omitted);
            if !omitted {
                assert_eq!(record.lock.reason.as_deref(), Some("fresh lock"));
            }
            assert_eq!(status.as_ref().unwrap().dirty.untracked, 1);
        }
    }

    #[test]
    fn repository_reload_preserves_newer_status_failures_and_clears_older_failures() {
        const TEST: &str = "app::git_ui::tests::repository_reload_preserves_newer_status_failures_and_clears_older_failures";
        if !crate::git_watch::tests::fixture_isolated(TEST) {
            return;
        }
        for newer_failure in [false, true] {
            for bound_sample_first in [false, true] {
                for bound_first in [false, true] {
                    let directory = tempfile::tempdir().unwrap();
                    let root = directory.path().join("repository");
                    let baseline = fixture(&root);
                    let key = ActiveWorktreeKey {
                        project_id: ProjectId::new(),
                        worktree_id: baseline.worktrees[0].0.id,
                        canonical_path: root.clone(),
                    };
                    let mut app = app_with_status_snapshot(baseline, &key);
                    app.active_workspace = Some(root.clone());
                    app.sidebar_state.sync_active_worktree(
                        &app.sidebar_model,
                        &SidebarFocus::Repository {
                            worktree: root.clone(),
                        },
                    );
                    let index = root.join(".git/index");
                    let original = fs::read(&index).unwrap();
                    if !newer_failure {
                        fs::write(&index, "broken index").unwrap();
                    }
                    app.repository_generations.insert(key.project_id, 10);
                    let mut load = Some(app.repository_read_task(
                        root.clone(),
                        key.project_id,
                        10,
                        false,
                        Some(ProjectKind::Git),
                    ));
                    let first = if bound_sample_first {
                        super::super::tests::task_app_message(
                            app.source_control_snapshot_task(key.clone()),
                        )
                    } else {
                        super::super::tests::task_app_message(load.take().unwrap())
                    };
                    if newer_failure {
                        fs::write(&index, "broken index").unwrap();
                    } else {
                        fs::write(&index, original).unwrap();
                        fs::write(root.join("new.txt"), "new\n").unwrap();
                    }
                    let second = if bound_sample_first {
                        super::super::tests::task_app_message(load.take().unwrap())
                    } else {
                        super::super::tests::task_app_message(
                            app.source_control_snapshot_task(key.clone()),
                        )
                    };
                    let (bound, loaded) = if bound_sample_first {
                        (first, second)
                    } else {
                        (second, first)
                    };
                    let bound_failed = if bound_sample_first {
                        !newer_failure
                    } else {
                        newer_failure
                    };
                    if bound_failed {
                        assert!(
                            matches!(&bound, Message::SourceControlLoaded { result: Err(failure), .. } if failure.status_read.is_some())
                        );
                    } else {
                        assert!(matches!(
                            &bound,
                            Message::SourceControlLoaded { result: Ok(_), .. }
                        ));
                    }
                    for message in if bound_first {
                        [bound, loaded]
                    } else {
                        [loaded, bound]
                    } {
                        drop(app.update(message));
                    }
                    let row = &app.sidebar_model.projects[0].worktrees[0];
                    assert_eq!(row.status_error.is_some(), newer_failure);
                    assert!(
                        row.dirty.is_some(),
                        "failure retains last-known scalar counts"
                    );
                    let entry = &app.refresh_ui.git.entries[&key.project_id];
                    assert_eq!(
                        entry.snapshot.status_errors.contains_key(&key.worktree_id),
                        newer_failure
                    );
                    assert_eq!(
                        entry.failed_status_attempts.contains_key(&key.worktree_id),
                        newer_failure
                    );
                    if !newer_failure {
                        assert_eq!(row.dirty.as_ref().unwrap().untracked, 1);
                    }
                }
            }
        }
    }

    #[test]
    fn bound_status_failure_marks_last_known_and_retries_without_a_watch_event() {
        const TEST: &str = "app::git_ui::tests::bound_status_failure_marks_last_known_and_retries_without_a_watch_event";
        if !crate::git_watch::tests::fixture_isolated(TEST) {
            return;
        }
        for delayed in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("repository");
            let baseline = fixture(&root);
            let key = ActiveWorktreeKey {
                project_id: ProjectId::new(),
                worktree_id: baseline.worktrees[0].0.id,
                canonical_path: root.clone(),
            };
            let mut app = app_with_status_snapshot(baseline, &key);
            let first = super::super::tests::task_app_message(
                app.source_control_snapshot_task(key.clone()),
            );
            assert!(matches!(
                &first,
                Message::SourceControlLoaded { result: Ok(_), .. }
            ));
            drop(app.update(first));
            drop(app.update(Message::SourceControlLoaded {
                key: key.clone(),
                request: app.refresh_ui.source_generation,
                result: Err("file enrichment failed after a successful status".into()),
            }));
            assert!(
                app.sidebar_model.projects[0].worktrees[0]
                    .status_error
                    .is_none(),
                "generic panel errors are not status failures"
            );
            let index = root.join(".git/index");
            let original = fs::read(&index).unwrap();
            fs::write(&index, "broken index").unwrap();
            // Deliberately do not inject a watcher event: the bound fallback
            // must make this actual failed status attempt visible itself.
            let failed = super::super::tests::task_app_message(
                app.source_control_snapshot_task(key.clone()),
            );
            assert!(
                matches!(&failed, Message::SourceControlLoaded { result: Err(failure), .. } if failure.status_read.is_some())
            );
            fs::write(&index, original).unwrap();
            fs::write(root.join("new.txt"), "new\n").unwrap();
            if delayed {
                let previous = app.refresh_ui.git.entries[&key.project_id].snapshot.clone();
                let recovered = read_local(
                    key.project_id,
                    previous,
                    git_watch::Change {
                        worktrees: [key.worktree_id].into(),
                        untracked: true,
                        ..Default::default()
                    },
                    CancellationToken::new(),
                )
                .unwrap();
                drop(app.update(Message::GitUi(Action::Read {
                    project: key.project_id,
                    generation: 9,
                    result: Ok(recovered),
                })));
                drop(app.update(failed));
            } else {
                drop(app.update(failed.clone()));
                let row = &app.sidebar_model.projects[0].worktrees[0];
                assert!(row.status_error.is_some());
                assert!(row.status_tooltip_for_test().contains("Last known:"));
                let entry = app.refresh_ui.git.entries.get_mut(&key.project_id).unwrap();
                assert!(entry.snapshot.status_errors.contains_key(&key.worktree_id));
                assert!(entry.failed_status_attempts.contains_key(&key.worktree_id));
                entry.cancellation = None;
                entry
                    .failed_status_attempts
                    .insert(key.worktree_id, Instant::now() - FALLBACK_INTERVAL);
                app.refresh_ui.source_last_refresh = Instant::now();
                let retry = super::super::tests::task_app_message(app.git_fallback_tick());
                assert!(
                    matches!(&retry, Message::GitUi(Action::Read { result: Ok((snapshot, _)), .. }) if snapshot.status_updates == HashSet::from([key.worktree_id]))
                );
                drop(app.update(retry));
                // An older duplicate failure cannot make the successful retry
                // unavailable again or reset its per-row recovery deadline.
                drop(app.update(failed));
            }
            let row = &app.sidebar_model.projects[0].worktrees[0];
            assert!(row.status_error.is_none());
            assert_eq!(row.dirty.as_ref().unwrap().untracked, 1);
            let entry = &app.refresh_ui.git.entries[&key.project_id];
            assert!(!entry.snapshot.status_errors.contains_key(&key.worktree_id));
            assert!(!entry.failed_status_attempts.contains_key(&key.worktree_id));
        }
    }

    #[test]
    fn fallback_local_reads_are_bounded_and_do_not_schedule_remote_reads() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = fixture(&directory.path().join("repo"));
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.sidebar_model.projects.clear();
        for _ in 0..4 {
            let project = ProjectId::new();
            app.sidebar_model.projects.push(SidebarProject::new(
                project,
                snapshot.repository.id,
                "Fallback".into(),
                snapshot.repository.common_dir.clone(),
                Vec::new(),
                Vec::new(),
            ));
            app.repository_generations.insert(project, 1);
            app.refresh_ui.git.entries.insert(
                project,
                Entry {
                    snapshot: snapshot.clone(),
                    generation: 1,
                    plan: None,
                    archived: Vec::new(),
                    remote_started: false,
                    cancellation: None,
                    read_failure: None,
                    read_full_scan: false,
                    catalog_requested: false,
                    pending: Default::default(),
                    fallback: Some("watch limit".into()),
                    last_full_scan: Instant::now() - FALLBACK_INTERVAL,
                    failed_status_attempts: HashMap::new(),
                },
            );
        }
        drop(app.git_fallback_tick());
        assert_eq!(
            app.refresh_ui
                .git
                .entries
                .values()
                .filter(|entry| entry.cancellation.is_some())
                .count(),
            local_workers().min(4)
        );
        assert_eq!(
            app.refresh_ui.git.queue.len(),
            4_usize.saturating_sub(local_workers())
        );
        assert!(
            app.refresh_ui.projects.is_empty(),
            "fallback never schedules gh"
        );
    }

    #[test]
    fn fallback_rescans_the_worktree_list_while_watched_edits_keep_arriving() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = fixture(&directory.path().join("repo"));
        let worktree = snapshot.worktrees[0].0.id;
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.sidebar_model.projects.clear();
        let project = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            snapshot.repository.id,
            "Partly watched".into(),
            snapshot.repository.common_dir.clone(),
            Vec::new(),
            Vec::new(),
        ));
        app.repository_generations.insert(project, 1);
        app.refresh_ui.git.entries.insert(
            project,
            Entry {
                snapshot: snapshot.clone(),
                generation: 1,
                plan: None,
                archived: Vec::new(),
                remote_started: false,
                cancellation: None,
                read_failure: None,
                read_full_scan: false,
                catalog_requested: false,
                pending: Default::default(),
                fallback: Some("directory watch limit reached".into()),
                last_full_scan: Instant::now() - FALLBACK_INTERVAL,
                failed_status_attempts: HashMap::new(),
            },
        );
        // An edit in a directory that is still watched is read on its own.
        app.refresh_ui.git.changed(
            project,
            git_watch::Change {
                worktrees: [worktree].into(),
                ..Default::default()
            },
        );
        drop(app.start_git_reads());
        assert!(!app.refresh_ui.git.entries[&project].read_full_scan);
        drop(app.update_git(Action::Read {
            project,
            generation: 1,
            result: Ok((snapshot, None)),
        }));
        drop(app.git_fallback_tick());
        let entry = &app.refresh_ui.git.entries[&project];
        assert!(
            entry.cancellation.is_some() && entry.read_full_scan,
            "a targeted read must not stand in for the overdue worktree scan"
        );
        assert!(entry.last_full_scan.elapsed() < FALLBACK_INTERVAL);
    }

    #[test]
    fn a_removed_worktree_leaves_without_failing_its_siblings() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let snapshot = fixture(&first);
        git(
            &first,
            &["worktree", "add", "-b", "second", second.to_str().unwrap()],
        );
        let project = ProjectId::new();
        let (snapshot, _) = read_local(
            project,
            snapshot,
            git_watch::Change {
                topology: true,
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(snapshot.worktrees.len(), 2);
        let ids = snapshot
            .worktrees
            .iter()
            .map(|(record, _)| record.id)
            .collect();
        // Removed outside the application; Git still lists it as prunable.
        fs::remove_dir_all(&second).unwrap();
        let (updated, plan) = read_local(
            project,
            snapshot,
            git_watch::Change {
                worktrees: ids,
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(
            updated
                .worktrees
                .iter()
                .map(|(record, _)| record.path.as_path())
                .collect::<Vec<_>>(),
            [first.as_path()]
        );
        assert!(updated.worktrees[0].1.is_some());
        assert_eq!(
            plan.expect("watches follow the surviving worktrees")
                .worktrees
                .len(),
            1
        );
        // `git worktree remove` takes `.git` first; a large checkout is still
        // on disk, and still listed, while its files go.
        let third = directory.path().join("third");
        git(
            &first,
            &["worktree", "add", "-b", "third", third.to_str().unwrap()],
        );
        fs::remove_file(third.join(".git")).unwrap();
        let (updated, _) = read_local(
            project,
            updated,
            git_watch::Change {
                topology: true,
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(updated.worktrees.len(), 1);
        assert_eq!(updated.worktrees[0].0.path, first);
    }

    #[test]
    fn local_edit_retains_archived_pr_authority_but_checkout_to_other_fork_drops_it() {
        let directory = tempfile::tempdir().unwrap();
        let mut snapshot = fixture(&directory.path().join("repo"));
        let project = ProjectId::new();
        let id = snapshot.worktrees[0].0.id;
        let key = GitHubRepoKey::new("github.com", None, "acme", "repo").unwrap();
        snapshot.heads.insert(id, key.clone());
        let pr: crate::github::PullRequest = serde_json::from_value(serde_json::json!({
            "number":7,"title":"Archived feature","state":"MERGED","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"acme"},"headRefName":"main","headRefOid":"older-commit","baseRefName":"main","updatedAt":"2026-01-01T00:00:00Z","url":"https://github.com/acme/repo/pull/7"
        })).unwrap();
        let (record, status) = &snapshot.worktrees[0];
        let mut row = SidebarWorktree::from_repository(record, status.as_ref());
        row.head_repository = Some(key.clone());
        let mut sidebar_project = SidebarProject::new(
            project,
            snapshot.repository.id,
            "Project".into(),
            snapshot.repository.common_dir.clone(),
            vec![row],
            Vec::new(),
        );
        sidebar_project.github_ready = true;
        sidebar_project.worktrees[0].set_pull_request(SidebarPullRequest::from_github(
            project,
            &pr,
            &pr.check_summaries(),
        ));
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.sidebar_model.projects.push(sidebar_project);
        app.repository_generations.insert(project, 2);
        app.github_availability.insert(
            project,
            GitHubAvailability::Ready {
                repository_key: key,
                pull_requests: Vec::new(),
                refreshed_at: 0,
            },
        );
        app.refresh_ui.git.entries.insert(
            project,
            Entry {
                snapshot: snapshot.clone(),
                generation: 2,
                plan: None,
                archived: vec![pr],
                remote_started: false,
                cancellation: None,
                read_failure: None,
                read_full_scan: false,
                catalog_requested: false,
                pending: Default::default(),
                fallback: None,
                last_full_scan: Instant::now(),
                failed_status_attempts: HashMap::new(),
            },
        );
        snapshot.worktrees[0].1.as_mut().unwrap().dirty.unstaged = 1;
        drop(app.update_git(Action::Read {
            project,
            generation: 2,
            result: Ok((snapshot.clone(), None)),
        }));
        assert_eq!(
            app.sidebar_model.project(project).unwrap().worktrees[0]
                .pull_request()
                .map(|pr| pr.number),
            Some(7)
        );
        snapshot.heads.insert(
            id,
            GitHubRepoKey::new("github.com", None, "another", "repo").unwrap(),
        );
        drop(app.update_git(Action::Read {
            project,
            generation: 2,
            result: Ok((snapshot, None)),
        }));
        assert!(
            app.sidebar_model.project(project).unwrap().worktrees[0]
                .pull_request()
                .is_none()
        );
    }

    #[test]
    fn successful_watch_rearm_stops_fallback_without_requesting_a_read() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = fixture(&directory.path().join("repo"));
        let project = ProjectId::new();
        let mut state = State::default();
        state.entries.insert(
            project,
            Entry {
                snapshot,
                generation: 1,
                plan: None,
                archived: Vec::new(),
                remote_started: false,
                cancellation: None,
                read_failure: None,
                read_full_scan: false,
                catalog_requested: false,
                pending: Default::default(),
                fallback: Some("watch limit".into()),
                last_full_scan: Instant::now(),
                failed_status_attempts: HashMap::new(),
            },
        );
        state.changed(
            project,
            git_watch::Change {
                coverage: Some(true),
                ..Default::default()
            },
        );
        assert!(state.entries[&project].fallback.is_none());
        assert!(state.queue.is_empty());
        state.entries.get_mut(&project).unwrap().read_failure = Some("index unreadable".into());
        state.changed(
            project,
            git_watch::Change {
                coverage: Some(true),
                ..Default::default()
            },
        );
        assert!(
            state.entries[&project].read_failure.is_some(),
            "another project reconfiguring watches cannot settle a failed Git read"
        );
    }

    fn catalog_app(snapshot: &Snapshot) -> (Cagents, ProjectState) {
        let project = ProjectState {
            id: ProjectId::new(),
            generation: 0,
            name: "Catalog fixture".into(),
            repository: snapshot.repository.common_dir.clone(),
            kind: ProjectKind::Git,
            existence: Existence::Open,
        };
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.sidebar_model.projects.push(SidebarProject::new(
            project.id,
            snapshot.repository.id,
            project.name.clone(),
            project.repository.clone(),
            snapshot
                .worktrees
                .iter()
                .map(|(record, status)| SidebarWorktree::from_repository(record, status.as_ref()))
                .collect(),
            Vec::new(),
        ));
        app.repository_generations.insert(project.id, 1);
        app.refresh_ui.git.entries.insert(
            project.id,
            Entry {
                snapshot: snapshot.clone(),
                generation: 1,
                plan: None,
                archived: Vec::new(),
                remote_started: false,
                cancellation: None,
                pending: Default::default(),
                fallback: None,
                read_failure: None,
                read_full_scan: false,
                catalog_requested: false,
                last_full_scan: Instant::now(),
                failed_status_attempts: HashMap::new(),
            },
        );
        (app, project)
    }

    fn catalog_plan(project: ProjectId, snapshot: &Snapshot) -> git_watch::Project {
        git_watch::Project::read_changed(
            project,
            snapshot.repository.common_dir.clone(),
            &snapshot
                .worktrees
                .iter()
                .map(|(record, _)| record.clone())
                .collect::<Vec<_>>(),
            &CancellationToken::default(),
            None,
            &Default::default(),
        )
        .unwrap()
    }

    #[test]
    fn established_local_reads_skip_equal_catalog_writes_but_keep_ui_and_reassertions() {
        let directory = tempfile::tempdir().unwrap();
        let mut snapshot = fixture(&directory.path().join("repo"));
        let (mut app, project) = catalog_app(&snapshot);
        let id = snapshot.worktrees[0].0.id;
        let (writer, _) = SessionRuntime::open(
            directory.path().join("window.sqlite3"),
            crate::model::BootId::new("catalog-delta"),
        )
        .unwrap();
        let store = writer.operation_store();
        writer.save_project(project.clone());
        writer.save_worktrees([worktree_ui::worktree_state(
            project.id,
            &snapshot.worktrees[0].0,
            worktree_ui::removed_outside(),
        )]);
        writer.flush(Duration::from_secs(5)).unwrap();
        let closed = store.load_worktree(id).unwrap().unwrap();
        assert!(!closed.existence.is_open());
        app.session = Some(writer);
        let plan = catalog_plan(project.id, &snapshot);
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((snapshot.clone(), Some(plan.clone()))),
        }));
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        let opened = store.load_worktree(id).unwrap().unwrap();
        assert!(
            opened.existence.is_open(),
            "first equal Git facts must reopen a tombstone"
        );
        assert!(opened.generation > closed.generation);
        assert!(app.refresh_ui.git.entries[&project.id].catalog_requested);
        let last_scan = app.refresh_ui.git.entries[&project.id].last_full_scan;

        fs::write(snapshot.worktrees[0].0.path.join("tracked.txt"), "dirty\n").unwrap();
        let (dirty, next_plan) = read_local_with_plan(
            project.id,
            snapshot,
            git_watch::Change {
                worktrees: [id].into_iter().collect(),
                ..Default::default()
            },
            CancellationToken::default(),
            Some(&plan),
        )
        .unwrap();
        assert!(next_plan.is_none());
        assert!(dirty.worktrees[0].1.as_ref().unwrap().dirty.is_dirty());
        snapshot = dirty;
        // Deliver real read facts repeatedly. This tests the handler/catalog
        // boundary, not physical worker concurrency or native event delivery.
        for _ in 0..64 {
            app.refresh_ui
                .git
                .entries
                .get_mut(&project.id)
                .unwrap()
                .cancellation = Some(CancellationToken::default());
            drop(app.update_git(Action::Read {
                project: project.id,
                generation: 1,
                result: Ok((snapshot.clone(), None)),
            }));
            let entry = &app.refresh_ui.git.entries[&project.id];
            assert!(entry.cancellation.is_none());
            assert_eq!(entry.last_full_scan, last_scan);
            app.session
                .as_ref()
                .unwrap()
                .flush(Duration::from_secs(5))
                .unwrap();
            assert_eq!(store.load_worktree(id).unwrap().unwrap(), opened);
            let row = &app.sidebar_model.project(project.id).unwrap().worktrees[0];
            assert!(row.dirty.as_ref().unwrap().is_dirty());
        }

        git(
            &snapshot.worktrees[0].0.path,
            &["checkout", "-b", "catalog-branch"],
        );
        let (changed, next_plan) = read_local_with_plan(
            project.id,
            snapshot,
            git_watch::Change {
                worktrees: [id].into_iter().collect(),
                ..Default::default()
            },
            CancellationToken::default(),
            Some(&plan),
        )
        .unwrap();
        assert!(
            next_plan.is_some(),
            "changed branch facts refresh routing without a new inventory"
        );
        snapshot = changed;
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((snapshot.clone(), None)),
        }));
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        let branched = store.load_worktree(id).unwrap().unwrap();
        assert_eq!(branched.branch.as_deref(), Some("catalog-branch"));
        assert!(branched.generation > opened.generation);

        // Equal watch values can represent a replaced directory incarnation.
        // Adopting a plan must still reassert catalog rows and replace the Arc.
        let old_plan = app.refresh_ui.git.entries[&project.id]
            .plan
            .clone()
            .unwrap();
        let same_plan = catalog_plan(project.id, &snapshot);
        assert_eq!(old_plan.as_ref(), &same_plan);
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((snapshot, Some(same_plan))),
        }));
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        assert!(store.load_worktree(id).unwrap().unwrap().generation > branched.generation);
        let next = app.refresh_ui.git.entries[&project.id]
            .plan
            .as_ref()
            .unwrap();
        assert!(!Arc::ptr_eq(&old_plan, next));
        // Native rearming itself remains covered by the existing watcher test.
    }

    #[test]
    fn unchanged_local_reads_leave_owed_catalog_rows_with_the_session_writer() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = fixture(&directory.path().join("repo"));
        let (mut app, project) = catalog_app(&snapshot);
        let id = snapshot.worktrees[0].0.id;
        let (writer, _) = SessionRuntime::open(
            directory.path().join("window.sqlite3"),
            crate::model::BootId::new("catalog-held-parent"),
        )
        .unwrap();
        let store = writer.operation_store();
        app.session = Some(writer);
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((snapshot.clone(), Some(catalog_plan(project.id, &snapshot)))),
        }));
        // A missing parent holds the child in the writer; the window barrier
        // can settle without falsely implying that this catalog row landed.
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        assert!(store.load_worktree(id).unwrap().is_none());
        assert!(app.refresh_ui.git.entries[&project.id].catalog_requested);
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((snapshot.clone(), None)),
        }));
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        assert!(store.load_worktree(id).unwrap().is_none());
        app.session.as_ref().unwrap().save_project(project.clone());
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        let landed = store.load_worktree(id).unwrap().unwrap();
        assert_eq!(landed.path, snapshot.worktrees[0].0.path);
        assert!(landed.existence.is_open());
    }

    #[test]
    fn a_read_without_a_session_cannot_suppress_the_next_catalog_baseline() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = fixture(&directory.path().join("repo"));
        let (mut app, project) = catalog_app(&snapshot);
        let id = snapshot.worktrees[0].0.id;
        let database = directory.path().join("window.sqlite3");
        let boot = crate::model::BootId::new("catalog-session-presence");
        let (writer, _) = SessionRuntime::open(&database, boot.clone()).unwrap();
        writer.save_project(project.clone());
        app.session = Some(writer);
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((snapshot.clone(), Some(catalog_plan(project.id, &snapshot)))),
        }));
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        assert!(app.refresh_ui.git.entries[&project.id].catalog_requested);
        let mut prior = app.session.take().unwrap();
        prior.shutdown();
        drop(prior);
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((snapshot.clone(), None)),
        }));
        assert!(!app.refresh_ui.git.entries[&project.id].catalog_requested);
        assert!(app.refresh_ui.git.entries[&project.id].plan.is_some());
        let (writer, _) = SessionRuntime::open(&database, boot).unwrap();
        let store = writer.operation_store();
        writer.save_worktrees([worktree_ui::worktree_state(
            project.id,
            &snapshot.worktrees[0].0,
            worktree_ui::removed_outside(),
        )]);
        writer.flush(Duration::from_secs(5)).unwrap();
        let closed = store.load_worktree(id).unwrap().unwrap();
        assert!(!closed.existence.is_open());
        app.session = Some(writer);
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((snapshot, None)),
        }));
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        let reopened = store.load_worktree(id).unwrap().unwrap();
        assert!(reopened.existence.is_open());
        assert!(reopened.generation > closed.generation);
        // Production bootstrap fixes SessionRuntime before repository loading;
        // this is a None->Some robustness guard, not general session migration.
    }
}
