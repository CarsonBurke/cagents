//! Bounded filesystem invalidation for sidebar Git state. The watcher never
//! runs Git or GitHub; its coalesced edges ask the UI's cancellable reader to do so.
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{
    model::{ProjectId, WorktreeId},
    repository::{CancellationToken, RepositoryService, WorktreeRecord},
};

const MAX_DIRECTORIES: usize = 8192;

fn watch_budget() -> usize {
    std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        // Leave half the kernel allowance for editors and other windows.
        .map(|limit| (limit / 2).clamp(1, 262_144))
        .unwrap_or(MAX_DIRECTORIES)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub id: WorktreeId,
    pub root: PathBuf,
    pub git_dir: PathBuf,
    branch: Option<String>,
    upstream: Option<String>,
    directories: BTreeSet<PathBuf>,
    polling: Option<String>,
}

/// Directory-only census includes empty nonignored folders without following
/// symlinks or descending into ignored build trees or nested repositories.
fn content_directories(
    root: &Path,
    ignored: &[crate::repository::RepoPath],
    cancellation: &CancellationToken,
) -> Result<(BTreeSet<PathBuf>, bool), String> {
    use std::os::fd::AsRawFd;
    let handle = crate::fs_safety::Root::open(root).map_err(|error| error.to_string())?;
    let ignored = ignored
        .iter()
        .map(|path| path.as_path())
        .collect::<BTreeSet<_>>();
    let limit = watch_budget();
    let mut pending = vec![PathBuf::new()];
    let mut directories = BTreeSet::new();
    while let Some(relative) = pending.pop() {
        if cancellation.is_cancelled() {
            return Err("watch planning cancelled".into());
        }
        let reader;
        let entries = if relative.as_os_str().is_empty() {
            handle.read_dir().map_err(|error| error.to_string())?
        } else {
            reader = match crate::fs_safety::open_beneath(&handle, &relative) {
                Ok(reader) => reader,
                Err(error) if error.is_not_found() => continue,
                Err(error) => return Err(error.to_string()),
            };
            std::fs::read_dir(format!("/proc/self/fd/{}", reader.as_raw_fd()))
                .map_err(|error| error.to_string())?
        };
        directories.insert(root.join(&relative));
        if directories.len() >= limit {
            return Ok((directories, true));
        }
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            if entry.file_name() == ".git" || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let child = relative.join(entry.file_name());
            if child.ancestors().any(|ancestor| ignored.contains(ancestor)) {
                continue;
            }
            // Nested checkouts and submodules have their own Git state. Their
            // already-known directory is covered, but never traverse its tree.
            if crate::fs_safety::metadata_beneath(&handle, &child.join(".git")).is_ok() {
                continue;
            }
            if pending.len() + directories.len() >= limit {
                return Ok((directories, true));
            }
            pending.push(child);
        }
    }
    Ok((directories, false))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: ProjectId,
    pub common_dir: PathBuf,
    pub worktrees: Vec<Worktree>,
    polling: Option<String>,
}

impl Project {
    pub fn read_changed(
        id: ProjectId,
        common_dir: PathBuf,
        records: &[WorktreeRecord],
        cancellation: &CancellationToken,
        previous: Option<&Self>,
        changed: &BTreeSet<WorktreeId>,
        upstreams: &BTreeMap<WorktreeId, String>,
    ) -> Result<Self, String> {
        Self::read_changed_with_inventory_limit(
            id,
            common_dir,
            records,
            cancellation,
            previous,
            changed,
            upstreams,
            watch_budget().min(100_000),
        )
    }

