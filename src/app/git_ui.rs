//! Local Git invalidation has its own queue and generations. GitHub reads never
//! replace local rows, and filesystem events never schedule a GitHub process.
use super::*;
use crate::{git_watch, model::WorktreeId, repository::HeadState};
use std::collections::BTreeMap;

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
    operations: HashMap<WorktreeId, crate::repository::GitOperationKind>,
    status_errors: HashMap<WorktreeId, String>,
    /// Only these rows were actually attempted by this read. Retained sibling
    /// snapshots cannot supersede a newer successful bound Changes read.
    status_updates: HashSet<WorktreeId>,
    status_reads: HashMap<WorktreeId, u64>,
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
            if let Some(operation) = current.operations.get(&id) {
                self.operations.insert(id, *operation);
            } else {
                self.operations.remove(&id);
            }
            if let Some(latest) = current_rows.get(&id) {
                *row = (**latest).clone();
                true
            } else {
                self.status_errors.remove(&id);
                self.operations.remove(&id);
                false
            }
        });
        // An omission is an attempted removal, not authority over a row whose
        // newer bound read already proved it present again.
        for row in &current.worktrees {
            let id = row.0.id;
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
            if let Some(operation) = current.operations.get(&id) {
                self.operations.insert(id, *operation);
            } else {
                self.operations.remove(&id);
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
    let roots = snapshot
        .worktrees
        .iter()
        .filter(|(record, _)| !record.bare)
        .map(|(record, _)| (record.id, &record.path))
        .collect::<HashMap<_, _>>();
    let replacements = previous
        .into_iter()
        .flat_map(|project| &project.worktrees)
        .filter(|worktree| {
            preserved.contains(&worktree.id)
                && roots
                    .get(&worktree.id)
                    .is_some_and(|root| **root == worktree.root)
        })
        .collect::<Vec<_>>();
    let replacement_ids = replacements
        .iter()
        .map(|worktree| worktree.id)
        .collect::<HashSet<_>>();
    plan.worktrees.retain(|worktree| {
        !replacement_ids.contains(&worktree.id)
            && roots
                .get(&worktree.id)
                .is_some_and(|root| **root == worktree.root)
    });
    plan.worktrees.extend(replacements.into_iter().cloned());
    let covered = plan
        .worktrees
        .iter()
        .map(|worktree| worktree.id)
        .collect::<HashSet<_>>();
    preserved
        .iter()
        .copied()
        .filter(|id| roots.contains_key(id) && !covered.contains(id))
        .collect()
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
    /// When the worktree list was last re-read. Polling is paced from this
    /// rather than from any read, since the edits a partial watch still
    /// reports would otherwise postpone the scan it stands in for.
    last_full_scan: Instant,
}

#[derive(Default)]
pub(super) struct State {
    monitor: Option<git_watch::Monitor>,
    entries: HashMap<ProjectId, Entry>,
    queue: VecDeque<ProjectId>,
    activity: Vec<ProjectId>,
}

impl State {
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
        operation: Option<crate::repository::GitOperationKind>,
        read: u64,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(&key.project_id) else {
            return true;
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
        entry.snapshot.status_errors.remove(&key.worktree_id);
        if let Some(operation) = operation {
            entry.snapshot.operations.insert(key.worktree_id, operation);
        } else {
            entry.snapshot.operations.remove(&key.worktree_id);
        }
        entry.snapshot.status_reads.insert(key.worktree_id, read);
        true
    }

    pub fn forget(&mut self, project: ProjectId) {
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
            entry.snapshot.status_updates.remove(&worktree);
            entry
                .snapshot
                .status_reads
                .insert(worktree, next_status_read());
        }
    }

    fn configure(&self) {
        if let Some(monitor) = &self.monitor {
            monitor.configure(
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
        let failure = if state.monitor.is_none() {
            match git_watch::Monitor::start() {
                Ok(monitor) => {
                    state.monitor = Some(monitor);
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
                    worktrees: loaded.worktrees.clone(),
                    heads: loaded.head_repositories.clone(),
                    operations: loaded.operations.clone(),
                    status_errors: HashMap::new(),
                    status_updates: HashSet::new(),
                    status_reads: HashMap::new(),
                },
                generation,
                plan: reusable_plan.clone(),
                archived: Vec::new(),
                remote_started: false,
                cancellation: None,
                read_failure: None,
                read_full_scan: false,
                pending: git_watch::Change::default(),
                fallback: failure,
                last_full_scan: Instant::now(),
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
        let events =
            self.refresh_ui
                .git
                .monitor
                .as_ref()
                .map_or_else(Subscription::none, |monitor| {
                    Subscription::run_with(Source(monitor.receiver.clone()), stream)
                        .map(|()| Message::GitUi(Action::Changed))
                });
        Subscription::batch([
            events,
            iced::time::every(FALLBACK_INTERVAL).map(|_| Message::GitUi(Action::Fallback)),
        ])
    }

    pub(super) fn git_fallback_tick(&mut self) -> Task<Message> {
        if self.refresh_ui.git.monitor.is_none()
            && !self.refresh_ui.git.entries.is_empty()
            && let Ok(monitor) = git_watch::Monitor::start()
        {
            self.refresh_ui.git.monitor = Some(monitor);
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
            .filter(|(_, entry)| {
                (entry.fallback.is_some() || entry.read_failure.is_some())
                    && entry.last_full_scan.elapsed() >= FALLBACK_INTERVAL
            })
            .map(|(&id, _)| id)
            .collect::<Vec<_>>();
        for project in due {
            let worktrees = self.refresh_ui.git.entries[&project]
                .snapshot
                .worktrees
                .iter()
                .map(|(record, _)| record.id)
                .collect();
            self.refresh_ui.git.changed(
                project,
                git_watch::Change {
                    worktrees,
                    topology: true,
                    membership: true,
                    ..Default::default()
                },
            );
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
                    .monitor
                    .as_ref()
                    .map(git_watch::Monitor::drain)
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
                    return Task::batch([self.reload_source_control(&key), self.start_git_reads()]);
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
                        let previous = std::mem::replace(&mut entry.snapshot, snapshot.clone());
                        let removed =
                            records_removed_outside(project, &previous, &snapshot.worktrees);
                        let reconfigure = plan.is_some();
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
                            session.save_worktrees(removed.into_iter().chain(
                                snapshot.worktrees.iter().map(|(record, _)| {
                                    worktree_ui::worktree_state(project, record, Existence::Open)
                                }),
                            ));
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
        if let Some(monitor) = &state.monitor {
            monitor.prioritize(priority, selected);
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

/// Local metadata only. Loading/count refreshes must not spawn another Git
/// command per worktree merely to discover merge/rebase state.
pub(super) fn worktree_operation(
    worktree: &Path,
    known_git_dir: Option<&Path>,
) -> Option<crate::repository::GitOperationKind> {
    let git_dir = if let Some(directory) = known_git_dir {
        directory.to_path_buf()
    } else {
        let entry = worktree.join(".git");
        if entry.is_dir() {
            entry
        } else {
            let content = std::fs::read_to_string(entry).ok()?;
            let directory = PathBuf::from(
                content
                    .trim_end_matches(['\r', '\n'])
                    .strip_prefix("gitdir: ")?,
            );
            if directory.is_absolute() {
                directory
            } else {
                worktree.join(directory)
            }
        }
    };
    RepositoryService::<crate::repository::SystemProcessExecutor>::operation_state_from_git_dir(
        &git_dir,
    )
    .map(|operation| operation.kind)
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
            .list_worktrees_for_repository(&snapshot.repository, &cancellation)
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
    for (record, status) in &mut snapshot.worktrees {
        if record.bare || (!change.topology && !change.worktrees.contains(&record.id)) {
            continue;
        }
        // A membership edge replaces the cached full count in this same
        // coalesced status process. Tracked-only writes do not rescan untracked
        // trees; the bounded topology fallback repairs incomplete watch coverage.
        let include_untracked =
            change.topology || change.membership || change.metadata || change.untracked;
        snapshot.status_updates.insert(record.id);
        snapshot.status_reads.insert(record.id, next_status_read());
        let current = match git.status_with_untracked(
            &record.path,
            include_untracked,
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
        snapshot.status_errors.remove(&record.id);
        let known_git_dir = previous_plan.and_then(|plan| {
            plan.worktrees
                .iter()
                .find(|worktree| worktree.id == record.id)
                .map(|worktree| worktree.git_dir.as_path())
        });
        if let Some(operation) = worktree_operation(&record.path, known_git_dir) {
            snapshot.operations.insert(record.id, operation);
        } else {
            snapshot.operations.remove(&record.id);
        }
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
        *status = Some(current);
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
    snapshot.operations.retain(|id, _| retained.contains(id));
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
    let plan = (change.membership || change.topology || !vanished.is_empty())
        .then(|| {
            git_watch::Project::read_changed(
                project,
                snapshot.repository.common_dir.clone(),
                &snapshot
                    .worktrees
                    .iter()
                    .map(|(record, _)| record.clone())
                    .collect::<Vec<_>>(),
                &cancellation,
                previous_plan,
                &change.worktrees,
                &upstreams,
            )
        })
        .transpose()?
        .or_else(|| previous_plan.and_then(|plan| plan.refresh_upstreams(&upstreams)));
    Ok((snapshot, plan))
}

fn apply_status_head(record: &mut WorktreeRecord, status: &StatusSnapshot) {
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
                if error.is_none() {
                    row.apply_operation(snapshot.operations.get(&record.id).copied());
                }
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
            .list_worktrees_for_repository(&repository, &cancellation)
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
            operations: HashMap::new(),
            status_errors: HashMap::new(),
            status_updates,
            status_reads: HashMap::new(),
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
                last_full_scan: Instant::now(),
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
        let operation = snapshot.operation.as_ref().map(|operation| operation.kind);
        let model = SourceControlModel::ready(key.clone(), &snapshot);
        Message::SourceControlLoaded {
            key: key.clone(),
            request,
            result: Ok(SourceControlRead {
                prepared: Some(Box::new((snapshot, model))),
                status,
                operation,
                read,
            }),
        }
    }

    #[test]
    fn failed_worktree_status_is_last_known_and_sibling_reads_cannot_replay_it_after_recovery() {
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
            .list_worktrees_for_repository(&snapshot.repository, &cancel)
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
        // An older watch snapshot may still remember an operation that a newer
        // successful Changes read has already cleared.
        snapshot
            .operations
            .insert(linked_id, crate::repository::GitOperationKind::Merge);
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
        let (failed, _) = read_local(project.id, snapshot, change(), cancel.clone()).unwrap();
        assert!(failed.status_errors[&linked_id].contains("index"));
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
        row.apply_operation(None);
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
        assert!(row.operation.is_none(), "nor B's old operation glyph");
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
            assert_eq!(
                baseline.operations.get(&worktree),
                Some(&crate::repository::GitOperationKind::Merge)
            );
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
                    last_full_scan: Instant::now(),
                },
            );
            app.sidebar_model.source_control = Some(SourceControlModel::loading(key.clone()));
            app.refresh_ui.source_generation = 7;
            let bound_message = Message::SourceControlLoaded {
                key: key.clone(),
                request: 7,
                result: Ok(SourceControlRead {
                    status: prepared.status.display_summary(),
                    operation: prepared.operation.as_ref().map(|operation| operation.kind),
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
            assert!(row.operation.is_none(), "{scenario}");
            let cached = &app.refresh_ui.git.entries[&project_id].snapshot;
            assert!(!cached.status_errors.contains_key(&worktree), "{scenario}");
            assert!(!cached.operations.contains_key(&worktree), "{scenario}");
            assert_eq!(
                cached.worktrees[0].1.as_ref().unwrap().dirty.untracked,
                1,
                "{scenario}"
            );
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
            app.refresh_ui.git.monitor = Some(git_watch::Monitor::start().unwrap());
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
            let monitor = app.refresh_ui.git.monitor.as_ref().unwrap();
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
        let (status, operation) = cache.row_status().unwrap();
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
                operation,
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
    fn full_count_edges_discover_new_untracked_files_and_clear_removed_ones() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let snapshot = fixture(&root);
        let id = snapshot.worktrees[0].0.id;
        let project = ProjectId::new();
        let change = git_watch::Change {
            worktrees: [id].into(),
            untracked: true,
            ..Default::default()
        };
        fs::write(root.join("new.txt"), "new untracked file\n").unwrap();
        let (snapshot, plan) = read_local(
            project,
            snapshot,
            change.clone(),
            CancellationToken::default(),
        )
        .unwrap();
        assert!(
            plan.is_none(),
            "file entries do not re-enumerate watch membership"
        );
        let status = snapshot.worktrees[0].1.as_ref().unwrap();
        assert!(!status.untracked_omitted);
        assert_eq!(status.dirty.untracked, 1);
        git(&root, &["add", "new.txt"]);
        let (snapshot, _) = read_local(
            project,
            snapshot,
            git_watch::Change {
                worktrees: [id].into(),
                metadata: true,
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        let status = snapshot.worktrees[0].1.as_ref().unwrap();
        assert_eq!(status.dirty.staged, 1);
        assert_eq!(
            status.dirty.untracked, 0,
            "index mutations must not double count newly staged files"
        );
        git(&root, &["reset", "--", "new.txt"]);
        fs::remove_file(root.join("new.txt")).unwrap();
        let (snapshot, _) =
            read_local(project, snapshot, change, CancellationToken::default()).unwrap();
        let status = snapshot.worktrees[0].1.as_ref().unwrap();
        assert!(!status.untracked_omitted);
        assert_eq!(status.dirty.untracked, 0);
    }

    #[test]
    fn operation_metadata_uses_linked_gitfiles_without_running_git() {
        use crate::repository::GitOperationKind;
        let fixture = tempfile::tempdir().unwrap();
        let worktree = fixture.path().join("worktree");
        let admin = fixture.path().join("admin with spaces ");
        fs::create_dir(&worktree).unwrap();
        fs::create_dir(&admin).unwrap();
        fs::write(worktree.join(".git"), "gitdir: ../admin with spaces \n").unwrap();
        assert_eq!(worktree_operation(&worktree, None), None);
        fs::write(admin.join("MERGE_HEAD"), "fixture-head\n").unwrap();
        assert_eq!(
            worktree_operation(&worktree, None),
            Some(GitOperationKind::Merge)
        );
        fs::remove_file(admin.join("MERGE_HEAD")).unwrap();
        fs::create_dir(admin.join("rebase-merge")).unwrap();
        assert_eq!(
            worktree_operation(&worktree, Some(&admin)),
            Some(GitOperationKind::Rebase)
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
                pending: Default::default(),
                fallback: None,
                last_full_scan: Instant::now(),
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
                pending: Default::default(),
                fallback: None,
                last_full_scan: Instant::now(),
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
                    pending: Default::default(),
                    fallback: Some("watch limit".into()),
                    last_full_scan: Instant::now() - FALLBACK_INTERVAL,
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
                pending: Default::default(),
                fallback: Some("directory watch limit reached".into()),
                last_full_scan: Instant::now() - FALLBACK_INTERVAL,
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
                pending: Default::default(),
                fallback: None,
                last_full_scan: Instant::now(),
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
                pending: Default::default(),
                fallback: Some("watch limit".into()),
                last_full_scan: Instant::now(),
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
}
