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

use notify::{Event, EventKind, RecursiveMode, Watcher};

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
    // Files from the index stay shared; independently observed directories
    // include empty/untracked folders without expanding the index census.
    directories: Arc<PathSet>,
    branch: Option<String>,
    upstream: Option<String>,
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

    fn directories(&self) -> impl Iterator<Item = &Path> {
        let mut tracked = self.census.directories.iter().peekable();
        let mut observed = self.directories.iter().peekable();
        std::iter::from_fn(move || match (tracked.peek(), observed.peek()) {
            (Some(left), Some(right)) => match left
                .as_os_str()
                .as_bytes()
                .cmp(right.as_os_str().as_bytes())
            {
                Ordering::Less => tracked.next(),
                Ordering::Greater => observed.next(),
                Ordering::Equal => {
                    observed.next();
                    tracked.next()
                }
            },
            (Some(_), None) => tracked.next(),
            (None, Some(_)) => observed.next(),
            (None, None) => None,
        })
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

/// Enumerate through retained directory descriptors, never symlinks. Both
/// directory state and entry/metadata work are bounded, including flat trees
/// full of files or ignored directories. Incomplete coverage explicitly polls.
fn content_directories(
    root: &Path,
    ignored: &[crate::repository::RepoPath],
    cancellation: &CancellationToken,
    limit: usize,
) -> Result<(BTreeSet<PathBuf>, bool), String> {
    use std::os::fd::AsRawFd;
    let handle = crate::fs_safety::Root::open(root).map_err(|error| error.to_string())?;
    let ignored = ignored
        .iter()
        .map(|path| path.as_path())
        .collect::<BTreeSet<_>>();
    let limit = limit.min(MAX_DIRECTORIES);
    let mut remaining = limit;
    let mut pending = vec![PathBuf::new()];
    let mut directories = BTreeSet::new();
    while let Some(relative) = pending.pop() {
        if cancellation.is_cancelled() {
            return Err("watch planning cancelled".into());
        }
        if remaining == 0 {
            return Ok((directories, true));
        }
        remaining -= 1;
        let reader;
        let entries = if relative.as_os_str().is_empty() {
            std::fs::read_dir(format!("/proc/self/fd/{}", handle.descriptor()))
                .map_err(|error| error.to_string())?
        } else {
            reader = match crate::fs_safety::open_beneath(&handle, &relative) {
                Ok(reader) => reader,
                Err(error) if error.is_not_found() => continue,
                Err(error) => return Err(error.to_string()),
            };
            std::fs::read_dir(format!("/proc/self/fd/{}", reader.as_raw_fd()))
                .map_err(|error| error.to_string())?
        };
        directories.insert(relative.clone());
        for entry in entries {
            if cancellation.is_cancelled() {
                return Err("watch planning cancelled".into());
            }
            if remaining == 0 {
                return Ok((directories, true));
            }
            remaining -= 1;
            let entry = entry.map_err(|error| error.to_string())?;
            if entry.file_name() == ".git" {
                continue;
            }
            let child = relative.join(entry.file_name());
            if child.ancestors().any(|ancestor| ignored.contains(ancestor)) {
                continue;
            }
            if !entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                continue;
            }
            if remaining == 0 {
                return Ok((directories, true));
            }
            remaining -= 1;
            // A nested checkout (including a gitfile) owns its own state.
            match crate::fs_safety::metadata_beneath(&handle, &child.join(".git")) {
                Ok(_) => continue,
                Err(error) if error.is_not_found() => {}
                Err(error) => return Err(error.to_string()),
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
    // UI directory coverage is independent of ordinary index publication;
    // the host intentionally retains its original index-only invalidation.
    directory_inventory: bool,
    polling: Option<String>,
}

impl Project {
    /// The host reader keeps its index-only census; sidebar inventory work is
    /// explicit in read_changed_with_upstreams and never adds host scan cost.
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
            let census = {
                let index = git
                    .index_paths(&record.path, cancellation)
                    .map_err(|error| error.to_string())?;
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
            let worktree_polling = if census.submodules {
                Some("submodule contents require a periodic local Git check".into())
            } else {
                network_filesystem(&record.path)
                    .map_err(|error| error.to_string())?
                    .then(|| "network or FUSE filesystem may omit native change events".to_owned())
            };
            worktrees.push(Worktree {
                id: record.id,
                root: record.path.clone(),
                git_dir,
                census,
                directories: Arc::new(PathSet::new([])?),
                branch: record.branch.clone(),
                upstream: None,
                polling: worktree_polling,
            });
        }
        Ok(Self {
            id,
            common_dir,
            worktrees,
            directory_inventory: false,
            polling,
        })
    }

    pub fn read_changed_with_upstreams(
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
            MAX_DIRECTORIES,
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
        let previous =
            previous.filter(|project| project.id == id && project.common_dir == common_dir);
        let mut worktrees: Vec<Worktree> = Vec::new();
        let mut planning_failure = None;
        let polling = network_filesystem(&common_dir)
            .map_err(|error| error.to_string())?
            .then(|| "network or FUSE filesystem may omit native change events".to_owned());
        for record in records.iter().filter(|record| !record.bare) {
            if cancellation.is_cancelled() {
                return Err("watch planning cancelled".into());
            }
            let old = previous.and_then(|project| {
                project
                    .worktrees
                    .iter()
                    .find(|old| old.id == record.id && old.root == record.path)
            });
            if !changed.contains(&record.id)
                && previous.is_some_and(|project| project.directory_inventory)
                && let Some(old) = old
            {
                let mut retained = old.clone();
                retained.branch = record.branch.clone();
                retained.upstream = upstreams.get(&record.id).cloned();
                worktrees.push(retained);
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
                let census = match git.index_paths(&record.path, cancellation) {
                    Ok(index) => {
                        let census = Census::new(
                            index.paths.iter().map(|path| path.as_os_str().as_bytes()),
                            index.gitlinks,
                        )?;
                        worktrees
                            .iter()
                            .chain(previous.iter().flat_map(|previous| &previous.worktrees))
                            .map(|worktree| &worktree.census)
                            .find(|known| ***known == census)
                            .cloned()
                            .unwrap_or_else(|| Arc::new(census))
                    }
                    Err(error) => {
                        worktree_polling = Some(error.to_string());
                        // Retain the actual shared index, without copying a
                        // large census just because its next read failed.
                        match old {
                            Some(old) => old.census.clone(),
                            None => Arc::new(Census::new([], false)?),
                        }
                    }
                };
                if census.submodules {
                    worktree_polling.get_or_insert_with(|| {
                        "submodule contents require a periodic local Git check".into()
                    });
                }
                let mut directories = BTreeSet::new();
                match git.watch_files_projection(&record.path, inventory_limit, cancellation) {
                    Ok(files) => {
                        if files.listing_truncated {
                            worktree_polling
                                .get_or_insert_with(|| "directory watch limit reached".into());
                        }
                        for file in &files.paths {
                            directories
                                .extend(file.as_path().ancestors().skip(1).map(Path::to_owned));
                        }
                        match git.ignored_files_with_limit(
                            &record.path,
                            &files.paths,
                            Some(inventory_limit),
                            cancellation,
                        ) {
                            Ok(ignored) if !ignored.listing_truncated => {
                                match content_directories(
                                    &record.path,
                                    &ignored.ignored_directories,
                                    cancellation,
                                    MAX_DIRECTORIES,
                                ) {
                                    Ok((observed, incomplete)) => {
                                        directories.extend(observed);
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
                                worktree_polling
                                    .get_or_insert_with(|| "directory watch limit reached".into());
                            }
                            Err(error) => worktree_polling = Some(error.to_string()),
                        }
                    }
                    Err(error) => worktree_polling = Some(error.to_string()),
                }
                if cancellation.is_cancelled() {
                    return Err("watch planning cancelled".into());
                }
                let directories =
                    PathSet::new(directories.iter().map(|path| path.as_os_str().as_bytes()))?;
                let directories = worktrees
                    .iter()
                    .chain(previous.iter().flat_map(|previous| &previous.worktrees))
                    .map(|worktree| &worktree.directories)
                    .find(|known| ***known == directories)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(directories));
                Ok(Worktree {
                    id: record.id,
                    root: record.path.clone(),
                    git_dir,
                    census,
                    directories,
                    branch: record.branch.clone(),
                    upstream: upstreams.get(&record.id).cloned(),
                    polling: worktree_polling,
                })
            })();
            if cancellation.is_cancelled() {
                return Err("watch planning cancelled".into());
            }
            match planned {
                Ok(worktree) => worktrees.push(worktree),
                Err(error) => {
                    if let Some(old) = old {
                        let mut retained = old.clone();
                        retained.branch = record.branch.clone();
                        retained.upstream = upstreams.get(&record.id).cloned();
                        retained.polling = Some(error);
                        worktrees.push(retained);
                    } else {
                        planning_failure = Some(error);
                    }
                }
            }
        }
        Ok(Self {
            id,
            common_dir,
            worktrees,
            directory_inventory: true,
            polling: polling.or(planning_failure),
        })
    }

    /// Check the freshly accepted status before its entries are discarded for
    /// display. Ordinary staging in a watched directory needs no inventory.
    /// A force-added ignored path or newly introduced submodule needs the
    /// existing row planner to repair coverage instead of growing a path cache.
    /// Invoke only for an index-like metadata+untracked edge. A plain content
    /// edit must not force this census. Truncated status is uncertain, but an
    /// already-polling row retains its explicit incomplete-coverage fallback.
    pub(crate) fn status_needs_replan(
        &self,
        record: &WorktreeRecord,
        status: &crate::repository::StatusSnapshot,
    ) -> bool {
        let Some(worktree) = self
            .worktrees
            .iter()
            .find(|worktree| worktree.id == record.id && worktree.root == record.path)
        else {
            return true;
        };
        for entry in status.entries.iter().take(MAX_DIRECTORIES) {
            if entry.submodule && !worktree.census.submodules {
                return true;
            }
            let path = worktree.root.join(entry.path.as_path());
            if worktree.tracks(&path).is_none()
                && !path
                    .parent()
                    .is_some_and(|parent| worktree.tracks(parent) == Some(Tracked::Directory))
            {
                return true;
            }
        }
        // A capped predicate never establishes full coverage. An existing
        // polling reason already makes that limitation explicit; avoid
        // repeatedly rebuilding an equally capped plan on ordinary edits.
        (status.listing_truncated || status.entries.len() > MAX_DIRECTORIES)
            && worktree.polling.is_none()
    }

    /// Reconcile an older worker plan with newer accepted row facts. This is
    /// entirely in memory: content coverage is retained, routing is refreshed,
    /// and only a missing prior census asks the worker to plan again.
    pub(crate) fn reconcile_coverage<'a>(
        &mut self,
        previous: Option<&Self>,
        records: impl IntoIterator<Item = &'a WorktreeRecord>,
        preserved: &BTreeSet<WorktreeId>,
        upstreams: &BTreeMap<WorktreeId, String>,
    ) -> BTreeSet<WorktreeId> {
        let records = records
            .into_iter()
            .filter(|record| !record.bare)
            .map(|record| (record.id, record))
            .collect::<BTreeMap<_, _>>();
        let previous = previous.filter(|project| {
            project.id == self.id
                && project.common_dir == self.common_dir
                && project.directory_inventory == self.directory_inventory
        });
        let replacements = previous
            .into_iter()
            .flat_map(|project| &project.worktrees)
            .filter(|worktree| {
                preserved.contains(&worktree.id)
                    && records
                        .get(&worktree.id)
                        .is_some_and(|record| record.path == worktree.root)
            })
            .collect::<Vec<_>>();
        let replacement_ids = replacements
            .iter()
            .map(|worktree| worktree.id)
            .collect::<BTreeSet<_>>();
        self.worktrees.retain(|worktree| {
            !replacement_ids.contains(&worktree.id)
                && records
                    .get(&worktree.id)
                    .is_some_and(|record| record.path == worktree.root)
        });
        self.worktrees.extend(replacements.into_iter().cloned());
        for worktree in &mut self.worktrees {
            worktree.branch = records[&worktree.id].branch.clone();
            worktree.upstream = upstreams.get(&worktree.id).cloned();
        }
        let covered = self
            .worktrees
            .iter()
            .map(|worktree| worktree.id)
            .collect::<BTreeSet<_>>();
        preserved
            .iter()
            .copied()
            .filter(|id| records.contains_key(id) && !covered.contains(id))
            .collect()
    }

    /// A successful bound read can repair missed HEAD/upstream events without
    /// another census. Keep the existing Arc/coverage on unchanged reads.
    pub(crate) fn refresh_worktree_routing(
        &self,
        record: &WorktreeRecord,
        upstream: Option<&str>,
    ) -> Option<Self> {
        let index = self
            .worktrees
            .iter()
            .position(|worktree| worktree.id == record.id && worktree.root == record.path)?;
        let current = &self.worktrees[index];
        if current.branch == record.branch && current.upstream.as_deref() == upstream {
            return None;
        }
        let mut next = self.clone();
        next.worktrees[index].branch = record.branch.clone();
        next.worktrees[index].upstream = upstream.map(str::to_owned);
        Some(next)
    }

    fn polling_reason(&self) -> Option<&str> {
        self.polling.as_deref().or_else(|| {
            self.worktrees
                .iter()
                .find_map(|worktree| worktree.polling.as_deref())
        })
    }

    /// Partial status reads can learn a missed HEAD change too. Refresh only
    /// pure routing metadata, retaining the existing census and polling reason.
    pub fn refresh_routing<'a>(
        &self,
        records: impl IntoIterator<Item = &'a WorktreeRecord>,
        upstreams: &BTreeMap<WorktreeId, String>,
    ) -> Option<Self> {
        let records = records
            .into_iter()
            .map(|record| (record.id, record))
            .collect::<BTreeMap<_, _>>();
        let changed = self.worktrees.iter().any(|worktree| {
            records.get(&worktree.id).is_some_and(|record| {
                record.path == worktree.root
                    && (record.branch != worktree.branch
                        || worktree.upstream.as_ref() != upstreams.get(&worktree.id))
            })
        });
        if !changed {
            return None;
        }
        let mut next = self.clone();
        for worktree in &mut next.worktrees {
            if let Some(record) = records
                .get(&worktree.id)
                .filter(|record| record.path == worktree.root)
            {
                worktree.branch = record.branch.clone();
                worktree.upstream = upstreams.get(&worktree.id).cloned();
            }
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
            let stash = common.is_some_and(|relative| {
                relative == Path::new("refs/stash") || relative == Path::new("logs/refs/stash")
            });
            if stash
                && !path
                    .file_name()
                    .is_some_and(|name| name.as_bytes().ends_with(b".lock"))
            {
                change.references = true;
                change.metadata = true;
                change.shared_snapshot = true;
                continue;
            }
            if common.is_some_and(common_metadata_noise) {
                continue;
            }
            if let Some(relative) = common.filter(|relative| relative.starts_with("refs")) {
                // Host Work inventory needs every shared ref, including a new
                // branch without any currently checked-out worktree.
                change.references = true;
                change.metadata = true;
                change.membership |= relative == Path::new("refs");
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
            if common.is_some_and(|relative| {
                matches!(
                    relative.to_str(),
                    Some("config" | "packed-refs" | "info" | "info/exclude")
                )
            }) {
                change.references |= common.is_some_and(|relative| {
                    matches!(relative.to_str(), Some("config" | "packed-refs"))
                });
                change.metadata = true;
                change
                    .worktrees
                    .extend(self.worktrees.iter().map(|worktree| worktree.id));
                change.membership |= common != Some(Path::new("packed-refs"));
                change.untracked |= common != Some(Path::new("packed-refs"));
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
                    change.untracked |= matches!(
                        relative.to_str(),
                        Some("index" | "config.worktree" | "info/exclude")
                    );
                    let topology = relative.as_os_str().is_empty()
                        || matches!(relative.to_str(), Some("locked" | "gitdir" | "commondir"));
                    change.topology |= topology;
                    // Host index coverage and UI directory inventory have
                    // distinct invalidation. UI status repairs exceptional
                    // uncovered index paths without scanning on every stage.
                    let index = relative == Path::new("index")
                        || relative.as_os_str().as_bytes().starts_with(b"sharedindex.");
                    change.untracked |= index;
                    change.membership |= topology
                        || (!self.directory_inventory && index)
                        || (self.directory_inventory && relative == Path::new("HEAD"))
                        || matches!(relative.to_str(), Some("config.worktree" | "info/exclude"))
                        || relative == Path::new("refs");
                } else if let Some(tracked) = worktree.tracks(path).or_else(|| {
                    path.parent()
                        .filter(|parent| worktree.tracks(parent) == Some(Tracked::Directory))
                        .map(|_| Tracked::File)
                }) {
                    change.worktrees.insert(worktree.id);
                    change.untracked |= matches!(
                        event.kind,
                        EventKind::Create(_)
                            | EventKind::Remove(_)
                            | EventKind::Modify(notify::event::ModifyKind::Name(_))
                    );
                    change.untracked |= path.file_name().is_some_and(|name| name == ".gitignore");
                    change.membership |= path.file_name().is_some_and(|name| name == ".gitignore")
                        || matches!(
                            event.kind,
                            EventKind::Create(notify::event::CreateKind::Folder)
                                | EventKind::Remove(notify::event::RemoveKind::Folder)
                        );
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
                        );
                    change.topology |= path == &worktree.root;
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
    pub metadata: bool,
    pub shared_snapshot: bool,
    pub untracked: bool,
    pub topology: bool,
    pub membership: bool,
    pub error: Option<String>,
    pub coverage: Option<bool>,
}

impl Change {
    pub fn merge(&mut self, other: Self) {
        self.worktrees.extend(other.worktrees);
        self.references |= other.references;
        self.metadata |= other.metadata;
        self.shared_snapshot |= other.shared_snapshot;
        self.untracked |= other.untracked;
        self.topology |= other.topology;
        self.membership |= other.membership;
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
    // Only desired recursive roots are retained, never an unbounded list of
    // failed descendants. Unknown quota errors stay degraded until removal.
    recursive: BTreeMap<PathBuf, BTreeSet<ProjectId>>,
    quota_rearm: BTreeSet<PathBuf>,
    quota_unknown: BTreeSet<ProjectId>,
}

#[derive(Default)]
struct Configuration {
    projects: Vec<Arc<Project>>,
    priority: Vec<ProjectId>,
    selected: Vec<WorktreeId>,
    stopped: bool,
}

impl Pending {
    fn event(&mut self, event: notify::Result<Event>) -> bool {
        if event.as_ref().is_ok_and(access_only) {
            return false;
        }
        if let Err(error) = &event {
            if matches!(error.kind, notify::ErrorKind::MaxFilesWatch) {
                let mut unknown = error.paths.is_empty();
                for path in &error.paths {
                    if let Some(root) = path
                        .ancestors()
                        .find(|root| self.recursive.contains_key(*root))
                    {
                        self.quota_rearm.insert(root.to_owned());
                    } else {
                        unknown = true;
                    }
                }
                if unknown {
                    self.quota_unknown.extend(self.projects.keys());
                }
            } else {
                self.faulted = true;
            }
        }
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

    fn rearm(
        &mut self,
        desired: &BTreeMap<PathBuf, RecursiveMode>,
        owners: &BTreeMap<PathBuf, BTreeSet<ProjectId>>,
    ) -> Rearm {
        self.recursive = desired
            .iter()
            .filter(|(_, mode)| **mode == RecursiveMode::Recursive)
            .map(|(path, _)| (path.clone(), owners.get(path).cloned().unwrap_or_default()))
            .collect();
        self.quota_unknown
            .retain(|id| self.projects.contains_key(id));
        self.quota_rearm
            .retain(|path| self.recursive.contains_key(path));
        Rearm {
            all: std::mem::take(&mut self.faulted),
            recursive: std::mem::take(&mut self.quota_rearm),
        }
    }

    fn retain_failures(&self, failures: &mut BTreeMap<ProjectId, String>) {
        // Native operations run without the Pending mutex: the backend calls
        // back on its own thread. A new fault during rearm must win over that
        // operation's success, including another error for the same root.
        for &id in &self.quota_unknown {
            failures.insert(
                id,
                "file watch limit reached; coverage remains unknown".into(),
            );
        }
        for id in self
            .quota_rearm
            .iter()
            .filter_map(|root| self.recursive.get(root))
            .flatten()
        {
            failures.insert(
                *id,
                "file watch limit reached; recursive rearm required".into(),
            );
        }
        if self.faulted {
            for &id in self.projects.keys() {
                failures.insert(id, "file watcher recovery required".into());
            }
        }
    }
}

#[derive(Default)]
struct Rearm {
    all: bool,
    recursive: BTreeSet<PathBuf>,
}

/// What the watcher should hold for a set of projects, and which of them it
/// could not cover in full.
#[derive(Default)]
struct Registrations {
    desired: BTreeMap<PathBuf, RecursiveMode>,
    metadata: BTreeSet<PathBuf>,
    owners: BTreeMap<PathBuf, BTreeSet<ProjectId>>,
    failures: BTreeMap<ProjectId, String>,
}

impl Registrations {
    /// Metadata gets first claim on the watch and filesystem-work budgets.
    /// Content traversal also has a candidate budget: missing or symlinked
    /// directories cannot make a large census an unbounded planning job.
    /// Projects left short of coverage retain the existing polling fallback.
    fn within(projects: &BTreeMap<ProjectId, Arc<Project>>, limit: usize) -> Self {
        Self::prioritized(projects, &[], &[], limit)
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

    fn within_with(
        projects: &BTreeMap<ProjectId, Arc<Project>>,
        limit: usize,
        inspect: impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
    ) -> Self {
        Self::prioritized_with(projects, &[], &[], limit, inspect)
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
        let mut candidates = limit;
        let mut ordered = projects.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|(id, _)| {
            priority
                .iter()
                .position(|candidate| candidate == *id)
                .unwrap_or(usize::MAX)
        });
        let hot = projects
            .iter()
            .filter(|(_, project)| {
                project
                    .worktrees
                    .iter()
                    .any(|row| selected.contains(&row.id))
            })
            .map(|(&id, _)| id)
            .collect::<BTreeSet<_>>();
        for (&id, project) in ordered.iter().copied() {
            if let Some(reason) = project.polling_reason() {
                registrations.failures.insert(id, reason.to_owned());
            }
        }
        // Choose the bounded desired set first. Reserve independently selected
        // content before background metadata can consume its entire budget.
        // reconcile() still attempts EVERY chosen metadata path before content.
        for (&id, project) in ordered
            .iter()
            .copied()
            .filter(|(id, _)| hot.is_empty() || hot.contains(*id))
        {
            registrations.metadata(project, id, limit, &mut checks, &mut inspect);
        }
        let mut seen = BTreeSet::new();
        let mut preferred = selected
            .iter()
            .filter(|id| seen.insert(**id))
            .filter_map(|selected| {
                ordered.iter().copied().find_map(|(&id, project)| {
                    let row = project.worktrees.iter().find(|row| row.id == *selected)?;
                    Some((id, row, row.directories(), BTreeSet::new()))
                })
            })
            .collect::<Vec<_>>();
        loop {
            let mut progressed = false;
            for (id, row, paths, blocked) in &mut preferred {
                if let Some(path) = paths.next() {
                    progressed = true;
                    registrations.content_path(
                        *id,
                        row,
                        path,
                        blocked,
                        limit,
                        &mut candidates,
                        &mut checks,
                        &mut inspect,
                    );
                }
            }
            if !progressed || candidates == 0 || checks == 0 {
                break;
            }
        }
        if !hot.is_empty() {
            for (&id, project) in ordered.iter().copied().filter(|(id, _)| !hot.contains(*id)) {
                registrations.metadata(project, id, limit, &mut checks, &mut inspect);
            }
        }
        let mut roots: BTreeMap<&Path, Vec<(ProjectId, &Worktree)>> = BTreeMap::new();
        for (&id, project) in ordered.iter().copied() {
            for worktree in &project.worktrees {
                roots
                    .entry(&worktree.root)
                    .or_default()
                    .push((id, worktree));
                if selected.contains(&worktree.id) {
                    // A stopped selected iterator must not silently claim full
                    // coverage, nor consume the same candidate budget twice.
                    if preferred.iter_mut().any(|(owner, row, paths, _)| {
                        *owner == id && row.id == worktree.id && paths.next().is_some()
                    }) {
                        registrations.exhausted(id);
                    }
                    continue;
                }
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
        // Attach all owners of already-selected paths without rescanning the
        // unbounded portion of another worktree's compact census.
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

    fn metadata(
        &mut self,
        project: &Project,
        id: ProjectId,
        limit: usize,
        checks: &mut usize,
        inspect: &mut impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
    ) {
        for (path, mode) in project.metadata_paths() {
            if self.claim(id, path.clone(), mode, limit, checks, inspect) == Some(true) {
                self.metadata.insert(path);
            }
        }
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
        for relative in worktree.directories() {
            if !self.content_path(
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

    fn content_path(
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
                        metadata,
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
                    let rearm = state
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .rearm(&desired, &owners);
                    let rearmed = match &mut watcher {
                        Ok(watcher) => reconcile(
                            watcher,
                            &mut registered,
                            &desired,
                            &metadata,
                            &owners,
                            &rearm,
                            &mut failures,
                        ),
                        Err(error) => {
                            failures.extend(projects.keys().map(|id| (*id, error.to_string())));
                            BTreeSet::new()
                        }
                    };
                    let mut pending = state.lock().unwrap_or_else(|p| p.into_inner());
                    pending.retain_failures(&mut failures);
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
    watcher: &mut impl Watcher,
    registered: &mut BTreeMap<PathBuf, (RecursiveMode, u64, u64)>,
    desired: &BTreeMap<PathBuf, RecursiveMode>,
    metadata: &BTreeSet<PathBuf>,
    owners: &BTreeMap<PathBuf, BTreeSet<ProjectId>>,
    rearm: &Rearm,
    failures: &mut BTreeMap<ProjectId, String>,
) -> BTreeSet<ProjectId> {
    let mut rearmed = BTreeSet::new();
    registered.retain(|path, (mode, device, inode)| {
        if !rearm.all
            && !rearm.recursive.contains(path)
            && desired.get(path) == Some(&*mode)
            && std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.dev() == *device && metadata.ino() == *inode)
        {
            true
        } else {
            let _ = watcher.unwatch(path);
            false
        }
    });
    // Planning priority must survive the path-sorted map: content from an
    // earlier project must not consume native capacity before later metadata.
    for (path, &mode) in desired
        .iter()
        .filter(|(path, _)| metadata.contains(*path))
        .chain(desired.iter().filter(|(path, _)| !metadata.contains(*path)))
    {
        if registered.contains_key(path) {
            continue;
        }
        let mut result = watcher.watch(path, mode);
        if metadata.contains(path)
            && result
                .as_ref()
                .is_err_and(|error| matches!(error.kind, notify::ErrorKind::MaxFilesWatch))
        {
            // Existing content may occupy all native capacity when a new
            // project's metadata arrives. Only quota refusal can preempt it;
            // every evicted path is admitted again in the content pass below.
            let content = registered
                .keys()
                .filter(|path| !metadata.contains(*path))
                .cloned()
                .collect::<Vec<_>>();
            if !content.is_empty() {
                for path in content {
                    let _ = watcher.unwatch(&path);
                    registered.remove(&path);
                }
                result = watcher.watch(path, mode);
            }
        }
        let result = result.and_then(|()| {
            let identity = std::fs::symlink_metadata(path).map_err(notify::Error::io)?;
            if !identity.is_dir() {
                return Err(notify::Error::generic(
                    "watched directory changed during registration",
                ));
            }
            registered.insert(path.clone(), (mode, identity.dev(), identity.ino()));
            Ok(())
        });
        match result {
            Ok(()) => {
                if let Some(ids) = owners.get(path) {
                    rearmed.extend(ids);
                }
            }
            Err(error) => {
                // Recursive watch can fail after installing a partial subtree.
                // Remove that subtree before a later retry, not unrelated roots.
                // Also retire a watch whose post-watch identity is unavailable.
                let _ = watcher.unwatch(path);
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
            directory_inventory: false,
            worktrees: vec![
                Worktree {
                    id: WorktreeId::new(),
                    root: first,
                    git_dir: common.clone(),
                    census: census.clone(),
                    directories: Arc::new(PathSet::new([]).unwrap()),
                    branch: Some("refs/heads/main".into()),
                    upstream: Some("origin/main".into()),
                    polling: None,
                },
                Worktree {
                    id: WorktreeId::new(),
                    root: linked,
                    git_dir: common.join("worktrees/linked"),
                    census,
                    directories: Arc::new(PathSet::new([]).unwrap()),
                    branch: Some("refs/heads/linked".into()),
                    upstream: Some("origin/linked".into()),
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
        let id = ProjectId::new();
        let read = |previous: Option<&Project>, changed: &[WorktreeId]| {
            Project::read_changed(
                id,
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
        assert!(submodule.polling_reason().is_some());
        assert!(worktree(&submodule, &linked).polling.is_none());
    }

    fn modified(path: PathBuf) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any))).add_path(path)
    }

    fn materialize(project: &Project) {
        for path in project.metadata_paths().into_keys() {
            std::fs::create_dir_all(path).unwrap();
        }
        for row in &project.worktrees {
            for relative in row.directories() {
                std::fs::create_dir_all(row.absolute(relative)).unwrap();
            }
        }
    }

    #[test]
    fn ui_index_edges_replan_only_uncovered_status_while_host_keeps_index_census() {
        let host = project(Path::new("/fixture"));
        let mut ui = host.clone();
        ui.directory_inventory = true;
        let row = &host.worktrees[0];
        let record = WorktreeRecord {
            id: row.id,
            path: row.root.clone(),
            created_at_unix_ms: None,
            head_oid: None,
            branch: row.branch.clone(),
            detached: false,
            bare: false,
            lock: Default::default(),
            prunable_reason: None,
        };
        for (plan, index, head) in [(&host, true, false), (&ui, false, true)] {
            let change = plan.change(&modified(row.git_dir.join("index"))).unwrap();
            assert_eq!(change.membership, index);
            assert!(change.metadata && change.untracked);
            assert_eq!(
                plan.change(&modified(row.git_dir.join("HEAD")))
                    .unwrap()
                    .membership,
                head
            );
        }
        let mut status = crate::repository::StatusSnapshot::default();
        let entry = |path: &str, submodule| crate::repository::ChangedPath {
            path: crate::repository::RepoPath::parse(path).unwrap(),
            old_path: None,
            area: crate::repository::ChangeArea::Staged,
            kind: crate::repository::ChangeKind::Added,
            conflict: None,
            submodule,
            added_lines: None,
            removed_lines: None,
        };
        for path in ["src/main.rs", "src/new.txt"] {
            status.entries = vec![entry(path, false)];
            assert!(!ui.status_needs_replan(&record, &status), "{path}");
        }
        status.entries = vec![entry("ignored/deep/force-added.txt", false)];
        assert!(ui.status_needs_replan(&record, &status));
        let mut repaired = ui.clone();
        repaired.worktrees[0].census = census(&["src/main.rs", "ignored/deep/force-added.txt"]);
        assert!(!repaired.status_needs_replan(&record, &status));
        assert!(
            repaired
                .change(&modified(row.root.join("ignored/deep/force-added.txt")))
                .unwrap()
                .worktrees
                .contains(&row.id)
        );
        status.entries = vec![entry("src/new-module", true)];
        assert!(ui.status_needs_replan(&record, &status));
        let mut foreign = record.clone();
        foreign.path = PathBuf::from("/foreign");
        assert!(ui.status_needs_replan(&foreign, &status));
        foreign = record.clone();
        foreign.id = WorktreeId::new();
        assert!(ui.status_needs_replan(&foreign, &status));
    }

    #[test]
    fn capped_index_status_repairs_covered_rows_but_keeps_explicit_polling_cheap() {
        let mut plan = project(Path::new("/fixture"));
        plan.directory_inventory = true;
        let row = &plan.worktrees[0];
        let record = WorktreeRecord {
            id: row.id,
            path: row.root.clone(),
            created_at_unix_ms: None,
            head_oid: None,
            branch: row.branch.clone(),
            detached: false,
            bare: false,
            lock: Default::default(),
            prunable_reason: None,
        };
        // Forty thousand dirty rows can be ordinary staging in an already
        // observed directory. The bounded sidebar projection is not complete.
        let mut status = crate::repository::StatusSnapshot {
            listing_truncated: true,
            entries: (0..40_000)
                .map(|i| crate::repository::ChangedPath {
                    path: crate::repository::RepoPath::parse(format!("src/file{i}.rs")).unwrap(),
                    old_path: None,
                    area: crate::repository::ChangeArea::Staged,
                    kind: crate::repository::ChangeKind::Modified,
                    conflict: None,
                    submodule: false,
                    added_lines: None,
                    removed_lines: None,
                })
                .collect(),
            ..Default::default()
        };
        assert!(
            plan.status_needs_replan(&record, &status),
            "previously covered rows conservatively repair uncertain status"
        );
        plan.worktrees[0].polling = Some("directory watch limit reached".into());
        let before = plan.clone();
        assert!(
            !plan.status_needs_replan(&record, &status),
            "ordinary staging must not rebuild an already capped inventory"
        );
        assert_eq!(
            plan, before,
            "predicate neither clears fallback nor grants full coverage"
        );
        // Any actually observed missing route or newly introduced submodule
        // still repairs, even if the rest of that status was capped.
        status.entries[0].path =
            crate::repository::RepoPath::parse("ignored/deep/force-added.rs").unwrap();
        assert!(plan.status_needs_replan(&record, &status));
        status.entries[0].path = crate::repository::RepoPath::parse("src/new-module").unwrap();
        status.entries[0].submodule = true;
        assert!(plan.status_needs_replan(&record, &status));
        status.entries.clear();
        assert!(!plan.status_needs_replan(&record, &status));
        plan.worktrees[0].polling = None;
        assert!(
            plan.status_needs_replan(&record, &status),
            "even an empty truncated prefix cannot establish coverage"
        );
        status.listing_truncated = false;
        assert!(!plan.status_needs_replan(&record, &status));
    }

    #[test]
    fn scoped_refs_keep_host_inventory_wakes_and_stash_is_shared_only() {
        let plan = project(Path::new("/fixture"));
        let first = plan.worktrees[0].id;
        let linked = plan.worktrees[1].id;
        for (reference, rows) in [
            ("refs/heads/main", BTreeSet::from([first])),
            ("refs/remotes/origin/linked", BTreeSet::from([linked])),
            ("refs/heads/not-checked-out", BTreeSet::new()),
        ] {
            let change = plan
                .change(&modified(plan.common_dir.join(reference)))
                .unwrap();
            assert!(change.references && change.metadata);
            assert_eq!(change.worktrees, rows, "{reference}");
            assert!(!change.membership && !change.untracked && !change.shared_snapshot);
        }
        for reference in ["refs/stash", "logs/refs/stash"] {
            let change = plan
                .change(&modified(plan.common_dir.join(reference)))
                .unwrap();
            assert!(change.references && change.metadata && change.shared_snapshot);
            assert!(change.worktrees.is_empty());
        }
        assert!(
            plan.change(&modified(plan.common_dir.join("logs/refs/stash.lock")))
                .is_none()
        );
        let changed = plan
            .change(&modified(plan.worktrees[1].git_dir.join("config.worktree")))
            .unwrap();
        assert_eq!(changed.worktrees, BTreeSet::from([linked]));
        assert!(changed.metadata && changed.membership && changed.untracked);
        let ignore = plan
            .change(&modified(plan.worktrees[0].root.join(".gitignore")))
            .unwrap();
        assert!(ignore.membership && ignore.untracked);
        let info = plan
            .change(
                &Event::new(EventKind::Create(notify::event::CreateKind::Folder))
                    .add_path(plan.common_dir.join("info")),
            )
            .unwrap();
        assert!(info.metadata && info.membership && info.untracked);
        assert!(!info.references);
        assert_eq!(
            info.worktrees,
            plan.worktrees.iter().map(|row| row.id).collect()
        );
    }

    #[test]
    fn directory_discovery_is_bounded_and_excludes_symlinks_ignored_and_nested_checkouts() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for relative in [
            "empty/nested",
            "ignored/deep",
            "nested/.git",
            "outside/deep",
        ] {
            std::fs::create_dir_all(root.join(relative)).unwrap();
        }
        std::os::unix::fs::symlink(root.join("outside"), root.join("link")).unwrap();
        let ignored = [crate::repository::RepoPath::parse("ignored").unwrap()];
        let (dirs, incomplete) =
            content_directories(root, &ignored, &CancellationToken::default(), 128).unwrap();
        assert!(!incomplete);
        assert!(dirs.contains(Path::new("empty/nested")));
        for relative in [
            "ignored",
            "ignored/deep",
            "link",
            "link/deep",
            "nested",
            "nested/.git",
        ] {
            assert!(!dirs.contains(Path::new(relative)), "{relative}");
        }
        let flat = tempfile::tempdir().unwrap();
        for i in 0..64 {
            std::fs::write(flat.path().join(format!("file{i}")), "").unwrap();
        }
        let (dirs, incomplete) =
            content_directories(flat.path(), &[], &CancellationToken::default(), 8).unwrap();
        assert!(
            incomplete,
            "flat files must consume the discovery work budget too"
        );
        assert_eq!(dirs, BTreeSet::from([PathBuf::new()]));
    }

    #[test]
    fn observed_directories_merge_with_shared_census_without_expanding_it() {
        let mut plan = project(Path::new("/fixture"));
        plan.worktrees[0].directories = Arc::new(
            PathSet::new([
                b"empty".as_slice(),
                b"empty/nested",
                b"src",
                b"src-a",
                b"src/a",
            ])
            .unwrap(),
        );
        assert!(Arc::ptr_eq(
            &plan.worktrees[0].census,
            &plan.worktrees[1].census
        ));
        assert_eq!(
            plan.worktrees[0].directories().collect::<Vec<_>>(),
            ["", "empty", "empty/nested", "src", "src-a", "src/a"].map(Path::new)
        );
        let change = plan
            .change(
                &Event::new(EventKind::Create(notify::event::CreateKind::File))
                    .add_path(plan.worktrees[0].root.join("empty/nested/first")),
            )
            .unwrap();
        assert_eq!(change.worktrees, BTreeSet::from([plan.worktrees[0].id]));
        assert!(change.untracked && !change.membership);
        assert_eq!(
            plan.worktrees[0].census.files.iter().collect::<Vec<_>>(),
            [Path::new("src/main.rs")]
        );
    }

    #[test]
    fn reconciled_coverage_retains_polling_and_routes_current_branch_and_upstream() {
        let directory = tempfile::tempdir().unwrap();
        let mut previous = project(directory.path());
        let linked = previous.worktrees[1].id;
        previous.worktrees[1].polling = Some("directory watch limit reached".into());
        let records = previous
            .worktrees
            .iter()
            .map(|worktree| WorktreeRecord {
                id: worktree.id,
                path: worktree.root.clone(),
                created_at_unix_ms: None,
                head_oid: None,
                branch: Some(
                    if worktree.id == linked {
                        "refs/heads/recovered"
                    } else {
                        "refs/heads/main"
                    }
                    .into(),
                ),
                detached: false,
                bare: false,
                lock: Default::default(),
                prunable_reason: None,
            })
            .collect::<Vec<_>>();
        materialize(&previous);
        let mut incoming = previous.clone();
        incoming.worktrees.retain(|worktree| worktree.id != linked);
        let preserved = BTreeSet::from([linked]);
        let upstreams = BTreeMap::from([(linked, "origin/recovered".to_owned())]);
        assert!(
            incoming
                .reconcile_coverage(Some(&previous), &records, &preserved, &upstreams)
                .is_empty()
        );
        let registrations = Registrations::within(
            &BTreeMap::from([(incoming.id, Arc::new(incoming.clone()))]),
            MAX_DIRECTORIES,
        );
        assert_eq!(
            registrations.failures.get(&incoming.id).map(String::as_str),
            Some("directory watch limit reached"),
            "restored incomplete census must retain its periodic repair"
        );
        for name in ["refs/heads/recovered", "refs/remotes/origin/recovered"] {
            assert_eq!(
                incoming
                    .change(&modified(incoming.common_dir.join(name)))
                    .unwrap()
                    .worktrees,
                preserved
            );
        }
        for name in ["refs/heads/linked", "refs/remotes/origin/linked"] {
            assert!(
                incoming
                    .change(&modified(incoming.common_dir.join(name)))
                    .unwrap()
                    .worktrees
                    .is_empty()
            );
        }
        let remaining = records.iter().filter(|record| record.id != linked);
        assert!(
            incoming
                .reconcile_coverage(Some(&previous), remaining, &preserved, &BTreeMap::new())
                .is_empty()
        );
        let registrations = Registrations::within(
            &BTreeMap::from([(incoming.id, Arc::new(incoming.clone()))]),
            MAX_DIRECTORIES,
        );
        assert!(
            !registrations.failures.contains_key(&incoming.id),
            "removed sole polling row cannot leave a stale fallback"
        );
        incoming.polling = Some("network filesystem".into());
        let registrations = Registrations::within(
            &BTreeMap::from([(incoming.id, Arc::new(incoming.clone()))]),
            MAX_DIRECTORIES,
        );
        assert_eq!(
            registrations.failures.get(&incoming.id).map(String::as_str),
            Some("network filesystem"),
            "project-wide fallback remains independently authoritative"
        );
    }

    #[test]
    fn watch_budget_prioritizes_active_project_and_selected_worktree() {
        let directory = tempfile::tempdir().unwrap();
        let active = Arc::new(project(&directory.path().join("active")));
        let other = Arc::new(project(&directory.path().join("other")));
        for project in [&active, &other] {
            materialize(project);
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
            materialize(project);
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
            materialize(project);
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
            materialize(project);
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
    fn native_watcher_observes_first_file_in_initial_and_later_empty_directories() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "-q"]);
        std::fs::create_dir_all(root.join("empty/nested")).unwrap();
        std::fs::create_dir_all(root.join("target/deep/ignored")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        let service = RepositoryService::system("git");
        let token = CancellationToken::default();
        let records = service.list_worktrees(&root, &token).unwrap();
        let common = service.common_dir(&root, &token).unwrap();
        let id = ProjectId::new();
        let changed = records.iter().map(|record| record.id).collect();
        let read = |previous| {
            Project::read_changed_with_upstreams(
                id,
                common.clone(),
                &records,
                &token,
                previous,
                &changed,
                &BTreeMap::new(),
            )
            .unwrap()
        };
        let plan = read(None);
        let same = read(Some(&plan));
        assert!(Arc::ptr_eq(
            &same.worktrees[0].directories,
            &plan.worktrees[0].directories
        ));
        assert_eq!(
            plan.worktrees[0].tracks(&root.join("empty/nested")),
            Some(Tracked::Directory)
        );
        assert_eq!(plan.worktrees[0].tracks(&root.join("target")), None);
        let (wake, woken) = mpsc::sync_channel(1);
        let monitor = (
            Monitor::start(move || {
                let _ = wake.try_send(());
            })
            .unwrap(),
            woken,
        );
        monitor.0.configure(vec![Arc::new(plan.clone())]);
        wait_for(&monitor, id, |change| change.coverage == Some(true));
        std::fs::write(root.join("empty/nested/first.txt"), "first").unwrap();
        wait_for(&monitor, id, |change| {
            change.untracked && change.worktrees.contains(&records[0].id)
        });
        std::fs::create_dir(root.join("later")).unwrap();
        wait_for(&monitor, id, |change| change.membership);
        let next = read(Some(&plan));
        assert_eq!(
            next.worktrees[0].tracks(&root.join("later")),
            Some(Tracked::Directory)
        );
        monitor.0.configure(vec![Arc::new(next)]);
        wait_for(&monitor, id, |change| change.coverage == Some(true));
        std::fs::write(root.join("later/first.txt"), "second").unwrap();
        wait_for(&monitor, id, |change| {
            change.untracked && change.worktrees.contains(&records[0].id)
        });
    }

    #[test]
    fn incomplete_inventory_and_unreadable_index_keep_root_and_admin_coverage() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(root.join("first.txt"), "first").unwrap();
        std::fs::write(root.join("second.txt"), "second").unwrap();
        std::fs::create_dir_all(root.join("empty/deep")).unwrap();
        let service = RepositoryService::system("git");
        let token = CancellationToken::default();
        let records = service.list_worktrees(&root, &token).unwrap();
        let common = service.common_dir(&root, &token).unwrap();
        let id = ProjectId::new();
        let changed = records.iter().map(|record| record.id).collect();
        let plan = Project::read_changed_with_inventory_limit(
            id,
            common.clone(),
            &records,
            &token,
            None,
            &changed,
            &BTreeMap::new(),
            1,
        )
        .unwrap();
        assert_eq!(
            plan.worktrees[0].tracks(&root.join("empty/deep")),
            Some(Tracked::Directory),
            "bounded listing and directory budgets are independent"
        );
        assert!(plan.polling_reason().is_some());
        std::fs::write(common.join("index"), "not an index").unwrap();
        let failed = Project::read_changed_with_upstreams(
            id,
            common.clone(),
            &records,
            &token,
            Some(&plan),
            &changed,
            &BTreeMap::new(),
        )
        .unwrap();
        let registrations = Registrations::within(
            &BTreeMap::from([(id, Arc::new(failed.clone()))]),
            MAX_DIRECTORIES,
        );
        assert!(registrations.desired.contains_key(&root));
        assert!(registrations.desired.contains_key(&common));
        assert!(registrations.failures.contains_key(&id));
        assert!(Arc::ptr_eq(
            &failed.worktrees[0].census,
            &plan.worktrees[0].census
        ));
    }

    #[test]
    fn failed_row_planning_retains_healthy_sibling_census_and_current_routing() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let root = directory.join("first");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "first").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        git(&root, &["worktree", "add", "-q", "../linked"]);
        let service = RepositoryService::system("git");
        let token = CancellationToken::default();
        let records = service.list_worktrees(&root, &token).unwrap();
        let common = service.common_dir(&root, &token).unwrap();
        let id = ProjectId::new();
        let changed = records.iter().map(|record| record.id).collect();
        let before = Project::read_changed_with_upstreams(
            id,
            common.clone(),
            &records,
            &token,
            None,
            &changed,
            &BTreeMap::new(),
        )
        .unwrap();
        let linked = before
            .worktrees
            .iter()
            .find(|row| row.root != root)
            .unwrap();
        std::fs::write(linked.root.join(".git"), "invalid private gitfile\n").unwrap();
        let broken_record = records
            .iter()
            .find(|record| record.id == linked.id)
            .unwrap();
        let mut new_records = records.clone();
        let refreshed = new_records
            .iter_mut()
            .find(|record| record.id == broken_record.id)
            .unwrap();
        refreshed.branch = Some("refs/heads/recovered".into());
        let upstreams = BTreeMap::from([(linked.id, "origin/recovered".to_owned())]);
        let after = Project::read_changed_with_upstreams(
            id,
            common,
            &new_records,
            &token,
            Some(&before),
            &changed,
            &upstreams,
        )
        .unwrap();
        assert_eq!(after.worktrees.len(), 2);
        for row in &after.worktrees {
            let previous = before
                .worktrees
                .iter()
                .find(|previous| previous.id == row.id)
                .unwrap();
            assert!(Arc::ptr_eq(&row.census, &previous.census));
            assert!(Arc::ptr_eq(&row.directories, &previous.directories));
            assert_eq!(row.polling.is_some(), row.id == linked.id);
        }
        assert_eq!(
            after
                .change(&modified(
                    after.common_dir.join("refs/remotes/origin/recovered")
                ))
                .unwrap()
                .worktrees,
            BTreeSet::from([linked.id])
        );
        assert!(after.matches(&after.common_dir, &new_records));
        // Pure metadata repair keeps the compact index identity and catches
        // missed events without asking another worker to rescan directories.
        let mut latest = new_records[0].clone();
        latest.branch = Some("refs/heads/latest".into());
        let repaired = after
            .refresh_worktree_routing(&latest, Some("origin/latest"))
            .unwrap();
        let row = repaired
            .worktrees
            .iter()
            .find(|row| row.id == latest.id)
            .unwrap();
        let previous = after
            .worktrees
            .iter()
            .find(|row| row.id == latest.id)
            .unwrap();
        assert!(Arc::ptr_eq(&row.census, &previous.census));
        assert!(
            repaired
                .refresh_worktree_routing(&latest, Some("origin/latest"))
                .is_none()
        );
        let mut moved = latest.clone();
        moved.path = directory.join("foreign");
        assert!(
            repaired
                .refresh_routing([&moved], &BTreeMap::new())
                .is_none()
        );
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
        assert_eq!(
            change.worktrees,
            BTreeSet::from([first.id]),
            "only the bound branch's status changes"
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
                for path in project.metadata_paths().into_keys() {
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
        assert_eq!(change.worktrees, BTreeSet::from([project.worktrees[0].id]));
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
            assert_eq!(change.membership, path == "config.worktree", "{path}");
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
            assert_eq!(change.membership, path == "config", "{path}");
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
        let mut second = first.clone();
        second.id = ProjectId::new();
        let ids = BTreeSet::from([first.id, second.id]);
        for path in first.metadata_paths().into_keys() {
            std::fs::create_dir_all(path).unwrap();
        }
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

    fn wait_for(
        (monitor, woken): &(Monitor, mpsc::Receiver<()>),
        project: ProjectId,
        predicate: impl Fn(&Change) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if woken.try_recv().is_ok() && monitor.drain().get(&project).is_some_and(&predicate) {
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
        let (wake, woken) = mpsc::sync_channel(1);
        let monitor = (
            Monitor::start(move || {
                let _ = wake.try_send(());
            })
            .unwrap(),
            woken,
        );
        monitor.0.configure(vec![Arc::new(project.clone())]);
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
        monitor.0.configure(vec![Arc::new(project.clone())]);
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

    /// An injected backend for the real registration transition. Each root
    /// consumes one slot here; recursive native mark counts are not simulated.
    #[derive(Default)]
    struct LimitedWatcher {
        capacity: usize,
        active: BTreeMap<PathBuf, RecursiveMode>,
        attempts: Vec<PathBuf>,
        removals: Vec<PathBuf>,
        fail: BTreeSet<PathBuf>,
        remove_after_watch: Option<PathBuf>,
    }

    impl Watcher for LimitedWatcher {
        fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
            Ok(Self::default())
        }

        fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
            self.attempts.push(path.to_owned());
            if self.fail.contains(path) {
                // Like a failed recursive walk, the root can already be
                // installed when a later descendant exceeds the quota.
                self.active.insert(path.to_owned(), mode);
                return Err(
                    notify::Error::new(notify::ErrorKind::MaxFilesWatch).add_path(path.join("new"))
                );
            }
            if !self.active.contains_key(path) && self.active.len() >= self.capacity {
                return Err(
                    notify::Error::new(notify::ErrorKind::MaxFilesWatch).add_path(path.to_owned())
                );
            }
            self.active.insert(path.to_owned(), mode);
            if self.remove_after_watch.as_deref() == Some(path) {
                std::fs::remove_dir(path).unwrap();
            }
            Ok(())
        }

        fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
            self.removals.push(path.to_owned());
            let Some(mode) = self.active.remove(path) else {
                return Err(notify::Error::watch_not_found().add_path(path.to_owned()));
            };
            if mode == RecursiveMode::Recursive {
                self.active.retain(|child, _| !child.starts_with(path));
            }
            Ok(())
        }

        fn kind() -> notify::WatcherKind {
            notify::WatcherKind::NullWatcher
        }
    }

    fn registered_transition(
        pending: &mut Pending,
        watcher: &mut LimitedWatcher,
        registered: &mut BTreeMap<PathBuf, (RecursiveMode, u64, u64)>,
        desired: &BTreeMap<PathBuf, RecursiveMode>,
        metadata: &BTreeSet<PathBuf>,
        owners: &BTreeMap<PathBuf, BTreeSet<ProjectId>>,
    ) -> BTreeMap<ProjectId, String> {
        let rearm = pending.rearm(desired, owners);
        let mut failures = BTreeMap::new();
        reconcile(
            watcher,
            registered,
            desired,
            metadata,
            owners,
            &rearm,
            &mut failures,
        );
        pending.retain_failures(&mut failures);
        failures
    }

    #[test]
    fn native_registration_attempts_all_metadata_before_lexically_earlier_content() {
        let directory = tempfile::tempdir().unwrap();
        let projects = ["a", "z"]
            .map(|name| {
                let mut project = project(&directory.path().join(name));
                project.worktrees.truncate(1);
                std::fs::create_dir_all(project.worktrees[0].root.join("src")).unwrap();
                std::fs::create_dir_all(project.common_dir.join("refs")).unwrap();
                std::fs::create_dir_all(project.common_dir.join("worktrees")).unwrap();
                (project.id, Arc::new(project))
            })
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let plan = Registrations::within(&projects, MAX_DIRECTORIES);
        let a_content = directory.path().join("a/first/src");
        let z_metadata = directory.path().join("z/first/.git");
        assert!(
            a_content < z_metadata,
            "the map alone would admit A content first"
        );
        assert!(plan.metadata.contains(&z_metadata));
        assert!(plan.desired.contains_key(&a_content));
        let mut watcher = LimitedWatcher {
            capacity: plan.metadata.len(),
            ..Default::default()
        };
        let mut registered = BTreeMap::new();
        let mut failures = BTreeMap::new();
        reconcile(
            &mut watcher,
            &mut registered,
            &plan.desired,
            &plan.metadata,
            &plan.owners,
            &Rearm::default(),
            &mut failures,
        );
        assert!(
            watcher.attempts[..plan.metadata.len()]
                .iter()
                .all(|p| plan.metadata.contains(p))
        );
        assert!(plan.metadata.iter().all(|p| registered.contains_key(p)));
        assert_eq!(
            watcher.active.keys().cloned().collect::<BTreeSet<_>>(),
            plan.metadata
        );
        assert!(!registered.contains_key(&a_content));
        assert_eq!(
            failures.len(),
            projects.len(),
            "content owners retain polling fallback"
        );
        assert!(
            watcher.removals.iter().all(|p| !plan.metadata.contains(p)),
            "a refused content watch must not remove healthy metadata"
        );
    }

    #[test]
    fn new_metadata_can_preempt_retained_content_only_after_a_native_quota_refusal() {
        let directory = tempfile::tempdir().unwrap();
        let content = directory.path().join("a-content");
        let old_meta = directory.path().join("z-metadata");
        let new_meta = directory.path().join("y-new-metadata");
        for p in [&content, &old_meta, &new_meta] {
            std::fs::create_dir(p).unwrap();
        }
        let id = ProjectId::new();
        let mut desired = BTreeMap::from([
            (content.clone(), RecursiveMode::NonRecursive),
            (old_meta.clone(), RecursiveMode::NonRecursive),
        ]);
        let mut metadata = BTreeSet::from([old_meta.clone()]);
        let mut owners = desired
            .keys()
            .map(|p| (p.clone(), BTreeSet::from([id])))
            .collect::<BTreeMap<_, _>>();
        let mut watcher = LimitedWatcher {
            capacity: 2,
            ..Default::default()
        };
        let mut registered = BTreeMap::new();
        let mut pending = Pending::default();
        assert!(
            registered_transition(
                &mut pending,
                &mut watcher,
                &mut registered,
                &desired,
                &metadata,
                &owners
            )
            .is_empty()
        );
        assert!(watcher.removals.is_empty());
        desired.insert(new_meta.clone(), RecursiveMode::NonRecursive);
        metadata.insert(new_meta.clone());
        owners.insert(new_meta.clone(), BTreeSet::from([id]));
        watcher.attempts.clear();
        let failures = registered_transition(
            &mut pending,
            &mut watcher,
            &mut registered,
            &desired,
            &metadata,
            &owners,
        );
        assert_eq!(
            watcher.attempts[..2],
            [new_meta.clone(), new_meta.clone()],
            "metadata first refuses, then succeeds after content yields capacity"
        );
        assert!(registered.contains_key(&old_meta) && registered.contains_key(&new_meta));
        assert!(!registered.contains_key(&content));
        assert!(
            failures.contains_key(&id),
            "failed content re-admission stays degraded"
        );
        assert!(watcher.removals.contains(&content));
        assert!(!watcher.removals.contains(&old_meta));
        watcher.capacity = 3;
        assert!(
            registered_transition(
                &mut pending,
                &mut watcher,
                &mut registered,
                &desired,
                &metadata,
                &owners
            )
            .is_empty()
        );
        assert!(
            registered.contains_key(&content),
            "later capacity restores content explicitly"
        );
    }

    #[test]
    fn recursive_quota_rearm_preserves_unrelated_watches_and_newer_faults() {
        let directory = tempfile::tempdir().unwrap();
        let project = Arc::new(project(directory.path()));
        let id = project.id;
        let common = project.common_dir.clone();
        let refs = common.join("refs");
        let content = directory.path().join("unrelated-content");
        let other_metadata = directory.path().join("another/.git");
        for path in [&common, &refs, &content, &other_metadata] {
            std::fs::create_dir_all(path).unwrap();
        }
        let desired = BTreeMap::from([
            (common.clone(), RecursiveMode::NonRecursive),
            (refs.clone(), RecursiveMode::Recursive),
            (other_metadata.clone(), RecursiveMode::NonRecursive),
            (content.clone(), RecursiveMode::NonRecursive),
        ]);
        let metadata = BTreeSet::from([common.clone(), refs.clone(), other_metadata.clone()]);
        let owners = desired
            .keys()
            .map(|p| (p.clone(), BTreeSet::from([id])))
            .collect::<BTreeMap<_, _>>();
        let mut pending = Pending::default();
        pending.projects.insert(id, project);
        let mut watcher = LimitedWatcher {
            capacity: 4,
            ..Default::default()
        };
        let mut registered = BTreeMap::new();
        assert!(
            registered_transition(
                &mut pending,
                &mut watcher,
                &mut registered,
                &desired,
                &metadata,
                &owners
            )
            .is_empty()
        );
        let quota = || {
            notify::Error::new(notify::ErrorKind::MaxFilesWatch).add_path(refs.join("new/deeper"))
        };
        for _ in 0..10_000 {
            pending.event(Err(quota()));
        }
        assert_eq!(pending.quota_rearm, BTreeSet::from([refs.clone()]));
        assert!(!pending.faulted);
        assert_eq!(pending.changes[&id].coverage, Some(false));
        watcher.fail.insert(refs.clone());
        watcher.removals.clear();
        let failures = registered_transition(
            &mut pending,
            &mut watcher,
            &mut registered,
            &desired,
            &metadata,
            &owners,
        );
        assert!(failures.contains_key(&id));
        assert!(
            !registered.contains_key(&refs) && !watcher.active.contains_key(&refs),
            "the backend's partial recursive install was removed for a clean retry"
        );
        assert!(registered.contains_key(&common) && registered.contains_key(&content));
        assert!(registered.contains_key(&other_metadata));
        assert!(
            !watcher.removals.contains(&common) && !watcher.removals.contains(&other_metadata),
            "quota repair never tears down unrelated healthy metadata"
        );
        // Metadata quota refusal may yield content capacity, but content must
        // actually be admitted again before reporting it healthy.
        assert!(watcher.active.contains_key(&content));
        watcher.fail.clear();
        let rearm = pending.rearm(&desired, &owners);
        let mut failures = BTreeMap::new();
        reconcile(
            &mut watcher,
            &mut registered,
            &desired,
            &metadata,
            &owners,
            &rearm,
            &mut failures,
        );
        assert!(failures.is_empty() && registered.contains_key(&refs));
        // An async expansion error can arrive after the native call but before
        // the coordinator publishes coverage. It must defeat that success.
        pending.event(Err(quota()));
        pending.retain_failures(&mut failures);
        assert!(failures.contains_key(&id));
        watcher.attempts.clear();
        assert!(
            registered_transition(
                &mut pending,
                &mut watcher,
                &mut registered,
                &desired,
                &metadata,
                &owners
            )
            .is_empty()
        );
        assert_eq!(watcher.attempts, [refs.clone()]);
        pending.event(Err(quota()));
        let reduced = BTreeMap::from([
            (common.clone(), RecursiveMode::NonRecursive),
            (other_metadata.clone(), RecursiveMode::NonRecursive),
            (content.clone(), RecursiveMode::NonRecursive),
        ]);
        assert!(
            registered_transition(
                &mut pending,
                &mut watcher,
                &mut registered,
                &reduced,
                &metadata,
                &owners
            )
            .is_empty()
        );
        assert!(pending.quota_rearm.is_empty() && !pending.recursive.contains_key(&refs));
        assert!(
            !registered.contains_key(&refs),
            "removed topology retires its fault and watch"
        );
        assert!(registered.contains_key(&common) && registered.contains_key(&content));
    }

    #[test]
    fn unknown_quota_error_keeps_coverage_degraded_without_discarding_healthy_watches() {
        let directory = tempfile::tempdir().unwrap();
        let project = Arc::new(project(directory.path()));
        let id = project.id;
        let path = project.common_dir.clone();
        std::fs::create_dir_all(&path).unwrap();
        let desired = BTreeMap::from([(path.clone(), RecursiveMode::NonRecursive)]);
        let metadata = BTreeSet::from([path.clone()]);
        let owners = BTreeMap::from([(path.clone(), BTreeSet::from([id]))]);
        let mut pending = Pending::default();
        pending.projects.insert(id, project.clone());
        let mut registered = BTreeMap::new();
        let mut watcher = LimitedWatcher {
            capacity: 1,
            ..Default::default()
        };
        assert!(
            registered_transition(
                &mut pending,
                &mut watcher,
                &mut registered,
                &desired,
                &metadata,
                &owners
            )
            .is_empty()
        );
        for error in [
            notify::Error::new(notify::ErrorKind::MaxFilesWatch),
            notify::Error::new(notify::ErrorKind::MaxFilesWatch)
                .add_path(directory.path().join("unknown")),
        ] {
            pending.event(Err(error));
            assert!(
                registered_transition(
                    &mut pending,
                    &mut watcher,
                    &mut registered,
                    &desired,
                    &metadata,
                    &owners
                )
                .contains_key(&id)
            );
            assert!(watcher.removals.is_empty() && registered.contains_key(&path));
        }
        assert_eq!(pending.quota_unknown, BTreeSet::from([id]));
        for _ in 0..3 {
            assert!(
                registered_transition(
                    &mut pending,
                    &mut watcher,
                    &mut registered,
                    &desired,
                    &metadata,
                    &owners
                )
                .contains_key(&id),
                "cached success alone cannot clear unknown quota coverage"
            );
        }
        assert!(watcher.removals.is_empty());
        pending.projects.clear();
        assert!(
            registered_transition(
                &mut pending,
                &mut watcher,
                &mut registered,
                &BTreeMap::new(),
                &BTreeSet::new(),
                &BTreeMap::new()
            )
            .is_empty()
        );
        assert!(pending.quota_unknown.is_empty() && registered.is_empty());
        pending.projects.insert(id, project);
        assert!(
            registered_transition(
                &mut pending,
                &mut watcher,
                &mut registered,
                &desired,
                &metadata,
                &owners
            )
            .is_empty(),
            "closing the owner retires its unknown fault"
        );
    }

    #[test]
    fn generic_faults_still_rearm_all_and_post_watch_identity_failure_is_not_coverage() {
        let directory = tempfile::tempdir().unwrap();
        let project = Arc::new(project(directory.path()));
        let id = project.id;
        let common = project.common_dir.clone();
        let content = directory.path().join("content");
        for path in [&common, &content] {
            std::fs::create_dir_all(path).unwrap();
        }
        let desired = BTreeMap::from([
            (common.clone(), RecursiveMode::NonRecursive),
            (content.clone(), RecursiveMode::NonRecursive),
        ]);
        let metadata = BTreeSet::from([common.clone()]);
        let owners = desired
            .keys()
            .map(|p| (p.clone(), BTreeSet::from([id])))
            .collect::<BTreeMap<_, _>>();
        let mut pending = Pending::default();
        pending.projects.insert(id, project);
        let mut registered = BTreeMap::new();
        let mut watcher = LimitedWatcher {
            capacity: 2,
            ..Default::default()
        };
        assert!(
            registered_transition(
                &mut pending,
                &mut watcher,
                &mut registered,
                &desired,
                &metadata,
                &owners
            )
            .is_empty()
        );
        pending.event(Err(notify::Error::io(std::io::Error::other(
            "backend failure",
        ))));
        watcher.removals.clear();
        watcher.attempts.clear();
        assert!(
            registered_transition(
                &mut pending,
                &mut watcher,
                &mut registered,
                &desired,
                &metadata,
                &owners
            )
            .is_empty()
        );
        assert_eq!(
            watcher.removals.iter().cloned().collect::<BTreeSet<_>>(),
            desired.keys().cloned().collect::<BTreeSet<_>>()
        );
        assert_eq!(watcher.attempts.len(), 2);
        pending.event(Err(notify::Error::io(std::io::Error::other(
            "backend failure",
        ))));
        watcher.remove_after_watch = Some(content.clone());
        let failures = registered_transition(
            &mut pending,
            &mut watcher,
            &mut registered,
            &desired,
            &metadata,
            &owners,
        );
        assert!(failures.contains_key(&id));
        assert!(!registered.contains_key(&content) && !watcher.active.contains_key(&content));
        assert!(registered.contains_key(&common));
        std::fs::create_dir(&content).unwrap();
        watcher.remove_after_watch = None;
        assert!(
            registered_transition(
                &mut pending,
                &mut watcher,
                &mut registered,
                &desired,
                &metadata,
                &owners
            )
            .is_empty()
        );
        assert!(registered.contains_key(&content));
    }
}
