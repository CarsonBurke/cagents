//! Bounded filesystem invalidation for Git state. The watcher never runs Git or
//! GitHub; its coalesced edges ask the owner's cancellable reader to do so. The
//! window's sidebar and the daemon's Work board each own one.
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
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

/// Paths relative to a worktree root, sorted and kept in one allocation. A
/// large checkout tracks tens of thousands of paths, and each watcher owner
/// holds them for every worktree.
#[derive(Clone, Default, PartialEq, Eq)]
struct PathSet {
    text: Box<[u8]>,
    /// Where each path ends in `text`.
    ends: Box<[u32]>,
}

impl PathSet {
    fn new<'a>(paths: impl IntoIterator<Item = &'a [u8]>) -> Result<Self, String> {
        let mut paths = paths.into_iter().collect::<Vec<_>>();
        paths.sort_unstable();
        paths.dedup();
        let mut text = Vec::with_capacity(paths.iter().map(|path| path.len()).sum());
        let mut ends = Vec::with_capacity(paths.len());
        for path in paths {
            text.extend_from_slice(path);
            ends.push(
                u32::try_from(text.len())
                    .map_err(|_| "the worktree tracks too many paths to watch".to_owned())?,
            );
        }
        Ok(Self {
            text: text.into(),
            ends: ends.into(),
        })
    }

    fn get(&self, index: usize) -> &[u8] {
        let start = index
            .checked_sub(1)
            .map_or(0, |previous| self.ends[previous] as usize);
        &self.text[start..self.ends[index] as usize]
    }

    fn contains(&self, path: &Path) -> bool {
        let path = path.as_os_str().as_bytes();
        let (mut low, mut high) = (0, self.ends.len());
        while low < high {
            let middle = low + (high - low) / 2;
            match self.get(middle).cmp(path) {
                Ordering::Less => low = middle + 1,
                Ordering::Equal => return true,
                Ordering::Greater => high = middle,
            }
        }
        false
    }

    fn iter(&self) -> impl Iterator<Item = &Path> {
        (0..self.ends.len()).map(|index| Path::new(OsStr::from_bytes(self.get(index))))
    }
}

impl std::fmt::Debug for PathSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "PathSet({} paths)", self.ends.len())
    }
}

/// What a worktree's index tracks. Branches rarely add or remove files, so
/// most worktrees of a repository track the same paths and share one census,
/// which a re-plan that finds them unchanged keeps as well.
#[derive(Debug, PartialEq, Eq)]
struct Census {
    files: PathSet,
    /// Every directory on the way to a file; the root is the empty path.
    directories: PathSet,
    submodules: bool,
}

