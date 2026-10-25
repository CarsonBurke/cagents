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
    fn new<'a>(files: impl IntoIterator<Item = &'a [u8]>, submodules: bool) -> Result<Self, String> {
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
    polling: Option<String>,
}

impl Worktree {
    /// Whether `path` is a tracked file or a directory on the way to one.
    fn tracks(&self, path: &Path) -> Option<Tracked> {
        let relative = path.strip_prefix(&self.root).ok()?;
        if self.census.directories.contains(relative) {
            Some(Tracked::Directory)
        } else {
            self.census.files.contains(relative).then_some(Tracked::File)
        }
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
        Self::read_changed_with_failed_status(
            id,
            common_dir,
            records,
            cancellation,
            previous,
            changed,
            &BTreeSet::new(),
        )
    }

    /// Preserve only known failed rows' last watcher census; their status
    /// error remains authoritative. Healthy siblings still rebuild normally.
    pub(crate) fn read_changed_with_failed_status(
        id: ProjectId,
        common_dir: PathBuf,
        records: &[WorktreeRecord],
        cancellation: &CancellationToken,
        previous: Option<&Self>,
        changed: &BTreeSet<WorktreeId>,
        failed: &BTreeSet<WorktreeId>,
    ) -> Result<Self, String> {
        let git = RepositoryService::system("git");
        let mut worktrees = Vec::new();
        let polling = network_filesystem(&common_dir)
            .map_err(|error| error.to_string())?
            .then(|| "network or FUSE filesystem may omit native change events".to_owned());
        for record in records.iter().filter(|record| !record.bare) {
            if !changed.contains(&record.id)
                && !failed.contains(&record.id)
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
            if failed.contains(&record.id) {
                // Validate live metadata before retaining a previous census.
                // A different row/root/repository must never inherit it.
                let old = previous
                    .filter(|old| old.id == id && old.common_dir == common_dir)
                    .and_then(|old| {
                        old.worktrees.iter().find(|old| {
                            old.id == record.id && old.root == record.path && old.git_dir == git_dir
                        })
                    });
                worktrees.push(match old {
                    Some(old) => old.clone(),
                    None => Worktree {
                        id: record.id,
                        root: record.path.clone(),
                        git_dir,
                        census: Arc::new(Census::new([], false)?),
                        polling: Some(
                            "unavailable index requires a periodic local Git check".into(),
                        ),
                    },
                });
                continue;
            }
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
                polling: worktree_polling,
            });
        }
        let polling = polling.or_else(|| {
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

    /// Git's own bookkeeping: the directories whose entries change when a
    /// worktree is added, removed, locked, or moved to another commit. A
    /// handful per worktree, so they are registered ahead of any content.
    fn metadata_paths(&self) -> BTreeMap<PathBuf, RecursiveMode> {
        let mut paths = BTreeMap::from([(self.common_dir.clone(), RecursiveMode::NonRecursive)]);
        if let Some(parent) = self.common_dir.parent() {
            paths.insert(parent.to_owned(), RecursiveMode::NonRecursive);
        }
        for name in ["refs", "worktrees"] {
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
    fn content_paths(&self) -> impl Iterator<Item = PathBuf> {
        self.worktrees.iter().flat_map(|worktree| {
            worktree
                .census
                .directories
                .iter()
                .map(|relative| worktree.absolute(relative))
                .filter(|directory| {
                    !directory
                        .ancestors()
                        .take_while(|path| path.starts_with(&worktree.root))
                        .any(|path| {
                            std::fs::symlink_metadata(path)
                                .is_ok_and(|metadata| metadata.file_type().is_symlink())
                        })
                })
        })
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
            if common.is_some_and(|relative| {
                relative == Path::new("config")
                    || relative == Path::new("packed-refs")
                    || relative.starts_with("refs")
            }) {
                change.references = true;
                change.membership |= common == Some(Path::new("refs"));
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
                    change.membership = true;
                    change.topology |= path
                        .file_name()
                        .is_some_and(|name| name == "locked" || name == "gitdir");
                } else if let Some(tracked) = worktree.tracks(path) {
                    change.worktrees.insert(worktree.id);
                    // A removed/recreated ancestor needs new directory watches;
                    // simple file edits keep the existing tracked-path census.
                    change.membership |= tracked == Tracked::Directory;
                    change.topology |= path == &worktree.root;
                }
            }
        }
        (change.topology || change.references || !change.worktrees.is_empty()).then_some(change)
    }
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
    pub error: Option<String>,
    pub coverage: Option<bool>,
}

impl Change {
    pub fn merge(&mut self, other: Self) {
        self.worktrees.extend(other.worktrees);
        self.references |= other.references;
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
}

#[derive(Default)]
struct Configuration {
    projects: Vec<Arc<Project>>,
    stopped: bool,
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
    /// Metadata is claimed for every project before any content, so a budget
    /// spent on one large checkout still leaves each project's worktree list
    /// watched. A project left short of content watches is reported and polled.
    fn within(projects: &BTreeMap<ProjectId, Arc<Project>>, limit: usize) -> Self {
        let mut registrations = Self::default();
        for (&id, project) in projects {
            if let Some(reason) = &project.polling {
                registrations.failures.insert(id, reason.clone());
            }
            for (path, mode) in project.metadata_paths() {
                registrations.claim(id, path, mode, limit);
            }
        }
        for (&id, project) in projects {
            for path in project.content_paths() {
                registrations.claim(id, path, RecursiveMode::NonRecursive, limit);
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
                    notify::recommended_watcher(move |event| {
                        let mut pending = events.lock().unwrap_or_else(|p| p.into_inner());
                        pending.event(event);
                        if !pending.changes.is_empty() {
                            signal();
                        }
                    })
                };
                let mut watcher = create_watcher();
                let mut registered: BTreeMap<PathBuf, (RecursiveMode, u64, u64)> = BTreeMap::new();
                while requests.recv().is_ok() {
                    let projects = {
                        let configuration = desired_configuration
                            .lock()
                            .unwrap_or_else(|p| p.into_inner());
                        if configuration.stopped {
                            break;
                        }
                        configuration.projects.clone()
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
                    } = Registrations::within(&projects, MAX_DIRECTORIES);
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
pub(crate) mod tests {
    use super::*;
    use notify::event::{DataChange, ModifyKind};
    use std::time::{Duration, Instant};

    /// Fixture commands must not inherit a caller's repository, injected
    /// configuration, template or hooks. Production policy tests use their own
    /// commands and are deliberately unaffected by this test-only isolation.
    pub(crate) fn isolated_fixture_command(
        mut command: crate::repository::CommandSpec,
    ) -> crate::repository::CommandSpec {
        let names = std::env::vars_os()
            .map(|(name, _)| name)
            .chain(command.environment.keys().cloned())
            .collect::<BTreeSet<_>>();
        for name in names {
            if name.as_encoded_bytes().starts_with(b"GIT_") {
                command.environment.remove(&name);
                command = command.env_remove(name);
            }
        }
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .timeout(Duration::from_secs(30))
    }

    pub(crate) fn run_fixture_command(
        mut command: crate::repository::CommandSpec,
    ) -> crate::repository::ProcessOutput {
        use crate::repository::ProcessExecutor;
        command.stdout_limit = 64 * 1024;
        command.stderr_limit = 64 * 1024;
        let output = crate::repository::SystemProcessExecutor
            .execute(&command, &CancellationToken::default())
            .expect("bounded isolated fixture command runs");
        assert!(
            output.success,
            "fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    pub(crate) fn fixture_isolated(test: &str) -> bool {
        const CHILD: &str = "CAGENTS_TEST_GIT_WATCH_FIXTURE";
        if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new(test)) {
            for (name, value) in
                std::env::vars_os().filter(|(name, _)| name.as_encoded_bytes().starts_with(b"GIT_"))
            {
                let expected = match name.to_str() {
                    Some("GIT_CONFIG_GLOBAL" | "GIT_CONFIG_SYSTEM") => "/dev/null",
                    Some("GIT_CONFIG_NOSYSTEM") => "1",
                    _ => panic!("unexpected Git environment in isolated fixture: {name:?}"),
                };
                assert_eq!(value, std::ffi::OsStr::new(expected));
            }
            return true;
        }
        let command = crate::repository::CommandSpec::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, test);
        let output = run_fixture_command(isolated_fixture_command(command));
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed;"),
            "the owned child must run exactly its fixture test"
        );
        false
    }

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
                    polling: None,
                },
                Worktree {
                    id: WorktreeId::new(),
                    root: linked,
                    git_dir: common.join("worktrees/linked"),
                    census,
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
        assert_eq!(empty.directories.iter().collect::<Vec<_>>(), [Path::new("")]);
    }

    fn git(root: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
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

    #[test]
    fn failed_index_retains_its_census_without_blocking_healthy_membership_or_removal() {
        const TEST: &str = "git_watch::tests::failed_index_retains_its_census_without_blocking_healthy_membership_or_removal";
        if !fixture_isolated(TEST) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let linked = directory.path().join("linked");
        let removed = directory.path().join("removed");
        std::fs::create_dir_all(first.join("src")).unwrap();
        std::fs::write(first.join("src/original"), "original\n").unwrap();
        git(&first, &["init", "-q"]);
        git(&first, &["add", "."]);
        git(&first, &["commit", "-qm", "fixture"]);
        git(
            &first,
            &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
        );
        git(
            &first,
            &[
                "worktree",
                "add",
                "-qb",
                "removed",
                removed.to_str().unwrap(),
            ],
        );
        let service = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        let common = service.common_dir(&first, &cancellation).unwrap();
        let records = service.list_worktrees(&first, &cancellation).unwrap();
        let project = ProjectId::new();
        let changed = records.iter().map(|record| record.id).collect();
        let original = Project::read_changed(
            project,
            common.clone(),
            &records,
            &cancellation,
            None,
            &changed,
        )
        .unwrap();
        let linked_original = original
            .worktrees
            .iter()
            .find(|row| row.root == linked)
            .unwrap();
        let failed = BTreeSet::from([linked_original.id]);
        let private = service.git_dir(&linked, &cancellation).unwrap();
        let index = std::fs::read(private.join("index")).unwrap();
        std::fs::write(private.join("index"), "broken index\n").unwrap();
        std::fs::create_dir(first.join("fresh")).unwrap();
        std::fs::write(first.join("fresh/new"), "new tracked path\n").unwrap();
        git(&first, &["add", "fresh/new"]);
        git(&first, &["worktree", "remove", removed.to_str().unwrap()]);
        let records = service.list_worktrees(&first, &cancellation).unwrap();
        let changed = records.iter().map(|record| record.id).collect();
        // Without an observed status failure, index-read errors stay strict.
        assert!(
            Project::read_changed(
                project,
                common.clone(),
                &records,
                &cancellation,
                Some(&original),
                &changed,
            )
            .is_err()
        );
        let retained = Project::read_changed_with_failed_status(
            project,
            common.clone(),
            &records,
            &cancellation,
            Some(&original),
            &changed,
            &failed,
        )
        .unwrap();
        let linked_retained = retained
            .worktrees
            .iter()
            .find(|row| row.root == linked)
            .unwrap();
        assert!(Arc::ptr_eq(
            &linked_retained.census,
            &linked_original.census
        ));
        assert_eq!(linked_retained.git_dir, linked_original.git_dir);
        assert_eq!(
            linked_retained.tracks(&linked.join("src/original")),
            Some(Tracked::File)
        );
        assert_eq!(
            retained
                .worktrees
                .iter()
                .find(|row| row.root == first)
                .unwrap()
                .tracks(&first.join("fresh/new")),
            Some(Tracked::File)
        );
        assert!(!retained.worktrees.iter().any(|row| row.root == removed));
        let registrations = Registrations::within(
            &BTreeMap::from([(project, Arc::new(retained.clone()))]),
            usize::MAX,
        );
        assert!(registrations.desired.contains_key(&first.join("fresh")));
        assert!(!registrations.desired.contains_key(&removed));
        // A new failed row gets honest metadata-only coverage, never a
        // successful empty index census borrowed from a different owner.
        let cold = Project::read_changed_with_failed_status(
            project,
            common.clone(),
            &records,
            &cancellation,
            None,
            &changed,
            &failed,
        )
        .unwrap();
        let cold_linked = cold
            .worktrees
            .iter()
            .find(|row| row.root == linked)
            .unwrap();
        assert_eq!(cold_linked.tracks(&linked.join("src/original")), None);
        assert_eq!(cold_linked.tracks(&linked), Some(Tracked::Directory));
        assert!(cold.polling.is_some());
        assert!(cold.metadata_paths().contains_key(&private));
        let cold_registrations = Registrations::within(
            &BTreeMap::from([(project, Arc::new(cold.clone()))]),
            usize::MAX,
        );
        assert!(
            cold_registrations
                .desired
                .contains_key(&first.join("fresh"))
        );
        assert!(cold_registrations.desired.contains_key(&private));
        assert!(cold_registrations.failures.contains_key(&project));
        // Known failure never excuses a different repository at the row path.
        let foreign = directory.path().join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        git(&foreign, &["init", "-q"]);
        let mut replaced = records.clone();
        replaced
            .iter_mut()
            .find(|row| row.id == linked_original.id)
            .unwrap()
            .path = foreign;
        assert!(
            Project::read_changed_with_failed_status(
                project,
                common.clone(),
                &replaced,
                &cancellation,
                Some(&original),
                &changed,
                &failed,
            )
            .is_err()
        );
        // Nor does it mask an unrelated healthy row's unreadable index.
        let first_index = std::fs::read(common.join("index")).unwrap();
        std::fs::write(common.join("index"), "another broken index\n").unwrap();
        assert!(
            Project::read_changed_with_failed_status(
                project,
                common.clone(),
                &records,
                &cancellation,
                Some(&original),
                &changed,
                &failed,
            )
            .is_err()
        );
        std::fs::write(common.join("index"), first_index).unwrap();
        std::fs::write(private.join("index"), index).unwrap();
        let repaired = Project::read_changed_with_failed_status(
            project,
            common,
            &records,
            &cancellation,
            Some(&retained),
            &changed,
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(repaired.polling.is_none());
        assert_eq!(
            repaired
                .worktrees
                .iter()
                .find(|row| row.root == first)
                .unwrap()
                .tracks(&first.join("fresh/new")),
            Some(Tracked::File)
        );
        assert_eq!(
            repaired
                .worktrees
                .iter()
                .find(|row| row.root == linked)
                .unwrap()
                .tracks(&linked.join("src/original")),
            Some(Tracked::File)
        );
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
        assert!(
            project
                .change(&modified(first.root.join("src/untracked.log")))
                .is_none()
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
        assert!(change.worktrees.is_empty(), "no worktree's own files changed");
        assert!(!change.topology);
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

    fn wait_for(
        (monitor, woken): &(Monitor, mpsc::Receiver<()>),
        project: ProjectId,
        predicate: impl Fn(&Change) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if woken.try_recv().is_ok()
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
}
