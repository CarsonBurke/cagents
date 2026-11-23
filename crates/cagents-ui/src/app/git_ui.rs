//! Local Git invalidation has its own queue and generations. GitHub reads never
//! replace local rows, and filesystem events never schedule a GitHub process.
use super::*;
use crate::{
    git_watch,
    model::WorktreeId,
    repository::{HeadState, TrackingState},
};

const LOCAL_WORKERS: usize = 2;
/// How often a checkout someone works in is read while the watcher cannot
/// see edits to its files, and how often a project whose Git metadata it
/// cannot see has its worktree list read.
const FALLBACK_INTERVAL: Duration = Duration::from_secs(60);
/// How old the status of a checkout whose files are not watched may be when
/// its row comes into view, or while it has a pane out of sight, before it
/// is read again. Git's own bookkeeping is watched for every checkout, so
/// this only bounds how long an edit made outside the application, without
/// staging or committing, goes unseen there. A row that stays in view is not
/// read again on a timer, nor is a checkout nobody looks at: with hundreds of
/// worktrees, reading each in turn kept a Git process running and the disk
/// busy for as long as the window had focus.
const SHOWN_STALE: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone)]
pub struct Snapshot {
    repository: RepositoryInfo,
    worktrees: Vec<(WorktreeRecord, Option<StatusSnapshot>)>,
    heads: HashMap<WorktreeId, GitHubRepoKey>,
    /// The commit pair each worktree's recorded ahead/behind counts were
    /// taken against, so a shared-ref change can tell whether those counts
    /// still hold without re-reading any worktree's status.
    tracks: HashMap<WorktreeId, TrackedPair>,
    archived: Arc<ArchivedPrs>,
}

impl Snapshot {
    /// Whether an archived PR names this checkout's branch but was proved
    /// against another HEAD, so only a local ancestry check can settle it.
    fn awaits_archived_proof(&self, record: &WorktreeRecord) -> bool {
        self.heads.get(&record.id).is_some_and(|head_repository| {
            self.archived.associations.iter().any(|association| {
                association.matches(record, head_repository)
                    && record.head_oid.as_deref() != Some(association.validated_head.as_str())
            })
        })
    }
}

/// Shared across snapshot clones to keep local result messages small. Remote
/// results can replace candidates while a local read is in flight.
#[derive(Debug, Clone, Default)]
struct ArchivedPrs {
    associations: Vec<ArchivedWorktreePr>,
    revision: u64,
}

/// The head and upstream commits one recorded comparison was counted
/// between, with the counts it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedPair {
    head: String,
    upstream: String,
    ahead: u64,
    behind: u64,
}

struct Entry {
    snapshot: Snapshot,
    generation: u64,
    plan: Option<Arc<git_watch::Project>>,
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
    /// While the watcher covers the project's metadata (`fallback` is
    /// `None`), the worktrees whose own files it does not watch.
    unwatched: BTreeSet<WorktreeId>,
    /// When each worktree's status was last read.
    read_at: HashMap<WorktreeId, Instant>,
}

impl Entry {
    fn new(snapshot: Snapshot, generation: u64, fallback: Option<String>) -> Self {
        let now = Instant::now();
        let read_at = snapshot
            .worktrees
            .iter()
            .filter(|(_, status)| status.is_some())
            .map(|(record, _)| (record.id, now))
            .collect();
        Self {
            snapshot,
            generation,
            plan: None,
            remote_started: false,
            cancellation: None,
            pending: git_watch::Change::default(),
            fallback,
            read_failure: None,
            read_full_scan: false,
            catalog_requested: false,
            last_full_scan: now,
            unwatched: BTreeSet::new(),
            read_at,
        }
    }

    /// How much each of this project's checkouts is worth watching, given
    /// the ones someone works in. The project's own checkout is shown, as
    /// the one its row stands for; the rest are watched for Git's
    /// bookkeeping alone.
    fn interest(
        &self,
        project: ProjectId,
        work: &BTreeMap<(ProjectId, WorktreeId), git_watch::Interest>,
    ) -> HashMap<WorktreeId, git_watch::Interest> {
        self.snapshot
            .worktrees
            .iter()
            .map(|(record, _)| {
                let interest = if let Some(interest) = work.get(&(project, record.id)) {
                    *interest
                } else if sidebar::is_primary_worktree(
                    &self.snapshot.repository.common_dir,
                    &record.path,
                ) {
                    git_watch::Interest::Shown
                } else {
                    git_watch::Interest::Metadata
                };
                (record.id, interest)
            })
            .collect()
    }
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
    /// The checkouts someone works in. [`Interest::Active`] for a pane on
    /// screen and the selected row: their files are watched first, and read
    /// every [`FALLBACK_INTERVAL`] when the watch budget cannot cover them.
    /// [`Interest::Shown`] for one whose panes are all out of sight.
    ///
    /// [`Interest::Active`]: git_watch::Interest::Active
    /// [`Interest::Shown`]: git_watch::Interest::Shown
    work: BTreeMap<(ProjectId, WorktreeId), git_watch::Interest>,
    /// The checkout rows in view at the last periodic check.
    shown: HashSet<(ProjectId, WorktreeId)>,
}

impl State {
    pub(super) fn remember_archived(
        &mut self,
        project: ProjectId,
        generation: u64,
        archived: &[ArchivedWorktreePr],
    ) {
        if let Some(entry) = self
            .entries
            .get_mut(&project)
            .filter(|entry| entry.generation == generation && !entry.remote_started)
        {
            let saved = Arc::make_mut(&mut entry.snapshot.archived);
            saved.associations = archived.to_vec();
            saved.revision += 1;
            self.revalidate_archived(project);
        }
    }

    /// A late remote result may have been proved against an earlier local
    /// HEAD. The ordinary local worker rechecks it without another gh read.
    fn revalidate_archived(&mut self, project: ProjectId) {
        let Some(entry) = self.entries.get(&project) else {
            return;
        };
        let worktrees = entry
            .snapshot
            .worktrees
            .iter()
            .filter(|(record, _)| entry.snapshot.awaits_archived_proof(record))
            .map(|(record, _)| record.id)
            .collect::<std::collections::BTreeSet<_>>();
        if !worktrees.is_empty() {
            self.changed(
                project,
                git_watch::Change {
                    worktrees,
                    ..Default::default()
                },
            );
        }
    }

