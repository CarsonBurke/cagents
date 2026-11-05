//! Local Git invalidation has its own queue and generations. GitHub reads never
//! replace local rows, and filesystem events never schedule a GitHub process.
use super::*;
use crate::{git_watch, model::WorktreeId, repository::HeadState};

const LOCAL_WORKERS: usize = 2;
const FALLBACK_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Snapshot {
    repository: RepositoryInfo,
    worktrees: Vec<(WorktreeRecord, Option<StatusSnapshot>)>,
    heads: HashMap<WorktreeId, GitHubRepoKey>,
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

#[derive(Default)]
pub(super) struct State {
    watch: Option<Watch>,
    entries: HashMap<ProjectId, Entry>,
    queue: VecDeque<ProjectId>,
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
        if change.worktrees.is_empty()
            && !change.references
            && !change.topology
            && !change.membership
        {
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
    Read {
        project: ProjectId,
        generation: u64,
        result: Result<(Snapshot, Option<git_watch::Project>), String>,
    },
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
        if let Some(old) = state.entries.remove(&project) {
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
                    worktrees: loaded.worktrees.clone(),
                    heads: loaded.head_repositories.clone(),
                },
                generation,
                plan: None,
                archived: Vec::new(),
                remote_started: false,
                cancellation: None,
                read_failure: None,
                read_full_scan: false,
                catalog_requested: false,
                pending: git_watch::Change::default(),
                fallback: failure,
                last_full_scan: Instant::now(),
            },
        );
        state.changed(
            project,
            git_watch::Change {
                membership: true,
                ..Default::default()
            },
        );
        self.start_git_reads()
    }

    pub(super) fn git_subscription(&self) -> Subscription<Message> {
        self.refresh_ui
            .git
            .watch
            .as_ref()
            .map_or_else(Subscription::none, |watch| {
                Subscription::run_with(Source(watch.receiver.clone()), stream)
                    .map(|()| Message::GitUi(Action::Changed))
            })
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
        self.start_git_reads()
    }

    pub(super) fn update_git(&mut self, action: Action) -> Task<Message> {
        match action {
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
                for (project, change) in changes {
                    if let Some(error) = &change.error
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
                if self.session.is_none() {
                    entry.catalog_requested = false;
                }
                match result {
                    Ok((snapshot, plan)) => {
                        if entry.read_full_scan {
                            entry.read_failure = None;
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
            }
        }
        self.start_git_reads()
    }

    fn start_git_reads(&mut self) -> Task<Message> {
        let state = &mut self.refresh_ui.git;
        let mut running = state
            .entries
            .values()
            .filter(|entry| entry.cancellation.is_some())
            .count();
        let mut tasks = Vec::new();
        let mut inspected = state.queue.len();
        while running < LOCAL_WORKERS && inspected > 0 {
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

fn read_local_with_plan(
    project: ProjectId,
    mut snapshot: Snapshot,
    change: git_watch::Change,
    cancellation: CancellationToken,
    previous_plan: Option<&git_watch::Project>,
) -> Result<(Snapshot, Option<git_watch::Project>), String> {
    let git = RepositoryService::system("git");
    if change.topology {
        snapshot.worktrees = git
            .list_worktrees(&snapshot.repository.common_dir, &cancellation)
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|record| record.bare || worktree_ui::worktree_directory_available(&record.path))
            .map(|record| (record, None))
            .collect();
    }
    let mut vanished = HashSet::new();
    for (record, status) in &mut snapshot.worktrees {
        // Shared refs move every worktree's upstream comparison.
        if record.bare
            || (!change.topology && !change.references && !change.worktrees.contains(&record.id))
        {
            continue;
        }
        let current = match git.status_with_untracked(&record.path, false, &cancellation) {
            Ok(current) => current,
            // Its own removal is often the event being read, and one absent
            // checkout must not fail its siblings.
            Err(_) if checkout_removed(&record.path) => {
                vanished.insert(record.id);
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        match &current.head {
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
        snapshot.heads.remove(&record.id);
        if let Some(branch) = current.branch()
            && let Some(head) =
                crate::github::worktree_head_repository(&record.path, branch, &cancellation)
        {
            snapshot.heads.insert(record.id, head);
        }
        *status = Some(current);
    }
    snapshot
        .worktrees
        .retain(|(record, _)| !vanished.contains(&record.id));
    let surviving_ids = snapshot
        .worktrees
        .iter()
        .map(|(record, _)| record.id)
        .collect::<HashSet<_>>();
    snapshot.heads.retain(|id, _| surviving_ids.contains(id));
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
            )
        })
        .transpose()?;
    Ok((snapshot, plan))
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
    let listed_ids = listed
        .iter()
        .map(|(record, _)| record.id)
        .collect::<HashSet<_>>();
    previous
        .worktrees
        .iter()
        .filter(|(old, _)| !listed_ids.contains(&old.id))
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
    // The writer keeps the last row for an identity. Compare only that row:
    // an earlier duplicate must not become the sole delta when the final row
    // already matches the durable baseline.
    let first_change = removed.len();
    let mut seen = HashSet::new();
    removed.extend(listed.iter().rev().filter_map(|(record, _)| {
        if !seen.insert(record.id) {
            return None;
        }
        let current = open(record);
        (previous.get(&record.id) != Some(&current)).then_some(current)
    }));
    removed[first_change..].reverse();
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
            let mut row = previous
                .remove(&record.id)
                .unwrap_or_else(|| SidebarWorktree::from_repository(record, status.as_ref()));
            row.apply_repository(record, status.as_ref());
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
        let output = Command::new("git")
            .current_dir(directory)
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
        let worktrees = service
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
        Snapshot {
            repository,
            worktrees,
            heads: HashMap::new(),
        }
    }

    #[test]
    fn reordered_catalog_membership_preserves_closure_order_and_surviving_heads() {
        let project = ProjectId::new();
        let root = PathBuf::from("/fixture/catalog-membership-only");
        let records = (0..2048)
            .map(|index| {
                (
                    WorktreeRecord {
                        id: WorktreeId::new(),
                        path: root.join(format!("worktree-{index}")),
                        created_at_unix_ms: None,
                        head_oid: None,
                        branch: Some(format!("refs/heads/branch-{index}")),
                        detached: false,
                        bare: false,
                        lock: Default::default(),
                        prunable_reason: None,
                    },
                    None,
                )
            })
            .collect::<Vec<_>>();
        let head = GitHubRepoKey::new("github.com", None, "fixture", "project").unwrap();
        let previous = Snapshot {
            repository: RepositoryInfo {
                id: crate::repository::RepositoryId::new(),
                selected_path: root.clone(),
                common_dir: root.join(".git"),
                git_dir: root.join(".git"),
                worktree_root: Some(root),
                is_bare: false,
            },
            heads: records
                .iter()
                .map(|(record, _)| (record.id, head.clone()))
                .collect(),
            worktrees: records,
        };
        let mut listed = previous
            .worktrees
            .iter()
            .enumerate()
            .filter(|(index, _)| index % 3 != 0)
            .map(|(_, record)| record.clone())
            .rev()
            .collect::<Vec<_>>();
        // Identity survives both a reordered listing and changed durable fields.
        listed[0].0.path = PathBuf::from("/fixture/moved-checkout");
        listed[0].0.branch = Some("refs/heads/renamed".into());
        let removed = records_removed_outside(project, &previous, &listed);
        assert_eq!(
            removed.iter().map(|record| record.id).collect::<Vec<_>>(),
            previous
                .worktrees
                .iter()
                .step_by(3)
                .map(|(record, _)| record.id)
                .collect::<Vec<_>>()
        );
        assert!(removed.iter().all(|record| {
            record.project_id == project && matches!(record.existence, Existence::Closed { .. })
        }));
        let changes = catalog_changes(project, &previous, &listed, removed.clone(), false);
        assert_eq!(changes[..removed.len()], removed);
        assert_eq!(changes.len(), removed.len() + 1);
        assert_eq!(changes.last().unwrap().id, listed[0].0.id);
        assert_eq!(changes.last().unwrap().branch.as_deref(), Some("renamed"));
        assert_eq!(changes.last().unwrap().path, listed[0].0.path);

        // No changed worktree or topology means this read performs no Git or
        // watch discovery. It still prunes stale heads by surviving identity.
        let expected_heads = listed
            .iter()
            .map(|(record, _)| (record.id, head.clone()))
            .collect::<HashMap<_, _>>();
        let (updated, plan) = read_local_with_plan(
            project,
            Snapshot {
                worktrees: listed.clone(),
                ..previous
            },
            Default::default(),
            CancellationToken::default(),
            None,
        )
        .unwrap();
        assert!(plan.is_none());
        assert_eq!(updated.worktrees, listed);
        assert_eq!(updated.heads, expected_heads);
        assert!(records_removed_outside(project, &updated, &updated.worktrees).is_empty());
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
                    catalog_requested: false,
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
            LOCAL_WORKERS
        );
        assert_eq!(app.refresh_ui.git.queue.len(), 4 - LOCAL_WORKERS);
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
    fn catalog_deltas_preserve_the_final_duplicate_identity_through_the_session_writer() {
        let directory = tempfile::tempdir().unwrap();
        let mut snapshot = fixture(&directory.path().join("repo"));
        let mut winner = snapshot.worktrees[0].clone();
        winner.0.branch = Some("refs/heads/winner".into());
        snapshot.worktrees.push(winner);
        let (mut app, project) = catalog_app(&snapshot);
        let id = snapshot.worktrees[0].0.id;
        let (writer, _) = SessionRuntime::open(
            directory.path().join("window.sqlite3"),
            crate::model::BootId::new("catalog-duplicate-winner"),
        )
        .unwrap();
        let store = writer.operation_store();
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
        let baseline = store.load_worktree(id).unwrap().unwrap();
        assert_eq!(baseline.branch.as_deref(), Some("winner"));
        assert!(baseline.existence.is_open());

        for shadow in ["main", "changed-shadow"] {
            snapshot.worktrees[0].0.branch = Some(format!("refs/heads/{shadow}"));
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
            assert_eq!(
                store.load_worktree(id).unwrap().unwrap(),
                baseline,
                "an unchanged final row must retain its exact durable generation and fields"
            );
        }
        snapshot.worktrees[1].0.branch = Some("refs/heads/new-winner".into());
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
        let changed = store.load_worktree(id).unwrap().unwrap();
        assert_eq!(changed.branch.as_deref(), Some("new-winner"));
        assert_eq!(changed.path, baseline.path);
        assert!(changed.generation > baseline.generation);

        snapshot.worktrees[0].0.branch = Some("refs/heads/another-shadow".into());
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
        assert_eq!(store.load_worktree(id).unwrap().unwrap(), changed);
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
        assert!(next_plan.is_none());
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