    fn read_changed_with_inventory_limit(
        id: ProjectId,
        common_dir: PathBuf,
        records: &[WorktreeRecord],
        cancellation: &CancellationToken,
        previous: Option<&Self>,
        changed: &BTreeSet<WorktreeId>,
        upstreams: &BTreeMap<WorktreeId, String>,
        inventory_limit: usize,
    ) -> Result<Self, String> {
        let git = RepositoryService::system("git");
        let mut worktrees = Vec::new();
        let mut planning_failure = None;
        let polling = network_filesystem(&common_dir)
            .map_err(|error| error.to_string())?
            .then(|| "network or FUSE filesystem may omit native change events".to_owned());
        for record in records.iter().filter(|record| !record.bare) {
            if !changed.contains(&record.id)
                && let Some(old) = previous.and_then(|previous| {
                    previous
                        .worktrees
                        .iter()
                        .find(|old| old.id == record.id && old.root == record.path)
                })
            {
                worktrees.push(old.clone());
                continue;
            }
            let planned = (|| -> Result<Worktree, String> {
                let actual_common = git
                    .common_dir(&record.path, cancellation)
                    .map_err(|error| error.to_string())?;
                if actual_common != common_dir {
                    return Err(format!(
                        "{} now belongs to another repository",
                        record.path.display()
                    ));
                }
                let git_dir = git
                    .git_dir(&record.path, cancellation)
                    .map_err(|error| error.to_string())?;
                let mut worktree_polling = network_filesystem(&record.path)
                    .map_err(|error| error.to_string())?
                    .then(|| "network or FUSE filesystem may omit native change events".to_owned());
                let files = match git.watch_files_projection(
                    &record.path,
                    inventory_limit,
                    cancellation,
                ) {
                    Ok(files) => {
                        if files.listing_truncated {
                            // The independent census below still covers every
                            // nonignored directory within the watch budget.
                            // Force-tracked children beyond this prefix need
                            // the explicit, quiet periodic fallback.
                            worktree_polling
                                .get_or_insert_with(|| "directory watch limit reached".into());
                        }
                        files
                    }
                    Err(error) => {
                        if cancellation.is_cancelled() {
                            return Err("watch planning cancelled".into());
                        }
                        worktree_polling = Some(error.to_string());
                        crate::repository::WorkingFiles::default()
                    }
                };
                // ls-files lists files, not empty directories. An agent can
                // create a folder long before its first file, so discover those
                // too. Git collapses ignored build trees before the filesystem
                // walk; force-tracked ancestors remain in the file-derived set.
                let ignored = git.ignored_files_with_limit(
                    &record.path,
                    &files.paths,
                    Some(inventory_limit),
                    cancellation,
                );

                let mut directories = BTreeSet::from([record.path.clone()]);
                for file in files.paths {
                    let path = record.path.join(file.as_path());
                    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                        worktree_polling.get_or_insert_with(|| {
                            "submodule contents require a periodic local Git check".into()
                        });
                    }

                    let mut parent = path.parent();
                    while let Some(directory) = parent.filter(|path| path.starts_with(&record.path))
                    {
                        directories.insert(directory.to_owned());
                        parent = directory.parent();
                    }
                }
                match ignored {
                    Ok(ignored) if !ignored.listing_truncated => {
                        match content_directories(
                            &record.path,
                            &ignored.ignored_directories,
                            cancellation,
                        ) {
                            Ok((content, incomplete)) => {
                                directories.extend(content);
                                if incomplete {
                                    worktree_polling.get_or_insert_with(|| {
                                        "directory watch limit reached".into()
                                    });
                                }
                            }
                            Err(error) => worktree_polling = Some(error),
                        }
                    }
                    Ok(_) => {
                        // Without complete ignore classification, descending
                        // could walk an ignored build tree. Keep the verified
                        // inventory/root/admin coverage and poll the rest.
                        worktree_polling
                            .get_or_insert_with(|| "directory watch limit reached".into());
                    }
                    Err(error) => worktree_polling = Some(error.to_string()),
                }
                if cancellation.is_cancelled() {
                    return Err("watch planning cancelled".into());
                }
                Ok(Worktree {
                    id: record.id,
                    root: record.path.clone(),
                    git_dir,
                    branch: record.branch.clone(),
                    upstream: upstreams.get(&record.id).cloned(),
                    directories,
                    polling: worktree_polling,
                })
            })();
            match planned {
                Ok(worktree) => worktrees.push(worktree),
                Err(error) => {
                    planning_failure = Some(error.clone());
                    tracing::warn!(worktree = %record.path.display(), %error, "cannot update worktree watches; retaining sibling coverage");
                    if let Some(old) = previous.and_then(|project| {
                        project
                            .worktrees
                            .iter()
                            .find(|worktree| worktree.id == record.id)
                    }) {
                        worktrees.push(old.clone());
                    }
                }
            }
        }
        let polling = polling.or(planning_failure).or_else(|| {
            worktrees
                .iter()
                .find_map(|worktree| worktree.polling.clone())
        });
        Ok(Self {
            id,
            common_dir,
            worktrees,
            polling,
        })
    }

    pub fn refresh_upstreams(&self, upstreams: &BTreeMap<WorktreeId, String>) -> Option<Self> {
        if self
            .worktrees
            .iter()
            .all(|worktree| worktree.upstream.as_ref() == upstreams.get(&worktree.id))
        {
            return None;
        }
        let mut next = self.clone();
        for worktree in &mut next.worktrees {
            worktree.upstream = upstreams.get(&worktree.id).cloned();
        }
        Some(next)
    }

    pub fn matches(&self, common_dir: &Path, records: &[WorktreeRecord]) -> bool {
        self.common_dir == common_dir
            && self.worktrees.len() == records.iter().filter(|record| !record.bare).count()
            && self.worktrees.iter().all(|previous| {
                records.iter().any(|record| {
                    previous.id == record.id
                        && previous.root == record.path
                        && previous.branch == record.branch
                })
            })
    }

    /// Git's own bookkeeping: the directories whose entries change when a
    /// worktree is added, removed, locked, or moved to another commit. A
    /// handful per worktree, so they are registered ahead of any content.
    fn metadata_paths(&self) -> BTreeMap<PathBuf, RecursiveMode> {
        let mut paths = BTreeMap::from([(self.common_dir.clone(), RecursiveMode::NonRecursive)]);
        if let Some(parent) = self.common_dir.parent() {
            paths.insert(parent.to_owned(), RecursiveMode::NonRecursive);
        }
        paths.insert(self.common_dir.join("info"), RecursiveMode::NonRecursive);
        for name in ["refs", "worktrees", "logs/refs"] {
            paths.insert(self.common_dir.join(name), RecursiveMode::Recursive);
        }
        for worktree in &self.worktrees {
            if let Some(parent) = worktree.root.parent() {
                paths.insert(parent.to_owned(), RecursiveMode::NonRecursive);
            }
            paths.insert(worktree.git_dir.clone(), RecursiveMode::NonRecursive);
            paths.insert(worktree.git_dir.join("refs"), RecursiveMode::Recursive);
        }
        paths
    }

    /// Every directory on the way to a tracked file: one watch each, and by
    /// far the larger share of a checkout's registrations.
    fn content_paths(&self) -> impl Iterator<Item = &Path> {
        self.worktrees.iter().flat_map(|worktree| {
            worktree
                .directories
                .iter()
                .filter(|directory| {
                    !directory
                        .ancestors()
                        .take_while(|path| path.starts_with(&worktree.root))
                        .any(|path| {
                            std::fs::symlink_metadata(path)
                                .is_ok_and(|metadata| metadata.file_type().is_symlink())
                        })
                })
                .map(PathBuf::as_path)
        })
    }

    fn change(&self, event: &Event) -> Option<Change> {
        if matches!(event.kind, EventKind::Access(_)) {
            return None;
        }
        let mut change = Change::default();
        for path in &event.paths {
            // Git writes temporary locks before publishing the actual file.
            // Reading those creates duplicate work and can race the mutation.
            if path.starts_with(&self.common_dir)
                && path
                    .extension()
                    .is_some_and(|extension| extension == "lock")
            {
                continue;
            }
            if path == &self.common_dir {
                change.topology = true;
                change.membership = true;
            }
            let common = path.strip_prefix(&self.common_dir).ok();
            if common.is_some_and(|relative| {
                relative == Path::new("config")
                    || relative == Path::new("packed-refs")
                    || relative == Path::new("info/exclude")
            }) {
                change
                    .worktrees
                    .extend(self.worktrees.iter().map(|worktree| worktree.id));
                change.metadata = true;
                change.membership |= common != Some(Path::new("packed-refs"));
                continue;
            }
            if common.is_some_and(|relative| {
                relative == Path::new("refs/stash") || relative == Path::new("logs/refs/stash")
            }) {
                change.metadata = true;
                change.shared_snapshot = true;
                continue;
            }
            if let Some(relative) = common.filter(|relative| relative.starts_with("refs")) {
                change.metadata = true;
                for worktree in &self.worktrees {
                    if worktree
                        .branch
                        .as_deref()
                        .is_some_and(|branch| relative == Path::new(branch))
                        || worktree.upstream.as_ref().is_some_and(|upstream| {
                            relative == Path::new(&format!("refs/remotes/{upstream}"))
                                || relative == Path::new(&format!("refs/heads/{upstream}"))
                        })
                    {
                        change.worktrees.insert(worktree.id);
                    }
                }
                continue;
            }
            if common.is_some_and(|relative| relative.starts_with("worktrees"))
                && !self.worktrees.iter().any(|worktree| {
                    worktree.git_dir != self.common_dir
                        && path != &worktree.git_dir
                        && path.starts_with(&worktree.git_dir)
                })
            {
                change.topology = true;
                change.membership = true;
            }
            let metadata_owner = self
                .worktrees
                .iter()
                .filter(|worktree| path.starts_with(&worktree.git_dir))
                .max_by_key(|worktree| worktree.git_dir.components().count())
                .map(|worktree| worktree.id);
            for worktree in &self.worktrees {
                if path == &worktree.root.join(".git") {
                    change.topology = true;
                    change.membership = true;
                    change.worktrees.insert(worktree.id);
                }
                if metadata_owner == Some(worktree.id) {
                    change.worktrees.insert(worktree.id);
                    let name = path.file_name();
                    change.metadata = true;
                    // Index publication can turn tracked paths into untracked
                    // paths without a content event (git rm --cached). Ref and
                    // operation metadata alone retain the complete cached count.
                    change.untracked |= name.is_some_and(|name| name == "index");
                    change.membership |=
                        name.is_some_and(|name| name == "HEAD" || name == "config.worktree");
                    change.topology |= path
                        .file_name()
                        .is_some_and(|name| name == "locked" || name == "gitdir");
                } else if !path.starts_with(&worktree.git_dir)
                    && (worktree.directories.contains(path)
                        || path
                            .parent()
                            .is_some_and(|parent| worktree.directories.contains(parent)))
                {
                    change.worktrees.insert(worktree.id);
                    // Creation, removal and rename can change the untracked
                    // count. Ordinary file writes retain the last complete count.
                    change.untracked |= matches!(event.kind,
                        EventKind::Create(_) | EventKind::Remove(_)
                        | EventKind::Modify(notify::event::ModifyKind::Name(_)));
                    // A removed/recreated ancestor needs new directory watches;
                    // simple file edits keep the existing tracked-path census.
                    change.membership |= worktree.directories.contains(path)
                        || path.file_name().is_some_and(|name| name == ".gitignore")
                        || matches!(
                            event.kind,
                            EventKind::Create(notify::event::CreateKind::Folder)
                                | EventKind::Remove(notify::event::RemoveKind::Folder)
                        );
                    change.topology |= path == &worktree.root;
                }
            }
        }
        (change.topology || change.metadata || !change.worktrees.is_empty()).then_some(change)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Change {
    pub worktrees: BTreeSet<WorktreeId>,
    pub topology: bool,
    pub membership: bool,
    /// A content entry appeared/disappeared; replace the cached untracked count.
    pub untracked: bool,
    /// Index, HEAD, refs or config changed, invalidating cached panel metadata.
    pub metadata: bool,
    /// Shared stash data changes the one bound panel, without rereading every checkout.
    pub shared_snapshot: bool,
    pub error: Option<String>,
    pub coverage: Option<bool>,
}

impl Change {
    pub fn merge(&mut self, other: Self) {
        self.worktrees.extend(other.worktrees);
        self.topology |= other.topology;
        self.membership |= other.membership;
        self.untracked |= other.untracked;
        self.metadata |= other.metadata;
        self.shared_snapshot |= other.shared_snapshot;
        if let Some(coverage) = other.coverage {
            self.coverage = Some(coverage);
            if coverage {
                self.error = None;
            }
        }
        if other.error.is_some() {
            self.error = other.error;
        }
    }
}

#[derive(Default)]
struct Pending {
    projects: BTreeMap<ProjectId, Arc<Project>>,
    roots: BTreeMap<PathBuf, BTreeSet<ProjectId>>,
    changes: BTreeMap<ProjectId, Change>,
    faulted: bool,
}

#[derive(Default)]
struct Configuration {
    projects: Vec<Arc<Project>>,
    stopped: bool,
    priority: Vec<ProjectId>,
    selected: Vec<WorktreeId>,
}

impl Pending {
    fn event(&mut self, event: notify::Result<Event>) {
        self.faulted |= event.is_err();
        let affected = match &event {
            Ok(event) if !event.need_rescan() => event
                .paths
                .iter()
                .flat_map(|path| path.ancestors())
                .filter_map(|path| self.roots.get(path))
                .flatten()
                .copied()
                .collect::<BTreeSet<_>>(),
            _ => self.projects.keys().copied().collect(),
        };
        for id in affected {
            let Some(project) = self.projects.get(&id) else {
                continue;
            };
            let change = match &event {
                Ok(event) if !event.need_rescan() => project.change(event),
                Ok(_) => Some(Change {
                    worktrees: project.worktrees.iter().map(|row| row.id).collect(),
                    topology: true,
                    membership: true,
                    ..Change::default()
                }),
                Err(error) => Some(Change {
                    worktrees: project.worktrees.iter().map(|row| row.id).collect(),
                    topology: true,
                    membership: true,
                    error: Some(error.to_string()),
                    coverage: Some(false),
                    ..Change::default()
                }),
            };
            if let Some(change) = change {
                self.changes.entry(id).or_default().merge(change);
            }
        }
    }
}

/// What the watcher should hold for a set of projects, and which of them it
/// could not cover in full.
#[derive(Default)]
struct Registrations {
    desired: BTreeMap<PathBuf, RecursiveMode>,
    owners: BTreeMap<PathBuf, BTreeSet<ProjectId>>,
    failures: BTreeMap<ProjectId, String>,
}

impl Registrations {
    /// Keep background admin coverage when it fits alongside selected content.
    /// Under tighter pressure the hot workspaces win and omitted coverage uses
    /// the quiet polling fallback.
    #[cfg(test)]
    fn within(projects: &BTreeMap<ProjectId, Arc<Project>>, limit: usize) -> Self {
        Self::prioritized(projects, &[], &[], limit)
    }

    fn prioritized(
        projects: &BTreeMap<ProjectId, Arc<Project>>,
        priority: &[ProjectId],
        selected: &[WorktreeId],
        limit: usize,
    ) -> Self {
        let mut registrations = Self::default();
        let mut metadata_order = projects.iter().collect::<Vec<_>>();
        metadata_order.sort_by_key(|(id, _)| {
            priority
                .iter()
                .position(|candidate| candidate == *id)
                .unwrap_or(usize::MAX)
        });
        let hot_projects = projects
            .iter()
            .filter(|(_, project)| {
                project
                    .worktrees
                    .iter()
                    .any(|worktree| selected.contains(&worktree.id))
            })
            .map(|(id, _)| *id)
            .collect::<BTreeSet<_>>();
        let mut reserved = projects
            .values()
            .flat_map(|project| project.metadata_paths().into_keys())
            .filter(|path| path.is_dir())
            .collect::<BTreeSet<_>>();
        for project in projects.values() {
            for worktree in project
                .worktrees
                .iter()
                .filter(|worktree| selected.contains(&worktree.id))
            {
                reserved.extend(
                    project
                        .content_paths()
                        .filter(|path| path.starts_with(&worktree.root))
                        .map(Path::to_owned),
                );
            }
        }
        // Reserve hot content even when all admin paths fit but leave too
        // little room for the selected workspaces. When both fit, keep every
        // project's admin edges before allocating any background content.
        let oversubscribed = reserved.len() > limit;
        for (&id, project) in metadata_order.iter().copied() {
            if let Some(reason) = &project.polling {
                registrations.failures.insert(id, reason.clone());
            }
            if !oversubscribed || hot_projects.contains(&id) {
                for (path, mode) in project.metadata_paths() {
                    registrations.claim(id, path, mode, limit);
                }
            }
        }
        let mut ordered = projects.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|(id, _)| {
            priority
                .iter()
                .position(|candidate| candidate == *id)
                .unwrap_or(usize::MAX)
        });
        // Share the available content budget between the active binding and
        // independently selected worktrees before filling in background ones.
        // Round-robin prevents a large active tree starving a selected project.
        let mut preferred = selected
            .iter()
            .filter_map(|selected| {
                projects.iter().find_map(|(&id, project)| {
                    let worktree = project
                        .worktrees
                        .iter()
                        .find(|worktree| worktree.id == *selected)?;
                    let paths = project
                        .content_paths()
                        .filter(|path| path.starts_with(&worktree.root))
                        .collect::<Vec<_>>();
                    Some((id, paths.into_iter()))
                })
            })
            .collect::<Vec<_>>();
        loop {
            let mut progressed = false;
            for (id, paths) in &mut preferred {
                if let Some(path) = paths.next() {
                    registrations.claim(*id, path.to_owned(), RecursiveMode::NonRecursive, limit);
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        if oversubscribed {
            for (&id, project) in metadata_order {
                if !hot_projects.contains(&id) {
                    for (path, mode) in project.metadata_paths() {
                        registrations.claim(id, path, mode, limit);
                    }
                }
            }
        }
        for (&id, project) in ordered {
            for path in project.content_paths() {
                registrations.claim(id, path.to_owned(), RecursiveMode::NonRecursive, limit);
            }
        }
        registrations
    }

    fn claim(&mut self, id: ProjectId, path: PathBuf, mode: RecursiveMode, limit: usize) {
        self.owners.entry(path.clone()).or_default().insert(id);
        // Never follow a tracked symlink into another checkout. Missing
        // ancestors remain covered by their existing parent; a creation event
        // rebuilds this registration.
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.failures.insert(id, error.to_string());
                return;
            }
        }
        if self.desired.len() >= limit && !self.desired.contains_key(&path) {
            self.failures
                .insert(id, "directory watch limit reached".to_owned());
            return;
        }
        self.desired
            .entry(path)
            .and_modify(|old| {
                if mode == RecursiveMode::Recursive {
                    *old = mode;
                }
            })
            .or_insert(mode);
    }
}

/// One native watcher for the window, with at most one pending wake regardless
/// of event rate. Registration happens on its coordinator, never the UI thread.
pub(crate) struct Monitor {
    commands: mpsc::SyncSender<()>,
    configuration: Arc<Mutex<Configuration>>,
    pending: Arc<Mutex<Pending>>,
    pub receiver: async_channel::Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn start() -> std::io::Result<Self> {
        let (commands, requests) = mpsc::sync_channel(1);
        let configuration = Arc::new(Mutex::new(Configuration::default()));
        let desired_configuration = configuration.clone();
        let (wake, receiver) = async_channel::bounded(1);
        let pending = Arc::new(Mutex::new(Pending::default()));
        let state = pending.clone();
        let thread = thread::Builder::new()
            .name("git-watch".into())
            .spawn(move || {
                let create_watcher = || {
                    let events = state.clone();
                    let signal = wake.clone();
                    notify::recommended_watcher(move |event| {
                        let mut pending = events.lock().unwrap_or_else(|p| p.into_inner());
                        pending.event(event);
                        if !pending.changes.is_empty() {
                            let _ = signal.try_send(());
                        }
                    })
                };
                let mut watcher = create_watcher();
                let mut registered: BTreeMap<PathBuf, (RecursiveMode, u64, u64)> = BTreeMap::new();
                while requests.recv().is_ok() {
                    let (projects, priority, selected) = {
                        let configuration = desired_configuration
                            .lock()
                            .unwrap_or_else(|p| p.into_inner());
                        if configuration.stopped {
                            break;
                        }
                        (
                            configuration.projects.clone(),
                            configuration.priority.clone(),
                            configuration.selected.clone(),
                        )
                    };
                    let projects = projects
                        .into_iter()
                        .map(|project: Arc<Project>| (project.id, project))
                        .collect::<BTreeMap<_, _>>();
                    if watcher.is_err() {
                        watcher = create_watcher();
                    }
                    let old = {
                        let mut pending = state.lock().unwrap_or_else(|p| p.into_inner());
                        pending.changes.retain(|id, _| projects.contains_key(id));
                        pending.roots.clear();
                        for (&id, project) in &projects {
                            pending
                                .roots
                                .entry(project.common_dir.clone())
                                .or_default()
                                .insert(id);
                            for worktree in &project.worktrees {
                                pending
                                    .roots
                                    .entry(worktree.root.clone())
                                    .or_default()
                                    .insert(id);
                            }
                        }
                        std::mem::replace(&mut pending.projects, projects.clone())
                    };
                    let Registrations {
                        mut desired,
                        owners,
                        mut failures,
                    } = Registrations::prioritized(&projects, &priority, &selected, watch_budget());
                    // A recursive metadata watch already owns its descendants.
                    // Avoid overlapping registrations, whose removal can also
                    // remove a descendant's native watch on some backends.
                    let recursive = desired
                        .iter()
                        .filter(|(_, mode)| **mode == RecursiveMode::Recursive)
                        .map(|(path, _)| path.clone())
                        .collect::<Vec<_>>();
                    desired.retain(|path, _| {
                        !recursive
                            .iter()
                            .any(|root| path != root && path.starts_with(root))
                    });
                    let rearmed = match &mut watcher {
                        Ok(watcher) => {
                            let faulted = std::mem::take(
                                &mut state.lock().unwrap_or_else(|p| p.into_inner()).faulted,
                            );
                            if faulted {
                                for path in registered.keys() {
                                    let _ = watcher.unwatch(path);
                                }
                                registered.clear();
                            }
                            reconcile(watcher, &mut registered, &desired, &owners, &mut failures)
                        }
                        Err(error) => {
                            failures.extend(projects.keys().map(|id| (*id, error.to_string())));
                            BTreeSet::new()
                        }
                    };
                    let mut pending = state.lock().unwrap_or_else(|p| p.into_inner());
                    for (&id, project) in &projects {
                        let coverage = !failures.contains_key(&id);
                        let changed = project
                            .worktrees
                            .iter()
                            .filter(|worktree| {
                                old.get(&id).and_then(|project| {
                                    project.worktrees.iter().find(|old| old.id == worktree.id)
                                }) != Some(*worktree)
                            })
                            .map(|worktree| worktree.id)
                            .collect::<BTreeSet<_>>();
                        let worktrees = if changed.is_empty() && rearmed.contains(&id) {
                            project
                                .worktrees
                                .iter()
                                .map(|worktree| worktree.id)
                                .collect()
                        } else {
                            changed
                        };
                        // Acknowledge coverage even when the paths did not change:
                        // this settles fallback after a successful retry.
                        pending.changes.entry(id).or_default().merge(Change {
                            worktrees,
                            coverage: Some(coverage),
                            error: failures.remove(&id),
                            ..Change::default()
                        });
                    }
                    if !pending.changes.is_empty() {
                        let _ = wake.try_send(());
                    }
                }
            })?;
        Ok(Self {
            commands,
            configuration,
            pending,
            receiver,
            thread: Some(thread),
        })
    }

    pub fn configure(&self, projects: Vec<Arc<Project>>) {
        self.configuration
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .projects = projects;
        let _ = self.commands.try_send(());
    }

    pub fn prioritize(&self, projects: Vec<ProjectId>, selected: Vec<WorktreeId>) {
        let mut configuration = self.configuration.lock().unwrap_or_else(|p| p.into_inner());
        if configuration.priority != projects || configuration.selected != selected {
            configuration.priority = projects;
            configuration.selected = selected;
            let _ = self.commands.try_send(());
        }
    }

    pub fn drain(&self) -> BTreeMap<ProjectId, Change> {
        std::mem::take(
            &mut self
                .pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .changes,
        )
    }
}

fn reconcile(
    watcher: &mut RecommendedWatcher,
    registered: &mut BTreeMap<PathBuf, (RecursiveMode, u64, u64)>,
    desired: &BTreeMap<PathBuf, RecursiveMode>,
    owners: &BTreeMap<PathBuf, BTreeSet<ProjectId>>,
    failures: &mut BTreeMap<ProjectId, String>,
) -> BTreeSet<ProjectId> {
    let mut rearmed = BTreeSet::new();
    registered.retain(|path, (mode, device, inode)| {
        if desired.get(path) == Some(&*mode)
            && std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.dev() == *device && metadata.ino() == *inode)
        {
            true
        } else {
            let _ = watcher.unwatch(path);
            false
        }
    });
    for (path, &mode) in desired {
        if registered.contains_key(path) {
            continue;
        }
        match watcher.watch(path, mode) {
            Ok(()) => {
                if let Ok(metadata) = std::fs::symlink_metadata(path) {
                    registered.insert(path.clone(), (mode, metadata.dev(), metadata.ino()));
                }
                if let Some(ids) = owners.get(path) {
                    rearmed.extend(ids);
                }
            }
            Err(error) => {
                for &id in owners.get(path).into_iter().flatten() {
                    failures.insert(id, error.to_string());
                }
            }
        }
    }
    rearmed
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.configuration
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopped = true;
        let _ = self.commands.try_send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Inotify cannot see changes performed by another machine/server. FUSE can
/// have the same limitation, so keep local polling alongside available edges.
fn network_filesystem(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: path is NUL-terminated and stat points to a writable statfs.
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: successful statfs initialized the complete structure.
    let magic = unsafe { stat.assume_init() }.f_type as u64;
    Ok(matches!(
        magic,
        0x6969 | 0x517b | 0xff534d42 | 0xfe534d42 | 0x65735546
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{DataChange, ModifyKind};
    use std::time::{Duration, Instant};

    fn project(root: &Path) -> Project {
        let first = root.join("first");
        let linked = root.join("linked");
        let common = first.join(".git");
        Project {
            id: ProjectId::new(),
            common_dir: common.clone(),
            polling: None,
            worktrees: vec![
                Worktree {
                    id: WorktreeId::new(),
                    root: first.clone(),
                    git_dir: common.clone(),
                    branch: Some("refs/heads/main".into()),
                    upstream: Some("origin/main".into()),
                    directories: BTreeSet::from([first.clone(), first.join("src")]),
                    polling: None,
                },
                Worktree {
                    id: WorktreeId::new(),
                    root: linked.clone(),
                    git_dir: common.join("worktrees/linked"),
                    branch: Some("refs/heads/linked".into()),
                    upstream: Some("origin/linked".into()),
                    directories: BTreeSet::from([linked.clone(), linked.join("src")]),
                    polling: None,
                },
            ],
        }
    }

    fn modified(path: PathBuf) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any))).add_path(path)
    }

    #[test]
    fn content_membership_events_refresh_full_counts_without_admin_fanout() {
        let project = project(Path::new("/repo"));
        let worktree = &project.worktrees[0];
        for kind in [
            EventKind::Create(notify::event::CreateKind::File),
            EventKind::Remove(notify::event::RemoveKind::File),
            EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Both)),
        ] {
            let change = project.change(&Event::new(kind).add_path(worktree.root.join("src/new.txt"))).unwrap();
            assert!(change.untracked);
            assert_eq!(change.worktrees, BTreeSet::from([worktree.id]));
            assert!(!change.metadata);
            assert!(!change.topology);
        }
        assert!(!project.change(&modified(worktree.root.join("src/main.rs"))).unwrap().untracked);
    }

    #[test]
    fn file_changes_are_local_but_shared_refs_invalidate_each_worktree() {
        let project = project(Path::new("/repo"));
        let first = &project.worktrees[0];
        let linked = &project.worktrees[1];
        let change = project
            .change(&modified(first.root.join("src/main.rs")))
            .unwrap();
        assert_eq!(change.worktrees, BTreeSet::from([first.id]));
        assert!(!change.membership);
        assert!(!change.topology);
        assert_eq!(
            project
                .change(&modified(first.root.join("src/untracked.log")))
                .unwrap()
                .worktrees,
            BTreeSet::from([first.id])
        );
        let change = project
            .change(&modified(linked.git_dir.join("index")))
            .unwrap();
        assert_eq!(change.worktrees, BTreeSet::from([linked.id]));
        assert!(!change.membership);
        assert!(change.metadata);
        assert!(change.untracked);
        assert!(!change.topology);
        let config = project
            .change(&modified(linked.git_dir.join("config.worktree")))
            .unwrap();
        assert!(config.membership);
        assert_eq!(config.worktrees, BTreeSet::from([linked.id]));
        let change = project
            .change(&modified(project.common_dir.join("refs/heads/main")))
            .unwrap();
        assert_eq!(change.worktrees, BTreeSet::from([first.id]));
        assert!(change.metadata);
        assert!(!change.untracked);
        assert!(!change.membership);
        assert!(!change.topology);
        assert_eq!(
            project
                .change(&modified(
                    project.common_dir.join("refs/remotes/origin/linked")
                ))
                .unwrap()
                .worktrees,
            BTreeSet::from([linked.id])
        );
        for stash in ["refs/stash", "logs/refs/stash"] {
            let change = project
                .change(&modified(project.common_dir.join(stash)))
                .unwrap();
            assert!(change.shared_snapshot);
            assert!(change.worktrees.is_empty());
        }
        let ignored = project
            .change(&modified(first.root.join(".gitignore")))
            .unwrap();
        assert!(ignored.membership);
        assert_eq!(ignored.worktrees, BTreeSet::from([first.id]));
        for shared in ["packed-refs", "config", "info/exclude"] {
            assert_eq!(
                project
                    .change(&modified(project.common_dir.join(shared)))
                    .unwrap()
                    .worktrees,
                BTreeSet::from([first.id, linked.id])
            );
        }
        assert!(
            project
                .change(&modified(project.common_dir.join("worktrees/new/gitdir")))
                .unwrap()
                .topology
        );
        assert!(
            project
                .change(&modified(linked.git_dir.clone()))
                .unwrap()
                .topology
        );
    }

    #[test]
    fn bursts_coalesce_and_closing_a_project_removes_its_routing() {
        let project = project(Path::new("/repo"));
        let mut pending = Pending::default();
        pending
            .projects
            .insert(project.id, Arc::new(project.clone()));
        pending.roots.insert(
            project.worktrees[0].root.clone(),
            BTreeSet::from([project.id]),
        );
        for _ in 0..10_000 {
            pending.event(Ok(modified(project.worktrees[0].root.join("src/main.rs"))));
        }
        assert_eq!(pending.changes.len(), 1);
        assert_eq!(pending.changes[&project.id].worktrees.len(), 1);
        pending.projects.clear();
        pending.changes.clear();
        pending.event(Ok(modified(project.worktrees[0].root.join("src/main.rs"))));
        assert!(pending.changes.is_empty());
    }

    #[test]
    fn a_spent_budget_still_watches_every_projects_worktree_list() {
        let directory = tempfile::tempdir().unwrap();
        let projects = [("large", 64), ("small", 0)]
            .map(|(name, modules)| {
                let mut project = project(&directory.path().join(name));
                for worktree in &mut project.worktrees {
                    worktree.directories.extend(
                        (0..modules).map(|module| worktree.root.join(format!("src/{module}"))),
                    );
                    for directory in &worktree.directories {
                        std::fs::create_dir_all(directory).unwrap();
                    }
                    std::fs::create_dir_all(worktree.git_dir.join("refs")).unwrap();
                }
                std::fs::create_dir_all(project.common_dir.join("worktrees")).unwrap();
                (project.id, Arc::new(project))
            })
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let metadata = projects
            .values()
            .flat_map(|project| project.metadata_paths().into_keys())
            .filter(|path| path.is_dir())
            .collect::<BTreeSet<_>>();
        // Room for all metadata and a single content directory.
        let registrations = Registrations::within(&projects, metadata.len() + 1);
        for project in projects.values() {
            assert_eq!(
                registrations
                    .desired
                    .get(&project.common_dir.join("worktrees")),
                Some(&RecursiveMode::Recursive)
            );
            for worktree in &project.worktrees {
                assert!(
                    registrations
                        .desired
                        .contains_key(worktree.root.parent().unwrap())
                );
                assert!(registrations.desired.contains_key(&worktree.git_dir));
            }
            assert_eq!(
                registrations.failures.get(&project.id).map(String::as_str),
                Some("directory watch limit reached"),
                "content left unwatched is still reported, so it is polled"
            );
        }
        assert_eq!(registrations.desired.len(), metadata.len() + 1);
        assert!(
            Registrations::within(&projects, MAX_DIRECTORIES)
                .failures
                .is_empty()
        );
    }

    #[test]
    fn watch_budget_prioritizes_active_project_and_selected_worktree() {
        let directory = tempfile::tempdir().unwrap();
        let active = Arc::new(project(&directory.path().join("active")));
        let other = Arc::new(project(&directory.path().join("other")));
        for project in [&active, &other] {
            for worktree in &project.worktrees {
                for path in &worktree.directories {
                    std::fs::create_dir_all(path).unwrap();
                }
                std::fs::create_dir_all(worktree.git_dir.join("refs")).unwrap();
            }
            std::fs::create_dir_all(project.common_dir.join("worktrees")).unwrap();
        }
        let metadata = [&active, &other]
            .into_iter()
            .flat_map(|project| project.metadata_paths().into_keys())
            .filter(|path| path.is_dir())
            .collect::<BTreeSet<_>>()
            .len();
        let selected = &active.worktrees[1];
        let projects = BTreeMap::from([(active.id, active.clone()), (other.id, other.clone())]);
        let registrations =
            Registrations::prioritized(&projects, &[active.id], &[selected.id], metadata + 2);
        assert!(
            registrations
                .desired
                .contains_key(&selected.root.join("src"))
        );
        assert!(
            !registrations
                .desired
                .contains_key(&other.worktrees[0].root.join("src"))
        );
    }

    #[test]
    fn independently_selected_projects_share_content_budget() {
        let directory = tempfile::tempdir().unwrap();
        let active = Arc::new(project(&directory.path().join("active")));
        let selected = Arc::new(project(&directory.path().join("selected")));
        for project in [&active, &selected] {
            for worktree in &project.worktrees {
                for path in &worktree.directories {
                    std::fs::create_dir_all(path).unwrap();
                }
                std::fs::create_dir_all(worktree.git_dir.join("refs")).unwrap();
            }
            std::fs::create_dir_all(project.common_dir.join("worktrees")).unwrap();
        }
        let metadata = [&active, &selected]
            .into_iter()
            .flat_map(|project| project.metadata_paths().into_keys())
            .filter(|path| path.is_dir())
            .collect::<BTreeSet<_>>()
            .len();
        let projects =
            BTreeMap::from([(active.id, active.clone()), (selected.id, selected.clone())]);
        let registrations = Registrations::prioritized(
            &projects,
            &[active.id, selected.id],
            &[active.worktrees[0].id, selected.worktrees[1].id],
            metadata + 4,
        );
        for worktree in [&active.worktrees[0], &selected.worktrees[1]] {
            assert!(registrations.desired.contains_key(&worktree.root));
            assert!(
                registrations
                    .desired
                    .contains_key(&worktree.root.join("src"))
            );
        }
        assert!(
            !registrations
                .desired
                .contains_key(&active.worktrees[1].root.join("src"))
        );
    }

    #[test]
    fn active_content_survives_oversubscribed_background_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let active = Arc::new(project(&directory.path().join("active")));
        let mut projects = BTreeMap::from([(active.id, active.clone())]);
        for index in 0..12 {
            let background = Arc::new(project(
                &directory.path().join(format!("background{index}")),
            ));
            projects.insert(background.id, background);
        }
        for project in projects.values() {
            for worktree in &project.worktrees {
                for path in &worktree.directories {
                    std::fs::create_dir_all(path).unwrap();
                }
                std::fs::create_dir_all(worktree.git_dir.join("refs")).unwrap();
            }
            std::fs::create_dir_all(project.common_dir.join("worktrees")).unwrap();
        }
        let hot_metadata = active
            .metadata_paths()
            .into_keys()
            .filter(|path| path.is_dir())
            .count();
        let limit = hot_metadata + 2;
        let registrations =
            Registrations::prioritized(&projects, &[active.id], &[active.worktrees[1].id], limit);
        assert!(
            registrations
                .desired
                .contains_key(&active.worktrees[1].root.join("src"))
        );
        assert!(registrations.failures.keys().any(|id| *id != active.id));
        assert!(registrations.desired.len() <= limit);
    }

    #[test]
    fn hot_content_is_reserved_when_background_admin_leaves_zero_or_one_slot() {
        let directory = tempfile::tempdir().unwrap();
        let active = Arc::new(project(&directory.path().join("active")));
        let other = Arc::new(project(&directory.path().join("other")));
        for project in [&active, &other] {
            for worktree in &project.worktrees {
                for path in &worktree.directories {
                    std::fs::create_dir_all(path).unwrap();
                }
                std::fs::create_dir_all(worktree.git_dir.join("refs")).unwrap();
            }
            std::fs::create_dir_all(project.common_dir.join("worktrees")).unwrap();
        }
        let projects = BTreeMap::from([(active.id, active.clone()), (other.id, other.clone())]);
        let all_metadata = projects
            .values()
            .flat_map(|project| project.metadata_paths().into_keys())
            .filter(|path| path.is_dir())
            .collect::<BTreeSet<_>>()
            .len();
        for spare in [0, 1] {
            let registrations = Registrations::prioritized(
                &projects,
                &[active.id],
                &[active.worktrees[1].id],
                all_metadata + spare,
            );
            assert!(
                registrations
                    .desired
                    .contains_key(&active.worktrees[1].root.join("src")),
                "spare={spare}"
            );
            assert!(registrations.desired.len() <= all_metadata + spare);
        }
    }

    #[test]
    fn capped_inventory_keeps_native_directory_and_admin_coverage() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        assert!(std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .arg(root)
            .output()
            .unwrap()
            .status
            .success());
        std::fs::create_dir_all(root.join("z/nested")).unwrap();
        std::fs::create_dir_all(root.join("target/deep/ignored")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("a.txt"), "prefix\n").unwrap();
        let outside = root.join("z/nested/outside.txt");
        std::fs::write(&outside, "outside prefix\n").unwrap();
        let git = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        let inventory = git.watch_files_projection(root, 1, &cancellation).unwrap();
        assert!(inventory.listing_truncated);
        assert!(!inventory.paths.iter().any(|path| root.join(path.as_path()) == outside));
        let records = git.list_worktrees(root, &cancellation).unwrap();
        let common = git.common_dir(root, &cancellation).unwrap();
        let id = ProjectId::new();
        let plan = Project::read_changed_with_inventory_limit(
            id,
            common.clone(),
            &records,
            &cancellation,
            None,
            &records.iter().map(|record| record.id).collect(),
            &BTreeMap::new(),
            1,
        ).unwrap();
        assert_eq!(plan.worktrees.len(), 1);
        assert!(plan.worktrees[0].directories.contains(&root.join("z/nested")));
        assert!(!plan.worktrees[0].directories.contains(&root.join("target")));
        assert!(plan.metadata_paths().contains_key(&common));
        assert_eq!(plan.polling.as_deref(), Some("directory watch limit reached"));
        let registrations = Registrations::within(
            &BTreeMap::from([(id, Arc::new(plan.clone()))]),
            watch_budget(),
        );
        assert!(registrations.desired.contains_key(&root.join("z/nested")));
        assert!(registrations.desired.contains_key(&common));
        let monitor = Monitor::start().unwrap();
        monitor.configure(vec![Arc::new(plan)]);
        wait_for(&monitor, id, |change| change.coverage.is_some());
        std::fs::write(&outside, "changed outside prefix\n").unwrap();
        wait_for(&monitor, id, |change| change.worktrees.contains(&records[0].id));
        std::fs::write(common.join("index"), "metadata event only\n").unwrap();
        wait_for(&monitor, id, |change| change.metadata && change.worktrees.contains(&records[0].id));
    }

    #[test]
    fn incomplete_ignored_inventory_does_not_descend_build_trees() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        assert!(std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .arg(root)
            .output()
            .unwrap()
            .status
            .success());
        for name in ["ignored-a", "ignored-b"] {
            std::fs::create_dir_all(root.join(name).join("deep/ignored")).unwrap();
            std::fs::write(root.join(name).join("deep/ignored/output"), "build\n").unwrap();
        }
        std::fs::write(root.join(".gitignore"), "ignored-a/\nignored-b/\n").unwrap();
        let git = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        let records = git.list_worktrees(root, &cancellation).unwrap();
        let plan = Project::read_changed_with_inventory_limit(
            ProjectId::new(),
            git.common_dir(root, &cancellation).unwrap(),
            &records,
            &cancellation,
            None,
            &records.iter().map(|record| record.id).collect(),
            &BTreeMap::new(),
            1,
        ).unwrap();
        assert_eq!(plan.worktrees.len(), 1);
        assert_eq!(plan.worktrees[0].directories, BTreeSet::from([root.to_owned()]));
        assert_eq!(plan.polling.as_deref(), Some("directory watch limit reached"));
    }

    #[test]
    fn unreadable_index_keeps_initial_root_and_admin_watches() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        assert!(std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .arg(root)
            .output()
            .unwrap()
            .status
            .success());
        std::fs::write(root.join(".git/index"), "not an index\n").unwrap();
        let git = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        let records = git.list_worktrees(root, &cancellation).unwrap();
        let common = git.common_dir(root, &cancellation).unwrap();
        let plan = Project::read_changed(
            ProjectId::new(),
            common.clone(),
            &records,
            &cancellation,
            None,
            &records.iter().map(|record| record.id).collect(),
            &BTreeMap::new(),
        ).unwrap();
        assert_eq!(plan.worktrees.len(), 1);
        assert!(plan.worktrees[0].directories.contains(root));
        assert!(plan.metadata_paths().contains_key(&common));
        assert!(plan.polling.is_some());
    }

    #[test]
    fn native_watcher_observes_first_file_in_initial_and_later_empty_directories() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let git = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        let output = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .arg(root)
            .output()
            .unwrap();
        assert!(output.status.success());
        std::fs::create_dir_all(root.join("empty/nested")).unwrap();
        std::fs::create_dir_all(root.join("target/deep/ignored")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        let records = git.list_worktrees(root, &cancellation).unwrap();
        let id = ProjectId::new();
        let common = git.common_dir(root, &cancellation).unwrap();
        let plan = Project::read_changed(
            id,
            common.clone(),
            &records,
            &cancellation,
            None,
            &records.iter().map(|record| record.id).collect(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(
            plan.worktrees[0]
                .directories
                .contains(&root.join("empty/nested"))
        );
        assert!(!plan.worktrees[0].directories.contains(&root.join("target")));
        let monitor = Monitor::start().unwrap();
        monitor.configure(vec![Arc::new(plan.clone())]);
        wait_for(&monitor, id, |change| change.error.is_none());
        std::fs::write(root.join("empty/nested/first.txt"), "first\n").unwrap();
        wait_for(&monitor, id, |change| {
            change.worktrees.contains(&records[0].id)
        });
        std::fs::create_dir(root.join("later")).unwrap();
        wait_for(&monitor, id, |change| change.membership);
        let next = Project::read_changed(
            id,
            common,
            &records,
            &cancellation,
            Some(&plan),
            &records.iter().map(|record| record.id).collect(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(next.worktrees[0].directories.contains(&root.join("later")));
        monitor.configure(vec![Arc::new(next)]);
        wait_for(&monitor, id, |change| {
            change.worktrees.contains(&records[0].id)
        });
        std::fs::write(root.join("later/first.txt"), "second\n").unwrap();
        wait_for(&monitor, id, |change| {
            change.worktrees.contains(&records[0].id)
        });
    }

    fn wait_for(monitor: &Monitor, project: ProjectId, predicate: impl Fn(&Change) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if monitor.receiver.try_recv().is_ok()
                && monitor.drain().get(&project).is_some_and(&predicate)
            {
                return;
            }
            assert!(Instant::now() < deadline, "watch event did not arrive");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn native_watcher_observes_atomic_replacement_and_recreated_directory() {
        let directory = tempfile::tempdir().unwrap();
        let mut project = project(directory.path());
        project.worktrees.truncate(1);
        let worktree = &project.worktrees[0];
        let source = worktree.root.join("src");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&worktree.git_dir).unwrap();
        let file = source.join("main.rs");
        std::fs::write(&file, "first").unwrap();
        let monitor = Monitor::start().unwrap();
        monitor.configure(vec![Arc::new(project.clone())]);
        wait_for(&monitor, project.id, |change| change.error.is_none());
        let replacement = source.join(".save");
        std::fs::write(&replacement, "second").unwrap();
        std::fs::rename(&replacement, &file).unwrap();
        wait_for(&monitor, project.id, |change| {
            change.worktrees.contains(&worktree.id)
        });
        std::fs::remove_dir_all(&source).unwrap();
        std::fs::create_dir(&source).unwrap();
        std::fs::write(&file, "third").unwrap();
        wait_for(&monitor, project.id, |change| change.membership);
        // The path set is unchanged, but the directory inode changed. Rearm
        // it instead of trusting a stale native registration for that path.
        monitor.configure(vec![Arc::new(project.clone())]);
        // Configure completion must be acknowledged even when paths compare
        // equal, so edits cannot race the re-registration barrier.
        wait_for(&monitor, project.id, |change| {
            change.worktrees.contains(&worktree.id)
        });
        std::fs::write(&file, "fourth").unwrap();
        wait_for(&monitor, project.id, |change| {
            change.worktrees.contains(&worktree.id)
        });
    }
}
