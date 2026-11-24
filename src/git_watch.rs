//! Bounded filesystem invalidation for Git state. The watcher never runs Git or
//! GitHub; its coalesced edges ask the owner's cancellable reader to do so. The
//! window's sidebar and the daemon's Work board each own one.
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::OsStr,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{
    model::{ProjectId, WorktreeId},
    process::ProcessExecutor,
    project_location::{GitDirectories, git_directories},
    repository::{CancellationToken, RepositoryService, WorktreeRecord},
};

const MAX_DIRECTORIES: usize = 8192;
/// How long metadata that evicting every content watch could not admit waits
/// before evicting for it again. Capacity can return in another process
/// meanwhile, but every eviction rewatches and rescans all content.
const EVICTION_RETRY: Duration = Duration::from_secs(10 * 60);

/// Paths relative to a worktree root, sorted and kept in one allocation. A
/// large checkout tracks tens of thousands of paths, and each watcher owner
/// holds them for every worktree.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
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
/// which a re-plan that finds them unchanged keeps as well. A worktree whose
/// files are not watched has the empty census, read from nothing.
#[derive(Debug, Default, PartialEq, Eq, Hash)]
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

/// Which version of a file Git read. Git replaces the index and configuration
/// by renaming new files over them, so a write gives them a new inode and
/// times. Only a filesystem that `reports_rewrites` can keep a cached read
/// by this identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileIdentity {
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        Self::from_metadata(&metadata)
    }

    fn from_metadata(metadata: &std::fs::Metadata) -> Option<Self> {
        metadata.is_file().then(|| Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusFile {
    Present(FileIdentity),
    Missing { device: u64, inode: u64 },
}

impl StatusFile {
    fn of(directory: &Path, name: &str) -> Option<Self> {
        let path = directory.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                let identity = FileIdentity::from_metadata(&metadata)?;
                Filesystem::of(&path)
                    .is_ok_and(Filesystem::reports_rewrites)
                    .then_some(Self::Present(identity))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !Filesystem::of(directory).is_ok_and(Filesystem::reports_rewrites) {
                    return None;
                }
                let directory = std::fs::metadata(directory).ok()?;
                directory.is_dir().then_some(Self::Missing {
                    device: directory.dev(),
                    inode: directory.ino(),
                })
            }
            Err(_) => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct StatusMetadata {
    directories: GitDirectories,
    index: StatusFile,
    config: StatusFile,
    worktree_config: StatusFile,
}

impl StatusMetadata {
    fn of(worktree: &Path) -> Option<Self> {
        let directories = git_directories(worktree)?;
        Some(Self {
            index: StatusFile::of(&directories.private, "index")?,
            config: StatusFile::of(&directories.common, "config")?,
            worktree_config: StatusFile::of(&directories.private, "config.worktree")?,
            directories,
        })
    }
}

/// Evidence supplied by a successful status read. Metadata identities are
/// taken before invoking Git so that a write during the read still invalidates it.
#[derive(Debug, Clone)]
pub struct StatusRead {
    metadata: Option<Arc<StatusMetadata>>,
    pub completed_at: Instant,
}

impl StatusRead {
    pub fn read<T, E>(
        worktree: &Path,
        read: impl FnOnce() -> Result<T, E>,
    ) -> Result<(T, Self), E> {
        let metadata = StatusMetadata::of(worktree).map(Arc::new);
        let status = read()?;
        Ok((
            status,
            Self {
                metadata,
                completed_at: Instant::now(),
            },
        ))
    }

    /// Whether the index or repository configuration may have changed since
    /// the last status read. The worktree listing checks HEAD separately.
    pub fn metadata_changed(&self, worktree: &Path) -> bool {
        self.metadata
            .as_deref()
            .is_none_or(|metadata| StatusMetadata::of(worktree).as_ref() != Some(metadata))
    }
}

/// How much watching a worktree's own files is worth to its owner.
///
/// Git's bookkeeping for every worktree, its index, `HEAD` and the shared
/// refs, is a handful of watches and always registered, so staging,
/// committing, switching branches and adding or removing worktrees arrive as
/// events whatever this says. An edit to a working file shows only through a
/// watch on its directory, though, and those cost one inotify watch per
/// tracked directory, an index census to plan, and a stat of each directory
/// on every registration. A repository with hundreds of worktrees cannot
/// have them all, so the shared directory budget goes in this order, and an
/// owner that only needs some worktrees current says which.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Interest {
    /// Someone works in it now.
    Active,
    /// Shown without anyone working in it, such as a project's own checkout.
    Shown,
    /// Watched while the budget lasts.
    #[default]
    Background,
    /// Only its Git metadata is watched: edits to its files go unseen until
    /// the owner reads it for another reason.
    Metadata,
}

impl Interest {
    pub fn watches_files(self) -> bool {
        self != Self::Metadata
    }
}

#[derive(Debug, Clone)]
pub struct Worktree {
    pub id: WorktreeId,
    pub root: PathBuf,
    pub git_dir: PathBuf,
    interest: Interest,
    census: Arc<Census>,
    /// The index `census` was read from, taken before reading it: a write
    /// that lands during the read changes it, so the next plan reads again.
    index: Option<FileIdentity>,
    polling: Option<String>,
}

/// Equal worktrees ask for the same watches, in the same order, and polling.
/// Which index file the census came from only saves its next read: an index
/// rewritten with the same paths changes nothing a watcher must re-register
/// or its owner re-read.
impl PartialEq for Worktree {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.root == other.root
            && self.git_dir == other.git_dir
            && self.interest == other.interest
            && self.census == other.census
            && self.polling == other.polling
    }
}

impl Eq for Worktree {}

impl Worktree {
    /// How much its owner asked for this worktree's files to be watched.
    pub fn interest(&self) -> Interest {
        self.interest
    }

    /// Whether the owner must read this worktree once it is registered in
    /// place of `before`: it is new or moved, or its files are watched by a
    /// census that changed, so an edit made before the new watches existed
    /// may not have arrived. Losing file watches, or moving within the budget
    /// order, misses nothing that was not already missed.
    fn needs_read_after(&self, before: Option<&Self>) -> bool {
        let Some(before) = before else {
            return true;
        };
        self.root != before.root
            || self.git_dir != before.git_dir
            || (self.interest.watches_files()
                && (!before.interest.watches_files()
                    || self.census != before.census
                    || self.polling != before.polling))
    }

    /// Whether `path` is a tracked file or a directory on the way to one.
    fn tracks(&self, path: &Path) -> Option<Tracked> {
        let relative = path.strip_prefix(&self.root).ok()?;
        if self.census.directories.contains(relative) {
            Some(Tracked::Directory)
        } else {
            self.census
                .files
                .contains(relative)
                .then_some(Tracked::File)
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
    /// An owner that discarded its status asks for a new baseline even when
    /// removal and re-addition of its watch plan coalesce.
    baseline_generation: u64,
    /// Why the shared metadata itself may change without an event. A reason
    /// confined to one worktree's files is that worktree's own.
    polling: Option<String>,
}

impl Project {
    /// Identifies the owner's status baseline independently of its watch paths.
    pub fn set_baseline_generation(&mut self, generation: u64) {
        self.baseline_generation = generation;
    }

    /// The watch plan for `records`, re-reading only what may have moved: a
    /// worktree `changed` names, or one `previous` does not hold at the same
    /// root. A re-read runs Git only for an index that was rewritten since
    /// `previous` read it, so polling every worktree costs no Git process
    /// where nothing changed. Every worktree's files are watched while the
    /// budget lasts.
    pub fn read_changed(
        id: ProjectId,
        common_dir: PathBuf,
        records: &[WorktreeRecord],
        cancellation: &CancellationToken,
        previous: Option<&Self>,
        changed: &BTreeSet<WorktreeId>,
    ) -> Result<Self, String> {
        Self::read_changed_with_interest(
            id,
            common_dir,
            records,
            cancellation,
            previous,
            changed,
            &|_| Interest::default(),
        )
    }

    /// [`read_changed`](Self::read_changed), with each worktree's files
    /// watched as `interest` says. A worktree of [`Interest::Metadata`] has
    /// no census, so its index is never read to plan it.
    pub fn read_changed_with_interest(
        id: ProjectId,
        common_dir: PathBuf,
        records: &[WorktreeRecord],
        cancellation: &CancellationToken,
        previous: Option<&Self>,
        changed: &BTreeSet<WorktreeId>,
        interest: &dyn Fn(WorktreeId) -> Interest,
    ) -> Result<Self, String> {
        Self::read_changed_with(
            &RepositoryService::system("git"),
            &Filesystem::of,
            id,
            common_dir,
            records,
            cancellation,
            previous,
            changed,
            interest,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn read_changed_with<E: ProcessExecutor>(
        git: &RepositoryService<E>,
        filesystem: &dyn Fn(&Path) -> std::io::Result<Filesystem>,
        id: ProjectId,
        common_dir: PathBuf,
        records: &[WorktreeRecord],
        cancellation: &CancellationToken,
        previous: Option<&Self>,
        changed: &BTreeSet<WorktreeId>,
        interest: &dyn Fn(WorktreeId) -> Interest,
    ) -> Result<Self, String> {
        let polling = filesystem(&common_dir)
            .map_err(|error| error.to_string())?
            .omits_events()
            .then(|| REMOTE_EVENTS.to_owned());
        let retained = previous
            .into_iter()
            .flat_map(|previous| &previous.worktrees)
            .map(|worktree| (worktree.id, worktree))
            .collect::<HashMap<_, _>>();
        // Most worktrees of a repository track the same paths: one census
        // serves them all, found by its contents rather than compared with
        // every worktree's in turn. Hashing a census reads all of its paths,
        // so the known ones are indexed only once an index is actually read.
        let mut censuses: Option<HashSet<Arc<Census>>> = None;
        let mut worktrees = Vec::new();
        for record in records.iter().filter(|record| !record.bare) {
            let interest = interest(record.id);
            let old = retained
                .get(&record.id)
                .copied()
                .filter(|old| old.root == record.path);
            if let Some(old) = old
                && !changed.contains(&record.id)
                && old.interest.watches_files() == interest.watches_files()
            {
                worktrees.push(Worktree {
                    interest,
                    ..old.clone()
                });
                continue;
            }
            // The marker names both directories; Git answers only for a
            // checkout whose marker cannot be read the way Git reads it.
            let GitDirectories {
                private: git_dir,
                common: actual_common,
            } = match git_directories(&record.path) {
                Some(directories) => directories,
                None => GitDirectories {
                    common: git
                        .common_dir(&record.path, cancellation)
                        .map_err(|error| error.to_string())?,
                    private: git
                        .git_dir(&record.path, cancellation)
                        .map_err(|error| error.to_string())?,
                },
            };
            if actual_common != common_dir {
                return Err(format!(
                    "{} now belongs to another repository",
                    record.path.display()
                ));
            }
            if !interest.watches_files() {
                worktrees.push(Worktree {
                    id: record.id,
                    root: record.path.clone(),
                    git_dir,
                    interest,
                    census: Arc::default(),
                    index: None,
                    polling: None,
                });
                continue;
            }
            let remote = filesystem(&record.path)
                .map_err(|error| error.to_string())?
                .omits_events();
            let index = if filesystem(&git_dir)
                .map_err(|error| error.to_string())?
                .reports_rewrites()
            {
                FileIdentity::of(&git_dir.join("index"))
            } else {
                None
            };
            let census = match old
                .filter(|old| index.is_some() && old.index == index && old.git_dir == git_dir)
            {
                Some(old) => old.census.clone(),
                None => {
                    let paths = git
                        .index_paths(&record.path, cancellation)
                        .map_err(|error| error.to_string())?;
                    let census = Arc::new(Census::new(
                        paths.paths.iter().map(|path| path.as_os_str().as_bytes()),
                        paths.gitlinks,
                    )?);
                    let censuses = censuses.get_or_insert_with(|| {
                        let mut shared = HashSet::new();
                        retained
                            .values()
                            .filter(|worktree| shared.insert(Arc::as_ptr(&worktree.census)))
                            .map(|worktree| worktree.census.clone())
                            .collect()
                    });
                    match censuses.get(&census) {
                        Some(known) => known.clone(),
                        None => {
                            censuses.insert(census.clone());
                            census
                        }
                    }
                }
            };
            let worktree_polling = if census.submodules {
                Some("submodule contents require a periodic local Git check".into())
            } else {
                remote.then(|| REMOTE_EVENTS.to_owned())
            };
            worktrees.push(Worktree {
                id: record.id,
                root: record.path.clone(),
                git_dir,
                interest,
                census,
                index,
                polling: worktree_polling,
            });
        }
        Ok(Self {
            id,
            common_dir,
            worktrees,
            baseline_generation: previous.map_or(0, |project| project.baseline_generation),
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
        // A reftable repository keeps its refs here instead.
        let reftable = self.common_dir.join("reftable");
        if reftable.is_dir() {
            paths.insert(reftable, RecursiveMode::NonRecursive);
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

    /// Checkouts with at least one installed content watch. A partially
    /// covered checkout can still lose events for the directories we saw.
    /// Limiting the search to newly acquired watches identifies which
    /// checkouts need a catch-up read after an ordinary plan change.
    fn watched_files(
        &self,
        registered: &BTreeMap<PathBuf, RegisteredPath>,
        only_acquired: bool,
    ) -> BTreeSet<WorktreeId> {
        self.worktrees
            .iter()
            .filter(|worktree| {
                worktree.interest.watches_files()
                    && (registered
                        .range(worktree.root.clone()..)
                        .take_while(|(path, _)| path.starts_with(&worktree.root))
                        .any(|(path, watch)| {
                            (!only_acquired || watch.acquired)
                                && worktree.tracks(path) == Some(Tracked::Directory)
                        })
                        || worktree.tracks(&worktree.root) == Some(Tracked::Directory)
                            && worktree.root.ancestors().skip(1).any(|ancestor| {
                                registered.get(ancestor).is_some_and(|watch| {
                                    (!only_acquired || watch.acquired)
                                        && watch.mode == RecursiveMode::Recursive
                                })
                            }))
            })
            .map(|worktree| worktree.id)
            .collect()
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
            if common.is_some_and(common_metadata_noise) {
                continue;
            }
            if common.is_some_and(|relative| {
                relative == Path::new("config")
                    || relative == Path::new("packed-refs")
                    || relative.starts_with("refs")
                    || relative.starts_with("reftable")
            }) {
                change.references = true;
                if common == Some(Path::new("config")) {
                    change.configuration = true;
                    // Shared settings such as core.fileMode can change status
                    // in every checkout without touching its HEAD or index.
                    change
                        .worktrees
                        .extend(self.worktrees.iter().map(|tree| tree.id));
                }
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
                // A checkout removed or moved without Git is seen from its
                // parent, whether or not its files are watched.
                if path == &worktree.root
                    && matches!(
                        event.kind,
                        EventKind::Any
                            | EventKind::Other
                            | EventKind::Create(_)
                            | EventKind::Remove(_)
                            | EventKind::Modify(notify::event::ModifyKind::Name(_))
                    )
                {
                    change.topology = true;
                    change.membership = true;
                    change.worktrees.insert(worktree.id);
                }
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
                        );
                    change.topology |= path == &worktree.root;
                }
            }
        }
        (change.topology || change.references || !change.worktrees.is_empty()).then_some(change)
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
pub struct Change {
    /// Worktrees whose own files, index or `HEAD` changed.
    pub worktrees: BTreeSet<WorktreeId>,
    /// Shared refs changed: a commit, fetch or branch update. Any
    /// worktree's comparison may have moved, but its status only where its
    /// own `HEAD` did, which a consumer can tell from the refs.
    pub references: bool,
    /// The shared configuration changed, so what a branch resolves to
    /// through it — its remote and that remote's URLs — may have moved even
    /// where no ref did.
    pub configuration: bool,
    /// Metadata events may have been lost. Compare the index and configuration
    /// with the last status read as well as the freshly listed branches and heads.
    pub rescan: bool,
    pub topology: bool,
    pub membership: bool,
    pub error: Option<String>,
    pub coverage: Option<bool>,
    /// With `coverage: Some(false)`, whether registering the same plan again
    /// may restore coverage: a native registration failed or the backend
    /// reported a fault. A plan that asks to be polled or exceeds the watch
    /// budget stays uncovered however often it is registered.
    pub recoverable: bool,
    /// Reported with `coverage`: the worktrees whose own files no watch
    /// covers, because their owner did not ask for it, the budget ran out, or
    /// they need periodic checks anyway. `Some` says that the project's
    /// metadata is watched, so `coverage: Some(false)` with it leaves only
    /// these worktrees' file edits unseen. `None` with `coverage` reported
    /// says that the metadata is not watched either.
    pub unwatched: Option<BTreeSet<WorktreeId>>,
}

impl Change {
    fn recovering(worktrees: BTreeSet<WorktreeId>) -> Self {
        Self {
            worktrees,
            references: true,
            configuration: true,
            rescan: true,
            topology: true,
            membership: true,
            ..Self::default()
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.worktrees.extend(other.worktrees);
        self.references |= other.references;
        self.configuration |= other.configuration;
        self.rescan |= other.rescan;
        self.topology |= other.topology;
        self.membership |= other.membership;
        if let Some(coverage) = other.coverage {
            self.coverage = Some(coverage);
            self.recoverable = other.recoverable;
            self.unwatched = other.unwatched;
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
    watched_files: BTreeMap<ProjectId, BTreeSet<WorktreeId>>,
    changes: BTreeMap<ProjectId, Change>,
    faulted: bool,
    // Only desired recursive roots are retained, never an unbounded list of
    // failed descendants. A quota error no recursive root accounts for leaves
    // coverage unknown until the next registration rearms every watch.
    recursive: BTreeMap<PathBuf, BTreeSet<ProjectId>>,
    quota_rearm: BTreeSet<PathBuf>,
    quota_unknown: BTreeSet<ProjectId>,
}

#[derive(Default)]
struct Configuration {
    projects: Vec<Arc<Project>>,
    stopped: bool,
}

impl Pending {
    fn event(&mut self, event: notify::Result<Event>) -> bool {
        if event.as_ref().is_ok_and(access_only) {
            return false;
        }
        if let Err(error) = &event {
            if matches!(error.kind, notify::ErrorKind::MaxFilesWatch) {
                // notify's inotify backend stops the whole pending expansion
                // batch at the first quota error. Other recursive roots may
                // have skipped descendants without appearing in error.paths.
                // Recheck those roots too, preserving nonrecursive watches.
                if self.quota_rearm.is_empty() {
                    self.quota_rearm.extend(self.recursive.keys().cloned());
                }
                let mut unknown = error.paths.is_empty();
                for path in &error.paths {
                    if !path
                        .ancestors()
                        .any(|root| self.recursive.contains_key(root))
                    {
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
            let change = if let Ok(event) = &event
                && !event.need_rescan()
            {
                project.change(event)
            } else {
                let mut change =
                    Change::recovering(self.watched_files.get(&id).cloned().unwrap_or_default());
                if let Err(error) = &event {
                    change.error = Some(error.to_string());
                    change.coverage = Some(false);
                    change.recoverable = true;
                }
                Some(change)
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
        self.quota_rearm
            .retain(|path| self.recursive.contains_key(path));
        // Which watches an unattributed quota error cost is unknown, so the
        // next registration rebuilds them all, metadata first, rather than
        // leaving every project polled until it is closed.
        let unknown = !std::mem::take(&mut self.quota_unknown).is_empty();
        Rearm {
            all: std::mem::take(&mut self.faulted) || unknown,
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

struct RegisteredPath {
    mode: RecursiveMode,
    identity: (u64, u64),
    acquired: bool,
}

/// What the watcher should hold for a set of projects, and which of them it
/// could not cover in full.
#[derive(Default)]
struct Registrations {
    desired: BTreeMap<PathBuf, RecursiveMode>,
    metadata: BTreeSet<PathBuf>,
    owners: BTreeMap<PathBuf, BTreeSet<ProjectId>>,
    /// Projects whose metadata is not covered: their owner must poll them.
    failures: BTreeMap<ProjectId, String>,
    /// Projects whose metadata is covered but whose wanted file watches are
    /// not all in place, with why.
    content_failures: BTreeMap<ProjectId, String>,
    /// Per project, the worktrees whose own files no watch covers.
    unwatched: BTreeMap<ProjectId, BTreeSet<WorktreeId>>,
}

/// What one registration claim found.
enum Claim {
    Watched,
    /// Absent, not a directory: nothing beneath it to watch.
    Absent,
    Unreadable(String),
    /// The watch or filesystem-work budget is spent.
    Exhausted,
}

const WATCH_LIMIT: &str = "directory watch limit reached";

impl Registrations {
    /// Metadata gets first claim on the watch and filesystem-work budgets,
    /// then worktrees' files in [`Interest`] order. Content traversal also
    /// has a candidate budget: missing or symlinked directories cannot make a
    /// large census an unbounded planning job. Projects left short of
    /// metadata coverage retain the existing polling fallback; a worktree
    /// left short of file coverage is reported unwatched.
    fn within(projects: &BTreeMap<ProjectId, Arc<Project>>, limit: usize) -> Self {
        Self::within_with(projects, limit, |path| std::fs::symlink_metadata(path))
    }

    fn within_with(
        projects: &BTreeMap<ProjectId, Arc<Project>>,
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
                match registrations.claim(id, path.clone(), mode, limit, &mut checks, &mut inspect)
                {
                    Claim::Watched => {
                        registrations.metadata.insert(path);
                    }
                    Claim::Absent => {}
                    Claim::Unreadable(error) => {
                        registrations.failures.insert(id, error);
                    }
                    Claim::Exhausted => {
                        registrations.failures.insert(id, WATCH_LIMIT.to_owned());
                    }
                }
            }
        }
        let mut roots: BTreeMap<&Path, Vec<(ProjectId, &Worktree)>> = BTreeMap::new();
        let mut wanted = Vec::new();
        for (&id, project) in projects {
            for worktree in &project.worktrees {
                roots
                    .entry(&worktree.root)
                    .or_default()
                    .push((id, worktree));
                if worktree.interest.watches_files() {
                    wanted.push((worktree.interest, id, worktree));
                } else {
                    registrations.unwatched(id, worktree.id);
                }
            }
        }
        // Stable: within one interest, projects and their worktrees keep
        // their order, so the same plan spends the budget the same way.
        wanted.sort_by_key(|(interest, ..)| *interest);
        let mut candidates = limit;
        let mut exhausted = false;
        for (_, id, worktree) in wanted {
            if !exhausted {
                exhausted = !registrations.content(
                    id,
                    worktree,
                    limit,
                    &mut candidates,
                    &mut checks,
                    &mut inspect,
                );
            } else {
                registrations.content_failure(id, worktree.id, WATCH_LIMIT);
            }
            if let Some(reason) = &worktree.polling {
                registrations.content_failure(id, worktree.id, reason);
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

    fn unwatched(&mut self, project: ProjectId, worktree: WorktreeId) {
        self.unwatched.entry(project).or_default().insert(worktree);
    }

    fn content_failure(&mut self, project: ProjectId, worktree: WorktreeId, reason: &str) {
        self.unwatched(project, worktree);
        self.content_failures
            .entry(project)
            .or_insert_with(|| reason.to_owned());
    }

    /// Watches `worktree`'s tracked directories. False when the budget ran
    /// out on the way; the worktree is then unwatched, as is one whose
    /// directories could not all be read.
    fn content(
        &mut self,
        id: ProjectId,
        worktree: &Worktree,
        limit: usize,
        candidates: &mut usize,
        checks: &mut usize,
        inspect: &mut impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
    ) -> bool {
        let mut blocked = BTreeSet::new();
        // The census includes every ancestor, sorted before its children.
        // One no-follow check per directory suffices; a non-directory
        // ancestor excludes its entire subtree without reopening it.
        for relative in worktree.census.directories.iter() {
            if *candidates == 0 {
                self.content_failure(id, worktree.id, WATCH_LIMIT);
                return false;
            }
            *candidates -= 1;
            if relative
                .ancestors()
                .any(|ancestor| blocked.contains(ancestor))
            {
                continue;
            }
            match self.claim(
                id,
                worktree.absolute(relative),
                RecursiveMode::NonRecursive,
                limit,
                checks,
                inspect,
            ) {
                Claim::Watched => {}
                Claim::Absent => {
                    blocked.insert(relative.to_owned());
                }
                Claim::Unreadable(error) => {
                    self.content_failure(id, worktree.id, &error);
                    blocked.insert(relative.to_owned());
                }
                Claim::Exhausted => {
                    self.content_failure(id, worktree.id, WATCH_LIMIT);
                    return false;
                }
            }
        }
        true
    }

    fn claim(
        &mut self,
        id: ProjectId,
        path: PathBuf,
        mode: RecursiveMode,
        limit: usize,
        checks: &mut usize,
        inspect: &mut impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
    ) -> Claim {
        if let Some(registered) = self.desired.get_mut(&path) {
            if mode == RecursiveMode::Recursive {
                *registered = mode;
            }
            self.owners.entry(path).or_default().insert(id);
            return Claim::Watched;
        }
        if self.desired.len() >= limit || *checks == 0 {
            return Claim::Exhausted;
        }
        *checks -= 1;
        match inspect(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Claim::Absent,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Claim::Absent,
            Err(error) => return Claim::Unreadable(error.to_string()),
        }
        self.owners.entry(path.clone()).or_default().insert(id);
        self.desired.insert(path, mode);
        Claim::Watched
    }
}

/// One native watcher for its owner, with at most one pending change set
/// regardless of event rate. Registration happens on its coordinator, never on
/// the owner's thread.
pub struct Monitor {
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
        // Every open of a watched file or directory, by Git status or a build,
        // would otherwise wake this watcher only to be discarded: ask the
        // kernel for changes alone.
        Self::with_watcher(wake, |handler| {
            RecommendedWatcher::new(
                handler,
                notify::Config::default().with_event_kinds(notify::EventKindMask::CORE),
            )
        })
    }

    #[cfg(all(any(test, feature = "test-support"), feature = "host"))]
    pub fn start_for_test<W: Watcher + Send + 'static>(
        wake: impl Fn() + Send + Sync + 'static,
        create: impl FnMut(Box<dyn FnMut(notify::Result<Event>) + Send>) -> notify::Result<W>
        + Send
        + 'static,
    ) -> std::io::Result<Self> {
        Self::with_watcher(wake, create)
    }

    #[cfg(all(any(test, feature = "test-support"), feature = "host"))]
    pub fn pending_coverage_for_test(&self, id: ProjectId) -> Option<bool> {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .changes
            .get(&id)
            .and_then(|change| change.coverage)
    }

    fn with_watcher<W: Watcher + Send + 'static>(
        wake: impl Fn() + Send + Sync + 'static,
        mut create: impl FnMut(Box<dyn FnMut(notify::Result<Event>) + Send>) -> notify::Result<W>
        + Send
        + 'static,
    ) -> std::io::Result<Self> {
        let (commands, requests) = mpsc::sync_channel(1);
        let configuration = Arc::new(Mutex::new(Configuration::default()));
        let desired_configuration = configuration.clone();
        let wake = Arc::new(wake);
        let pending = Arc::new(Mutex::new(Pending::default()));
        let state = pending.clone();
        let thread = thread::Builder::new()
            .name("git-watch".into())
            .spawn(move || {
                let mut create_watcher = || {
                    let events = state.clone();
                    let signal = wake.clone();
                    create(Box::new(move |event: notify::Result<Event>| {
                        let mut pending = events.lock().unwrap_or_else(|p| p.into_inner());
                        if pending.event(event) {
                            signal();
                        }
                    }))
                };
                let mut watcher = create_watcher();
                let mut registered = BTreeMap::new();
                let mut unadmitted = BTreeMap::new();
                // What the last registration left unwatched, per project: a
                // worktree that leaves this set gained watches only now.
                let mut was_unwatched: BTreeMap<ProjectId, BTreeSet<WorktreeId>> = BTreeMap::new();
                let mut was_metadata_blind = BTreeSet::new();
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
                        pending
                            .watched_files
                            .retain(|id, _| projects.contains_key(id));
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
                        mut content_failures,
                        mut unwatched,
                    } = Registrations::within(&projects, MAX_DIRECTORIES);
                    // The plan's own refusals: registering it again cannot
                    // cover these projects.
                    let planned = failures
                        .keys()
                        .chain(content_failures.keys())
                        .copied()
                        .collect::<BTreeSet<_>>();
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
                    match &mut watcher {
                        Ok(watcher) => reconcile(
                            watcher,
                            &mut registered,
                            &mut unadmitted,
                            &desired,
                            &metadata,
                            &owners,
                            &rearm,
                            &mut failures,
                        ),
                        Err(error) => {
                            failures.extend(projects.keys().map(|id| (*id, error.to_string())));
                        }
                    }
                    let rearmed_metadata = rearm
                        .recursive
                        .iter()
                        .filter_map(|path| owners.get(path))
                        .flatten()
                        .copied()
                        .collect::<BTreeSet<_>>();
                    let mut watched_files = projects
                        .iter()
                        .map(|(&id, project)| {
                            (
                                id,
                                (
                                    project.watched_files(&registered, false),
                                    project.watched_files(&registered, true),
                                ),
                            )
                        })
                        .collect::<BTreeMap<_, _>>();
                    let mut pending = state.lock().unwrap_or_else(|p| p.into_inner());
                    pending.retain_failures(&mut failures);
                    for (&id, project) in &projects {
                        let metadata_covered = !failures.contains_key(&id);
                        let coverage = metadata_covered && !content_failures.contains_key(&id);
                        let now_unwatched = if metadata_covered {
                            unwatched.remove(&id).unwrap_or_default()
                        } else {
                            project
                                .worktrees
                                .iter()
                                .map(|worktree| worktree.id)
                                .collect()
                        };
                        let before = old
                            .get(&id)
                            .map(Arc::as_ref)
                            .filter(|old| old.baseline_generation == project.baseline_generation);
                        let (watched, mut acquired) = watched_files.remove(&id).unwrap_or_default();
                        if rearm.all
                            && let Some(previous) = pending.watched_files.get(&id)
                        {
                            // A checkout can lose coverage during the rebuild.
                            // Catch up changes made before it becomes polled.
                            acquired.extend(previous);
                        }
                        let restoring_metadata =
                            metadata_covered && was_metadata_blind.contains(&id);
                        let recovering = before.is_none()
                            || restoring_metadata
                            || rearm.all
                            || rearmed_metadata.contains(&id);
                        let worktrees = reads_after_registration(
                            project,
                            before,
                            was_unwatched.get(&id),
                            &now_unwatched,
                            &acquired,
                        );
                        let mut change = if recovering {
                            // The listing and bookkeeping could have moved
                            // before these watches were installed.
                            Change::recovering(worktrees)
                        } else {
                            Change {
                                worktrees,
                                ..Change::default()
                            }
                        };
                        // Acknowledge coverage even when the paths did not change:
                        // this settles fallback after a successful retry.
                        change.coverage = Some(coverage);
                        change.recoverable = !coverage && !planned.contains(&id);
                        change.error = failures
                            .remove(&id)
                            .or_else(|| content_failures.remove(&id));
                        change.unwatched = metadata_covered.then(|| now_unwatched.clone());
                        pending.changes.entry(id).or_default().merge(change);
                        pending.watched_files.insert(id, watched);
                        was_unwatched.insert(id, now_unwatched);
                        if metadata_covered {
                            was_metadata_blind.remove(&id);
                        } else {
                            was_metadata_blind.insert(id);
                        }
                    }
                    was_unwatched.retain(|id, _| projects.contains_key(id));
                    was_metadata_blind.retain(|id| projects.contains_key(id));
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

/// `unadmitted` holds, across registrations, the metadata paths that did not
/// fit even with every content watch evicted, and when that was found.
/// Evicting again at once would only churn every content watch and rescan
/// its projects, so later passes retry the watch alone until
/// [`EVICTION_RETRY`] has passed, a full rearm, or success.
fn reconcile(
    watcher: &mut impl Watcher,
    registered: &mut BTreeMap<PathBuf, RegisteredPath>,
    unadmitted: &mut BTreeMap<PathBuf, Instant>,
    desired: &BTreeMap<PathBuf, RecursiveMode>,
    metadata: &BTreeSet<PathBuf>,
    owners: &BTreeMap<PathBuf, BTreeSet<ProjectId>>,
    rearm: &Rearm,
    failures: &mut BTreeMap<ProjectId, String>,
) {
    if rearm.all {
        unadmitted.clear();
    } else {
        unadmitted.retain(|path, at| metadata.contains(path) && at.elapsed() < EVICTION_RETRY);
    }
    registered.retain(|path, watch| {
        if !rearm.all
            && !rearm.recursive.contains(path)
            && desired.get(path) == Some(&watch.mode)
            && std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == watch.identity)
        {
            watch.acquired = false;
            true
        } else {
            let _ = watcher.unwatch(path);
            false
        }
    });
    // Once this pass has evicted all content, metadata that still meets the
    // quota cannot be helped by evicting what the content pass re-adds.
    let mut evicted = false;
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
        let quota = |result: &notify::Result<()>| {
            result
                .as_ref()
                .is_err_and(|error| matches!(error.kind, notify::ErrorKind::MaxFilesWatch))
        };
        let mut result = watcher.watch(path, mode);
        if metadata.contains(path) && !unadmitted.contains_key(path) && quota(&result) {
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
                evicted = true;
                result = watcher.watch(path, mode);
            }
            if evicted && quota(&result) {
                unadmitted.insert(path.clone(), Instant::now());
            }
        }
        let result = result.and_then(|()| {
            let identity = std::fs::symlink_metadata(path).map_err(notify::Error::io)?;
            if !identity.is_dir() {
                return Err(notify::Error::generic(
                    "watched directory changed during registration",
                ));
            }
            registered.insert(
                path.clone(),
                RegisteredPath {
                    mode,
                    identity: (identity.dev(), identity.ino()),
                    acquired: true,
                },
            );
            Ok(())
        });
        match result {
            Ok(()) => {
                unadmitted.remove(path);
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
}

/// The worktrees `project`'s owner must read once a registration has put
/// its watches in place, given the plan `before` it and which worktrees the
/// previous and this registration left without file watches:
///
/// - Every checkout when the owner has no registered baseline. Even a
///   checkout with only metadata watches may have changed since a status
///   read that ran before registration.
/// - One whose plan asks for it ([`Worktree::needs_read_after`]).
/// - One whose files are watched only now, because the budget reached it:
///   an edit made while it was unwatched would otherwise never show.
/// - One whose file watches were acquired in this pass: events may have
///   been lost while they were down.
fn reads_after_registration(
    project: &Project,
    before: Option<&Project>,
    was_unwatched: Option<&BTreeSet<WorktreeId>>,
    now_unwatched: &BTreeSet<WorktreeId>,
    acquired: &BTreeSet<WorktreeId>,
) -> BTreeSet<WorktreeId> {
    let before = before
        .into_iter()
        .flat_map(|project| &project.worktrees)
        .map(|worktree| (worktree.id, worktree))
        .collect::<HashMap<_, _>>();
    project
        .worktrees
        .iter()
        .filter(|worktree| {
            let old = before.get(&worktree.id).copied();
            acquired.contains(&worktree.id)
                || worktree.needs_read_after(old)
                || (worktree.interest.watches_files()
                    && !now_unwatched.contains(&worktree.id)
                    && was_unwatched.is_some_and(|was| was.contains(&worktree.id)))
        })
        .map(|worktree| worktree.id)
        .collect()
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

/// A filesystem, as `statfs` names it by its superblock magic. It decides two
/// things that want different defaults for a filesystem it does not name:
///
/// - Whether it is watched. Inotify sees every change made through this
///   kernel, so only changes made elsewhere escape it: by another machine, a
///   cluster node, a hypervisor host or a FUSE server. Those are named, and an
///   unknown filesystem is watched, as one that misses events would be at
///   worst caught by the daemon's periodic re-read of a covered project,
///   while polling it would cost a Git process per worktree each round.
/// - Whether an index census is kept by the index file's identity. A stale
///   census silently drops watches, while reading the index again costs one
///   `git ls-files`, so only local filesystems whose attributes come from the
///   kernel's own inode qualify, and an unknown one never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Filesystem(u64);

/// Superblock magic numbers, from the kernel's `include/uapi/linux/magic.h`
/// unless noted.
mod magic {
    pub const EXT4: u64 = 0xef53; // also ext2 and ext3
    pub const BTRFS: u64 = 0x9123_683e;
    pub const XFS: u64 = 0x5846_5342;
    pub const F2FS: u64 = 0xf2f5_2010;
    pub const TMPFS: u64 = 0x0102_1994;
    pub const BCACHEFS: u64 = 0xca45_1a4e;
    pub const OVERLAYFS: u64 = 0x794c_7630;
    /// OpenZFS `include/os/linux/zfs/sys/zfs_vfsops_os.h`.
    pub const ZFS: u64 = 0x2fc1_2fc1;

    pub const NFS: u64 = 0x6969;
    pub const SMB: u64 = 0x517b;
    pub const CIFS: u64 = 0xff53_4d42;
    pub const SMB2: u64 = 0xfe53_4d42;
    /// Also virtiofs, which is FUSE underneath.
    pub const FUSE: u64 = 0x6573_5546;
    /// 9p: WSL2's Windows drives and virtual machine shares.
    pub const V9FS: u64 = 0x0102_1997;
    pub const CEPH: u64 = 0x00c3_6400;
    pub const AFS: u64 = 0x5346_414f;
    pub const AFS_FS: u64 = 0x6b41_4653;
    pub const CODA: u64 = 0x7375_7245;
    pub const OCFS2: u64 = 0x7461_636f;
    /// `include/uapi/linux/gfs2_ondisk.h`.
    pub const GFS2: u64 = 0x0116_1970;
    /// Lustre's `LL_SUPER_MAGIC`, as in its `lustre_user.h`.
    pub const LUSTRE: u64 = 0x0bd0_0bd0;
    /// VirtualBox shared folders, `fs/vboxsf/super.c`.
    pub const VBOXSF: u64 = 0x786f_4256;
    /// User-mode Linux's view of its host.
    pub const HOSTFS: u64 = 0x00c0_ffee;
}

/// Whether native notifications can cover external writes to this directory.
#[cfg(feature = "host")]
pub(crate) fn reliable_filesystem_events(path: &Path) -> bool {
    !Filesystem::of(path).is_ok_and(Filesystem::omits_events)
}

impl Filesystem {
    fn of(path: &Path) -> std::io::Result<Self> {
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: path is NUL-terminated and stat points to a writable statfs.
        if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: successful statfs initialized the complete structure.
        let magic = unsafe { stat.assume_init() }.f_type;
        // `f_type` is signed on some targets; the magic is its low 32 bits.
        Ok(Self(magic as u64 & 0xffff_ffff))
    }

    /// Whether changes may arrive without this kernel seeing them.
    fn omits_events(self) -> bool {
        use magic::*;
        matches!(
            self.0,
            NFS | SMB
                | CIFS
                | SMB2
                | FUSE
                | V9FS
                | CEPH
                | AFS
                | AFS_FS
                | CODA
                | OCFS2
                | GFS2
                | LUSTRE
                | VBOXSF
                | HOSTFS
        )
    }

    /// Whether every write to a file shows in its attributes as this kernel
    /// reports them, so an unchanged identity means an unchanged index.
    fn reports_rewrites(self) -> bool {
        use magic::*;
        matches!(
            self.0,
            EXT4 | BTRFS | XFS | F2FS | TMPFS | BCACHEFS | OVERLAYFS | ZFS
        )
    }
}

const REMOTE_EVENTS: &str = "a shared, network or FUSE filesystem may omit native change events";

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
            baseline_generation: 0,
            polling: None,
            worktrees: vec![
                Worktree {
                    id: WorktreeId::new(),
                    root: first,
                    git_dir: common.clone(),
                    interest: Interest::default(),
                    census: census.clone(),
                    index: None,
                    polling: None,
                },
                Worktree {
                    id: WorktreeId::new(),
                    root: linked,
                    git_dir: common.join("worktrees/linked"),
                    interest: Interest::default(),
                    census,
                    index: None,
                    polling: None,
                },
            ],
        }
    }

    #[test]
    fn status_read_dates_the_result_and_rejects_an_index_modified_during_the_read() {
        let directory = tempfile::tempdir().unwrap();
        let git_dir = directory.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let index = git_dir.join("index");
        let replacement = git_dir.join("index.lock");
        let known_filesystem = Filesystem::of(&git_dir).unwrap().reports_rewrites();
        std::fs::write(&index, "before").unwrap();
        std::fs::write(&replacement, "after").unwrap();

        let (at, read) = StatusRead::read(directory.path(), || {
            std::fs::rename(&replacement, &index)?;
            Ok::<_, std::io::Error>(Instant::now())
        })
        .unwrap();
        assert!(read.completed_at >= at);
        assert!(read.metadata_changed(directory.path()));

        let (_, read) = StatusRead::read(directory.path(), || Ok::<_, ()>(())).unwrap();
        assert_eq!(read.metadata_changed(directory.path()), !known_filesystem);
        std::fs::remove_file(&index).unwrap();
        assert!(read.metadata_changed(directory.path()));

        let (_, missing_index) = StatusRead::read(directory.path(), || Ok::<_, ()>(())).unwrap();
        assert_eq!(
            missing_index.metadata_changed(directory.path()),
            !known_filesystem
        );
        std::fs::write(&index, "recreated").unwrap();
        assert!(missing_index.metadata_changed(directory.path()));

        let (_, read) = StatusRead::read(directory.path(), || Ok::<_, ()>(())).unwrap();
        std::fs::rename(&git_dir, directory.path().join("moved-git-dir")).unwrap();
        std::fs::write(&git_dir, "gitdir: moved-git-dir\n").unwrap();
        assert!(read.metadata_changed(directory.path()));
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
        // Only that worktree's files need the check; the shared metadata is
        // still watched.
        assert!(submodule.polling.is_none());
        assert!(worktree(&submodule, &linked).polling.is_none());
    }

    /// Runs Git, recording each command's subcommand.
    #[derive(Default)]
    struct Recording(Mutex<Vec<String>>);

    impl Recording {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut self.0.lock().unwrap())
        }
    }

    impl ProcessExecutor for Recording {
        fn execute(
            &self,
            command: &crate::process::CommandSpec,
            cancellation: &CancellationToken,
        ) -> Result<crate::process::ProcessOutput, crate::process::ProcessFailure> {
            // Global options and their values precede the subcommand.
            let mut arguments = command.arguments.iter().map(|argument| argument.to_str());
            let subcommand = loop {
                match arguments.next().flatten() {
                    Some("-C" | "-c") => {
                        arguments.next();
                    }
                    Some(option) if option.starts_with('-') => {}
                    other => break other.unwrap_or_default().to_owned(),
                }
            };
            self.0.lock().unwrap().push(subcommand);
            crate::process::SystemProcessExecutor.execute(
                &command
                    .clone()
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1"),
                cancellation,
            )
        }
    }

    /// A repository at `first` with linked worktrees `linked` and `other`.
    fn repository(directory: &Path) -> (PathBuf, Vec<WorktreeRecord>) {
        let first = directory.join("first");
        std::fs::create_dir_all(first.join("src")).unwrap();
        std::fs::write(first.join("src/main.rs"), "").unwrap();
        git(&first, &["init", "-q"]);
        git(&first, &["add", "."]);
        git(&first, &["commit", "-qm", "init"]);
        git(&first, &["worktree", "add", "-q", "../linked"]);
        git(&first, &["worktree", "add", "-q", "../other"]);
        let system = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        (
            system.common_dir(&first, &cancellation).unwrap(),
            system.list_worktrees(&first, &cancellation).unwrap(),
        )
    }

    /// Plans every worktree of `records` as changed, on a filesystem `magic`
    /// names, whatever the test's own directory is on.
    fn reader<'a>(
        service: &'a RepositoryService<Recording>,
        magic: u64,
        common: &'a Path,
        records: &'a [WorktreeRecord],
    ) -> impl Fn(Option<&Project>) -> Result<Project, String> + 'a {
        let every = records
            .iter()
            .map(|record| record.id)
            .collect::<BTreeSet<_>>();
        let id = ProjectId::new();
        move |previous| {
            Project::read_changed_with(
                service,
                &|_| Ok(Filesystem(magic)),
                id,
                common.to_path_buf(),
                records,
                &CancellationToken::default(),
                previous,
                &every,
                &|_| Interest::default(),
            )
        }
    }

    #[test]
    fn replans_find_git_directories_without_git_and_read_only_rewritten_indexes() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let (common, records) = repository(&directory);
        let (linked, other) = (directory.join("linked"), directory.join("other"));
        let system = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        let recording = Arc::new(Recording::default());
        let service = RepositoryService::new("git", recording.clone());
        let read = reader(&service, magic::EXT4, &common, &records);

        let plan = read(None).unwrap();
        assert_eq!(
            recording.take(),
            ["ls-files"; 3],
            "one index read per checkout, and no Git process for its directories"
        );
        for (worktree, record) in plan.worktrees.iter().zip(&records) {
            assert_eq!(worktree.root, record.path);
            assert_eq!(
                worktree.git_dir,
                system.git_dir(&record.path, &cancellation).unwrap(),
                "the marker names the directory Git does"
            );
            assert_eq!(
                system.common_dir(&record.path, &cancellation).unwrap(),
                common
            );
        }

        // A poll naming every worktree finds every index as it was read.
        let again = read(Some(&plan)).unwrap();
        assert!(recording.take().is_empty(), "no index was rewritten");
        for (worktree, old) in again.worktrees.iter().zip(&plan.worktrees) {
            assert!(Arc::ptr_eq(&worktree.census, &old.census));
        }

        // Staging rewrites one index, and only that one is read again.
        std::fs::write(other.join("README.md"), "").unwrap();
        git(&other, &["add", "README.md"]);
        let staged = read(Some(&again)).unwrap();
        assert_eq!(recording.take(), ["ls-files"]);
        let staged_other = staged
            .worktrees
            .iter()
            .find(|worktree| worktree.root == other)
            .unwrap();
        assert_eq!(
            staged_other.tracks(&other.join("README.md")),
            Some(Tracked::File)
        );

        // A marker Git would not follow either is left to Git to explain.
        std::fs::write(linked.join(".git"), "not a marker\n").unwrap();
        assert!(read(Some(&staged)).is_err());
        assert!(recording.take().contains(&"rev-parse".to_owned()));
    }

    #[test]
    fn worktrees_watched_for_metadata_alone_never_read_their_index() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let (common, records) = repository(&directory);
        let recording = Arc::new(Recording::default());
        let service = RepositoryService::new("git", recording.clone());
        let id = ProjectId::new();
        let every = records.iter().map(|record| record.id).collect();
        let read = |previous: Option<&Project>,
                    changed: &BTreeSet<WorktreeId>,
                    active: Option<WorktreeId>| {
            Project::read_changed_with(
                &service,
                &|_| Ok(Filesystem(magic::EXT4)),
                id,
                common.clone(),
                &records,
                &CancellationToken::default(),
                previous,
                changed,
                &|worktree| {
                    if Some(worktree) == active {
                        Interest::Active
                    } else {
                        Interest::Metadata
                    }
                },
            )
            .unwrap()
        };

        let cold = read(None, &every, None);
        assert!(recording.take().is_empty(), "no index is read to plan");
        assert!(
            cold.worktrees.iter().all(|worktree| worktree
                .census
                .directories
                .iter()
                .next()
                .is_none())
        );
        let projects = BTreeMap::from([(id, Arc::new(cold.clone()))]);
        let plan = Registrations::within(&projects, MAX_DIRECTORIES);
        assert!(plan.failures.is_empty() && plan.content_failures.is_empty());
        assert_eq!(
            plan.unwatched[&id], every,
            "metadata alone is still covered"
        );
        // The repository's own checkout is the common directory's parent,
        // which is watched for the worktree list either way.
        assert!(
            records[1..]
                .iter()
                .all(|record| !plan.desired.contains_key(&record.path)),
            "no linked working directory is watched"
        );

        // Someone starts working in one: only its index is read, without a
        // change naming it, and the watcher asks for a read once it watches.
        let worked = records[1].id;
        let warm = read(Some(&cold), &BTreeSet::new(), Some(worked));
        assert_eq!(recording.take(), ["ls-files"]);
        let promoted = warm.worktrees.iter().find(|w| w.id == worked).unwrap();
        let before = cold.worktrees.iter().find(|w| w.id == worked).unwrap();
        assert!(promoted.needs_read_after(Some(before)));
        let projects = BTreeMap::from([(id, Arc::new(warm.clone()))]);
        let plan = Registrations::within(&projects, MAX_DIRECTORIES);
        assert!(plan.desired.contains_key(&records[1].path));
        assert!(!plan.unwatched[&id].contains(&worked));

        // When the work ends its watches go, without a read: nothing it
        // missed while watched is missed by dropping the watches.
        let cooled = read(Some(&warm), &BTreeSet::new(), None);
        assert!(recording.take().is_empty());
        let demoted = cooled.worktrees.iter().find(|w| w.id == worked).unwrap();
        assert!(!demoted.needs_read_after(Some(promoted)));
        assert_eq!(cooled, cold);
    }

    #[test]
    fn a_worktree_the_budget_reaches_only_now_is_read() {
        let directory = tempfile::tempdir().unwrap();
        let mut project = project(directory.path());
        for worktree in &mut project.worktrees {
            worktree.census = census(&["src/main.rs", "docs/index.md"]);
            for directory in worktree.census.directories.iter() {
                std::fs::create_dir_all(worktree.absolute(directory)).unwrap();
            }
            std::fs::create_dir_all(worktree.git_dir.join("refs")).unwrap();
        }
        std::fs::create_dir_all(project.common_dir.join("worktrees")).unwrap();
        let id = project.id;
        let (first, linked) = (project.worktrees[0].id, project.worktrees[1].id);
        let metadata = {
            let mut bare = project.clone();
            for worktree in &mut bare.worktrees {
                worktree.interest = Interest::Metadata;
            }
            Registrations::within(&BTreeMap::from([(id, Arc::new(bare))]), MAX_DIRECTORIES)
                .desired
                .len()
        };
        // Room for the linked checkout's three directories, beyond the
        // first checkout's root, which its metadata already watches.
        let limit = metadata + 3;
        let plan = |active: WorktreeId| {
            let mut project = project.clone();
            for worktree in &mut project.worktrees {
                worktree.interest = if worktree.id == active {
                    Interest::Active
                } else {
                    Interest::Shown
                };
            }
            let projects = BTreeMap::from([(id, Arc::new(project.clone()))]);
            let unwatched = Registrations::within(&projects, limit)
                .unwatched
                .remove(&id)
                .unwrap_or_default();
            (project, unwatched)
        };
        let (worked_first, first_unwatched) = plan(first);
        assert!(first_unwatched.contains(&linked), "the budget fell short");
        // Work moves to the linked checkout: its plan is the same, it only
        // moved up the order, yet it is watched only now.
        let (worked_linked, linked_unwatched) = plan(linked);
        assert!(!linked_unwatched.contains(&linked));
        assert!(
            !worked_linked.worktrees[1].needs_read_after(Some(&worked_first.worktrees[1])),
            "the plan alone does not ask for it"
        );
        let reads = reads_after_registration(
            &worked_linked,
            Some(&worked_first),
            Some(&first_unwatched),
            &linked_unwatched,
            &BTreeSet::new(),
        );
        assert_eq!(reads, BTreeSet::from([linked]));
        // Registering the same plan again only reads the checkouts whose
        // content watches had to be set up again.
        let again = |acquired: &BTreeSet<WorktreeId>| {
            reads_after_registration(
                &worked_linked,
                Some(&worked_linked),
                Some(&linked_unwatched),
                &linked_unwatched,
                acquired,
            )
        };
        assert!(again(&BTreeSet::new()).is_empty());
        assert_eq!(again(&BTreeSet::from([first])), BTreeSet::from([first]));
    }

    #[test]
    fn file_watches_go_to_active_then_shown_worktrees_whatever_the_project_order() {
        let directory = tempfile::tempdir().unwrap();
        let mut projects = ["a", "b"]
            .map(|name| {
                let mut project = project(&directory.path().join(name));
                for worktree in &mut project.worktrees {
                    worktree.census = census(&["src/main.rs", "docs/index.md"]);
                    for directory in worktree.census.directories.iter() {
                        std::fs::create_dir_all(worktree.absolute(directory)).unwrap();
                    }
                    std::fs::create_dir_all(worktree.git_dir.join("refs")).unwrap();
                }
                std::fs::create_dir_all(project.common_dir.join("worktrees")).unwrap();
                project
            })
            .to_vec();
        projects.sort_by_key(|project| project.id);
        // The later project's linked worktree is worked in, the earlier
        // project's own checkout is shown, and the rest are background.
        projects[1].worktrees[1].interest = Interest::Active;
        projects[0].worktrees[0].interest = Interest::Shown;
        let active = projects[1].worktrees[1].id;
        let shown = projects[0].worktrees[0].id;
        let projects = projects
            .into_iter()
            .map(|project| (project.id, Arc::new(project)))
            .collect::<BTreeMap<_, _>>();
        let metadata = projects
            .values()
            .flat_map(|project| project.metadata_paths().into_keys())
            .collect::<BTreeSet<_>>();
        // Three directories each (root, src, docs): room for two worktrees.
        let registrations = Registrations::within(&projects, metadata.len() + 6);
        assert!(registrations.failures.is_empty());
        let unwatched = registrations
            .unwatched
            .values()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>();
        assert!(!unwatched.contains(&active));
        assert!(!unwatched.contains(&shown));
        assert_eq!(unwatched.len(), 2);
    }

    #[test]
    fn indexes_on_shared_or_unknown_filesystems_are_read_on_every_replan() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let (common, records) = repository(&directory);
        let recording = Arc::new(Recording::default());
        let service = RepositoryService::new("git", recording.clone());
        for (magic, polled) in [
            (magic::NFS, true),
            (magic::V9FS, true),
            (magic::FUSE, true),
            // Watched, as local writes reach inotify, but its attributes are
            // not known to show every rewrite.
            (0x1234_5678, false),
        ] {
            let read = reader(&service, magic, &common, &records);
            let plan = read(None).unwrap();
            assert_eq!(plan.polling.is_some(), polled, "{magic:#x}");
            assert!(
                plan.worktrees.iter().all(
                    |worktree| worktree.index.is_none() && worktree.polling.is_some() == polled
                ),
                "{magic:#x}"
            );
            recording.take();
            read(Some(&plan)).unwrap();
            assert_eq!(recording.take(), ["ls-files"; 3], "{magic:#x}");
        }
    }

    #[test]
    fn a_rewritten_index_tracking_the_same_paths_needs_no_reread() {
        struct Accepting;

        impl Watcher for Accepting {
            fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
                unreachable!("the test constructs it directly")
            }

            fn watch(&mut self, _: &Path, _: RecursiveMode) -> notify::Result<()> {
                Ok(())
            }

            fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
                Ok(())
            }

            fn kind() -> notify::WatcherKind {
                RecommendedWatcher::kind()
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let project = project(directory.path());
        for worktree in &project.worktrees {
            std::fs::create_dir_all(worktree.root.join("src")).unwrap();
            std::fs::create_dir_all(&worktree.git_dir).unwrap();
        }
        let (wake, woken) = mpsc::channel();
        let monitor = Monitor::with_watcher(
            move || {
                let _ = wake.send(());
            },
            |_| Ok(Accepting),
        )
        .unwrap();
        let settle = |plan: Project| {
            monitor.configure(vec![Arc::new(plan)]);
            woken.recv_timeout(Duration::from_secs(5)).unwrap();
            monitor.drain().remove(&project.id).unwrap()
        };
        assert_eq!(
            settle(project.clone()).worktrees.len(),
            2,
            "a new plan's worktrees are read again once watched"
        );
        let mut rewritten = project.clone();
        rewritten.worktrees[0].index = Some(FileIdentity {
            device: 1,
            inode: 2,
            size: 3,
            modified: (4, 5),
            changed: (6, 7),
        });
        assert!(settle(rewritten).worktrees.is_empty());
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
        assert!(!change.configuration);
        assert!(
            change.worktrees.is_empty(),
            "no worktree's own files changed"
        );
        assert!(!change.topology);
        let change = project
            .change(&modified(project.common_dir.join("config")))
            .unwrap();
        assert!(change.references);
        assert!(change.configuration);
        assert_eq!(change.worktrees, BTreeSet::from([first.id, linked.id]));
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
            assert!(
                !registrations.failures.contains_key(&project.id),
                "every project's metadata is still watched"
            );
            assert_eq!(
                registrations
                    .content_failures
                    .get(&project.id)
                    .map(String::as_str),
                Some("directory watch limit reached"),
                "content left unwatched is still reported, so it is polled"
            );
        }
        // One directory covers at most the small project's own checkout,
        // whose root is already watched as the repository's parent.
        let unwatched = |worktree: &Worktree| {
            registrations
                .unwatched
                .values()
                .any(|unwatched| unwatched.contains(&worktree.id))
        };
        for project in projects.values() {
            let large = project.worktrees[0].census.directories.ends.len() > 2;
            for (index, worktree) in project.worktrees.iter().enumerate() {
                if large || index > 0 {
                    assert!(unwatched(worktree), "{:?}", worktree.root);
                }
            }
        }
        assert_eq!(registrations.desired.len(), metadata.len() + 1);
        let complete = Registrations::within(&projects, MAX_DIRECTORIES);
        assert!(complete.failures.is_empty());
        assert!(complete.content_failures.is_empty());
        assert!(complete.unwatched.is_empty());
    }

    #[test]
    fn metadata_bursts_ignore_locks_but_keep_real_rename_destinations() {
        use notify::event::{AccessKind, RenameMode};

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
    fn a_checkout_removed_without_git_or_a_reftable_write_is_seen() {
        let mut project = project(Path::new("/repo"));
        // Only Git's bookkeeping is watched in it, so nothing of its census
        // claims the event from its parent directory.
        project.worktrees[1].interest = Interest::Metadata;
        project.worktrees[1].census = Arc::default();
        let linked = &project.worktrees[1];
        for kind in [
            EventKind::Remove(notify::event::RemoveKind::Folder),
            EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::From)),
        ] {
            let change = project
                .change(&Event::new(kind).add_path(linked.root.clone()))
                .unwrap();
            assert!(change.topology && change.membership, "{kind:?}");
            assert!(change.worktrees.contains(&linked.id));
        }
        assert!(
            project
                .change(
                    &Event::new(EventKind::Modify(ModifyKind::Metadata(
                        notify::event::MetadataKind::Any
                    )))
                    .add_path(linked.root.clone())
                )
                .is_none(),
            "touching the directory moves nothing"
        );
        assert!(
            project
                .change(&modified(project.common_dir.join("reftable/tables.list")))
                .unwrap()
                .references
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
        assert_eq!(
            registrations.content_failures[&id],
            "directory watch limit reached"
        );
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
        let metadata = first.metadata_paths().len();
        let projects = BTreeMap::from([(first.id, Arc::new(first)), (second.id, Arc::new(second))]);
        let registrations = Registrations::within(&projects, metadata + 2);
        assert_eq!(registrations.content_failures.len(), 2);
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

    /// An injected backend for the real registration transition. By default
    /// each requested root consumes one slot; walk_recursive also counts the
    /// descendant directory marks needed by recursive batch recovery.
    #[derive(Default)]
    struct LimitedWatcher {
        capacity: usize,
        walk_recursive: bool,
        active: BTreeMap<PathBuf, RecursiveMode>,
        attempts: Vec<PathBuf>,
        removals: Vec<PathBuf>,
        fail: BTreeSet<PathBuf>,
        remove_after_watch: Option<PathBuf>,
        /// The coordinator's memory of metadata eviction could not admit.
        unadmitted: BTreeMap<PathBuf, Instant>,
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
            if self.walk_recursive && mode == RecursiveMode::Recursive {
                for child in std::fs::read_dir(path).map_err(notify::Error::io)? {
                    let child = child.map_err(notify::Error::io)?;
                    if child.file_type().map_err(notify::Error::io)?.is_dir() {
                        self.watch(&child.path(), mode)?;
                    }
                }
            }
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

    impl LimitedWatcher {
        // Like notify's inotify event batch, stop adding descendant watches
        // after the first quota error, without reporting skipped directories.
        fn expand_batch(&mut self, directories: &[PathBuf]) -> notify::Result<()> {
            for directory in directories {
                self.watch(directory, RecursiveMode::Recursive)?;
            }
            Ok(())
        }

        fn file_event(&self, path: &Path) -> Option<Event> {
            self.active.contains_key(path.parent()?).then(|| {
                Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
                    .add_path(path.to_owned())
            })
        }
    }

    #[test]
    fn registration_overflow_and_rearm_only_invalidate_affected_watches() {
        use notify::event::Flag;

        let directory = tempfile::tempdir().unwrap();
        let mut project = project(directory.path());
        project.worktrees[1].interest = Interest::Metadata;
        project.worktrees[1].census = Arc::default();
        project.worktrees.push(Worktree {
            id: WorktreeId::new(),
            root: directory.path().join("third"),
            git_dir: project.common_dir.join("worktrees/third"),
            ..project.worktrees[0].clone()
        });
        for worktree in &project.worktrees {
            std::fs::create_dir_all(worktree.root.join("src")).unwrap();
            std::fs::create_dir_all(&worktree.git_dir).unwrap();
        }
        let primary = project.worktrees[0].id;
        let expected = BTreeSet::from([primary, project.worktrees[2].id]);
        let (wake, woken) = mpsc::sync_channel(1);
        let (handler, incoming) = mpsc::channel();
        let watcher = Monitor::with_watcher(
            move || {
                let _ = wake.try_send(());
            },
            move |emit| {
                assert!(handler.send(emit).is_ok());
                Ok(LimitedWatcher {
                    capacity: 32,
                    ..Default::default()
                })
            },
        )
        .unwrap();
        let monitor = (watcher, woken);
        let mut emit = incoming.recv_timeout(Duration::from_secs(5)).unwrap();
        let id = project.id;
        let project = Arc::new(project);
        let recovering = |change: &Change| {
            assert!(change.rescan && change.topology && change.membership);
            assert!(change.references && change.configuration);
        };

        monitor.0.configure(vec![project.clone()]);
        wait_for(&monitor, id, "first registration", |change| {
            assert_eq!(change.coverage, Some(true));
            recovering(change);
            assert_eq!(
                change.worktrees,
                project
                    .worktrees
                    .iter()
                    .map(|worktree| worktree.id)
                    .collect()
            );
            true
        });

        let source = project.worktrees[0].root.join("src");
        let old_source = std::fs::File::open(&source).unwrap();
        std::fs::remove_dir(&source).unwrap();
        std::fs::create_dir(&source).unwrap();
        monitor.0.configure(vec![project.clone()]);
        wait_for(&monitor, id, "replaced directory watch", |change| {
            assert!(!change.rescan && !change.topology && !change.references);
            assert_eq!(change.worktrees, BTreeSet::from([primary]));
            true
        });
        drop(old_source);

        emit(Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan)));
        wait_for(&monitor, id, "overflow", |change| {
            assert_eq!(change.coverage, None);
            recovering(change);
            assert_eq!(change.worktrees, expected);
            true
        });

        emit(Err(notify::Error::generic("fixture backend fault")));
        wait_for(&monitor, id, "backend fault", |change| {
            assert_eq!(change.coverage, Some(false));
            assert!(change.recoverable);
            recovering(change);
            assert_eq!(change.worktrees, expected);
            true
        });
        monitor.0.configure(vec![project]);
        wait_for(&monitor, id, "full rearm", |change| {
            assert_eq!(change.coverage, Some(true));
            recovering(change);
            assert_eq!(change.worktrees, expected);
            true
        });
    }

    fn registered_transition(
        pending: &mut Pending,
        watcher: &mut LimitedWatcher,
        registered: &mut BTreeMap<PathBuf, RegisteredPath>,
        desired: &BTreeMap<PathBuf, RecursiveMode>,
        metadata: &BTreeSet<PathBuf>,
        owners: &BTreeMap<PathBuf, BTreeSet<ProjectId>>,
    ) -> BTreeMap<ProjectId, String> {
        let rearm = pending.rearm(desired, owners);
        let mut failures = BTreeMap::new();
        let mut unadmitted = std::mem::take(&mut watcher.unadmitted);
        reconcile(
            watcher,
            registered,
            &mut unadmitted,
            desired,
            metadata,
            owners,
            &rearm,
            &mut failures,
        );
        watcher.unadmitted = unadmitted;
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
            &mut BTreeMap::new(),
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
    fn metadata_that_eviction_cannot_admit_stops_evicting_content() {
        let directory = tempfile::tempdir().unwrap();
        let content = directory.path().join("a-content");
        let first_metadata = directory.path().join("y-metadata");
        let second_metadata = directory.path().join("z-metadata");
        for p in [&content, &first_metadata, &second_metadata] {
            std::fs::create_dir(p).unwrap();
        }
        let id = ProjectId::new();
        let desired = BTreeMap::from([
            (content.clone(), RecursiveMode::NonRecursive),
            (first_metadata.clone(), RecursiveMode::Recursive),
            (second_metadata.clone(), RecursiveMode::Recursive),
        ]);
        let metadata = BTreeSet::from([first_metadata.clone(), second_metadata.clone()]);
        let owners = desired
            .keys()
            .map(|p| (p.clone(), BTreeSet::from([id])))
            .collect::<BTreeMap<_, _>>();
        // Each metadata root's subtree exceeds the quota however much content
        // yields, as when other processes hold most of the inotify limit.
        let mut watcher = LimitedWatcher {
            capacity: 2,
            fail: metadata.clone(),
            ..Default::default()
        };
        let mut registered = BTreeMap::new();
        let mut pending = Pending::default();
        let mut transition = |watcher: &mut LimitedWatcher, registered: &mut BTreeMap<_, _>| {
            watcher.removals.clear();
            watcher.attempts.clear();
            registered_transition(
                &mut pending,
                watcher,
                registered,
                &desired,
                &metadata,
                &owners,
            )
        };
        // Nothing is registered to evict yet; content then fills capacity.
        assert!(transition(&mut watcher, &mut registered).contains_key(&id));
        assert!(registered.contains_key(&content));
        assert!(watcher.unadmitted.is_empty());
        // The retry evicts content once, for the first metadata path; neither
        // fits, and content is admitted again.
        assert!(transition(&mut watcher, &mut registered).contains_key(&id));
        assert_eq!(
            watcher.removals.iter().filter(|p| **p == content).count(),
            1
        );
        assert!(registered.contains_key(&content));
        assert_eq!(
            watcher.unadmitted.keys().cloned().collect::<BTreeSet<_>>(),
            metadata,
            "the second path is not left to evict everything next time"
        );
        for _ in 0..3 {
            assert!(
                transition(&mut watcher, &mut registered).contains_key(&id),
                "the metadata stays uncovered"
            );
            assert_eq!(
                watcher.attempts,
                vec![first_metadata.clone(), second_metadata.clone()],
                "a retry tries each metadata path once and leaves content registered"
            );
            assert!(!watcher.removals.contains(&content));
            assert!(registered.contains_key(&content));
        }
        // Capacity can return elsewhere, so eviction is tried again later.
        for at in watcher.unadmitted.values_mut() {
            *at = Instant::now() - EVICTION_RETRY;
        }
        assert!(transition(&mut watcher, &mut registered).contains_key(&id));
        assert!(watcher.removals.contains(&content));
        assert_eq!(watcher.unadmitted.len(), 2);
        // Once the watches fit, the memory is settled.
        watcher.fail.clear();
        watcher.capacity = 3;
        assert!(transition(&mut watcher, &mut registered).is_empty());
        assert!(watcher.unadmitted.is_empty());
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
            &mut BTreeMap::new(),
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
    fn quota_aborted_batch_repairs_other_recursive_roots_before_clearing_polling() {
        let directory = tempfile::tempdir().unwrap();
        let a = Arc::new(project(&directory.path().join("a")));
        let b = Arc::new(project(&directory.path().join("b")));
        let a_refs = a.common_dir.join("refs");
        let b_refs = b.common_dir.join("refs");
        let mut desired = BTreeMap::new();
        let mut owners = BTreeMap::new();
        for (project, refs) in [(&a, &a_refs), (&b, &b_refs)] {
            std::fs::create_dir_all(refs).unwrap();
            for (path, mode) in [
                (&project.common_dir, RecursiveMode::NonRecursive),
                (refs, RecursiveMode::Recursive),
            ] {
                desired.insert(path.clone(), mode);
                owners.insert(path.clone(), BTreeSet::from([project.id]));
            }
        }
        let metadata = desired.keys().cloned().collect();
        let mut pending = Pending::default();
        pending.projects.insert(a.id, a.clone());
        pending.projects.insert(b.id, b.clone());
        let mut watcher = LimitedWatcher {
            capacity: 4,
            walk_recursive: true,
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
                &owners,
            )
            .is_empty()
        );

        let a_new = a_refs.join("new-a");
        let b_new = b_refs.join("new-b");
        for path in [&a_new, &b_new] {
            std::fs::create_dir(path).unwrap();
        }
        watcher.attempts.clear();
        let error = watcher
            .expand_batch(&[a_new.clone(), b_new.clone()])
            .unwrap_err();
        assert!(matches!(error.kind, notify::ErrorKind::MaxFilesWatch));
        assert_eq!(error.paths, [a_new]);
        assert_eq!(watcher.attempts.len(), 1, "B's expansion was skipped");
        let later_ref = b_new.join("later-branch");
        assert!(watcher.file_event(&later_ref).is_none());
        pending.event(Err(error));

        watcher.removals.clear();
        let failures = registered_transition(
            &mut pending,
            &mut watcher,
            &mut registered,
            &desired,
            &metadata,
            &owners,
        );
        assert!(
            failures.contains_key(&b.id),
            "B must retain polling while its skipped descendant cannot be installed"
        );
        assert!(!registered.contains_key(&b_refs));
        for project in [&a, &b] {
            assert!(registered.contains_key(&project.common_dir));
            assert!(!watcher.removals.contains(&project.common_dir));
        }

        watcher.capacity = 6;
        assert!(
            registered_transition(
                &mut pending,
                &mut watcher,
                &mut registered,
                &desired,
                &metadata,
                &owners,
            )
            .is_empty(),
            "coverage can recover once both recursive subtrees are installed"
        );
        std::fs::write(&later_ref, "fixture-ref").unwrap();
        let event = watcher.file_event(&later_ref).unwrap();
        assert!(b.change(&event).unwrap().references);
    }

    #[test]
    fn unknown_quota_error_rebuilds_every_watch_on_the_next_registration() {
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
            // An unattributed fault arriving while registration runs without
            // the Pending lock keeps that pass from claiming coverage.
            let rearm = pending.rearm(&desired, &owners);
            let mut failures = BTreeMap::new();
            reconcile(
                &mut watcher,
                &mut registered,
                &mut BTreeMap::new(),
                &desired,
                &metadata,
                &owners,
                &rearm,
                &mut failures,
            );
            pending.event(Err(error));
            pending.retain_failures(&mut failures);
            assert!(failures.contains_key(&id));
            assert_eq!(pending.quota_unknown, BTreeSet::from([id]));

            // The next registration cannot tell which watch the fault cost,
            // so it rebuilds every one rather than polling until close.
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
                .is_empty(),
                "a complete rebuild restores coverage"
            );
            assert_eq!(watcher.removals, vec![path.clone()]);
            assert_eq!(watcher.attempts, vec![path.clone()]);
            assert!(registered.contains_key(&path));
            assert!(pending.quota_unknown.is_empty());
        }
        // Healthy registrations are kept once the fault is settled.
        watcher.removals.clear();
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

    #[test]
    fn planned_refusals_are_unrecoverable_and_backend_faults_recoverable() {
        let directory = tempfile::tempdir().unwrap();
        let mut project = project(directory.path());
        project.worktrees.truncate(1);
        std::fs::create_dir_all(project.worktrees[0].root.join("src")).unwrap();
        std::fs::create_dir_all(&project.common_dir).unwrap();
        // The plan itself asks to be polled, as on a network filesystem.
        project.polling = Some("network or FUSE filesystem".into());
        let (wake, woken) = mpsc::sync_channel(1);
        let monitor = (
            Monitor::start(move || {
                let _ = wake.try_send(());
            })
            .unwrap(),
            woken,
        );
        monitor.0.configure(vec![Arc::new(project.clone())]);
        wait_for(&monitor, project.id, "a polled plan", |change| {
            change.coverage == Some(false) && !change.recoverable
        });
        // A backend fault, by contrast, may clear on a later registration.
        let mut pending = Pending::default();
        pending
            .projects
            .insert(project.id, Arc::new(project.clone()));
        pending.event(Err(notify::Error::io(std::io::Error::other(
            "backend failure",
        ))));
        let change = &pending.changes[&project.id];
        assert_eq!(change.coverage, Some(false));
        assert!(change.recoverable);
    }

    #[test]
    fn staging_committing_and_switching_branches_arrive_without_open_events() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("repository");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "first").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        let service = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        let project = Project::read_changed(
            ProjectId::new(),
            service.common_dir(&root, &cancellation).unwrap(),
            &service.list_worktrees(&root, &cancellation).unwrap(),
            &cancellation,
            None,
            &BTreeSet::new(),
        )
        .unwrap();
        let worktree = project.worktrees[0].id;
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
        // Each Git operation replaces its files by renaming a lock into place.
        let settle = || {
            thread::sleep(Duration::from_millis(100));
            while monitor.1.try_recv().is_ok() {}
            monitor.0.drain();
        };
        std::fs::write(root.join("src/main.rs"), "second").unwrap();
        settle();
        git(&root, &["add", "src/main.rs"]);
        wait_for(&monitor, project.id, "staging", |change| {
            change.worktrees.contains(&worktree) && change.membership
        });
        settle();
        git(&root, &["commit", "-qm", "second"]);
        wait_for(&monitor, project.id, "committing", |change| {
            change.references
        });
        settle();
        git(&root, &["switch", "-qc", "topic"]);
        wait_for(&monitor, project.id, "switching branches", |change| {
            change.worktrees.contains(&worktree)
        });
    }

    /// Voluntary context switches, the times a thread slept and was woken,
    /// of this process's native watcher threads, by thread ID.
    fn watcher_thread_switches() -> BTreeMap<String, u64> {
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(|task| {
                let task = task.ok()?.path();
                let name = std::fs::read_to_string(task.join("comm")).ok()?;
                if !name.starts_with("notify-rs inoti") {
                    return None;
                }
                let switches = std::fs::read_to_string(task.join("status"))
                    .ok()?
                    .lines()
                    .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))?
                    .trim()
                    .parse::<u64>()
                    .ok()?;
                Some((task.file_name()?.to_string_lossy().into_owned(), switches))
            })
            .collect()
    }

    #[test]
    fn reading_watched_files_does_not_wake_the_watcher() {
        let directory = tempfile::tempdir().unwrap();
        let mut project = project(directory.path());
        project.worktrees.truncate(1);
        let worktree = &project.worktrees[0];
        let source = worktree.root.join("src");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&worktree.git_dir).unwrap();
        let file = source.join("main.rs");
        std::fs::write(&file, "first").unwrap();
        std::fs::write(worktree.git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let others = watcher_thread_switches();
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
        // This monitor's watcher thread, and any that other tests started
        // meanwhile, but none that were already running.
        let mut before = watcher_thread_switches();
        before.retain(|thread, _| !others.contains_key(thread));
        assert!(!before.is_empty(), "the watcher thread was not found");
        // What Git status and a compiler do to a checkout: open its files and
        // list its directories, many times over, without changing anything.
        for _ in 0..200 {
            std::fs::read(&file).unwrap();
            std::fs::read(worktree.git_dir.join("HEAD")).unwrap();
            std::fs::read_dir(&source).unwrap().for_each(drop);
            thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(100));
        let after = watcher_thread_switches();
        let woke: u64 = before
            .iter()
            .map(|(thread, switches)| {
                after
                    .get(thread)
                    .map_or(0, |after| after.saturating_sub(*switches))
            })
            .sum();
        assert!(woke < 40, "the watcher woke {woke} times for reads alone");
        // A change still arrives.
        std::fs::write(&file, "second").unwrap();
        wait_for(&monitor, project.id, "edit after reads", |change| {
            change.worktrees.contains(&worktree.id)
        });
    }
}