    pub(super) fn worktrees(
        &self,
        project: ProjectId,
    ) -> Option<&[(WorktreeRecord, Option<StatusSnapshot>)]> {
        self.entries
            .get(&project)
            .map(|entry| entry.snapshot.worktrees.as_slice())
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

    /// Records which checkouts someone works in. A project whose set moved
    /// is planned again, which gives a newly worked checkout its file
    /// watches; the watcher asks for its read once they are in place.
    pub(super) fn set_work(
        &mut self,
        work: BTreeMap<(ProjectId, WorktreeId), git_watch::Interest>,
    ) {
        if work == self.work {
            return;
        }
        let moved = self
            .work
            .iter()
            .filter(|(key, interest)| work.get(key) != Some(interest))
            .chain(
                work.iter()
                    .filter(|(key, interest)| self.work.get(key) != Some(interest)),
            )
            .map(|((project, _), _)| *project)
            .collect::<BTreeSet<_>>();
        let before = std::mem::replace(&mut self.work, work);
        // Moving between on screen and out of sight changes only the order
        // the budget goes in, which matters only while it falls short of a
        // checkout that wants file watches, in any project, since the budget
        // is shared. Otherwise a project re-plans only when one of its
        // checkouts gains or loses file watches.
        let short = self.entries.iter().any(|(project, entry)| {
            !entry.unwatched.is_empty()
                && entry
                    .interest(*project, &before)
                    .iter()
                    .any(|(worktree, interest)| {
                        interest.watches_files() && entry.unwatched.contains(worktree)
                    })
        });
        let replan = moved
            .into_iter()
            .filter(|project| {
                let Some(entry) = self.entries.get(project) else {
                    return false;
                };
                if short {
                    return true;
                }
                let was = entry.interest(*project, &before);
                let now = entry.interest(*project, &self.work);
                now.iter().any(|(worktree, interest)| {
                    interest.watches_files()
                        != was.get(worktree).is_some_and(|was| was.watches_files())
                        || (interest.watches_files() && entry.unwatched.contains(worktree))
                })
            })
            .collect::<Vec<_>>();
        for project in replan {
            self.changed(
                project,
                git_watch::Change {
                    membership: true,
                    ..Default::default()
                },
            );
        }
    }

    /// What a periodic check reads, for the checkouts `shown` lists:
    ///
    /// - A project whose metadata the watcher cannot see has its worktree
    ///   list read every [`FALLBACK_INTERVAL`]. That read also notices a
    ///   checkout whose `HEAD` moved.
    /// - A checkout someone works in on screen, or selected, whose files are
    ///   not watched is read every [`FALLBACK_INTERVAL`].
    /// - One worked in out of sight is read once its status is
    ///   [`SHOWN_STALE`], and so is one whose row has come into view since
    ///   the last check.
    /// - Every other checkout, a row that stays in view included, waits for
    ///   a Git event, or for its row to come into view again.
    fn due(
        &self,
        shown: &HashSet<(ProjectId, WorktreeId)>,
        now: Instant,
    ) -> Vec<(ProjectId, git_watch::Change)> {
        let mut due = Vec::new();
        for (&project, entry) in &self.entries {
            let blind = entry.fallback.is_some() || entry.read_failure.is_some();
            let mut change = git_watch::Change::default();
            if blind && now.saturating_duration_since(entry.last_full_scan) >= FALLBACK_INTERVAL {
                change.topology = true;
                change.membership = true;
            }
            for (record, _) in &entry.snapshot.worktrees {
                if record.bare || !(blind || entry.unwatched.contains(&record.id)) {
                    continue;
                }
                let key = (project, record.id);
                let interval = if self.work.get(&key) == Some(&git_watch::Interest::Active) {
                    FALLBACK_INTERVAL
                } else if self.work.contains_key(&key)
                    || (shown.contains(&key) && !self.shown.contains(&key))
                {
                    SHOWN_STALE
                } else {
                    continue;
                };
                if entry
                    .read_at
                    .get(&record.id)
                    .is_none_or(|read| now.saturating_duration_since(*read) >= interval)
                {
                    change.worktrees.insert(record.id);
                }
            }
            if change.topology || !change.worktrees.is_empty() {
                due.push((project, change));
            }
        }
        due
    }

    fn changed(&mut self, project: ProjectId, change: git_watch::Change) {
        let Some(entry) = self.entries.get_mut(&project) else {
            return;
        };
        if let Some(covered) = change.coverage {
            match &change.unwatched {
                Some(unwatched) => {
                    entry.fallback = None;
                    entry.unwatched.clone_from(unwatched);
                }
                None if covered => {
                    entry.fallback = None;
                    entry.unwatched.clear();
                }
                None => {
                    entry.fallback = Some(
                        change
                            .error
                            .clone()
                            .unwrap_or_else(|| "file watching unavailable".to_owned()),
                    );
                }
            }
        } else if let Some(error) = &change.error {
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
        result: Result<(GitHubAvailability, Vec<ArchivedWorktreePr>), String>,
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
            Entry::new(
                Snapshot {
                    repository: repository.clone(),
                    worktrees: loaded.worktrees.clone(),
                    heads: loaded.head_repositories.clone(),
                    tracks: HashMap::new(),
                    archived: Arc::default(),
                },
                generation,
                failure,
            ),
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

    /// Tells local Git which checkouts someone works in: those holding an
    /// open pane, in a channel or not, and the selected row. Cheap when
    /// nothing moved, as after most projections.
    pub(super) fn refresh_git_interest(&mut self) -> Task<Message> {
        let index = &self.worktree_paths;
        let mut work = BTreeMap::new();
        for card in self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
        {
            let Some(directory) = self.unchanneled_card_worktree_indexed(card, &index) else {
                continue;
            };
            let interest = if self.workspace.visibility(card.id) == Some(VisibilityClass::Visible) {
                git_watch::Interest::Active
            } else {
                git_watch::Interest::Shown
            };
            for row in index.rows(directory) {
                // The closest wins: a checkout in view and also out of view
                // is in view.
                let slot = work.entry(*row).or_insert(interest);
                *slot = (*slot).min(interest);
            }
        }
        if let Some(selected) = self.sidebar_state.selected_checkout() {
            work.insert(selected, git_watch::Interest::Active);
        }
        self.refresh_ui.git.set_work(work);
        self.start_git_reads()
    }

    /// The checkouts whose rows are drawn: in the sidebar, and every one
    /// while a Worktrees pane is on screen.
    fn shown_worktrees(&self) -> HashSet<(ProjectId, WorktreeId)> {
        let pane = self.work_ui.shows_worktrees()
            && self.durable.cards.iter().any(|card| {
                card.kind == CardKind::Work
                    && card.existence.is_open()
                    && self.workspace.visibility(card.id) == Some(VisibilityClass::Visible)
            });
        self.sidebar_model
            .projects
            .iter()
            .flat_map(|project| {
                project
                    .worktrees
                    .iter()
                    .filter(move |worktree| {
                        pane || self.sidebar_state.draws_worktree_row(project, worktree)
                    })
                    .map(move |worktree| (project.id, worktree.id))
            })
            .collect()
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
        let shown = self.shown_worktrees();
        for (project, change) in self.refresh_ui.git.due(&shown, Instant::now()) {
            self.refresh_ui.git.changed(project, change);
        }
        self.refresh_ui.git.shown = shown;
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
                self.refresh_ui
                    .finished(project, availability.refresh_succeeded());
                if matches!(availability, GitHubAvailability::Ready { .. })
                    && let Some(entry) = self.refresh_ui.git.entries.get_mut(&project)
                {
                    let saved = Arc::make_mut(&mut entry.snapshot.archived);
                    saved.associations = archived.clone();
                    saved.revision += 1;
                    self.refresh_ui.git.revalidate_archived(project);
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
                        // A proof from before a local commit waits for the
                        // queued read instead of clearing a valid PR first.
                        let settled = entry
                            .snapshot
                            .worktrees
                            .iter()
                            .filter(|(record, _)| !entry.snapshot.awaits_archived_proof(record))
                            .cloned()
                            .collect::<Vec<_>>();
                        apply_attached_worktree_prs(
                            row,
                            &settled,
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
                    self.start_git_reads(),
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
                    // Unwatched files alone are routine with many worktrees;
                    // only metadata the watcher cannot see is worth saying.
                    if let Some(error) = &change.error
                        && change.unwatched.is_none()
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
                    Ok((mut snapshot, plan)) => {
                        if entry.read_full_scan {
                            entry.read_failure = None;
                        }
                        if snapshot.archived.revision != entry.snapshot.archived.revision {
                            // A newer remote lookup owns the candidates and
                            // metadata; the older local read cannot restore
                            // a removed PR or overwrite its newer state.
                            snapshot.archived = entry.snapshot.archived.clone();
                        }
                        // A checkout first listed by this read was read by it.
                        let now = Instant::now();
                        let mut read_at = HashMap::with_capacity(snapshot.worktrees.len());
                        for (record, status) in &snapshot.worktrees {
                            if let Some(read) = entry.read_at.get(&record.id) {
                                read_at.insert(record.id, *read);
                            } else if status.is_some() {
                                read_at.insert(record.id, now);
                            }
                        }
                        entry.read_at = read_at;
                        let previous = std::mem::replace(&mut entry.snapshot, snapshot.clone());
                        let removed =
                            records_removed_outside(project, &previous, &snapshot.worktrees);
                        let reconfigure = plan.is_some();
                        let full_catalog = !entry.catalog_requested
                            || entry.plan.is_none()
                            || entry.read_full_scan;
                        let archived = snapshot.archived.associations.clone();
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
                        // A newer remote proof from before this read's HEAD
                        // waits for the queued recheck rather than clearing
                        // a still-valid PR in between.
                        let settled = snapshot
                            .worktrees
                            .iter()
                            .filter(|(record, _)| !snapshot.awaits_archived_proof(record))
                            .cloned()
                            .collect::<Vec<_>>();
                        if let Some(GitHubAvailability::Ready { pull_requests, .. }) =
                            self.github_availability.get(&project)
                        {
                            apply_attached_worktree_prs(
                                row,
                                &settled,
                                &pull_requests
                                    .iter()
                                    .map(|(pr, _)| pr.clone())
                                    .collect::<Vec<_>>(),
                                &archived,
                            );
                        } else {
                            // Local Git still revokes an archived proof when
                            // GitHub is unavailable. Leave unrelated cached
                            // recent PR presentation alone.
                            let old_archived = previous
                                .archived
                                .associations
                                .iter()
                                .map(|association| association.pull_request.number)
                                .collect::<HashSet<_>>();
                            let attached = row
                                .worktrees
                                .iter()
                                .filter(|worktree| {
                                    worktree
                                        .pull_request()
                                        .is_some_and(|pr| old_archived.contains(&pr.number))
                                })
                                .map(|worktree| worktree.id)
                                .collect::<HashSet<_>>();
                            let worktrees = settled
                                .into_iter()
                                .filter(|(record, _)| attached.contains(&record.id))
                                .collect::<Vec<_>>();
                            apply_attached_worktree_prs(row, &worktrees, &[], &archived);
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
                        self.refresh_ui.git.revalidate_archived(project);
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

    pub(super) fn start_git_reads(&mut self) -> Task<Message> {
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
            let interest = entry.interest(project, &state.work);
            let generation = entry.generation;
            let cancellation = CancellationToken::default();
            entry.cancellation = Some(cancellation.clone());
            let now = Instant::now();
            if change.topology {
                entry.last_full_scan = now;
            }
            for worktree in &change.worktrees {
                entry.read_at.insert(*worktree, now);
            }
            running += 1;
            tasks.push(cosmic::task::future(async move {
                let result = tokio::task::spawn_blocking(move || {
                    read_local_with_plan(
                        project,
                        snapshot,
                        change,
                        cancellation,
                        plan.as_deref(),
                        &interest,
                    )
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
    read_local_with_plan(
        project,
        snapshot,
        change,
        cancellation,
        None,
        &HashMap::new(),
    )
}

/// Reads what `change` names. `interest` says how much each checkout's
/// files are worth watching; one it does not name is watched for its Git
/// metadata alone.
fn read_local_with_plan(
    project: ProjectId,
    mut snapshot: Snapshot,
    change: git_watch::Change,
    cancellation: CancellationToken,
    previous_plan: Option<&git_watch::Project>,
    interest: &HashMap<WorktreeId, git_watch::Interest>,
) -> Result<(Snapshot, Option<git_watch::Project>), String> {
    let git = RepositoryService::system("git");
    if change.topology {
        // The listing names each checkout's branch and HEAD. One where
        // neither moved keeps the status it had: adding or removing one
        // worktree must not read every other one again.
        let mut previous = std::mem::take(&mut snapshot.worktrees)
            .into_iter()
            .map(|(record, status)| (record.id, (record, status)))
            .collect::<HashMap<_, _>>();
        snapshot.worktrees = git
            .list_worktrees(&snapshot.repository.common_dir, &cancellation)
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|record| record.bare || worktree_ui::worktree_directory_available(&record.path))
            .map(|record| {
                let status = previous
                    .remove(&record.id)
                    .filter(|(old, _)| {
                        old.path == record.path
                            && old.head_oid == record.head_oid
                            && old.branch == record.branch
                            && old.detached == record.detached
                    })
                    .and_then(|(_, status)| status);
                (record, status)
            })
            .collect();
    }
    // A shared-ref change can only move a worktree's head or its upstream
    // comparison. One ref read for the whole repository says which, so below
    // a status re-runs only where a head actually moved; every other
    // worktree keeps its recorded changes and recounts at most the one
    // comparison that moved. Failing to read the refs falls back to the
    // status reads, which stay authoritative.
    let refs = if change.references {
        snapshot
            .worktrees
            .iter()
            .find(|(record, _)| !record.bare)
            .and_then(|(record, _)| git.change_ref_listing(&record.path, &cancellation).ok())
            .flatten()
    } else {
        None
    };
    let mut recounted: HashMap<(String, String), (u64, u64)> = HashMap::new();
    let mut vanished = HashSet::new();
    for (record, status) in &mut snapshot.worktrees {
        // A checkout the listing just found, or found moved, has no status.
        let listed = change.topology && status.is_none();
        // Shared refs move every worktree's upstream comparison.
        if record.bare || (!listed && !change.references && !change.worktrees.contains(&record.id))
        {
            continue;
        }
        if !listed
            && !change.worktrees.contains(&record.id)
            && let Some(refs) = &refs
            && recount_from_refs(
                &git,
                refs,
                record,
                status,
                &mut snapshot.tracks,
                &mut recounted,
                &cancellation,
            )
        {
            if change.configuration {
                // The branch did not move, but what it resolves to through
                // the configuration — its remote and that remote's URLs —
                // may have, and the recorded head repository came from
                // that resolution.
                snapshot.heads.remove(&record.id);
                if let Some(branch) = status
                    .as_ref()
                    .and_then(|current| current.branch().map(str::to_owned))
                    && let Some(head) = crate::github::worktree_head_repository(
                        &record.path,
                        &branch,
                        &cancellation,
                    )
                {
                    snapshot.heads.insert(record.id, head);
                }
            }
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
    snapshot.tracks.retain(|id, _| surviving_ids.contains(id));
    // This function runs in the local blocking worker. Only a changed proof
    // runs ancestry Git; ordinary edits retain the proof without extra work.
    let mut archived = Vec::new();
    for association in &snapshot.archived.associations {
        let checkout = snapshot
            .worktrees
            .iter()
            .map(|(record, _)| record)
            .find(|record| record.id == association.worktree)
            .and_then(|record| Some((record, snapshot.heads.get(&record.id)?)))
            .filter(|(record, head_repository)| association.matches(record, head_repository));
        let Some((record, _)) = checkout else {
            // A checkout that is briefly elsewhere (another branch, a
            // rebase's detached HEAD) has not disproved the association. It
            // applies again only at the proved HEAD or a descendant.
            archived.push(association.clone());
            continue;
        };
        let Some(local_head) = &record.head_oid else {
            continue;
        };
        if !cancellation.is_cancelled()
            && (association.validated_head == *local_head
                || association.pull_request.head_ref_oid == *local_head
                || pr_head_is_ancestor(
                    &record.path,
                    &association.pull_request.head_ref_oid,
                    local_head,
                ))
        {
            archived.push(ArchivedWorktreePr {
                validated_head: local_head.clone(),
                ..association.clone()
            });
        }
    }
    archived.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    archived.dedup_by(|a, b| a.sort_key() == b.sort_key());
    snapshot.archived = Arc::new(ArchivedPrs {
        associations: archived,
        revision: snapshot.archived.revision,
    });
    let plan = (change.membership || change.topology || !vanished.is_empty())
        .then(|| {
            git_watch::Project::read_changed_with_interest(
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
                &|worktree| {
                    interest
                        .get(&worktree)
                        .copied()
                        .unwrap_or(git_watch::Interest::Metadata)
                },
            )
        })
        .transpose()?;
    Ok((snapshot, plan))
}

/// Settles one worktree from the shared refs alone, when they prove its
/// recorded status still holds. Returns false where only a status read can
/// say: the branch went away or was born, the head moved, or the upstream
/// configuration changed. The head repository stays as recorded, because
/// the branch it was resolved from did not change.
fn recount_from_refs(
    git: &RepositoryService,
    refs: &crate::repository::RefListing,
    record: &WorktreeRecord,
    status: &mut Option<StatusSnapshot>,
    tracks: &mut HashMap<WorktreeId, TrackedPair>,
    recounted: &mut HashMap<(String, String), (u64, u64)>,
    cancellation: &CancellationToken,
) -> bool {
    let Some(previous) = status.as_ref() else {
        return false;
    };
    let Some(branch) = record.branch.as_deref() else {
        // A detached head names no ref, so a ref change cannot move it.
        // Anything else without a branch needs a status read to explain it.
        return record.detached;
    };
    let Some(local) = refs
        .local
        .iter()
        .find(|local| local.name.as_str() == branch)
    else {
        return false;
    };
    let Some(head) = record.head_oid.as_deref() else {
        return false;
    };
    if local.oid.as_str() != head {
        return false;
    }
    match &previous.tracking {
        TrackingState::Compared {
            upstream,
            ahead,
            behind,
        } => {
            let Some(upstream_ref) = local.upstream.as_ref() else {
                return false;
            };
            if upstream_ref.as_str().strip_prefix("refs/remotes/") != Some(upstream.as_str()) {
                return false;
            }
            let Some(upstream_oid) = refs
                .remote
                .iter()
                .find(|remote| remote.name == *upstream_ref)
                .map(|remote| remote.oid.clone())
            else {
                return false;
            };
            if let Some(pair) = tracks.get(&record.id)
                && pair.head == head
                && pair.upstream == upstream_oid.as_str()
            {
                let (counted_ahead, counted_behind) = (pair.ahead, pair.behind);
                if (*ahead, *behind) != (counted_ahead, counted_behind) {
                    let mut next = previous.clone();
                    next.tracking = TrackingState::Compared {
                        upstream: upstream.clone(),
                        ahead: counted_ahead,
                        behind: counted_behind,
                    };
                    *status = Some(next);
                }
                return true;
            }
            let key = (head.to_owned(), upstream_oid.to_string());
            let counts = match recounted.get(&key) {
                Some(counts) => *counts,
                None => {
                    let Ok(head_oid) = crate::repository::Oid::parse(head) else {
                        return false;
                    };
                    match git.ahead_behind(&record.path, &head_oid, &upstream_oid, cancellation) {
                        Ok(counts) => {
                            recounted.insert(key, counts);
                            counts
                        }
                        Err(_) => return false,
                    }
                }
            };
            let mut next = previous.clone();
            next.tracking = TrackingState::Compared {
                upstream: upstream.clone(),
                ahead: counts.0,
                behind: counts.1,
            };
            *status = Some(next);
            tracks.insert(
                record.id,
                TrackedPair {
                    head: head.to_owned(),
                    upstream: upstream_oid.to_string(),
                    ahead: counts.0,
                    behind: counts.1,
                },
            );
            true
        }
        TrackingState::NoUpstream => local.upstream.is_none(),
        TrackingState::Unknown { upstream } => upstream.is_none() && local.upstream.is_none(),
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
/// First reads and full scans reassert every open row, including tombstone reopen.
/// Adopting a watch plan alone does not.
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
    pub(super) fn github_project_polling_ready(&self, project: ProjectId) -> bool {
        self.review_repositories
            .get(&project)
            .is_none_or(|repository| crate::github::polling::ready(&repository.authority(), false))
    }

    pub(super) fn github_read_task(&mut self, project: ProjectId) -> Task<Message> {
        if !self.github_project_polling_ready(project) {
            return Task::none();
        }
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
                let availability =
                    load_repository_github(&snapshot.repository, true, &cancellation);
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
            tracks: HashMap::new(),
            archived: Arc::default(),
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
            tracks: HashMap::new(),
            archived: Arc::default(),
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
            &HashMap::new(),
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
                .env("TMPDIR", directory.path())
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

    /// A clone with two worktrees, `main` and `feature`, both tracking
    /// their `origin` counterparts: (snapshot, seed, work, feature worktree).
    fn two_worktree_clone(root: &Path) -> (Snapshot, PathBuf, PathBuf, PathBuf) {
        let seed = root.join("seed");
        fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-b", "main"]);
        git(&seed, &["config", "user.name", "Fixture"]);
        git(&seed, &["config", "user.email", "fixture@example.invalid"]);
        fs::write(seed.join("tracked.txt"), "original\n").unwrap();
        git(&seed, &["add", "tracked.txt"]);
        git(&seed, &["commit", "-m", "fixture"]);
        git(&seed, &["branch", "feature"]);
        let origin = root.join("origin.git");
        git(
            root,
            &[
                "clone",
                "--bare",
                seed.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        );
        let work = root.join("work");
        git(
            root,
            &["clone", origin.to_str().unwrap(), work.to_str().unwrap()],
        );
        git(&work, &["config", "user.name", "Fixture"]);
        git(&work, &["config", "user.email", "fixture@example.invalid"]);
        let feature_worktree = root.join("wt-feature");
        git(
            &work,
            &[
                "worktree",
                "add",
                "--track",
                "-b",
                "feature",
                feature_worktree.to_str().unwrap(),
                "origin/feature",
            ],
        );
        let service = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        let repository = service.discover(&work, &cancellation).unwrap();
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
            .collect::<Vec<_>>();
        assert_eq!(worktrees.len(), 2);
        (
            Snapshot {
                repository,
                worktrees,
                heads: HashMap::new(),
                tracks: HashMap::new(),
                archived: Arc::default(),
            },
            seed,
            work,
            feature_worktree,
        )
    }

    fn status_of(snapshot: &Snapshot, branch: &str) -> StatusSnapshot {
        snapshot
            .worktrees
            .iter()
            .find(|(record, _)| record.branch.as_deref() == Some(branch))
            .and_then(|(_, status)| status.clone())
            .unwrap()
    }

    #[test]
    fn reference_change_recounts_moved_upstreams_without_rereading_status() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let (snapshot, seed, work, feature_worktree) = two_worktree_clone(root);
        let before = snapshot.clone();
        // Advance only feature on the remote, then fetch so that nothing but
        // shared refs moves in the working clone.
        git(&seed, &["checkout", "feature"]);
        fs::write(seed.join("tracked.txt"), "advanced\n").unwrap();
        git(&seed, &["commit", "-a", "-m", "advance feature"]);
        git(&seed, &["checkout", "main"]);
        git(
            &seed,
            &["push", root.join("origin.git").to_str().unwrap(), "feature"],
        );
        git(&work, &["fetch", "origin"]);
        // Uncommitted edits a status re-read would report; a reference-only
        // read must leave both recorded statuses alone, because a worktree's
        // own change event carries that news separately.
        fs::write(work.join("tracked.txt"), "dirty\n").unwrap();
        fs::write(feature_worktree.join("tracked.txt"), "dirty\n").unwrap();
        let read = |snapshot: Snapshot| {
            read_local_with_plan(
                ProjectId::new(),
                snapshot,
                git_watch::Change {
                    references: true,
                    ..Default::default()
                },
                CancellationToken::default(),
                None,
                &HashMap::new(),
            )
            .unwrap()
            .0
        };
        let updated = read(snapshot);
        // The worktree whose upstream stayed put keeps its recorded status
        // exactly: no status ran again to notice the dirty edit.
        assert_eq!(
            status_of(&updated, "refs/heads/main"),
            status_of(&before, "refs/heads/main")
        );
        // The worktree whose upstream moved is recounted against the new
        // tip, with its recorded changes left as they were.
        let feature = status_of(&updated, "refs/heads/feature");
        assert_eq!(
            feature.tracking,
            TrackingState::Compared {
                upstream: "origin/feature".into(),
                ahead: 0,
                behind: 1,
            }
        );
        assert_eq!(
            feature.entries,
            status_of(&before, "refs/heads/feature").entries
        );
        assert_eq!(
            feature.dirty,
            status_of(&before, "refs/heads/feature").dirty
        );
        // A second identical change reuses the counted pair: the same
        // result, still without re-reading either status.
        let updated = read(updated);
        assert_eq!(
            status_of(&updated, "refs/heads/main"),
            status_of(&before, "refs/heads/main")
        );
        assert_eq!(status_of(&updated, "refs/heads/feature"), feature);
    }

    #[test]
    fn reference_change_rereads_status_where_a_head_moved() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let (snapshot, seed, work, _feature_worktree) = two_worktree_clone(root);
        let before = snapshot.clone();
        // Advance main on the remote, fetch, then move the local branch to
        // the fetched tip the way a branch update made elsewhere does.
        fs::write(seed.join("tracked.txt"), "advanced\n").unwrap();
        git(&seed, &["commit", "-a", "-m", "advance main"]);
        git(
            &seed,
            &["push", root.join("origin.git").to_str().unwrap(), "main"],
        );
        git(&work, &["fetch", "origin"]);
        git(&work, &["update-ref", "refs/heads/main", "origin/main"]);
        let (updated, _) = read_local_with_plan(
            ProjectId::new(),
            snapshot,
            git_watch::Change {
                references: true,
                ..Default::default()
            },
            CancellationToken::default(),
            None,
            &HashMap::new(),
        )
        .unwrap();
        // Only a status read can report the new head, so it must have run.
        assert_ne!(
            status_of(&updated, "refs/heads/main").head,
            status_of(&before, "refs/heads/main").head
        );
        assert_eq!(
            status_of(&updated, "refs/heads/main").tracking,
            TrackingState::Compared {
                upstream: "origin/main".into(),
                ahead: 0,
                behind: 0,
            }
        );
    }

    #[test]
    fn configuration_change_reresolves_head_repositories() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let (snapshot, _seed, work, _feature_worktree) = two_worktree_clone(root);
        let before = snapshot.clone();
        assert!(snapshot.heads.is_empty());
        git(
            &work,
            &[
                "config",
                "remote.origin.url",
                "https://github.com/fixture/project.git",
            ],
        );
        let (updated, _) = read_local_with_plan(
            ProjectId::new(),
            snapshot,
            git_watch::Change {
                references: true,
                configuration: true,
                ..Default::default()
            },
            CancellationToken::default(),
            None,
            &HashMap::new(),
        )
        .unwrap();
        // No status re-ran — the recorded statuses are untouched — but the
        // head repositories follow the URLs the configuration now names.
        assert_eq!(
            status_of(&updated, "refs/heads/main"),
            status_of(&before, "refs/heads/main")
        );
        let expected = GitHubRepoKey::new("github.com", None, "fixture", "project").unwrap();
        assert_eq!(updated.heads.len(), 2);
        assert!(updated.heads.values().all(|head| *head == expected));
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
        state
            .entries
            .insert(project, Entry::new(previous.clone(), 1, None));
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
        app.refresh_ui
            .git
            .entries
            .insert(project, Entry::new(snapshot.clone(), 2, None));
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
                    last_full_scan: Instant::now() - FALLBACK_INTERVAL,
                    ..Entry::new(snapshot.clone(), 1, Some("watch limit".into()))
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
                last_full_scan: Instant::now() - FALLBACK_INTERVAL,
                ..Entry::new(
                    snapshot.clone(),
                    1,
                    Some("directory watch limit reached".into()),
                )
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

    /// Hundreds of worktrees cost nothing while idle: the metadata watch
    /// stands in for a timer everywhere but where someone works or looks.
    #[test]
    fn periodic_reads_follow_interest_not_the_worktree_count() {
        let directory = tempfile::tempdir().unwrap();
        let mut snapshot = fixture(&directory.path().join("repo"));
        let template = snapshot.worktrees[0].clone();
        for index in 0..800 {
            let mut record = template.0.clone();
            record.id = WorktreeId::new();
            record.path = directory.path().join(format!("worktree-{index}"));
            snapshot.worktrees.push((record, template.1.clone()));
        }
        let ids = snapshot
            .worktrees
            .iter()
            .map(|(record, _)| record.id)
            .collect::<Vec<_>>();
        let (own, worked, looked_at, idle, out_of_sight) = (ids[0], ids[1], ids[2], ids[3], ids[4]);
        let project = ProjectId::new();
        let mut state = State::default();
        let mut entry = Entry::new(snapshot, 1, None);
        let start = entry.last_full_scan;
        entry.read_at = ids.iter().map(|id| (*id, start)).collect();
        state.entries.insert(project, entry);
        state.set_work(
            [
                ((project, worked), git_watch::Interest::Active),
                ((project, out_of_sight), git_watch::Interest::Shown),
            ]
            .into(),
        );
        let interest = state.entries[&project].interest(project, &state.work);
        assert_eq!(interest[&worked], git_watch::Interest::Active);
        assert_eq!(interest[&own], git_watch::Interest::Shown);
        assert_eq!(interest[&out_of_sight], git_watch::Interest::Shown);
        assert_eq!(interest[&idle], git_watch::Interest::Metadata);

        // The budget covered the project's own checkout alone.
        state.changed(
            project,
            git_watch::Change {
                coverage: Some(false),
                error: Some("directory watch limit reached".into()),
                unwatched: Some(ids[1..].iter().copied().collect()),
                ..Default::default()
            },
        );
        let entry = &state.entries[&project];
        assert!(entry.fallback.is_none(), "the metadata is still watched");
        let shown = HashSet::from([(project, own), (project, looked_at)]);
        let reads = |state: &State, after: Duration| {
            state
                .due(&shown, start + after)
                .into_iter()
                .map(|(id, change)| {
                    assert_eq!(id, project);
                    (change.topology, change.worktrees)
                })
                .collect::<Vec<_>>()
        };
        assert!(reads(&state, FALLBACK_INTERVAL - Duration::from_secs(1)).is_empty());
        assert_eq!(
            reads(&state, FALLBACK_INTERVAL),
            [(false, BTreeSet::from([worked]))],
            "only the checkout someone works in on screen is polled"
        );
        assert_eq!(
            reads(&state, SHOWN_STALE),
            [(false, BTreeSet::from([worked, looked_at, out_of_sight]))],
            "a row come into view, or a checkout worked in out of sight, is \
             read once its status is old; the watched one and the 797 neither \
             shown nor worked in never are"
        );
        // A row that stays in view is not read again on a timer.
        state.shown = shown.clone();
        assert_eq!(
            reads(&state, SHOWN_STALE),
            [(false, BTreeSet::from([worked, out_of_sight]))]
        );
        state.shown.clear();

        // Metadata the watcher cannot see brings back the worktree list,
        // still without reading every checkout.
        state.changed(
            project,
            git_watch::Change {
                coverage: Some(false),
                error: Some("network filesystem".into()),
                ..Default::default()
            },
        );
        assert!(state.entries[&project].fallback.is_some());
        assert_eq!(
            reads(&state, FALLBACK_INTERVAL),
            [(true, BTreeSet::from([worked]))]
        );
    }

    /// Panes come into and out of view all the time; that re-plans watches
    /// only when it can change which checkouts' files are watched.
    #[test]
    fn work_moving_on_and_off_screen_replans_only_when_coverage_can_change() {
        let directory = tempfile::tempdir().unwrap();
        let mut snapshot = fixture(&directory.path().join("repo"));
        let template = snapshot.worktrees[0].clone();
        for index in 0..2 {
            let mut record = template.0.clone();
            record.id = WorktreeId::new();
            record.path = directory.path().join(format!("worktree-{index}"));
            snapshot.worktrees.push((record, template.1.clone()));
        }
        let (worked, other) = (snapshot.worktrees[1].0.id, snapshot.worktrees[2].0.id);
        let project = ProjectId::new();
        let mut state = State::default();
        state.entries.insert(project, Entry::new(snapshot, 1, None));
        let replans = |state: &mut State, work: &[(WorktreeId, git_watch::Interest)]| {
            state.queue.clear();
            state.entries.get_mut(&project).unwrap().pending = Default::default();
            state.set_work(
                work.iter()
                    .map(|(id, interest)| ((project, *id), *interest))
                    .collect(),
            );
            state.queue.contains(&project)
        };
        use git_watch::Interest::{Active, Shown};
        assert!(
            replans(&mut state, &[(worked, Active)]),
            "it gains file watches"
        );
        assert!(
            !replans(&mut state, &[(worked, Shown)]),
            "out of sight it keeps them, and the budget covers everything"
        );
        // The budget falls short of the other checkout someone works in.
        state.entries.get_mut(&project).unwrap().unwatched = BTreeSet::from([other]);
        assert!(replans(&mut state, &[(worked, Shown), (other, Active)]));
        assert!(
            replans(&mut state, &[(worked, Active), (other, Shown)]),
            "the order decides which of them the budget covers"
        );
        state.entries.get_mut(&project).unwrap().unwatched.clear();
        assert!(replans(&mut state, &[]), "both lose their watches");

        // The budget is shared: another project's own checkout going without
        // makes this project's order matter too.
        assert!(replans(&mut state, &[(worked, Active)]));
        let elsewhere = fixture(&directory.path().join("elsewhere"));
        let own = elsewhere.worktrees[0].0.id;
        let mut entry = Entry::new(elsewhere, 1, None);
        entry.unwatched = BTreeSet::from([own]);
        state.entries.insert(ProjectId::new(), entry);
        assert!(replans(&mut state, &[(worked, Shown)]));
    }

    #[test]
    fn a_new_worktree_reads_only_itself() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let snapshot = fixture(&first);
        // Edited outside any watch: only a read of this checkout would see it.
        fs::write(first.join("tracked.txt"), "edited\n").unwrap();
        let second = directory.path().join("second");
        git(
            &first,
            &["worktree", "add", "-b", "second", second.to_str().unwrap()],
        );
        let (updated, _) = read_local(
            ProjectId::new(),
            snapshot,
            git_watch::Change {
                topology: true,
                membership: true,
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(updated.worktrees.len(), 2);
        assert!(
            !status_of(&updated, "refs/heads/main").dirty.is_dirty(),
            "the existing checkout keeps the status it had"
        );
        assert!(updated.worktrees[1].1.is_some(), "the new one is read");
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
        snapshot.archived = Arc::new(ArchivedPrs {
            associations: vec![ArchivedWorktreePr {
                pull_request: pr,
                worktree: snapshot.worktrees[0].0.id,
                validated_head: snapshot.worktrees[0].0.head_oid.clone().unwrap(),
            }],
            revision: 0,
        });
        app.refresh_ui
            .git
            .entries
            .insert(project, Entry::new(snapshot.clone(), 2, None));
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
        state
            .entries
            .insert(project, Entry::new(snapshot, 1, Some("watch limit".into())));
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
        app.refresh_ui
            .git
            .entries
            .insert(project.id, Entry::new(snapshot.clone(), 1, None));
        (app, project)
    }

    #[test]
    fn resetting_same_branch_drops_its_archived_merged_pr() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let snapshot = fixture(&root);
        let repository_name = format!("reset-{}", snapshot.worktrees[0].0.id);
        let key = GitHubRepoKey::new("github.com", None, "acme", &repository_name).unwrap();
        git(
            &root,
            &[
                "remote",
                "add",
                "origin",
                &format!("https://github.com/acme/{repository_name}.git"),
            ],
        );
        git(&root, &["checkout", "-b", "feature"]);
        fs::write(root.join("tracked.txt"), "feature\n").unwrap();
        git(&root, &["commit", "-am", "feature"]);
        git(&root, &["checkout", "main"]);
        git(&root, &["merge", "--squash", "feature"]);
        git(&root, &["commit", "-m", "squashed feature"]);
        git(&root, &["checkout", "feature"]);
        let (snapshot, _) = read_local(
            ProjectId::new(),
            snapshot,
            git_watch::Change {
                topology: true,
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        let old_head = snapshot.worktrees[0].0.head_oid.clone().unwrap();
        let pr: crate::github::PullRequest = serde_json::from_value(serde_json::json!({
            "number":7,"title":"Previous feature","state":"MERGED",
            "headRepository":{"name":repository_name},"headRepositoryOwner":{"login":"acme"},
            "headRefName":"feature","headRefOid":old_head,"baseRefName":"main",
            "updatedAt":"2026-01-01T00:00:00Z",
            "url":format!("https://github.com/acme/{repository_name}/pull/7")
        }))
        .unwrap();
        // Only the external GitHub response is a fixture. Historical lookup,
        // local Git reads and the application's result handlers are real.
        fs::write(
            directory.path().join("prs.json"),
            serde_json::to_vec(&vec![pr]).unwrap(),
        )
        .unwrap();
        let executable = directory.path().join("gh");
        fs::write(
            &executable,
            "#!/bin/sh\ncat \"$(dirname \"$0\")/prs.json\"\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let archived = attached_worktree_pull_requests(
            &GitHubService::system(&executable),
            &key,
            &snapshot.worktrees,
            &[],
            &snapshot.heads,
            &CancellationToken::default(),
        );
        assert_eq!(archived.len(), 1);
        let (mut app, project) = catalog_app(&snapshot);
        app.sidebar_model.projects[0].worktrees[0].head_repository = Some(key.clone());
        drop(app.update_git(Action::Remote {
            project: project.id,
            generation: 1,
            result: Ok((
                GitHubAvailability::Ready {
                    repository_key: key,
                    pull_requests: Vec::new(),
                    refreshed_at: 0,
                },
                archived,
            )),
        }));
        assert_eq!(
            app.sidebar_model.projects[0].worktrees[0]
                .pull_request()
                .map(|pr| pr.number),
            Some(7)
        );

        git(&root, &["reset", "--hard", "main"]);
        let (reset, plan) = read_local(
            project.id,
            app.refresh_ui.git.entries[&project.id].snapshot.clone(),
            git_watch::Change {
                worktrees: [snapshot.worktrees[0].0.id].into_iter().collect(),
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(reset.worktrees[0].0.branch, snapshot.worktrees[0].0.branch);
        assert!(!pr_head_is_ancestor(
            &root,
            &old_head,
            reset.worktrees[0].0.head_oid.as_deref().unwrap()
        ));
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((reset, plan)),
        }));
        assert!(
            app.sidebar_model.projects[0].worktrees[0]
                .pull_request()
                .is_none(),
            "a reset branch must not retain the archived PR proved against its previous head; got {:?}",
            app.sidebar_model.projects[0].worktrees[0]
                .pull_request()
                .map(|pr| (pr.number, pr.state))
        );
    }

    fn archived_fixture(root: &Path) -> (Snapshot, GitHubRepoKey, ArchivedWorktreePr) {
        let snapshot = fixture(root);
        git(
            root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/repo.git",
            ],
        );
        git(root, &["checkout", "-b", "feature"]);
        git(root, &["commit", "--allow-empty", "-m", "old feature"]);
        let (snapshot, _) = read_local(
            ProjectId::new(),
            snapshot,
            git_watch::Change {
                topology: true,
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        let key = GitHubRepoKey::new("github.com", None, "acme", "repo").unwrap();
        let worktree = snapshot.worktrees[0].0.id;
        let oid = snapshot.worktrees[0].0.head_oid.clone().unwrap();
        let pr = serde_json::from_value(serde_json::json!({
            "number":7,"title":"Archived feature","state":"MERGED",
            "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"acme"},
            "headRefName":"feature","headRefOid":oid,"baseRefName":"main",
            "updatedAt":"2026-01-01T00:00:00Z","url":"https://github.com/acme/repo/pull/7"
        }))
        .unwrap();
        (
            snapshot,
            key,
            ArchivedWorktreePr {
                pull_request: pr,
                worktree,
                validated_head: oid,
            },
        )
    }

    fn archived_remote(
        app: &mut Cagents,
        project: ProjectId,
        key: &GitHubRepoKey,
        archived: Vec<ArchivedWorktreePr>,
    ) {
        drop(app.update_git(Action::Remote {
            project,
            generation: 1,
            result: Ok((
                GitHubAvailability::Ready {
                    repository_key: key.clone(),
                    pull_requests: Vec::new(),
                    refreshed_at: 0,
                },
                archived,
            )),
        }));
    }

    #[test]
    fn descendant_commit_revalidates_archived_pr_in_local_worker() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, key, archived) = archived_fixture(directory.path());
        let (mut app, project) = catalog_app(&snapshot);
        apply_local(&mut app.sidebar_model.projects[0], &snapshot);
        archived_remote(&mut app, project.id, &key, vec![archived]);
        git(
            directory.path(),
            &["commit", "--allow-empty", "-m", "later local work"],
        );
        let (updated, plan) = read_local(
            project.id,
            app.refresh_ui.git.entries[&project.id].snapshot.clone(),
            git_watch::Change {
                worktrees: [snapshot.worktrees[0].0.id].into_iter().collect(),
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(updated.archived.associations.len(), 1);
        assert_eq!(
            updated.archived.associations[0].validated_head,
            updated.worktrees[0].0.head_oid.as_deref().unwrap()
        );
        assert_ne!(
            updated.archived.associations[0].validated_head,
            snapshot.worktrees[0].0.head_oid.as_deref().unwrap()
        );
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((updated, plan)),
        }));
        assert_eq!(
            app.sidebar_model.projects[0].worktrees[0]
                .pull_request()
                .map(|pr| pr.number),
            Some(7)
        );
    }

    fn read_worktree(app: &Cagents, project: ProjectId) -> (Snapshot, Option<git_watch::Project>) {
        let snapshot = app.refresh_ui.git.entries[&project].snapshot.clone();
        let change = git_watch::Change {
            worktrees: [snapshot.worktrees[0].0.id].into_iter().collect(),
            ..Default::default()
        };
        read_local(project, snapshot, change, CancellationToken::default()).unwrap()
    }

    fn attached_number(app: &Cagents) -> Option<u64> {
        app.sidebar_model.projects[0].worktrees[0]
            .pull_request()
            .map(|pr| pr.number)
    }

    #[test]
    fn leaving_and_returning_to_proved_head_restores_archived_pr() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, key, archived) = archived_fixture(directory.path());
        let (mut app, project) = catalog_app(&snapshot);
        apply_local(&mut app.sidebar_model.projects[0], &snapshot);
        archived_remote(&mut app, project.id, &key, vec![archived]);
        assert_eq!(attached_number(&app), Some(7));
        for (step, expected) in [
            (&["checkout", "main"][..], None),
            (&["checkout", "feature"][..], Some(7)),
        ] {
            git(directory.path(), step);
            let (updated, plan) = read_worktree(&app, project.id);
            drop(app.update_git(Action::Read {
                project: project.id,
                generation: 1,
                result: Ok((updated, plan)),
            }));
            assert_eq!(attached_number(&app), expected, "after git {step:?}");
        }
    }

    #[test]
    fn remote_proof_from_before_local_commit_keeps_pr_until_recheck() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, key, archived) = archived_fixture(directory.path());
        let (mut app, project) = catalog_app(&snapshot);
        apply_local(&mut app.sidebar_model.projects[0], &snapshot);
        archived_remote(&mut app, project.id, &key, vec![archived.clone()]);
        git(
            directory.path(),
            &["commit", "--allow-empty", "-m", "later local work"],
        );
        let (updated, plan) = read_worktree(&app, project.id);
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((updated, plan)),
        }));
        assert_eq!(attached_number(&app), Some(7));
        // A refresh started before the commit returns its older proof.
        archived_remote(&mut app, project.id, &key, vec![archived]);
        assert_eq!(attached_number(&app), Some(7));
        let (rechecked, plan) = read_worktree(&app, project.id);
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((rechecked, plan)),
        }));
        assert_eq!(attached_number(&app), Some(7));
    }

    #[test]
    fn older_remote_proof_during_local_read_keeps_pr_until_recheck() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, key, archived) = archived_fixture(directory.path());
        let (mut app, project) = catalog_app(&snapshot);
        apply_local(&mut app.sidebar_model.projects[0], &snapshot);
        archived_remote(&mut app, project.id, &key, vec![archived.clone()]);
        git(
            directory.path(),
            &["commit", "--allow-empty", "-m", "later local work"],
        );
        let in_flight = read_worktree(&app, project.id);
        // A refresh started before the commit finishes before the read.
        archived_remote(&mut app, project.id, &key, vec![archived]);
        assert_eq!(attached_number(&app), Some(7));
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok(in_flight),
        }));
        assert_eq!(attached_number(&app), Some(7));
        let (rechecked, plan) = read_worktree(&app, project.id);
        assert_eq!(
            rechecked.archived.associations[0].validated_head,
            rechecked.worktrees[0].0.head_oid.as_deref().unwrap()
        );
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((rechecked, plan)),
        }));
        assert_eq!(attached_number(&app), Some(7));
    }

    #[test]
    fn another_checkout_of_branch_does_not_requeue_its_proof_check() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let (mut snapshot, _, archived) = archived_fixture(&root);
        let second = directory.path().join("second");
        git(
            &root,
            &[
                "worktree",
                "add",
                "--force",
                second.to_str().unwrap(),
                "feature",
            ],
        );
        snapshot.archived = Arc::new(ArchivedPrs {
            associations: vec![archived.clone()],
            revision: 0,
        });
        let (listed, _) = read_local(
            ProjectId::new(),
            snapshot,
            git_watch::Change {
                membership: true,
                topology: true,
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(listed.heads.len(), 2);
        let second_id = listed
            .worktrees
            .iter()
            .map(|(record, _)| record.id)
            .find(|id| *id != archived.worktree)
            .unwrap();
        // The shared branch moves, but only the second checkout is reread
        // before the first checkout's own change arrives.
        git(&second, &["reset", "--hard", "main"]);
        let (updated, _) = read_local(
            ProjectId::new(),
            listed,
            git_watch::Change {
                worktrees: [second_id].into_iter().collect(),
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        assert!(
            !updated
                .worktrees
                .iter()
                .any(|(record, _)| updated.awaits_archived_proof(record)),
            "the second checkout must not wait on the first checkout's proof"
        );
        assert!(
            updated
                .archived
                .associations
                .iter()
                .all(|association| association.worktree == archived.worktree)
        );
    }

    #[test]
    fn delayed_initial_and_remote_archives_cannot_restore_pr_after_reset() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, key, archived) = archived_fixture(directory.path());
        let (mut app, project) = catalog_app(&snapshot);
        apply_local(&mut app.sidebar_model.projects[0], &snapshot);
        git(directory.path(), &["reset", "--hard", "main"]);
        let (reset, plan) = read_local(
            project.id,
            snapshot.clone(),
            git_watch::Change {
                worktrees: [snapshot.worktrees[0].0.id].into_iter().collect(),
                ..Default::default()
            },
            CancellationToken::default(),
        )
        .unwrap();
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((reset, plan)),
        }));
        drop(app.update(Message::AttachedWorktreePrsResolved {
            project_id: project.id,
            generation: 1,
            worktrees: snapshot.worktrees.clone(),
            recent: Vec::new(),
            archived: vec![archived.clone()],
            expected: [(snapshot.worktrees[0].0.id, None)].into_iter().collect(),
        }));
        assert!(
            app.sidebar_model.projects[0].worktrees[0]
                .pull_request()
                .is_none()
        );
        archived_remote(&mut app, project.id, &key, vec![archived]);
        assert!(
            app.sidebar_model.projects[0].worktrees[0]
                .pull_request()
                .is_none()
        );
        let (checked, plan) = read_local(
            project.id,
            app.refresh_ui.git.entries[&project.id].snapshot.clone(),
            Default::default(),
            CancellationToken::default(),
        )
        .unwrap();
        assert!(
            checked.archived.associations.is_empty(),
            "a stale proof must be checked even when HEAD did not move again"
        );
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((checked, plan)),
        }));
        assert!(
            app.sidebar_model.projects[0].worktrees[0]
                .pull_request()
                .is_none()
        );
    }

    #[test]
    fn local_completion_preserves_newer_remote_archive_candidates_and_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, key, archived) = archived_fixture(directory.path());
        let (mut app, project) = catalog_app(&snapshot);
        apply_local(&mut app.sidebar_model.projects[0], &snapshot);
        archived_remote(&mut app, project.id, &key, vec![archived.clone()]);
        let old_read = read_local(
            project.id,
            app.refresh_ui.git.entries[&project.id].snapshot.clone(),
            Default::default(),
            CancellationToken::default(),
        )
        .unwrap();
        archived_remote(&mut app, project.id, &key, Vec::new());
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok(old_read.clone()),
        }));
        assert!(
            app.refresh_ui.git.entries[&project.id]
                .snapshot
                .archived
                .associations
                .is_empty()
        );
        assert!(
            app.sidebar_model.projects[0].worktrees[0]
                .pull_request()
                .is_none()
        );
        let mut updated = archived;
        updated.pull_request.title = "Updated remote title".into();
        archived_remote(&mut app, project.id, &key, vec![updated]);
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok(old_read),
        }));
        assert_eq!(
            app.sidebar_model.projects[0].worktrees[0]
                .pull_request()
                .unwrap()
                .title,
            "Updated remote title"
        );
    }

    #[test]
    fn failed_remote_refresh_keeps_valid_archive_but_local_reset_revokes_it() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, key, archived) = archived_fixture(directory.path());
        let (mut app, project) = catalog_app(&snapshot);
        apply_local(&mut app.sidebar_model.projects[0], &snapshot);
        archived_remote(&mut app, project.id, &key, vec![archived.clone()]);
        let mut recent = archived.pull_request;
        recent.number = 8;
        recent.head_ref_name = "another-feature".into();
        app.sidebar_model.projects[0].replace_pull_requests(vec![SidebarPullRequest::from_github(
            project.id,
            &recent,
            &[],
        )]);
        drop(app.update_git(Action::Remote {
            project: project.id,
            generation: 1,
            result: Err("fixture remote unavailable".into()),
        }));
        fs::write(directory.path().join("tracked.txt"), "ordinary edit\n").unwrap();
        let change = git_watch::Change {
            worktrees: [snapshot.worktrees[0].0.id].into_iter().collect(),
            ..Default::default()
        };
        let read = read_local(
            project.id,
            app.refresh_ui.git.entries[&project.id].snapshot.clone(),
            change.clone(),
            CancellationToken::default(),
        )
        .unwrap();
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok(read),
        }));
        assert_eq!(
            app.sidebar_model.projects[0].worktrees[0]
                .pull_request()
                .map(|pr| pr.number),
            Some(7)
        );

        git(directory.path(), &["reset", "--hard", "main"]);
        let read = read_local(
            project.id,
            app.refresh_ui.git.entries[&project.id].snapshot.clone(),
            change,
            CancellationToken::default(),
        )
        .unwrap();
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok(read),
        }));
        assert!(
            app.sidebar_model.projects[0].worktrees[0]
                .pull_request()
                .is_none()
        );
        assert_eq!(
            app.sidebar_model.projects[0]
                .display_pull_requests()
                .iter()
                .map(|pr| pr.number)
                .collect::<Vec<_>>(),
            vec![8]
        );
        assert!(!app.sidebar_model.projects[0].github_ready);
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
    fn established_local_reads_skip_equal_catalog_writes_but_keep_ui_and_rearm() {
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
            &HashMap::new(),
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
            &HashMap::new(),
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
        // Adopting a plan must replace the Arc and rearm, but rows that did not
        // change are not rewritten.
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
        assert_eq!(store.load_worktree(id).unwrap().unwrap(), branched);
        let next = app.refresh_ui.git.entries[&project.id]
            .plan
            .as_ref()
            .unwrap();
        assert!(!Arc::ptr_eq(&old_plan, next));
        // Native rearming itself remains covered by the existing watcher test.
    }

    #[test]
    fn a_plan_read_still_writes_a_row_whose_git_facts_changed() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = fixture(&directory.path().join("repo"));
        let (mut app, project) = catalog_app(&snapshot);
        let id = snapshot.worktrees[0].0.id;
        let (writer, _) = SessionRuntime::open(
            directory.path().join("window.sqlite3"),
            crate::model::BootId::new("catalog-plan-delta"),
        )
        .unwrap();
        let store = writer.operation_store();
        writer.save_project(project.clone());
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
        let baseline = store.load_worktree(id).unwrap().unwrap();

        git(
            &snapshot.worktrees[0].0.path,
            &["checkout", "-b", "plan-branch"],
        );
        let (snapshot, next_plan) = read_local_with_plan(
            project.id,
            snapshot,
            git_watch::Change {
                membership: true,
                worktrees: [id].into_iter().collect(),
                ..Default::default()
            },
            CancellationToken::default(),
            Some(&plan),
            &HashMap::new(),
        )
        .unwrap();
        let next_plan = next_plan.expect("a membership read must return a watch plan");
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((snapshot, Some(next_plan))),
        }));
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        let changed = store.load_worktree(id).unwrap().unwrap();
        assert_eq!(changed.branch.as_deref(), Some("plan-branch"));
        assert!(changed.generation > baseline.generation);
    }

    #[test]
    fn a_full_scan_read_reasserts_unchanged_catalog_rows() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = fixture(&directory.path().join("repo"));
        let (mut app, project) = catalog_app(&snapshot);
        let id = snapshot.worktrees[0].0.id;
        let (writer, _) = SessionRuntime::open(
            directory.path().join("window.sqlite3"),
            crate::model::BootId::new("catalog-full-scan"),
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

        let entry = app.refresh_ui.git.entries.get_mut(&project.id).unwrap();
        entry.read_full_scan = true;
        entry.cancellation = Some(CancellationToken::default());
        let plan = catalog_plan(project.id, &snapshot);
        drop(app.update_git(Action::Read {
            project: project.id,
            generation: 1,
            result: Ok((snapshot, Some(plan))),
        }));
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        assert!(store.load_worktree(id).unwrap().unwrap().generation > baseline.generation);
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