impl Census {
    fn new<'a>(
        files: impl IntoIterator<Item = &'a [u8]>,
        submodules: bool,
    ) -> Result<Self, String> {
        let files = PathSet::new(files)?;
        let directories = PathSet::new(
            files
                .iter()
                .flat_map(|file| file.ancestors().skip(1))
                .chain([Path::new("")])
                .map(|directory| directory.as_os_str().as_bytes()),
        )?;
        Ok(Self {
            files,
            directories,
            submodules,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tracked {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub id: WorktreeId,
    pub root: PathBuf,
    pub git_dir: PathBuf,
    census: Arc<Census>,
    directories: PathSet,
    polling: Option<String>,
}

impl Worktree {
    /// Whether `path` is a tracked file or a directory on the way to one.
    fn tracks(&self, path: &Path) -> Option<Tracked> {
        let relative = path.strip_prefix(&self.root).ok()?;
        if self.census.directories.contains(relative) || self.directories.contains(relative) {
            Some(Tracked::Directory)
        } else {
            self.census
                .files
                .contains(relative)
                .then_some(Tracked::File)
        }
    }

    fn content_directories(&self) -> impl Iterator<Item = &Path> {
        self.census.directories.iter().chain(
            self.directories
                .iter()
                .filter(|path| !self.census.directories.contains(path)),
        )
    }

    /// `relative` beneath the root; `join("")` would append a separator.
    fn absolute(&self, relative: &Path) -> PathBuf {
        if relative.as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.root.join(relative)
        }
    }
}

/// Discover empty and untracked directories without following links or
/// descending into ignored trees. The shared entry budget also bounds a
/// directory containing many plain files, not just the directory count.
fn content_directories(
    root: &Path,
    ignored: &[crate::repository::RepoPath],
    cancellation: &CancellationToken,
    remaining: &mut usize,
) -> Result<(PathSet, bool), String> {
    use std::os::fd::AsRawFd;
    let handle = crate::fs_safety::Root::open(root).map_err(|error| error.to_string())?;
    let ignored = ignored
        .iter()
        .map(|path| path.as_path())
        .collect::<BTreeSet<_>>();
    let mut pending = vec![PathBuf::new()];
    let mut directories = Vec::new();
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
        directories.push(relative.clone());
        for entry in entries {
            if cancellation.is_cancelled() {
                return Err("watch planning cancelled".into());
            }
            if *remaining == 0 {
                return Ok((
                    PathSet::new(directories.iter().map(|path| path.as_os_str().as_bytes()))?,
                    true,
                ));
            }
            *remaining -= 1;
            let entry = entry.map_err(|error| error.to_string())?;
            if entry.file_name() == ".git" || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let child = relative.join(entry.file_name());
            if child.ancestors().any(|ancestor| ignored.contains(ancestor)) {
                continue;
            }
            if crate::fs_safety::metadata_beneath(&handle, &child.join(".git")).is_ok() {
                continue;
            }
            pending.push(child);
        }
    }
    Ok((
        PathSet::new(directories.iter().map(|path| path.as_os_str().as_bytes()))?,
        false,
    ))
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
    ) -> Result<Self, String> {
        let git = RepositoryService::system("git");
        let mut worktrees = Vec::new();
        let mut remaining = MAX_DIRECTORIES;
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
            let mut identity_mismatch = false;
            let planned = (|| -> Result<Worktree, String> {
                let actual_common = git
                    .common_dir(&record.path, cancellation)
                    .map_err(|error| error.to_string())?;
                if actual_common != common_dir {
                    identity_mismatch = true;
                    return Err(format!(
                        "{} now belongs to another repository",
                        record.path.display()
                    ));
                }
                let git_dir = git
                    .git_dir(&record.path, cancellation)
                    .map_err(|error| error.to_string())?;
                let index = git
                    .index_paths(&record.path, cancellation)
                    .map_err(|error| error.to_string())?;
                let census = {
                    Census::new(
                        index.paths.iter().map(|path| path.as_os_str().as_bytes()),
                        index.gitlinks,
                    )?
                };
                let census = worktrees
                    .iter()
                    .chain(previous.iter().flat_map(|previous| &previous.worktrees))
                    .map(|worktree| &worktree.census)
                    .find(|known| ***known == census)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(census));
                let ignored = git
                    .ignored_files(&record.path, &index.paths, cancellation)
                    .map_err(|error| error.to_string())?;
                let (directories, incomplete) = content_directories(
                    &record.path,
                    &ignored.ignored_directories,
                    cancellation,
                    &mut remaining,
                )?;
                let worktree_polling = if incomplete {
                    Some("directory watch limit reached".into())
                } else if census.submodules {
                    Some("submodule contents require a periodic local Git check".into())
                } else {
                    network_filesystem(&record.path)
                        .map_err(|error| error.to_string())?
                        .then(|| {
                            "network or FUSE filesystem may omit native change events".to_owned()
                        })
                };
                Ok(Worktree {
                    id: record.id,
                    root: record.path.clone(),
                    git_dir,
                    census,
                    directories,
                    polling: worktree_polling,
                })
            })();
            match planned {
                Ok(worktree) => worktrees.push(worktree),
                Err(error) => {
                    if identity_mismatch || cancellation.is_cancelled() {
                        return Err(error);
                    }
                    planning_failure = Some(error.clone());
                    tracing::warn!(worktree = %record.path.display(), %error, "cannot update worktree watches; retaining sibling coverage");
                    if let Some(old) = previous.and_then(|project| {
                        project
                            .worktrees
                            .iter()
                            .find(|old| old.id == record.id && old.root == record.path)
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

    pub fn matches(&self, common: &Path, records: &[WorktreeRecord]) -> bool {
        self.common_dir == common
            && self.worktrees.len() == records.iter().filter(|record| !record.bare).count()
            && records.iter().filter(|record| !record.bare).all(|record| {
                self.worktrees
                    .iter()
                    .any(|old| old.id == record.id && old.root == record.path)
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
        paths.insert(self.common_dir.join("logs"), RecursiveMode::NonRecursive);
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

    fn change(&self, event: &Event) -> Option<Change> {
        if matches!(event.kind, EventKind::Access(_)) {
            return None;
        }
        let mut change = Change::default();
        for path in &event.paths {
            if path == &self.common_dir {
                change.topology = true;
                change.membership = true;
            }
            let common = path.strip_prefix(&self.common_dir).ok();
            if common
                .is_some_and(|relative| matches!(relative.to_str(), Some("logs" | "logs/refs")))
            {
                change.metadata = true;
                change.shared_snapshot = true;
                change.membership = true;
                continue;
            }
            if common.is_some_and(|relative| {
                relative == Path::new("refs/stash") || relative == Path::new("logs/refs/stash")
            }) {
                change.metadata = true;
                change.shared_snapshot = true;
                continue;
            }
            if common.is_some_and(common_metadata_noise) {
                continue;
            }
            if common.is_some_and(|relative| {
                relative == Path::new("config")
                    || relative == Path::new("info/exclude")
                    || relative == Path::new("packed-refs")
                    || relative.starts_with("refs")
            }) {
                change.references = true;
                change.metadata = true;
                change.membership |= common.is_some_and(|relative| {
                    matches!(relative.to_str(), Some("refs" | "config" | "info/exclude"))
                });
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
                    let relative = path.strip_prefix(&worktree.git_dir).unwrap();
                    if metadata_noise(relative) {
                        continue;
                    }
                    change.worktrees.insert(worktree.id);
                    change.metadata = true;
                    let topology = relative.as_os_str().is_empty()
                        || matches!(relative.to_str(), Some("locked" | "gitdir" | "commondir"));
                    change.topology |= topology;
                    // HEAD and operation-state updates invalidate status, but
                    // only index changes can change the tracked-path census.
                    change.membership |= topology
                        || relative == Path::new("index")
                        || relative == Path::new("refs")
                        || relative.as_os_str().as_bytes().starts_with(b"sharedindex.");
                } else if let Some(tracked) = worktree.tracks(path) {
                    change.worktrees.insert(worktree.id);
                    // A removed/recreated ancestor needs new directory watches;
                    // simple file edits keep the existing tracked-path census.
                    change.membership |= tracked == Tracked::Directory
                        && matches!(
                            event.kind,
                            EventKind::Any
                                | EventKind::Other
                                | EventKind::Create(_)
                                | EventKind::Remove(_)
                                | EventKind::Modify(notify::event::ModifyKind::Any)
                                | EventKind::Modify(notify::event::ModifyKind::Name(_))
                        )
                        || path.file_name().is_some_and(|name| name == ".gitignore");
                    change.topology |= path == &worktree.root;
                } else if path
                    .strip_prefix(&worktree.root)
                    .ok()
                    .is_some_and(|relative| {
                        relative.parent().is_some_and(|parent| {
                            worktree.census.directories.contains(parent)
                                || worktree.directories.contains(parent)
                        })
                    })
                {
                    change.worktrees.insert(worktree.id);
                    change.membership |=
                        matches!(
                            event.kind,
                            EventKind::Any
                                | EventKind::Other
                                | EventKind::Create(_)
                                | EventKind::Remove(_)
                                | EventKind::Modify(notify::event::ModifyKind::Any)
                                | EventKind::Modify(notify::event::ModifyKind::Name(_))
                        ) || path.file_name().is_some_and(|name| name == ".gitignore");
                }
            }
        }
        (change.topology || change.references || change.metadata || !change.worktrees.is_empty())
            .then_some(change)
    }
}

/// Git writes temporary locks and bookkeeping during status/refresh too.
/// Classify paths individually: a rename may contain both index.lock and index.
fn metadata_noise(relative: &Path) -> bool {
    relative.starts_with("objects")
        || relative.starts_with("logs")
        || relative
            .file_name()
            .is_some_and(|name| name.as_bytes().ends_with(b".lock"))
}

fn common_metadata_noise(relative: &Path) -> bool {
    // Linked worktree administrative directories share this shape even before
    // the next topology read discovers their owner. Strip the directory name,
    // not a suffix: a legitimate worktree can itself be named `checkout.lock`.
    if let Ok(relative) = relative.strip_prefix("worktrees") {
        let mut components = relative.components();
        components.next();
        metadata_noise(components.as_path())
    } else {
        metadata_noise(relative)
    }
}

fn access_only(event: &Event) -> bool {
    matches!(event.kind, EventKind::Access(_)) && !event.need_rescan()
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Change {
    /// Worktrees whose own files, index or `HEAD` changed.
    pub worktrees: BTreeSet<WorktreeId>,
    /// Shared refs or configuration changed: a commit, fetch or branch
    /// update. Any worktree's comparison may have moved, but its status only
    /// where its own `HEAD` did, which a consumer can tell from the refs.
    pub references: bool,
    pub topology: bool,
    pub membership: bool,
    pub metadata: bool,
    pub shared_snapshot: bool,
    pub error: Option<String>,
    pub coverage: Option<bool>,
}

impl Change {
    pub fn merge(&mut self, other: Self) {
        self.worktrees.extend(other.worktrees);
        self.references |= other.references;
        self.topology |= other.topology;
        self.membership |= other.membership;
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
    fn event(&mut self, event: notify::Result<Event>) -> bool {
        if event.as_ref().is_ok_and(access_only) {
            return false;
        }
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
        let mut changed = false;
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
                changed = true;
            }
        }
        changed
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
    /// Metadata gets first claim on the watch and filesystem-work budgets.
    /// Content traversal also has a candidate budget: missing or symlinked
    /// directories cannot make a large census an unbounded planning job.
    /// Projects left short of coverage retain the existing polling fallback.
    fn within(projects: &BTreeMap<ProjectId, Arc<Project>>, limit: usize) -> Self {
        Self::within_with(projects, limit, |path| std::fs::symlink_metadata(path))
    }

    fn within_with(
        projects: &BTreeMap<ProjectId, Arc<Project>>,
        limit: usize,
        inspect: impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
    ) -> Self {
        Self::prioritized_with(projects, &[], &[], limit, inspect)
    }

    fn prioritized(
        projects: &BTreeMap<ProjectId, Arc<Project>>,
        priority: &[ProjectId],
        selected: &[WorktreeId],
        limit: usize,
    ) -> Self {
        Self::prioritized_with(projects, priority, selected, limit, |path| {
            std::fs::symlink_metadata(path)
        })
    }

    fn prioritized_with(
        projects: &BTreeMap<ProjectId, Arc<Project>>,
        priority: &[ProjectId],
        selected: &[WorktreeId],
        limit: usize,
        mut inspect: impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
    ) -> Self {
        let mut registrations = Self::default();
        let mut checks = limit;
        for (&id, project) in projects {
            if let Some(reason) = &project.polling {
                registrations.failures.insert(id, reason.clone());
            }
            for (path, mode) in project.metadata_paths() {
                let _ = registrations.claim(id, path, mode, limit, &mut checks, &mut inspect);
            }
        }
        let mut candidates = limit;
        // Selected worktrees share the remaining content budget one directory
        // at a time. Neither their trees nor filesystem facts are precollected.
        let mut preferred = selected
            .iter()
            .filter_map(|selected| {
                projects.iter().find_map(|(&id, project)| {
                    let worktree = project
                        .worktrees
                        .iter()
                        .find(|worktree| worktree.id == *selected)?;
                    Some((
                        id,
                        worktree,
                        worktree.content_directories(),
                        BTreeSet::new(),
                    ))
                })
            })
            .collect::<Vec<_>>();
        loop {
            let mut progressed = false;
            for (id, worktree, paths, blocked) in &mut preferred {
                if let Some(relative) = paths.next() {
                    progressed = true;
                    registrations.content_candidate(
                        *id,
                        worktree,
                        relative,
                        blocked,
                        limit,
                        &mut candidates,
                        &mut checks,
                        &mut inspect,
                    );
                }
            }
            if !progressed || candidates == 0 || checks == 0 {
                if progressed {
                    for (id, _, paths, _) in &mut preferred {
                        if paths.next().is_some() {
                            registrations.exhausted(*id);
                        }
                    }
                }
                break;
            }
        }
        let mut ordered = projects.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|(id, _)| {
            priority
                .iter()
                .position(|candidate| candidate == *id)
                .unwrap_or(usize::MAX)
        });
        let mut roots: BTreeMap<&Path, Vec<(ProjectId, &Worktree)>> = BTreeMap::new();
        for (&id, project) in ordered {
            for worktree in &project.worktrees {
                roots
                    .entry(&worktree.root)
                    .or_default()
                    .push((id, worktree));
                if !selected.contains(&worktree.id) {
                    registrations.content_worktree(
                        id,
                        worktree,
                        limit,
                        &mut candidates,
                        &mut checks,
                        &mut inspect,
                    );
                }
            }
        }
        // A project may share already-registered content after the traversal
        // budget is spent. Route the bounded desired paths through their root
        // ancestors, without scanning every project's entire census or every
        // unrelated worktree for each watch.
        for path in registrations.desired.keys() {
            for &(id, worktree) in path
                .ancestors()
                .filter_map(|ancestor| roots.get(ancestor))
                .flatten()
            {
                if worktree.tracks(path) == Some(Tracked::Directory) {
                    registrations
                        .owners
                        .entry(path.clone())
                        .or_default()
                        .insert(id);
                }
            }
        }
        registrations
    }

    fn exhausted(&mut self, id: ProjectId) {
        self.failures
            .insert(id, "directory watch limit reached".to_owned());
    }

    fn content_worktree(
        &mut self,
        id: ProjectId,
        worktree: &Worktree,
        limit: usize,
        candidates: &mut usize,
        checks: &mut usize,
        inspect: &mut impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
    ) {
        let mut blocked = BTreeSet::new();
        for relative in worktree.content_directories() {
            if !self.content_candidate(
                id,
                worktree,
                relative,
                &mut blocked,
                limit,
                candidates,
                checks,
                inspect,
            ) {
                return;
            }
        }
    }

    fn content_candidate(
        &mut self,
        id: ProjectId,
        worktree: &Worktree,
        relative: &Path,
        blocked: &mut BTreeSet<PathBuf>,
        limit: usize,
        candidates: &mut usize,
        checks: &mut usize,
        inspect: &mut impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
    ) -> bool {
        if *candidates == 0 {
            self.exhausted(id);
            return false;
        }
        *candidates -= 1;
        if relative
            .ancestors()
            .any(|ancestor| blocked.contains(ancestor))
        {
            return true;
        }
        match self.claim(
            id,
            worktree.absolute(relative),
            RecursiveMode::NonRecursive,
            limit,
            checks,
            inspect,
        ) {
            Some(true) => true,
            Some(false) => {
                blocked.insert(relative.to_owned());
                true
            }
            None => false,
        }
    }

    /// None means exhausted; false means absent, non-directory, or unreadable.
    fn claim(
        &mut self,
        id: ProjectId,
        path: PathBuf,
        mode: RecursiveMode,
        limit: usize,
        checks: &mut usize,
        inspect: &mut impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
    ) -> Option<bool> {
        if let Some(registered) = self.desired.get_mut(&path) {
            if mode == RecursiveMode::Recursive {
                *registered = mode;
            }
            self.owners.entry(path).or_default().insert(id);
            return Some(true);
        }
        if self.desired.len() >= limit || *checks == 0 {
            self.exhausted(id);
            return None;
        }
        *checks -= 1;
        match inspect(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Some(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(false),
            Err(error) => {
                self.failures.insert(id, error.to_string());
                return Some(false);
            }
        }
        self.owners.entry(path.clone()).or_default().insert(id);
        self.desired.insert(path, mode);
        Some(true)
    }
}

/// One native watcher for its owner, with at most one pending change set
/// regardless of event rate. Registration happens on its coordinator, never on
/// the owner's thread.
pub(crate) struct Monitor {
    commands: mpsc::SyncSender<()>,
    configuration: Arc<Mutex<Configuration>>,
    pending: Arc<Mutex<Pending>>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    /// `wake` runs on the watcher's threads whenever changes are pending to
    /// [`drain`](Self::drain). It may run for every event, so it must only
    /// signal (a bounded `try_send`, a condvar notify) and never block.
    pub fn start(wake: impl Fn() + Send + Sync + 'static) -> std::io::Result<Self> {
        let (commands, requests) = mpsc::sync_channel(1);
        let configuration = Arc::new(Mutex::new(Configuration::default()));
        let desired_configuration = configuration.clone();
        let wake = Arc::new(wake);
        let pending = Arc::new(Mutex::new(Pending::default()));
        let state = pending.clone();
        let thread = thread::Builder::new()
            .name("git-watch".into())
            .spawn(move || {
                let create_watcher = || {
                    let events = state.clone();
                    let signal = wake.clone();
                    notify::recommended_watcher(move |event: notify::Result<Event>| {
                        // Git status itself produces Access traffic. A requested
                        // rescan still reaches the normal recovery path.
                        if event.as_ref().is_ok_and(access_only) {
                            return;
                        }
                        let mut pending = events.lock().unwrap_or_else(|p| p.into_inner());
                        if pending.event(event) {
                            signal();
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
                    } = Registrations::prioritized(
                        &projects,
                        &priority,
                        &selected,
                        MAX_DIRECTORIES,
                    );
                    // A recursive metadata watch already owns its descendants.
                    // Avoid overlapping registrations, whose removal can also
                    // remove a descendant's native watch on some backends.
                    let recursive = desired
                        .iter()
                        .filter(|(_, mode)| **mode == RecursiveMode::Recursive)
                        .map(|(path, _)| path.clone())
                        .collect::<BTreeSet<_>>();
                    desired.retain(|path, _| {
                        !path
                            .ancestors()
                            .skip(1)
                            .any(|ancestor| recursive.contains(ancestor))
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
                        wake();
                    }
                }
            })?;
        Ok(Self {
            commands,
            configuration,
            pending,
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

    fn census(files: &[&str]) -> Arc<Census> {
        Arc::new(Census::new(files.iter().map(|file| file.as_bytes()), false).unwrap())
    }

    fn project(root: &Path) -> Project {
        let first = root.join("first");
        let linked = root.join("linked");
        let common = first.join(".git");
        let census = census(&["src/main.rs"]);
        Project {
            id: ProjectId::new(),
            common_dir: common.clone(),
            polling: None,
            worktrees: vec![
                Worktree {
                    id: WorktreeId::new(),
                    root: first,
                    git_dir: common.clone(),
                    census: census.clone(),
                    directories: PathSet::default(),
                    polling: None,
                },
                Worktree {
                    id: WorktreeId::new(),
                    root: linked,
                    git_dir: common.join("worktrees/linked"),
                    census,
                    directories: PathSet::default(),
                    polling: None,
                },
            ],
        }
    }

    #[test]
    fn path_sets_hold_sorted_unique_relative_paths() {
        let census = census(&["src/b.rs", "README.md", "src/a/mod.rs", "src/b.rs"]);
        assert_eq!(
            census.files.iter().collect::<Vec<_>>(),
            ["README.md", "src/a/mod.rs", "src/b.rs"].map(Path::new)
        );
        assert_eq!(
            census.directories.iter().collect::<Vec<_>>(),
            ["", "src", "src/a"].map(Path::new)
        );
        assert!(census.files.contains(Path::new("src/a/mod.rs")));
        assert!(!census.files.contains(Path::new("src/a")));
        assert!(!census.files.contains(Path::new("src/c.rs")));
        assert!(census.directories.contains(Path::new("")));
        let empty = Census::new([], false).unwrap();
        assert!(!empty.files.contains(Path::new("")));
        assert_eq!(
            empty.directories.iter().collect::<Vec<_>>(),
            [Path::new("")]
        );
    }

    fn git(root: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn plans_share_censuses_and_read_submodules_from_the_index() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let first = directory.join("first");
        std::fs::create_dir_all(first.join("src")).unwrap();
        std::fs::write(first.join("src/main.rs"), "").unwrap();
        git(&first, &["init", "-q"]);
        git(&first, &["add", "."]);
        git(&first, &["commit", "-qm", "init"]);
        git(&first, &["worktree", "add", "-q", "../linked"]);
        git(&first, &["worktree", "add", "-q", "../other"]);
        let (linked, other) = (directory.join("linked"), directory.join("other"));
        std::fs::write(other.join("README.md"), "").unwrap();
        git(&other, &["add", "README.md"]);

        let service = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        let common = service.common_dir(&first, &cancellation).unwrap();
        let records = service.list_worktrees(&first, &cancellation).unwrap();
        let read = |previous: Option<&Project>, changed: &[WorktreeId]| {
            Project::read_changed(
                ProjectId::new(),
                common.clone(),
                &records,
                &cancellation,
                previous,
                &changed.iter().copied().collect(),
            )
            .unwrap()
        };
        let worktree = |plan: &Project, root: &Path| {
            plan.worktrees
                .iter()
                .find(|worktree| worktree.root == root)
                .unwrap()
                .clone()
        };
        let plan = read(None, &[]);
        let main = worktree(&plan, &first);
        assert!(
            Arc::ptr_eq(&main.census, &worktree(&plan, &linked).census),
            "identical indexes share one census"
        );
        assert!(!Arc::ptr_eq(&main.census, &worktree(&plan, &other).census));
        assert_eq!(
            worktree(&plan, &other).tracks(&other.join("README.md")),
            Some(Tracked::File)
        );
        assert_eq!(main.tracks(&first.join("src/main.rs")), Some(Tracked::File));
        assert_eq!(main.tracks(&first.join("src")), Some(Tracked::Directory));
        assert_eq!(main.tracks(&first), Some(Tracked::Directory));
        assert_eq!(main.tracks(&first.join("target/debug")), None);
        // Roots match by component: a sibling sharing their prefix is not
        // beneath them, and a trailing separator names the same directory.
        assert_eq!(main.tracks(&directory.join("first2/src")), None);
        assert_eq!(
            main.tracks(Path::new(&format!("{}/src/", first.display()))),
            Some(Tracked::Directory)
        );
        assert_eq!(main.tracks(&linked.join("src/main.rs")), None);
        assert!(plan.polling.is_none());

        // Re-reading a worktree whose paths did not change keeps its census.
        let again = read(Some(&plan), &[main.id]);
        assert!(Arc::ptr_eq(&worktree(&again, &first).census, &main.census));

        // A gitlink is a submodule, whose contents only a periodic check sees.
        let head = git(&first, &["rev-parse", "HEAD"]);
        let gitlink = format!("160000,{},vendor/library", head.trim());
        git(&first, &["update-index", "--add", "--cacheinfo", &gitlink]);
        let submodule = read(Some(&again), &[main.id]);
        let main = worktree(&submodule, &first);
        assert_eq!(
            main.polling.as_deref(),
            Some("submodule contents require a periodic local Git check")
        );
        assert_eq!(main.tracks(&first.join("vendor")), Some(Tracked::Directory));
        assert!(submodule.polling.is_some());
        assert!(worktree(&submodule, &linked).polling.is_none());
    }

    fn modified(path: PathBuf) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any))).add_path(path)
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
        assert!(change.membership);
        assert!(!change.topology);
        let change = project
            .change(&modified(project.common_dir.join("refs/heads/main")))
            .unwrap();
        assert!(change.references);
        assert!(
            change.worktrees.is_empty(),
            "no worktree's own files changed"
        );
        assert!(!change.topology);
        assert!(
            project
                .change(&modified(project.common_dir.join("worktrees/new/gitdir")))
                .unwrap()
                .topology
        );
        assert!(
            project
                .change(&modified(
                    project.common_dir.join("worktrees/checkout.lock")
                ))
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
                let files = (0..modules)
                    .map(|module| format!("src/{module}/mod.rs"))
                    .chain(["src/main.rs".to_owned()])
                    .collect::<Vec<_>>();
                let shared = census(&files.iter().map(String::as_str).collect::<Vec<_>>());
                for worktree in &mut project.worktrees {
                    worktree.census = shared.clone();
                    for directory in worktree.census.directories.iter() {
                        std::fs::create_dir_all(worktree.absolute(directory)).unwrap();
                    }
                    std::fs::create_dir_all(worktree.git_dir.join("refs")).unwrap();
                }
                std::fs::create_dir_all(project.common_dir.join("worktrees")).unwrap();
                for path in project.metadata_paths().keys() {
                    std::fs::create_dir_all(path).unwrap();
                }
                (project.id, Arc::new(project))
            })
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let metadata = projects
            .values()
            .flat_map(|project| project.metadata_paths().into_keys())
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
    fn metadata_bursts_ignore_locks_but_keep_real_rename_destinations_and_rescans() {
        use notify::event::{AccessKind, Flag, RenameMode};

        let project = project(Path::new("/repo"));
        let linked = &project.worktrees[1];
        let mut pending = Pending::default();
        pending
            .projects
            .insert(project.id, Arc::new(project.clone()));
        pending
            .roots
            .insert(project.common_dir.clone(), BTreeSet::from([project.id]));
        assert!(pending.event(Ok(modified(linked.git_dir.join("HEAD")))));
        assert_eq!(
            pending.changes[&project.id].worktrees,
            BTreeSet::from([linked.id])
        );
        assert!(!pending.changes[&project.id].membership);
        for _ in 0..10_000 {
            for path in ["index.lock", "logs/HEAD", "objects/pack/tmp_pack"] {
                assert!(!pending.event(Ok(modified(linked.git_dir.join(path)))));
            }
            assert!(
                !pending.event(Ok(Event::new(EventKind::Access(AccessKind::Any))
                    .add_path(linked.git_dir.join("index"))))
            );
        }
        assert!(
            !pending.changes[&project.id].membership,
            "old queued work must not make noise signal new work"
        );
        let renamed = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(linked.git_dir.join("index.lock"))
            .add_path(linked.git_dir.join("index"));
        assert!(pending.event(Ok(renamed)));
        assert!(pending.changes[&project.id].membership);
        assert_eq!(
            pending.changes[&project.id].worktrees,
            BTreeSet::from([linked.id])
        );

        let refs = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(project.common_dir.join("refs/heads/main.lock"))
            .add_path(project.common_dir.join("refs/heads/main"));
        let change = project.change(&refs).unwrap();
        assert!(change.references);
        assert!(!change.membership);
        assert!(change.worktrees.is_empty());
        pending.changes.clear();
        let rescan = Event::new(EventKind::Access(AccessKind::Any)).set_flag(Flag::Rescan);
        assert!(pending.event(Ok(rescan)));
        assert!(pending.changes[&project.id].topology);
        assert!(pending.changes[&project.id].membership);
        assert_eq!(pending.changes[&project.id].worktrees.len(), 2);
    }

    #[test]
    fn metadata_status_and_topology_keep_their_original_worktree_scope() {
        use notify::event::{CreateKind, RemoveKind};

        let project = project(Path::new("/repo"));
        let linked = &project.worktrees[1];
        for path in ["HEAD", "MERGE_HEAD", "config.worktree", "rebase-merge/done"] {
            let change = project
                .change(&modified(linked.git_dir.join(path)))
                .unwrap();
            assert_eq!(change.worktrees, BTreeSet::from([linked.id]), "{path}");
            assert!(!change.membership, "{path}");
            assert!(!change.topology, "{path}");
        }
        for path in ["gitdir", "commondir", "locked"] {
            let change = project
                .change(&modified(linked.git_dir.join(path)))
                .unwrap();
            assert_eq!(change.worktrees, BTreeSet::from([linked.id]));
            assert!(change.membership && change.topology, "{path}");
        }
        for path in ["config", "packed-refs", "refs/heads/main"] {
            let change = project
                .change(&modified(project.common_dir.join(path)))
                .unwrap();
            assert!(change.references, "{path}");
            assert!(!change.membership, "{path}");
        }
        let source = linked.root.join("src");
        assert!(
            !project
                .change(&modified(source.clone()))
                .unwrap()
                .membership
        );
        for kind in [
            EventKind::Create(CreateKind::Folder),
            EventKind::Remove(RemoveKind::Folder),
        ] {
            let change = project
                .change(&Event::new(kind).add_path(source.clone()))
                .unwrap();
            assert!(change.membership);
            assert_eq!(change.worktrees, BTreeSet::from([linked.id]));
        }
        assert!(
            project
                .change(&modified(linked.root.join(".git")))
                .unwrap()
                .topology
        );
        assert!(
            project
                .change(&modified(project.common_dir.join("worktrees/new/gitdir")))
                .unwrap()
                .topology
        );
    }

    #[test]
    fn oversized_sparse_censuses_bound_filesystem_work_and_owner_storage() {
        let directory = tempfile::tempdir().unwrap();
        let mut project = project(directory.path());
        project.worktrees.truncate(1);
        let files = (0..MAX_DIRECTORIES * 4)
            .map(|index| format!("module{index:06}/file.rs"))
            .collect::<Vec<_>>();
        project.worktrees[0].census =
            Arc::new(Census::new(files.iter().map(|s| s.as_bytes()), false).unwrap());
        let root = project.worktrees[0].root.clone();
        std::fs::create_dir_all(root.join(".git/refs")).unwrap();
        std::fs::create_dir_all(root.join(".git/worktrees")).unwrap();
        let id = project.id;
        let projects = BTreeMap::from([(id, Arc::new(project))]);
        let mut inspected = BTreeSet::new();
        let mut checks = 0;
        let registrations = Registrations::within_with(&projects, MAX_DIRECTORIES, |path| {
            checks += 1;
            inspected.insert(path.to_owned());
            std::fs::symlink_metadata(path)
        });
        assert!(
            checks <= MAX_DIRECTORIES,
            "the budget bounds actual metadata calls, not only successful watches"
        );
        assert!(!inspected.contains(&root.join("module032767")));
        assert!(registrations.desired.len() <= MAX_DIRECTORIES);
        assert_eq!(
            registrations.owners.len(),
            registrations.desired.len(),
            "rejected paths must not grow the owner map"
        );
        assert_eq!(registrations.failures[&id], "directory watch limit reached");
        assert_eq!(
            registrations.desired[&root.join(".git/worktrees")],
            RecursiveMode::Recursive
        );
    }

    #[test]
    fn shared_registered_paths_keep_all_owners_when_content_work_is_exhausted() {
        let directory = tempfile::tempdir().unwrap();
        let mut first = project(directory.path());
        first.worktrees.truncate(1);
        first.worktrees[0].census = census(&["src/a/file.rs", "src/b/file.rs"]);
        let worktree = &first.worktrees[0];
        for path in [".git/refs", ".git/worktrees", "src/a", "src/b"] {
            std::fs::create_dir_all(worktree.root.join(path)).unwrap();
        }
        for path in first.metadata_paths().keys() {
            std::fs::create_dir_all(path).unwrap();
        }
        let mut second = first.clone();
        second.id = ProjectId::new();
        let ids = BTreeSet::from([first.id, second.id]);
        let metadata = first.metadata_paths().len();
        let projects = BTreeMap::from([(first.id, Arc::new(first)), (second.id, Arc::new(second))]);
        let registrations = Registrations::within(&projects, metadata + 2);
        assert_eq!(registrations.failures.len(), 2);
        for (path, owners) in &registrations.owners {
            assert_eq!(
                owners, &ids,
                "shared native watch failures/rearms must reach both owners: {path:?}"
            );
        }
        assert_eq!(registrations.desired.len(), metadata + 2);
    }

    #[test]
    fn symlink_and_missing_ancestors_are_not_followed_and_can_be_recreated() {
        let directory = tempfile::tempdir().unwrap();
        let mut project = project(directory.path());
        project.worktrees.truncate(1);
        project.worktrees[0].census = census(&[
            "a/nested/file.rs",
            "a-other/file.rs",
            "missing/nested/file.rs",
            "missing-other/file.rs",
        ]);
        let root = project.worktrees[0].root.clone();
        std::fs::create_dir_all(root.join(".git/refs")).unwrap();
        std::fs::create_dir_all(root.join(".git/worktrees")).unwrap();
        std::fs::create_dir_all(root.join("a-other")).unwrap();
        std::fs::create_dir_all(root.join("missing-other")).unwrap();
        let outside = directory.path().join("outside");
        std::fs::create_dir_all(outside.join("nested")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("a")).unwrap();
        let id = project.id;
        let projects = BTreeMap::from([(id, Arc::new(project))]);
        let registrations = Registrations::within(&projects, MAX_DIRECTORIES);
        assert!(registrations.desired.contains_key(&root));
        assert!(registrations.desired.contains_key(&root.join("a-other")));
        for path in ["a", "a/nested", "missing", "missing/nested"] {
            assert!(
                !registrations.desired.contains_key(&root.join(path)),
                "{path}"
            );
        }
        std::fs::remove_file(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("a/nested")).unwrap();
        std::fs::create_dir_all(root.join("missing/nested")).unwrap();
        let registrations = Registrations::within(&projects, MAX_DIRECTORIES);
        for path in ["a", "a/nested", "missing", "missing/nested"] {
            assert!(
                registrations.desired.contains_key(&root.join(path)),
                "{path}"
            );
        }
        assert!(registrations.failures.is_empty());
    }

    #[test]
    fn supplemental_directory_walk_bounds_plain_file_entries() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..100 {
            std::fs::write(directory.path().join(format!("file-{index}")), b"file").unwrap();
        }
        let mut remaining = 7;
        let (directories, incomplete) = content_directories(
            directory.path(),
            &[],
            &CancellationToken::default(),
            &mut remaining,
        )
        .unwrap();
        assert!(incomplete);
        assert_eq!(remaining, 0);
        assert_eq!(directories.iter().collect::<Vec<_>>(), [Path::new("")]);
    }

    #[test]
    fn stash_log_is_shared_panel_metadata_while_other_logs_and_locks_are_noise() {
        let project = project(Path::new("/repo"));
        for path in ["refs/stash", "logs/refs/stash"] {
            let change = project
                .change(&modified(project.common_dir.join(path)))
                .unwrap();
            assert!(change.metadata && change.shared_snapshot);
            assert!(!change.references && change.worktrees.is_empty());
        }
        for path in [
            "refs/stash.lock",
            "logs/HEAD",
            "logs/refs/heads/main",
            "objects/pack/tmp",
        ] {
            assert!(
                project
                    .change(&modified(project.common_dir.join(path)))
                    .is_none(),
                "{path}"
            );
        }
    }

    #[test]
    fn selected_worktrees_share_content_after_all_metadata_with_one_budget() {
        let directory = tempfile::tempdir().unwrap();
        let mut first = project(&directory.path().join("first-project"));
        let mut second = project(&directory.path().join("second-project"));
        first.worktrees.truncate(1);
        second.worktrees.truncate(1);
        for project in [&mut first, &mut second] {
            project.worktrees[0].census = census(&["src/a/file.rs", "src/b/file.rs"]);
        }
        for project in [&first, &second] {
            for path in project.metadata_paths().keys() {
                std::fs::create_dir_all(path).unwrap();
            }
            for relative in project.worktrees[0].census.directories.iter() {
                std::fs::create_dir_all(project.worktrees[0].absolute(relative)).unwrap();
            }
        }
        let selected = [first.worktrees[0].id, second.worktrees[0].id];
        let roots = [
            first.worktrees[0].root.clone(),
            second.worktrees[0].root.clone(),
        ];
        let projects = BTreeMap::from([(first.id, Arc::new(first)), (second.id, Arc::new(second))]);
        let metadata = projects
            .values()
            .flat_map(|project| project.metadata_paths().into_keys())
            .collect::<BTreeSet<_>>();
        let limit = metadata.len() + 4;
        let mut inspected = Vec::new();
        let registrations =
            Registrations::prioritized_with(&projects, &[], &selected, limit, |path| {
                inspected.push(path.to_path_buf());
                std::fs::symlink_metadata(path)
            });
        for path in &metadata {
            assert!(registrations.desired.contains_key(path));
        }
        for root in roots {
            assert!(registrations.desired.contains_key(&root));
            assert!(registrations.desired.contains_key(&root.join("src")));
            assert!(registrations.desired.contains_key(&root.join("src/a")));
            assert!(!registrations.desired.contains_key(&root.join("src/b")));
        }
        assert_eq!(
            inspected[..metadata.len()]
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            metadata
        );
        assert!(registrations.desired.len() <= limit && inspected.len() <= limit);
        assert_eq!(registrations.failures.len(), 2);
    }

    #[test]
    fn native_watcher_observes_first_file_in_initial_and_new_empty_directories() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        git(root, &["init", "-b", "main"]);
        std::fs::create_dir_all(root.join("empty/nested")).unwrap();
        std::fs::create_dir_all(root.join("target/ignored")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        let service = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        let records = service.list_worktrees(root, &cancellation).unwrap();
        let id = ProjectId::new();
        let common = service.common_dir(root, &cancellation).unwrap();
        let changed = records.iter().map(|record| record.id).collect();
        let plan =
            Project::read_changed(id, common.clone(), &records, &cancellation, None, &changed)
                .unwrap();
        assert!(
            plan.worktrees[0]
                .directories
                .contains(Path::new("empty/nested"))
        );
        assert!(!plan.worktrees[0].directories.contains(Path::new("target")));
        let (wake, woken) = mpsc::sync_channel(1);
        let monitor = (
            Monitor::start(move || {
                let _ = wake.try_send(());
            })
            .unwrap(),
            woken,
        );
        monitor.0.configure(vec![Arc::new(plan.clone())]);
        wait_for(&monitor, id, "initial empty directory", |change| {
            change.coverage == Some(true)
        });
        std::fs::write(root.join("empty/nested/first.txt"), "first\n").unwrap();
        wait_for(&monitor, id, "first untracked file", |change| {
            change.worktrees.contains(&records[0].id)
        });
        std::fs::create_dir(root.join("later")).unwrap();
        wait_for(&monitor, id, "new empty directory", |change| {
            change.membership
        });
        let next =
            Project::read_changed(id, common, &records, &cancellation, Some(&plan), &changed)
                .unwrap();
        assert!(next.worktrees[0].directories.contains(Path::new("later")));
        monitor.0.configure(vec![Arc::new(next)]);
        wait_for(&monitor, id, "new directory rearmed", |change| {
            change.coverage == Some(true)
        });
        std::fs::write(root.join("later/first.txt"), "second\n").unwrap();
        wait_for(&monitor, id, "later first untracked file", |change| {
            change.worktrees.contains(&records[0].id)
        });
    }

    #[test]
    fn native_watcher_replans_unignored_directories_and_observes_stash_log_only() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        git(root, &["init", "-b", "main"]);
        git(root, &["config", "user.name", "Fixture"]);
        git(root, &["config", "user.email", "fixture@example.invalid"]);
        std::fs::write(root.join("tracked.txt"), "original\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        git(root, &["commit", "-m", "fixture"]);
        std::fs::write(root.join("tracked.txt"), "stash\n").unwrap();
        git(root, &["stash", "push", "-m", "fixture stash"]);
        std::fs::create_dir_all(root.join("hidden/deep")).unwrap();
        std::fs::write(root.join(".git/info/exclude"), "hidden/\n").unwrap();
        let service = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        let records = service.list_worktrees(root, &cancellation).unwrap();
        let id = ProjectId::new();
        let common = service.common_dir(root, &cancellation).unwrap();
        let changed = records.iter().map(|record| record.id).collect();
        let plan =
            Project::read_changed(id, common.clone(), &records, &cancellation, None, &changed)
                .unwrap();
        assert!(!plan.worktrees[0].directories.contains(Path::new("hidden")));
        let (wake, woken) = mpsc::sync_channel(1);
        let monitor = (
            Monitor::start(move || {
                let _ = wake.try_send(());
            })
            .unwrap(),
            woken,
        );
        monitor.0.configure(vec![Arc::new(plan.clone())]);
        wait_for(&monitor, id, "initial ignore and stash watches", |change| {
            change.coverage == Some(true)
        });
        let stash_ref = std::fs::read(common.join("refs/stash")).unwrap();
        let stash_log = common.join("logs/refs/stash");
        let original_log = std::fs::read(&stash_log).unwrap();
        std::fs::write(&stash_log, &original_log).unwrap();
        wait_for(&monitor, id, "stash-log-only write", |change| {
            change.shared_snapshot
                && change.metadata
                && !change.references
                && change.worktrees.is_empty()
        });
        assert_eq!(std::fs::read(common.join("refs/stash")).unwrap(), stash_ref);
        std::fs::write(common.join("info/exclude"), "").unwrap();
        wait_for(&monitor, id, "unignore directory", |change| {
            change.membership && change.metadata
        });
        let next =
            Project::read_changed(id, common, &records, &cancellation, Some(&plan), &changed)
                .unwrap();
        assert!(
            next.worktrees[0]
                .directories
                .contains(Path::new("hidden/deep"))
        );
        monitor.0.configure(vec![Arc::new(next)]);
        wait_for(&monitor, id, "unignored directory rearmed", |change| {
            change.coverage == Some(true)
        });
        std::fs::write(root.join("hidden/deep/first.txt"), "visible\n").unwrap();
        wait_for(&monitor, id, "first unignored file", |change| {
            change.worktrees.contains(&records[0].id)
        });
    }

    fn wait_for(
        (monitor, woken): &(Monitor, mpsc::Receiver<()>),
        project: ProjectId,
        phase: &str,
        predicate: impl Fn(&Change) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last_change = None;
        loop {
            if woken.try_recv().is_ok()
                && let Some(change) = monitor.drain().remove(&project)
            {
                if predicate(&change) {
                    return;
                }
                last_change = Some(change);
            }
            assert!(
                Instant::now() < deadline,
                "watch event did not arrive during {phase}; last change: {last_change:?}"
            );
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
        let (wake, woken) = mpsc::sync_channel(1);
        let monitor = (
            Monitor::start(move || {
                let _ = wake.try_send(());
            })
            .unwrap(),
            woken,
        );
        monitor.0.configure(vec![Arc::new(project.clone())]);
        wait_for(&monitor, project.id, "initial configuration", |change| {
            change.coverage == Some(true) && change.error.is_none()
        });
        let replacement = source.join(".save");
        std::fs::write(&replacement, "second").unwrap();
        std::fs::rename(&replacement, &file).unwrap();
        wait_for(&monitor, project.id, "atomic replacement", |change| {
            change.worktrees.contains(&worktree.id)
        });
        // Retain the old inode so the filesystem cannot immediately reuse its
        // identity for the replacement this test expects us to re-register.
        let original_directory = std::fs::File::open(&source).unwrap();
        let original_identity = original_directory.metadata().unwrap();
        std::fs::remove_dir_all(&source).unwrap();
        std::fs::create_dir(&source).unwrap();
        let replacement_identity = std::fs::metadata(&source).unwrap();
        assert_ne!(
            (original_identity.dev(), original_identity.ino()),
            (replacement_identity.dev(), replacement_identity.ino())
        );
        std::fs::write(&file, "third").unwrap();
        wait_for(&monitor, project.id, "directory recreation", |change| {
            change.membership
        });
        // The path set is unchanged, but the directory inode changed. Rearm
        // it instead of trusting a stale native registration for that path.
        monitor.0.configure(vec![Arc::new(project.clone())]);
        // Configure completion must be acknowledged even when paths compare
        // equal, so edits cannot race the re-registration barrier.
        wait_for(&monitor, project.id, "rearm acknowledgement", |change| {
            change.coverage == Some(true) && change.error.is_none()
        });
        std::fs::write(&file, "fourth").unwrap();
        wait_for(&monitor, project.id, "post-rearm tracked edit", |change| {
            change.worktrees.contains(&worktree.id)
        });
        drop(original_directory);
    }
}
