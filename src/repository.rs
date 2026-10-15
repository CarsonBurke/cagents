//! Git repository and worktree services.
//!
//! The service is deliberately synchronous: callers should place potentially slow
//! operations on `spawn_blocking`.  Commands are represented as data before they are
//! executed, which keeps argument handling shell-free and makes every operation
//! straightforward to test with a fake executor.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{self, Read},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

mod ignore_path;
mod browse_reveal;
pub(crate) use browse_reveal::include as include_revealed_path;
mod status_limit;

use crate::fs_safety;
use crate::model::{OperationId, WorktreeId};
use crate::persistence::{PendingOperation, PendingOperationStatus, StateStore, StoreError};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[cfg(test)]
use std::{
    io::Write,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

#[cfg(test)]
use crate::process::WaitEvent;
pub(crate) use crate::process::{
    CLEANUP_GRACE, DEFAULT_STDERR_LIMIT, SIGKILL, SIGTERM, TERMINATION_GRACE, process_failure,
    signal_process_group,
};
pub use crate::process::{
    CancellationToken, CommandSpec, FAILURE_DETAIL_LIMIT, ProcessExecutor, ProcessFailure,
    ProcessFailureKind, ProcessOutput, SystemProcessExecutor,
};

#[path = "repository_stash.rs"]
mod stash_mutations;

const ZERO_SHA1: &str = "0000000000000000000000000000000000000000";

/// Commits returned by one [`RepositoryService::history`] page.
pub const HISTORY_PAGE_SIZE: u32 = 200;

/// Largest single side of a comparison that is read into memory as text.
///
/// It matches `diff_view::MAX_TEXT_DIFF_BYTES_PER_SIDE`, so a side this service
/// hands over as [`DiffSide::Text`] is always a side the diff engine will
/// actually align instead of rejecting as oversize.
pub const MAX_DIFF_SIDE_BYTES: u64 = 16 * 1024 * 1024;

/// Bytes inspected when deciding whether content is binary. Git uses the same
/// leading-window heuristic, so a file this service calls binary is a file Git
/// also refuses to diff as text.
const BINARY_SNIFF_BYTES: usize = 8000;

/// One status refresh may inspect many untracked files. Bound aggregate reads
/// and path lookups as well as each file, leaving excess entries unmeasured.
const MAX_UNTRACKED_STATUS_BYTES: u64 = 64 * 1024 * 1024;
const MAX_UNTRACKED_STATUS_FILES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepositoryId(pub Uuid);

impl RepositoryId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RepositoryId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryInfo {
    pub id: RepositoryId,
    pub selected_path: PathBuf,
    pub common_dir: PathBuf,
    pub git_dir: PathBuf,
    pub worktree_root: Option<PathBuf>,
    pub is_bare: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorktreeLock {
    pub locked: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub id: WorktreeId,
    pub path: PathBuf,
    /// Filesystem birth time when available, never commit time or mtime.
    #[serde(default)]
    pub created_at_unix_ms: Option<i64>,
    pub head_oid: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub lock: WorktreeLock,
    pub prunable_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DirtyState {
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
    pub conflicted: u32,
    pub submodule: u32,
}

impl DirtyState {
    pub fn is_dirty(&self) -> bool {
        self.staged != 0
            || self.unstaged != 0
            || self.untracked != 0
            || self.conflicted != 0
            || self.submodule != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HeadState {
    Unborn {
        branch: Option<String>,
    },
    Branch {
        name: String,
        oid: String,
    },
    Detached {
        oid: String,
    },
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrackingState {
    #[default]
    NoUpstream,
    Compared {
        upstream: String,
        ahead: u64,
        behind: u64,
    },
    Unknown {
        upstream: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RepoPathError {
    #[error("repository path is empty")]
    Empty,
    #[error("repository path must be relative: {path}")]
    NotRelative { path: String },
    #[error("repository path has an empty component: {path}")]
    EmptyComponent { path: String },
    #[error("repository path may not contain a {component:?} component: {path}")]
    InvalidComponent { path: String, component: String },
    #[error("repository path contains a NUL byte")]
    InteriorNul,
    #[error("repository paths require Unix byte paths")]
    UnsupportedPlatform,
}

/// A worktree-relative path, byte-identical to what Git reported.
///
/// Git speaks bytes: a filename is an arbitrary NUL-free byte string, and the
/// same bytes must come back unchanged when the path is used as a pathspec.
/// Round-tripping through `String` would replace an undecodable byte with
/// U+FFFD and silently select a *different* file, so this type stores the
/// `OsString` and offers a lossy rendering only for display. Construction
/// rejects anything that could name something outside the worktree, which is
/// what makes [`RepoPath::resolve`] safe to hand to the filesystem.
///
/// Git marks a directory it will not look inside — an untracked nested
/// repository — with a trailing separator, in both `status` and `ls-files`.
/// That single trailing separator carries no path information, so it is
/// canonicalized away here and such an entry names the directory itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(OsString);

impl RepoPath {
    /// Validates raw Git path bytes.
    #[cfg(unix)]
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RepoPathError> {
        use std::os::unix::ffi::OsStringExt;

        let bytes = canonical_repo_path_bytes(bytes)?;
        Ok(Self(OsString::from_vec(bytes.to_vec())))
    }

    /// Non-Unix targets have no byte-exact path representation, so a path is
    /// rejected outright rather than transcoded into a different filename.
    #[cfg(not(unix))]
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RepoPathError> {
        canonical_repo_path_bytes(bytes)?;
        Err(RepoPathError::UnsupportedPlatform)
    }

    pub fn parse(value: impl AsRef<OsStr>) -> Result<Self, RepoPathError> {
        Self::from_bytes(value.as_ref().as_encoded_bytes())
    }

    /// The exact bytes Git reported. On Unix these are the filesystem bytes;
    /// no other target can construct a `RepoPath`.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_encoded_bytes()
    }

    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    /// A lossy rendering for labels and accessibility strings only. Never feed
    /// this back to Git or the filesystem.
    pub fn display(&self) -> Cow<'_, str> {
        self.0.to_string_lossy()
    }

    /// The final path component, lossily rendered for display.
    pub fn file_name(&self) -> Cow<'_, str> {
        self.as_path()
            .file_name()
            .map(OsStr::to_string_lossy)
            .unwrap_or(Cow::Borrowed(""))
    }

    /// The lowercase extension, or `None` when there is none or it is not
    /// UTF-8. Language selection compares against UTF-8 extension lists, so an
    /// undecodable extension must not match one by accident.
    pub fn extension(&self) -> Option<String> {
        self.as_path()
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
    }

    /// The absolute path of this entry inside `worktree`.
    pub fn resolve(&self, worktree: &Path) -> PathBuf {
        worktree.join(self.as_path())
    }

    /// Slash-separated components, as bytes.
    pub fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.as_bytes().split(|byte| *byte == b'/')
    }

    /// The containing directory, or `None` for a top-level entry.
    pub fn parent(&self) -> Option<Self> {
        let bytes = self.as_bytes();
        let separator = bytes.iter().rposition(|byte| *byte == b'/')?;
        Self::from_bytes(&bytes[..separator]).ok()
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.display())
    }
}

/// Serialized as raw bytes through `serde_bytes`, not as a `PathBuf`: a
/// `PathBuf` field would force the value through `str` in text formats and
/// corrupt any filename that is not UTF-8.
impl Serialize for RepoPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde_bytes::Bytes::new(self.as_bytes()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = serde_bytes::ByteBuf::deserialize(deserializer)?;
        Self::from_bytes(bytes.as_ref()).map_err(serde::de::Error::custom)
    }
}

/// Validates Git path bytes and returns the canonical form: exactly one
/// trailing separator, which is how Git names a directory, is removed. A
/// second one is still an empty component and is rejected, so only the form
/// Git actually emits is accepted.
fn canonical_repo_path_bytes(bytes: &[u8]) -> Result<&[u8], RepoPathError> {
    let canonical = match bytes.split_last() {
        Some((b'/', rest)) => rest,
        _ => bytes,
    };
    if canonical.is_empty() {
        return Err(RepoPathError::Empty);
    }
    if canonical.contains(&0) {
        return Err(RepoPathError::InteriorNul);
    }
    let rendering = || String::from_utf8_lossy(canonical).into_owned();
    if canonical[0] == b'/' {
        return Err(RepoPathError::NotRelative { path: rendering() });
    }
    // Validated on raw bytes rather than through `Path::components`, which
    // normalises `.` and repeated separators away instead of reporting them.
    for component in canonical.split(|byte| *byte == b'/') {
        match component {
            b"" => return Err(RepoPathError::EmptyComponent { path: rendering() }),
            b"." | b".." => {
                return Err(RepoPathError::InvalidComponent {
                    path: rendering(),
                    component: String::from_utf8_lossy(component).into_owned(),
                });
            }
            _ => {}
        }
    }
    Ok(canonical)
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OidError {
    #[error("object id must be 40 or 64 lowercase hexadecimal characters: {0}")]
    Invalid(String),
    #[error("the null object id does not name an object")]
    Null,
}

/// A full Git object id.
///
/// Lowercase hex of SHA-1 or SHA-256 length; Git never prints anything else for
/// `%H`, `rev-parse`, or `for-each-ref`, so accepting anything else would mean
/// passing an unvalidated string back as a revision argument.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Oid(String);

impl Oid {
    pub fn parse(value: impl Into<String>) -> Result<Self, OidError> {
        let value = value.into();
        let valid = matches!(value.len(), 40 | 64)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
        if valid {
            Ok(Self(value))
        } else {
            Err(OidError::Invalid(value))
        }
    }

    /// Parses an id that must name a real object, rejecting the all-zero id
    /// Git prints for "no object".
    pub fn parse_object(value: impl Into<String>) -> Result<Self, OidError> {
        let oid = Self::parse(value)?;
        if oid.is_null() {
            Err(OidError::Null)
        } else {
            Ok(oid)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_null(&self) -> bool {
        self.0.bytes().all(|byte| byte == b'0')
    }

    /// An abbreviated id for labels. Hex is ASCII, so this never splits a
    /// character.
    pub fn short(&self) -> &str {
        &self.0[..8.min(self.0.len())]
    }
}

impl TryFrom<String> for Oid {
    type Error = OidError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<Oid> for String {
    fn from(value: Oid) -> Self {
        value.0
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A fully qualified ref name such as `refs/heads/main` or
/// `refs/remotes/origin/main`.
///
/// Comparison bases arrive from GitHub metadata and user selection, so the
/// value that reaches a revision argument must be validated: an unqualified
/// name is ambiguous between a branch, a tag, and a path, and a leading `-`
/// would be read as an option.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ValidatedRefName(String);

impl ValidatedRefName {
    pub fn parse(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        match value.strip_prefix("refs/") {
            Some(rest) if ValidatedBranchName::parse(rest).is_ok() => Ok(Self(value)),
            _ => Err(ValidationError::InvalidBase(value)),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name without its `refs/heads/` or `refs/remotes/` prefix, for
    /// display only.
    pub fn short_name(&self) -> &str {
        self.0
            .strip_prefix("refs/heads/")
            .or_else(|| self.0.strip_prefix("refs/remotes/"))
            .or_else(|| self.0.strip_prefix("refs/tags/"))
            .unwrap_or(&self.0)
    }
}

impl TryFrom<String> for ValidatedRefName {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ValidatedRefName> for String {
    fn from(value: ValidatedRefName) -> Self {
        value.0
    }
}

impl fmt::Display for ValidatedRefName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Which of Git's three views of a path a change belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeArea {
    /// HEAD to index.
    Staged,
    /// Index to worktree.
    Unstaged,
    Untracked,
    Conflict,
}

/// One porcelain-v2 status code.
///
/// `Unknown` keeps a future Git status letter from failing the whole snapshot;
/// every code Git documents today has a named variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Ignored,
    Unmerged,
    Unknown(char),
}

/// The seven unmerged code pairs Git reports in a porcelain-v2 `u` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// `DD`
    BothDeleted,
    /// `AU`
    AddedByUs,
    /// `UD`
    DeletedByThem,
    /// `UA`
    AddedByThem,
    /// `DU`
    DeletedByUs,
    /// `AA`
    BothAdded,
    /// `UU`
    BothModified,
}

/// One changed path in one area.
///
/// A path that is modified in the index *and* again in the worktree produces
/// two of these, because staging, discarding, and diffing all act on one area
/// at a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedPath {
    pub path: RepoPath,
    /// The rename or copy source, on the entry whose kind is a rename or copy.
    pub old_path: Option<RepoPath>,
    pub area: ChangeArea,
    pub kind: ChangeKind,
    pub conflict: Option<ConflictKind>,
    pub submodule: bool,
    /// `None` until text has actually been measured. Untracked text is measured
    /// against absence; binary, conflicted, and unreadable paths stay unmeasured.
    pub added_lines: Option<u64>,
    pub removed_lines: Option<u64>,
}

/// One path in a committed comparison (branch or commit diff), which has no
/// index/worktree area and cannot be conflicted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffEntry {
    pub path: RepoPath,
    pub old_path: Option<RepoPath>,
    pub kind: ChangeKind,
    pub added_lines: Option<u64>,
    pub removed_lines: Option<u64>,
}

/// A branch comparison whose entries and endpoints belong to the same range,
/// even if HEAD or the base ref moves while Git is reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchComparison {
    pub base_ref: ValidatedRefName,
    pub base_oid: Oid,
    pub head: Oid,
    pub merge_base: Oid,
    pub entries: Vec<DiffEntry>,
}

/// A pull request's endpoints, fetched into the private ref namespace so the
/// comparison can be read without checking anything out.
///
/// `head` is the commit the caller asked for, proven equal to what the remote
/// served; `merge_base` is computed locally against `base_tip`, which is the
/// base branch as the remote had it at fetch time. The two refs are display
/// handles only: every comparison built from this range names the object ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestRange {
    pub number: u64,
    /// The base branch by the name the forge and the user know it, e.g. `main`.
    pub base_branch: ValidatedBranchName,
    /// The private mirror of `refs/pull/<number>/head`.
    pub head_mirror: ValidatedRefName,
    /// The private mirror of the base branch's remote tip.
    pub base_mirror: ValidatedRefName,
    pub head: Oid,
    pub base_tip: Oid,
    pub merge_base: Oid,
}

impl PullRequestRange {
    /// The comparison this range reviews. Its display ref is the base branch's
    /// public name, never the private mirror the fetch wrote.
    pub fn basis(&self) -> DiffBasis {
        DiffBasis::PullRequest {
            number: self.number,
            base_ref: ValidatedRefName::parse(self.base_branch.full_ref())
                .expect("a validated branch name is a valid ref name"),
            head: self.head.clone(),
            merge_base: self.merge_base.clone(),
        }
    }
}

/// The private ref namespace pull request fetches write to. It is outside
/// `refs/heads`, `refs/remotes` and `refs/tags`, so [`RepositoryService::refs_command`]
/// never lists it, no branch picker offers it, and a push cannot select it.
pub const PULL_REQUEST_REF_NAMESPACE: &str = "refs/cagents/pull";

/// The pull request number a private ref names, or `None` for anything that
/// is not a `<number>/head` or `<number>/base` ref in the namespace.
fn pull_request_ref_number(name: &str) -> Option<u64> {
    let rest = name
        .strip_prefix(PULL_REQUEST_REF_NAMESPACE)?
        .strip_prefix('/')?;
    let (number, kind) = rest.split_once('/')?;
    if !matches!(kind, "head" | "base") || number.starts_with('0') {
        return None;
    }
    number.parse().ok().filter(|number| *number != 0)
}

/// Whether a refused fetch says the remote has no such ref, as opposed to
/// a network, authentication, or signal failure that names no ref at all.
/// Git has reported a missing ref this way since long before anything this
/// application supports, and the message is not localized.
fn base_ref_is_missing(failure: &ProcessFailure) -> bool {
    failure.kind == ProcessFailureKind::Exit
        && String::from_utf8_lossy(&failure.stderr).contains("couldn't find remote ref")
}

/// The private refs one pull request's fetch owns.
pub fn pull_request_refs(
    number: u64,
) -> Result<(ValidatedRefName, ValidatedRefName), ValidationError> {
    if number == 0 {
        return Err(ValidationError::InvalidBase("pull request 0".into()));
    }
    Ok((
        ValidatedRefName::parse(format!("{PULL_REQUEST_REF_NAMESPACE}/{number}/head"))?,
        ValidatedRefName::parse(format!("{PULL_REQUEST_REF_NAMESPACE}/{number}/base"))?,
    ))
}

/// Independent source-control and file-browser demand. Neither collapsed changes
/// nor background branch/HEAD checks need an untracked scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotOptions {
    /// UI-only cap. Safety and full-group mutation reads remain uncapped.
    pub status_limit: Option<usize>,
    pub untracked_changes: bool,
    pub untracked_files: bool,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            status_limit: None,
            untracked_changes: true,
            untracked_files: true,
        }
    }
}

/// One bound panel's cache. Dropping it on a binding change releases retained
/// file lists and untracked measurements rather than keeping every visited tree.
#[derive(Debug, Default)]
pub struct SnapshotCache {
    digest: Option<[u8; 32]>,
    dirty: bool,
    fsmonitor: Option<OsString>,
    fsmonitor_checked: bool,
    options: Option<SnapshotOptions>,
    metadata: Option<(
        WorkingFiles,
        Vec<RemoteRecord>,
        Vec<LocalRefRecord>,
        Vec<RemoteRefRecord>,
        Vec<RemoteHead>,
    )>,
    control_metadata: Option<ControlMetadata>,
    lines: BTreeMap<RepoPath, (FileIdentity, u64)>,
    observed: BTreeMap<RepoPath, Option<FileIdentity>>,
    browse_root: Option<FileIdentity>,
    admin: BTreeMap<PathBuf, Option<FileIdentity>>,
}

#[derive(Debug, Clone)]
struct ControlMetadata {
    operation: Option<GitOperationState>,
    stashes: Vec<StashEntry>,
    last_fetch_unix_ms: Option<u64>,
}

/// Resolve administrative metadata locally, including linked worktree gitfiles.
fn local_git_dir(worktree: &Path) -> Option<PathBuf> {
    let path = worktree.join(".git");
    if path.is_dir() {
        return Some(path);
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(4097)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    let value = bytes.strip_prefix(b"gitdir: ")?;
    let value = value.strip_suffix(b"\n").unwrap_or(value);
    let value = value.strip_suffix(b"\r").unwrap_or(value);
    use std::os::unix::ffi::OsStrExt;
    let directory = Path::new(OsStr::from_bytes(value));
    Some(if directory.is_absolute() {
        directory.to_owned()
    } else {
        worktree.join(directory)
    })
}

type FileIdentity = (u64, u64, u64, i64, i64, i64, i64);

fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

/// Filesystem-only metadata observations keep the missed-event fallback
/// authoritative without extra Git commands on an unchanged snapshot.
fn snapshot_admin_fingerprint(worktree: &Path) -> BTreeMap<PathBuf, Option<FileIdentity>> {
    let read_path = |path: &Path| -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        fs::File::open(path)
            .ok()?
            .take(4097)
            .read_to_end(&mut bytes)
            .ok()?;
        (bytes.len() <= 4096).then_some(bytes)
    };
    let marker = worktree.join(".git");
    let git_dir = if marker.is_dir() {
        marker.clone()
    } else {
        let Some(bytes) = read_path(&marker) else {
            return BTreeMap::new();
        };
        let Some(relative) = bytes.strip_prefix(b"gitdir: ") else {
            return BTreeMap::new();
        };
        worktree.join(bytes_to_path(trim_one_line_ending(relative)))
    };
    let git_dir = fs::canonicalize(&git_dir).unwrap_or(git_dir);
    let common = read_path(&git_dir.join("commondir"))
        .map(|bytes| git_dir.join(bytes_to_path(trim_one_line_ending(&bytes))))
        .and_then(|path| fs::canonicalize(path).ok())
        .unwrap_or_else(|| git_dir.clone());
    let mut paths = BTreeMap::new();
    for root in [&git_dir, &common] {
        // Lock creation/removal changes directory times without changing Git
        // state. For the admin roots only their directory identity matters.
        let identity = fs::metadata(root).ok().map(|metadata| {
            let (device, inode, ..) = file_identity(&metadata);
            (device, inode, 0, 0, 0, 0, 0)
        });
        paths.insert(root.clone(), identity);
    }
    for name in [
        "HEAD",
        "index",
        "commondir",
        "config.worktree",
        "FETCH_HEAD",
        "MERGE_HEAD",
        "MERGE_MSG",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-merge/message",
        "rebase-merge/onto",
        "rebase-apply",
        "rebase-apply/final-commit",
        "rebase-apply/onto",
    ] {
        let path = git_dir.join(name);
        paths.insert(
            path.clone(),
            fs::metadata(path)
                .ok()
                .map(|metadata| file_identity(&metadata)),
        );
    }
    for name in [
        "config",
        "packed-refs",
        "info/exclude",
        "refs/stash",
        "logs/refs/stash",
    ] {
        let path = common.join(name);
        paths.insert(
            path.clone(),
            fs::metadata(path)
                .ok()
                .map(|metadata| file_identity(&metadata)),
        );
    }
    let mut pending = vec![common.join("refs"), git_dir.join("refs")];
    while let Some(path) = pending.pop() {
        if paths.contains_key(&path) {
            continue;
        }
        // Child identities below detect real ref additions/removals. Ignore
        // directory times, which also change for unpublished *.lock files.
        let identity = fs::metadata(&path).ok().map(|metadata| {
            let (device, inode, ..) = file_identity(&metadata);
            (device, inode, 0, 0, 0, 0, 0)
        });
        paths.insert(path.clone(), identity);
        if let Ok(entries) = fs::read_dir(&path) {
            for entry in entries.flatten() {
                if entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "lock")
                {
                    continue;
                }
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    pending.push(entry.path());
                } else {
                    let path = entry.path();
                    paths.insert(
                        path.clone(),
                        fs::symlink_metadata(path)
                            .ok()
                            .map(|metadata| file_identity(&metadata)),
                    );
                }
            }
        }
    }
    paths
}

impl SnapshotCache {
    /// Ignored-folder expansion is independent of porcelain status. Observe
    /// its directory membership too, so the slow fallback repairs missed events.
    pub fn observe_browse(&mut self, worktree: &Path, files: &WorkingFiles) {
        let Ok(root) = fs_safety::Root::open(worktree) else {
            return;
        };
        self.browse_root = fs::symlink_metadata(worktree)
            .ok()
            .map(|metadata| file_identity(&metadata));
        for path in files
            .paths
            .iter()
            .chain(&files.ignored)
            .chain(&files.directories)
            .chain(&files.ignored_directories)
        {
            for ancestor in path.as_path().ancestors() {
                let Ok(path) = RepoPath::parse(ancestor) else {
                    continue;
                };
                if self.observed.contains_key(&path) {
                    continue;
                }
                let metadata = fs_safety::metadata_beneath(&root, path.as_path())
                    .ok()
                    .map(|metadata| file_identity(&metadata));
                self.observed.insert(path, metadata);
            }
        }
    }

    pub fn invalidate(&mut self, metadata: bool) {
        self.dirty = true;
        if metadata {
            self.metadata = None;
            self.control_metadata = None;
            self.fsmonitor_checked = false;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StatusSnapshot {
    /// This read deliberately skipped untracked files; zero is not authoritative.
    #[serde(default)]
    pub untracked_omitted: bool,
    /// The listed entries are a lower bound, never a complete safety snapshot.
    #[serde(default)]
    pub listing_truncated: bool,
    pub head: HeadState,
    pub tracking: TrackingState,
    pub dirty: DirtyState,
    /// Every listed changed path, one entry per area. `dirty` is derived from
    /// these same records. When `listing_truncated` is set, both are lower
    /// bounds and must not authorize a destructive or whole-group operation.
    #[serde(default)]
    pub entries: Vec<ChangedPath>,
}

impl StatusSnapshot {
    /// Entries in one area, in Git's reporting order.
    pub fn area(&self, area: ChangeArea) -> impl Iterator<Item = &ChangedPath> {
        self.entries.iter().filter(move |entry| entry.area == area)
    }

    /// The checked-out branch, or `None` when HEAD is detached or unknown.
    pub fn branch(&self) -> Option<&str> {
        match &self.head {
            HeadState::Branch { name, .. } => Some(name),
            HeadState::Unborn { branch } => branch.as_deref(),
            HeadState::Detached { .. } | HeadState::Unknown => None,
        }
    }
}

/// One row of the synthesized file tree.
///
/// Directory rows exist only in memory: they are derived from the paths Git
/// listed. Shallow filesystem listings can supply explicit directory leaves
/// without recursively walking their descendants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTreeEntry {
    pub path: RepoPath,
    /// Number of ancestors, for indentation in a flat virtualized list.
    pub depth: usize,
    pub directory: bool,
}

#[derive(Debug, Default)]
struct FileTreeNode {
    children: BTreeMap<Vec<u8>, FileTreeNode>,
    file: bool,
}

/// Expands a flat path list into a pre-order tree of directory and file rows,
/// directories first and byte-sorted within each level.
pub fn build_file_tree(paths: &[RepoPath]) -> Vec<FileTreeEntry> {
    build_file_tree_with_directories(paths, &[])
}

/// Same tree as [`build_file_tree`], plus directory-only leaves that have no
/// files listed yet. Used by shallow folder listings so collapsed trees can
/// show a folder before its children have been read.
pub fn build_file_tree_with_directories(
    files: &[RepoPath],
    directories: &[RepoPath],
) -> Vec<FileTreeEntry> {
    let mut root = FileTreeNode::default();
    for path in files {
        let mut node = &mut root;
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            node = node.children.entry(component.to_vec()).or_default();
            if components.peek().is_none() {
                node.file = true;
            }
        }
    }
    for path in directories {
        let mut node = &mut root;
        for component in path.components() {
            node = node.children.entry(component.to_vec()).or_default();
        }
    }
    let mut rows = Vec::with_capacity(files.len() + directories.len());
    emit_file_tree(&root, &mut Vec::new(), 0, &mut rows);
    rows
}

fn emit_file_tree(
    node: &FileTreeNode,
    prefix: &mut Vec<u8>,
    depth: usize,
    rows: &mut Vec<FileTreeEntry>,
) {
    for directory in [true, false] {
        for (name, child) in
            node.children.iter().filter(
                |(_, child)| {
                    if directory { !child.file } else { child.file }
                },
            )
        {
            let restore = prefix.len();
            if !prefix.is_empty() {
                prefix.push(b'/');
            }
            prefix.extend_from_slice(name);
            // Every component came from an already validated `RepoPath`, so a
            // rejection here is impossible; skipping rather than panicking
            // keeps a future validation change from taking the tree down.
            if let Ok(path) = RepoPath::from_bytes(prefix) {
                rows.push(FileTreeEntry {
                    path,
                    depth,
                    directory,
                });
            }
            if directory {
                emit_file_tree(child, prefix, depth + 1, rows);
            }
            prefix.truncate(restore);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRecord {
    pub name: String,
    pub fetch_url: Option<String>,
    /// Set only when `remote.<name>.pushurl` overrides the fetch URL.
    pub push_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalRefRecord {
    /// The full ref name, retained so it can be used as a revision argument.
    pub name: ValidatedRefName,
    pub oid: Oid,
    /// The configured upstream's full ref name, when there is one.
    pub upstream: Option<ValidatedRefName>,
    pub head: bool,
}

/// One remote-tracking branch.
///
/// A symbolic ref is never one of these: see [`RemoteHead`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRefRecord {
    pub name: ValidatedRefName,
    pub oid: Oid,
    /// Remote name parsed out of `refs/remotes/<remote>/<branch>`, split at
    /// the same point [`RemoteHead::remote`] is, so a remote whose own name
    /// contains a slash is named identically by both and the two can be
    /// joined on this field. `git remote add te/am` is legal, and taking the
    /// first component alone gave `te` here against `te/am` there.
    pub remote: String,
    pub branch: String,
}

/// A remote's default branch: the target of `refs/remotes/<remote>/HEAD`.
///
/// That ref is a symbolic ref rather than a branch — it has no commit of its
/// own and every clone has one — so it is reported here instead of among the
/// remote-tracking branches, where it would otherwise appear as a branch
/// literally named `HEAD` beside the branch it points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHead {
    /// The remote this pointer belongs to, taken as everything between
    /// `refs/remotes/` and the trailing `/HEAD`, so a remote whose own name
    /// contains a slash is named in full. This is the unambiguous half of the
    /// pair: `HEAD` is always the last component, while
    /// [`RemoteRefRecord::remote`] has to be split against the remote names
    /// these entries establish.
    pub remote: String,
    /// The remote-tracking ref it points at. The ref need not exist: a
    /// symbolic ref survives the deletion of its target.
    pub target: ValidatedRefName,
}

/// Everything one `for-each-ref` walk yields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefListing {
    pub local: Vec<LocalRefRecord>,
    pub remote: Vec<RemoteRefRecord>,
    pub remote_heads: Vec<RemoteHead>,
}

/// The files one worktree offers, split by whether Git tracks them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkingFiles {
    /// A UI listing cap omitted paths; absence is not authoritative.
    #[serde(default)]
    pub listing_truncated: bool,
    /// Tracked and untracked paths together, excluding ignored ones,
    /// deduplicated and sorted: the file tree shows both.
    pub paths: Vec<RepoPath>,
    /// Existing explicit directories, including a revealed empty parent.
    #[serde(default)]
    pub directories: Vec<RepoPath>,
    /// How many of `paths` Git tracks; the remainder are untracked. A count
    /// rather than a second list, because nothing needs the tracked paths on
    /// their own and duplicating the listing of a large worktree to answer one
    /// number would not be free.
    pub tracked_count: usize,
    /// Ignored entries for file browsing, read separately from source control.
    #[serde(default)]
    pub ignored: Vec<RepoPath>,
    #[serde(default)]
    pub ignored_directories: Vec<RepoPath>,
    /// Failed lazy folder reads, retained until the user explicitly retries.
    #[serde(default)]
    pub ignored_directory_errors: std::collections::BTreeMap<RepoPath, String>,
}

/// A sequenced operation in the worktree's administrative directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitOperationKind {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl GitOperationKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Merge => "Merge",
            Self::Rebase => "Rebase",
            Self::CherryPick => "Cherry-pick",
            Self::Revert => "Revert",
        }
    }
    fn command(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitOperationState {
    pub kind: GitOperationKind,
    pub incoming_ref: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashEntry {
    pub reference: String,
    pub oid: Oid,
    pub message: String,
}

/// The HEAD state a rendered patch was prepared against. No commit is a real
/// precondition for an unborn repository, rather than an absent guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHeadGuard {
    Unborn,
    Commit(Oid),
}

/// Commands share cancellation, ordering, failure output and refresh semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitOperation {
    Fetch {
        remote: String,
        prune: bool,
    },
    Sync {
        rebase: bool,
    },
    StashPush {
        message: Option<String>,
        include_untracked: bool,
    },
    StashApply {
        reference: String,
        expected_oid: Oid,
        pop: bool,
    },
    StashDrop {
        reference: String,
        expected_oid: Oid,
    },
    ResolveConflict {
        path: RepoPath,
        ours: bool,
    },
    Continue {
        kind: GitOperationKind,
    },
    Abort {
        kind: GitOperationKind,
    },
    ApplyPatch {
        patch: Arc<[u8]>,
        cached: bool,
        reverse: bool,
        expected_head: Option<GitHeadGuard>,
    },
    RevertCommit {
        oid: Oid,
        mainline: Option<u32>,
    },
    CherryPick {
        oid: Oid,
        mainline: Option<u32>,
    },
    CreateBranch {
        oid: Oid,
        name: ValidatedBranchName,
    },
    IgnorePath { path: RepoPath },
}

impl GitOperation {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Fetch { .. } => "Fetch",
            Self::Sync { .. } => "Sync",
            Self::StashPush { .. } => "Stash",
            Self::StashApply { pop: true, .. } => "Pop stash",
            Self::StashApply { .. } => "Apply stash",
            Self::StashDrop { .. } => "Drop stash",
            Self::ResolveConflict { .. } => "Resolve conflict",
            Self::Continue { .. } => "Continue",
            Self::Abort { .. } => "Abort",
            Self::ApplyPatch { .. } => "Apply patch",
            Self::RevertCommit { .. } => "Revert commit",
            Self::CherryPick { .. } => "Cherry-pick",
            Self::CreateBranch { .. } => "Create branch",
            Self::IgnorePath { .. } => "Add to .gitignore",
        }
    }
}

/// Identity of the exact file contents presented in a marker confirmation.
pub type ConflictMarkerVersion = (u64, u64, u64, i64, i64, i64, i64);

/// Recognize complete or incomplete conflict marker blocks conservatively.
pub fn has_conflict_markers(bytes: &[u8]) -> bool {
    bytes.split(|byte| *byte == b'\n').any(|line| {
        [b"<<<<<<<".as_slice(), b"|||||||", b"=======", b">>>>>>>"]
            .iter()
            .any(|marker| line.starts_with(marker))
    })
}

/// Scan line prefixes in bounded chunks so even a very large conflicted file
/// cannot hide markers beyond an editor preview limit or allocate a giant line.
pub fn scan_conflict_markers(
    reader: &mut impl Read,
    cancellation: &CancellationToken,
) -> io::Result<bool> {
    let mut chunk = [0u8; 64 * 1024];
    let mut prefix = [0u8; 7];
    let mut used = 0;
    loop {
        if cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "conflict marker scan cancelled",
            ));
        }
        let length = reader.read(&mut chunk)?;
        if length == 0 {
            return Ok(false);
        }
        for byte in &chunk[..length] {
            if *byte == b'\n' {
                used = 0;
            } else if used < prefix.len() {
                prefix[used] = *byte;
                used += 1;
                if used == prefix.len() && has_conflict_markers(&prefix) {
                    return Ok(true);
                }
            }
        }
    }
}

/// Everything the source-control surfaces need about one worktree in one shot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySnapshot {
    #[serde(default)]
    pub operation: Option<GitOperationState>,
    #[serde(default)]
    pub stashes: Vec<StashEntry>,
    #[serde(default)]
    pub last_fetch_unix_ms: Option<u64>,
    pub status: StatusSnapshot,
    pub files: WorkingFiles,
    pub remotes: Vec<RemoteRecord>,
    pub local_refs: Vec<LocalRefRecord>,
    /// Real remote-tracking branches only.
    pub remote_refs: Vec<RemoteRefRecord>,
    /// One entry per remote whose `HEAD` pointer this repository has fetched.
    pub remote_heads: Vec<RemoteHead>,
}

impl RepositorySnapshot {
    /// The default branch a remote advertises, or `None` when this repository
    /// has no `refs/remotes/<remote>/HEAD` — which a remote added by hand,
    /// rather than by cloning, never gets.
    pub fn remote_head(&self, remote: &str) -> Option<&ValidatedRefName> {
        self.remote_heads
            .iter()
            .find(|head| head.remote == remote)
            .map(|head| &head.target)
    }

    /// The branch comparison base to open with, or `None` when nothing
    /// resolves.
    ///
    /// The upstream's symbolic remote HEAD is tried first, then
    /// `refs/remotes/origin/HEAD`. A name is never guessed: without a
    /// resolvable symbolic HEAD the caller must show the ref selector.
    #[must_use]
    pub fn default_branch_base(&self) -> Option<&ValidatedRefName> {
        let upstream_remote = self
            .local_refs
            .iter()
            .find(|record| record.head)
            .and_then(|record| record.upstream.as_ref())
            .and_then(|upstream| {
                self.remote_refs
                    .iter()
                    .find(|record| record.name == *upstream)
                    .map(|record| record.remote.as_str())
            });
        upstream_remote
            .and_then(|remote| self.remote_head(remote))
            .or_else(|| self.remote_head("origin"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryRevision {
    #[default]
    Head,
    Reference {
        name: ValidatedRefName,
    },
    Object {
        oid: Oid,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRequest {
    pub worktree: PathBuf,
    pub revision: HistoryRevision,
    /// Zero-based page; each page holds [`HISTORY_PAGE_SIZE`] commits.
    pub page: u32,
    /// Restricts the walk to one path when set.
    pub path: Option<RepoPath>,
}

/// An author or committer signature with its own recorded UTC offset, so a
/// commit renders in the timezone it was made in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitIdentity {
    pub name: String,
    pub email: String,
    /// Seconds since the Unix epoch.
    pub timestamp: i64,
    /// Offset from UTC in minutes, as recorded in the commit.
    pub offset_minutes: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSummary {
    pub oid: Oid,
    /// First parent first; empty for a root commit.
    pub parents: Vec<Oid>,
    pub author: CommitIdentity,
    pub committer: CommitIdentity,
    pub subject: String,
    /// Message body after the subject, with trailing newlines removed.
    pub body: String,
}

/// Which pair of trees a comparison is between.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub enum DiffBasis {
    /// Index to worktree — the unstaged view.
    IndexToWorktree,
    /// HEAD to index — the staged view.
    HeadToIndex,
    /// An immutable merge-base-to-HEAD range; `base_ref` is its display label.
    MergeBaseToHead {
        base_ref: ValidatedRefName,
        head: Oid,
        merge_base: Oid,
    },
    /// One commit against a chosen parent; `None` for a root commit.
    CommitToParent { oid: Oid, parent: Option<Oid> },
    /// A pull request's merge base to its head, both fetched into the private
    /// namespace [`RepositoryService::fetch_pull_request_range`] owns. It is
    /// immutable like [`Self::MergeBaseToHead`], but its head is not HEAD and
    /// its chrome names the pull request rather than the checked-out branch.
    PullRequest {
        number: u64,
        base_ref: ValidatedRefName,
        head: Oid,
        merge_base: Oid,
    },
}

impl DiffBasis {
    /// Whether the comparison names immutable objects, so a working-tree
    /// mutation or a background refresh can neither change nor invalidate it.
    pub fn is_immutable(&self) -> bool {
        match self {
            Self::IndexToWorktree | Self::HeadToIndex => false,
            Self::MergeBaseToHead { .. }
            | Self::CommitToParent { .. }
            | Self::PullRequest { .. } => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRequest {
    pub worktree: PathBuf,
    pub basis: DiffBasis,
    pub path: RepoPath,
    /// Rename or copy source; the old side reads it instead of `path`.
    pub old_path: Option<RepoPath>,
}

/// One side of a comparison.
///
/// Only [`DiffSide::Text`] is UTF-8 content. Bytes are never decoded lossily:
/// a non-UTF-8 or binary side is reported as metadata so the UI shows what it
/// is instead of an editable buffer full of replacement characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSide {
    /// The path does not exist on this side: added, deleted, or untracked.
    Absent,
    Text {
        text: String,
        bytes: u64,
    },
    Binary {
        bytes: u64,
    },
    InvalidUtf8 {
        bytes: u64,
    },
    /// Larger than [`MAX_DIFF_SIDE_BYTES`]; never partially read.
    Oversize {
        bytes: u64,
    },
    /// A gitlink: the recorded commit of a submodule, not a blob.
    Submodule {
        oid: Option<Oid>,
    },
    Symlink {
        target: PathBuf,
    },
    /// A worktree entry that is neither a regular file nor a symlink.
    NotRegularFile,
    Unreadable {
        reason: String,
    },
}

impl DiffSide {
    /// UTF-8 content, when this side has any.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text { text, .. } => Some(text),
            _ => None,
        }
    }

    /// True when a text diff can be computed from this side; [`Self::Absent`]
    /// counts, because an added or deleted file compares against nothing.
    pub fn is_diffable(&self) -> bool {
        matches!(self, Self::Absent | Self::Text { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffContent {
    Comparison {
        old: DiffSide,
        new: DiffSide,
        /// Human-readable side labels, e.g. `HEAD` and `Index`.
        old_label: String,
        new_label: String,
        /// HEAD used by this comparison, including an explicitly unborn HEAD.
        /// Index/worktree and immutable commit comparisons need no HEAD guard.
        source_head: Option<GitHeadGuard>,
        /// Git modes read alongside each side, not inferred from text presence.
        modes: [Option<u32>; 2],
    },
    /// An unmerged path, read from index stages 1, 2 and 3 plus the worktree.
    /// A missing stage-0 blob is never reported as an empty added side.
    Unmerged {
        base: DiffSide,
        ours: DiffSide,
        theirs: DiffSide,
        worktree: DiffSide,
    },
}

/// A path chosen for a mutation, carrying its rename source so both halves of
/// a rename are acted on together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathSelection {
    pub path: RepoPath,
    pub old_path: Option<RepoPath>,
}

impl PathSelection {
    pub fn new(path: RepoPath) -> Self {
        Self {
            path,
            old_path: None,
        }
    }

    pub fn from_changed_path(entry: &ChangedPath) -> Self {
        Self {
            path: entry.path.clone(),
            old_path: entry.old_path.clone(),
        }
    }
}

/// Flattens a selection into the pathspec list a Git mutation receives.
///
/// A rename contributes both halves — staging only the destination would leave
/// the source's deletion behind — and duplicates are removed so overlapping
/// selections cannot list a path twice.
pub fn selection_pathspecs(selection: &[PathSelection]) -> Vec<RepoPath> {
    let mut paths = Vec::with_capacity(selection.len() * 2);
    let mut seen = HashSet::new();
    for entry in selection {
        for path in [entry.old_path.as_ref(), Some(&entry.path)]
            .into_iter()
            .flatten()
        {
            if seen.insert(path.clone()) {
                paths.push(path.clone());
            }
        }
    }
    paths
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageRequest {
    pub worktree: PathBuf,
    pub selection: Vec<PathSelection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnstageRequest {
    pub worktree: PathBuf,
    pub selection: Vec<PathSelection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscardRequest {
    pub worktree: PathBuf,
    /// Tracked paths, restored from the index.
    pub tracked: Vec<PathSelection>,
    /// Untracked paths, deleted from the worktree.
    pub untracked: Vec<RepoPath>,
    /// The user's explicit confirmation. Discard destroys work that exists
    /// nowhere else, so the service refuses to run without it.
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRequest {
    pub worktree: PathBuf,
    pub message: String,
}

/// A ref name as the *remote* knows it: the right-hand side of a push refspec,
/// and the ref argument to `git ls-remote`.
///
/// The distinction this type exists to hold is between that name and the
/// remote-tracking ref this clone keeps for the same branch.
/// `refs/remotes/origin/main` is this repository's own bookkeeping — the
/// remote calls that branch `refs/heads/main` and has never heard the other
/// name. Putting the tracking name on the wire is not a mistake Git can
/// report: `git push origin refs/heads/main:refs/remotes/origin/main` creates
/// a literal `refs/remotes/origin/main` *in the remote repository*, reports
/// the update as successful, exits 0, and leaves the branch exactly where it
/// was. Every outcome check therefore passes while nothing was published, and
/// the only symptom is that the branch stays "1 ahead" forever.
///
/// So no value of this type can be built except through a conversion that was
/// told which of the two spellings it was given, and neither `push_refspec`
/// nor [`RepositoryService::remote_ref_oid`] accepts anything else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RemoteSideRef(String);

impl RemoteSideRef {
    /// The remote-side name of a branch named directly, as on a first publish
    /// where the user chose the name the remote will use.
    pub fn branch(branch: &ValidatedBranchName) -> Self {
        Self(branch.full_ref())
    }

    /// A name already spelled the way the remote spells it.
    pub fn parse(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        let qualified = value
            .strip_prefix("refs/")
            .is_some_and(|rest| ValidatedBranchName::parse(rest).is_ok());
        if !qualified || value.starts_with("refs/remotes/") {
            return Err(ValidationError::NotARemoteSideRef(value));
        }
        Ok(Self(value))
    }

    /// What `remote` calls the ref this clone names `reference`.
    ///
    /// A `refs/remotes/...` name is split at the *known* remote name rather
    /// than at its first slash, because the name alone cannot say where the
    /// boundary is: with a remote called `te`, `refs/remotes/te/am/main` is the
    /// branch `am/main`, and with one called `te/am` it is `main`. The branch
    /// name that comes out is the upstream's, never the local branch's — an
    /// upstream of `refs/remotes/origin/published` for a local `work` is what a
    /// rename on either side leaves behind, and it publishes to `published`.
    ///
    /// Anything not under `refs/remotes/` is already a remote-side name and is
    /// taken as it stands: `branch.<name>.merge`, which is what Git records and
    /// what [`RepositoryService::upstream_push_target`] reads back, is spelled
    /// `refs/heads/<branch>`.
    pub fn resolve(remote: &str, reference: &ValidatedRefName) -> Result<Self, ValidationError> {
        let Some(rest) = reference.as_str().strip_prefix("refs/remotes/") else {
            return Self::parse(reference.as_str());
        };
        // A tracking ref belonging to a different remote names no branch on
        // this one, and guessing which branch it meant is precisely how a wrong
        // ref reaches the wire.
        let branch = rest
            .strip_prefix(remote)
            .and_then(|rest| rest.strip_prefix('/'))
            .ok_or_else(|| ValidationError::ForeignTrackingRef {
                remote: remote.to_owned(),
                reference: reference.as_str().to_owned(),
            })?;
        Ok(Self::branch(&ValidatedBranchName::parse(branch)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RemoteSideRef {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<RemoteSideRef> for String {
    fn from(value: RemoteSideRef) -> Self {
        value.0
    }
}

impl fmt::Display for RemoteSideRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum PushTarget {
    /// The branch already has an upstream; its remote and the upstream's ref
    /// are pushed explicitly rather than relying on push defaults.
    Upstream {
        remote: String,
        /// The upstream as *this clone* names it, which is either spelling:
        /// the remote-tracking ref `refs/remotes/<remote>/<branch>` that
        /// `%(upstream)` reports and the sidebar carries, or the remote-side
        /// `refs/heads/<branch>` that `branch.<name>.merge` holds. Neither is
        /// a name to put on the wire without going through
        /// [`RemoteSideRef::resolve`].
        remote_ref: ValidatedRefName,
    },
    /// First publish: the remote branch name is chosen by the user and the
    /// upstream is set as part of the push.
    SetUpstream {
        remote: String,
        remote_branch: ValidatedBranchName,
    },
}

impl PushTarget {
    pub fn remote(&self) -> &str {
        match self {
            Self::Upstream { remote, .. } | Self::SetUpstream { remote, .. } => remote,
        }
    }

    /// The ref this push updates, named the way the remote names it.
    pub fn remote_side_ref(&self) -> Result<RemoteSideRef, ValidationError> {
        match self {
            Self::Upstream { remote, remote_ref } => RemoteSideRef::resolve(remote, remote_ref),
            Self::SetUpstream { remote_branch, .. } => Ok(RemoteSideRef::branch(remote_branch)),
        }
    }

    pub fn sets_upstream(&self) -> bool {
        matches!(self, Self::SetUpstream { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRequest {
    pub worktree: PathBuf,
    /// The checked-out branch. A detached HEAD has none, and this service
    /// never guesses one.
    pub branch: ValidatedBranchName,
    pub target: PushTarget,
}

/// Why a mutation failed or its result could not be confirmed, keeping the raw evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MutationFailure {
    Process(ProcessFailure),
    /// A filesystem operation on one path failed, or the path failed its
    /// pre-delete validation.
    Path {
        path: RepoPath,
        reason: String,
    },
    /// Git completed the mutation, but a subsequent observation could not
    /// establish the specific repository state this operation would report.
    Reconciliation {
        reason: String,
    },
    /// The request itself was not runnable, e.g. nothing staged to commit.
    Rejected {
        reason: String,
    },
}

impl fmt::Display for MutationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Process(failure) => {
                write!(formatter, "{failure}")?;
                let output = failure.full_output();
                if !output.is_empty() {
                    write!(formatter, "\n\nGit output:\n{output}")?;
                }
                Ok(())
            }
            Self::Path { path, reason } => write!(formatter, "{path}: {reason}"),
            Self::Rejected { reason } | Self::Reconciliation { reason } => {
                formatter.write_str(reason)
            }
        }
    }
}

impl From<ProcessFailure> for MutationFailure {
    fn from(failure: ProcessFailure) -> Self {
        Self::Process(failure)
    }
}

/// Outcome of a stage, unstage, or discard.
///
/// Mirrors [`WorktreeCreationOutcome`]: a failure that never started the
/// process cannot have changed anything, while a failure after launch is only
/// reported as unknown when the service could not establish what happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MutationOutcome {
    Applied {
        /// Paths the mutation acted on, after rename expansion and
        /// deduplication.
        paths: Vec<RepoPath>,
    },
    /// Part of the request was applied and the rest was refused, with both
    /// halves known exactly.
    ///
    /// This is not [`Self::OutcomeUnknown`]: a discard deletes each untracked
    /// path through its own syscall, so one path Git will not let go of
    /// neither hides the deletions that already happened nor makes them
    /// uncertain. The caller can retry precisely the paths in `failures`.
    PartiallyApplied {
        paths: Vec<RepoPath>,
        failures: Vec<MutationFailure>,
    },
    NotApplied {
        failure: MutationFailure,
    },
    OutcomeUnknown {
        failure: MutationFailure,
    },
}

/// What a Git command's own non-zero exit says about how much of a multi-path
/// request it applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationScope {
    /// The command works through its pathspecs and reports the ones it could
    /// not handle at the end: `git add -A -- good ignored` exits 1 *having
    /// staged* `good`, so a non-zero exit leaves the split between the two
    /// halves unknown.
    Incremental,
    /// The command assembles its whole result in memory and commits it with a
    /// single write, so a non-zero exit means nothing was written at all:
    /// Path-limited `git reset --no-refresh` and `git update-index` each write
    /// the index once, after the last selected path, under `index.lock`.
    /// That single write says nothing about unmatched pathspecs — `reset`
    /// ignores those and still exits 0 — so a caller that must refuse ghosts
    /// validates the selection first (see `unstage`).
    AllOrNothing,
    /// The command resolves every pathspec before it writes anything, and
    /// then writes entry by entry.
    ///
    /// `git restore --worktree` refuses a pathspec that matches nothing, or a
    /// path that is unmerged, without having restored any of the others, in
    /// either pathspec order — that half really is all-or-nothing. Its
    /// checkout loop is not: it walks the index ORing each entry's error into
    /// one return value, so a failure raised *inside* it — an unwritable
    /// parent directory, a blob a partial clone does not have — still leaves
    /// every entry the loop had already reached overwritten, and the entry it
    /// failed on unlinked and not put back. Reporting "this did not happen"
    /// while the user's uncommitted work is gone is the one thing the one
    /// operation that destroys work must never do.
    AtomicRefusal,
}

impl MutationScope {
    /// Classifies a non-zero exit from a command of this scope.
    ///
    /// Git's own exit code separates [`Self::AtomicRefusal`]'s two modes: a
    /// refusal is an `error()` and exit 1, while a failure inside the write
    /// loop is a negative return that reaches `exit` as 255, or a `die()` as
    /// 128. Anything that is not the refusal's own 1 is therefore read as
    /// possibly-written, which is both the safe direction and one that does
    /// not tie this to the wording of a Git diagnostic.
    fn classify(self, exit_code: Option<i32>, failure: MutationFailure) -> MutationOutcome {
        match self {
            Self::Incremental => MutationOutcome::OutcomeUnknown { failure },
            Self::AllOrNothing => MutationOutcome::NotApplied { failure },
            Self::AtomicRefusal if exit_code == Some(1) => MutationOutcome::NotApplied { failure },
            Self::AtomicRefusal => MutationOutcome::OutcomeUnknown { failure },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CommitOutcome {
    Created {
        oid: Oid,
        /// A post-commit failure, such as a failing hook, that did not prevent
        /// the commit HEAD now points at.
        warnings: Vec<MutationFailure>,
    },
    NotCreated {
        failure: MutationFailure,
    },
    OutcomeUnknown {
        failure: MutationFailure,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PushOutcome {
    Pushed {
        remote: String,
        /// The ref the *remote* moved, named the way the remote names it —
        /// this is what the user is told was published, so a tracking ref here
        /// would be a report of something that never happened.
        remote_ref: RemoteSideRef,
        /// The local commit that is now on the remote.
        oid: Oid,
    },
    NotPushed {
        failure: MutationFailure,
    },
    OutcomeUnknown {
        failure: MutationFailure,
    },
}

#[derive(Debug, Default)]
struct KeyedLock {
    held: Mutex<bool>,
    released: Condvar,
}

/// A key's lock and how many guards and waiters use it. The count changes
/// only under the registry's own lock, so exactly the last user to leave
/// removes the key.
#[derive(Debug, Default)]
struct Registered {
    lock: Arc<KeyedLock>,
    users: usize,
}

type Registry = Arc<Mutex<BTreeMap<PathBuf, Registered>>>;

/// Serializes mutations per Git common directory.
///
/// Two `git add` runs against one index corrupt each other's `index.lock`, but
/// a read of an unrelated repository — or of the same one — has no reason to
/// wait. Keying on the common directory is what makes that distinction: every
/// linked worktree of one repository shares an index lock and therefore one
/// key, while separate repositories never contend.
#[derive(Debug, Clone, Default)]
pub struct MutationLocks(Registry);

/// Serializes [`RepositoryService::fetch_pull_request_range`] per repository.
/// The service is constructed per call, so the registry lives for the process.
static PULL_REQUEST_FETCH_LOCKS: LazyLock<MutationLocks> = LazyLock::new(MutationLocks::new);

impl MutationLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Blocks until no other holder owns `common_dir`, then returns the guard
    /// that must be held for the whole mutation.
    pub fn lock(&self, common_dir: &Path) -> MutationGuard {
        let entry = {
            let mut registry = lock_registry(&self.0);
            let registered = registry.entry(common_dir.to_path_buf()).or_default();
            registered.users += 1;
            Arc::clone(&registered.lock)
        };
        let mut held = entry
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *held {
            held = entry
                .released
                .wait(held)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *held = true;
        drop(held);
        MutationGuard {
            registry: Arc::clone(&self.0),
            key: common_dir.to_path_buf(),
            entry,
        }
    }

    /// Number of tracked keys, for tests.
    #[cfg(test)]
    fn tracked_keys(&self) -> usize {
        lock_registry(&self.0).len()
    }
}

/// Held for the duration of one mutation; releasing it wakes one waiter.
#[derive(Debug)]
pub struct MutationGuard {
    registry: Registry,
    key: PathBuf,
    entry: Arc<KeyedLock>,
}

impl Drop for MutationGuard {
    fn drop(&mut self) {
        let mut held = self
            .entry
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *held = false;
        drop(held);
        self.entry.released.notify_one();
        // The registry entry is only worth keeping while somebody is waiting on
        // it; dropping the last one keeps a long session from accumulating a
        // key per repository it ever touched. The count is explicit because
        // the `Arc` count is not: a guard's own reference outlives this body,
        // so two guards leaving together could each see the other and both
        // keep the key.
        let mut registry = lock_registry(&self.registry);
        if let Some(registered) = registry.get_mut(&self.key) {
            registered.users -= 1;
            if registered.users == 0 {
                registry.remove(&self.key);
            }
        }
    }
}

/// A panic while holding this registry cannot leave the map inconsistent, and
/// refusing to lock afterwards would deadlock every later mutation.
fn lock_registry(
    registry: &Mutex<BTreeMap<PathBuf, Registered>>,
) -> MutexGuard<'_, BTreeMap<PathBuf, Registered>> {
    registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GitParseError {
    #[error("truncated NUL-delimited {format} output")]
    Truncated { format: &'static str },
    #[error("malformed {format} record {record}: {reason}")]
    Malformed {
        format: &'static str,
        record: usize,
        reason: String,
    },
    #[error("counter overflow while parsing {format}")]
    CounterOverflow { format: &'static str },
}

pub fn parse_worktree_list(output: &[u8]) -> Result<Vec<WorktreeRecord>, GitParseError> {
    let fields = nul_fields(output, "git worktree list")?;
    let mut records = Vec::new();
    let mut current: Option<WorktreeRecord> = None;

    for (index, field) in fields.iter().enumerate() {
        if field.is_empty() {
            if let Some(record) = current.take() {
                records.push(record);
            }
            continue;
        }
        let (key, value) = split_once_space(field);
        match key {
            b"worktree" => {
                if current.is_some() {
                    return Err(malformed(
                        "git worktree list",
                        index,
                        "missing record separator",
                    ));
                }
                let value = value.filter(|value| !value.is_empty()).ok_or_else(|| {
                    malformed("git worktree list", index, "worktree path is missing")
                })?;
                let path = bytes_to_path(value);
                current = Some(WorktreeRecord {
                    id: stable_worktree_id(&path),
                    path,
                    created_at_unix_ms: None,
                    head_oid: None,
                    branch: None,
                    detached: false,
                    bare: false,
                    lock: WorktreeLock::default(),
                    prunable_reason: None,
                });
            }
            b"HEAD" => {
                let record = current.as_mut().ok_or_else(|| {
                    malformed("git worktree list", index, "HEAD appeared before worktree")
                })?;
                let oid = utf8_value(value, "git worktree list", index, "HEAD")?;
                if !is_hex_oid(oid) {
                    return Err(malformed(
                        "git worktree list",
                        index,
                        "invalid HEAD object id",
                    ));
                }
                if oid != ZERO_SHA1 && !oid.bytes().all(|byte| byte == b'0') {
                    record.head_oid = Some(oid.into());
                }
            }
            b"branch" => {
                let record = current.as_mut().ok_or_else(|| {
                    malformed(
                        "git worktree list",
                        index,
                        "branch appeared before worktree",
                    )
                })?;
                let value = value
                    .ok_or_else(|| malformed("git worktree list", index, "branch is missing"))?;
                // A ref name is an arbitrary byte string and this label is
                // shown, never handed back to Git — the same standing as
                // `branch.head` in `git status`. Forcing it through a
                // conversion that can fail let one worktree checked out on an
                // undecodable branch fail the entire listing, and with it the
                // project load, `discover`, and every worktree reconciliation
                // that reads the list back.
                record.branch = Some(String::from_utf8_lossy(value).into_owned());
            }
            b"detached" => current_record(&mut current, index)?.detached = true,
            b"bare" => current_record(&mut current, index)?.bare = true,
            b"locked" => {
                let record = current_record(&mut current, index)?;
                record.lock.locked = true;
                record.lock.reason = optional_utf8(value, "git worktree list", index)?;
            }
            b"prunable" => {
                current_record(&mut current, index)?.prunable_reason =
                    optional_utf8(value, "git worktree list", index)?
                        .or_else(|| Some(String::new()));
            }
            _ => {
                if current.is_none() {
                    return Err(malformed(
                        "git worktree list",
                        index,
                        "field appeared before worktree",
                    ));
                }
                // Porcelain output may gain fields. Unknown fields are scoped to the
                // current record and can be ignored safely.
            }
        }
    }
    if current.is_some() {
        return Err(GitParseError::Truncated {
            format: "git worktree list",
        });
    }
    Ok(records)
}

pub fn parse_status(output: &[u8]) -> Result<StatusSnapshot, GitParseError> {
    let fields = nul_fields(output, "git status")?;
    let mut oid: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut saw_oid = false;
    let mut saw_head = false;
    let mut upstream: Option<String> = None;
    let mut ahead_behind: Option<(u64, u64)> = None;
    let mut dirty = DirtyState::default();
    let mut entries = Vec::new();
    let mut index = 0;

    while index < fields.len() {
        let field = fields[index];
        if field.is_empty() {
            index += 1;
            continue;
        }
        if let Some(header) = field.strip_prefix(b"# ") {
            if let Some(value) = header.strip_prefix(b"branch.oid ") {
                saw_oid = true;
                let value = utf8(value, "git status", index, "branch.oid")?;
                if value == "(initial)" {
                    oid = None;
                } else if is_hex_oid(value) {
                    oid = Some(value.into());
                } else {
                    return Err(malformed("git status", index, "invalid branch.oid"));
                }
            } else if let Some(value) = header.strip_prefix(b"branch.head ") {
                saw_head = true;
                // A ref name is an arbitrary NUL- and LF-free byte string, not
                // UTF-8, and `git checkout -b` accepts one that no encoding
                // decodes. These two labels are display data — the branch is
                // shown, and the upstream is shown beside its ahead/behind
                // counts — so an undecodable byte renders as U+FFFD instead of
                // discarding every changed path in the snapshot with it.
                //
                // The exact bytes are deliberately not kept alongside, because
                // acting on the label is not what stops a lossy name from
                // naming the wrong ref. `ValidatedBranchName::parse` screens
                // ASCII only — U+FFFD contains no byte it forbids, so the
                // lossy name parses — and what actually refuses it is the
                // existence check every branch operation makes first:
                // `Self::push` resolves `refs/heads/<name>` through
                // `resolve_optional_revision` and reports "does not exist"
                // before a `git push` is ever spawned, because
                // `refs/heads/b<U+FFFD>ad` is not a ref this repository has.
                // That check is load-bearing, not incidental, and
                // `a_lossy_branch_label_cannot_act_on_the_branch_it_names`
                // pins it.
                branch = Some(String::from_utf8_lossy(value).into_owned());
            } else if let Some(value) = header.strip_prefix(b"branch.upstream ") {
                upstream = Some(String::from_utf8_lossy(value).into_owned());
            } else if let Some(value) = header.strip_prefix(b"branch.ab ") {
                ahead_behind = Some(parse_ahead_behind(value, index)?);
            }
            index += 1;
            continue;
        }

        match field[0] {
            b'1' => {
                let record = parse_changed_record(field, index, 9)?;
                push_changed_entries(&mut entries, &mut dirty, &record, None, index)?;
            }
            b'2' => {
                let record = parse_changed_record(field, index, 10)?;
                // The rename or copy source is its own NUL-delimited field
                // immediately after the record.
                index += 1;
                let source = fields.get(index).copied().ok_or_else(|| {
                    malformed("git status", index, "rename source path is missing")
                })?;
                if source.is_empty() {
                    return Err(malformed(
                        "git status",
                        index,
                        "rename source path is empty",
                    ));
                }
                let source = repo_path_field(source, "git status", index)?;
                push_changed_entries(&mut entries, &mut dirty, &record, Some(source), index)?;
            }
            b'u' => {
                let record = fixed_fields(field, index, 11)?;
                let xy = record[1];
                if xy.len() != 2 {
                    return Err(malformed(
                        "git status",
                        index,
                        "XY status must contain two bytes",
                    ));
                }
                increment(&mut dirty.conflicted)?;
                let submodule = record[2].first() == Some(&b'S');
                if submodule {
                    increment(&mut dirty.submodule)?;
                }
                entries.push(ChangedPath {
                    path: repo_path_field(record[10], "git status", index)?,
                    old_path: None,
                    area: ChangeArea::Conflict,
                    kind: ChangeKind::Unmerged,
                    conflict: Some(parse_conflict_kind(xy, index)?),
                    submodule,
                    added_lines: None,
                    removed_lines: None,
                });
            }
            b'?' => {
                let path = path_record(field, index)?;
                increment(&mut dirty.untracked)?;
                entries.push(ChangedPath {
                    path: repo_path_field(path, "git status", index)?,
                    old_path: None,
                    area: ChangeArea::Untracked,
                    kind: ChangeKind::Untracked,
                    conflict: None,
                    submodule: false,
                    added_lines: None,
                    removed_lines: None,
                });
            }
            // Only reported with `--ignored`, which this service never passes;
            // tolerated so a caller that adds it does not fail parsing.
            b'!' => {
                path_record(field, index)?;
            }
            _ => return Err(malformed("git status", index, "unknown record type")),
        }
        index += 1;
    }

    if !saw_oid || !saw_head {
        return Err(malformed(
            "git status",
            fields.len(),
            "branch.oid and branch.head headers are required",
        ));
    }

    let head = if let Some(oid) = oid {
        match branch.as_deref() {
            Some("(detached)") | None => HeadState::Detached { oid },
            Some(name) => HeadState::Branch {
                name: name.into(),
                oid,
            },
        }
    } else {
        HeadState::Unborn {
            branch: branch.filter(|name| name != "(detached)"),
        }
    };
    let tracking = match (upstream, ahead_behind) {
        (None, None) => TrackingState::NoUpstream,
        (Some(upstream), Some((ahead, behind))) => TrackingState::Compared {
            upstream,
            ahead,
            behind,
        },
        (upstream, _) => TrackingState::Unknown { upstream },
    };
    Ok(StatusSnapshot {
        untracked_omitted: false,
        listing_truncated: false,
        head,
        tracking,
        dirty,
        entries,
    })
}

/// The fields of a porcelain-v2 `1` or `2` record that this service uses.
struct ChangedRecord<'a> {
    xy: &'a [u8],
    submodule: bool,
    path: &'a [u8],
}

fn parse_changed_record<'a>(
    field: &'a [u8],
    index: usize,
    count: usize,
) -> Result<ChangedRecord<'a>, GitParseError> {
    let fields = fixed_fields(field, index, count)?;
    let xy = fields[1];
    if xy.len() != 2 {
        return Err(malformed(
            "git status",
            index,
            "XY status must contain two bytes",
        ));
    }
    Ok(ChangedRecord {
        xy,
        submodule: fields[2].first() == Some(&b'S'),
        path: fields[count - 1],
    })
}

/// Emits one entry per non-`.` half of an XY status.
///
/// A path staged *and* modified again in the worktree is two separate pieces of
/// work — the staged version can be committed while the worktree version is
/// still being edited — so it becomes two entries rather than one ambiguous
/// row. `dirty` is incremented exactly as it was before entries existed, so
/// the sidebar badges keep their current counts.
fn push_changed_entries(
    entries: &mut Vec<ChangedPath>,
    dirty: &mut DirtyState,
    record: &ChangedRecord<'_>,
    source: Option<RepoPath>,
    index: usize,
) -> Result<(), GitParseError> {
    let path = repo_path_field(record.path, "git status", index)?;
    if record.xy[0] != b'.' {
        increment(&mut dirty.staged)?;
    }
    if record.xy[1] != b'.' {
        increment(&mut dirty.unstaged)?;
    }
    if record.submodule {
        increment(&mut dirty.submodule)?;
    }
    for (code, area) in [
        (record.xy[0], ChangeArea::Staged),
        (record.xy[1], ChangeArea::Unstaged),
    ] {
        if code == b'.' {
            continue;
        }
        let kind = change_kind_from_code(code);
        entries.push(ChangedPath {
            path: path.clone(),
            // The source belongs to the half that actually renamed or copied:
            // for `R.` the index moved the file, and the worktree half of `RM`
            // is an ordinary modification of the destination.
            old_path: match kind {
                ChangeKind::Renamed | ChangeKind::Copied => source.clone(),
                _ => None,
            },
            area,
            kind,
            conflict: None,
            submodule: record.submodule,
            added_lines: None,
            removed_lines: None,
        });
    }
    Ok(())
}

fn change_kind_from_code(code: u8) -> ChangeKind {
    match code {
        b'M' => ChangeKind::Modified,
        b'T' => ChangeKind::TypeChanged,
        b'A' => ChangeKind::Added,
        b'D' => ChangeKind::Deleted,
        b'R' => ChangeKind::Renamed,
        b'C' => ChangeKind::Copied,
        b'U' => ChangeKind::Unmerged,
        b'?' => ChangeKind::Untracked,
        b'!' => ChangeKind::Ignored,
        other => ChangeKind::Unknown(char::from(other)),
    }
}

fn parse_conflict_kind(xy: &[u8], index: usize) -> Result<ConflictKind, GitParseError> {
    Ok(match (xy[0], xy[1]) {
        (b'D', b'D') => ConflictKind::BothDeleted,
        (b'A', b'U') => ConflictKind::AddedByUs,
        (b'U', b'D') => ConflictKind::DeletedByThem,
        (b'U', b'A') => ConflictKind::AddedByThem,
        (b'D', b'U') => ConflictKind::DeletedByUs,
        (b'A', b'A') => ConflictKind::BothAdded,
        (b'U', b'U') => ConflictKind::BothModified,
        _ => {
            return Err(malformed("git status", index, "unknown unmerged XY status"));
        }
    })
}

fn repo_path_field(
    bytes: &[u8],
    format: &'static str,
    index: usize,
) -> Result<RepoPath, GitParseError> {
    RepoPath::from_bytes(bytes).map_err(|error| malformed(format, index, error.to_string()))
}

fn parse_ahead_behind(value: &[u8], index: usize) -> Result<(u64, u64), GitParseError> {
    let value = utf8(value, "git status", index, "branch.ab")?;
    let mut parts = value.split_ascii_whitespace();
    let ahead = parts
        .next()
        .and_then(|part| part.strip_prefix('+'))
        .and_then(|part| part.parse().ok());
    let behind = parts
        .next()
        .and_then(|part| part.strip_prefix('-'))
        .and_then(|part| part.parse().ok());
    if parts.next().is_some() || ahead.is_none() || behind.is_none() {
        return Err(malformed("git status", index, "invalid branch.ab"));
    }
    Ok((ahead.unwrap(), behind.unwrap()))
}

fn fixed_fields(field: &[u8], index: usize, count: usize) -> Result<Vec<&[u8]>, GitParseError> {
    let fields: Vec<_> = field.splitn(count, |byte| *byte == b' ').collect();
    if fields.len() != count || fields.iter().any(|field| field.is_empty()) {
        Err(malformed(
            "git status",
            index,
            "record has missing or empty fields",
        ))
    } else {
        Ok(fields)
    }
}

/// The path of a `?` or `!` record, which is one space-separated field.
fn path_record<'a>(field: &'a [u8], index: usize) -> Result<&'a [u8], GitParseError> {
    if field.len() < 3 || field[1] != b' ' {
        Err(malformed("git status", index, "path is missing"))
    } else {
        Ok(&field[2..])
    }
}

/// One `git diff --numstat -z` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumstatRecord {
    pub path: RepoPath,
    pub old_path: Option<RepoPath>,
    /// `None` where Git printed `-`, which it does for binary files.
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

pub fn parse_numstat(output: &[u8]) -> Result<Vec<NumstatRecord>, GitParseError> {
    const FORMAT: &str = "git diff --numstat";
    let fields = nul_fields(output, FORMAT)?;
    let mut records = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        if field.is_empty() {
            return Err(malformed(FORMAT, index, "record is empty"));
        }
        let mut columns = field.splitn(3, |byte| *byte == b'\t');
        let added = columns
            .next()
            .ok_or_else(|| malformed(FORMAT, index, "added count is missing"))?;
        let removed = columns
            .next()
            .ok_or_else(|| malformed(FORMAT, index, "removed count is missing"))?;
        let path = columns
            .next()
            .ok_or_else(|| malformed(FORMAT, index, "path is missing"))?;
        let added = parse_numstat_count(added, FORMAT, index)?;
        let removed = parse_numstat_count(removed, FORMAT, index)?;
        let (path, old_path) = if path.is_empty() {
            // A rename or copy: the in-record path column is empty and the
            // source and destination follow as two more NUL-delimited fields.
            let source = fields
                .get(index + 1)
                .copied()
                .ok_or(GitParseError::Truncated { format: FORMAT })?;
            let destination = fields
                .get(index + 2)
                .copied()
                .ok_or(GitParseError::Truncated { format: FORMAT })?;
            index += 2;
            (
                repo_path_field(destination, FORMAT, index)?,
                Some(repo_path_field(source, FORMAT, index)?),
            )
        } else {
            (repo_path_field(path, FORMAT, index)?, None)
        };
        records.push(NumstatRecord {
            path,
            old_path,
            added,
            removed,
        });
        index += 1;
    }
    Ok(records)
}

fn parse_numstat_count(
    value: &[u8],
    format: &'static str,
    index: usize,
) -> Result<Option<u64>, GitParseError> {
    if value == b"-" {
        return Ok(None);
    }
    utf8(value, format, index, "count")?
        .parse::<u64>()
        .map(Some)
        .map_err(|_| malformed(format, index, "count is not a number"))
}

/// Compact measured line changes, omitting zero and unavailable sides.
///
/// A missing count is never promoted to zero: binary or unreadable paths can
/// remain unmeasured even when their change status is known.
pub fn line_change_counts(added: Option<u64>, removed: Option<u64>) -> String {
    match (
        added.filter(|count| *count != 0),
        removed.filter(|count| *count != 0),
    ) {
        (Some(added), Some(removed)) => format!("+{added} −{removed}"),
        (Some(added), None) => format!("+{added}"),
        (None, Some(removed)) => format!("−{removed}"),
        (None, None) => String::new(),
    }
}

/// Fills measured line counts into the entries of one area.
///
/// Counts stay `None` where Git reported none, which is every binary file.
/// Conflicts have no comparison; untracked text is measured separately from
/// the worktree because it does not appear in `git diff`.
pub fn merge_numstat(entries: &mut [ChangedPath], area: ChangeArea, records: &[NumstatRecord]) {
    let counts: BTreeMap<&[u8], (Option<u64>, Option<u64>)> = records
        .iter()
        .map(|record| (record.path.as_bytes(), (record.added, record.removed)))
        .collect();
    for entry in entries.iter_mut().filter(|entry| entry.area == area) {
        if let Some((added, removed)) = counts.get(entry.path.as_bytes()) {
            entry.added_lines = *added;
            entry.removed_lines = *removed;
        }
    }
}

fn merge_diff_numstat(entries: &mut [DiffEntry], records: &[NumstatRecord]) {
    let counts: BTreeMap<&[u8], (Option<u64>, Option<u64>)> = records
        .iter()
        .map(|record| (record.path.as_bytes(), (record.added, record.removed)))
        .collect();
    for entry in entries.iter_mut() {
        if let Some((added, removed)) = counts.get(entry.path.as_bytes()) {
            entry.added_lines = *added;
            entry.removed_lines = *removed;
        }
    }
}

pub fn parse_name_status(output: &[u8]) -> Result<Vec<DiffEntry>, GitParseError> {
    const FORMAT: &str = "git diff --name-status";
    let fields = nul_fields(output, FORMAT)?;
    let mut entries = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let status = fields[index];
        let code = *status
            .first()
            .ok_or_else(|| malformed(FORMAT, index, "status is empty"))?;
        let kind = change_kind_from_code(code);
        let path_index = index + 1;
        let path = fields
            .get(path_index)
            .copied()
            .ok_or(GitParseError::Truncated { format: FORMAT })?;
        if matches!(kind, ChangeKind::Renamed | ChangeKind::Copied) {
            let destination = fields
                .get(path_index + 1)
                .copied()
                .ok_or(GitParseError::Truncated { format: FORMAT })?;
            entries.push(DiffEntry {
                path: repo_path_field(destination, FORMAT, path_index)?,
                old_path: Some(repo_path_field(path, FORMAT, path_index)?),
                kind,
                added_lines: None,
                removed_lines: None,
            });
            index = path_index + 2;
        } else {
            entries.push(DiffEntry {
                path: repo_path_field(path, FORMAT, path_index)?,
                old_path: None,
                kind,
                added_lines: None,
                removed_lines: None,
            });
            index = path_index + 1;
        }
    }
    Ok(entries)
}

/// Fields per commit in [`RepositoryService::history_command`]'s format.
const HISTORY_FIELDS: usize = 10;

pub fn parse_history(output: &[u8]) -> Result<Vec<CommitSummary>, GitParseError> {
    const FORMAT: &str = "git log";
    let fields = nul_fields(output, FORMAT)?;
    if !fields.len().is_multiple_of(HISTORY_FIELDS) {
        return Err(malformed(
            FORMAT,
            fields.len() / HISTORY_FIELDS,
            "record does not have the requested field count",
        ));
    }
    let mut commits = Vec::with_capacity(fields.len() / HISTORY_FIELDS);
    for (record, chunk) in fields.chunks_exact(HISTORY_FIELDS).enumerate() {
        let oid = Oid::parse_object(utf8(chunk[0], FORMAT, record, "commit id")?)
            .map_err(|error| malformed(FORMAT, record, error.to_string()))?;
        let parents = utf8(chunk[1], FORMAT, record, "parents")?
            .split_ascii_whitespace()
            .map(|parent| {
                Oid::parse_object(parent)
                    .map_err(|error| malformed(FORMAT, record, error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        commits.push(CommitSummary {
            oid,
            parents,
            author: parse_identity(chunk[2], chunk[3], chunk[4]),
            committer: parse_identity(chunk[5], chunk[6], chunk[7]),
            // Message text is display data, not a protocol field: a commit
            // written in a legacy encoding still has a usable subject, so it is
            // rendered lossily instead of failing the page.
            subject: String::from_utf8_lossy(chunk[8]).into_owned(),
            body: String::from_utf8_lossy(chunk[9])
                .trim_end_matches('\n')
                .to_owned(),
        });
    }
    Ok(commits)
}

/// Builds one signature, degrading a date this cannot read rather than
/// discarding the page it belongs to.
///
/// A commit object's date line is whatever was written into it, and a history
/// imported from CVS or Subversion carries offsets Git itself will not
/// normalise: `+051800` comes back as `+51800`, `+0060` comes back verbatim,
/// and an offset that is not a number at all makes `%ad` print nothing. None
/// of that is evidence about the other 199 commits in the chunk, and a date is
/// display data — so an unreadable one becomes the epoch in UTC for that one
/// commit, which is visibly wrong in the row it belongs to instead of
/// invisibly fatal for the whole page.
fn parse_identity(name: &[u8], email: &[u8], date: &[u8]) -> CommitIdentity {
    let (timestamp, offset) = match std::str::from_utf8(date)
        .unwrap_or_default()
        .split_once(' ')
    {
        Some((timestamp, offset)) => (timestamp.parse::<i64>().unwrap_or_default(), offset),
        None => (0, ""),
    };
    CommitIdentity {
        name: String::from_utf8_lossy(name).into_owned(),
        email: String::from_utf8_lossy(email).into_owned(),
        timestamp,
        offset_minutes: parse_timezone_offset(offset).unwrap_or_default(),
    }
}

/// Parses a commit's recorded UTC offset into minutes, exactly as Git reads it.
///
/// Git treats the field as a signed decimal number `N` and splits it as
/// `N / 100` hours plus `N % 100` minutes, with no range check on either: that
/// is why `+0060` displays an hour east of UTC and `+51800` displays 518 hours
/// east of it. Reproducing the same arithmetic is what makes a rendered
/// timestamp agree with `git log` on the commits that have such an offset,
/// rather than merely not crashing on them. `None` means the field was not a
/// number, or named an offset too large to hold.
fn parse_timezone_offset(value: &str) -> Option<i32> {
    let (sign, digits) = match value.as_bytes().first()? {
        b'+' => (1, &value[1..]),
        b'-' => (-1, &value[1..]),
        _ => return None,
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value = digits.parse::<i32>().ok()?;
    (value / 100)
        .checked_mul(60)?
        .checked_add(value % 100)?
        .checked_mul(sign)
}

/// Parses the `for-each-ref` format used by
/// [`RepositoryService::refs_command`]: NUL-separated fields, one newline-
/// terminated line per ref. Ref names cannot contain either byte, so the
/// framing is unambiguous.
///
/// A ref this cannot classify is skipped rather than fatal, exactly as
/// [`parse_worktree_list`] skips a field it does not know and
/// [`ChangeKind::Unknown`] absorbs a status letter it does not know. The
/// alternative — one `git update-ref refs/remotes/weird` or one branch whose
/// name is not UTF-8 discarding every changed path in the snapshot — treats a
/// ref nobody asked about as evidence that Git's whole answer is untrustworthy.
pub fn parse_refs(output: &[u8]) -> Result<RefListing, GitParseError> {
    const FORMAT: &str = "git for-each-ref";
    const REMOTES: &str = "refs/remotes/";
    let mut listing = RefListing::default();
    // A remote name may contain a slash, so `refs/remotes/te/am/main` is
    // `main` on a remote called `te/am` or `am/main` on one called `te`, and
    // the ref name alone cannot say which. The symbolic
    // `refs/remotes/<remote>/HEAD` refs in this same output are the one place
    // the boundary is unambiguous, because `HEAD` is always the last
    // component — so the remote-tracking refs are held back until every one of
    // those has been read, and then split at the longest remote name the
    // listing actually proved exists.
    let mut pending: Vec<(ValidatedRefName, Oid)> = Vec::new();
    for (record, line) in output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let fields: Vec<_> = line.split(|byte| *byte == 0).collect();
        if fields.len() != 5 {
            return Err(malformed(
                FORMAT,
                record,
                "ref record has a wrong field count",
            ));
        }
        // The field count is the framing and stays a hard requirement; the
        // name is not, because a ref name is bytes and only a `ValidatedRefName`
        // can ever be handed back to Git as a revision.
        let Some(name) = std::str::from_utf8(fields[0])
            .ok()
            .and_then(|name| ValidatedRefName::parse(name).ok())
        else {
            continue;
        };
        let oid = Oid::parse_object(utf8(fields[1], FORMAT, record, "objectname")?)
            .map_err(|error| malformed(FORMAT, record, error.to_string()))?;
        if name.as_str().starts_with("refs/heads/") {
            listing.local.push(LocalRefRecord {
                name,
                oid,
                upstream: optional_ref(fields[3]),
                head: fields[4] == b"*",
            });
            continue;
        }
        let Some(rest) = name.as_str().strip_prefix(REMOTES) else {
            continue;
        };
        // Symbolic-ness is read from the raw field rather than from a parsed
        // target, so a target this cannot represent still keeps the ref out of
        // the branch list.
        if !fields[2].is_empty() {
            if let (Some(remote), Some(target)) =
                (rest.strip_suffix("/HEAD"), optional_ref(fields[2]))
            {
                listing.remote_heads.push(RemoteHead {
                    remote: remote.to_owned(),
                    target,
                });
            }
            continue;
        }
        pending.push((name, oid));
    }
    let remotes: Vec<&str> = listing
        .remote_heads
        .iter()
        .map(|head| head.remote.as_str())
        .collect();
    for (name, oid) in pending {
        // Guaranteed: only a name carrying this prefix reaches `pending`.
        let rest = &name.as_str()[REMOTES.len()..];
        let split = remotes
            .iter()
            .filter(|remote| {
                rest.strip_prefix(**remote)
                    .is_some_and(|branch| branch.starts_with('/'))
            })
            .max_by_key(|remote| remote.len())
            .map(|remote| ((*remote).to_owned(), rest[remote.len() + 1..].to_owned()))
            // No remote in this listing advertises a `HEAD` that covers this
            // ref, so the first component is the only reading there is — and
            // it is the one Git's own default refspec produces.
            .or_else(|| {
                rest.split_once('/')
                    .map(|(remote, branch)| (remote.to_owned(), branch.to_owned()))
            });
        let Some((remote, branch)) = split else {
            continue;
        };
        listing.remote.push(RemoteRefRecord {
            remote,
            branch,
            name,
            oid,
        });
    }
    Ok(listing)
}

/// A ref name Git reported in an optional field, or `None` when the field was
/// empty or named something this cannot hand back to Git.
fn optional_ref(value: &[u8]) -> Option<ValidatedRefName> {
    ValidatedRefName::parse(std::str::from_utf8(value).ok()?).ok()
}

/// Parses `git config -z --get-regexp ^remote\.` output, whose records are
/// `key LF value NUL`.
pub fn parse_remote_config(output: &[u8]) -> Result<Vec<RemoteRecord>, GitParseError> {
    const FORMAT: &str = "git config";
    let mut remotes: BTreeMap<String, RemoteRecord> = BTreeMap::new();
    for (record, field) in nul_fields(output, FORMAT)?.into_iter().enumerate() {
        if field.is_empty() {
            continue;
        }
        let (key, value) = match field.iter().position(|byte| *byte == b'\n') {
            Some(split) => (&field[..split], &field[split + 1..]),
            None => (field, &field[field.len()..]),
        };
        let key = utf8(key, FORMAT, record, "config key")?;
        let Some(rest) = key.strip_prefix("remote.") else {
            continue;
        };
        // A remote name may itself contain dots, so the *last* dot separates
        // the name from the variable.
        let Some((name, variable)) = rest.rsplit_once('.') else {
            continue;
        };
        if name.is_empty() || !matches!(variable, "url" | "pushurl") {
            continue;
        }
        let entry = remotes
            .entry(name.to_owned())
            .or_insert_with(|| RemoteRecord {
                name: name.to_owned(),
                fetch_url: None,
                push_url: None,
            });
        // URLs are opaque configuration text rather than a path this service
        // reopens, so an undecodable byte is rendered rather than fatal.
        let value = String::from_utf8_lossy(value).into_owned();
        // Git fetches from the first configured URL. Preserve that primary
        // destination when a remote has multiple fetch or push URLs.
        if variable == "url" {
            entry.fetch_url.get_or_insert(value);
        } else {
            entry.push_url.get_or_insert(value);
        }
    }
    Ok(remotes.into_values().collect())
}

/// Parses a NUL-delimited path list, deduplicated and sorted.
///
/// `git ls-files -c` prints one line per index stage, so an unmerged path
/// appears up to three times; the file tree wants it once.
pub fn parse_path_list(
    output: &[u8],
    format: &'static str,
) -> Result<Vec<RepoPath>, GitParseError> {
    let mut paths = nul_fields(output, format)?
        .into_iter()
        .filter(|field| !field.is_empty())
        .enumerate()
        .map(|(index, field)| repo_path_field(field, format, index))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Parses `git ls-files -t`, deduplicated, sorted, and counted by
/// trackedness.
///
/// `-t` puts a one-byte status tag and a space ahead of each path. `?` is an
/// "other", i.e. an untracked file; every other tag a listing of cached and
/// other files can carry — `H` cached, `S` skip-worktree, `M` unmerged —
/// describes an index entry, so anything but `?` is tracked. `git ls-files`
/// prints one record per index stage, so an unmerged path arrives three times
/// and the count is taken after deduplication.
pub fn parse_working_files(output: &[u8]) -> Result<WorkingFiles, GitParseError> {
    const FORMAT: &str = "git ls-files -t";
    let mut tagged: BTreeMap<RepoPath, bool> = BTreeMap::new();
    for (record, field) in nul_fields(output, FORMAT)?.into_iter().enumerate() {
        if field.is_empty() {
            continue;
        }
        if field.len() < 2 || field[1] != b' ' {
            return Err(malformed(FORMAT, record, "record has no status tag"));
        }
        let tracked = tagged
            .entry(repo_path_field(&field[2..], FORMAT, record)?)
            .or_default();
        *tracked |= field[0] != b'?';
    }
    let tracked_count = tagged.values().filter(|tracked| **tracked).count();
    Ok(WorkingFiles {
        paths: tagged.into_keys().collect(),
        tracked_count,
        ..WorkingFiles::default()
    })
}

/// Git's file mode for a gitlink, i.e. a submodule's recorded commit.
const GITLINK_MODE: u32 = 0o160_000;
/// Git's file mode for a symbolic link blob.
const SYMLINK_MODE: u32 = 0o120_000;

/// One index entry from `git ls-files -s -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexEntry {
    mode: u32,
    oid: Oid,
    /// 0 for a merged path, 1/2/3 for base/ours/theirs of a conflict.
    stage: u8,
    path: RepoPath,
}

fn parse_index_entries(output: &[u8]) -> Result<Vec<IndexEntry>, GitParseError> {
    const FORMAT: &str = "git ls-files -s";
    let mut entries = Vec::new();
    for (record, field) in nul_fields(output, FORMAT)?.into_iter().enumerate() {
        if field.is_empty() {
            continue;
        }
        let split = field
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| malformed(FORMAT, record, "index entry has no path separator"))?;
        let mut columns = utf8(&field[..split], FORMAT, record, "index entry")?.split(' ');
        let mode = columns
            .next()
            .and_then(|mode| u32::from_str_radix(mode, 8).ok())
            .ok_or_else(|| malformed(FORMAT, record, "index entry mode is invalid"))?;
        let oid = columns
            .next()
            .ok_or_else(|| malformed(FORMAT, record, "index entry object id is missing"))
            .and_then(|oid| {
                Oid::parse_object(oid).map_err(|error| malformed(FORMAT, record, error.to_string()))
            })?;
        let stage = columns
            .next()
            .and_then(|stage| stage.parse::<u8>().ok())
            .filter(|stage| *stage <= 3)
            .ok_or_else(|| malformed(FORMAT, record, "index entry stage is invalid"))?;
        entries.push(IndexEntry {
            mode,
            oid,
            stage,
            path: repo_path_field(&field[split + 1..], FORMAT, record)?,
        });
    }
    Ok(entries)
}

/// One tree entry from `git ls-tree -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEntry {
    mode: u32,
    oid: Oid,
    path: RepoPath,
}

fn parse_tree_entries(output: &[u8]) -> Result<Vec<TreeEntry>, GitParseError> {
    const FORMAT: &str = "git ls-tree";
    let mut entries = Vec::new();
    for (record, field) in nul_fields(output, FORMAT)?.into_iter().enumerate() {
        if field.is_empty() {
            continue;
        }
        let split = field
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| malformed(FORMAT, record, "tree entry has no path separator"))?;
        let mut columns = utf8(&field[..split], FORMAT, record, "tree entry")?.split(' ');
        let mode = columns
            .next()
            .and_then(|mode| u32::from_str_radix(mode, 8).ok())
            .ok_or_else(|| malformed(FORMAT, record, "tree entry mode is invalid"))?;
        // The object type column is redundant with the mode this service acts
        // on, so it is validated for shape and then skipped.
        columns
            .next()
            .ok_or_else(|| malformed(FORMAT, record, "tree entry type is missing"))?;
        let oid = columns
            .next()
            .ok_or_else(|| malformed(FORMAT, record, "tree entry object id is missing"))
            .and_then(|oid| {
                Oid::parse_object(oid).map_err(|error| malformed(FORMAT, record, error.to_string()))
            })?;
        entries.push(TreeEntry {
            mode,
            oid,
            path: repo_path_field(&field[split + 1..], FORMAT, record)?,
        });
    }
    Ok(entries)
}

/// One object of a `git cat-file --batch` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BatchObject {
    Content(Vec<u8>),
    /// Git answered `<oid> missing`, which it does for an object this
    /// repository does not have — the state a blobless or otherwise partial
    /// clone is left in when its promisor remote cannot be reached.
    Missing,
}

/// Splits `git cat-file --batch` output into one payload per requested object.
///
/// Each present object is a `<oid> <type> <size>` header line, exactly `size`
/// bytes of content, and a terminating newline, so the content itself needs no
/// escaping and a NUL or newline inside a blob cannot desynchronise the
/// stream. An absent object is a bare `<oid> missing` line with no content and
/// no trailing newline of its own.
fn parse_batch_blobs(
    output: &[u8],
    format: &'static str,
) -> Result<Vec<BatchObject>, GitParseError> {
    let mut objects = Vec::new();
    let mut cursor = 0;
    while cursor < output.len() {
        let record = objects.len();
        let header_end = output[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| cursor + offset)
            .ok_or(GitParseError::Truncated { format })?;
        let header = utf8(&output[cursor..header_end], format, record, "batch header")?;
        if header.ends_with(" missing") {
            objects.push(BatchObject::Missing);
            cursor = header_end + 1;
            continue;
        }
        let size = header
            .rsplit(' ')
            .next()
            .and_then(|size| size.parse::<usize>().ok())
            .ok_or_else(|| malformed(format, record, "batch header size is invalid"))?;
        let content_start = header_end + 1;
        let content_end = content_start
            .checked_add(size)
            .filter(|end| *end < output.len())
            .ok_or(GitParseError::Truncated { format })?;
        objects.push(BatchObject::Content(
            output[content_start..content_end].to_vec(),
        ));
        // Skip the newline Git appends after the content.
        cursor = content_end + 1;
    }
    Ok(objects)
}

/// The push status flag Git printed for one refspec, or `None` when the
/// refspec is absent from the output.
///
/// `--porcelain` lines are `<flag> TAB <from>:<to> TAB <summary>`, and `!`
/// means the remote refused the update, which is definitive evidence that the
/// remote ref did not move.
fn push_ref_flag(output: &[u8], refspec: &str) -> Option<u8> {
    for line in output.split(|byte| *byte == b'\n') {
        let mut columns = line.split(|byte| *byte == b'\t');
        let flag = columns.next()?;
        let Some(pair) = columns.next() else {
            continue;
        };
        if pair == refspec.as_bytes() {
            return Some(flag.first().copied().unwrap_or(b' '));
        }
    }
    None
}

/// Classifies raw bytes without ever decoding them lossily.
fn classify_bytes(bytes: Vec<u8>) -> DiffSide {
    let size = bytes.len() as u64;
    if bytes.iter().take(BINARY_SNIFF_BYTES).any(|byte| *byte == 0) {
        return DiffSide::Binary { bytes: size };
    }
    match String::from_utf8(bytes) {
        Ok(text) => DiffSide::Text { text, bytes: size },
        Err(_) => DiffSide::InvalidUtf8 { bytes: size },
    }
}

/// Reads one comparison side from the worktree.
///
/// Every step here is anchored to a descriptor rather than to a name, because a
/// name resolves to whatever is there at the moment it is resolved. A symlink
/// is still reported as a link with its target rather than read as though it
/// were the file itself, but no component of the path is ever followed, the
/// size comes from the same descriptor the bytes come from, and the read is
/// bounded — so a classification and the read that acts on it cannot describe
/// different objects.
fn read_worktree_side(worktree: &Path, path: &RepoPath) -> (DiffSide, Option<u32>) {
    use std::os::unix::fs::PermissionsExt;
    let unreadable = |error: &dyn fmt::Display| (DiffSide::Unreadable {
        reason: error.to_string(),
    }, None);
    let root = match fs_safety::Root::open(worktree) {
        Ok(root) => root,
        Err(error) => return unreadable(&error),
    };
    let relative = path.as_path();
    let metadata = match fs_safety::metadata_beneath(&root, relative) {
        Ok(metadata) => metadata,
        Err(error) if error.is_not_found() => return (DiffSide::Absent, None),
        Err(error) => return unreadable(&error),
    };
    if metadata.file_type().is_symlink() {
        return match fs_safety::read_link_beneath(&root, relative) {
            Ok(target) => (DiffSide::Symlink { target }, Some(0o120000)),
            Err(error) => unreadable(&error),
        };
    }
    if !metadata.is_file() {
        return (DiffSide::NotRegularFile, None);
    }

    let mut file = match fs_safety::open_beneath(&root, relative) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => return (DiffSide::Absent, None),
        Err(error) => return unreadable(&error),
    };
    let opened = match fs_safety::metadata_of(&file) {
        Ok(metadata) => metadata,
        Err(error) => return unreadable(&error),
    };
    if !opened.is_file() {
        return (DiffSide::NotRegularFile, None);
    }
    let mode = Some(if opened.permissions().mode() & 0o100 != 0 { 0o100755 } else { 0o100644 });
    if opened.len() > MAX_DIFF_SIDE_BYTES {
        return (DiffSide::Oversize { bytes: opened.len() }, mode);
    }
    let side = match fs_safety::read_bounded(&mut file, MAX_DIFF_SIDE_BYTES) {
        // `read_bounded` reads one byte past the limit precisely so that "at the
        // limit" and "over it" stay distinguishable. A file that grew after its
        // size was taken from the descriptor lands here.
        Ok(bytes) if bytes.len() as u64 > MAX_DIFF_SIDE_BYTES => DiffSide::Oversize {
            bytes: bytes.len() as u64,
        },
        Ok(bytes) => classify_bytes(bytes),
        Err(error) => unreadable(&error).0,
    };
    (side, mode)
}

/// Measures untracked text without one Git process (or allocation) per file.
/// The same descriptor-relative resolver and per-side limit as the diff reader
/// apply; failed, changing, and budget-exhausted reads leave honest unknowns.
#[cfg(test)]
fn measure_untracked(
    worktree: &Path,
    entries: &mut [ChangedPath],
    cancellation: &CancellationToken,
) {
    measure_untracked_cached(worktree, entries, cancellation, &mut BTreeMap::new());
}

fn measure_untracked_cached(
    worktree: &Path,
    entries: &mut [ChangedPath],
    cancellation: &CancellationToken,
    cache: &mut BTreeMap<RepoPath, (FileIdentity, u64)>,
) {
    let current = entries
        .iter()
        .filter(|entry| entry.area == ChangeArea::Untracked && !entry.submodule)
        .take(MAX_UNTRACKED_STATUS_FILES)
        .map(|entry| &entry.path)
        .collect::<std::collections::BTreeSet<_>>();
    cache.retain(|path, _| current.contains(path));
    let mut untracked = entries
        .iter_mut()
        .filter(|entry| entry.area == ChangeArea::Untracked && !entry.submodule)
        .take(MAX_UNTRACKED_STATUS_FILES)
        .peekable();
    if cancellation.is_cancelled() || untracked.peek().is_none() {
        return;
    }
    let Ok(root) = fs_safety::Root::open(worktree) else {
        return;
    };
    let mut remaining = MAX_UNTRACKED_STATUS_BYTES;
    let mut bytes = Vec::new();
    for entry in untracked {
        if cancellation.is_cancelled() || remaining == 0 {
            break;
        }
        let identity = fs_safety::metadata_beneath(&root, entry.path.as_path())
            .ok()
            .filter(|metadata| metadata.is_file())
            .map(|metadata| file_identity(&metadata));
        let cached = identity.and_then(|identity| {
            cache
                .get(&entry.path)
                .filter(|(previous, _)| *previous == identity)
                .map(|(_, lines)| *lines)
        });
        if let Some(lines) = cached.or_else(|| {
            untracked_line_count(&root, &entry.path, cancellation, &mut remaining, &mut bytes)
        }) {
            if let Some(identity) = identity {
                cache.insert(entry.path.clone(), (identity, lines));
            }
            entry.added_lines = Some(lines);
            entry.removed_lines = Some(0);
        }
    }
}

fn untracked_line_count(
    root: &fs_safety::Root,
    path: &RepoPath,
    cancellation: &CancellationToken,
    remaining: &mut u64,
    bytes: &mut Vec<u8>,
) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;

    let relative = path.as_path();
    // Refuse known nonregular objects before opening, then classify the actual
    // descriptor again: a name can change between these two calls.
    let metadata = fs_safety::metadata_beneath(root, relative).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mut file = fs_safety::open_beneath(root, relative).ok()?;
    let opened = fs_safety::metadata_of(&file).ok()?;
    if !opened.is_file() || opened.len() > MAX_DIFF_SIDE_BYTES || opened.len() >= *remaining {
        return None;
    }
    bytes.clear();
    let mut chunk = [0; 64 * 1024];
    loop {
        if cancellation.is_cancelled() || *remaining == 0 {
            return None;
        }
        // One extra byte distinguishes a file that grew beyond the limit.
        let limit = (MAX_DIFF_SIDE_BYTES + 1 - bytes.len() as u64)
            .min(*remaining)
            .min(chunk.len() as u64) as usize;
        let read = match file.read(&mut chunk[..limit]) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        *remaining -= read as u64;
        if read == 0 {
            break;
        }
        let sniff = read.min(BINARY_SNIFF_BYTES.saturating_sub(bytes.len()));
        if chunk[..sniff].contains(&0) {
            return None;
        }
        if bytes.len() as u64 + read as u64 > MAX_DIFF_SIDE_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    // Reject concurrent writes and replacement/unlink races rather than attach
    // counts from a different object to this path in the snapshot.
    let identity = |metadata: &fs::Metadata| {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    };
    let after = fs_safety::metadata_of(&file).ok()?;
    let named = fs_safety::metadata_beneath(root, relative).ok()?;
    if identity(&opened) != identity(&after)
        || identity(&after) != identity(&named)
        || bytes.len() as u64 != after.len()
        || cancellation.is_cancelled()
    {
        return None;
    }
    std::str::from_utf8(bytes).ok()?;
    Some(
        bytes.iter().filter(|byte| **byte == b'\n').count() as u64
            + u64::from(!bytes.is_empty() && bytes.last() != Some(&b'\n')),
    )
}

fn increment(value: &mut u32) -> Result<(), GitParseError> {
    *value = value.checked_add(1).ok_or(GitParseError::CounterOverflow {
        format: "git status",
    })?;
    Ok(())
}

fn nul_fields<'a>(output: &'a [u8], format: &'static str) -> Result<Vec<&'a [u8]>, GitParseError> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    if output.last() != Some(&0) {
        return Err(GitParseError::Truncated { format });
    }
    Ok(output[..output.len() - 1]
        .split(|byte| *byte == 0)
        .collect())
}

fn split_once_space(field: &[u8]) -> (&[u8], Option<&[u8]>) {
    match field.iter().position(|byte| *byte == b' ') {
        Some(index) => (&field[..index], Some(&field[index + 1..])),
        None => (field, None),
    }
}

fn current_record(
    current: &mut Option<WorktreeRecord>,
    index: usize,
) -> Result<&mut WorktreeRecord, GitParseError> {
    current
        .as_mut()
        .ok_or_else(|| malformed("git worktree list", index, "field appeared before worktree"))
}

fn utf8_value<'a>(
    value: Option<&'a [u8]>,
    format: &'static str,
    index: usize,
    name: &str,
) -> Result<&'a str, GitParseError> {
    let value = value.ok_or_else(|| malformed(format, index, format!("{name} is missing")))?;
    utf8(value, format, index, name)
}

fn utf8<'a>(
    value: &'a [u8],
    format: &'static str,
    index: usize,
    name: &str,
) -> Result<&'a str, GitParseError> {
    std::str::from_utf8(value).map_err(|_| malformed(format, index, format!("{name} is not UTF-8")))
}

fn optional_utf8(
    value: Option<&[u8]>,
    format: &'static str,
    index: usize,
) -> Result<Option<String>, GitParseError> {
    value
        .filter(|value| !value.is_empty())
        .map(|value| utf8(value, format, index, "value").map(str::to_owned))
        .transpose()
}

fn malformed(format: &'static str, record: usize, reason: impl Into<String>) -> GitParseError {
    GitParseError::Malformed {
        format,
        record,
        reason: reason.into(),
    }
}

fn is_hex_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(unix)]
fn bytes_to_path(value: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(value.to_vec()))
}

pub(crate) fn stable_worktree_id(path: &Path) -> WorktreeId {
    WorktreeId::from_uuid(stable_path_uuid(path))
}

fn stable_path_uuid(path: &Path) -> Uuid {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let path_text = path.to_string_lossy();
    #[cfg(not(unix))]
    let bytes = path_text.as_bytes();

    fn fnv(bytes: &[u8], seed: u64) -> u64 {
        bytes.iter().fold(seed, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
    }
    let first = fnv(bytes, 0xcbf29ce484222325);
    let second = fnv(bytes, 0x84222325cbf29ce4);
    let mut uuid = [0_u8; 16];
    uuid[..8].copy_from_slice(&first.to_be_bytes());
    uuid[8..].copy_from_slice(&second.to_be_bytes());
    uuid[6] = (uuid[6] & 0x0f) | 0x80;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    Uuid::from_bytes(uuid)
}

#[cfg(not(unix))]
fn bytes_to_path(value: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(value).into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    #[error("branch name is invalid: {0}")]
    InvalidBranch(String),
    #[error("base revision is invalid: {0}")]
    InvalidBase(String),
    #[error("tracking requires a local or remote branch base")]
    TrackingRequiresBranch,
    #[error("destination root is not a directory")]
    InvalidDestinationRoot,
    #[error("destination must have an existing parent beneath the managed root")]
    DestinationOutsideRoot,
    #[error("destination already exists")]
    DestinationOccupied,
    #[error("destination leaf may not be a symlink")]
    DestinationSymlink,
    #[error("path validation failed: {0}")]
    Io(String),
    #[error("no paths were selected")]
    EmptySelection,
    #[error("commit message is empty")]
    EmptyCommitMessage,
    #[error("remote name is invalid: {0}")]
    InvalidRemote(String),
    #[error("discarding changes requires explicit confirmation")]
    ConfirmationRequired,
    #[error("worktree path must be absolute: {0}")]
    InvalidWorktree(String),
    #[error("repository path is invalid: {0}")]
    InvalidPath(String),
    #[error("{reference} is not a ref remote {remote} knows")]
    ForeignTrackingRef { remote: String, reference: String },
    #[error("{0} names no ref on a remote")]
    NotARemoteSideRef(String),
}

impl From<RepoPathError> for ValidationError {
    fn from(error: RepoPathError) -> Self {
        Self::InvalidPath(error.to_string())
    }
}

impl From<OidError> for ValidationError {
    fn from(error: OidError) -> Self {
        Self::InvalidBase(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ValidatedBranchName(String);

impl ValidatedBranchName {
    pub fn parse(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        let invalid_component = value.split('/').any(|component| {
            component.is_empty()
                || component == "."
                || component == ".."
                || component.ends_with(".lock")
                || component.ends_with('.')
        });
        let forbidden = value.bytes().any(|byte| {
            byte.is_ascii_control()
                || byte == b' '
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        });
        if value.is_empty()
            || value.starts_with('-')
            || value.ends_with('/')
            || value.contains("..")
            || value.contains("@{")
            || value == "@"
            || invalid_component
            || forbidden
        {
            Err(ValidationError::InvalidBranch(value))
        } else {
            Ok(Self(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn full_ref(&self) -> String {
        format!("refs/heads/{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum BaseRevision {
    ObjectId(String),
    LocalBranch(ValidatedBranchName),
    RemoteRef(String),
}

impl BaseRevision {
    pub fn object_id(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        if is_hex_oid(&value) {
            Ok(Self::ObjectId(value))
        } else {
            Err(ValidationError::InvalidBase(value))
        }
    }

    pub fn remote_ref(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        let suffix = value.strip_prefix("refs/remotes/");
        if suffix.is_some_and(|suffix| ValidatedBranchName::parse(suffix).is_ok()) {
            Ok(Self::RemoteRef(value))
        } else {
            Err(ValidationError::InvalidBase(value))
        }
    }

    pub fn revision_argument(&self) -> String {
        match self {
            Self::ObjectId(oid) => oid.clone(),
            Self::LocalBranch(branch) => branch.full_ref(),
            Self::RemoteRef(reference) => reference.clone(),
        }
    }

    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ObjectId(oid) => Self::object_id(oid.clone()).map(|_| ()),
            Self::LocalBranch(branch) => ValidatedBranchName::parse(branch.as_str()).map(|_| ()),
            Self::RemoteRef(reference) => Self::remote_ref(reference.clone()).map(|_| ()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateWorktreeRequest {
    pub repository: PathBuf,
    pub managed_root: PathBuf,
    pub destination: PathBuf,
    pub branch: ValidatedBranchName,
    pub base: BaseRevision,
    pub track: bool,
    pub operation_id: OperationId,
}

/// User intent, resolved without creating directories or changing Git refs.
#[derive(Debug, Clone)]
pub struct WorktreeInput {
    pub repository: PathBuf,
    pub input: String,
    pub base: String,
    pub managed_directory: String,
    /// Optional final destination, absolute or relative to the repository root.
    pub destination: String,
    pub operation_id: OperationId,
}

/// The branch a worktree request names: the input itself, or the branch a
/// Linear issue URL implies. Pure, so a form can check it as the user types;
/// what the repository already holds is settled by `resolve_worktree_request`.
pub fn worktree_branch_input(input: &str) -> Result<String, ValidationError> {
    let input = input.trim();
    if !input.contains("://") {
        return ValidatedBranchName::parse(input).map(|branch| branch.0);
    }
    let url = url::Url::parse(input)
        .map_err(|_| ValidationError::InvalidBranch("Enter a branch or Linear issue URL".into()))?;
    if url.scheme() != "https" || url.host_str() != Some("linear.app") {
        return Err(ValidationError::InvalidBranch(
            "Expected an https://linear.app issue URL".into(),
        ));
    }
    let segments: Vec<_> = url.path_segments().into_iter().flatten().collect();
    let issue = segments
        .iter()
        .position(|segment| *segment == "issue")
        .and_then(|index| segments.get(index + 1).map(|key| (index, *key)))
        .filter(|(_, key)| linear_issue_key(key));
    let Some((index, key)) = issue else {
        return Err(ValidationError::InvalidBranch(
            "Linear URL must contain /issue/TEAM-123".into(),
        ));
    };
    let title = segments.get(index + 2).copied().unwrap_or_default();
    let mut decoded = Vec::with_capacity(title.len());
    let mut bytes = title.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hex = bytes
                .next()
                .and_then(|high| char::from(high).to_digit(16))
                .zip(bytes.next().and_then(|low| char::from(low).to_digit(16)))
                .ok_or_else(|| {
                    ValidationError::InvalidBranch("Malformed URL title encoding".into())
                })?;
            decoded.push((hex.0 * 16 + hex.1) as u8);
        } else {
            decoded.push(byte);
        }
    }
    let title = String::from_utf8(decoded)
        .map_err(|_| ValidationError::InvalidBranch("URL title is not UTF-8".into()))?;
    // Only use the slug actually supplied in the URL, never inferred metadata.
    let slug = title
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    ValidatedBranchName::parse(if slug.is_empty() {
        key.to_string()
    } else {
        format!("{key}-{slug}")
    })
    .map(|branch| branch.0)
}

fn linear_issue_key(value: &str) -> bool {
    value.split_once('-').is_some_and(|(team, number)| {
        !team.is_empty()
            && team.bytes().all(|byte| byte.is_ascii_alphabetic())
            && !number.is_empty()
            && number.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// The managed directory a branch's worktree is created in, relative to the
/// managed root.
///
/// A branch is a path already: `carson/main-5717-fix` names a directory
/// `main-5717-fix` inside `carson`, which is how every other tool that lays
/// worktrees out by branch spells it and what a reader expects to see in a
/// file manager. Encoding the slash produced `carson%2Fmain-5717-fix`, a name
/// nobody typed and nobody could read.
///
/// Every other byte a filesystem or a shell could misread is still
/// percent-encoded, and a component the branch grammar forbids anyway - empty,
/// `.` or `..` - falls back to the encoded form so a destination can never
/// leave the managed root, whatever Git accepted.
fn branch_destination_component(branch: &str) -> PathBuf {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    fn encode(component: &str, allow_dot: bool) -> String {
        let mut encoded = String::with_capacity(component.len());
        for byte in component.bytes() {
            if byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_')
                || (allow_dot && byte == b'.')
            {
                encoded.push(char::from(byte));
            } else {
                encoded.push('%');
                encoded.push(char::from(HEX[(byte >> 4) as usize]));
                encoded.push(char::from(HEX[(byte & 15) as usize]));
            }
        }
        encoded
    }
    let components: Vec<&str> = branch.split('/').collect();
    let nestable = components
        .iter()
        .all(|component| !component.is_empty() && !component.starts_with('.'));
    if !nestable {
        return PathBuf::from(encode(branch, false));
    }
    components
        .iter()
        .map(|component| encode(component, true))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCreationPlan {
    pub validate_branch: CommandSpec,
    pub resolve_base: CommandSpec,
    pub check_branch_absent: CommandSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeResidualState {
    pub branch_exists: bool,
    pub registered_worktree: Option<WorktreeRecord>,
    pub destination: DestinationResidualState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationResidualState {
    Absent,
    EmptyDirectory,
    NonEmptyDirectory,
    File,
    Symlink,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WorktreeCreationOutcome {
    Created {
        worktree: WorktreeRecord,
        warnings: Vec<ProcessFailure>,
    },
    NotCreated {
        failure: ProcessFailure,
    },
    OutcomeUnknown {
        failure: ProcessFailure,
        residual: WorktreeResidualState,
    },
}

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Parse(#[from] GitParseError),
    #[error("invalid Git output: {error}")]
    InvalidGitOutput {
        error: GitParseError,
        process: ProcessOutput,
    },
    #[error("invalid Git command response: {source}")]
    InvalidResponse {
        #[source]
        source: Box<RepositoryError>,
        process: ProcessOutput,
    },
    #[error(transparent)]
    Process(#[from] ProcessFailure),
    #[error("invalid git output for {field}: {reason}")]
    InvalidOutput { field: &'static str, reason: String },
    #[error("{} is a Git directory with no resolvable work tree", .git_dir.display())]
    NoWorkTree { git_dir: PathBuf },
    /// The remote's pull request head is no longer the commit the caller was
    /// reviewing. The private ref now names `actual`; nothing else moved.
    #[error("pull request head moved from {expected} to {actual}")]
    PullRequestHeadMoved { expected: Oid, actual: Oid },
    /// The pull request head fetched, but the remote no longer serves its base
    /// branch — deleted after a merge, or living only on a fork — so there is
    /// no tip to compute a merge base against.
    #[error("the remote no longer serves base branch {}: {failure}", base_branch.as_str())]
    PullRequestBaseUnavailable {
        base_branch: ValidatedBranchName,
        failure: Box<ProcessFailure>,
    },
}

#[derive(Clone)]
pub struct RepositoryService<E = SystemProcessExecutor> {
    git: PathBuf,
    executor: Arc<E>,
}

impl RepositoryService<SystemProcessExecutor> {
    pub fn system(git: impl Into<PathBuf>) -> Self {
        Self::new(git, Arc::new(SystemProcessExecutor))
    }
}

impl<E: ProcessExecutor> RepositoryService<E> {
    pub fn new(git: impl Into<PathBuf>, executor: Arc<E>) -> Self {
        Self {
            git: git.into(),
            executor,
        }
    }

    /// The queries every repository load starts from, in the order
    /// [`Self::discover`] consumes them.
    ///
    /// `--show-toplevel` is deliberately absent. It is the one query that is
    /// not valid everywhere a repository can be selected from: run inside a
    /// Git directory it exits 128 with "this operation must be run in a work
    /// tree", and a reopened project hands its common directory —
    /// `<root>/.git` — straight back here, so batching it in failed a Git call
    /// on every single load. `--is-inside-work-tree` answers the same question
    /// from anywhere without failing, so discovery asks that and resolves the
    /// root separately.
    pub fn discovery_commands(&self, selected_path: &Path) -> Vec<CommandSpec> {
        [
            "--is-bare-repository",
            "--git-common-dir",
            "--git-dir",
            "--is-inside-work-tree",
        ]
        .into_iter()
        .map(|query| {
            self.git_read(
                selected_path,
                ["rev-parse", "--path-format=absolute", query],
            )
        })
        .collect()
    }

    /// The root of the work tree containing `directory`, or configured by
    /// `core.worktree` when `directory` is a Git directory.
    pub fn worktree_root_command(&self, directory: &Path) -> CommandSpec {
        self.git_read(
            directory,
            ["rev-parse", "--path-format=absolute", "--show-toplevel"],
        )
    }

    pub fn worktree_list_command(&self, repository: &Path) -> CommandSpec {
        self.git_read(repository, ["worktree", "list", "--porcelain", "-z"])
    }

    pub fn status_command(&self, worktree: &Path) -> CommandSpec {
        self.status_command_with_untracked(worktree, true)
    }

    pub fn status_command_with_untracked(&self, worktree: &Path, untracked: bool) -> CommandSpec {
        self.git_read(
            worktree,
            [
                "status",
                "--porcelain=v2",
                "--branch",
                "--ahead-behind",
                // Every untracked file individually, not just the directory
                // that contains them: staging and discarding act on files.
                if untracked {
                    "--untracked-files=all"
                } else {
                    "--untracked-files=no"
                },
                "-z",
            ],
        )
    }

    /// Every file the working tree offers: tracked entries plus untracked ones
    /// that are not ignored, which is exactly what the file tree shows.
    ///
    /// `-t` prefixes each record with a status tag, and that tag is what lets
    /// the two be told apart: `-o` is "others", i.e. untracked, so a combined
    /// list counted as tracked overstates the tracked total by exactly the
    /// number of untracked files. `-z` keeps every path byte-exact.
    pub fn working_files_command(&self, worktree: &Path) -> CommandSpec {
        self.git_read(worktree, ["ls-files", "-tco", "--exclude-standard", "-z"])
    }

    /// Lists ignored files and collapsed folders without walking build trees.
    pub fn ignored_files(
        &self,
        worktree: &Path,
        known_files: &[RepoPath],
        cancellation: &CancellationToken,
    ) -> Result<WorkingFiles, RepositoryError> {
        self.ignored_files_with_limit(worktree, known_files, None, cancellation)
    }

    pub fn ignored_files_with_limit(
        &self,
        worktree: &Path,
        known_files: &[RepoPath],
        limit: Option<usize>,
        cancellation: &CancellationToken,
    ) -> Result<WorkingFiles, RepositoryError> {
        let (output, truncated) = self.listing_output(
            &self.git_read(
                worktree,
                [
                    "ls-files",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                    "--directory",
                    "-z",
                ],
            ),
            limit,
            status_limit::Format::Files,
            cancellation,
        )?;
        let parsed = (|| {
            let mut files = WorkingFiles { listing_truncated: truncated, ..WorkingFiles::default() };
            for (record, field) in nul_fields(&output.stdout, "git ls-files --ignored")?
                .into_iter()
                .enumerate()
            {
                if field.is_empty() {
                    continue;
                }
                let (name, directory) = field
                    .strip_suffix(b"/")
                    .map_or((field, false), |name| (name, true));
                let path = repo_path_field(name, "git ls-files --ignored", record)?;
                if directory {
                    files.ignored_directories.push(path);
                } else {
                    files.ignored.push(path);
                }
            }
            Ok(files)
        })();
        let mut files = invalid_output(parsed, output)?;
        // A force-tracked child makes Git list an ignored folder's contents
        // instead of the folder itself. Classify their ancestors as well so
        // the folder retains its own ignored styling.
        let candidates: std::collections::BTreeSet<_> = files
            .ignored
            .iter()
            .chain(&files.ignored_directories)
            .chain(known_files)
            .flat_map(|path| path.as_path().ancestors().skip(1))
            .filter(|path| !path.as_os_str().is_empty())
            .collect();
        if !candidates.is_empty() {
            let input: Vec<u8> = candidates
                .into_iter()
                .flat_map(|path| {
                    path.as_os_str()
                        .as_encoded_bytes()
                        .iter()
                        .copied()
                        .chain([0])
                })
                .collect();
            let command = self
                .git_read(worktree, ["check-ignore", "--no-index", "-z", "--stdin"])
                .stdin(input);
            let output = self.run(&command, cancellation)?;
            if !output.success && output.exit_code != Some(1) {
                return Err(git_failure(&command, output).into());
            }
            let parsed = parse_path_list(&output.stdout, "git check-ignore");
            files
                .ignored_directories
                .extend(invalid_output(parsed, output)?);
            files.ignored_directories.sort();
            files.ignored_directories.dedup();
        }
        Ok(files)
    }

    /// Watch membership includes untracked directories, so later edits and
    /// creations within them produce native events too.
    pub fn watch_files(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<WorkingFiles, RepositoryError> {
        let output = self.run_checked(&self.working_files_command(worktree), cancellation)?;
        invalid_output(parse_working_files(&output.stdout), output)
    }

    /// A bounded read-only inventory prefix for watch planning. A truncated
    /// prefix is not complete membership: callers must preserve root/admin
    /// coverage and provide an explicit directory census or polling fallback.
    pub fn watch_files_projection(
        &self,
        worktree: &Path,
        limit: usize,
        cancellation: &CancellationToken,
    ) -> Result<WorkingFiles, RepositoryError> {
        let (output, truncated) = self.listing_output(
            &self.working_files_command(worktree),
            Some(limit.clamp(1, 100_000)),
            status_limit::Format::Files,
            cancellation,
        )?;
        let mut files = invalid_output(parse_working_files(&output.stdout), output)?;
        files.listing_truncated = truncated;
        Ok(files)
    }

    /// Tracked paths alone are sufficient to watch the sidebar's tracked-only
    /// status without enumerating ignored build trees or untracked files.
    pub fn tracked_files(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<WorkingFiles, RepositoryError> {
        let output = self.run_checked(
            &self.git_read(worktree, ["ls-files", "-tc", "--exclude-standard", "-z"]),
            cancellation,
        )?;
        let parsed = parse_working_files(&output.stdout);
        invalid_output(parsed, output)
    }

    /// Reads remote URLs from configuration rather than parsing `git remote
    /// -v`, whose columns are ambiguous for a URL containing whitespace.
    pub fn remotes_command(&self, worktree: &Path) -> CommandSpec {
        self.git_read(worktree, ["config", "-z", "--get-regexp", r"^remote\."])
    }

    pub fn refs_command(&self, worktree: &Path) -> CommandSpec {
        self.git_read(
            worktree,
            [
                "for-each-ref",
                // `%00` separates the fields; a ref name can contain neither a
                // NUL nor a newline, so record framing stays unambiguous.
                "--format=%(refname)%00%(objectname)%00%(symref)%00%(upstream)%00%(HEAD)",
                "refs/heads",
                "refs/remotes",
            ],
        )
    }

    /// Line counts for one uncommitted area. The staged form compares HEAD to
    /// the index and also works on an unborn branch, where Git compares against
    /// the empty tree.
    pub fn numstat_command(&self, worktree: &Path, staged: bool) -> CommandSpec {
        let mut arguments = vec![OsString::from("diff")];
        if staged {
            arguments.push(OsString::from("--cached"));
        }
        arguments.extend([
            OsString::from("--numstat"),
            OsString::from("-z"),
            OsString::from("--find-renames"),
            OsString::from("--no-color"),
        ]);
        self.git_read(worktree, arguments)
    }

    pub fn discover(
        &self,
        selected_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<RepositoryInfo, RepositoryError> {
        let commands = self.discovery_commands(selected_path);
        let bare_output = self.run_checked(&commands[0], cancellation)?;
        let bare =
            attach_repository_process(parse_bool_line(&bare_output.stdout, "bare"), &bare_output)?;
        let common_output = self.run_checked(&commands[1], cancellation)?;
        let common_dir = attach_repository_process(
            parse_path_line(&common_output.stdout, "common_dir"),
            &common_output,
        )?;
        let git_dir_output = self.run_checked(&commands[2], cancellation)?;
        let git_dir = attach_repository_process(
            parse_path_line(&git_dir_output.stdout, "git_dir"),
            &git_dir_output,
        )?;
        // A bare repository has no work tree, so neither of the two remaining
        // queries is asked: `--is-inside-work-tree` has nothing to report and
        // `--show-toplevel` would fail there exactly as it does inside a Git
        // directory.
        let worktree_root = if bare {
            None
        } else {
            let inside_output = self.run_checked(&commands[3], cancellation)?;
            let inside_work_tree = attach_repository_process(
                parse_bool_line(&inside_output.stdout, "inside_work_tree"),
                &inside_output,
            )?;
            Some(if inside_work_tree {
                let command = self.worktree_root_command(selected_path);
                let output = self.run_checked(&command, cancellation)?;
                attach_repository_process(
                    parse_path_line(&output.stdout, "worktree_root"),
                    &output,
                )?
            } else {
                // The selected path is the Git directory itself, which has no
                // current work tree to be asked about. The repository still
                // has one, and a non-bare repository must never be rooted at
                // its Git directory: that is what titles a reopened card
                // `.git` and points its file tree at the repository's own
                // metadata.
                self.worktree_root_of(selected_path, &git_dir, cancellation)?
            })
        };
        Ok(RepositoryInfo {
            id: RepositoryId(stable_path_uuid(&common_dir)),
            selected_path: selected_path.to_path_buf(),
            common_dir,
            git_dir,
            worktree_root,
            is_bare: bare,
        })
    }

    /// The work tree a Git directory belongs to, resolved from outside every
    /// work tree.
    ///
    /// `git worktree list` answers from inside a Git directory. Match each
    /// candidate by its Git directory, then ask Git for the actual root: the
    /// main entry can name the metadata directory for a custom Git directory,
    /// while `--show-toplevel` respects `core.worktree`. A linked work tree's
    /// Git directory is `<common>/worktrees/<id>`, and `<id>` cannot be derived
    /// from the work tree's path. Deleted candidates cannot answer and are
    /// skipped; cancellation belongs to the whole request and ends the search.
    fn worktree_root_of(
        &self,
        git_directory: &Path,
        git_dir: &Path,
        cancellation: &CancellationToken,
    ) -> Result<PathBuf, RepositoryError> {
        for candidate in self.list_worktrees(git_directory, cancellation)? {
            if candidate.bare {
                continue;
            }
            let command = self.git_read(
                &candidate.path,
                ["rev-parse", "--path-format=absolute", "--git-dir"],
            );
            let output = match self.run_checked(&command, cancellation) {
                Ok(output) => output,
                Err(failure) if failure.kind == ProcessFailureKind::Cancelled => {
                    return Err(failure.into());
                }
                Err(_) => continue,
            };
            let Ok(candidate_git_dir) = parse_path_line(&output.stdout, "git_dir") else {
                continue;
            };
            if candidate_git_dir.as_path() == git_dir {
                let command = self.worktree_root_command(&candidate.path);
                let output = match self.run_checked(&command, cancellation) {
                    Ok(output) => output,
                    Err(failure) if failure.kind == ProcessFailureKind::Cancelled => {
                        return Err(failure.into());
                    }
                    Err(_) => continue,
                };
                return attach_repository_process(
                    parse_path_line(&output.stdout, "worktree_root"),
                    &output,
                );
            }
        }
        Err(RepositoryError::NoWorkTree {
            git_dir: git_dir.to_path_buf(),
        })
    }

    pub fn list_worktrees(
        &self,
        repository: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Vec<WorktreeRecord>, RepositoryError> {
        let command = self.worktree_list_command(repository);
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_worktree_list(&output.stdout);
        match parsed {
            Ok(mut worktrees) => {
                for worktree in &mut worktrees {
                    worktree.created_at_unix_ms = std::fs::metadata(&worktree.path)
                        .and_then(|metadata| metadata.created())
                        .ok()
                        .and_then(|created| created.duration_since(std::time::UNIX_EPOCH).ok())
                        .and_then(|duration| i64::try_from(duration.as_millis()).ok());
                }
                Ok(worktrees)
            }
            Err(error) => Err(RepositoryError::InvalidGitOutput {
                error,
                process: output,
            }),
        }
    }

    pub fn ignore_path(&self, worktree: &Path, path: &RepoPath, cancellation: &CancellationToken) -> Result<(), RepositoryError> {
        let _protection = crate::worktree_cleanup::protect(worktree).map_err(|error| ValidationError::Io(error.to_string()))?;
        if cancellation.is_cancelled() { return Err(process_failure(&CommandSpec::new(&self.git), ProcessFailureKind::Cancelled, "termination requested before updating .gitignore", false).into()); }
        ignore_path::add(worktree, path).map_err(|error| ValidationError::Io(error).into())
    }

    pub fn status(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<StatusSnapshot, RepositoryError> {
        self.status_with_untracked(worktree, true, cancellation)
    }

    pub fn status_with_untracked(
        &self,
        worktree: &Path,
        untracked: bool,
        cancellation: &CancellationToken,
    ) -> Result<StatusSnapshot, RepositoryError> {
        let command = self.status_command_with_untracked(worktree, untracked);
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_status(&output.stdout);
        match parsed {
            Ok(mut status) => {
                status.untracked_omitted = !untracked;
                Ok(status)
            }
            Err(error) => Err(RepositoryError::InvalidGitOutput {
                error,
                process: output,
            }),
        }
    }

    fn listing_output(&self, command: &CommandSpec, limit: Option<usize>, format: status_limit::Format, cancellation: &CancellationToken) -> Result<(ProcessOutput, bool), RepositoryError> {
        let command = if let Some(limit) = limit {
            let physical_records = match format {
                status_limit::Format::Status => limit.saturating_mul(2).saturating_add(16),
                status_limit::Format::Numstat => limit.saturating_mul(3),
                status_limit::Format::Files => limit,
            };
            command.clone().output_limits(64 * 1024 * 1024, command.stderr_limit).stdout_record_limit(physical_records)
        } else { command.clone() };
        let mut output = self.run_checked(&command, cancellation)?;
        let Some(limit) = limit else { return Ok((output, false)); };
        let (length, truncated) = match status_limit::prefix(&output.stdout, limit, output.stdout_truncated, format) {
            Ok((prefix, truncated)) => (prefix.len(), truncated),
            Err(error) => return Err(RepositoryError::InvalidGitOutput { error, process: output }),
        };
        output.stdout.truncate(length);
        output.stdout_truncated = truncated;
        Ok((output, truncated))
    }

    /// A bounded read-only status projection for sidebar/watch presentation.
    /// Counts are lower bounds when `listing_truncated` is true. This result
    /// must never authorize all-path mutations or deletion safety decisions;
    /// those callers continue to use the complete `status` read.
    pub fn status_projection(
        &self,
        worktree: &Path,
        untracked: bool,
        limit: usize,
        cancellation: &CancellationToken,
    ) -> Result<StatusSnapshot, RepositoryError> {
        self.status_listing(worktree, untracked, Some(limit.clamp(1, 100_000)), cancellation)
    }

    fn status_listing(&self, worktree: &Path, untracked: bool, limit: Option<usize>, cancellation: &CancellationToken) -> Result<StatusSnapshot, RepositoryError> {
        let command = self.status_command_with_untracked(worktree, untracked);
        let (output, truncated) = self.listing_output(&command, limit, status_limit::Format::Status, cancellation)?;
        let parsed = parse_status(&output.stdout);
        let mut status = invalid_output(parsed, output)?;
        status.untracked_omitted = !untracked;
        status.listing_truncated = truncated;
        Ok(status)
    }

    /// Everything the source-control surfaces need about one worktree.
    pub fn snapshot(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<RepositorySnapshot, RepositoryError> {
        self.snapshot_with_options(worktree, SnapshotOptions::default(), cancellation)
    }

    /// Collapsed source-control groups do not enumerate or read untracked files.
    /// File browsing can still request the complete working-file list separately.
    pub fn snapshot_with_options(
        &self,
        worktree: &Path,
        options: SnapshotOptions,
        cancellation: &CancellationToken,
    ) -> Result<RepositorySnapshot, RepositoryError> {
        self.snapshot_cached(
            worktree,
            options,
            cancellation,
            &mut SnapshotCache::default(),
        )
        .map(|snapshot| snapshot.expect("a fresh cache always returns a snapshot"))
    }

    /// Only status runs for an unchanged refresh; callers invalidate content on
    /// file events, since porcelain does not encode modified file contents.
    pub fn snapshot_cached(
        &self,
        worktree: &Path,
        options: SnapshotOptions,
        cancellation: &CancellationToken,
        cache: &mut SnapshotCache,
    ) -> Result<Option<RepositorySnapshot>, RepositoryError> {
        let admin = snapshot_admin_fingerprint(worktree);
        if !cache.admin.is_empty() && cache.admin != admin {
            cache.invalidate(true);
        }
        if cache.options != Some(options) {
            cache.invalidate(true);
            cache.options = Some(options);
        }
        let git_dir = local_git_dir(worktree);
        if !cache.fsmonitor_checked {
            let mut command = self.git_read(
                worktree,
                ["config", "--show-scope", "--get", "core.fsmonitor"],
            );
            // `git config` never refreshes an index or invokes fsmonitor. Read
            // its scope without the safety override that normal commands use.
            remove_fsmonitor_override(&mut command);
            let output = self.run(&command, cancellation)?;
            cache.fsmonitor = if output.success {
                trusted_fsmonitor(&output.stdout, worktree)
            } else if output.exit_code == Some(1) {
                None
            } else {
                return Err(git_failure(&command, output).into());
            };
            cache.fsmonitor_checked = true;
        }
        let (output, truncated) = self.listing_output(
            &snapshot_fsmonitor(
                self.status_command_with_untracked(worktree, options.untracked_changes),
                cache.fsmonitor.as_deref(),
            ),
            options.status_limit,
            status_limit::Format::Status,
            cancellation,
        )?;
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update([u8::from(truncated)]);
        hasher.update(&output.stdout);
        let digest: [u8; 32] = hasher.finalize().into();
        if !cache.dirty && cache.digest == Some(digest) && cache.metadata.is_some() {
            let root = fs_safety::Root::open(worktree).ok();
            let unchanged = cache.observed.iter().all(|(path, previous)| {
                let current = root
                    .as_ref()
                    .and_then(|root| fs_safety::metadata_beneath(root, path.as_path()).ok())
                    .map(|metadata| file_identity(&metadata));
                current == *previous
            });
            let root_unchanged = !options.untracked_files
                || fs::symlink_metadata(worktree)
                    .ok()
                    .map(|metadata| file_identity(&metadata))
                    == cache.browse_root;
            if unchanged && root_unchanged {
                return Ok(None);
            }
        }
        let mut status = invalid_output(parse_status(&output.stdout), output)?;
        status.untracked_omitted = !options.untracked_changes;
        status.listing_truncated = truncated;
        for (staged, area) in [(true, ChangeArea::Staged), (false, ChangeArea::Unstaged)] {
            let (output, _) = self.listing_output(
                &snapshot_fsmonitor(
                    self.numstat_command(worktree, staged),
                    cache.fsmonitor.as_deref(),
                ),
                options.status_limit,
                status_limit::Format::Numstat,
                cancellation,
            )?;
            let records = invalid_output(parse_numstat(&output.stdout), output)?;
            merge_numstat(&mut status.entries, area, &records);
        }
        measure_untracked_cached(
            worktree,
            &mut status.entries,
            cancellation,
            &mut cache.lines,
        );
        if options.untracked_files
            && let Some((files, ..)) = cache.metadata.as_mut()
        {
            let (output, truncated) = self.listing_output(&self.working_files_command(worktree), options.status_limit, status_limit::Format::Files, cancellation)?;
            *files = invalid_output(parse_working_files(&output.stdout), output)?;
            files.listing_truncated = truncated;
        }
        if cache.metadata.is_none() {
            let files_command = if options.untracked_files {
                self.working_files_command(worktree)
            } else {
                self.git_read(worktree, ["ls-files", "-tc", "--exclude-standard", "-z"])
            };
            let (output, truncated) = self.listing_output(&files_command, options.status_limit, status_limit::Format::Files, cancellation)?;
            let mut files = invalid_output(parse_working_files(&output.stdout), output)?;
            files.listing_truncated = truncated;
            let remotes = self.remotes(worktree, cancellation)?;
            let output = self.run_checked(&self.refs_command(worktree), cancellation)?;
            let refs = invalid_output(parse_refs(&output.stdout), output)?;
            cache.metadata = Some((files, remotes, refs.local, refs.remote, refs.remote_heads));
        }
        if cache.control_metadata.is_none() {
            cache.control_metadata = Some(ControlMetadata {
                operation: self.snapshot_operation_state(
                    worktree,
                    git_dir.as_deref(),
                    cancellation,
                )?,
                stashes: self.stashes(worktree, cancellation)?,
                last_fetch_unix_ms: git_dir
                    .as_ref()
                    .and_then(|directory| fs::metadata(directory.join("FETCH_HEAD")).ok())
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64),
            });
        }
        let control = cache.control_metadata.as_ref().unwrap();
        let (files, remotes, local_refs, remote_refs, remote_heads) =
            cache.metadata.as_ref().unwrap();
        let root = fs_safety::Root::open(worktree).ok();
        cache.observed = status
            .entries
            .iter()
            .map(|entry| {
                let identity = root
                    .as_ref()
                    .and_then(|root| fs_safety::metadata_beneath(root, entry.path.as_path()).ok())
                    .map(|metadata| file_identity(&metadata));
                (entry.path.clone(), identity)
            })
            .collect();
        cache.browse_root = options
            .untracked_files
            .then(|| {
                fs::symlink_metadata(worktree)
                    .ok()
                    .map(|metadata| file_identity(&metadata))
            })
            .flatten();
        cache.admin = admin;
        cache.digest = Some(digest);
        cache.dirty = false;
        Ok(Some(RepositorySnapshot {
            operation: control.operation.clone(),
            stashes: control.stashes.clone(),
            last_fetch_unix_ms: control.last_fetch_unix_ms,
            status,
            files: files.clone(),
            remotes: remotes.clone(),
            local_refs: local_refs.clone(),
            remote_refs: remote_refs.clone(),
            remote_heads: remote_heads.clone(),
        }))
    }

    /// Generated operation drafts follow Git's configured comment policy.
    /// Explicitly authored composer messages keep their separate cleanup rules.
    fn snapshot_operation_state(
        &self,
        worktree: &Path,
        directory: Option<&Path>,
        cancellation: &CancellationToken,
    ) -> Result<Option<GitOperationState>, RepositoryError> {
        let mut state = directory.and_then(Self::operation_state_from_git_dir);
        if let Some(message) = state.as_ref().and_then(|state| state.message.as_ref()) {
            let command = self
                .git_read(worktree, ["stripspace", "--strip-comments"])
                .stdin(message.as_bytes().to_vec());
            let output = self.run_checked(&command, cancellation)?;
            if let Some(state) = state.as_mut() {
                state.message = Some(
                    String::from_utf8_lossy(&output.stdout)
                        .trim_end_matches('\n')
                        .to_owned(),
                )
                .filter(|message| !message.is_empty());
            }
        }
        Ok(state)
    }

    pub fn operation_state(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Option<GitOperationState>, RepositoryError> {
        let directory = self.git_dir(worktree, cancellation)?;
        Ok(Self::operation_state_from_git_dir(&directory))
    }

    pub fn operation_state_from_git_dir(directory: &Path) -> Option<GitOperationState> {
        let read = |path: &str| {
            fs::read_to_string(directory.join(path))
                .ok()
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty())
        };
        let (kind, incoming_ref, message) =
            if directory.join("rebase-merge").is_dir() || directory.join("rebase-apply").is_dir() {
                let prefix = if directory.join("rebase-merge").is_dir() {
                    "rebase-merge"
                } else {
                    "rebase-apply"
                };
                (
                    GitOperationKind::Rebase,
                    read(&format!("{prefix}/onto")),
                    read(&format!("{prefix}/message")).or_else(|| read("MERGE_MSG")),
                )
            } else if directory.join("MERGE_HEAD").exists() {
                (
                    GitOperationKind::Merge,
                    read("MERGE_HEAD"),
                    read("MERGE_MSG"),
                )
            } else if directory.join("CHERRY_PICK_HEAD").exists() {
                (
                    GitOperationKind::CherryPick,
                    read("CHERRY_PICK_HEAD"),
                    read("MERGE_MSG"),
                )
            } else if directory.join("REVERT_HEAD").exists() {
                (
                    GitOperationKind::Revert,
                    read("REVERT_HEAD"),
                    read("MERGE_MSG"),
                )
            } else {
                return None;
            };
        Some(GitOperationState {
            kind,
            incoming_ref,
            message,
        })
    }

    pub fn stashes(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Vec<StashEntry>, RepositoryError> {
        // NUL separators preserve subjects containing tabs, with a separate
        // record terminator supplied by Git's -z option.
        let output = self.run_checked(
            &self.git_read(
                worktree,
                ["stash", "list", "-z", "--format=%gd%x00%H%x00%s"],
            ),
            cancellation,
        )?;
        let fields = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
        let mut entries = Vec::new();
        for record in fields.chunks(3) {
            if record.len() < 3 || record[0].is_empty() {
                continue;
            }
            let reference = String::from_utf8_lossy(record[0]).into_owned();
            let oid = Oid::parse_object(String::from_utf8_lossy(record[1]).into_owned()).map_err(
                |error| RepositoryError::InvalidOutput {
                    field: "stash object id",
                    reason: error.to_string(),
                },
            )?;
            entries.push(StashEntry {
                reference,
                oid,
                message: String::from_utf8_lossy(record[2]).into_owned(),
            });
        }
        Ok(entries)
    }

    /// Execute a non-path mutation under the same worktree lifetime protection
    /// as commit/push. On failure the next snapshot reads any conflict state.
    pub fn git_operation(
        &self,
        worktree: &Path,
        operation: &GitOperation,
        cancellation: &CancellationToken,
    ) -> Result<MutationOutcome, RepositoryError> {
        let _protection = crate::worktree_cleanup::protect(worktree)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        if let GitOperation::ApplyPatch {
            expected_head: Some(expected),
            ..
        } = operation
        {
            let expected = match expected {
                GitHeadGuard::Unborn => None,
                GitHeadGuard::Commit(oid) => Some(oid),
            };
            if self.resolve_head(worktree, cancellation)?.as_ref() != expected {
                return Ok(MutationOutcome::NotApplied { failure: MutationFailure::Rejected { reason: "HEAD changed after this patch was prepared; reload the diff before applying it".into() } });
            }
        }
        let mut command = match operation {
            GitOperation::Fetch { remote, prune } => {
                self.checked_remote(worktree, remote, cancellation)?;
                let mut command = self.git_mutation(worktree, ["fetch"]);
                if *prune {
                    command = command.arg("--prune");
                }
                command.arg("--").arg(remote)
            }
            GitOperation::Sync { rebase } => {
                let pull = self
                    .git_mutation(
                        worktree,
                        ["pull", if *rebase { "--rebase" } else { "--ff-only" }],
                    )
                    .without_timeout();
                if let Some(outcome) = self.operation_failure(&pull, cancellation)? {
                    return Ok(outcome);
                }
                let after_pull = (|| -> Result<MutationOutcome, RepositoryError> {
                    // Resolve the exact upstream destination through the same
                    // checked push path; never depend on push.default.
                    let snapshot = self.snapshot(worktree, cancellation)?;
                    let Some(reference) =
                        snapshot.local_refs.iter().find(|reference| reference.head)
                    else {
                        return Ok(MutationOutcome::NotApplied {
                            failure: MutationFailure::Rejected {
                                reason: "Sync requires a checked-out branch".into(),
                            },
                        });
                    };
                    let Some(upstream) = reference.upstream.as_ref() else {
                        return Ok(MutationOutcome::NotApplied {
                            failure: MutationFailure::Rejected {
                                reason: "Sync requires an upstream".into(),
                            },
                        });
                    };
                    let Some(remote_ref) = snapshot
                        .remote_refs
                        .iter()
                        .find(|candidate| candidate.name == *upstream)
                    else {
                        return Ok(MutationOutcome::NotApplied {
                            failure: MutationFailure::Rejected {
                                reason: "The upstream does not name a remote branch".into(),
                            },
                        });
                    };
                    let branch = ValidatedBranchName::parse(
                        reference
                            .name
                            .as_str()
                            .strip_prefix("refs/heads/")
                            .unwrap_or(reference.name.as_str()),
                    )?;
                    Ok(
                        match self.push(
                            &PushRequest {
                                worktree: worktree.to_owned(),
                                branch,
                                target: PushTarget::Upstream {
                                    remote: remote_ref.remote.clone(),
                                    remote_ref: remote_ref.name.clone(),
                                },
                            },
                            cancellation,
                        )? {
                            PushOutcome::Pushed { .. } => {
                                MutationOutcome::Applied { paths: Vec::new() }
                            }
                            PushOutcome::NotPushed { failure } => {
                                MutationOutcome::PartiallyApplied {
                                    paths: Vec::new(),
                                    failures: vec![
                                        MutationFailure::Reconciliation {
                                            reason: "Pull completed, but the push did not complete"
                                                .into(),
                                        },
                                        failure,
                                    ],
                                }
                            }
                            PushOutcome::OutcomeUnknown { failure } => {
                                MutationOutcome::OutcomeUnknown { failure }
                            }
                        },
                    )
                })();
                let completed = || MutationFailure::Reconciliation {
                    reason: "Pull completed, but the push did not complete".into(),
                };
                return Ok(match after_pull {
                    Ok(MutationOutcome::NotApplied { failure }) => {
                        MutationOutcome::PartiallyApplied {
                            paths: Vec::new(),
                            failures: vec![completed(), failure],
                        }
                    }
                    Ok(MutationOutcome::OutcomeUnknown { failure }) => {
                        MutationOutcome::OutcomeUnknown {
                            failure: MutationFailure::Reconciliation {
                                reason: format!(
                                    "Pull completed; the push outcome is unconfirmed: {failure}"
                                ),
                            },
                        }
                    }
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let failure = match error {
                            RepositoryError::Process(failure) => MutationFailure::Process(failure),
                            error => MutationFailure::Reconciliation {
                                reason: format!("Could not prepare the push: {error}"),
                            },
                        };
                        MutationOutcome::PartiallyApplied {
                            paths: Vec::new(),
                            failures: vec![completed(), failure],
                        }
                    }
                });
            }
            GitOperation::StashPush {
                message,
                include_untracked,
            } => {
                let mut command = self.git_mutation(worktree, ["stash", "push"]);
                if *include_untracked {
                    command = command.arg("--include-untracked");
                }
                if let Some(message) = message {
                    command = command.arg("--message").arg(message);
                }
                command
            }
            GitOperation::StashApply {
                reference,
                expected_oid,
                pop,
            } => {
                let apply = self
                    .git_mutation(worktree, ["stash", "apply", expected_oid.as_str()])
                    .without_timeout();
                if let Some(outcome) = self.operation_failure(&apply, cancellation)? {
                    return Ok(outcome);
                }
                if *pop
                    && let Err(failure) =
                        self.drop_stash_at(worktree, reference, expected_oid, cancellation)
                {
                    let reason = match failure {
                        stash_mutations::StashDropFailure::Refused(reason) => format!(
                            "The selected stash was applied, but its entry was preserved: {reason}"
                        ),
                        stash_mutations::StashDropFailure::Uncertain(reason) => format!(
                            "The selected stash was applied, but deletion is unconfirmed; check the stash list before retrying: {reason}"
                        ),
                    };
                    return Ok(MutationOutcome::PartiallyApplied {
                        paths: Vec::new(),
                        failures: vec![MutationFailure::Reconciliation { reason }],
                    });
                }
                return Ok(MutationOutcome::Applied { paths: Vec::new() });
            }
            GitOperation::StashDrop {
                reference,
                expected_oid,
            } => {
                return Ok(
                    match self.drop_stash_at(worktree, reference, expected_oid, cancellation) {
                        Ok(()) => MutationOutcome::Applied { paths: Vec::new() },
                        Err(stash_mutations::StashDropFailure::Refused(reason)) => {
                            MutationOutcome::NotApplied {
                                failure: MutationFailure::Rejected { reason },
                            }
                        }
                        Err(stash_mutations::StashDropFailure::Uncertain(reason)) => {
                            MutationOutcome::OutcomeUnknown {
                                failure: MutationFailure::Reconciliation { reason },
                            }
                        }
                    },
                );
            }
            GitOperation::ResolveConflict { path, ours } => {
                let entries = self.index_entries(worktree, path, cancellation)?;
                if !entries.iter().any(|entry| entry.stage > 0) {
                    return Ok(MutationOutcome::NotApplied {
                        failure: MutationFailure::Rejected {
                            reason: format!("{path} is no longer conflicted"),
                        },
                    });
                }
                if !entries
                    .iter()
                    .any(|entry| entry.stage == if *ours { 2 } else { 3 })
                {
                    // A modify/delete conflict has no blob on the chosen side.
                    // Git's rm records that deletion as resolved.
                    let command = self.git_pathspec_mutation(
                        worktree,
                        ["rm", "--force"],
                        std::slice::from_ref(path),
                    );
                    if let Some(outcome) = self.operation_failure(&command, cancellation)? {
                        return Ok(outcome);
                    }
                    return Ok(MutationOutcome::Applied {
                        paths: vec![path.clone()],
                    });
                }
                self.git_literal_mutation(
                    worktree,
                    [
                        OsString::from("checkout"),
                        OsString::from(if *ours { "--ours" } else { "--theirs" }),
                        OsString::from("--"),
                        path.as_os_str().to_owned(),
                    ],
                )
            }
            GitOperation::Continue { kind } => self
                .git_mutation(worktree, [kind.command(), "--continue"])
                .env("GIT_EDITOR", "true"),
            GitOperation::Abort { kind } => {
                self.git_mutation(worktree, [kind.command(), "--abort"])
            }
            GitOperation::ApplyPatch {
                patch,
                cached,
                reverse,
                ..
            } => {
                let mut command = self.git_mutation(worktree, ["apply", "--whitespace=nowarn"]);
                if *cached {
                    command = command.arg("--cached");
                }
                if *reverse {
                    command = command.arg("--reverse");
                }
                command.arg("-").stdin(patch.to_vec())
            }
            GitOperation::RevertCommit { oid, mainline }
            | GitOperation::CherryPick { oid, mainline } => {
                if *mainline == Some(0) {
                    return Ok(MutationOutcome::NotApplied {
                        failure: MutationFailure::Rejected {
                            reason: "Choose a positive parent number for the merge commit".into(),
                        },
                    });
                }
                let mut command = self.git_mutation(
                    worktree,
                    [
                        if matches!(operation, GitOperation::RevertCommit { .. }) {
                            "revert"
                        } else {
                            "cherry-pick"
                        },
                        "--no-edit",
                    ],
                );
                if let Some(parent) = mainline {
                    command = command.arg("--mainline").arg(parent.to_string());
                }
                command.arg(oid.as_str())
            }
            GitOperation::CreateBranch { oid, name } => {
                self.git_mutation(worktree, ["branch", "--", name.as_str(), oid.as_str()])
            }
            GitOperation::IgnorePath { path } => {
                self.ignore_path(worktree, path, cancellation)?;
                return Ok(MutationOutcome::Applied { paths: vec![path.clone()] });
            }
        };
        command = command.without_timeout();
        if let Some(outcome) = self.operation_failure(&command, cancellation)? {
            return Ok(outcome);
        }
        if let GitOperation::ResolveConflict { path, .. } = operation {
            return self.stage(
                &StageRequest {
                    worktree: worktree.to_owned(),
                    selection: vec![PathSelection::new(path.clone())],
                },
                cancellation,
            );
        }
        Ok(MutationOutcome::Applied { paths: Vec::new() })
    }

    fn operation_failure(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<Option<MutationOutcome>, RepositoryError> {
        Ok(match self.run(command, cancellation) {
            Ok(output) if output.success => None,
            Ok(output) => Some(MutationOutcome::OutcomeUnknown {
                failure: git_failure(command, output).into(),
            }),
            Err(failure) if !failure.started => Some(MutationOutcome::NotApplied {
                failure: failure.into(),
            }),
            Err(failure) => Some(MutationOutcome::OutcomeUnknown {
                failure: failure.into(),
            }),
        })
    }

    /// The remotes configured for one worktree.
    ///
    /// Separate from [`Self::snapshot`] because a caller that only needs to
    /// know whether a repository has a remote at all must not be made to pay
    /// for a status, two numstats, a file listing and a ref walk.
    pub fn remotes(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Vec<RemoteRecord>, RepositoryError> {
        let command = self.remotes_command(worktree);
        let output = self.run(&command, cancellation)?;
        // `--get-regexp` exits 1 when nothing matches, which only means the
        // repository has no remotes configured.
        if output.success {
            let parsed = parse_remote_config(&output.stdout);
            invalid_output(parsed, output)
        } else if output.exit_code == Some(1) {
            Ok(Vec::new())
        } else {
            Err(git_failure(&command, output).into())
        }
    }

    /// The commit HEAD points at, or `None` on an unborn branch.
    pub fn resolve_head(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Option<Oid>, RepositoryError> {
        self.resolve_optional_revision(worktree, "HEAD", cancellation)
    }

    /// Resolves a revision to a commit id, or `None` when it does not exist.
    ///
    /// `--quiet` makes an unresolvable revision exit 1 with no output, which is
    /// how an unborn branch is distinguished from a broken repository.
    fn resolve_optional_revision(
        &self,
        worktree: &Path,
        revision: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<Oid>, RepositoryError> {
        let command = self
            .git_read(
                worktree,
                [
                    OsString::from("rev-parse"),
                    OsString::from("--verify"),
                    OsString::from("--quiet"),
                    OsString::from("--end-of-options"),
                    format!("{revision}^{{commit}}").into(),
                ],
            )
            .env("GIT_NO_LAZY_FETCH", "1");
        let output = self.run(&command, cancellation)?;
        if !output.success {
            return if output.exit_code == Some(1) {
                Ok(None)
            } else {
                Err(git_failure(&command, output).into())
            };
        }
        let line = parse_utf8_line(&output.stdout, "revision")?;
        Oid::parse_object(line)
            .map(Some)
            .map_err(|error| RepositoryError::InvalidOutput {
                field: "revision",
                reason: error.to_string(),
            })
    }

    /// The `git log` page for one history request.
    pub fn history_command(
        &self,
        worktree: &Path,
        start: &Oid,
        page: u32,
        path: Option<&RepoPath>,
    ) -> CommandSpec {
        let mut arguments = vec![
            OsString::from("log"),
            OsString::from("--no-color"),
            OsString::from("-z"),
            format!("--format={HISTORY_FORMAT}").into(),
            OsString::from("--date=raw"),
            format!("--skip={}", u64::from(page) * u64::from(HISTORY_PAGE_SIZE)).into(),
            format!("--max-count={HISTORY_PAGE_SIZE}").into(),
            OsString::from(start.as_str()),
        ];
        if let Some(path) = path {
            arguments.push(OsString::from("--"));
            arguments.push(path.as_os_str().to_owned());
        }
        self.git_literal_read(worktree, arguments)
    }

    /// One page of history, newest first.
    pub fn history(
        &self,
        request: &HistoryRequest,
        cancellation: &CancellationToken,
    ) -> Result<Vec<CommitSummary>, RepositoryError> {
        let start = match &request.revision {
            // Resolved before walking so that every page of one paging session
            // is taken from the same starting commit, and so that an unborn
            // branch is an empty history rather than a `git log` failure.
            HistoryRevision::Head => match self.resolve_head(&request.worktree, cancellation)? {
                Some(oid) => oid,
                None => return Ok(Vec::new()),
            },
            HistoryRevision::Object { oid } => oid.clone(),
            HistoryRevision::Reference { name } => {
                match self.resolve_optional_revision(
                    &request.worktree,
                    name.as_str(),
                    cancellation,
                )? {
                    Some(oid) => oid,
                    None => return Ok(Vec::new()),
                }
            }
        };
        let command = self.history_command(
            &request.worktree,
            &start,
            request.page,
            request.path.as_ref(),
        );
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_history(&output.stdout);
        invalid_output(parsed, output)
    }

    /// The merge base of `base_ref` and HEAD, which is what a branch
    /// comparison must diff against: comparing against the base tip would also
    /// show every commit the base gained since this branch left it.
    pub fn resolve_compare_base(
        &self,
        worktree: &Path,
        base_ref: &ValidatedRefName,
        cancellation: &CancellationToken,
    ) -> Result<Oid, RepositoryError> {
        self.resolve_branch_range(worktree, base_ref, cancellation)
            .map(|(_, merge_base, _)| merge_base)
    }

    /// Resolves `(head, merge_base, base_tip)` for an immutable branch diff.
    pub fn resolve_branch_range(
        &self,
        worktree: &Path,
        base_ref: &ValidatedRefName,
        cancellation: &CancellationToken,
    ) -> Result<(Oid, Oid, Oid), RepositoryError> {
        let head =
            self.resolve_head(worktree, cancellation)?
                .ok_or(RepositoryError::InvalidOutput {
                    field: "head",
                    reason: "an unborn branch has nothing to compare".into(),
                })?;
        let base = self
            .resolve_optional_revision(worktree, base_ref.as_str(), cancellation)?
            .ok_or_else(|| RepositoryError::InvalidOutput {
                field: "base_ref",
                reason: format!("{base_ref} does not resolve to a commit"),
            })?;
        let command = self
            .git_read(worktree, ["merge-base", base.as_str(), head.as_str()])
            .env("GIT_NO_LAZY_FETCH", "1");
        let output = self.run_checked(&command, cancellation)?;
        let line = parse_utf8_line(&output.stdout, "merge_base")?;
        let merge_base =
            Oid::parse_object(line).map_err(|error| RepositoryError::InvalidOutput {
                field: "merge_base",
                reason: error.to_string(),
            })?;
        Ok((head, merge_base, base))
    }

    /// Files that differ between the merge base and HEAD.
    pub fn branch_diff_entries(
        &self,
        worktree: &Path,
        base_ref: &ValidatedRefName,
        cancellation: &CancellationToken,
    ) -> Result<Vec<DiffEntry>, RepositoryError> {
        self.branch_comparison(worktree, base_ref, cancellation)
            .map(|comparison| comparison.entries)
    }

    /// Compares locally resolved immutable commits, without fetching missing
    /// objects. The returned HEAD is the one actually used by both diff reads.
    pub fn branch_comparison(
        &self,
        worktree: &Path,
        base_ref: &ValidatedRefName,
        cancellation: &CancellationToken,
    ) -> Result<BranchComparison, RepositoryError> {
        let (head, merge_base, base_oid) =
            self.resolve_branch_range(worktree, base_ref, cancellation)?;
        let entries = self.range_entries(worktree, Some(&merge_base), &head, cancellation)?;
        Ok(BranchComparison {
            base_ref: base_ref.clone(),
            base_oid,
            head,
            merge_base,
            entries,
        })
    }

    /// Fetches one pull request's head and base branch into the private
    /// [`PULL_REQUEST_REF_NAMESPACE`] and resolves the range to review.
    ///
    /// The fetch names exactly two refspecs and forces only the private refs,
    /// with `--no-tags`, no submodule recursion and no `FETCH_HEAD` write, so
    /// no branch, tag or remote-tracking ref of the user's can move. Fetches
    /// for one repository are serialized process-wide: two review panes
    /// opening at once would otherwise race Git's own ref lock and one of
    /// them would fail for no reason the user can see.
    ///
    /// `expected_head` is the commit the caller read from the forge. When the
    /// remote now serves a different head, the fetch has still updated the
    /// private ref, and the caller gets [`RepositoryError::PullRequestHeadMoved`]
    /// rather than a range whose line numbers no longer match the review
    /// comments it was about to anchor.
    pub fn fetch_pull_request_range(
        &self,
        worktree: &Path,
        remote: &str,
        number: u64,
        base_branch: &ValidatedBranchName,
        expected_head: &Oid,
        cancellation: &CancellationToken,
    ) -> Result<PullRequestRange, RepositoryError> {
        let _remote = self.checked_remote(worktree, remote, cancellation)?;
        let (head_ref, base_ref) = pull_request_refs(number)?;
        let output = self.run_checked(
            &self.git_read(
                worktree,
                ["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ),
            cancellation,
        )?;
        let common_dir = parse_path_line(&output.stdout, "common_dir")?;
        let _guard = PULL_REQUEST_FETCH_LOCKS.lock(&common_dir);
        let fetch = |refspec: String| {
            self.git_mutation(
                worktree,
                [
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--no-recurse-submodules",
                    "--no-write-fetch-head",
                    "--end-of-options",
                    remote,
                    &refspec,
                ],
            )
        };
        let resolve = |reference: &ValidatedRefName, field: &'static str| {
            self.resolve_optional_revision(worktree, reference.as_str(), cancellation)?
                .ok_or_else(|| RepositoryError::InvalidOutput {
                    field,
                    reason: format!("{reference} was not written by the fetch"),
                })
        };
        // The head is fetched on its own first: a moved head is settled before
        // any time is spent on the base, and a base the remote no longer
        // serves cannot take the head down with it in one refused fetch.
        self.run_checked(
            &fetch(format!("+refs/pull/{number}/head:{}", head_ref.as_str())),
            cancellation,
        )?;
        let head = resolve(&head_ref, "pull_request_head")?;
        if head != *expected_head {
            return Err(RepositoryError::PullRequestHeadMoved {
                expected: expected_head.clone(),
                actual: head,
            });
        }
        let base_refspec = format!("+refs/heads/{}:{}", base_branch.as_str(), base_ref.as_str());
        match self.run_checked(&fetch(base_refspec), cancellation) {
            Ok(_) => {}
            Err(failure) if base_ref_is_missing(&failure) => {
                return Err(RepositoryError::PullRequestBaseUnavailable {
                    base_branch: base_branch.clone(),
                    failure: Box::new(failure),
                });
            }
            Err(failure) => return Err(failure.into()),
        }
        let base_tip = resolve(&base_ref, "pull_request_base")?;
        let command = self
            .git_read(worktree, ["merge-base", base_tip.as_str(), head.as_str()])
            .env("GIT_NO_LAZY_FETCH", "1");
        let output = self.run_checked(&command, cancellation)?;
        let line = parse_utf8_line(&output.stdout, "merge_base")?;
        let merge_base =
            Oid::parse_object(line).map_err(|error| RepositoryError::InvalidOutput {
                field: "merge_base",
                reason: error.to_string(),
            })?;
        Ok(PullRequestRange {
            number,
            base_branch: base_branch.clone(),
            head_mirror: head_ref,
            base_mirror: base_ref,
            head,
            base_tip,
            merge_base,
        })
    }

    /// Deletes the private pull request refs whose number `retain` rejects,
    /// returning the numbers it removed.
    ///
    /// A fetched pair pins its objects against `gc` for as long as it exists,
    /// so a caller that knows which pull requests are still worth reviewing —
    /// typically the open ones — prunes the rest. Refs outside
    /// [`PULL_REQUEST_REF_NAMESPACE`], and anything in it that is not a
    /// `<number>/head` or `<number>/base` pair this application writes, are
    /// never touched.
    pub fn prune_pull_request_refs(
        &self,
        worktree: &Path,
        retain: impl Fn(u64) -> bool,
        cancellation: &CancellationToken,
    ) -> Result<Vec<u64>, RepositoryError> {
        let output = self.run_checked(
            &self.git_read(
                worktree,
                ["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ),
            cancellation,
        )?;
        let common_dir = parse_path_line(&output.stdout, "common_dir")?;
        let _guard = PULL_REQUEST_FETCH_LOCKS.lock(&common_dir);
        let listing = self.run_checked(
            &self.git_read(
                worktree,
                [
                    "for-each-ref",
                    "--format=%(refname)",
                    &format!("{PULL_REQUEST_REF_NAMESPACE}/"),
                ],
            ),
            cancellation,
        )?;
        let listing =
            String::from_utf8(listing.stdout).map_err(|error| RepositoryError::InvalidOutput {
                field: "pull_request_refs",
                reason: error.to_string(),
            })?;
        let mut pruned = Vec::new();
        for name in listing.lines() {
            let Some(number) = pull_request_ref_number(name) else {
                continue;
            };
            if retain(number) {
                continue;
            }
            let reference = ValidatedRefName::parse(name)?;
            self.run_checked(
                &self.git_mutation(
                    worktree,
                    ["update-ref", "-d", "--end-of-options", reference.as_str()],
                ),
                cancellation,
            )?;
            if !pruned.contains(&number) {
                pruned.push(number);
            }
        }
        Ok(pruned)
    }

    /// Files that differ between two commits' trees, with rename detection and
    /// line counts, in the same form the branch and commit listings use.
    pub fn range_diff_entries(
        &self,
        worktree: &Path,
        old: &Oid,
        new: &Oid,
        cancellation: &CancellationToken,
    ) -> Result<Vec<DiffEntry>, RepositoryError> {
        self.range_entries(worktree, Some(old), new, cancellation)
    }

    /// Files a commit changed relative to `parent`, or to the empty tree when
    /// `parent` is `None`.
    pub fn commit_diff_entries(
        &self,
        worktree: &Path,
        oid: &Oid,
        parent: Option<&Oid>,
        cancellation: &CancellationToken,
    ) -> Result<Vec<DiffEntry>, RepositoryError> {
        self.range_entries(worktree, parent, oid, cancellation)
    }

    fn range_entries(
        &self,
        worktree: &Path,
        old: Option<&Oid>,
        new: &Oid,
        cancellation: &CancellationToken,
    ) -> Result<Vec<DiffEntry>, RepositoryError> {
        let command = self.range_diff_command(worktree, old, new, "--name-status");
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_name_status(&output.stdout);
        let mut entries = invalid_output(parsed, output)?;
        let command = self.range_diff_command(worktree, old, new, "--numstat");
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_numstat(&output.stdout);
        let records = invalid_output(parsed, output)?;
        merge_diff_numstat(&mut entries, &records);
        Ok(entries)
    }

    fn range_diff_command(
        &self,
        worktree: &Path,
        old: Option<&Oid>,
        new: &Oid,
        format: &str,
    ) -> CommandSpec {
        let mut arguments = vec![
            OsString::from("diff-tree"),
            OsString::from("-r"),
            OsString::from("-z"),
            OsString::from("--no-commit-id"),
            OsString::from("--find-renames"),
            OsString::from("--no-color"),
            OsString::from(format),
        ];
        match old {
            Some(old) => {
                arguments.extend([OsString::from(old.as_str()), OsString::from(new.as_str())])
            }
            // `--root` compares a root commit against the empty tree without
            // this service having to know the repository's hash algorithm and
            // therefore its empty-tree id.
            None => arguments.extend([OsString::from("--root"), OsString::from(new.as_str())]),
        }
        self.git_read(worktree, arguments)
            .env("GIT_NO_LAZY_FETCH", "1")
    }

    /// Reads both sides of one path's comparison.
    ///
    /// Blobs are read lazily, one path at a time, because a repository-wide
    /// prefetch would read megabytes the user never looks at.
    pub fn diff(
        &self,
        request: &DiffRequest,
        cancellation: &CancellationToken,
    ) -> Result<DiffContent, RepositoryError> {
        let worktree = request.worktree.as_path();
        let old_path = request.old_path.as_ref().unwrap_or(&request.path);
        let comparison = |old, new, old_label, new_label, source_head, modes| DiffContent::Comparison {
            old, new, old_label, new_label, source_head, modes,
        };
        match &request.basis {
            DiffBasis::IndexToWorktree => {
                let stages = self.index_entries(worktree, old_path, cancellation)?;
                let (worktree_side, worktree_mode) = read_worktree_side(worktree, &request.path);
                let stage = |number: u8| stages.iter().find(|entry| entry.stage == number);
                if stage(0).is_none() && (1..=3).any(|number| stage(number).is_some()) {
                    return Ok(DiffContent::Unmerged {
                        base: self.entry_side(worktree, stage(1), cancellation)?,
                        ours: self.entry_side(worktree, stage(2), cancellation)?,
                        theirs: self.entry_side(worktree, stage(3), cancellation)?,
                        worktree: worktree_side,
                    });
                }
                Ok(comparison(
                    self.entry_side(worktree, stage(0), cancellation)?, worktree_side,
                    "Index".into(), "Working tree".into(), None,
                    [stage(0).map(|entry| entry.mode), worktree_mode],
                ))
            }
            DiffBasis::HeadToIndex => {
                // Resolve once: metadata and bytes must refer to the same HEAD,
                // even if another process commits while the blobs are read.
                let head = self.resolve_head(worktree, cancellation)?;
                let (old, old_mode) = match head.as_ref() {
                    Some(head) => self.tree_side(worktree, head, old_path, cancellation)?,
                    None => (DiffSide::Absent, None),
                };
                let source_head = Some(match head { Some(head) => GitHeadGuard::Commit(head), None => GitHeadGuard::Unborn });
                let stages = self.index_entries(worktree, &request.path, cancellation)?;
                let staged = stages.iter().find(|entry| entry.stage == 0);
                Ok(comparison(old, self.entry_side(worktree, staged, cancellation)?,
                    "HEAD".into(), "Index".into(), source_head,
                    [old_mode, staged.map(|entry| entry.mode)]))
            }
            DiffBasis::MergeBaseToHead { base_ref, head, merge_base } => {
                let (old, old_mode) = self.tree_side(worktree, merge_base, old_path, cancellation)?;
                let (new, new_mode) = self.tree_side(worktree, head, &request.path, cancellation)?;
                Ok(comparison(old, new, format!("{} ({})", base_ref.short_name(), merge_base.short()),
                    format!("HEAD ({})", head.short()), None, [old_mode, new_mode]))
            }
            DiffBasis::CommitToParent { oid, parent } => {
                let (old, old_mode) = match parent {
                    Some(parent) => self.tree_side(worktree, parent, old_path, cancellation)?,
                    None => (DiffSide::Absent, None),
                };
                let (new, new_mode) = self.tree_side(worktree, oid, &request.path, cancellation)?;
                Ok(comparison(old, new, match parent { Some(parent) => parent.short().to_owned(), None => "root commit".into() },
                    oid.short().to_owned(), None, [old_mode, new_mode]))
            }
            DiffBasis::PullRequest { number, base_ref, head, merge_base } => {
                let (old, old_mode) = self.tree_side(worktree, merge_base, old_path, cancellation)?;
                let (new, new_mode) = self.tree_side(worktree, head, &request.path, cancellation)?;
                Ok(comparison(old, new, format!("{} ({})", base_ref.short_name(), merge_base.short()),
                    format!("#{number} ({})", head.short()), None, [old_mode, new_mode]))
            }
        }
    }

    /// Index entries for exactly one path, including every conflict stage.
    fn index_entries(
        &self,
        worktree: &Path,
        path: &RepoPath,
        cancellation: &CancellationToken,
    ) -> Result<Vec<IndexEntry>, RepositoryError> {
        let command = self.git_literal_read(
            worktree,
            [
                OsString::from("ls-files"),
                OsString::from("-s"),
                OsString::from("-z"),
                OsString::from("--"),
                path.as_os_str().to_owned(),
            ],
        );
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_index_entries(&output.stdout);
        let entries = invalid_output(parsed, output)?;
        // A pathspec also matches everything beneath a directory of the same
        // name, so only the exact path is kept.
        Ok(entries
            .into_iter()
            .filter(|entry| entry.path == *path)
            .collect())
    }

    fn entry_side(
        &self,
        worktree: &Path,
        entry: Option<&IndexEntry>,
        cancellation: &CancellationToken,
    ) -> Result<DiffSide, RepositoryError> {
        match entry {
            Some(entry) => self.object_side(worktree, entry.mode, &entry.oid, cancellation),
            None => Ok(DiffSide::Absent),
        }
    }

    fn tree_side(
        &self,
        worktree: &Path,
        revision: &Oid,
        path: &RepoPath,
        cancellation: &CancellationToken,
    ) -> Result<(DiffSide, Option<u32>), RepositoryError> {
        let command = self.git_literal_read(
            worktree,
            [
                OsString::from("ls-tree"),
                OsString::from("-z"),
                OsString::from(revision.as_str()),
                OsString::from("--"),
                path.as_os_str().to_owned(),
            ],
        );
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_tree_entries(&output.stdout);
        let entries = invalid_output(parsed, output)?;
        match entries.into_iter().find(|entry| entry.path == *path) {
            Some(entry) => self.object_side(worktree, entry.mode, &entry.oid, cancellation).map(|side| (side, Some(entry.mode))),
            None => Ok((DiffSide::Absent, None)),
        }
    }

    /// One side read from the object database, classified by its file mode.
    fn object_side(
        &self,
        worktree: &Path,
        mode: u32,
        oid: &Oid,
        cancellation: &CancellationToken,
    ) -> Result<DiffSide, RepositoryError> {
        if mode == GITLINK_MODE {
            // A gitlink's object is a commit in another repository, not
            // content this repository can read.
            return Ok(DiffSide::Submodule {
                oid: Some(oid.clone()),
            });
        }
        let command = self
            .git_read(worktree, ["cat-file", "--batch"])
            .stdin(format!("{oid}\n").into_bytes())
            // The header arrives before the content, so even a truncated read
            // still carries the true size and can report oversize precisely.
            .output_limits(
                MAX_DIFF_SIDE_BYTES as usize + BATCH_HEADER_SLACK,
                DEFAULT_STDERR_LIMIT,
            );
        let output = match self.executor.execute(&command, cancellation) {
            Ok(output) if output.success => output,
            Ok(output) => return Err(git_failure(&command, output).into()),
            // An object too large for one side of a comparison is an answer,
            // not a failure, so this one kind is deliberately not logged as
            // one.
            Err(failure) if failure.kind == ProcessFailureKind::OutputLimit => {
                return Ok(DiffSide::Oversize {
                    bytes: batch_header_size(&failure.stdout).unwrap_or(MAX_DIFF_SIDE_BYTES + 1),
                });
            }
            Err(failure) => {
                log_git_failure(&command, &failure);
                return Err(failure.into());
            }
        };
        let parsed = parse_batch_blobs(&output.stdout, "git cat-file --batch");
        let mut objects = invalid_output(parsed, output)?;
        let bytes = match objects.pop() {
            Some(BatchObject::Content(bytes)) => bytes,
            // Git understood itself perfectly well; the object simply is not
            // here. Reporting that as malformed output blamed the wrong thing
            // and hid the only actionable fact, which is that this side of the
            // comparison cannot be read from this repository.
            Some(BatchObject::Missing) => {
                return Ok(DiffSide::Unreadable {
                    reason: format!("{oid} is not in this repository"),
                });
            }
            None => {
                return Err(RepositoryError::InvalidOutput {
                    field: "cat_file_batch",
                    reason: "no object was returned".into(),
                });
            }
        };
        if bytes.len() as u64 > MAX_DIFF_SIDE_BYTES {
            return Ok(DiffSide::Oversize {
                bytes: bytes.len() as u64,
            });
        }
        if mode == SYMLINK_MODE {
            // A symlink's blob is its target, which is a path rather than file
            // content and must keep its bytes.
            return Ok(DiffSide::Symlink {
                target: bytes_to_path(&bytes),
            });
        }
        Ok(classify_bytes(bytes))
    }

    fn git_pathspec_mutation<'a>(
        &self,
        worktree: &Path,
        leading: impl IntoIterator<Item = &'a str>,
        paths: &[RepoPath],
    ) -> CommandSpec {
        self.git_literal_mutation(worktree, leading)
            .args(["--pathspec-from-file=-", "--pathspec-file-nul"])
            .stdin(pathspec_input(paths))
    }

    /// The `git add` invocation for an already-resolved pathspec list.
    pub fn stage_command(
        &self,
        worktree: &Path,
        paths: &[RepoPath],
    ) -> Result<CommandSpec, ValidationError> {
        if paths.is_empty() {
            return Err(ValidationError::EmptySelection);
        }
        Ok(self.git_pathspec_mutation(worktree, ["add", "-A"], paths))
    }

    /// Check the current unmerged index at queue execution, never at the time
    /// a row was clicked. An explicit approval applies only to those exact
    /// scanned file identities, so editing a waiting file requires a new answer.
    pub fn check_stage_markers(
        &self,
        request: &StageRequest,
        approved: &BTreeMap<RepoPath, ConflictMarkerVersion>,
        cancellation: &CancellationToken,
    ) -> Result<Option<BTreeMap<RepoPath, ConflictMarkerVersion>>, RepositoryError> {
        use std::os::unix::fs::MetadataExt;
        let command = self.git_read(&request.worktree, ["ls-files", "--unmerged", "-z"]);
        let output = self.run_checked(&command, cancellation)?;
        let entries = invalid_output(parse_index_entries(&output.stdout), output)?;
        let mut conflicted = entries
            .into_iter()
            .map(|entry| entry.path)
            .collect::<HashSet<_>>();
        conflicted.extend(approved.keys().cloned());
        let paths = selection_pathspecs(&request.selection);
        if !paths.iter().any(|path| conflicted.contains(path)) {
            return Ok(None);
        }
        let root = fs_safety::Root::open(&request.worktree)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let version = |metadata: &fs::Metadata| {
            (
                metadata.dev(),
                metadata.ino(),
                metadata.len(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            )
        };
        let mut markers = BTreeMap::new();
        for path in paths.into_iter().filter(|path| conflicted.contains(path)) {
            let metadata = match fs_safety::metadata_beneath(&root, path.as_path()) {
                Ok(metadata) => metadata,
                Err(error) if error.is_not_found() => continue,
                Err(error) => return Err(ValidationError::Io(error.to_string()).into()),
            };
            if !metadata.is_file() {
                continue;
            }
            let mut file = fs_safety::open_beneath(&root, path.as_path())
                .map_err(|error| ValidationError::Io(error.to_string()))?;
            let opened = file
                .metadata()
                .map_err(|error| ValidationError::Io(error.to_string()))?;
            let remaining = scan_conflict_markers(&mut file, cancellation)
                .map_err(|error| ValidationError::Io(error.to_string()))?;
            let after = file
                .metadata()
                .map_err(|error| ValidationError::Io(error.to_string()))?;
            let named = fs_safety::metadata_beneath(&root, path.as_path())
                .map_err(|error| ValidationError::Io(error.to_string()))?;
            if version(&metadata) != version(&opened)
                || version(&opened) != version(&after)
                || version(&after) != version(&named)
            {
                return Err(ValidationError::Io(format!(
                    "{path} changed while checking conflict markers; retry staging"
                ))
                .into());
            }
            if remaining {
                markers.insert(path, version(&after));
            }
        }
        Ok(markers
            .iter()
            .any(|(path, version)| approved.get(path) != Some(version))
            .then_some(markers))
    }

    /// Stages the selected paths, including the deletion half of a rename the
    /// index still has.
    pub fn stage(
        &self,
        request: &StageRequest,
        cancellation: &CancellationToken,
    ) -> Result<MutationOutcome, RepositoryError> {
        // Keep the actual mutation protected even if its UI task is dropped.
        let _protection = crate::worktree_cleanup::protect(&request.worktree)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let paths =
            self.matchable_pathspecs(&request.worktree, &request.selection, cancellation)?;
        let command = self.stage_command(&request.worktree, &paths)?;
        Ok(self.run_mutation(&command, paths, MutationScope::Incremental, cancellation))
    }

    /// The pathspecs a worktree-side mutation can actually match.
    ///
    /// `git add` and `git restore --worktree` resolve a pathspec against the
    /// index and the working tree, never against HEAD. The source half of a
    /// *staged* rename is in neither — only HEAD still has that name — so
    /// listing it makes `add` exit 128 having staged nothing and `restore` exit
    /// 1 having restored nothing, for the whole selection. Whether the source
    /// is matchable is a property of the index at this moment rather than of
    /// the selection, which is why it is asked here and not at the point the
    /// selection was built: the same [`PathSelection`] names an unmatchable
    /// source while the rename is staged and a matchable one once it is not,
    /// and the unstaged rename genuinely has to be staged as a pair.
    fn matchable_pathspecs(
        &self,
        worktree: &Path,
        selection: &[PathSelection],
        cancellation: &CancellationToken,
    ) -> Result<Vec<RepoPath>, RepositoryError> {
        let sources: Vec<RepoPath> = selection
            .iter()
            .filter_map(|entry| entry.old_path.clone())
            .collect();
        let indexed = if sources.is_empty() {
            Vec::new()
        } else {
            self.indexed_paths(worktree, &sources, cancellation)?
        };
        let indexed: HashSet<_> = indexed.into_iter().collect();
        let mut paths = Vec::with_capacity(selection.len() + indexed.len());
        let mut seen = HashSet::new();
        for entry in selection {
            let source = entry
                .old_path
                .as_ref()
                .filter(|source| indexed.contains(source));
            for path in [source, Some(&entry.path)].into_iter().flatten() {
                if seen.insert(path.clone()) {
                    paths.push(path.clone());
                }
            }
        }
        Ok(paths)
    }

    pub fn unstage(
        &self,
        request: &UnstageRequest,
        cancellation: &CancellationToken,
    ) -> Result<MutationOutcome, RepositoryError> {
        // Keep the actual mutation protected even if its UI task is dropped.
        let _protection = crate::worktree_cleanup::protect(&request.worktree)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let paths = selection_pathspecs(&request.selection);
        if paths.is_empty() {
            return Err(ValidationError::EmptySelection.into());
        }
        if let Some(head) = self.resolve_head(&request.worktree, cancellation)? {
            // `reset` silently ignores pathspecs that match nothing and still
            // exits 0, so a stale selection would report ghosts as applied
            // while unstaging the rest. Refuse the whole request up front
            // when any selected path resolves to nothing git knows — the
            // index, or the HEAD tree for renamed-away sources.
            let indexed: HashSet<_> = self
                .indexed_paths(&request.worktree, &paths, cancellation)?
                .into_iter()
                .collect();
            let unindexed: Vec<&RepoPath> = paths
                .iter()
                .filter(|path| !indexed.contains(path))
                .collect();
            if !unindexed.is_empty() {
                let in_head: HashSet<_> = self
                    .head_tree_paths(&request.worktree, &head, &unindexed, cancellation)?
                    .into_iter()
                    .collect();
                if let Some(ghost) = unindexed.iter().find(|path| !in_head.contains(path)) {
                    return Ok(MutationOutcome::NotApplied {
                        failure: MutationFailure::Rejected {
                            reason: format!("{ghost}: path does not match anything known to git"),
                        },
                    });
                }
            }
            // `restore --staged` preloads worktree stat information and runs
            // post-checkout hooks. Neither belongs in an index-only operation.
            // A path-limited reset never moves HEAD; --no-refresh also skips
            // the otherwise repository-wide stat refresh after updating it.
            let command = self.git_pathspec_mutation(
                &request.worktree,
                ["reset", "--no-refresh", "--quiet", head.as_str()],
                &paths,
            );
            return Ok(self.run_mutation(
                &command,
                paths,
                MutationScope::AllOrNothing,
                cancellation,
            ));
        }
        // An unborn branch has no HEAD to restore from. Remove only selected
        // indexed paths without inspecting working files (`rm --cached` would
        // refresh their stat data and refuse a file edited since staging).
        let indexed = self.indexed_paths(&request.worktree, &paths, cancellation)?;
        if indexed.is_empty() {
            return Ok(MutationOutcome::Applied { paths: Vec::new() });
        }
        let command = self
            .git_literal_mutation(
                &request.worktree,
                ["update-index", "--force-remove", "-z", "--stdin"],
            )
            .stdin(pathspec_input(&indexed));
        Ok(self.run_mutation(&command, indexed, MutationScope::AllOrNothing, cancellation))
    }

    /// Filters a selection down to the paths the index actually contains,
    /// preserving the caller's order.
    fn indexed_paths(
        &self,
        worktree: &Path,
        paths: &[RepoPath],
        cancellation: &CancellationToken,
    ) -> Result<Vec<RepoPath>, RepositoryError> {
        // ls-files has no stdin pathspec support. Read the index once and
        // filter by hashed membership; argv stays bounded for every selection.
        let command = self.git_literal_read(worktree, ["ls-files", "-z"]);
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_path_list(&output.stdout, "git ls-files");
        let listed: HashSet<_> = invalid_output(parsed, output)?.into_iter().collect();
        Ok(paths
            .iter()
            .filter(|path| listed.contains(path))
            .cloned()
            .collect())
    }

    /// Filters paths down to the ones present in a commit's tree, preserving
    /// the caller's order. Renamed-away sources live here even after leaving
    /// the index, so this is the second half of "matches something git
    /// knows" for selections that may name renames.
    fn head_tree_paths(
        &self,
        worktree: &Path,
        revision: &Oid,
        paths: &[&RepoPath],
        cancellation: &CancellationToken,
    ) -> Result<Vec<RepoPath>, RepositoryError> {
        // ls-tree likewise lacks stdin pathspecs. A recursive listing plus
        // membership filtering avoids both ARG_MAX and quadratic scans.
        let command = self.git_literal_read(worktree, ["ls-tree", "-r", "-z", revision.as_str()]);
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_tree_entries(&output.stdout);
        let entries = invalid_output(parsed, output)?;
        let listed: HashSet<RepoPath> = entries.into_iter().map(|entry| entry.path).collect();
        Ok(paths
            .iter()
            .filter(|path| listed.contains(*path))
            .map(|path| (*path).clone())
            .collect())
    }

    /// Restores tracked paths from the index and deletes untracked files.
    ///
    /// This destroys work that exists nowhere else, so it refuses to run
    /// without the caller's confirmation and never removes a directory.
    pub fn discard(
        &self,
        request: &DiscardRequest,
        cancellation: &CancellationToken,
    ) -> Result<MutationOutcome, RepositoryError> {
        // Keep the actual mutation protected even if its UI task is dropped.
        let _protection = crate::worktree_cleanup::protect(&request.worktree)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        if !request.confirmed {
            return Err(ValidationError::ConfirmationRequired.into());
        }
        if !request.worktree.is_absolute() {
            return Err(
                ValidationError::InvalidWorktree(request.worktree.display().to_string()).into(),
            );
        }
        let tracked =
            self.matchable_pathspecs(&request.worktree, &request.tracked, cancellation)?;
        let mut untracked: Vec<RepoPath> = Vec::with_capacity(request.untracked.len());
        let mut seen = HashSet::new();
        for path in &request.untracked {
            if seen.insert(path.clone()) {
                untracked.push(path.clone());
            }
        }
        if tracked.is_empty() && untracked.is_empty() {
            return Err(ValidationError::EmptySelection.into());
        }

        let mut applied = Vec::new();
        if !tracked.is_empty() {
            let command =
                self.git_pathspec_mutation(&request.worktree, ["restore", "--worktree"], &tracked);
            // A `restore` that reports a refusal changed nothing, and one that
            // failed inside its write loop changed an amount only the user can
            // establish. Neither leaves a half of the request that this may
            // finish on its own: unlinking the untracked half afterwards would
            // destroy files as part of an operation the caller is being told
            // did not happen, or is being told to go and check.
            match self.run_mutation(
                &command,
                tracked,
                MutationScope::AtomicRefusal,
                cancellation,
            ) {
                MutationOutcome::Applied { mut paths } => applied.append(&mut paths),
                outcome => return Ok(outcome),
            }
        }
        if untracked.is_empty() {
            return Ok(MutationOutcome::Applied { paths: applied });
        }

        // This is Git's actual worktree index, including linked worktrees.
        // Hold its standard lock while classifying and unlinking: otherwise a
        // terminal's `git add` can make a selected path tracked after the read.
        let mut index_lock = match self.discard_index_lock(&request.worktree, cancellation) {
            Ok(lock) => lock,
            Err(error) => {
                return Ok(report_discard(
                    applied,
                    discard_failures(&untracked, format!("cannot protect the index: {error}")),
                ));
            }
        };
        let mut failures = (|| {
            // The confirmation may have outlived the snapshot. Re-establish
            // tracking under the lock, before deleting any untracked file.
            let tracked_now = match self.indexed_paths(&request.worktree, &untracked, cancellation)
            {
                Ok(paths) => paths.into_iter().collect::<HashSet<_>>(),
                Err(error) => {
                    return discard_failures(
                        &untracked,
                        format!("could not verify that the path is still untracked: {error}"),
                    );
                }
            };
            let root = match fs_safety::Root::open(&request.worktree) {
                Ok(root) => root,
                Err(error) => {
                    return discard_failures(
                        &untracked,
                        format!("{}: {error}", request.worktree.display()),
                    );
                }
            };
            // A refused directory does not prevent independent selected files
            // being attempted. Cancellation or loss of the index lock ends the
            // remaining batch, whose paths are still known untouched.
            let mut failures = Vec::new();
            for (position, path) in untracked.iter().enumerate() {
                let protected = if cancellation.is_cancelled() {
                    Err("discard cancelled before deletion".to_owned())
                } else {
                    index_lock.check()
                };
                if let Err(reason) = protected {
                    failures.extend(discard_failures(&untracked[position..], reason));
                    break;
                }
                if tracked_now.contains(path) {
                    failures.push(MutationFailure::Path {
                        path: path.clone(),
                        reason: "the path is tracked now but was untracked when it was selected"
                            .into(),
                    });
                    continue;
                }
                match delete_untracked(&root, path) {
                    Ok(()) => applied.push(path.clone()),
                    Err(failure) => failures.push(failure),
                }
            }
            failures
        })();
        if let Err(reason) = index_lock.release() {
            let cleanup = format!("could not release the discard index lock: {reason}");
            if applied.is_empty()
                && let Some(MutationFailure::Path { reason, .. }) = failures.first_mut()
            {
                // A wholly refused discard presents its first path failure.
                // Keep lock cleanup diagnostics visible there as well.
                reason.push_str("; ");
                reason.push_str(&cleanup);
            } else {
                failures.push(MutationFailure::Rejected { reason: cleanup });
            }
        }
        Ok(report_discard(applied, failures))
    }

    fn discard_index_lock(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<GitFileLock, RepositoryError> {
        let output = self.run_checked(
            &self.git_read(
                worktree,
                ["rev-parse", "--path-format=absolute", "--git-path", "index"],
            ),
            cancellation,
        )?;
        let index = parse_path_line(&output.stdout, "index_path")?;
        GitFileLock::acquire(&index).map_err(|reason| ValidationError::Io(reason).into())
    }

    pub fn commit_command(&self, request: &CommitRequest) -> Result<CommandSpec, ValidationError> {
        if request.message.trim().is_empty() {
            return Err(ValidationError::EmptyCommitMessage);
        }
        Ok(self
            .git_mutation(
                &request.worktree,
                // The message travels on stdin so that no length limit,
                // quoting rule, or temporary file is involved.
                //
                // `whitespace`, not `strip`, and for the same reason Git itself
                // defaults `--file` to it: `strip` removes every line beginning
                // with `core.commentChar`, which is what an editor session
                // needs and is silent data loss for a message that was never in
                // an editor. A message beginning `#42 fix the thing` would be
                // committed without its first line, and one written entirely in
                // such lines would be committed as empty.
                ["commit", "--file=-", "--cleanup=whitespace"],
            )
            .stdin(request.message.clone().into_bytes())
            .without_timeout())
    }

    /// True when the index differs from HEAD.
    ///
    /// `git diff --cached --quiet` exits 1 to mean "there are differences", and
    /// works on an unborn branch because Git compares against the empty tree.
    pub fn has_staged_changes(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<bool, RepositoryError> {
        let command = self.git_read(worktree, ["diff", "--cached", "--quiet"]);
        let output = self.run(&command, cancellation)?;
        match output.exit_code {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(git_failure(&command, output).into()),
        }
    }

    /// The sequencer state files whose presence means Git will accept a commit
    /// with an index identical to HEAD's tree.
    ///
    /// These are per-worktree pseudo-refs, so their location comes from Git
    /// rather than from an assumed `.git/` layout: a linked worktree keeps them
    /// in its own administrative directory.
    const PENDING_COMMIT_STATES: [&'static str; 3] =
        ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"];

    /// True when Git has a merge, cherry-pick or revert waiting to be
    /// concluded by a commit.
    ///
    /// This is how `git commit` itself decides, and it is what makes the
    /// "nothing staged" refusal correct rather than merely usual: a conflict
    /// resolved in favour of ours leaves the index equal to HEAD's tree, and
    /// `git status` reports nothing changed, yet the merge commit is still
    /// required and plain `git commit` still creates it.
    pub fn has_pending_commit_state(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<bool, RepositoryError> {
        let mut arguments = vec![OsString::from("rev-parse")];
        for state in Self::PENDING_COMMIT_STATES {
            arguments.push(OsString::from("--git-path"));
            arguments.push(OsString::from(state));
        }
        let command = self.git_read(worktree, arguments);
        let output = self.run_checked(&command, cancellation)?;
        Ok(output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(bytes_to_path)
            // `--git-path` answers relative to the working directory it ran
            // in, which is the worktree.
            .any(|path| worktree.join(path).exists()))
    }

    /// Commits the index, reconciling the outcome against HEAD.
    pub fn commit(
        &self,
        request: &CommitRequest,
        cancellation: &CancellationToken,
    ) -> Result<CommitOutcome, RepositoryError> {
        self.commit_with_amend(request, false, cancellation)
    }

    pub fn commit_with_amend(
        &self,
        request: &CommitRequest,
        amend: bool,
        cancellation: &CancellationToken,
    ) -> Result<CommitOutcome, RepositoryError> {
        // Keep the actual mutation protected even if its UI task is dropped.
        let _protection = crate::worktree_cleanup::protect(&request.worktree)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let mut command = self.commit_command(request)?;
        if amend {
            command = command.arg("--amend");
        }
        if !amend
            && !self.has_staged_changes(&request.worktree, cancellation)?
            && !self.has_pending_commit_state(&request.worktree, cancellation)?
        {
            return Ok(CommitOutcome::NotCreated {
                failure: MutationFailure::Rejected {
                    reason: "there are no staged changes to commit".into(),
                },
            });
        }
        let before = self.resolve_head(&request.worktree, cancellation)?;
        let failure = match self.run(&command, cancellation) {
            Ok(output) if output.success => None,
            Ok(output) => Some(MutationFailure::from(git_failure(&command, output))),
            // A process that never started cannot have committed.
            Err(failure) if !failure.started => {
                return Ok(CommitOutcome::NotCreated {
                    failure: failure.into(),
                });
            }
            Err(failure) => Some(MutationFailure::from(failure)),
        };
        // HEAD is the only authority on whether the commit exists: a failing
        // post-commit hook, or a cancellation after `git commit` already wrote
        // the object, both leave a real commit behind. The reconciliation runs
        // under a fresh token so a cancelled operation still learns the truth.
        let reconciliation = CancellationToken::new();
        let after = match self.resolve_head(&request.worktree, &reconciliation) {
            Ok(after) => after,
            Err(error) => {
                return Ok(CommitOutcome::OutcomeUnknown {
                    failure: failure.unwrap_or_else(|| {
                        MutationFailure::Process(repository_warning(&self.git, error))
                    }),
                });
            }
        };
        let moved = match &after {
            Some(oid) if Some(oid) != before.as_ref() => Some(oid),
            // An identical amend can produce the same object within one second.
            Some(oid) if amend && failure.is_none() => Some(oid),
            _ => None,
        };
        let Some(oid) = moved else {
            return Ok(match failure {
                Some(failure) => CommitOutcome::NotCreated { failure },
                None => CommitOutcome::OutcomeUnknown {
                    failure: MutationFailure::Rejected {
                        reason: "git reported success but HEAD did not move".into(),
                    },
                },
            });
        };
        let Some(failure) = failure else {
            // Git exited 0, which is Git saying it made this commit, and the
            // ref it moved is the one it made. A hook is entitled to rewrite
            // the message Git stored — prepending a ticket prefix is the whole
            // purpose of `prepare-commit-msg`, and reflowing one is a
            // `commit-msg` hook's — so no comparison against the message this
            // call sent can be the authority here. Making one the authority
            // reported every commit in such a repository as a failure.
            return Ok(CommitOutcome::Created {
                oid: oid.clone(),
                warnings: Vec::new(),
            });
        };
        // Git failed and HEAD moved anyway. Either this call's commit object
        // was written before whatever failed — a cancellation arriving after
        // `git commit` had already updated the ref — or the user's own
        // terminal committed in this worktree, which `MutationLocks` cannot
        // exclude, and claiming that object id would attribute a stranger's
        // commit, with a stranger's message and contents, to this call.
        //
        // The stored message is the only evidence there is, and it points one
        // way only: finding the message this call sent identifies the commit
        // as this call's, while not finding it establishes nothing, because a
        // hook may have rewritten it. So a match is reported as created with
        // the failure demoted to a warning, and everything else — a mismatch,
        // or a reconciliation read that failed itself — is the unknown outcome
        // it really is, rather than a claim in either direction. A read is
        // never allowed to turn a mutation that completed into an error.
        if matches!(
            self.carries_sent_message(&request.worktree, oid, &request.message, &reconciliation),
            Ok(true)
        ) {
            return Ok(CommitOutcome::Created {
                oid: oid.clone(),
                warnings: vec![failure],
            });
        }
        Ok(CommitOutcome::OutcomeUnknown { failure })
    }

    /// True when the commit at `oid` carries the message this call sent.
    ///
    /// Positive evidence of authorship and nothing more. A `commit-msg` hook
    /// may append to a message — a `Change-Id` trailer is the common case — so
    /// the sent message is looked for as a prefix rather than as an equal, and
    /// both sides are cleaned the way `--cleanup=whitespace` cleaned the one
    /// Git stored. A hook that *prepends*, *reflows* or replaces the message
    /// makes this false for a commit this call did write, which is why a false
    /// answer is never read as "someone else wrote it".
    fn carries_sent_message(
        &self,
        worktree: &Path,
        oid: &Oid,
        message: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, RepositoryError> {
        let command = self.git_read(
            worktree,
            [
                OsString::from("log"),
                OsString::from("-z"),
                OsString::from("--max-count=1"),
                OsString::from("--format=%B"),
                OsString::from("--end-of-options"),
                OsString::from(oid.as_str()),
            ],
        );
        let output = self.run_checked(&command, cancellation)?;
        // `-z` terminates the record, so the stored message is exactly what
        // precedes the NUL and no trailing newline has to be guessed at.
        let stored = output.stdout.strip_suffix(&[0]).unwrap_or(&output.stdout);
        Ok(cleanup_whitespace(&String::from_utf8_lossy(stored))
            .starts_with(&cleanup_whitespace(message)))
    }

    /// The branch's configured upstream as an explicit push target, or `None`
    /// when it has no upstream and the user must choose one.
    pub fn upstream_push_target(
        &self,
        worktree: &Path,
        branch: &ValidatedBranchName,
        cancellation: &CancellationToken,
    ) -> Result<Option<PushTarget>, RepositoryError> {
        let remote = self.branch_config(worktree, branch, "remote", cancellation)?;
        let merge = self.branch_config(worktree, branch, "merge", cancellation)?;
        let (Some(remote), Some(merge)) = (remote, merge) else {
            return Ok(None);
        };
        let _remote = self.checked_remote(worktree, &remote, cancellation)?;
        Ok(Some(PushTarget::Upstream {
            remote,
            remote_ref: ValidatedRefName::parse(merge)?,
        }))
    }

    /// The repository a local branch publishes to, independent of the forge's
    /// default base repository (which can be the upstream of a fork).
    pub fn branch_remote(
        &self,
        worktree: &Path,
        branch: &ValidatedBranchName,
        cancellation: &CancellationToken,
    ) -> Result<Option<RemoteRecord>, RepositoryError> {
        let command = self.git_read(
            worktree,
            ["config", "-z", "--get-regexp", r"^(remote|branch)\."],
        );
        let output = self.run(&command, cancellation)?;
        if !output.success {
            return if output.exit_code == Some(1) {
                Ok(None)
            } else {
                Err(git_failure(&command, output).into())
            };
        }
        let config = String::from_utf8_lossy(&output.stdout).into_owned();
        let remotes = invalid_output(parse_remote_config(&output.stdout), output)?;
        let values: BTreeMap<_, _> = config
            .split('\0')
            .filter_map(|entry| entry.split_once('\n'))
            .collect();
        let selected = values
            .get(format!("branch.{}.pushremote", branch.as_str()).as_str())
            .or_else(|| values.get("remote.pushdefault"))
            .or_else(|| values.get(format!("branch.{}.remote", branch.as_str()).as_str()));
        Ok(match selected {
            Some(name) => remotes.into_iter().find(|remote| remote.name == *name),
            None => remotes
                .iter()
                .find(|remote| remote.name == "origin")
                .cloned()
                .or_else(|| (remotes.len() == 1).then(|| remotes[0].clone())),
        })
    }

    fn branch_config(
        &self,
        worktree: &Path,
        branch: &ValidatedBranchName,
        key: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, RepositoryError> {
        let command = self.git_read(
            worktree,
            [
                OsString::from("config"),
                OsString::from("-z"),
                OsString::from("--get"),
                format!("branch.{}.{key}", branch.as_str()).into(),
            ],
        );
        let output = self.run(&command, cancellation)?;
        if !output.success {
            return if output.exit_code == Some(1) {
                Ok(None)
            } else {
                Err(git_failure(&command, output).into())
            };
        }
        let value = output
            .stdout
            .strip_suffix(&[0])
            .unwrap_or(&output.stdout)
            .to_vec();
        if value.is_empty() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&value).into_owned()))
    }

    /// Validates a proposed remote branch name with Git itself.
    pub fn check_ref_format_command(&self, worktree: &Path, reference: &str) -> CommandSpec {
        self.git_read(
            worktree,
            [OsString::from("check-ref-format"), reference.into()],
        )
    }

    pub fn push_command(&self, request: &PushRequest) -> Result<CommandSpec, ValidationError> {
        let remote = CheckedRemote::simple(request.target.remote())?;
        Ok(self.push_command_to(request, &request.target.remote_side_ref()?, remote))
    }

    /// The push command for one already-resolved destination, so the ref that
    /// goes on the wire and the ref every outcome check consults are one value
    /// rather than two derivations that could drift apart.
    fn push_command_to(
        &self,
        request: &PushRequest,
        destination: &RemoteSideRef,
        remote: CheckedRemote<'_>,
    ) -> CommandSpec {
        let mut arguments = vec![OsString::from("push"), OsString::from("--porcelain")];
        if request.target.sets_upstream() {
            arguments.push(OsString::from("--set-upstream"));
        }
        arguments.push(OsString::from(remote.0));
        // An explicit refspec, never a push default: the remote-side ref this
        // updates must be the one the user was shown. `--set-upstream` records
        // this refspec's own destination as `branch.<name>.merge`, so the
        // fully qualified `refs/heads/<branch>` here is also what makes the
        // tracking configuration land in the form Git expects to read back.
        // Force is never offered.
        arguments.push(push_refspec(&request.branch, destination).into());
        self.git_mutation(&request.worktree, arguments)
            .without_timeout()
    }

    /// Pushes one branch, reconciling an uncertain outcome against the remote.
    pub fn push(
        &self,
        request: &PushRequest,
        cancellation: &CancellationToken,
    ) -> Result<PushOutcome, RepositoryError> {
        // Keep the actual mutation protected even if its UI task is dropped.
        let _protection = crate::worktree_cleanup::protect(&request.worktree)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        // Resolved exactly once: the refspec Git is given, the name Git is
        // asked to validate, the ref reconciliation looks up on the remote,
        // and the ref this reports as updated are all this one value.
        let destination = request.target.remote_side_ref()?;
        let remote =
            self.checked_remote(&request.worktree, request.target.remote(), cancellation)?;
        let command = self.push_command_to(request, &destination, remote);
        if request.target.sets_upstream() {
            // The remote-side name is user text on a first publish, so Git
            // validates it before anything leaves the machine.
            let check = self.check_ref_format_command(&request.worktree, destination.as_str());
            if !self.run(&check, cancellation)?.success {
                return Ok(PushOutcome::NotPushed {
                    failure: MutationFailure::Rejected {
                        reason: format!("{destination} is not a valid ref name"),
                    },
                });
            }
        }
        // Load-bearing beyond reporting the branch's object id: this is the
        // check that stops a branch label recovered lossily from Git's own
        // output — see the `branch.head` note in `parse_status` — from ever
        // reaching a `git push`. `ValidatedBranchName::parse` screens ASCII
        // and lets U+FFFD through, so refusing a name that names no ref is
        // what keeps a name this application cannot spell out of a refspec.
        let local_ref = request.branch.full_ref();
        let Some(local_oid) =
            self.resolve_optional_revision(&request.worktree, &local_ref, cancellation)?
        else {
            return Ok(PushOutcome::NotPushed {
                failure: MutationFailure::Rejected {
                    reason: format!("{local_ref} does not exist"),
                },
            });
        };
        let refspec = push_refspec(&request.branch, &destination);
        let pushed = |oid: Oid| PushOutcome::Pushed {
            remote: request.target.remote().to_owned(),
            remote_ref: destination.clone(),
            oid,
        };
        // Capture the endpoints before Git attempts the push. External Git
        // clients may change remote configuration while the command runs;
        // evidence from a newly configured destination cannot confirm this push.
        // Capture errors do not prevent Git from returning a definitive result.
        let destinations =
            self.push_destinations(&request.worktree, request.target.remote(), cancellation);
        let failure = match self.run(&command, cancellation) {
            Ok(output) if output.success => {
                // The refspec names the branch so Git can retain first-publish
                // --set-upstream behavior. Another Git client may advance it
                // after our preflight read: never report that earlier OID as
                // published unless the observed source remains the same.
                // This observes endpoints of the attempt; it does not prevent
                // an external change or detect an A -> B -> A transition.
                let observation = CancellationToken::new();
                let reason = match self.resolve_optional_revision(
                    &request.worktree,
                    &local_ref,
                    &observation,
                ) {
                    Ok(Some(after)) if after == local_oid => return Ok(pushed(local_oid)),
                    Ok(_) => "The source branch changed during the push",
                    Err(_) => "The source branch could not be verified after the push",
                };
                return Ok(PushOutcome::OutcomeUnknown {
                    failure: MutationFailure::Reconciliation {
                        reason: reason.into(),
                    },
                });
            }
            Ok(output) => {
                let rejected = push_ref_flag(&output.stdout, &refspec) == Some(b'!');
                let failure = git_failure(&command, output);
                if rejected {
                    // `!` is Git reporting that the remote refused this exact
                    // update, so the remote ref definitively did not move and
                    // there is nothing ambiguous to reconcile.
                    return Ok(PushOutcome::NotPushed {
                        failure: failure.into(),
                    });
                }
                MutationFailure::from(failure)
            }
            Err(failure) if !failure.started => {
                return Ok(PushOutcome::NotPushed {
                    failure: failure.into(),
                });
            }
            Err(failure) => {
                if push_ref_flag(&failure.stdout, &refspec) == Some(b'!') {
                    return Ok(PushOutcome::NotPushed {
                        failure: failure.into(),
                    });
                }
                MutationFailure::from(failure)
            }
        };
        // Explicit cancellation must release the panel promptly. A new remote
        // observation could hang on the very credential helper just cancelled;
        // retain the uncertain result instead of starting another network call.
        if cancellation.is_cancelled() {
            return Ok(PushOutcome::OutcomeUnknown { failure });
        }
        // The push may have completed at the remote before the local process
        // died, so the remote's own ref is consulted under a fresh token —
        // reconciliation has to run even when the original token is cancelled.
        let reconciliation = CancellationToken::new();
        let Some(destinations) = destinations else {
            return Ok(PushOutcome::OutcomeUnknown { failure });
        };
        if self.push_destinations_match(
            &request.worktree,
            request.target.remote(),
            &destinations,
            &destination,
            &local_oid,
            &reconciliation,
        ) {
            Ok(pushed(local_oid))
        } else {
            Ok(PushOutcome::OutcomeUnknown { failure })
        }
    }

    /// A slash-containing argument can also name a local repository path.
    /// Accept it only when this repository explicitly configures that remote.
    fn checked_remote<'a>(
        &self,
        worktree: &Path,
        remote: &'a str,
        cancellation: &CancellationToken,
    ) -> Result<CheckedRemote<'a>, RepositoryError> {
        if !remote.contains('/') {
            return Ok(CheckedRemote::simple(remote)?);
        }
        ValidatedRefName::parse(format!("refs/remotes/{remote}"))
            .map_err(|_| ValidationError::InvalidRemote(remote.to_owned()))?;
        if remote.starts_with('-') || remote.starts_with('/') || remote.contains(':') {
            return Err(ValidationError::InvalidRemote(remote.to_owned()).into());
        }
        if !self
            .remotes(worktree, cancellation)?
            .iter()
            .any(|configured| {
                configured.name == remote
                    && configured
                        .fetch_url
                        .as_ref()
                        .is_some_and(|url| !url.is_empty())
            })
        {
            return Err(ValidationError::InvalidRemote(remote.to_owned()).into());
        }
        Ok(CheckedRemote(remote))
    }

    /// A configured remote may fetch from one URL and push to several others.
    /// Every push destination must provide the exact ref as evidence. Lookup
    /// errors remain unknown; their resolved URLs and stderr may contain
    /// credentials, so they are neither logged nor attached to the outcome.
    fn push_destinations_match(
        &self,
        worktree: &Path,
        remote: &str,
        captured_destinations: &[OsString],
        remote_ref: &RemoteSideRef,
        expected: &Oid,
        cancellation: &CancellationToken,
    ) -> bool {
        if self
            .push_destinations(worktree, remote, cancellation)
            .as_deref()
            != Some(captured_destinations)
        {
            return false;
        }
        let inherited_count = std::env::var_os("GIT_CONFIG_COUNT");
        captured_destinations.iter().all(|destination| {
            let Some(command) = self.push_endpoint_command(
                worktree,
                destination,
                remote_ref,
                inherited_count.as_deref(),
            ) else {
                return false;
            };
            let Ok(output) = self.executor.execute(&command, cancellation) else {
                return false;
            };
            output.success
                && matches!(parse_remote_ref_oid(&output.stdout, remote_ref),
                    Ok(Some(oid)) if oid == *expected)
        })
    }

    fn push_endpoint_command(
        &self,
        worktree: &Path,
        endpoint: &OsStr,
        remote_ref: &RemoteSideRef,
        inherited_count: Option<&OsStr>,
    ) -> Option<CommandSpec> {
        let count = match inherited_count {
            None => 0,
            Some(value) => value.to_str()?.parse::<u32>().ok()?,
        };
        // Git's environment config count is bounded by a signed int. Leave
        // existing entries inherited and append one without overflowing it.
        if count >= i32::MAX as u32 {
            return None;
        }
        let marker = format!("cagents-reconciliation-{}:", uuid::Uuid::new_v4().simple());
        let mut key = OsString::from("url.");
        key.push(endpoint);
        key.push(".insteadOf");
        // Passing an already resolved URL to ls-remote would apply insteadOf
        // a second time. A fresh full-marker alias maps directly to this URL
        // once, while retaining all inherited auth and transport configuration.
        // Environment framing also preserves '=' and non-UTF8 endpoint bytes,
        // and keeps credential-bearing URLs out of process arguments.
        Some(
            self.git_read(
                worktree,
                [
                    "ls-remote",
                    "--refs",
                    "--exit-code",
                    "--",
                    &marker,
                    remote_ref.as_str(),
                ],
            )
            .env("GIT_CONFIG_COUNT", (count + 1).to_string())
            .env(format!("GIT_CONFIG_KEY_{count}"), key)
            .env(format!("GIT_CONFIG_VALUE_{count}"), marker),
        )
    }

    fn push_destinations(
        &self,
        worktree: &Path,
        remote: &str,
        cancellation: &CancellationToken,
    ) -> Option<Vec<OsString>> {
        use std::os::unix::ffi::OsStringExt;
        let values = |variable: &str| {
            self.executor
                .execute(
                    &self.git_read(
                        worktree,
                        [
                            "config",
                            "--null",
                            "--get-all",
                            &format!("remote.{remote}.{variable}"),
                        ],
                    ),
                    cancellation,
                )
                .ok()
        };
        let mut configured = values("pushurl")?;
        if !configured.success && configured.exit_code == Some(1) {
            configured = values("url")?;
        }
        if !configured.success || !configured.stdout.ends_with(&[0]) {
            return None;
        }
        let raw = &configured.stdout[..configured.stdout.len() - 1];
        let configured_count = raw.split(|byte| *byte == 0).count();
        if raw
            .split(|byte| *byte == 0)
            .any(|value| value.is_empty() || value.contains(&b'\n'))
        {
            return None;
        }
        // Git can drop unmatched fetch URLs when pushInsteadOf produces a
        // separate push list. Certify that no rewrite replacement base contains
        // a newline, so get-url's line framing stays unambiguous even when its
        // destination count is smaller than the configured fetch URL count.
        let keys = self
            .executor
            .execute(
                &self.git_read(worktree, ["config", "--name-only", "--null", "--list"]),
                cancellation,
            )
            .ok()?;
        if !keys.success || (!keys.stdout.is_empty() && !keys.stdout.ends_with(&[0])) {
            return None;
        }
        for key in keys.stdout.split(|byte| *byte == 0) {
            if !key
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"url."))
            {
                continue;
            }
            let variable = key.rsplit(|byte| *byte == b'.').next().unwrap_or_default();
            if (variable.eq_ignore_ascii_case(b"insteadOf")
                || variable.eq_ignore_ascii_case(b"pushInsteadOf"))
                && key.contains(&b'\n')
            {
                return None;
            }
        }
        // Delegate longest-prefix URL rewrites to Git; their raw keys, values,
        // resolved destinations, and lookup stderr remain private evidence.
        let output = self
            .executor
            .execute(
                &self.git_read(
                    worktree,
                    ["remote", "get-url", "--push", "--all", "--", remote],
                ),
                cancellation,
            )
            .ok()?;
        if !output.success || !output.stdout.ends_with(b"\n") {
            return None;
        }
        let resolved = &output.stdout[..output.stdout.len() - 1];
        let destinations: Vec<_> = resolved.split(|byte| *byte == b'\n').collect();
        if destinations.len() > configured_count
            || destinations
                .iter()
                .any(|value| value.is_empty() || value.contains(&0))
        {
            return None;
        }
        Some(
            destinations
                .into_iter()
                .map(|value| OsString::from_vec(value.to_vec()))
                .collect(),
        )
    }

    /// The object id a remote currently has for one ref, or `None` when the
    /// remote does not have it.
    ///
    /// The ref is named the way the remote names it. Since ls-remote arguments
    /// also match ref-name tails, only the exact advertised name is accepted;
    /// a local `refs/remotes/origin/main` is not the remote's branch name.
    pub fn remote_ref_oid(
        &self,
        worktree: &Path,
        remote: &str,
        remote_ref: &RemoteSideRef,
        cancellation: &CancellationToken,
    ) -> Result<Option<Oid>, RepositoryError> {
        let _remote = self.checked_remote(worktree, remote, cancellation)?;
        let command = self.git_read(
            worktree,
            [
                OsString::from("ls-remote"),
                OsString::from("--refs"),
                OsString::from("--exit-code"),
                OsString::from("--"),
                OsString::from(remote),
                OsString::from(remote_ref.as_str()),
            ],
        );
        let output = self.run(&command, cancellation)?;
        if !output.success {
            // 2 is `--exit-code`'s "no matching ref"; anything else is a
            // genuine lookup failure and must not be read as absence.
            return if output.exit_code == Some(2) {
                Ok(None)
            } else {
                Err(git_failure(&command, output).into())
            };
        }
        parse_remote_ref_oid(&output.stdout, remote_ref)
    }

    /// Runs one mutation command and classifies the outcome.
    ///
    /// A command that never started cannot have touched the repository, so it
    /// is definitively `NotApplied`. What a *non-zero exit* means depends on
    /// the command, which is what `scope` carries; a command that died after
    /// launch is `OutcomeUnknown` either way, because a killed `restore` can
    /// have written some of the files it had already validated.
    fn run_mutation(
        &self,
        command: &CommandSpec,
        paths: Vec<RepoPath>,
        scope: MutationScope,
        cancellation: &CancellationToken,
    ) -> MutationOutcome {
        match self.run(command, cancellation) {
            Ok(output) if output.success => MutationOutcome::Applied { paths },
            Ok(output) => {
                let exit_code = output.exit_code;
                scope.classify(exit_code, git_failure(command, output).into())
            }
            Err(failure) if !failure.started => MutationOutcome::NotApplied {
                failure: failure.into(),
            },
            Err(failure) => MutationOutcome::OutcomeUnknown {
                failure: failure.into(),
            },
        }
    }

    /// Resolve branch/ticket intent using local refs and advertised remote refs.
    /// Fetching is deferred until the durable journal owns the mutation.
    pub fn resolve_worktree_request(
        &self,
        input: &WorktreeInput,
        cancellation: &CancellationToken,
    ) -> Result<CreateWorktreeRequest, RepositoryError> {
        let discovered = self.discover(&input.repository, cancellation)?;
        let repository = discovered.worktree_root.unwrap_or(discovered.common_dir);
        let output = self.run_checked(&self.refs_command(&repository), cancellation)?;
        let refs = parse_refs(&output.stdout)?;
        let remotes = self.remotes(&repository, cancellation)?;
        let mut name = worktree_branch_input(&input.input)?;
        let explicit = remotes
            .iter()
            .filter_map(|remote| {
                name.strip_prefix(&format!("{}/", remote.name))
                    .map(|branch| (remote.name.clone(), branch.to_owned()))
            })
            .max_by_key(|(remote, _)| remote.len());
        if let Some((_, branch)) = &explicit {
            name = branch.clone();
        }
        let branch = ValidatedBranchName::parse(name)?;
        let local = refs
            .local
            .iter()
            .find(|local| local.name.as_str() == branch.full_ref());
        let selected_remote = if let Some((remote, name)) = explicit {
            Some((remote, name))
        } else if let Some(local) = local {
            local.upstream.as_ref().and_then(|upstream| {
                remotes
                    .iter()
                    .filter_map(|remote| {
                        upstream
                            .as_str()
                            .strip_prefix(&format!("refs/remotes/{}/", remote.name))
                            .map(|name| (remote.name.clone(), name.to_owned()))
                    })
                    .max_by_key(|(remote, _)| remote.len())
            })
        } else {
            let mut candidates = Vec::new();
            for remote in &remotes {
                if self
                    .remote_ref_oid(
                        &repository,
                        &remote.name,
                        &RemoteSideRef::parse(branch.full_ref())?,
                        cancellation,
                    )?
                    .is_some()
                {
                    candidates.push((remote.name.clone(), branch.as_str().to_owned()));
                }
            }
            if candidates.len() > 1 {
                return Err(ValidationError::InvalidBranch(format!(
                    "{} exists on multiple remotes; enter one of: {}",
                    branch.as_str(),
                    candidates
                        .iter()
                        .map(|(remote, branch)| format!("{remote}/{branch}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .into());
            }
            candidates.pop()
        };
        let existing = local.is_some() || selected_remote.is_some();
        if existing && !input.base.trim().is_empty() {
            return Err(ValidationError::InvalidBase(
                "An existing branch keeps its history; clear the base override or choose a new branch".into()
            ).into());
        }
        if let Some(worktree) = self
            .list_worktrees(&repository, cancellation)?
            .iter()
            .find(|worktree| worktree.branch.as_deref() == Some(branch.full_ref().as_str()))
        {
            return Err(ValidationError::InvalidBranch(format!(
                "{} is already checked out at {}; use that worktree or switch its branch first",
                branch.as_str(),
                worktree.path.display()
            ))
            .into());
        }
        let (base, track) = if let Some((remote, name)) = selected_remote {
            (
                BaseRevision::remote_ref(format!("refs/remotes/{remote}/{name}"))?,
                true,
            )
        } else if local.is_some() {
            (BaseRevision::LocalBranch(branch.clone()), false)
        } else if !input.base.trim().is_empty() {
            (
                self.resolve_worktree_base(
                    &repository,
                    input.base.trim(),
                    &refs,
                    &remotes,
                    cancellation,
                )?,
                false,
            )
        } else {
            (
                self.default_worktree_base(&repository, &refs, &remotes, cancellation)?,
                false,
            )
        };
        let configured = input.managed_directory.trim();
        let configured = Path::new(if configured.is_empty() {
            "worktrees/"
        } else {
            configured
        });
        let managed = if configured.is_absolute() {
            configured.to_path_buf()
        } else {
            repository.join(configured)
        };
        let (managed_root, destination) = if input.destination.trim().is_empty() {
            let managed = canonical_managed_directory(&managed)
                .map_err(|error| ValidationError::Io(error.to_string()))?;
            let destination = managed.join(branch_destination_component(branch.as_str()));
            (managed, destination)
        } else {
            let path = Path::new(input.destination.trim());
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                repository.join(path)
            };
            let parent = path
                .parent()
                .ok_or(ValidationError::DestinationOutsideRoot)?;
            let root = canonical_future_path(parent)
                .map_err(|error| ValidationError::Io(error.to_string()))?;
            let leaf = path
                .file_name()
                .ok_or(ValidationError::DestinationOutsideRoot)?;
            (root.clone(), root.join(leaf))
        };
        if inspect_destination(&destination) != DestinationResidualState::Absent {
            return Err(ValidationError::DestinationOccupied.into());
        }
        Ok(CreateWorktreeRequest {
            // Preserve the project identity used to restore operation rows.
            // Only relative destination resolution uses its worktree root.
            repository: input.repository.clone(),
            managed_root,
            destination,
            branch,
            base,
            track,
            operation_id: input.operation_id,
        })
    }

    fn resolve_worktree_base(
        &self,
        repository: &Path,
        name: &str,
        refs: &RefListing,
        remotes: &[RemoteRecord],
        cancellation: &CancellationToken,
    ) -> Result<BaseRevision, RepositoryError> {
        if is_hex_oid(name) {
            return Ok(BaseRevision::object_id(name)?);
        }
        let local_name = name.strip_prefix("refs/heads/").unwrap_or(name);
        if refs
            .local
            .iter()
            .any(|local| local.name.as_str() == format!("refs/heads/{local_name}"))
        {
            return Ok(BaseRevision::LocalBranch(ValidatedBranchName::parse(
                local_name,
            )?));
        }
        let remote_name = name.strip_prefix("refs/remotes/").unwrap_or(name);
        if let Some((remote, branch)) = remotes
            .iter()
            .filter_map(|remote| {
                remote_name
                    .strip_prefix(&format!("{}/", remote.name))
                    .map(|branch| (&remote.name, branch))
            })
            .max_by_key(|(remote, _)| remote.len())
        {
            ValidatedBranchName::parse(branch)?;
            return Ok(BaseRevision::remote_ref(format!(
                "refs/remotes/{remote}/{branch}"
            ))?);
        }
        let branch = ValidatedBranchName::parse(name)?;
        let mut candidates = Vec::new();
        for remote in remotes {
            if self
                .remote_ref_oid(
                    repository,
                    &remote.name,
                    &RemoteSideRef::parse(branch.full_ref())?,
                    cancellation,
                )?
                .is_some()
            {
                candidates.push(format!("refs/remotes/{}/{name}", remote.name));
            }
        }
        if candidates.len() == 1 {
            return Ok(BaseRevision::remote_ref(candidates.remove(0))?);
        }
        Err(ValidationError::InvalidBase(format!(
            "Cannot uniquely resolve {name}; use a local branch, remote/branch, or full commit ID"
        ))
        .into())
    }

    fn default_worktree_base(
        &self,
        repository: &Path,
        refs: &RefListing,
        remotes: &[RemoteRecord],
        cancellation: &CancellationToken,
    ) -> Result<BaseRevision, RepositoryError> {
        let remote = remotes
            .iter()
            .find(|remote| remote.name == "origin")
            .or_else(|| (remotes.len() == 1).then(|| &remotes[0]));
        if let Some(remote) = remote {
            let _remote = self.checked_remote(repository, &remote.name, cancellation)?;
            let output = self.run_checked(
                &self.git_read(
                    repository,
                    ["ls-remote", "--symref", remote.name.as_str(), "HEAD"],
                ),
                cancellation,
            )?;
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(branch) = text.lines().find_map(|line| {
                line.strip_prefix("ref: refs/heads/")
                    .and_then(|line| line.strip_suffix("\tHEAD"))
            }) {
                return Ok(BaseRevision::remote_ref(format!(
                    "refs/remotes/{}/{branch}",
                    remote.name
                ))?);
            }
        }
        // The checked-out branch is a proven ref. A repository whose default
        // is not `main` must not be silently based on a local branch of that
        // name, and a missing symbolic remote HEAD is not a reason to guess.
        if let Some(current) = refs.local.iter().find(|record| record.head) {
            return Ok(BaseRevision::LocalBranch(ValidatedBranchName::parse(
                current.name.short_name(),
            )?));
        }
        if let Some(head) = self.resolve_head(repository, cancellation)? {
            return Ok(BaseRevision::object_id(head.to_string())?);
        }
        Err(ValidationError::InvalidBase(
            "No default branch or commit exists; create an initial commit or specify a base".into(),
        )
        .into())
    }

    pub fn plan_worktree_creation(
        &self,
        request: &CreateWorktreeRequest,
    ) -> Result<WorktreeCreationPlan, ValidationError> {
        ValidatedBranchName::parse(request.branch.as_str())?;
        request.base.validate()?;
        if request.track && matches!(&request.base, BaseRevision::ObjectId(_)) {
            return Err(ValidationError::TrackingRequiresBranch);
        }
        validate_destination(&request.managed_root, &request.destination)?;
        let branch = request.branch.as_str();
        let base = format!("{}^{{commit}}", request.base.revision_argument());
        Ok(WorktreeCreationPlan {
            // A read: `check-ref-format` inspects a name and touches no
            // repository state at all.
            validate_branch: self.git_read(
                &request.repository,
                [
                    OsString::from("check-ref-format"),
                    OsString::from("--branch"),
                    branch.into(),
                ],
            ),
            resolve_base: self.git_read(
                &request.repository,
                [
                    OsString::from("rev-parse"),
                    OsString::from("--verify"),
                    OsString::from("--end-of-options"),
                    base.into(),
                ],
            ),
            check_branch_absent: self.git_read(
                &request.repository,
                [
                    OsString::from("show-ref"),
                    OsString::from("--verify"),
                    OsString::from("--quiet"),
                    request.branch.full_ref().into(),
                ],
            ),
        })
    }

    pub fn create_worktree_command(
        &self,
        request: &CreateWorktreeRequest,
        base_oid: &str,
    ) -> Result<CommandSpec, ValidationError> {
        ValidatedBranchName::parse(request.branch.as_str())?;
        request.base.validate()?;
        validate_destination(&request.managed_root, &request.destination)?;
        if !is_hex_oid(base_oid) {
            return Err(ValidationError::InvalidBase(base_oid.into()));
        }
        Ok(self.git_mutation(
            &request.repository,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--lock"),
                OsString::from("--reason"),
                format!("cagents:{}", request.operation_id).into(),
                OsString::from("--no-track"),
                OsString::from("-b"),
                request.branch.as_str().into(),
                request.destination.as_os_str().to_owned(),
                base_oid.into(),
            ],
        ))
    }

    pub fn set_upstream_command(
        &self,
        request: &CreateWorktreeRequest,
    ) -> Result<CommandSpec, ValidationError> {
        request.base.validate()?;
        let upstream = match &request.base {
            BaseRevision::LocalBranch(branch) => branch.full_ref(),
            BaseRevision::RemoteRef(reference) => reference.clone(),
            BaseRevision::ObjectId(_) => return Err(ValidationError::TrackingRequiresBranch),
        };
        Ok(self.git_mutation(
            &request.repository,
            [
                OsString::from("branch"),
                format!("--set-upstream-to={upstream}").into(),
                request.branch.as_str().into(),
            ],
        ))
    }

    fn reconcile_worktree(
        &self,
        request: &CreateWorktreeRequest,
        cancellation: &CancellationToken,
    ) -> Result<WorktreeResidualState, RepositoryError> {
        let worktrees = self.list_worktrees(&request.repository, cancellation)?;
        let command = self.git_read(
            &request.repository,
            [
                OsString::from("show-ref"),
                OsString::from("--verify"),
                OsString::from("--quiet"),
                request.branch.full_ref().into(),
            ],
        );
        let branch_output = self.run(&command, cancellation)?;
        let branch_exists = match branch_output.exit_code {
            _ if branch_output.success => true,
            Some(1) => false,
            _ => return Err(git_failure(&command, branch_output).into()),
        };
        Ok(WorktreeResidualState {
            branch_exists,
            registered_worktree: worktrees
                .into_iter()
                .find(|worktree| worktree.path == request.destination),
            destination: inspect_destination(&request.destination),
        })
    }

    /// Runs one Git command, recording a failure to *launch* it. The exit code
    /// is left to the caller, because several Git commands answer a question
    /// with a non-zero exit rather than by failing.
    fn run(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        self.executor
            .execute(command, cancellation)
            .inspect_err(|failure| log_git_failure(command, failure))
    }

    fn run_checked(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        let output = self.run(command, cancellation)?;
        if output.success {
            Ok(output)
        } else {
            Err(git_failure(command, output))
        }
    }

    fn git_read<I, S>(&self, repository: &Path, arguments: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        read_options(self.git_base(repository, false)).args(arguments)
    }

    /// A read that receives a [`RepoPath`].
    fn git_literal_read<I, S>(&self, repository: &Path, arguments: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        read_options(self.git_base(repository, true)).args(arguments)
    }

    fn git_mutation<I, S>(&self, repository: &Path, arguments: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.git_base(repository, false).args(arguments)
    }

    /// A mutation that receives a [`RepoPath`].
    fn git_literal_mutation<I, S>(&self, repository: &Path, arguments: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.git_base(repository, true).args(arguments)
    }

    /// `--literal-pathspecs` is placed before `-C` because Git only accepts
    /// its global options ahead of the subcommand's. It is required, not
    /// belt-and-braces: `--` stops option parsing but not pathspec magic, so
    /// without it a file literally named `:(top)x` or `:x` would be read as a
    /// magic pathspec and could select a different set of files than the one
    /// the user picked.
    ///
    /// `core.fsmonitor=false` is set here, for *every* invocation, because it
    /// is a trust boundary rather than a latency measure. Configured to
    /// anything other than a boolean, `core.fsmonitor` names a program Git
    /// executes whenever it refreshes the index — and `add`, `restore`,
    /// `rm --cached` and `commit` all refresh the index. A repository that
    /// arrived as an archive carries its own `.git/config`, so without this a
    /// single click on Stage would run code the repository chose.
    fn git_base(&self, repository: &Path, literal_pathspecs: bool) -> CommandSpec {
        let mut command = CommandSpec::new(&self.git);
        if literal_pathspecs {
            command = command.arg("--literal-pathspecs");
        }
        command
            .args([OsString::from("-C"), repository.as_os_str().to_owned()])
            .args(["-c", "core.fsmonitor=false"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
    }
}

/// A repository may request Git's builtin monitor (boolean true), but it may
/// not select a hook program. Programs explicitly configured by the user in
/// global/system configuration may retain authority when their absolute
/// executable is outside repository content and contains no shell syntax.
fn trusted_fsmonitor(output: &[u8], worktree: &Path) -> Option<OsString> {
    let text = std::str::from_utf8(output).ok()?.trim_end_matches('\n');
    let (scope, value) = text.split_once('\t')?;
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(OsString::from("true")),
        "false" | "no" | "off" | "0" | "" => None,
        _ if matches!(scope, "global" | "system" | "command") => {
            // Git interprets hook values as commands. Relative commands and
            // shell syntax can select code supplied by the current checkout.
            if !Path::new(value).is_absolute()
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
            {
                return None;
            }
            let executable = fs::canonicalize(value).ok()?;
            let worktree = fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
            if executable.starts_with(&worktree)
                || executable
                    .ancestors()
                    .skip(1)
                    .any(|parent| parent.join(".git").exists())
            {
                return None;
            }
            use std::os::unix::fs::PermissionsExt;
            let metadata = fs::metadata(&executable).ok()?;
            (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                .then(|| executable.into_os_string())
        }
        _ => None,
    }
}

fn remove_fsmonitor_override(command: &mut CommandSpec) {
    if let Some(index) = command
        .arguments
        .iter()
        .position(|argument| argument == OsStr::new("core.fsmonitor=false"))
    {
        command.arguments.remove(index);
        command.arguments.remove(index - 1);
    }
}

fn snapshot_fsmonitor(mut command: CommandSpec, configured: Option<&OsStr>) -> CommandSpec {
    if let Some(configured) = configured
        && let Some(argument) = command
            .arguments
            .iter_mut()
            .find(|argument| **argument == OsStr::new("core.fsmonitor=false"))
    {
        let mut value = OsString::from("core.fsmonitor=");
        value.push(configured);
        *argument = value;
    }
    command
}

/// The ten NUL-separated fields [`parse_history`] expects, in order.
const HISTORY_FORMAT: &str = "%H%x00%P%x00%an%x00%ae%x00%ad%x00%cn%x00%ce%x00%cd%x00%s%x00%b";

/// Room for one `cat-file --batch` header above the content limit, so an
/// oversize object still reports its true size before being rejected.
const BATCH_HEADER_SLACK: usize = 4096;

/// Bytes of a failed Git command's standard error carried into the log. Room
/// for Git's own `fatal:` line and the hint under it, far short of the
/// megabytes a failing command is allowed to print.
const LOG_STDERR_EXCERPT: usize = 512;

/// The failure for a Git command that ran and exited non-zero.
///
/// Every caller that reads a non-zero exit as a real failure goes through here
/// so that the argv behind it is recorded exactly once. Without that record,
/// `git: process exited with status Some(128)` on a banner is unattributable:
/// identifying which command of a repository load returned it needed a shim on
/// `PATH`.
fn git_failure(command: &CommandSpec, output: ProcessOutput) -> ProcessFailure {
    let failure = output
        .require_success(&command.program)
        .expect_err("a command that did not succeed is a failure");
    log_git_failure(command, &failure);
    failure
}

/// Records one failed Git invocation.
///
/// A cancellation is not a failure: superseding an in-flight request is how
/// this application refreshes, and logging every superseded read would bury
/// the failures that matter.
fn log_git_failure(command: &CommandSpec, failure: &ProcessFailure) {
    if failure.kind == ProcessFailureKind::Cancelled {
        return;
    }
    let argv = argv_summary(command);
    let excerpt =
        String::from_utf8_lossy(&failure.stderr[..failure.stderr.len().min(LOG_STDERR_EXCERPT)]);
    tracing::warn!(
        argv = %argv,
        kind = ?failure.kind,
        status = ?failure.exit_code,
        signal = ?failure.signal,
        stderr = %excerpt.trim_end(),
        "git command failed"
    );
}

/// One log line's worth of argv, rendered lossily because a path need not be
/// UTF-8. Only a failure pays for the allocation.
fn argv_summary(command: &CommandSpec) -> String {
    let mut summary = command.program.display().to_string();
    for argument in &command.arguments {
        summary.push(' ');
        summary.push_str(&argument.to_string_lossy());
    }
    summary
}

/// The options only a read may carry. A mutation is entitled to the index lock
/// and to refreshing the index, so it must never be given any of these.
///
/// `--no-optional-locks` is Git's own answer for a tool that polls status. It
/// stops a read from taking `.git/index.lock`, so a background refresh can
/// never block a user action — and, the reason it is spelled out on the
/// command line rather than left to the environment variable alone, it stops
/// `git status` from rewriting `.git/index` when it finds the stat cache
/// stale. The environment variable stays beside it so that any Git this one
/// re-executes inherits the same rule.
///
/// `diff.autoRefreshIndex=false` closes the hole the option leaves open.
/// `git diff` against the working tree finishes by refreshing the index under
/// the index lock, and it does that whatever the optional-lock setting says.
/// Because the card re-reads state on a periodic freshness poll, that write is
/// seen as a change, which schedules another state read, which runs another
/// `git diff`: a refresh loop that spins Git processes for as long as the
/// window is open.
/// The comparison itself is unaffected, because a diff reads content rather
/// than trusting the stat cache.
fn read_options(command: CommandSpec) -> CommandSpec {
    command
        .args(["--no-optional-locks", "-c", "diff.autoRefreshIndex=false"])
        .env("GIT_OPTIONAL_LOCKS", "0")
}

/// NUL-delimited literal paths preserve arbitrary Unix names without argv limits.
fn pathspec_input(paths: &[RepoPath]) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_os_str().as_bytes());
        input.push(0);
    }
    input
}

#[cfg(test)]
/// Subcommand arguments followed by `--` and the selected paths.
fn pathspec_arguments<'a>(
    leading: impl IntoIterator<Item = &'a str>,
    paths: &[RepoPath],
) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = leading.into_iter().map(OsString::from).collect();
    arguments.push(OsString::from("--"));
    arguments.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
    arguments
}

/// A cooperative Git index guard. Git writers respect `index.lock`; clients
/// that deliberately replace another writer's lock violate that protocol.
/// Never publish index bytes or remove a foreign/stale lock. A crash leaves
/// the same stale-lock evidence Git itself leaves, requiring explicit recovery.
pub(crate) struct GitFileLock {
    parent: fs_safety::Root,
    name: OsString,
    file: fs::File,
    version: (u64, u64, u64, i64, i64, i64, i64),
    active: bool,
}

impl GitFileLock {
    /// Git removed the containing administrative directory itself.
    pub(crate) fn retired_with_directory(&mut self) {
        self.active = false;
    }

    pub(crate) fn acquire(index: &Path) -> Result<Self, String> {
        if !index.is_absolute() {
            return Err("Git reported a relative index path".into());
        }
        let parent = index.parent().ok_or("Git reported no index parent")?;
        let parent = fs_safety::Root::open(parent).map_err(|error| error.to_string())?;
        let mut name = index
            .file_name()
            .ok_or("Git reported no index name")?
            .to_owned();
        name.push(".lock");
        let (file, temporary) =
            fs_safety::create_temporary_in(&parent, 0o600).map_err(|error| error.to_string())?;
        if let Err(error) = fs_safety::rename_new_within(&parent, &temporary, &name) {
            let reason = format!(
                "cannot acquire {}: {error}",
                parent.path().join(&name).display()
            );
            let cleanup = fs_safety::unlink_within(&parent, &temporary);
            return Err(match cleanup {
                Ok(()) => reason,
                Err(cleanup) => format!("{reason}; temporary cleanup failed: {cleanup}"),
            });
        }
        let metadata = file.metadata().map_err(|error| {
            format!(
                "index lock acquired but its metadata is unavailable; left {} untouched: {error}",
                parent.path().join(&name).display()
            )
        })?;
        Ok(Self {
            parent,
            name,
            version: Self::version(&metadata),
            file,
            active: true,
        })
    }

    fn version(metadata: &fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
        use std::os::unix::fs::MetadataExt;
        (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    }

    fn owns_lock(&self) -> Result<(), String> {
        let opened = self.file.metadata().map_err(|error| error.to_string())?;
        let named = fs_safety::metadata_beneath(&self.parent, Path::new(&self.name))
            .map_err(|error| error.to_string())?;
        if Self::version(&opened) != self.version || Self::version(&named) != self.version {
            return Err(
                "index lock was replaced or modified; foreign evidence was preserved".into(),
            );
        }
        Ok(())
    }

    pub(crate) fn check(&self) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let opened = self.parent.metadata().map_err(|error| error.to_string())?;
        let named = fs::metadata(self.parent.path()).map_err(|error| error.to_string())?;
        if (opened.dev(), opened.ino()) != (named.dev(), named.ino()) {
            return Err("the index directory was replaced; no further files were deleted".into());
        }
        self.owns_lock()
    }

    fn release(&mut self) -> Result<(), String> {
        if !std::mem::take(&mut self.active) {
            return Ok(());
        }
        self.owns_lock()?;
        fs_safety::unlink_within(&self.parent, &self.name).map_err(|error| error.to_string())
    }
}

impl Drop for GitFileLock {
    fn drop(&mut self) {
        if self.active
            && let Err(reason) = self.release()
        {
            tracing::warn!(path = %self.parent.path().join(&self.name).display(), %reason, "discard index lock cleanup failed");
        }
    }
}

fn discard_failures(paths: &[RepoPath], reason: String) -> Vec<MutationFailure> {
    paths
        .iter()
        .map(|path| MutationFailure::Path {
            path: path.clone(),
            reason: reason.clone(),
        })
        .collect()
}

/// Names exactly what a discard deleted and what it refused.
fn report_discard(paths: Vec<RepoPath>, mut failures: Vec<MutationFailure>) -> MutationOutcome {
    if failures.is_empty() {
        return MutationOutcome::Applied { paths };
    }
    if paths.is_empty() {
        // Nothing was deleted, so the request is retried whole and the first
        // reason is the one the caller acts on.
        return MutationOutcome::NotApplied {
            failure: failures.swap_remove(0),
        };
    }
    MutationOutcome::PartiallyApplied { paths, failures }
}

/// Applies Git's `--cleanup=whitespace` to a commit message.
///
/// Trailing whitespace goes from every line, leading and trailing empty lines
/// go, and a run of empty lines becomes one. Comment lines are kept, which is
/// the whole difference from `strip`. Used to compare a message this service
/// sent against the one Git stored, so both sides are reduced by the same rule
/// that Git already applied to one of them — which is why "whitespace" has to
/// mean exactly what `git stripspace` means by it, and no more:
/// `strbuf_stripspace` right-trims each line with Git's own `isspace`, whose
/// `sane_ctype` table holds space, tab, LF and CR and deliberately leaves out
/// vertical tab and form feed, so `is_ascii_whitespace` is the wrong set.
/// `str::lines` takes one CR as part of a CRLF terminator and leaves any
/// others, so a line ending `\r\r\n` needs the second one taken here.
fn cleanup_whitespace(message: &str) -> String {
    let mut cleaned = String::with_capacity(message.len());
    let mut pending_blank = false;
    for line in message.lines() {
        let line = line.trim_end_matches([' ', '\t', '\r']);
        if line.is_empty() {
            // Only remembered, so leading and trailing runs never reach the
            // output and an interior run collapses to one.
            pending_blank = !cleaned.is_empty();
            continue;
        }
        if !cleaned.is_empty() {
            cleaned.push('\n');
        }
        if pending_blank {
            cleaned.push('\n');
            pending_blank = false;
        }
        cleaned.push_str(line);
    }
    cleaned
}

/// ls-remote arguments are suffix patterns, even when fully qualified. Only
/// the exact advertised name proves presence; a matching tail in another ref
/// must not select a branch or establish that a push reached its destination.
fn parse_remote_ref_oid(
    bytes: &[u8],
    requested: &RemoteSideRef,
) -> Result<Option<Oid>, RepositoryError> {
    let invalid = |reason: &str| RepositoryError::InvalidOutput {
        field: "ls_remote_oid",
        reason: reason.to_owned(),
    };
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if bytes.is_empty() {
        return Ok(None);
    }
    let mut matched = None;
    for line in bytes.split(|byte| *byte == b'\n') {
        let mut fields = line.split(|byte| *byte == b'\t');
        let object = fields.next().unwrap_or_default();
        let name = fields
            .next()
            .ok_or_else(|| invalid("missing advertised ref name"))?;
        if name.is_empty() || fields.next().is_some() {
            return Err(invalid("malformed advertised ref record"));
        }
        let object = std::str::from_utf8(object).map_err(|_| invalid("object id is not UTF-8"))?;
        let oid = Oid::parse_object(object).map_err(|_| invalid("invalid advertised object id"))?;
        if name == requested.as_str().as_bytes() {
            if matched.is_some() {
                return Err(invalid("duplicate exact advertised ref"));
            }
            matched = Some(oid);
        }
    }
    Ok(matched)
}

/// `<local ref>:<remote-side ref>`, both fully qualified.
///
/// The destination is a [`RemoteSideRef`] rather than a string so that this
/// cannot be handed the remote-tracking ref for the same branch, which Git
/// would push to happily and to no effect.
fn push_refspec(branch: &ValidatedBranchName, destination: &RemoteSideRef) -> String {
    format!("{}:{destination}", branch.full_ref())
}

/// A remote argument whose path-like spelling has been checked against the
/// repository configuration, or which meets the existing simple-name rules.
struct CheckedRemote<'a>(&'a str);

impl<'a> CheckedRemote<'a> {
    fn simple(remote: &'a str) -> Result<Self, ValidationError> {
        validate_remote_name(remote)?;
        Ok(Self(remote))
    }
}

/// Remote names reach both a configuration key and a command line, so a name
/// that could be read as an option, a path, or a refspec is rejected here
/// rather than handed to Git.
fn validate_remote_name(name: &str) -> Result<(), ValidationError> {
    let invalid = name.is_empty()
        || name.starts_with('-')
        || name.starts_with('.')
        || name.contains('/')
        || name.contains(':')
        || name.contains("..")
        || name.bytes().any(|byte| {
            byte.is_ascii_control()
                || byte == b' '
                || matches!(byte, b'~' | b'^' | b'?' | b'*' | b'[' | b'\\')
        });
    if invalid {
        Err(ValidationError::InvalidRemote(name.to_owned()))
    } else {
        Ok(())
    }
}

/// Deletes one untracked entry, resolving it only beneath the worktree root.
///
/// The hazard a discard has to close is the *second* path resolution: checking
/// the ancestors by name and then unlinking by name lets a concurrent writer
/// swap a checked directory for a symlink in between, after which the unlink
/// lands on a same-named file outside the worktree. So the parent directory is
/// opened beneath `root` following no symlink in any component, and both the
/// classification and the unlink go through *that* descriptor — the name is
/// never resolved from the worktree a second time, and neither step can reach
/// outside the directory the other one verified.
///
/// A directory is never removed, and by construction rather than by inspection:
/// [`fs_safety::unlink_within`] unlinks without `AT_REMOVEDIR`, which the
/// kernel refuses for a directory.
fn delete_untracked(root: &fs_safety::Root, path: &RepoPath) -> Result<(), MutationFailure> {
    let reject = |reason: String| MutationFailure::Path {
        path: path.clone(),
        reason,
    };
    let (parent, name) = fs_safety::open_parent_beneath(root, path.as_path())
        .map_err(|error| reject(error.to_string()))?;
    match fs_safety::metadata_beneath(&parent, Path::new(&name)) {
        Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
            fs_safety::unlink_within(&parent, &name).map_err(|error| reject(error.to_string()))
        }
        Ok(_) => Err(reject("only regular files and symlinks are deleted".into())),
        // Already gone: the requested end state is the current state.
        Err(error) if error.is_not_found() => Ok(()),
        Err(error) => Err(reject(error.to_string())),
    }
}

/// The object size from a `cat-file --batch` header, even when the content that
/// followed it was truncated.
fn batch_header_size(output: &[u8]) -> Option<u64> {
    let line = output.split(|byte| *byte == b'\n').next()?;
    std::str::from_utf8(line)
        .ok()?
        .rsplit(' ')
        .next()?
        .parse()
        .ok()
}

/// Attaches the raw process output to a parse failure, so a report always
/// carries the bytes that could not be understood.
fn invalid_output<T>(
    result: Result<T, GitParseError>,
    process: ProcessOutput,
) -> Result<T, RepositoryError> {
    result.map_err(|error| RepositoryError::InvalidGitOutput { error, process })
}

fn validate_destination(root: &Path, destination: &Path) -> Result<(), ValidationError> {
    let root = fs::canonicalize(root).map_err(|error| ValidationError::Io(error.to_string()))?;
    if !root.is_dir() {
        return Err(ValidationError::InvalidDestinationRoot);
    }
    if !destination.is_absolute() {
        return Err(ValidationError::DestinationOutsideRoot);
    }
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ValidationError::DestinationSymlink);
            }
            return Err(ValidationError::DestinationOccupied);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(ValidationError::Io(error.to_string())),
    }
    let parent = destination
        .parent()
        .ok_or(ValidationError::DestinationOutsideRoot)?;
    // A slash-separated branch nests its worktree under directories that
    // `git worktree add` creates on the way; they need not exist yet.
    let parent =
        canonical_future_path(parent).map_err(|_| ValidationError::DestinationOutsideRoot)?;
    if parent != root && !parent.starts_with(&root) {
        return Err(ValidationError::DestinationOutsideRoot);
    }
    if destination.file_name().is_none() {
        return Err(ValidationError::DestinationOutsideRoot);
    }
    Ok(())
}

fn inspect_destination(destination: &Path) -> DestinationResidualState {
    match fs::symlink_metadata(destination) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => DestinationResidualState::Absent,
        Err(_) => DestinationResidualState::Unknown,
        Ok(metadata) if metadata.file_type().is_symlink() => DestinationResidualState::Symlink,
        Ok(metadata) if metadata.is_file() => DestinationResidualState::File,
        Ok(metadata) if metadata.is_dir() => match fs::read_dir(destination) {
            Ok(mut entries) => {
                if entries.next().is_none() {
                    DestinationResidualState::EmptyDirectory
                } else {
                    DestinationResidualState::NonEmptyDirectory
                }
            }
            Err(_) => DestinationResidualState::Unknown,
        },
        Ok(_) => DestinationResidualState::Unknown,
    }
}

fn parse_bool_line(output: &[u8], field: &'static str) -> Result<bool, RepositoryError> {
    match parse_utf8_line(output, field)?.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        value => Err(RepositoryError::InvalidOutput {
            field,
            reason: format!("expected true or false, got {value:?}"),
        }),
    }
}

fn parse_path_line(output: &[u8], field: &'static str) -> Result<PathBuf, RepositoryError> {
    let value = trim_one_line_ending(output);
    if value.is_empty() {
        return Err(RepositoryError::InvalidOutput {
            field,
            reason: "path was empty".into(),
        });
    }
    Ok(bytes_to_path(value))
}

fn parse_utf8_line(output: &[u8], field: &'static str) -> Result<String, RepositoryError> {
    let output = trim_one_line_ending(output);
    std::str::from_utf8(output)
        .map(str::to_owned)
        .map_err(|error| RepositoryError::InvalidOutput {
            field,
            reason: error.to_string(),
        })
}

fn trim_one_line_ending(output: &[u8]) -> &[u8] {
    output
        .strip_suffix(b"\r\n")
        .or_else(|| output.strip_suffix(b"\n"))
        .unwrap_or(output)
}

fn attach_repository_process<T>(
    result: Result<T, RepositoryError>,
    process: &ProcessOutput,
) -> Result<T, RepositoryError> {
    result.map_err(|source| RepositoryError::InvalidResponse {
        source: Box::new(source),
        process: process.clone(),
    })
}

fn repository_warning(program: &Path, error: RepositoryError) -> ProcessFailure {
    match error {
        RepositoryError::Process(failure) => failure,
        RepositoryError::InvalidGitOutput { error, process } => ProcessFailure {
            kind: ProcessFailureKind::Io,
            program: program.to_path_buf(),
            exit_code: process.exit_code,
            signal: process.signal,
            stdout: process.stdout,
            stderr: process.stderr,
            message: error.to_string(),
            started: true,
        },
        RepositoryError::InvalidResponse { source, process } => ProcessFailure {
            kind: ProcessFailureKind::Io,
            program: program.to_path_buf(),
            exit_code: process.exit_code,
            signal: process.signal,
            stdout: process.stdout,
            stderr: process.stderr,
            message: source.to_string(),
            started: true,
        },
        error => ProcessFailure {
            kind: ProcessFailureKind::Io,
            program: program.to_path_buf(),
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            message: error.to_string(),
            started: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsStr, sync::Mutex};

    #[derive(Default)]
    struct FakeExecutor {
        commands: Mutex<Vec<CommandSpec>>,
        outputs: Mutex<Vec<Result<ProcessOutput, ProcessFailure>>>,
    }

    impl FakeExecutor {
        fn with_outputs(outputs: Vec<Result<ProcessOutput, ProcessFailure>>) -> Self {
            Self {
                commands: Mutex::new(Vec::new()),
                outputs: Mutex::new(outputs.into_iter().rev().collect()),
            }
        }
    }

    impl ProcessExecutor for FakeExecutor {
        fn execute(
            &self,
            command: &CommandSpec,
            _cancellation: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            self.commands.lock().unwrap().push(command.clone());
            self.outputs.lock().unwrap().pop().unwrap()
        }
    }

    #[test]
    fn watch_projection_preserves_a_prefix_without_changing_complete_inventory_reads() {
        let listing = b"H first.txt\0H second.txt\0";
        let service = fake_service(vec![success(listing), success(listing)]);
        let cancel = CancellationToken::new();
        let prefix = service.watch_files_projection(Path::new("/repo"), 1, &cancel).unwrap();
        assert_eq!(prefix.paths, vec![path("first.txt")]);
        assert_eq!(prefix.tracked_count, 1);
        assert!(prefix.listing_truncated);
        let complete = service.watch_files(Path::new("/repo"), &cancel).unwrap();
        assert_eq!(complete.paths.len(), 2);
        assert!(!complete.listing_truncated);
        let commands = service.executor.commands.lock().unwrap();
        assert_eq!(commands[0].stdout_record_limit, Some(1));
        assert_eq!(commands[1].stdout_record_limit, None);
    }

    #[test]
    fn ui_listing_cap_preserves_a_snapshot_when_status_exceeds_sixteen_megabytes() {
        let mut stdout = b"# branch.head main\0# branch.oid (initial)\0".to_vec();
        let path = "x".repeat(200);
        for index in 0..90_000 { stdout.extend_from_slice(format!("? {index}-{path}\0").as_bytes()); }
        assert!(stdout.len() > 16 * 1024 * 1024);
        let executor = Arc::new(FakeExecutor::with_outputs(vec![success(&stdout)]));
        let service = RepositoryService::new("git", executor.clone());
        let status = service.status_projection(Path::new("/repo"), true, 10_000, &CancellationToken::new()).unwrap();
        assert!(status.listing_truncated);
        assert_eq!(status.entries.len(), 10_000);
        let command = &executor.commands.lock().unwrap()[0];
        assert_eq!(command.stdout_record_limit, Some(20_016));
        assert_eq!(command.stdout_limit, 64 * 1024 * 1024);
    }

    fn success(stdout: &[u8]) -> Result<ProcessOutput, ProcessFailure> {
        Ok(ProcessOutput {
            stdout_truncated: false,
            exit_code: Some(0),
            signal: None,
            success: true,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        })
    }

    #[test]
    fn marker_scan_is_bounded_and_finds_markers_after_a_large_line() {
        let mut reader = io::repeat(b'x')
            .take(17 * 1024 * 1024)
            .chain(io::Cursor::new(b"\n<<<<<<< incoming\n"));
        assert!(scan_conflict_markers(&mut reader, &CancellationToken::new()).unwrap());
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            scan_conflict_markers(&mut io::Cursor::new(b"safe"), &cancel)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn snapshot_fsmonitor_honours_user_configuration_without_trusting_repository_hooks() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let worktree = directory.path().join("repo");
        fs::create_dir(&worktree).unwrap();
        let trusted = directory.path().join("monitor");
        fs::write(&trusted, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&trusted, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            trusted_fsmonitor(b"local\ttrue\n", &worktree),
            Some("true".into())
        );
        for scope in ["global", "system", "command"] {
            let config = format!("{scope}\t{}\n", trusted.display());
            assert_eq!(
                trusted_fsmonitor(config.as_bytes(), &worktree),
                Some(trusted.clone().into_os_string())
            );
        }
        for config in [
            "local\t./malicious-hook\n",
            "worktree\t./malicious-hook\n",
            "global\t./hooks/monitor\n",
            "global\t/bin/true;./hook\n",
            "local\tfalse\n",
        ] {
            assert_eq!(trusted_fsmonitor(config.as_bytes(), &worktree), None);
        }
        fs::create_dir(worktree.join(".git")).unwrap();
        let untrusted = worktree.join("monitor");
        fs::copy(&trusted, &untrusted).unwrap();
        assert_eq!(
            trusted_fsmonitor(
                format!("global\t{}\n", untrusted.display()).as_bytes(),
                &worktree
            ),
            None
        );
    }

    #[test]
    fn snapshot_cache_unchanged_reads_only_status_and_metadata_invalidation_rereads() {
        let outputs = (0..17).map(|_| success(b"")).collect();
        let executor = Arc::new(FakeExecutor::with_outputs(outputs));
        let git = RepositoryService::new("git", executor.clone());
        let mut cache = SnapshotCache::default();
        let cancel = CancellationToken::default();
        let root = Path::new("/snapshot-cache-test");
        assert!(
            git.snapshot_cached(root, SnapshotOptions::default(), &cancel, &mut cache)
                .unwrap()
                .is_some()
        );
        assert_eq!(executor.commands.lock().unwrap().len(), 8);
        assert!(
            git.snapshot_cached(root, SnapshotOptions::default(), &cancel, &mut cache)
                .unwrap()
                .is_none()
        );
        assert_eq!(executor.commands.lock().unwrap().len(), 9);
        cache.invalidate(true);
        assert!(
            git.snapshot_cached(root, SnapshotOptions::default(), &cancel, &mut cache)
                .unwrap()
                .is_some()
        );
        assert_eq!(executor.commands.lock().unwrap().len(), 17);
    }

    #[test]
    fn snapshot_cache_notices_truncation_even_when_the_retained_prefix_is_identical() {
        let mut outputs = vec![success(b""), success(b"? first.txt\0")];
        outputs.extend((0..6).map(|_| success(b"")));
        outputs.extend([success(b"? first.txt\0? second.txt\0"), success(b""), success(b"")]);
        let executor = Arc::new(FakeExecutor::with_outputs(outputs));
        let git = RepositoryService::new("git", executor);
        let mut cache = SnapshotCache::default();
        let cancel = CancellationToken::new();
        let options = SnapshotOptions { status_limit: Some(1), untracked_changes: true, untracked_files: false };
        let first = git.snapshot_cached(Path::new("/snapshot-limit-test"), options, &cancel, &mut cache).unwrap().unwrap();
        assert_eq!(first.status.entries.len(), 1);
        assert!(!first.status.listing_truncated);
        let next = git.snapshot_cached(Path::new("/snapshot-limit-test"), options, &cancel, &mut cache).unwrap()
            .expect("a newly truncated snapshot must update its lower-bound notice");
        assert_eq!(next.status.entries, first.status.entries);
        assert!(next.status.listing_truncated);
    }

    #[test]
    fn snapshot_cache_status_or_content_change_reuses_metadata() {
        let mut outputs = (0..8).map(|_| success(b"")).collect::<Vec<_>>();
        outputs.extend([success(b"? new.txt\0"), success(b""), success(b"")]);
        outputs.extend([success(b"? new.txt\0"), success(b""), success(b"")]);
        let executor = Arc::new(FakeExecutor::with_outputs(outputs));
        let git = RepositoryService::new("git", executor.clone());
        let mut cache = SnapshotCache::default();
        let cancel = CancellationToken::default();
        let root = Path::new("/snapshot-cache-test");
        git.snapshot_cached(
            root,
            SnapshotOptions {
                status_limit: None,
                untracked_changes: true,
                untracked_files: false,
            },
            &cancel,
            &mut cache,
        )
        .unwrap();
        let changed = git
            .snapshot_cached(
                root,
                SnapshotOptions {
                    status_limit: None,
                    untracked_changes: true,
                    untracked_files: false,
                },
                &cancel,
                &mut cache,
            )
            .unwrap()
            .unwrap();
        assert_eq!(changed.status.entries.len(), 1);
        cache.invalidate(false);
        assert!(
            git.snapshot_cached(
                root,
                SnapshotOptions {
                    status_limit: None,
                    untracked_changes: true,
                    untracked_files: false
                },
                &cancel,
                &mut cache
            )
            .unwrap()
            .is_some()
        );
        assert_eq!(executor.commands.lock().unwrap().len(), 14);
    }

    #[test]
    fn snapshot_cache_fallback_detects_content_changes_with_identical_status() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("new.txt");
        fs::write(&file, "abc\n").unwrap();
        let executor = Arc::new(FakeExecutor::with_outputs(vec![
            exit(1, b""),
            success(b"? new.txt\0"),
            success(b""),
            success(b""),
            success(b"? new.txt\0"),
            exit(1, b""),
            success(b""),
            success(b""),
            success(b"? new.txt\0"),
            success(b""),
            success(b""),
            success(b"? new.txt\0"),
        ]));
        let git = RepositoryService::new("git", executor.clone());
        let options = SnapshotOptions {
            status_limit: None,
            untracked_changes: true,
            untracked_files: false,
        };
        let cancel = CancellationToken::default();
        let mut cache = SnapshotCache::default();
        let first = git
            .snapshot_cached(directory.path(), options, &cancel, &mut cache)
            .unwrap()
            .unwrap();
        assert_eq!(first.status.entries[0].added_lines, Some(1));
        // Same size and same porcelain; only descriptor metadata detects this
        // when the native event was lost.
        fs::write(&file, "a\nb\n").unwrap();
        fs::File::open(&file)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(60))
            .unwrap();
        let second = git
            .snapshot_cached(directory.path(), options, &cancel, &mut cache)
            .unwrap()
            .unwrap();
        assert_eq!(second.status.entries[0].added_lines, Some(2));
        assert!(
            git.snapshot_cached(directory.path(), options, &cancel, &mut cache)
                .unwrap()
                .is_none()
        );
        assert_eq!(executor.commands.lock().unwrap().len(), 12);
    }

    #[test]
    fn snapshot_cache_browse_creation_refreshes_files_without_untracked_status() {
        let directory = tempfile::tempdir().unwrap();
        let executor = Arc::new(FakeExecutor::with_outputs(vec![
            exit(1, b""),
            success(b""),
            success(b""),
            success(b""),
            success(b""),
            exit(1, b""),
            success(b""),
            success(b""),
            success(b""),
            success(b""),
            success(b""),
            success(b"? new.txt\0"),
            success(b""),
        ]));
        let git = RepositoryService::new("git", executor.clone());
        let options = SnapshotOptions {
            status_limit: None,
            untracked_changes: false,
            untracked_files: true,
        };
        let cancel = CancellationToken::default();
        let mut cache = SnapshotCache::default();
        git.snapshot_cached(directory.path(), options, &cancel, &mut cache)
            .unwrap();
        fs::write(directory.path().join("new.txt"), "new\n").unwrap();
        fs::File::open(directory.path())
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(60))
            .unwrap();
        let next = git
            .snapshot_cached(directory.path(), options, &cancel, &mut cache)
            .unwrap()
            .unwrap();
        assert_eq!(next.files.paths, vec![path("new.txt")]);
        assert!(
            git.snapshot_cached(directory.path(), options, &cancel, &mut cache)
                .unwrap()
                .is_none()
        );
        assert_eq!(executor.commands.lock().unwrap().len(), 13);
    }

    #[test]
    fn snapshot_admin_fingerprint_tracks_refs_config_and_index_without_lock_noise() {
        let directory = tempfile::tempdir().unwrap();
        let gitdir = directory.path().join(".git");
        fs::create_dir_all(gitdir.join("refs/heads/feature")).unwrap();
        fs::write(gitdir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let first = snapshot_admin_fingerprint(directory.path());
        fs::write(gitdir.join("index.lock"), "temporary").unwrap();
        fs::remove_file(gitdir.join("index.lock")).unwrap();
        assert_eq!(first, snapshot_admin_fingerprint(directory.path()));
        fs::write(gitdir.join("refs/heads/main.lock"), "unpublished").unwrap();
        assert_eq!(first, snapshot_admin_fingerprint(directory.path()));
        fs::remove_file(gitdir.join("refs/heads/main.lock")).unwrap();
        assert_eq!(first, snapshot_admin_fingerprint(directory.path()));
        fs::write(
            gitdir.join("config"),
            "[remote \"origin\"]\nurl = example\n",
        )
        .unwrap();
        let configured = snapshot_admin_fingerprint(directory.path());
        assert_ne!(first, configured);
        fs::write(gitdir.join("index"), "new-index").unwrap();
        let indexed = snapshot_admin_fingerprint(directory.path());
        assert_ne!(configured, indexed);
        fs::write(gitdir.join("refs/heads/feature/new"), "oid\n").unwrap();
        fs::File::open(gitdir.join("refs/heads/feature"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(60))
            .unwrap();
        let created = snapshot_admin_fingerprint(directory.path());
        assert_ne!(indexed, created);
        fs::write(gitdir.join("refs/heads/feature/new"), "updated-oid\n").unwrap();
        assert_ne!(created, snapshot_admin_fingerprint(directory.path()));
    }

    #[test]
    fn untracked_measurement_cache_invalidates_size_mtime_and_inode() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("new.txt");
        fs::write(&file, "one\n").unwrap();
        let mut cache = BTreeMap::new();
        let measure = |cache: &mut BTreeMap<RepoPath, (FileIdentity, u64)>| {
            let mut entries = parse_status(b"? new.txt\0").unwrap().entries;
            measure_untracked_cached(
                directory.path(),
                &mut entries,
                &CancellationToken::default(),
                cache,
            );
            entries[0].added_lines.unwrap()
        };
        assert_eq!(measure(&mut cache), 1);
        let original = cache[&path("new.txt")].0;
        assert_eq!(measure(&mut cache), 1);
        fs::write(&file, "one\ntwo\n").unwrap();
        assert_eq!(measure(&mut cache), 2);
        assert_ne!(cache[&path("new.txt")].0, original);
        let second = cache[&path("new.txt")].0;
        let replacement = directory.path().join("replacement");
        fs::write(&replacement, "one two\n").unwrap();
        fs::rename(replacement, &file).unwrap();
        assert_eq!(measure(&mut cache), 1);
        assert_ne!(cache[&path("new.txt")].0, second);
    }

    #[test]
    fn parses_worktree_porcelain_and_ignores_future_fields() {
        let input = b"worktree /repo\0HEAD 0123456789012345678901234567890123456789\0branch refs/heads/main\0locked reason\0future value\0\0worktree /repo/wt\0HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0detached\0prunable missing\0\0";
        let records = parse_worktree_list(input).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].path, PathBuf::from("/repo"));
        assert_eq!(records[0].branch.as_deref(), Some("refs/heads/main"));
        assert_eq!(records[0].lock.reason.as_deref(), Some("reason"));
        assert!(records[1].detached);
        assert_eq!(records[1].prunable_reason.as_deref(), Some("missing"));
    }

    #[test]
    fn rejects_truncated_worktree_porcelain() {
        assert!(matches!(
            parse_worktree_list(b"worktree /repo"),
            Err(GitParseError::Truncated { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_worktree_paths() {
        use std::os::unix::ffi::OsStrExt;

        let mut input = b"worktree /repo/".to_vec();
        input.push(0xff);
        input.extend_from_slice(b"\0HEAD 0123456789012345678901234567890123456789\0\0");
        let records = parse_worktree_list(&input).unwrap();
        assert_eq!(records[0].path.as_os_str().as_bytes().last(), Some(&0xff));
    }

    /// The branch a worktree is on is a label. Requiring it to decode made one
    /// worktree checked out on a branch Git accepts fail the whole listing —
    /// the very degradation the status parser already applies to the same
    /// value under a different name.
    #[cfg(unix)]
    #[test]
    fn a_worktree_branch_that_is_not_utf8_costs_nothing_but_its_own_label() {
        let mut input =
            b"worktree /repo\0HEAD 0123456789012345678901234567890123456789\0branch refs/heads/b"
                .to_vec();
        input.push(0xff);
        input.extend_from_slice(b"ad\0\0worktree /repo/wt\0HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0branch refs/heads/main\0\0");
        let records = parse_worktree_list(&input).expect("a label is not a framing failure");

        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].branch.as_deref(),
            Some(String::from_utf8_lossy(b"refs/heads/b\xffad").as_ref())
        );
        // The worktree that shares the listing is unharmed, which is the whole
        // point: `list_worktrees` is what a project load depends on.
        assert_eq!(records[1].branch.as_deref(), Some("refs/heads/main"));
        // A `branch` field with no value at all is still framing, not a label.
        assert!(matches!(
            parse_worktree_list(b"worktree /repo\0branch\0\0"),
            Err(GitParseError::Malformed { .. })
        ));
    }

    #[test]
    fn parses_status_headers_changes_conflict_and_rename_source() {
        let input = b"# branch.oid 0123456789012345678901234567890123456789\0# branch.head feature\0# branch.upstream origin/feature\0# branch.ab +2 -3\x001 M. N... 100644 100644 100644 a b file\x002 .M S... 100644 100644 100644 a b R100 new name\0old name\0u UU N... 100644 100644 100644 100644 a b c conflict\0? untracked\0";
        let status = parse_status(input).unwrap();
        assert!(matches!(status.head, HeadState::Branch { ref name, .. } if name == "feature"));
        assert_eq!(status.dirty.staged, 1);
        assert_eq!(status.dirty.unstaged, 1);
        assert_eq!(status.dirty.conflicted, 1);
        assert_eq!(status.dirty.untracked, 1);
        assert_eq!(status.dirty.submodule, 1);
        assert!(matches!(
            status.tracking,
            TrackingState::Compared {
                ahead: 2,
                behind: 3,
                ..
            }
        ));
    }

    #[test]
    fn rejects_missing_rename_source_and_malformed_ab() {
        assert!(parse_status(b"2 R. N... 100644 100644 100644 a b R100 path\0").is_err());
        assert!(parse_status(b"# branch.ab +x -2\0").is_err());
        assert!(matches!(
            parse_status(b"? path"),
            Err(GitParseError::Truncated { .. })
        ));
        assert!(parse_status(&[]).is_err());
    }

    #[test]
    fn status_command_is_shell_free_and_explicit() {
        let service = RepositoryService::new("/usr/bin/git", Arc::new(FakeExecutor::default()));
        let command = service.status_command(Path::new("/tmp/repo; touch /tmp/pwned"));
        assert_eq!(command.program, PathBuf::from("/usr/bin/git"));
        assert_eq!(command.arguments[0], OsString::from("-C"));
        assert_eq!(
            command.arguments[1],
            OsString::from("/tmp/repo; touch /tmp/pwned")
        );
        assert!(
            !command
                .arguments
                .iter()
                .any(|argument| argument == OsStr::new("sh"))
        );
    }

    mod git_contracts {
        use super::*;

        struct IsolatedExecutor;

        impl ProcessExecutor for IsolatedExecutor {
            fn execute(
                &self,
                command: &CommandSpec,
                cancellation: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                SystemProcessExecutor.execute(
                    &command
                        .clone()
                        .env("GIT_CONFIG_GLOBAL", "/dev/null")
                        .env("GIT_CONFIG_SYSTEM", "/dev/null")
                        .env("GIT_CONFIG_NOSYSTEM", "1")
                        .env("GIT_AUTHOR_NAME", "Cagents Contract")
                        .env("GIT_AUTHOR_EMAIL", "contract@example.invalid")
                        .env("GIT_COMMITTER_NAME", "Cagents Contract")
                        .env("GIT_COMMITTER_EMAIL", "contract@example.invalid"),
                    cancellation,
                )
            }
        }

        fn service() -> RepositoryService<IsolatedExecutor> {
            RepositoryService::new("git", Arc::new(IsolatedExecutor))
        }

        #[test]
        fn large_literal_pathspec_mutations_roundtrip_without_argv_limits() {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let root = directory.path();
            git(root, &["init", "-b", "main"]);
            let names = [
                "space name",
                "line\nname",
                "-leading",
                "[glob]*",
                ":(glob)magic",
            ];
            let selection: Vec<_> = names
                .iter()
                .map(|name| PathSelection {
                    path: path(name),
                    old_path: None,
                })
                .collect();
            for name in names {
                fs::write(root.join(name), "base").unwrap();
            }
            let service = service();
            let cancel = CancellationToken::new();
            assert!(matches!(
                service
                    .stage(
                        &StageRequest {
                            worktree: root.into(),
                            selection: selection.clone()
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            git(root, &["commit", "-m", "initial"]);
            for name in names {
                fs::write(root.join(name), "changed").unwrap();
            }
            assert!(matches!(
                service
                    .stage(
                        &StageRequest {
                            worktree: root.into(),
                            selection: selection.clone()
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            assert!(matches!(
                service
                    .unstage(
                        &UnstageRequest {
                            worktree: root.into(),
                            selection: selection.clone()
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            assert!(matches!(
                service
                    .discard(
                        &DiscardRequest {
                            worktree: root.into(),
                            tracked: selection,
                            untracked: vec![],
                            confirmed: true
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            for name in names {
                assert_eq!(fs::read_to_string(root.join(name)).unwrap(), "base");
            }

            // 40k names exceed Linux ARG_MAX even before argv pointer overhead.
            let mut selection = Vec::with_capacity(40_000);
            for index in 0..40_000 {
                let name = format!(
                    "generated_component_file_with_an_extraordinarily_long_name_{index:05}.txt"
                );
                fs::write(root.join(&name), "generated").unwrap();
                selection.push(PathSelection {
                    path: path(&name),
                    old_path: None,
                });
            }
            assert!(pathspec_input(&selection_pathspecs(&selection)).len() > 2_097_152);
            assert!(matches!(
                service
                    .stage(
                        &StageRequest {
                            worktree: root.into(),
                            selection: selection.clone()
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            assert!(matches!(
                service
                    .unstage(
                        &UnstageRequest {
                            worktree: root.into(),
                            selection: selection.clone()
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            assert!(matches!(
                service
                    .discard(
                        &DiscardRequest {
                            worktree: root.into(),
                            tracked: vec![],
                            untracked: selection.into_iter().map(|entry| entry.path).collect(),
                            confirmed: true
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            assert!(
                !root
                    .join("generated_component_file_with_an_extraordinarily_long_name_00000.txt")
                    .exists()
            );
        }

        #[test]
        fn ignored_file_listing_preserves_collapsed_folders_and_git_rules() {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            git(root, &["init", "-b", "main"]);
            fs::write(
                root.join(".gitignore"),
                "build/\nempty/\n*.log\n!keep.log\n",
            )
            .unwrap();
            fs::create_dir_all(root.join("build/nested")).unwrap();
            fs::create_dir(root.join("empty")).unwrap();
            fs::write(root.join("build/nested/artifact"), "generated").unwrap();
            fs::write(root.join("build/tracked"), "tracked").unwrap();
            git(root, &["add", "-f", "build/tracked"]);
            fs::write(root.join("ignored #1.log"), "ignored").unwrap();
            fs::write(root.join("keep.log"), "included").unwrap();
            fs::write(root.join("private.txt"), "local ignore").unwrap();
            fs::write(root.join(".git/info/exclude"), "private.txt\n").unwrap();
            let files = service()
                .ignored_files(
                    root,
                    &[path("build/tracked")],
                    &CancellationToken::default(),
                )
                .unwrap();
            assert_eq!(
                files.ignored,
                vec![path("ignored #1.log"), path("private.txt")]
            );
            assert_eq!(
                files.ignored_directories,
                vec![path("build"), path("build/nested"), path("empty")]
            );
            assert!(
                !files
                    .ignored
                    .iter()
                    .any(|path| path.display().contains("artifact")),
                "collapsed folders must not enumerate descendants"
            );
            fs::remove_dir_all(root.join("build/nested")).unwrap();
            let files = service()
                .ignored_files(
                    root,
                    &[path("build/tracked")],
                    &CancellationToken::default(),
                )
                .unwrap();
            assert!(
                files.ignored_directories.contains(&path("build")),
                "an ignored folder with only tracked children still has ignored styling"
            );
        }

        #[test]
        fn amend_changes_the_message_without_requiring_staged_changes() {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            git(root, &["init", "-b", "main"]);
            fs::write(root.join("file.txt"), "original\n").unwrap();
            git(root, &["add", "file.txt"]);
            git(root, &["commit", "-m", "original"]);
            let before = git_output(root, &["rev-parse", "HEAD"]);
            let request = CommitRequest {
                worktree: root.into(),
                message: "New subject\n\nA full description\n".into(),
            };
            let result = service()
                .commit_with_amend(&request, true, &CancellationToken::new())
                .unwrap();
            assert!(
                matches!(result, CommitOutcome::Created { .. }),
                "{result:?}"
            );
            assert_ne!(git_output(root, &["rev-parse", "HEAD"]), before);
            assert_eq!(
                git_output(root, &["log", "-1", "--format=%B"]).as_slice(),
                b"New subject\n\nA full description\n\n"
            );
            assert_eq!(git_output(root, &["rev-list", "--count", "HEAD"]), b"1\n");
            let repeated = service()
                .commit_with_amend(&request, true, &CancellationToken::new())
                .unwrap();
            assert!(
                matches!(repeated, CommitOutcome::Created { .. }),
                "{repeated:?}"
            );
        }

        #[test]
        fn collapsed_untracked_snapshot_skips_untracked_status_and_file_listing() {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            git(root, &["init", "-b", "main"]);
            fs::write(root.join("tracked.txt"), "tracked\n").unwrap();
            fs::write(root.join(".gitignore"), "ignored.txt\nignored-dir/\n").unwrap();
            git(root, &["add", "tracked.txt", ".gitignore"]);
            fs::write(root.join("untracked.txt"), "one\ntwo\n").unwrap();
            fs::write(root.join("ignored.txt"), "ignored\n").unwrap();
            fs::create_dir(root.join("ignored-dir")).unwrap();
            fs::write(root.join("ignored-dir/nested.txt"), "ignored\n").unwrap();
            fs::write(root.join(".git/info/exclude"), "private.txt\n").unwrap();
            fs::write(root.join("private.txt"), "ignored\n").unwrap();
            let cancel = CancellationToken::new();
            let collapsed = service()
                .snapshot_with_options(
                    root,
                    SnapshotOptions {
                        status_limit: None,
                        untracked_changes: false,
                        untracked_files: false,
                    },
                    &cancel,
                )
                .unwrap();
            assert_eq!(collapsed.status.dirty.untracked, 0);
            assert_eq!(
                collapsed.files.paths,
                vec![path(".gitignore"), path("tracked.txt")]
            );
            assert!(
                collapsed
                    .status
                    .entries
                    .iter()
                    .all(|entry| entry.area != ChangeArea::Untracked)
            );
            let expanded = service()
                .snapshot_with_options(root, SnapshotOptions::default(), &cancel)
                .unwrap();
            assert_eq!(expanded.status.dirty.untracked, 1);
            assert_eq!(
                expanded.files.paths,
                vec![
                    path(".gitignore"),
                    path("tracked.txt"),
                    path("untracked.txt")
                ]
            );
            assert_eq!(
                expanded
                    .status
                    .area(ChangeArea::Untracked)
                    .next()
                    .unwrap()
                    .added_lines,
                Some(2)
            );
            let command = service().status_command_with_untracked(root, false);
            assert!(
                command
                    .arguments
                    .contains(&OsString::from("--untracked-files=no"))
            );
        }

        #[test]
        fn commit_navigation_reads_root_renames_and_deletions_from_immutable_objects() {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            git(root, &["init", "-b", "main"]);
            fs::write(root.join("old.txt"), "original\n").unwrap();
            git(root, &["add", "-A"]);
            git(root, &["commit", "-m", "initial"]);
            let cancel = CancellationToken::new();
            let first = service().resolve_head(root, &cancel).unwrap().unwrap();
            let entries = service()
                .commit_diff_entries(root, &first, None, &cancel)
                .unwrap();
            assert_eq!(entries[0].kind, ChangeKind::Added);
            git(root, &["mv", "old.txt", "new.txt"]);
            git(root, &["commit", "-m", "rename"]);
            let renamed = service().resolve_head(root, &cancel).unwrap().unwrap();
            git(root, &["rm", "new.txt"]);
            git(root, &["commit", "-m", "delete"]);
            let deleted = service().resolve_head(root, &cancel).unwrap().unwrap();
            let entries = service()
                .commit_diff_entries(root, &renamed, Some(&first), &cancel)
                .unwrap();
            assert_eq!(entries[0].kind, ChangeKind::Renamed);
            assert_eq!(entries[0].old_path, Some(path("old.txt")));
            let entries = service()
                .commit_diff_entries(root, &deleted, Some(&renamed), &cancel)
                .unwrap();
            assert_eq!(entries[0].kind, ChangeKind::Deleted);
            assert!(!root.join("new.txt").exists());
            // A historical rename remains readable after both names disappeared.
            let diff = service()
                .diff(
                    &DiffRequest {
                        worktree: root.into(),
                        path: path("new.txt"),
                        old_path: Some(path("old.txt")),
                        basis: DiffBasis::CommitToParent {
                            oid: renamed,
                            parent: Some(first),
                        },
                    },
                    &cancel,
                )
                .unwrap();
            assert!(matches!(
                diff,
                DiffContent::Comparison {
                    old: DiffSide::Text { .. },
                    new: DiffSide::Text { .. },
                    ..
                }
            ));
        }

        fn git(directory: &Path, arguments: &[&str]) {
            let service = service();
            service
                .run_checked(
                    &service.git_read(directory, arguments.iter().copied()),
                    &CancellationToken::new(),
                )
                .expect("Git fixture command succeeds");
        }

        fn git_output(directory: &Path, arguments: &[&str]) -> Vec<u8> {
            let service = service();
            service
                .run_checked(
                    &service.git_read(directory, arguments.iter().copied()),
                    &CancellationToken::new(),
                )
                .expect("Git fixture read succeeds")
                .stdout
        }

        #[cfg(unix)]
        #[test]
        fn unstage_preserves_worktree_head_and_unselected_index_entries() {
            use std::os::unix::fs::PermissionsExt;

            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            git(root, &["init", "-b", "feature"]);
            let unusual = RepoPath::from_bytes(b":(glob)*\n\xff").unwrap();
            for name in ["old.txt", "deleted.txt", "unselected.txt"] {
                fs::write(root.join(name), b"base\n").unwrap();
            }
            fs::write(root.join(unusual.as_os_str()), b"base\n").unwrap();
            git(root, &["add", "-A"]);
            git(root, &["commit", "-m", "base"]);
            let original_head = git_output(root, &["rev-parse", "HEAD"]);
            fs::rename(root.join("old.txt"), root.join("new.txt")).unwrap();
            fs::remove_file(root.join("deleted.txt")).unwrap();
            fs::write(root.join(unusual.as_os_str()), b"staged\n").unwrap();
            fs::write(root.join("unselected.txt"), b"keep staged\n").unwrap();
            git(root, &["add", "-A"]);
            fs::write(root.join("new.txt"), b"working rename\n").unwrap();
            fs::write(root.join(unusual.as_os_str()), b"working unusual\n").unwrap();
            let hook = root.join(".git/hooks/post-checkout");
            fs::write(&hook, b"#!/bin/sh\nprintf invoked > hook-ran\nexit 1\n").unwrap();
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

            let outcome = service()
                .unstage(
                    &UnstageRequest {
                        worktree: root.into(),
                        selection: vec![
                            PathSelection {
                                path: path("new.txt"),
                                old_path: Some(path("old.txt")),
                            },
                            PathSelection::new(path("deleted.txt")),
                            PathSelection::new(unusual.clone()),
                        ],
                    },
                    &CancellationToken::new(),
                )
                .unwrap();

            assert!(matches!(outcome, MutationOutcome::Applied { .. }));
            assert_eq!(git_output(root, &["rev-parse", "HEAD"]), original_head);
            assert_eq!(
                git_output(root, &["diff", "--cached", "--name-only", "-z"]),
                b"unselected.txt\0"
            );
            assert_eq!(fs::read(root.join("new.txt")).unwrap(), b"working rename\n");
            assert_eq!(
                fs::read(root.join(unusual.as_os_str())).unwrap(),
                b"working unusual\n"
            );
            assert!(!root.join("old.txt").exists());
            assert!(!root.join("deleted.txt").exists());
            assert!(!root.join("hook-ran").exists());
        }

        #[test]
        fn unborn_unstage_does_not_require_worktree_to_match_staged_content() {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            git(root, &["init", "-b", "feature"]);
            fs::write(root.join("selected.txt"), b"staged\n").unwrap();
            fs::write(root.join("unselected.txt"), b"keep\n").unwrap();
            git(root, &["add", "-A"]);
            fs::write(root.join("selected.txt"), b"working\n").unwrap();

            let outcome = service()
                .unstage(
                    &UnstageRequest {
                        worktree: root.into(),
                        selection: vec![
                            PathSelection::new(path("selected.txt")),
                            PathSelection::new(path("absent.txt")),
                        ],
                    },
                    &CancellationToken::new(),
                )
                .unwrap();

            assert_eq!(
                outcome,
                MutationOutcome::Applied {
                    paths: vec![path("selected.txt")]
                }
            );
            assert_eq!(git_output(root, &["ls-files", "-z"]), b"unselected.txt\0");
            assert_eq!(fs::read(root.join("selected.txt")).unwrap(), b"working\n");
            assert_eq!(
                service()
                    .resolve_head(root, &CancellationToken::new())
                    .unwrap(),
                None
            );
        }

        #[test]
        fn unstage_index_lock_refusal_preserves_staged_content() {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            git(root, &["init", "-b", "feature"]);
            fs::write(root.join("file.txt"), b"base\n").unwrap();
            git(root, &["add", "-A"]);
            git(root, &["commit", "-m", "base"]);
            fs::write(root.join("file.txt"), b"staged\n").unwrap();
            git(root, &["add", "-A"]);
            let index = fs::read(root.join(".git/index")).unwrap();
            fs::write(root.join(".git/index.lock"), b"another writer").unwrap();

            let outcome = service()
                .unstage(
                    &UnstageRequest {
                        worktree: root.into(),
                        selection: vec![PathSelection::new(path("file.txt"))],
                    },
                    &CancellationToken::new(),
                )
                .unwrap();

            assert!(matches!(outcome, MutationOutcome::NotApplied { .. }));
            assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
            assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"staged\n");
        }

        #[test]
        fn branch_comparison_remains_pinned_when_refs_move_between_git_reads() {
            struct AdvancingExecutor {
                root: PathBuf,
                next: Oid,
            }

            impl ProcessExecutor for AdvancingExecutor {
                fn execute(
                    &self,
                    command: &CommandSpec,
                    cancellation: &CancellationToken,
                ) -> Result<ProcessOutput, ProcessFailure> {
                    let output = IsolatedExecutor.execute(command, cancellation)?;
                    if command
                        .arguments
                        .iter()
                        .any(|argument| argument == "merge-base")
                    {
                        git(&self.root, &["update-ref", "HEAD", self.next.as_str()]);
                        git(
                            &self.root,
                            &["update-ref", "refs/heads/base", self.next.as_str()],
                        );
                    }
                    Ok(output)
                }
            }

            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let cancellation = CancellationToken::new();
            git(root, &["init", "-b", "base"]);
            fs::write(root.join("old.txt"), b"rename contents\n").unwrap();
            fs::write(root.join("deleted.txt"), b"removed\n").unwrap();
            git(root, &["add", "-A"]);
            git(root, &["commit", "-m", "ancestor"]);
            let ancestor = service()
                .resolve_head(root, &cancellation)
                .unwrap()
                .unwrap();
            git(root, &["checkout", "-b", "feature"]);
            fs::rename(root.join("old.txt"), root.join("new\nname.txt")).unwrap();
            fs::remove_file(root.join("deleted.txt")).unwrap();
            git(root, &["add", "-A"]);
            git(root, &["commit", "-m", "branch changes"]);
            let compared = service()
                .resolve_head(root, &cancellation)
                .unwrap()
                .unwrap();
            fs::write(root.join("later.txt"), b"not in comparison\n").unwrap();
            git(root, &["add", "-A"]);
            git(root, &["commit", "-m", "later"]);
            let next = service()
                .resolve_head(root, &cancellation)
                .unwrap()
                .unwrap();
            git(root, &["checkout", "base"]);
            fs::write(root.join("base-only.txt"), b"not a branch change\n").unwrap();
            git(root, &["add", "-A"]);
            git(root, &["commit", "-m", "base advanced"]);
            git(root, &["checkout", "feature"]);
            git(root, &["update-ref", "HEAD", compared.as_str()]);
            let base_ref = ValidatedRefName::parse("refs/heads/base").unwrap();
            let advancing = RepositoryService::new(
                "git",
                Arc::new(AdvancingExecutor {
                    root: root.into(),
                    next,
                }),
            );

            let comparison = advancing
                .branch_comparison(root, &base_ref, &cancellation)
                .unwrap();

            assert_eq!(comparison.head, compared);
            assert_eq!(comparison.merge_base, ancestor);
            assert_eq!(
                comparison.entries,
                vec![
                    DiffEntry {
                        path: path("deleted.txt"),
                        old_path: None,
                        kind: ChangeKind::Deleted,
                        added_lines: Some(0),
                        removed_lines: Some(1),
                    },
                    DiffEntry {
                        path: path("new\nname.txt"),
                        old_path: Some(path("old.txt")),
                        kind: ChangeKind::Renamed,
                        added_lines: Some(0),
                        removed_lines: Some(0),
                    },
                ]
            );
            let content = advancing
                .diff(
                    &DiffRequest {
                        worktree: root.into(),
                        basis: DiffBasis::MergeBaseToHead {
                            base_ref,
                            head: comparison.head,
                            merge_base: comparison.merge_base,
                        },
                        path: path("new\nname.txt"),
                        old_path: Some(path("old.txt")),
                    },
                    &cancellation,
                )
                .unwrap();
            let DiffContent::Comparison { old, new, .. } = content else {
                panic!("committed range must be a comparison");
            };
            assert_eq!(old.text(), Some("rename contents\n"));
            assert_eq!(new.text(), Some("rename contents\n"));
        }

        #[test]
        fn persisted_common_directory_preserves_identity_and_main_worktree() {
            let directory = tempfile::tempdir().unwrap();
            git(directory.path(), &["init", "-b", "main", "checkout"]);
            let root = directory.path().join("checkout").canonicalize().unwrap();
            let nested = root.join("nested");
            fs::create_dir(&nested).unwrap();
            let service = service();
            let cancellation = CancellationToken::new();
            let initial = service.discover(&nested, &cancellation).unwrap();
            let reopened = service
                .discover(&initial.common_dir, &cancellation)
                .unwrap();
            assert_eq!(initial.worktree_root.as_deref(), Some(root.as_path()));
            assert_eq!(reopened.worktree_root, initial.worktree_root);
            assert_eq!(reopened.id, initial.id);
            assert_eq!(reopened.common_dir, initial.common_dir);
            assert!(!reopened.is_bare);

            git(&root, &["commit", "--allow-empty", "-m", "initial"]);
            git(&root, &["worktree", "add", "-b", "linked", "../linked"]);
            let linked = directory.path().join("linked").canonicalize().unwrap();
            let linked_info = service.discover(&linked, &cancellation).unwrap();
            let linked_reopened = service
                .discover(&linked_info.git_dir, &cancellation)
                .unwrap();
            assert_eq!(
                linked_reopened.worktree_root.as_deref(),
                Some(linked.as_path())
            );
            assert_eq!(linked_reopened.id, initial.id);
            assert_eq!(linked_reopened.common_dir, initial.common_dir);
            let project_reopened = service
                .discover(&linked_info.common_dir, &cancellation)
                .unwrap();
            assert_eq!(
                project_reopened.worktree_root.as_deref(),
                Some(root.as_path())
            );
            assert_eq!(project_reopened.id, initial.id);
        }

        #[test]
        fn custom_git_directory_resolves_configured_worktree_not_metadata() {
            let directory = tempfile::tempdir().unwrap();
            git(
                directory.path(),
                &[
                    "init",
                    "-b",
                    "main",
                    "--separate-git-dir",
                    "metadata",
                    "checkout",
                ],
            );
            let root = directory.path().join("checkout").canonicalize().unwrap();
            let metadata = directory.path().join("metadata").canonicalize().unwrap();
            git(&root, &["config", "core.worktree", "../checkout"]);
            let service = service();
            let cancellation = CancellationToken::new();
            let initial = service.discover(&root, &cancellation).unwrap();
            let reopened = service
                .discover(&initial.common_dir, &cancellation)
                .unwrap();
            assert_eq!(reopened.common_dir, metadata);
            assert_eq!(reopened.worktree_root.as_deref(), Some(root.as_path()));
            assert_eq!(reopened.id, initial.id);
            assert!(!reopened.is_bare);
        }

        #[test]
        fn separate_git_directory_without_worktree_backlink_is_not_a_root() {
            let directory = tempfile::tempdir().unwrap();
            git(
                directory.path(),
                &[
                    "init",
                    "-b",
                    "main",
                    "--separate-git-dir",
                    "metadata",
                    "checkout",
                ],
            );
            let root = directory.path().join("checkout").canonicalize().unwrap();
            let metadata = directory.path().join("metadata").canonicalize().unwrap();
            let service = service();
            let cancellation = CancellationToken::new();
            assert_eq!(
                service
                    .discover(&root, &cancellation)
                    .unwrap()
                    .worktree_root,
                Some(root),
            );
            let error = service.discover(&metadata, &cancellation).unwrap_err();
            assert!(
                matches!(&error, RepositoryError::NoWorkTree { git_dir } if git_dir == &metadata),
                "{error:?}"
            );
        }

        #[test]
        fn bare_repository_preserves_identity_without_a_worktree() {
            let directory = tempfile::tempdir().unwrap();
            git(
                directory.path(),
                &["init", "--bare", "-b", "main", "bare.git"],
            );
            let root = directory.path().join("bare.git").canonicalize().unwrap();
            let service = service();
            let cancellation = CancellationToken::new();
            let initial = service.discover(&root, &cancellation).unwrap();
            let reopened = service
                .discover(&initial.common_dir, &cancellation)
                .unwrap();
            assert!(reopened.is_bare);
            assert_eq!(reopened.worktree_root, None);
            assert_eq!(reopened.common_dir, root);
            assert_eq!(reopened.id, initial.id);
        }
    }

    #[test]
    fn validates_branch_names_without_normalizing_them() {
        assert_eq!(
            ValidatedBranchName::parse("feature/safe").unwrap().as_str(),
            "feature/safe"
        );
        for invalid in ["-force", "bad name", "a..b", "a.lock", "a@{b", "a//b"] {
            assert!(ValidatedBranchName::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn create_command_keeps_untrusted_values_in_single_arguments() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("feature; echo nope");
        let request = CreateWorktreeRequest {
            repository: PathBuf::from("/repo"),
            managed_root: root.path().into(),
            destination: destination.clone(),
            branch: ValidatedBranchName::parse("feature/safe").unwrap(),
            base: BaseRevision::object_id("a".repeat(40)).unwrap(),
            track: false,
            operation_id: OperationId::from_uuid(Uuid::nil()),
        };
        let service = RepositoryService::new("git", Arc::new(FakeExecutor::default()));
        let plan = service.plan_worktree_creation(&request).unwrap();
        assert_eq!(
            plan.resolve_base.arguments.last().unwrap(),
            &OsString::from(format!("{}^{{commit}}", "a".repeat(40)))
        );
        let command = service
            .create_worktree_command(&request, &"a".repeat(40))
            .unwrap();
        assert!(command.arguments.contains(&destination.into_os_string()));
        assert!(
            !command
                .arguments
                .iter()
                .any(|argument| argument == OsStr::new("sh"))
        );
    }

    #[test]
    fn tracking_is_configured_after_creation_from_the_validated_ref() {
        let root = tempfile::tempdir().unwrap();
        let request = CreateWorktreeRequest {
            repository: PathBuf::from("/repo"),
            managed_root: root.path().into(),
            destination: root.path().join("feature"),
            branch: ValidatedBranchName::parse("feature").unwrap(),
            base: BaseRevision::remote_ref("refs/remotes/origin/main").unwrap(),
            track: true,
            operation_id: OperationId::from_uuid(Uuid::nil()),
        };
        let service = RepositoryService::new("git", Arc::new(FakeExecutor::default()));
        let create = service
            .create_worktree_command(&request, &"a".repeat(40))
            .unwrap();
        assert!(create.arguments.contains(&OsString::from("--no-track")));

        let tracking = service.set_upstream_command(&request).unwrap();
        assert!(tracking.arguments.contains(&OsString::from(
            "--set-upstream-to=refs/remotes/origin/main"
        )));
        assert_eq!(tracking.arguments.last(), Some(&OsString::from("feature")));
    }

    fn exit(code: i32, stdout: &[u8]) -> Result<ProcessOutput, ProcessFailure> {
        Ok(ProcessOutput {
            stdout_truncated: false,
            exit_code: Some(code),
            signal: None,
            success: code == 0,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        })
    }

    fn launched_failure(stdout: &[u8]) -> ProcessFailure {
        ProcessFailure {
            kind: ProcessFailureKind::Cancelled,
            program: "git".into(),
            exit_code: None,
            signal: None,
            stdout: stdout.to_vec(),
            stderr: b"cancelled".to_vec(),
            message: "cancelled".into(),
            started: true,
        }
    }

    fn unstarted_failure() -> ProcessFailure {
        ProcessFailure {
            kind: ProcessFailureKind::Spawn,
            program: "git".into(),
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            message: "no such file".into(),
            started: false,
        }
    }

    fn oid(seed: u8) -> Oid {
        Oid::parse(format!("{:02x}", seed).repeat(20)).unwrap()
    }

    fn path(value: &str) -> RepoPath {
        RepoPath::parse(value).unwrap()
    }

    fn fake_service(
        outputs: Vec<Result<ProcessOutput, ProcessFailure>>,
    ) -> RepositoryService<FakeExecutor> {
        RepositoryService::new("git", Arc::new(FakeExecutor::with_outputs(outputs)))
    }

    fn status_headers() -> Vec<u8> {
        let mut input = b"# branch.oid ".to_vec();
        input.extend_from_slice(oid(1).as_str().as_bytes());
        input.extend_from_slice(b"\0# branch.head main\0");
        input
    }

    #[test]
    fn repo_path_rejects_traversal_absolute_and_empty_paths() {
        assert!(matches!(RepoPath::parse(""), Err(RepoPathError::Empty)));
        assert!(matches!(
            RepoPath::parse("/etc/passwd"),
            Err(RepoPathError::NotRelative { .. })
        ));
        assert!(matches!(
            RepoPath::parse("../outside"),
            Err(RepoPathError::InvalidComponent { .. })
        ));
        assert!(matches!(
            RepoPath::parse("src/../../etc"),
            Err(RepoPathError::InvalidComponent { .. })
        ));
        assert!(matches!(
            RepoPath::parse("./here"),
            Err(RepoPathError::InvalidComponent { .. })
        ));
        assert!(matches!(
            RepoPath::parse("src//main.rs"),
            Err(RepoPathError::EmptyComponent { .. })
        ));
        // Git names a directory with one trailing separator, which is
        // canonicalized rather than refused; a second one is still an empty
        // component.
        assert_eq!(RepoPath::parse("src/").unwrap(), path("src"));
        assert!(matches!(
            RepoPath::parse("src//"),
            Err(RepoPathError::EmptyComponent { .. })
        ));
        assert!(matches!(RepoPath::parse("/"), Err(RepoPathError::Empty)));
        assert!(matches!(
            RepoPath::from_bytes(b"src/\0main.rs"),
            Err(RepoPathError::InteriorNul)
        ));

        let valid = path("src/deep/main.rs");
        assert_eq!(valid.extension().as_deref(), Some("rs"));
        assert_eq!(valid.file_name(), "main.rs");
        assert_eq!(valid.parent(), Some(path("src/deep")));
        assert_eq!(path("main.rs").parent(), None);
        assert_eq!(
            valid.resolve(Path::new("/work/tree")),
            PathBuf::from("/work/tree/src/deep/main.rs")
        );
        // A name that looks like pathspec magic is an ordinary path here; only
        // `--literal-pathspecs` keeps Git from reinterpreting it.
        assert_eq!(path(":(top)odd").display(), ":(top)odd");
    }

    #[cfg(unix)]
    #[test]
    fn repo_path_round_trips_non_utf8_bytes_through_serde() {
        let raw = b"src/\xffbroken.rs";
        let path = RepoPath::from_bytes(raw).unwrap();
        assert_eq!(path.as_bytes(), raw);
        // Lossy display must not be what is stored.
        assert_ne!(path.display().as_bytes(), raw);

        let encoded = serde_json::to_string(&path).unwrap();
        let decoded: RepoPath = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.as_bytes(), raw);
        assert!(
            serde_json::from_str::<RepoPath>("[47, 101, 116, 99]").is_err(),
            "deserialization must re-validate the path"
        );
    }

    #[test]
    fn oid_requires_lowercase_hex_and_rejects_the_null_id() {
        assert!(Oid::parse("a".repeat(40)).is_ok());
        assert!(Oid::parse("a".repeat(64)).is_ok());
        assert!(Oid::parse("A".repeat(40)).is_err());
        assert!(Oid::parse("a".repeat(39)).is_err());
        assert!(Oid::parse("z".repeat(40)).is_err());
        assert!(Oid::parse(ZERO_SHA1).is_ok());
        assert!(matches!(Oid::parse_object(ZERO_SHA1), Err(OidError::Null)));
    }

    #[test]
    fn status_command_lists_every_untracked_file() {
        let service = fake_service(Vec::new());
        let command = service.status_command(Path::new("/repo"));
        assert!(
            command
                .arguments
                .contains(&OsString::from("--untracked-files=all"))
        );
        assert!(
            !command
                .arguments
                .contains(&OsString::from("--untracked-files=normal"))
        );
    }

    #[test]
    fn status_reports_simultaneous_staged_and_unstaged_work_as_two_entries() {
        let mut input = status_headers();
        input.extend_from_slice(
            b"1 MM N... 100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 src/main.rs\0",
        );
        let status = parse_status(&input).unwrap();

        assert_eq!(status.entries.len(), 2);
        assert_eq!(status.entries[0].area, ChangeArea::Staged);
        assert_eq!(status.entries[0].kind, ChangeKind::Modified);
        assert_eq!(status.entries[1].area, ChangeArea::Unstaged);
        assert_eq!(status.entries[1].kind, ChangeKind::Modified);
        assert_eq!(status.entries[0].path, path("src/main.rs"));
        assert_eq!(status.entries[0].path, status.entries[1].path);
        // The badge counts must not change because entries now exist.
        assert_eq!(status.dirty.staged, 1);
        assert_eq!(status.dirty.unstaged, 1);
        assert_eq!(status.area(ChangeArea::Staged).count(), 1);
    }

    #[test]
    fn status_keeps_the_rename_source_on_the_renaming_half_only() {
        let mut input = status_headers();
        input.extend_from_slice(
            b"2 RM N... 100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 R100 new name.txt\0old name.txt\0",
        );
        let status = parse_status(&input).unwrap();

        assert_eq!(status.entries.len(), 2);
        assert_eq!(status.entries[0].kind, ChangeKind::Renamed);
        assert_eq!(status.entries[0].old_path, Some(path("old name.txt")));
        assert_eq!(status.entries[0].path, path("new name.txt"));
        // The worktree half is a modification of the destination, so the source
        // is not part of it.
        assert_eq!(status.entries[1].kind, ChangeKind::Modified);
        assert_eq!(status.entries[1].old_path, None);
    }

    #[test]
    fn status_maps_every_unmerged_code_to_its_conflict_kind() {
        let expected = [
            (&b"DD"[..], ConflictKind::BothDeleted),
            (b"AU", ConflictKind::AddedByUs),
            (b"UD", ConflictKind::DeletedByThem),
            (b"UA", ConflictKind::AddedByThem),
            (b"DU", ConflictKind::DeletedByUs),
            (b"AA", ConflictKind::BothAdded),
            (b"UU", ConflictKind::BothModified),
        ];
        let mut input = status_headers();
        for (code, _) in expected {
            input.extend_from_slice(b"u ");
            input.extend_from_slice(code);
            input.extend_from_slice(b" N... 100644 100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 3333333333333333333333333333333333333333 conflict/");
            input.extend_from_slice(code);
            input.push(0);
        }
        let status = parse_status(&input).unwrap();

        assert_eq!(status.entries.len(), expected.len());
        assert_eq!(status.dirty.conflicted, expected.len() as u32);
        for (entry, (code, kind)) in status.entries.iter().zip(expected) {
            assert_eq!(entry.area, ChangeArea::Conflict, "{:?}", entry.path);
            assert_eq!(entry.kind, ChangeKind::Unmerged);
            assert_eq!(entry.conflict, Some(kind), "{code:?}");
            assert_eq!(entry.added_lines, None);
        }

        let mut invalid = status_headers();
        invalid.extend_from_slice(b"u XY N... 100644 100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 3333333333333333333333333333333333333333 odd\0");
        assert!(parse_status(&invalid).is_err());
    }

    #[test]
    fn status_marks_submodule_entries_and_untracked_files() {
        let mut input = status_headers();
        input.extend_from_slice(
            b"1 .M SC.. 160000 160000 160000 1111111111111111111111111111111111111111 1111111111111111111111111111111111111111 vendor/dep\0",
        );
        input.extend_from_slice(b"? build/output.log\0");
        let status = parse_status(&input).unwrap();

        assert!(status.entries[0].submodule);
        assert_eq!(status.dirty.submodule, 1);
        assert_eq!(status.entries[1].area, ChangeArea::Untracked);
        assert_eq!(status.entries[1].kind, ChangeKind::Untracked);
        assert!(!status.entries[1].submodule);
        assert_eq!(status.dirty.untracked, 1);
    }

    /// `--untracked-files=all` still reports an untracked nested repository as
    /// a directory, because Git will not look inside another repository. That
    /// must not fail the whole snapshot.
    #[test]
    fn a_nested_repository_is_reported_as_its_directory() {
        let mut input = status_headers();
        input.extend_from_slice(b"? subclone/\0");
        input.extend_from_slice(b"? plain/deep/file.txt\0");
        let status = parse_status(&input).unwrap();

        assert_eq!(status.entries[0].path, path("subclone"));
        assert_eq!(status.entries[0].area, ChangeArea::Untracked);
        assert_eq!(status.entries[1].path, path("plain/deep/file.txt"));

        // `ls-files -co` interleaves cached and other entries, so the list is
        // sorted for a stable file tree.
        let listed = parse_path_list(b"subclone/\0a.txt\0", "git ls-files").unwrap();
        assert_eq!(listed, vec![path("a.txt"), path("subclone")]);
    }

    #[cfg(unix)]
    #[test]
    fn status_preserves_newline_and_non_utf8_paths() {
        let mut input = status_headers();
        input.extend_from_slice(b"? two\nlines.txt\0");
        input.extend_from_slice(b"? broken-\xff.txt\0");
        input.extend_from_slice(
            b"1 A. N... 000000 100644 100644 0000000000000000000000000000000000000000 2222222222222222222222222222222222222222 odd \xfe name\0",
        );
        let status = parse_status(&input).unwrap();

        assert_eq!(status.entries[0].path.as_bytes(), b"two\nlines.txt");
        assert_eq!(status.entries[1].path.as_bytes(), b"broken-\xff.txt");
        assert_eq!(status.entries[2].path.as_bytes(), b"odd \xfe name");
    }

    #[test]
    fn status_reports_an_unborn_head_with_its_branch() {
        let input = b"# branch.oid (initial)\0# branch.head main\0? first.txt\0";
        let status = parse_status(input).unwrap();

        assert_eq!(
            status.head,
            HeadState::Unborn {
                branch: Some("main".into())
            }
        );
        assert_eq!(status.branch(), Some("main"));
        assert_eq!(status.entries.len(), 1);
    }

    #[test]
    fn numstat_merging_fills_text_counts_and_leaves_binary_counts_unset() {
        let mut input = status_headers();
        input.extend_from_slice(
            b"1 M. N... 100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 src/main.rs\0",
        );
        input.extend_from_slice(
            b"1 A. N... 000000 100644 100644 0000000000000000000000000000000000000000 2222222222222222222222222222222222222222 art/logo.png\0",
        );
        input.extend_from_slice(
            b"2 R. N... 100644 100644 100644 1111111111111111111111111111111111111111 1111111111111111111111111111111111111111 R100 renamed.txt\0original.txt\0",
        );
        input.extend_from_slice(b"? untracked.txt\0");
        let mut status = parse_status(&input).unwrap();

        let records = parse_numstat(
            b"7\t2\tsrc/main.rs\0-\t-\tart/logo.png\x000\t0\t\0original.txt\0renamed.txt\0",
        )
        .unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[2].path, path("renamed.txt"));
        assert_eq!(records[2].old_path, Some(path("original.txt")));

        merge_numstat(&mut status.entries, ChangeArea::Staged, &records);
        let by_path = |name: &str| {
            status
                .entries
                .iter()
                .find(|entry| entry.path == path(name))
                .unwrap()
        };
        assert_eq!(by_path("src/main.rs").added_lines, Some(7));
        assert_eq!(by_path("src/main.rs").removed_lines, Some(2));
        // Binary files have no line counts, and a `-` must not become zero.
        assert_eq!(by_path("art/logo.png").added_lines, None);
        assert_eq!(by_path("renamed.txt").added_lines, Some(0));
        assert_eq!(by_path("untracked.txt").added_lines, None);
    }

    #[test]
    fn numstat_rejects_a_truncated_rename_record() {
        assert!(matches!(
            parse_numstat(b"0\t0\t\0original.txt\0"),
            Err(GitParseError::Truncated { .. })
        ));
        assert!(parse_numstat(b"x\t1\tfile\0").is_err());
    }

    #[test]
    fn name_status_keeps_rename_pairs_and_merges_counts() {
        let mut entries =
            parse_name_status(b"M\0src/main.rs\0R100\0old.txt\0new.txt\0A\0added.txt\0").unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].kind, ChangeKind::Renamed);
        assert_eq!(entries[1].old_path, Some(path("old.txt")));
        assert_eq!(entries[1].path, path("new.txt"));

        let records = parse_numstat(b"3\t1\tsrc/main.rs\0").unwrap();
        merge_diff_numstat(&mut entries, &records);
        assert_eq!(entries[0].added_lines, Some(3));
        assert_eq!(entries[0].removed_lines, Some(1));
        assert_eq!(entries[2].added_lines, None);
    }

    #[test]
    fn history_parses_identities_offsets_and_bodies() {
        let mut input = Vec::new();
        input.extend_from_slice(oid(0x0a).as_str().as_bytes());
        input.push(0);
        input.extend_from_slice(oid(0x0b).as_str().as_bytes());
        input.push(b' ');
        input.extend_from_slice(oid(0x0c).as_str().as_bytes());
        input.extend_from_slice(
            b"\0Ada\0ada@example.invalid\x001700000000 +0230\0Grace\0grace@example.invalid\x001700000900 -0500\0merge branch\0body line\n\n\0",
        );
        let commits = parse_history(&input).unwrap();

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].oid, oid(0x0a));
        assert_eq!(commits[0].parents, vec![oid(0x0b), oid(0x0c)]);
        assert_eq!(commits[0].author.name, "Ada");
        assert_eq!(commits[0].author.timestamp, 1_700_000_000);
        assert_eq!(commits[0].author.offset_minutes, 150);
        assert_eq!(commits[0].committer.offset_minutes, -300);
        assert_eq!(commits[0].subject, "merge branch");
        assert_eq!(commits[0].body, "body line");
    }

    #[test]
    fn history_rejects_malformed_records_instead_of_skipping_them() {
        // One field short of a record.
        let mut short = Vec::new();
        short.extend_from_slice(oid(1).as_str().as_bytes());
        short.extend_from_slice(b"\0\0Ada\0ada@example.invalid\x001700000000 +0000\0Ada\0ada@example.invalid\x001700000000 +0000\0subject\0");
        assert!(matches!(
            parse_history(&short),
            Err(GitParseError::Malformed { .. })
        ));

        let mut bad_oid = Vec::new();
        bad_oid.extend_from_slice(b"nothex\0\0Ada\0ada@example.invalid\x001700000000 +0000\0Ada\0ada@example.invalid\x001700000000 +0000\0subject\0\0");
        assert!(parse_history(&bad_oid).is_err());
        assert!(matches!(
            parse_history(b"missing terminator"),
            Err(GitParseError::Truncated { .. })
        ));
    }

    /// Git splits the offset field as `N / 100` hours plus `N % 100` minutes
    /// with no range check, so these are the offsets it really reports for
    /// commit objects imported from CVS and Subversion — and a rendered
    /// timestamp has to agree with `git log` on them rather than merely
    /// survive them. The commit objects themselves are pinned against real
    /// Git in `tests/repository_git_contract.rs`.
    #[test]
    fn a_commit_offset_is_read_exactly_as_git_reads_it() {
        assert_eq!(parse_timezone_offset("+0230"), Some(150));
        assert_eq!(parse_timezone_offset("-0500"), Some(-300));
        // Minutes are not capped at 60: `+0060` displays an hour east of UTC.
        assert_eq!(parse_timezone_offset("+0060"), Some(60));
        // Five digits, which the old four-digit length check rejected.
        assert_eq!(parse_timezone_offset("+51800"), Some(518 * 60));
        assert_eq!(parse_timezone_offset("+9999"), Some(99 * 60 + 99));
        assert_eq!(parse_timezone_offset(""), None);
        assert_eq!(parse_timezone_offset("0230"), None);
        assert_eq!(parse_timezone_offset("+garbage"), None);
        assert_eq!(parse_timezone_offset("+99999999999999"), None);
    }

    /// A date this cannot read costs its own commit its timestamp and nothing
    /// else. The whole 200-commit page used to be discarded instead.
    #[test]
    fn one_unreadable_commit_date_does_not_discard_the_page() {
        let mut input = Vec::new();
        for date in [&b"1700000000 0230"[..], b"", b"1700000000 +0060"] {
            input.extend_from_slice(oid(1).as_str().as_bytes());
            input.extend_from_slice(b"\0\0Ada\0ada@example.invalid\0");
            input.extend_from_slice(date);
            input.extend_from_slice(b"\0Ada\0ada@example.invalid\x001700000000 +0000\0subject\0\0");
        }
        let commits = parse_history(&input).unwrap();

        assert_eq!(commits.len(), 3);
        // An offset with no sign, then no date at all: both degrade to the
        // epoch in UTC without taking the record with them.
        assert_eq!(commits[0].author.timestamp, 1_700_000_000);
        assert_eq!(commits[0].author.offset_minutes, 0);
        assert_eq!(commits[1].author.timestamp, 0);
        assert_eq!(commits[1].author.offset_minutes, 0);
        assert_eq!(commits[2].author.offset_minutes, 60);
        // The committer half of every record was readable and is unaffected.
        assert!(
            commits
                .iter()
                .all(|commit| commit.committer.timestamp == 1_700_000_000)
        );
    }

    #[test]
    fn refs_and_remotes_retain_full_names() {
        let mut input = Vec::new();
        input.extend_from_slice(b"refs/heads/main\0");
        input.extend_from_slice(oid(1).as_str().as_bytes());
        input.extend_from_slice(b"\0\0refs/remotes/origin/main\0*\n");
        input.extend_from_slice(b"refs/remotes/origin/main\0");
        input.extend_from_slice(oid(1).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        input.extend_from_slice(b"refs/remotes/origin/HEAD\0");
        input.extend_from_slice(oid(1).as_str().as_bytes());
        input.extend_from_slice(b"\0refs/remotes/origin/main\0\0 \n");
        let refs = parse_refs(&input).unwrap();

        assert_eq!(refs.local.len(), 1);
        assert_eq!(refs.local[0].name.as_str(), "refs/heads/main");
        assert_eq!(refs.local[0].name.short_name(), "main");
        assert!(refs.local[0].head);
        assert_eq!(
            refs.local[0]
                .upstream
                .as_ref()
                .map(ValidatedRefName::as_str),
            Some("refs/remotes/origin/main")
        );
        // `refs/remotes/origin/HEAD` is a symbolic ref, so it is the remote's
        // default-branch pointer and not a branch named `HEAD`.
        assert_eq!(refs.remote.len(), 1);
        assert_eq!(refs.remote[0].remote, "origin");
        assert_eq!(refs.remote[0].branch, "main");
        assert_eq!(refs.remote_heads.len(), 1);
        assert_eq!(refs.remote_heads[0].remote, "origin");
        assert_eq!(
            refs.remote_heads[0].target.as_str(),
            "refs/remotes/origin/main"
        );

        let remotes = parse_remote_config(
            b"remote.origin.url\nssh://git@example.invalid/repo.git\0remote.origin.fetch\n+refs/heads/*\0remote.with.dots.pushurl\nhttps://example.invalid/other.git\0",
        )
        .unwrap();
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(
            remotes[0].fetch_url.as_deref(),
            Some("ssh://git@example.invalid/repo.git")
        );
        assert_eq!(remotes[0].push_url, None);
        assert_eq!(remotes[1].name, "with.dots");
        assert_eq!(
            remotes[1].push_url.as_deref(),
            Some("https://example.invalid/other.git")
        );
    }

    /// A ref nobody asked about is not evidence that Git's answer to the
    /// question that *was* asked is untrustworthy. Each of these used to
    /// return `Malformed` and take the entire snapshot — every changed path,
    /// every count — with it.
    #[test]
    fn an_unclassifiable_ref_is_skipped_rather_than_fatal() {
        let mut input = Vec::new();
        // Legal, and some tools create it: a ref directly under refs/remotes/
        // with no branch component.
        input.extend_from_slice(b"refs/remotes/weird\0");
        input.extend_from_slice(oid(1).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        // A branch name Git accepts and no encoding decodes.
        input.extend_from_slice(b"refs/heads/b\xffad\0");
        input.extend_from_slice(oid(2).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        // A tag, which this format never asks for but a caller could.
        input.extend_from_slice(b"refs/tags/v1\0");
        input.extend_from_slice(oid(3).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        // A symbolic remote ref that is not a HEAD pointer.
        input.extend_from_slice(b"refs/remotes/origin/alias\0");
        input.extend_from_slice(oid(4).as_str().as_bytes());
        input.extend_from_slice(b"\0refs/remotes/origin/main\0\0 \n");
        input.extend_from_slice(b"refs/heads/main\0");
        input.extend_from_slice(oid(5).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0*\n");
        let refs = parse_refs(&input).unwrap();

        assert_eq!(refs.local.len(), 1);
        assert_eq!(refs.local[0].oid, oid(5));
        assert!(refs.remote.is_empty());
        assert!(refs.remote_heads.is_empty());

        // The framing itself is still a hard requirement: a record with the
        // wrong field count means the output cannot be walked at all.
        assert!(parse_refs(b"refs/heads/main\0\0\n").is_err());
    }

    /// `git remote add te/am` is legal, so `refs/remotes/te/am/main` is `main`
    /// on `te/am` or `am/main` on `te` and the ref name alone cannot say
    /// which. The remote's symbolic `HEAD` is where the boundary is
    /// unambiguous. Both halves of the listing must be split there, because
    /// they exist to be joined on that field: naming the remote `te/am` in one
    /// and `te` in the other silently lost every remote-head lookup.
    #[test]
    fn both_halves_of_a_ref_listing_name_a_remote_the_same_way() {
        let mut input = Vec::new();
        input.extend_from_slice(b"refs/remotes/te/am/HEAD\0");
        input.extend_from_slice(oid(1).as_str().as_bytes());
        input.extend_from_slice(b"\0refs/remotes/te/am/main\0\0 \n");
        input.extend_from_slice(b"refs/remotes/te/am/main\0");
        input.extend_from_slice(oid(2).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        // A branch whose own name contains a slash, on a remote whose pointer
        // arrives *after* it: the split is against the whole listing, not
        // against whatever has been read so far.
        input.extend_from_slice(b"refs/remotes/origin/feature/x\0");
        input.extend_from_slice(oid(3).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        input.extend_from_slice(b"refs/remotes/origin/HEAD\0");
        input.extend_from_slice(oid(4).as_str().as_bytes());
        input.extend_from_slice(b"\0refs/remotes/origin/main\0\0 \n");
        // A remote added by hand rather than cloned has no `HEAD` pointer, so
        // there is nothing to split against and the first component is the
        // only reading available — which is also the one Git's own default
        // refspec produces.
        input.extend_from_slice(b"refs/remotes/bare/main\0");
        input.extend_from_slice(oid(5).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        let refs = parse_refs(&input).unwrap();

        assert_eq!(refs.remote_heads.len(), 2);
        assert_eq!(refs.remote_heads[0].remote, "te/am");
        let named = |name: &str| {
            refs.remote
                .iter()
                .find(|record| record.name.as_str() == name)
                .unwrap_or_else(|| panic!("{name} is missing from {:?}", refs.remote))
        };
        assert_eq!(named("refs/remotes/te/am/main").remote, "te/am");
        assert_eq!(named("refs/remotes/te/am/main").branch, "main");
        assert_eq!(named("refs/remotes/origin/feature/x").remote, "origin");
        assert_eq!(named("refs/remotes/origin/feature/x").branch, "feature/x");
        assert_eq!(named("refs/remotes/bare/main").remote, "bare");
        assert_eq!(named("refs/remotes/bare/main").branch, "main");

        // The join the two fields exist for: every branch of a remote that has
        // a pointer finds that remote's own pointer, and never another's.
        let snapshot = RepositorySnapshot {
            operation: None,
            stashes: Vec::new(),
            last_fetch_unix_ms: None,
            status: StatusSnapshot::default(),
            files: WorkingFiles::default(),
            remotes: Vec::new(),
            local_refs: refs.local.clone(),
            remote_refs: refs.remote.clone(),
            remote_heads: refs.remote_heads.clone(),
        };
        assert_eq!(
            snapshot
                .remote_head(&named("refs/remotes/te/am/main").remote)
                .map(ValidatedRefName::as_str),
            Some("refs/remotes/te/am/main")
        );
        assert_eq!(
            snapshot
                .remote_head(&named("refs/remotes/origin/feature/x").remote)
                .map(ValidatedRefName::as_str),
            Some("refs/remotes/origin/main")
        );
        assert_eq!(
            snapshot.remote_head(&named("refs/remotes/bare/main").remote),
            None
        );
    }

    /// `cleanup_whitespace` reduces a message this service sent by the rule
    /// Git already applied to the one it stored, so any divergence makes an
    /// identical message compare unequal. Differential against the tool
    /// itself, because `git stripspace` with no arguments *is*
    /// `--cleanup=whitespace`: it right-trims with Git's own `isspace`, which
    /// takes CR but leaves vertical tab and form feed alone.
    #[test]
    fn cleanup_whitespace_agrees_with_git_stripspace() {
        const MESSAGES: [&str; 8] = [
            "a\r\r\n",
            "subject\r\n\r\n\r\nbody  \t\r\n",
            "  a leading space is kept\n#a comment is kept\n\n",
            "\n\n\nsubject\n\n\n\nbody\n\n\n",
            "a vertical tab \x0b\nand a form feed \x0c\n",
            "\t \t \n",
            "",
            "one line, no newline",
        ];
        for message in MESSAGES {
            let Some(expected) = git_stripspace(message) else {
                eprintln!("skipping: `git` is not available on PATH");
                return;
            };
            assert_eq!(cleanup_whitespace(message), expected, "message {message:?}");
        }
    }

    /// The real `git stripspace`'s answer, with the trailing newline it always
    /// appends removed, or `None` when Git is not installed.
    fn git_stripspace(message: &str) -> Option<String> {
        use std::io::Write;

        let mut child = std::process::Command::new("git")
            .arg("stripspace")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(message.as_bytes())
            .expect("the message reaches git");
        let output = child.wait_with_output().expect("git stripspace runs");
        assert!(output.status.success(), "git stripspace failed");
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 from a UTF-8 message");
        Some(stdout.strip_suffix('\n').unwrap_or(&stdout).to_owned())
    }

    #[test]
    fn file_tree_synthesizes_directories_without_touching_the_filesystem() {
        let paths = vec![
            path("src/app/main.rs"),
            path("src/lib.rs"),
            path("Cargo.toml"),
            path("src/app/ui/view.rs"),
        ];
        let rows = build_file_tree(&paths);
        let rendered: Vec<_> = rows
            .iter()
            .map(|row| (row.path.display().into_owned(), row.depth, row.directory))
            .collect();

        assert_eq!(
            rendered,
            vec![
                ("src".to_owned(), 0, true),
                ("src/app".to_owned(), 1, true),
                ("src/app/ui".to_owned(), 2, true),
                ("src/app/ui/view.rs".to_owned(), 3, false),
                ("src/app/main.rs".to_owned(), 2, false),
                ("src/lib.rs".to_owned(), 1, false),
                ("Cargo.toml".to_owned(), 0, false),
            ]
        );
    }

    #[test]
    fn path_lists_deduplicate_unmerged_index_stages() {
        let paths =
            parse_path_list(b"src/main.rs\0src/main.rs\0Cargo.toml\0", "git ls-files").unwrap();
        assert_eq!(paths, vec![path("Cargo.toml"), path("src/main.rs")]);
    }

    #[test]
    fn selection_expands_renames_and_removes_duplicates() {
        let selection = vec![
            PathSelection {
                path: path("new.txt"),
                old_path: Some(path("old.txt")),
            },
            PathSelection::new(path("old.txt")),
            PathSelection::new(path("new.txt")),
        ];
        assert_eq!(
            selection_pathspecs(&selection),
            vec![path("old.txt"), path("new.txt")]
        );
    }

    #[test]
    fn pathspec_commands_place_literal_pathspecs_before_the_repository() {
        let service = fake_service(Vec::new());
        let command = service
            .stage_command(
                Path::new("/repo"),
                &[path(":(top)old.txt"), path("new name.txt")],
            )
            .unwrap();

        assert_eq!(command.program, PathBuf::from("git"));
        assert_eq!(
            command.arguments,
            vec![
                OsString::from("--literal-pathspecs"),
                OsString::from("-C"),
                OsString::from("/repo"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("add"),
                OsString::from("-A"),
                OsString::from("--pathspec-from-file=-"),
                OsString::from("--pathspec-file-nul"),
            ]
        );
        // A mutation must not disable optional locks: it needs the index lock.
        assert!(
            !command
                .environment
                .contains_key(OsStr::new("GIT_OPTIONAL_LOCKS"))
        );
        assert_eq!(
            command.environment.get(OsStr::new("GIT_TERMINAL_PROMPT")),
            Some(&OsString::from("0"))
        );

        let empty = service.stage_command(Path::new("/repo"), &[]);
        assert!(matches!(empty, Err(ValidationError::EmptySelection)));
    }

    /// `git add` and `git restore --worktree` match a pathspec against the
    /// index and the working tree. The source half of a staged rename is in
    /// neither, so listing it made the whole invocation fatal — exit 128,
    /// nothing staged — and reported as maybe-partial.
    #[test]
    fn a_rename_source_the_index_has_lost_is_not_sent_as_a_pathspec() {
        let selection = vec![PathSelection {
            path: path("new name.txt"),
            old_path: Some(path("old name.txt")),
        }];

        // The index no longer has the source: only HEAD does.
        let service = fake_service(vec![success(b""), success(b"")]);
        let outcome = service
            .stage(
                &StageRequest {
                    worktree: PathBuf::from("/repo"),
                    selection: selection.clone(),
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(
            outcome,
            MutationOutcome::Applied {
                paths: vec![path("new name.txt")]
            }
        );
        let commands = service.executor.commands.lock().unwrap();
        assert_eq!(
            commands[1].stdin.as_deref(),
            Some(b"new name.txt\0".as_slice())
        );
        assert!(
            !commands[1]
                .arguments
                .contains(&OsString::from("old name.txt"))
        );
        drop(commands);

        // The index still has it, which is the unstaged rename this
        // application also has to stage as a pair.
        let service = fake_service(vec![success(b"old name.txt\0"), success(b"")]);
        let outcome = service
            .stage(
                &StageRequest {
                    worktree: PathBuf::from("/repo"),
                    selection,
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(
            outcome,
            MutationOutcome::Applied {
                paths: vec![path("old name.txt"), path("new name.txt")]
            }
        );
    }

    /// `core.fsmonitor` set to a non-boolean value names a program Git runs
    /// whenever it refreshes the index, and `add`, `restore`, `rm --cached` and
    /// `commit` all refresh the index. Disabling it on reads alone would let a
    /// repository that arrived as an archive run code of its own choosing the
    /// first time the user staged a file, so every invocation is checked here.
    #[test]
    fn every_git_invocation_disables_the_repository_filesystem_monitor() {
        let service = fake_service(Vec::new());
        let worktree = PathBuf::from("/repo");
        let mutations = [
            service
                .stage_command(&worktree, &[path("new.txt")])
                .unwrap(),
            service
                .commit_command(&CommitRequest {
                    worktree: worktree.clone(),
                    message: "subject".into(),
                })
                .unwrap(),
            service.push_command(&publish_request()).unwrap(),
            service.git_literal_mutation(
                &worktree,
                pathspec_arguments(
                    ["reset", "--no-refresh", "--quiet", "HEAD"],
                    &[path("new.txt")],
                ),
            ),
            service.git_literal_mutation(
                &worktree,
                pathspec_arguments(["update-index", "--force-remove"], &[path("new.txt")]),
            ),
            service.git_literal_mutation(
                &worktree,
                pathspec_arguments(["restore", "--worktree"], &[path("new.txt")]),
            ),
        ];
        let reads = [
            service.status_command(&worktree),
            service.remotes_command(&worktree),
        ];

        for command in mutations.iter().chain(reads.iter()) {
            let position = command
                .arguments
                .iter()
                .position(|argument| argument == OsStr::new("core.fsmonitor=false"))
                .unwrap_or_else(|| {
                    panic!("{:?} does not disable core.fsmonitor", command.arguments)
                });
            // Git only accepts `-c` ahead of the subcommand, so the flag has to
            // sit in the global option block rather than anywhere in the argv.
            assert_eq!(command.arguments[position - 1], OsString::from("-c"));
            assert!(
                position <= 4,
                "core.fsmonitor=false is not in the global option block: {:?}",
                command.arguments
            );
        }

        // The read-only lock suppression stays read-only: a mutation needs the
        // index lock it would otherwise be denied.
        for command in &mutations {
            assert!(
                !command
                    .environment
                    .contains_key(OsStr::new("GIT_OPTIONAL_LOCKS"))
            );
        }
        for command in &reads {
            assert_eq!(
                command.environment.get(OsStr::new("GIT_OPTIONAL_LOCKS")),
                Some(&OsString::from("0"))
            );
        }
    }

    #[test]
    fn history_command_pages_two_hundred_commits_from_a_resolved_start() {
        let service = fake_service(Vec::new());
        let command =
            service.history_command(Path::new("/repo"), &oid(2), 3, Some(&path("src/main.rs")));

        assert_eq!(command.arguments[0], OsString::from("--literal-pathspecs"));
        assert_eq!(command.arguments[1], OsString::from("-C"));
        assert!(command.arguments.contains(&OsString::from("--skip=600")));
        assert!(
            command
                .arguments
                .contains(&OsString::from("--max-count=200"))
        );
        assert!(command.arguments.contains(&OsString::from("--date=raw")));
        assert!(
            command
                .arguments
                .contains(&OsString::from(format!("--format={HISTORY_FORMAT}")))
        );
        assert_eq!(
            command.arguments.last(),
            Some(&OsString::from("src/main.rs"))
        );
        assert_eq!(HISTORY_FORMAT.matches("%x00").count() + 1, HISTORY_FIELDS);
    }

    #[test]
    fn history_of_an_unborn_branch_is_empty_without_running_git_log() {
        let service = fake_service(vec![exit(1, b"")]);
        let commits = service
            .history(
                &HistoryRequest {
                    worktree: PathBuf::from("/repo"),
                    revision: HistoryRevision::Head,
                    page: 0,
                    path: None,
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(commits.is_empty());
        assert_eq!(service.executor.commands.lock().unwrap().len(), 1);
    }

    #[test]
    fn unstage_on_an_unborn_head_with_no_indexed_paths_runs_no_mutation() {
        let service = fake_service(vec![exit(1, b""), success(b"")]);
        let outcome = service
            .unstage(
                &UnstageRequest {
                    worktree: PathBuf::from("/repo"),
                    selection: vec![PathSelection::new(path("absent.txt"))],
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert_eq!(outcome, MutationOutcome::Applied { paths: Vec::new() });
        assert_eq!(service.executor.commands.lock().unwrap().len(), 2);
    }

    #[test]
    fn stage_that_never_started_is_definitively_not_applied() {
        let service = fake_service(vec![Err(unstarted_failure())]);
        let outcome = service
            .stage(
                &StageRequest {
                    worktree: PathBuf::from("/repo"),
                    selection: vec![PathSelection::new(path("a.txt"))],
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(outcome, MutationOutcome::NotApplied { .. }));

        let service = fake_service(vec![Err(launched_failure(b""))]);
        let outcome = service
            .stage(
                &StageRequest {
                    worktree: PathBuf::from("/repo"),
                    selection: vec![PathSelection::new(path("a.txt"))],
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(outcome, MutationOutcome::OutcomeUnknown { .. }));
    }

    #[test]
    fn discard_index_guard_preserves_foreign_or_modified_evidence() {
        for replace in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let index = directory.path().join("index");
            let lock = directory.path().join("index.lock");
            let mut guard = GitFileLock::acquire(&index).unwrap();
            if replace {
                fs::remove_file(&lock).unwrap();
            }
            fs::write(&lock, b"foreign evidence").unwrap();
            assert!(guard.check().is_err());
            assert!(guard.release().is_err());
            drop(guard);
            assert_eq!(fs::read(&lock).unwrap(), b"foreign evidence");
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn discard_index_guard_releases_through_its_open_directory_after_a_move() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("metadata");
        let moved = root.path().join("moved");
        fs::create_dir(&directory).unwrap();
        let mut guard = GitFileLock::acquire(&directory.join("index")).unwrap();
        fs::rename(&directory, &moved).unwrap();
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("index.lock"), b"replacement writer").unwrap();
        assert!(guard.check().is_err());
        guard.release().unwrap();
        assert!(!moved.join("index.lock").exists());
        assert_eq!(
            fs::read(directory.join("index.lock")).unwrap(),
            b"replacement writer"
        );
    }

    #[test]
    fn discard_index_guard_releases_owned_lock_during_unwind() {
        let directory = tempfile::tempdir().unwrap();
        let result = std::panic::catch_unwind(|| {
            let _guard = GitFileLock::acquire(&directory.path().join("index")).unwrap();
            panic!("injected discard worker panic");
        });
        assert!(result.is_err());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    fn fake_discard_service(
        worktree: &Path,
        classifications: Vec<Result<ProcessOutput, ProcessFailure>>,
    ) -> RepositoryService<FakeExecutor> {
        let git_directory = worktree.join(".git");
        fs::create_dir_all(&git_directory).unwrap();
        let mut index_path = git_directory
            .join("index")
            .as_os_str()
            .as_encoded_bytes()
            .to_vec();
        index_path.push(b'\n');
        fake_service(
            classifications
                .into_iter()
                .flat_map(|classification| [success(&index_path), classification])
                .collect(),
        )
    }

    #[test]
    fn discard_requires_confirmation_and_never_removes_a_directory() {
        let worktree = tempfile::tempdir().unwrap();
        fs::create_dir(worktree.path().join("keep")).unwrap();
        fs::write(worktree.path().join("keep/file.txt"), b"data").unwrap();

        // Resolve the index and classify the selection once per request,
        // while holding the index lock against external Git writers.
        let service = fake_discard_service(worktree.path(), vec![success(b""), success(b"")]);
        let unconfirmed = service.discard(
            &DiscardRequest {
                worktree: worktree.path().to_path_buf(),
                tracked: Vec::new(),
                untracked: vec![path("keep/file.txt")],
                confirmed: false,
            },
            &CancellationToken::new(),
        );
        assert!(matches!(
            unconfirmed,
            Err(RepositoryError::Validation(
                ValidationError::ConfirmationRequired
            ))
        ));
        assert!(worktree.path().join("keep/file.txt").exists());

        let directory = service
            .discard(
                &DiscardRequest {
                    worktree: worktree.path().to_path_buf(),
                    tracked: Vec::new(),
                    untracked: vec![path("keep")],
                    confirmed: true,
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(directory, MutationOutcome::NotApplied { .. }));
        assert!(worktree.path().join("keep/file.txt").exists());

        let removed = service
            .discard(
                &DiscardRequest {
                    worktree: worktree.path().to_path_buf(),
                    tracked: Vec::new(),
                    untracked: vec![path("keep/file.txt"), path("keep/file.txt")],
                    confirmed: true,
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(
            removed,
            MutationOutcome::Applied {
                paths: vec![path("keep/file.txt")]
            }
        );
        assert!(!worktree.path().join("keep/file.txt").exists());
        assert!(worktree.path().join("keep").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn discard_refuses_to_follow_a_symlinked_directory_out_of_the_worktree() {
        let worktree = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), worktree.path().join("link")).unwrap();

        let service = fake_discard_service(worktree.path(), vec![success(b"")]);
        let outcome = service
            .discard(
                &DiscardRequest {
                    worktree: worktree.path().to_path_buf(),
                    tracked: Vec::new(),
                    untracked: vec![path("link/secret.txt")],
                    confirmed: true,
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(matches!(
            outcome,
            MutationOutcome::NotApplied {
                failure: MutationFailure::Path { .. }
            }
        ));
        assert!(outside.path().join("secret.txt").exists());
    }

    /// The ancestor check and the unlink used to be two independent path
    /// resolutions, so a directory that was a real directory when it was
    /// checked could be a symlink by the time `remove_file` followed it. The
    /// unlink is now relative to the descriptor the check verified, which makes
    /// the window unexploitable rather than merely narrow.
    ///
    /// The swap runs concurrently because that is the only way to exercise an
    /// atomicity defect at all; the assertion is one-sided, so this can fail
    /// only when a path outside the worktree really was deleted.
    #[cfg(unix)]
    #[test]
    fn discard_never_unlinks_outside_the_worktree_while_an_ancestor_is_swapped() {
        const ROUNDS: usize = 400;

        let worktree = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let hostage = outside.path().join("victim.txt");
        fs::write(&hostage, b"outside").unwrap();

        let parked_directory = worktree.path().join("parked-directory");
        let parked_link = worktree.path().join("parked-link");
        let active = worktree.path().join("sub");
        fs::create_dir(&parked_directory).unwrap();
        std::os::unix::fs::symlink(outside.path(), &parked_link).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let swapper = {
            let stop = Arc::clone(&stop);
            let parked_directory = parked_directory.clone();
            let parked_link = parked_link.clone();
            let active = active.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    for parked in [&parked_directory, &parked_link] {
                        let _ = fs::rename(parked, &active);
                        let _ = fs::rename(&active, parked);
                    }
                }
            })
        };

        let request = DiscardRequest {
            worktree: worktree.path().to_path_buf(),
            tracked: Vec::new(),
            untracked: vec![path("sub/victim.txt")],
            confirmed: true,
        };
        let service = fake_discard_service(
            worktree.path(),
            (0..=ROUNDS).map(|_| success(b"")).collect(),
        );
        for _ in 0..ROUNDS {
            let _ = fs::write(parked_directory.join("victim.txt"), b"inside");
            let _ = fs::write(active.join("victim.txt"), b"inside");
            let _ = service.discard(&request, &CancellationToken::new());
            assert!(
                hostage.exists(),
                "discard followed a swapped ancestor out of the worktree"
            );
        }

        stop.store(true, Ordering::Relaxed);
        swapper.join().unwrap();

        // Each swap is balanced, so the swapper leaves `active` absent and both
        // prepared entries where they started. This quiescent round proves the
        // deletion the loop above was racing really happens, rather than being
        // refused every time.
        fs::rename(&parked_directory, &active).unwrap();
        fs::write(active.join("victim.txt"), b"inside").unwrap();
        let outcome = service
            .discard(&request, &CancellationToken::new())
            .unwrap();
        assert_eq!(
            outcome,
            MutationOutcome::Applied {
                paths: vec![path("sub/victim.txt")]
            }
        );
        assert!(!active.join("victim.txt").exists());
        assert!(hostage.exists());
    }

    /// `symlink_metadata` refuses to follow only the *final* component, so
    /// classifying `link/secret.txt` by name reported the file the link's target
    /// contains and the read that followed happily returned its bytes. Every
    /// component is now resolved beneath the worktree, so a link anywhere in the
    /// path is a refusal rather than a redirect.
    #[cfg(unix)]
    #[test]
    fn a_worktree_side_is_never_read_through_a_symlinked_directory() {
        let worktree = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), worktree.path().join("link")).unwrap();

        // An empty index listing leaves the old side absent, so the comparison
        // is decided entirely by what the worktree side reads.
        let service = fake_service(vec![success(b"")]);
        let content = service
            .diff(
                &DiffRequest {
                    worktree: worktree.path().to_path_buf(),
                    basis: DiffBasis::IndexToWorktree,
                    path: path("link/secret.txt"),
                    old_path: None,
                },
                &CancellationToken::new(),
            )
            .unwrap();

        let DiffContent::Comparison { new, .. } = content else {
            panic!("expected a comparison, got {content:?}");
        };
        assert!(
            matches!(new, DiffSide::Unreadable { .. }),
            "the worktree side was read through a symlinked directory: {new:?}"
        );
    }

    /// `--cleanup=strip` removes every line starting with `core.commentChar`,
    /// which is why Git's own default for `-F`/`--file` is `whitespace`: a
    /// message that was never shown in an editor has no commentary in it, so
    /// a subject like `#42 fix the thing` is content, not a comment.
    #[test]
    fn commit_sends_the_message_on_stdin_without_stripping_comment_lines() {
        let service = fake_service(Vec::new());
        let command = service
            .commit_command(&CommitRequest {
                worktree: PathBuf::from("/repo"),
                message: "#42 fix the thing\n\nbody\n".into(),
            })
            .unwrap();

        assert_eq!(
            command.arguments,
            vec![
                OsString::from("-C"),
                OsString::from("/repo"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("commit"),
                OsString::from("--file=-"),
                OsString::from("--cleanup=whitespace"),
            ]
        );
        assert_eq!(
            command.stdin.as_deref(),
            Some(&b"#42 fix the thing\n\nbody\n"[..])
        );
        assert!(matches!(
            service.commit_command(&CommitRequest {
                worktree: PathBuf::from("/repo"),
                message: "   \n".into(),
            }),
            Err(ValidationError::EmptyCommitMessage)
        ));
    }

    #[test]
    fn commit_without_staged_changes_is_not_created() {
        let service = fake_service(vec![
            // Nothing staged, and no sequencer state waiting to be concluded.
            exit(0, b""),
            success(b".git/MERGE_HEAD\n.git/CHERRY_PICK_HEAD\n.git/REVERT_HEAD\n"),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(matches!(
            outcome,
            CommitOutcome::NotCreated {
                failure: MutationFailure::Rejected { .. }
            }
        ));
        // Nothing beyond the two questions ran: no commit was attempted.
        assert_eq!(service.executor.commands.lock().unwrap().len(), 2);
    }

    /// A `git commit` that exits 0 made the commit HEAD now names, and that is
    /// the end of the question. A `prepare-commit-msg` hook prepending a
    /// ticket prefix is the ordinary furniture of a corporate repository, and
    /// a message comparison that outranked Git's own exit code reported every
    /// commit in one as "git reported success but HEAD names a commit this did
    /// not write".
    #[test]
    fn a_hook_that_rewrote_the_message_does_not_unmake_a_successful_commit() {
        let service = fake_service(vec![
            // Staged changes exist.
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            // `git commit` succeeded.
            success(b""),
            success(format!("{}\n", oid(2)).as_bytes()),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        match outcome {
            CommitOutcome::Created {
                oid: created,
                warnings,
            } => {
                assert_eq!(created, oid(2));
                assert!(warnings.is_empty(), "nothing failed: {warnings:?}");
            }
            other => panic!("expected a created commit, got {other:?}"),
        }
        // No fifth command: the message the hook stored was never read, so no
        // canned answer for it had to exist. Reading it could only produce a
        // worse answer than the one Git already gave.
        assert_eq!(service.executor.commands.lock().unwrap().len(), 4);
    }

    #[test]
    fn commit_that_moved_head_despite_a_failure_is_created_with_a_warning() {
        let service = fake_service(vec![
            // Staged changes exist.
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            // The commit itself failed, e.g. a cancellation that arrived after
            // `git commit` had already moved the ref.
            exit(1, b""),
            success(format!("{}\n", oid(2)).as_bytes()),
            // The commit at the new HEAD carries this call's message, with the
            // `Change-Id` trailer a `commit-msg` hook is allowed to append.
            success(b"subject\n\nChange-Id: I0\n\0"),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        match outcome {
            CommitOutcome::Created {
                oid: created,
                warnings,
            } => {
                assert_eq!(created, oid(2));
                assert_eq!(warnings.len(), 1);
            }
            other => panic!("expected a created commit, got {other:?}"),
        }
        // The message really was consulted, and it was consulted about the
        // commit HEAD moved to: on this path the message is the only evidence
        // of authorship there is, so a version of this that reached the same
        // verdict without asking would be reaching it by assumption.
        let commands = service.executor.commands.lock().unwrap();
        assert_eq!(commands.len(), 5);
        assert!(
            commands[4].arguments.ends_with(&[
                OsString::from("log"),
                OsString::from("-z"),
                OsString::from("--max-count=1"),
                OsString::from("--format=%B"),
                OsString::from("--end-of-options"),
                OsString::from(oid(2).as_str()),
            ]),
            "{:?}",
            commands[4].arguments
        );
    }

    /// The embedded terminals share this worktree and `MutationLocks` cannot
    /// exclude them, so HEAD moving while this commit failed may be the user's
    /// own `git commit` landing. Claiming that object id attributed a
    /// stranger's commit — a stranger's message, a stranger's contents — to
    /// this call. Denying it is no better, because the hook that rewrites a
    /// message is entitled to make the same evidence disappear for a commit
    /// this call did write. Unknown is the only honest answer, and a
    /// reconciliation read that fails itself lands in the same place rather
    /// than becoming an error.
    #[test]
    fn a_commit_this_call_did_not_write_is_never_claimed_or_denied() {
        let service = fake_service(vec![
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            exit(1, b"the hook said no\n"),
            success(format!("{}\n", oid(2)).as_bytes()),
            success(b"someone else's work\n\0"),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(
            matches!(outcome, CommitOutcome::OutcomeUnknown { .. }),
            "got {outcome:?}"
        );

        // The authorship read is a read, and a read never downgrades a
        // mutation that may have completed into a hard error.
        let service = fake_service(vec![
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            exit(1, b"the hook said no\n"),
            success(format!("{}\n", oid(2)).as_bytes()),
            exit(128, b""),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .expect("a failed reconciliation read is an outcome, not an error");
        assert!(
            matches!(outcome, CommitOutcome::OutcomeUnknown { .. }),
            "got {outcome:?}"
        );
    }

    #[test]
    fn commit_that_left_head_alone_is_not_created() {
        let service = fake_service(vec![
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            Err(launched_failure(b"")),
            success(format!("{}\n", oid(1)).as_bytes()),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(matches!(outcome, CommitOutcome::NotCreated { .. }));
    }

    #[test]
    fn commit_is_unknown_when_head_cannot_be_read_afterwards() {
        let service = fake_service(vec![
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            success(b""),
            // Reconciliation itself failed.
            exit(128, b""),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(matches!(outcome, CommitOutcome::OutcomeUnknown { .. }));
    }

    #[test]
    fn commit_that_reported_success_without_moving_head_is_unknown() {
        let service = fake_service(vec![
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            success(b""),
            success(format!("{}\n", oid(1)).as_bytes()),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(matches!(
            outcome,
            CommitOutcome::OutcomeUnknown {
                failure: MutationFailure::Rejected { .. }
            }
        ));
    }

    fn publish_request() -> PushRequest {
        PushRequest {
            worktree: PathBuf::from("/repo"),
            branch: ValidatedBranchName::parse("feature").unwrap(),
            target: PushTarget::SetUpstream {
                remote: "origin".into(),
                remote_branch: ValidatedBranchName::parse("feature").unwrap(),
            },
        }
    }

    #[test]
    fn push_command_sets_upstream_with_an_explicit_refspec_and_never_forces() {
        let service = fake_service(Vec::new());
        let command = service.push_command(&publish_request()).unwrap();

        assert_eq!(
            command.arguments,
            vec![
                OsString::from("-C"),
                OsString::from("/repo"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("push"),
                OsString::from("--porcelain"),
                OsString::from("--set-upstream"),
                OsString::from("origin"),
                OsString::from("refs/heads/feature:refs/heads/feature"),
            ]
        );
        assert!(
            !command
                .arguments
                .iter()
                .any(|argument| argument.to_string_lossy().contains("force"))
        );

        let existing = service
            .push_command(&PushRequest {
                worktree: PathBuf::from("/repo"),
                branch: ValidatedBranchName::parse("feature").unwrap(),
                target: PushTarget::Upstream {
                    remote: "upstream".into(),
                    remote_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
                },
            })
            .unwrap();
        assert!(
            !existing
                .arguments
                .contains(&OsString::from("--set-upstream"))
        );
        assert_eq!(
            existing.arguments.last(),
            Some(&OsString::from("refs/heads/feature:refs/heads/main"))
        );

        assert!(matches!(
            service.push_command(&PushRequest {
                worktree: PathBuf::from("/repo"),
                branch: ValidatedBranchName::parse("feature").unwrap(),
                target: PushTarget::Upstream {
                    remote: "--upload-pack=evil".into(),
                    remote_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
                },
            }),
            Err(ValidationError::InvalidRemote(_))
        ));
    }

    /// The refspec's destination is what the *remote* calls the branch, whether
    /// the upstream arrived as a remote-tracking ref or out of
    /// `branch.<name>.merge`.
    #[test]
    fn a_refspec_destination_is_never_a_remote_tracking_ref() {
        let service = fake_service(Vec::new());
        let refspec = |remote: &str, upstream: &str| {
            service
                .push_command(&PushRequest {
                    worktree: PathBuf::from("/repo"),
                    branch: ValidatedBranchName::parse("feature").unwrap(),
                    target: PushTarget::Upstream {
                        remote: remote.into(),
                        remote_ref: ValidatedRefName::parse(upstream).unwrap(),
                    },
                })
                .map(|command| command.arguments.last().unwrap().clone())
        };

        // What a snapshot's `%(upstream)` gives, which is the spelling the
        // sidebar carries. Pushing this one verbatim created a literal
        // `refs/remotes/origin/main` in the remote and left the branch alone.
        assert_eq!(
            refspec("origin", "refs/remotes/origin/main").unwrap(),
            OsString::from("refs/heads/feature:refs/heads/main")
        );
        // A branch published under another name follows the upstream's name.
        assert_eq!(
            refspec("origin", "refs/remotes/origin/published").unwrap(),
            OsString::from("refs/heads/feature:refs/heads/published")
        );
        // A slash in the branch name is not a second boundary to guess at.
        assert_eq!(
            refspec("origin", "refs/remotes/origin/topic/one").unwrap(),
            OsString::from("refs/heads/feature:refs/heads/topic/one")
        );
        // Split at the remote name that is known, not at the first slash: with
        // a remote called `te`, this ref tracks the branch `am/main`.
        assert_eq!(
            refspec("te", "refs/remotes/te/am/main").unwrap(),
            OsString::from("refs/heads/feature:refs/heads/am/main")
        );
        // What `branch.<name>.merge` holds is already the remote's own name.
        assert_eq!(
            refspec("origin", "refs/heads/main").unwrap(),
            OsString::from("refs/heads/feature:refs/heads/main")
        );
        // A tracking ref under a different remote names no branch on this one,
        // and which branch it meant is not something to guess on the wire.
        assert!(matches!(
            refspec("origin", "refs/remotes/upstream/main"),
            Err(ValidationError::ForeignTrackingRef { .. })
        ));
        // Nor can the wire-side type be spelled with a tracking ref directly,
        // which is also what keeps a deserialized outcome honest.
        assert!(matches!(
            RemoteSideRef::parse("refs/remotes/origin/main"),
            Err(ValidationError::NotARemoteSideRef(_))
        ));
        assert!(matches!(
            RemoteSideRef::parse("main"),
            Err(ValidationError::NotARemoteSideRef(_))
        ));
    }

    #[test]
    fn push_validates_the_remote_branch_name_with_git() {
        let service = fake_service(vec![exit(1, b"")]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();

        assert!(matches!(
            outcome,
            PushOutcome::NotPushed {
                failure: MutationFailure::Rejected { .. }
            }
        ));
        let commands = service.executor.commands.lock().unwrap();
        assert_eq!(
            commands[0].arguments,
            vec![
                OsString::from("-C"),
                OsString::from("/repo"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("--no-optional-locks"),
                OsString::from("-c"),
                OsString::from("diff.autoRefreshIndex=false"),
                OsString::from("check-ref-format"),
                OsString::from("refs/heads/feature"),
            ]
        );
    }

    /// The two `check-ref-format` invocations answer the same question about
    /// the same kind of argument and touch no repository state, so they are
    /// both reads. One of them used to be built as a mutation, which took the
    /// index lock and refreshed the index to inspect a string.
    #[test]
    fn every_command_that_plans_a_worktree_is_classified_as_a_read() {
        let service = fake_service(Vec::new());
        // The destination is validated against a real directory before any
        // command is built.
        let managed = tempfile::tempdir().unwrap();
        let plan = service
            .plan_worktree_creation(&CreateWorktreeRequest {
                repository: PathBuf::from("/repo"),
                managed_root: managed.path().to_path_buf(),
                destination: managed.path().join("feature"),
                branch: ValidatedBranchName::parse("feature").unwrap(),
                base: BaseRevision::LocalBranch(ValidatedBranchName::parse("main").unwrap()),
                track: false,
                operation_id: OperationId::new(),
            })
            .unwrap();

        for command in [
            &plan.validate_branch,
            &plan.resolve_base,
            &plan.check_branch_absent,
        ] {
            assert!(
                command
                    .arguments
                    .contains(&OsString::from("--no-optional-locks")),
                "{:?} must decline the index lock",
                command.arguments
            );
            assert!(
                command
                    .arguments
                    .contains(&OsString::from("diff.autoRefreshIndex=false")),
                "{:?} must not refresh the index",
                command.arguments
            );
        }
    }

    #[test]
    fn a_successful_push_reports_the_pushed_commit() {
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            success(b"/origin\0"),
            success(b""),
            success(b"/origin\n"),
            success(b"To /origin\n*\trefs/heads/feature:refs/heads/feature\t[new branch]\nDone\n"),
            success(format!("{}\n", oid(4)).as_bytes()),
        ]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();

        assert_eq!(
            outcome,
            PushOutcome::Pushed {
                remote: "origin".into(),
                remote_ref: RemoteSideRef::parse("refs/heads/feature").unwrap(),
                oid: oid(4),
            }
        );
    }

    #[test]
    fn a_rejected_push_is_definitively_not_pushed() {
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            success(b"/origin\0"),
            success(b""),
            success(b"/origin\n"),
            exit(
                1,
                b"To /origin\n!\trefs/heads/feature:refs/heads/feature\t[rejected] (fetch first)\nDone\n",
            ),
        ]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();

        assert!(matches!(outcome, PushOutcome::NotPushed { .. }));
        // No endpoint lookup follows rejection; only private preflight configuration reads run.
        assert_eq!(service.executor.commands.lock().unwrap().len(), 6);
    }

    #[test]
    fn a_push_that_never_started_is_not_pushed() {
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            success(b"/origin\0"),
            success(b""),
            success(b"/origin\n"),
            Err(unstarted_failure()),
        ]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();

        assert!(matches!(outcome, PushOutcome::NotPushed { .. }));
        assert_eq!(service.executor.commands.lock().unwrap().len(), 6);
    }

    #[test]
    fn an_uncertain_push_whose_remote_matches_is_reconciled_as_pushed() {
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            success(b"/origin\0"),
            success(b""),
            success(b"/origin\n"),
            Err(launched_failure(b"To /origin\n")),
            success(b"/origin\0"),
            success(b""),
            success(b"/origin\n"),
            success(format!("{}\trefs/heads/feature\n", oid(4)).as_bytes()),
        ]);
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let outcome = service.push(&publish_request(), &cancelled).unwrap();

        assert_eq!(
            outcome,
            PushOutcome::Pushed {
                remote: "origin".into(),
                remote_ref: RemoteSideRef::parse("refs/heads/feature").unwrap(),
                oid: oid(4),
            }
        );
        let commands = service.executor.commands.lock().unwrap();
        let count: u32 = commands[9].environment[OsStr::new("GIT_CONFIG_COUNT")]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let marker =
            commands[9].environment[OsStr::new(&format!("GIT_CONFIG_VALUE_{}", count - 1))].clone();
        assert_eq!(
            commands[9].environment[OsStr::new(&format!("GIT_CONFIG_KEY_{}", count - 1))],
            OsString::from("url./origin.insteadOf")
        );
        assert_eq!(
            commands[9].arguments,
            vec![
                OsString::from("-C"),
                OsString::from("/repo"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("--no-optional-locks"),
                OsString::from("-c"),
                OsString::from("diff.autoRefreshIndex=false"),
                OsString::from("ls-remote"),
                OsString::from("--refs"),
                OsString::from("--exit-code"),
                OsString::from("--"),
                marker,
                OsString::from("refs/heads/feature"),
            ]
        );
    }

    #[test]
    fn an_uncertain_push_whose_remote_differs_stays_unknown() {
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            success(b"/origin\0"),
            success(b""),
            success(b"/origin\n"),
            Err(launched_failure(b"")),
            success(b"/origin\0"),
            success(b""),
            success(b"/origin\n"),
            success(format!("{}\trefs/heads/feature\n", oid(5)).as_bytes()),
        ]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();
        assert!(matches!(outcome, PushOutcome::OutcomeUnknown { .. }));

        // An absent remote ref is also not proof that the push landed.
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            success(b"/origin\0"),
            success(b""),
            success(b"/origin\n"),
            Err(launched_failure(b"")),
            success(b"/origin\0"),
            success(b""),
            success(b"/origin\n"),
            exit(2, b""),
        ]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();
        assert!(matches!(outcome, PushOutcome::OutcomeUnknown { .. }));

        // Neither is a failed reconciliation lookup.
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            success(b"/origin\0"),
            success(b""),
            success(b"/origin\n"),
            Err(launched_failure(b"")),
            success(b"/origin\0"),
            success(b""),
            success(b"/origin\n"),
            exit(128, b""),
        ]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();
        assert!(matches!(outcome, PushOutcome::OutcomeUnknown { .. }));
    }

    #[test]
    fn failed_push_destination_capture_preserves_definitive_results_and_refuses_reconciliation() {
        for definite_success in [true, false] {
            let result = if definite_success {
                success(
                    b"To /origin\n*\trefs/heads/feature:refs/heads/feature\t[new branch]\nDone\n",
                )
            } else {
                Err(launched_failure(b""))
            };
            let mut responses = vec![
                success(b""),
                success(format!("{}\n", oid(4)).as_bytes()),
                Err(unstarted_failure()),
                result,
            ];
            if definite_success {
                responses.push(success(format!("{}\n", oid(4)).as_bytes()));
            }
            let service = fake_service(responses);
            let outcome = service
                .push(&publish_request(), &CancellationToken::new())
                .unwrap();
            assert!(if definite_success {
                matches!(outcome, PushOutcome::Pushed { .. })
            } else {
                matches!(outcome, PushOutcome::OutcomeUnknown { .. })
            });
            let commands = service.executor.commands.lock().unwrap();
            assert_eq!(commands.len(), if definite_success { 5 } else { 4 });
            assert!(
                commands[3]
                    .arguments
                    .iter()
                    .any(|argument| argument == "push")
            );
            assert!(!commands.iter().any(|command| {
                command
                    .arguments
                    .iter()
                    .any(|argument| argument == "ls-remote")
            }));
        }
    }

    #[test]
    fn successful_push_requires_a_readable_unchanged_source_observation() {
        for after in [
            success(format!("{}\n", oid(5)).as_bytes()),
            exit(1, b""),
            Err(unstarted_failure()),
        ] {
            let service = fake_service(vec![
                success(b""),
                success(format!("{}\n", oid(4)).as_bytes()),
                success(b"/origin\0"),
                success(b""),
                success(b"/origin\n"),
                success(
                    b"To /origin\n*\trefs/heads/feature:refs/heads/feature\t[new branch]\nDone\n",
                ),
                after,
            ]);
            let outcome = service
                .push(&publish_request(), &CancellationToken::new())
                .unwrap();
            assert!(matches!(
                outcome,
                PushOutcome::OutcomeUnknown {
                    failure: MutationFailure::Reconciliation { .. }
                }
            ));
            let commands = service.executor.commands.lock().unwrap();
            assert_eq!(commands.len(), 7);
            assert!(
                commands[6]
                    .arguments
                    .iter()
                    .any(|argument| argument == "rev-parse")
            );
            assert!(
                commands[6]
                    .arguments
                    .contains(&OsString::from("refs/heads/feature^{commit}"))
            );
        }
    }

    #[test]
    fn remote_ref_parser_refuses_malformed_or_duplicate_exact_records() {
        let requested = RemoteSideRef::parse("refs/heads/main").unwrap();
        for bytes in [
            b"missing-tab\n".to_vec(),
            format!("{}\t\n", oid(1)).into_bytes(),
            b"not-an-object\trefs/heads/main\n".to_vec(),
            format!("{}\trefs/heads/main\textra\n", oid(1)).into_bytes(),
            format!("{}\trefs/heads/main\n{}\trefs/heads/main\n", oid(1), oid(2)).into_bytes(),
        ] {
            assert!(matches!(
                parse_remote_ref_oid(&bytes, &requested),
                Err(RepositoryError::InvalidOutput { .. })
            ));
        }
        assert_eq!(parse_remote_ref_oid(b"", &requested).unwrap(), None);
    }

    #[test]
    fn push_destination_resolution_refuses_ambiguous_framing_and_preserves_path_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let service = fake_service(vec![
            exit(1, b""),
            success(b"/first\0/second\0"),
            success(b""),
            success(b"/rewritten-first\n/rewritten-second\n"),
        ]);
        assert_eq!(
            service.push_destinations(Path::new("/repo"), "origin", &CancellationToken::new()),
            Some(vec![
                OsString::from("/rewritten-first"),
                OsString::from("/rewritten-second")
            ])
        );
        for responses in [
            vec![success(b"/first\n/second\0")],
            vec![
                success(b"/original\0"),
                success(b""),
                success(b"/first\n/second\n"),
            ],
            vec![success(b"\0")],
            vec![
                success(b"/original\0"),
                success(b"UrL./bad\nbase.PushInsteadOf\0"),
            ],
            vec![success(b"/original\0"), success(b"malformed-key-list")],
            vec![
                success(b"/original\0"),
                success(b""),
                success(b"/embedded\0nul\n"),
            ],
        ] {
            let service = fake_service(responses);
            assert!(
                service
                    .push_destinations(Path::new("/repo"), "origin", &CancellationToken::new())
                    .is_none()
            );
        }
        let service = fake_service(vec![
            success(b"/original\0"),
            success(b""),
            success(b"/non-utf8-\xff\n"),
        ]);
        let destinations = service
            .push_destinations(Path::new("/repo"), "origin", &CancellationToken::new())
            .unwrap();
        assert_eq!(destinations[0].as_os_str().as_bytes(), b"/non-utf8-\xff");
    }

    #[test]
    fn push_endpoint_alias_preserves_bytes_and_appends_bounded_config_entries() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let service = fake_service(vec![]);
        let endpoint = OsString::from_vec(b"/private=endpoint-\xff.git".to_vec());
        let reference = RemoteSideRef::parse("refs/heads/main").unwrap();
        let command = service
            .push_endpoint_command(
                Path::new("/repo"),
                &endpoint,
                &reference,
                Some(OsStr::new("2")),
            )
            .unwrap();
        assert_eq!(
            command.environment[OsStr::new("GIT_CONFIG_COUNT")],
            OsString::from("3")
        );
        assert_eq!(
            command.environment[OsStr::new("GIT_CONFIG_KEY_2")].as_bytes(),
            b"url./private=endpoint-\xff.git.insteadOf"
        );
        let marker = &command.environment[OsStr::new("GIT_CONFIG_VALUE_2")];
        assert!(command.arguments.contains(marker));
        assert!(!command.arguments.iter().any(|argument| {
            argument
                .as_bytes()
                .windows(b"private=endpoint".len())
                .any(|part| part == b"private=endpoint")
        }));
        assert!(
            !command
                .environment
                .contains_key(OsStr::new("GIT_CONFIG_KEY_0"))
        );
        assert!(
            !command
                .environment
                .contains_key(OsStr::new("GIT_CONFIG_KEY_1"))
        );
        assert!(
            !command
                .removed_environment
                .iter()
                .any(|name| name.as_bytes().starts_with(b"GIT_CONFIG_"))
        );
        for invalid in [
            OsString::from("-1"),
            OsString::from("not-a-count"),
            OsString::from("2147483647"),
            OsString::from("4294967296"),
            OsString::from_vec(vec![0xff]),
        ] {
            assert!(
                service
                    .push_endpoint_command(
                        Path::new("/repo"),
                        &endpoint,
                        &reference,
                        Some(&invalid)
                    )
                    .is_none()
            );
        }
        let default = service
            .push_endpoint_command(Path::new("/repo"), &endpoint, &reference, None)
            .unwrap();
        assert_eq!(
            default.environment[OsStr::new("GIT_CONFIG_COUNT")],
            OsString::from("1")
        );
    }

    #[test]
    fn push_reconciliation_never_logs_resolved_credentials_or_lookup_stderr() {
        if isolated_logging_test(
            "push_reconciliation_never_logs_resolved_credentials_or_lookup_stderr",
        ) {
            return;
        }
        const SECRET: &str = "synthetic-secret-for-reconciliation-test";
        let endpoint = format!("https://alice:{SECRET}@example.invalid/repository.git");
        let sensitive_failure = || {
            let mut failure = launched_failure(b"");
            failure.kind = ProcessFailureKind::TimedOut;
            failure.stderr = format!("lookup refused {endpoint}").into_bytes();
            failure.message = endpoint.clone();
            Err(failure)
        };
        let logs = captured_logs(|| {
            for responses in [
                vec![sensitive_failure()],
                vec![
                    success(format!("{endpoint}\0").as_bytes()),
                    sensitive_failure(),
                ],
                vec![
                    success(format!("{endpoint}\0").as_bytes()),
                    success(b""),
                    sensitive_failure(),
                ],
                vec![
                    success(format!("{endpoint}\0").as_bytes()),
                    success(b""),
                    success(format!("{endpoint}\n").as_bytes()),
                    sensitive_failure(),
                ],
                vec![
                    success(format!("{endpoint}\0").as_bytes()),
                    success(b""),
                    success(format!("{endpoint}\n").as_bytes()),
                    Ok(ProcessOutput {
                        stdout_truncated: false,
                        exit_code: Some(128),
                        signal: None,
                        success: false,
                        stdout: Vec::new(),
                        stderr: endpoint.as_bytes().to_vec(),
                    }),
                ],
            ] {
                let service = fake_service(responses);
                assert!(!service.push_destinations_match(
                    Path::new("/repo"),
                    "origin",
                    &[OsString::from(&endpoint)],
                    &RemoteSideRef::parse("refs/heads/main").unwrap(),
                    &oid(4),
                    &CancellationToken::new()
                ));
            }
        });
        assert!(
            !logs.contains(SECRET),
            "resolved credentials must not appear in diagnostics"
        );
        assert!(
            logs.is_empty(),
            "lookup failures remain unknown without exposing their argv or stderr"
        );
    }

    #[test]
    fn a_push_of_a_missing_local_branch_is_not_pushed() {
        let service = fake_service(vec![success(b""), exit(1, b"")]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();

        assert!(matches!(
            outcome,
            PushOutcome::NotPushed {
                failure: MutationFailure::Rejected { .. }
            }
        ));
    }

    #[test]
    fn branch_publication_remote_honors_explicit_targets_and_never_guesses_between_forks() {
        let branch = ValidatedBranchName::parse("master").unwrap();
        let remote_config = b"remote.origin.url\nhttps://github.com/local/repo.git\0remote.upstream.url\nhttps://github.com/upstream/repo.git\0";
        for (push, default, tracking, expected) in [
            (
                Some("origin"),
                Some("upstream"),
                Some("upstream"),
                Some("origin"),
            ),
            (None, Some("origin"), Some("upstream"), Some("origin")),
            (None, None, Some("upstream"), Some("upstream")),
            (None, None, None, Some("origin")),
            (Some("missing"), None, Some("origin"), None),
            (None, None, Some("."), None),
        ] {
            let mut config = remote_config.to_vec();
            for (key, value) in [
                ("branch.master.pushremote", push),
                ("remote.pushdefault", default),
                ("branch.master.remote", tracking),
            ] {
                if let Some(value) = value {
                    config.extend_from_slice(format!("{key}\n{value}\0").as_bytes());
                }
            }
            let service = fake_service(vec![success(&config)]);
            let remote = service
                .branch_remote(Path::new("/repo"), &branch, &CancellationToken::new())
                .unwrap();
            assert_eq!(remote.as_ref().map(|remote| remote.name.as_str()), expected);
        }
        let service = fake_service(vec![success(b"remote.one.url\nhttps://github.com/one/repo.git\0remote.two.url\nhttps://github.com/two/repo.git\0")]);
        assert!(
            service
                .branch_remote(Path::new("/repo"), &branch, &CancellationToken::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_upstream_push_target_comes_from_branch_configuration() {
        let service = fake_service(vec![success(b"origin\0"), success(b"refs/heads/main\0")]);
        let target = service
            .upstream_push_target(
                Path::new("/repo"),
                &ValidatedBranchName::parse("feature").unwrap(),
                &CancellationToken::new(),
            )
            .unwrap();

        assert_eq!(
            target,
            Some(PushTarget::Upstream {
                remote: "origin".into(),
                remote_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
            })
        );

        let service = fake_service(vec![exit(1, b""), exit(1, b"")]);
        assert_eq!(
            service
                .upstream_push_target(
                    Path::new("/repo"),
                    &ValidatedBranchName::parse("feature").unwrap(),
                    &CancellationToken::new(),
                )
                .unwrap(),
            None
        );
    }

    #[test]
    fn an_unmerged_path_reports_its_index_stages_rather_than_an_added_file() {
        let worktree = tempfile::tempdir().unwrap();
        fs::write(worktree.path().join("conflict.txt"), b"merged by hand\n").unwrap();
        let stages = format!(
            "100644 {} 1\tconflict.txt\0100644 {} 2\tconflict.txt\0100644 {} 3\tconflict.txt\0",
            oid(1),
            oid(2),
            oid(3)
        );
        let service = fake_service(vec![
            success(stages.as_bytes()),
            success(format!("{} blob 5\nbase\n\n", oid(1)).as_bytes()),
            success(format!("{} blob 5\nours\n\n", oid(2)).as_bytes()),
            success(format!("{} blob 7\ntheirs\n\n", oid(3)).as_bytes()),
        ]);
        let content = service
            .diff(
                &DiffRequest {
                    worktree: worktree.path().to_path_buf(),
                    basis: DiffBasis::IndexToWorktree,
                    path: path("conflict.txt"),
                    old_path: None,
                },
                &CancellationToken::new(),
            )
            .unwrap();

        match content {
            DiffContent::Unmerged {
                base,
                ours,
                theirs,
                worktree: working,
            } => {
                assert_eq!(base.text(), Some("base\n"));
                assert_eq!(ours.text(), Some("ours\n"));
                assert_eq!(theirs.text(), Some("theirs\n"));
                assert_eq!(working.text(), Some("merged by hand\n"));
            }
            other => panic!("expected an unmerged comparison, got {other:?}"),
        }
    }

    #[test]
    fn an_untracked_file_compares_against_an_absent_index_side() {
        let worktree = tempfile::tempdir().unwrap();
        fs::write(worktree.path().join("new.txt"), b"fresh\n").unwrap();
        let service = fake_service(vec![success(b"")]);
        let content = service
            .diff(
                &DiffRequest {
                    worktree: worktree.path().to_path_buf(),
                    basis: DiffBasis::IndexToWorktree,
                    path: path("new.txt"),
                    old_path: None,
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert_eq!(
            content,
            DiffContent::Comparison {
                old: DiffSide::Absent,
                new: DiffSide::Text {
                    text: "fresh\n".into(),
                    bytes: 6
                },
                old_label: "Index".into(),
                new_label: "Working tree".into(),
                source_head: None,
                modes: [None, Some(0o100644)],
            }
        );
    }

    #[test]
    fn binary_and_submodule_sides_are_reported_as_metadata() {
        let worktree = tempfile::tempdir().unwrap();
        let service = fake_service(vec![success(
            format!("160000 commit {}\tvendor/dep\0", oid(7)).as_bytes(),
        )]);
        let content = service
            .diff(
                &DiffRequest {
                    worktree: worktree.path().to_path_buf(),
                    basis: DiffBasis::CommitToParent {
                        oid: oid(8),
                        parent: None,
                    },
                    path: path("vendor/dep"),
                    old_path: None,
                },
                &CancellationToken::new(),
            )
            .unwrap();

        match content {
            DiffContent::Comparison {
                old,
                new,
                old_label,
                ..
            } => {
                // A root commit has no parent side at all.
                assert_eq!(old, DiffSide::Absent);
                assert_eq!(old_label, "root commit");
                // The gitlink's object is never read as a blob.
                assert_eq!(new, DiffSide::Submodule { oid: Some(oid(7)) });
            }
            other => panic!("expected a comparison, got {other:?}"),
        }
        assert_eq!(service.executor.commands.lock().unwrap().len(), 1);

        assert_eq!(
            classify_bytes(b"text\n".to_vec()),
            DiffSide::Text {
                text: "text\n".into(),
                bytes: 5
            }
        );
        assert_eq!(
            classify_bytes(b"bin\0ary".to_vec()),
            DiffSide::Binary { bytes: 7 }
        );
        assert_eq!(
            classify_bytes(b"invalid \xff utf8".to_vec()),
            DiffSide::InvalidUtf8 { bytes: 14 }
        );
    }

    #[test]
    fn a_head_to_index_diff_uses_the_rename_source_for_the_old_side() {
        let worktree = tempfile::tempdir().unwrap();
        let service = fake_service(vec![
            success(format!("{}\n", oid(9)).as_bytes()),
            success(format!("100755 blob {}\told.txt\0", oid(1)).as_bytes()),
            success(format!("{} blob 4\nold\n\n", oid(1)).as_bytes()),
            success(format!("100644 {} 0\tnew.txt\0", oid(2)).as_bytes()),
            success(format!("{} blob 4\nnew\n\n", oid(2)).as_bytes()),
        ]);
        let content = service
            .diff(
                &DiffRequest {
                    worktree: worktree.path().to_path_buf(),
                    basis: DiffBasis::HeadToIndex,
                    path: path("new.txt"),
                    old_path: Some(path("old.txt")),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        let commands = service.executor.commands.lock().unwrap();
        assert_eq!(
            commands[1].arguments,
            vec![
                OsString::from("--literal-pathspecs"),
                OsString::from("-C"),
                OsString::from(worktree.path()),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("--no-optional-locks"),
                OsString::from("-c"),
                OsString::from("diff.autoRefreshIndex=false"),
                OsString::from("ls-tree"),
                OsString::from("-z"),
                OsString::from(oid(9).as_str()),
                OsString::from("--"),
                OsString::from("old.txt"),
            ]
        );
        match content {
            DiffContent::Comparison {
                old_label,
                new_label,
                source_head,
                modes,
                ..
            } => {
                assert_eq!(old_label, "HEAD");
                assert_eq!(new_label, "Index");
                assert_eq!(source_head, Some(GitHeadGuard::Commit(oid(9))));
                assert_eq!(modes, [Some(0o100755), Some(0o100644)]);
            }
            other => panic!("expected a comparison, got {other:?}"),
        }
    }

    #[test]
    fn unborn_diff_metadata_keeps_an_explicit_head_guard() {
        let mut unborn = success(b"").unwrap();
        unborn.success = false;
        unborn.exit_code = Some(1);
        let service = fake_service(vec![
            Ok(unborn),
            success(format!("100755 {} 0\tnew.sh\0", oid(2)).as_bytes()),
            success(format!("{} blob 4\nnew\n\n", oid(2)).as_bytes()),
        ]);
        let DiffContent::Comparison { source_head, modes, old, .. } = service.diff(
            &DiffRequest { worktree: PathBuf::from("/repo"), basis: DiffBasis::HeadToIndex,
                path: path("new.sh"), old_path: None }, &CancellationToken::new()).unwrap()
        else { panic!("expected a text comparison"); };
        assert_eq!(source_head, Some(GitHeadGuard::Unborn));
        assert_eq!(modes, [None, Some(0o100755)]);
        assert_eq!(old, DiffSide::Absent);
        assert_eq!(service.executor.commands.lock().unwrap().len(), 3);
    }

    #[test]
    fn worktree_diff_mode_comes_from_the_opened_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("new.sh");
        fs::write(&target, "script\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let service = fake_service(vec![success(b"")]);
        let DiffContent::Comparison { modes, source_head, new, .. } = service.diff(
            &DiffRequest { worktree: root.path().into(), basis: DiffBasis::IndexToWorktree,
                path: path("new.sh"), old_path: None }, &CancellationToken::new()).unwrap()
        else { panic!("expected a text comparison"); };
        assert_eq!(modes, [None, Some(0o100755)]);
        assert_eq!(source_head, None);
        assert_eq!(new.text(), Some("script\n"));
    }

    #[test]
    fn an_oversize_object_reports_its_size_without_being_read() {
        let service = fake_service(vec![Err(ProcessFailure {
            kind: ProcessFailureKind::OutputLimit,
            program: "git".into(),
            exit_code: None,
            signal: None,
            stdout: format!("{} blob 99999999\npartial", oid(1)).into_bytes(),
            stderr: Vec::new(),
            message: "stdout exceeded its limit".into(),
            started: true,
        })]);
        let side = service
            .object_side(
                Path::new("/repo"),
                0o100_644,
                &oid(1),
                &CancellationToken::new(),
            )
            .unwrap();

        assert_eq!(side, DiffSide::Oversize { bytes: 99_999_999 });
    }

    #[test]
    fn batch_blobs_survive_newlines_and_nul_bytes_in_content() {
        let payload = format!("{} blob 6\na\0b\nc\n\n{} blob 0\n\n", oid(1), oid(2));
        let blobs = parse_batch_blobs(payload.as_bytes(), "git cat-file --batch").unwrap();
        assert_eq!(
            blobs,
            vec![
                BatchObject::Content(b"a\0b\nc\n".to_vec()),
                BatchObject::Content(Vec::new())
            ]
        );
        assert!(matches!(
            parse_batch_blobs(b"deadbeef blob 10\nshort", "git cat-file --batch"),
            Err(GitParseError::Truncated { .. })
        ));
        assert_eq!(batch_header_size(b"abc blob 42\nxx"), Some(42));
    }

    /// A blobless or partial clone whose promisor remote is unreachable
    /// answers `<oid> missing`, which is Git telling the truth rather than
    /// Git malfunctioning. It used to surface as a parse error over the whole
    /// read.
    #[test]
    fn a_missing_object_is_reported_as_unavailable_not_as_malformed_output() {
        let payload = format!("{} missing\n{} blob 2\nhi\n", oid(1), oid(2));
        let objects = parse_batch_blobs(payload.as_bytes(), "git cat-file --batch").unwrap();
        assert_eq!(
            objects,
            vec![BatchObject::Missing, BatchObject::Content(b"hi".to_vec())]
        );

        // A path whose blob Git cannot produce is unreadable, not empty: an
        // empty side would render as a whole-file deletion.
        let service = fake_service(vec![success(format!("{} missing\n", oid(1)).as_bytes())]);
        let side = service
            .object_side(
                Path::new("/repo"),
                0o100_644,
                &oid(1),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(side, DiffSide::Unreadable { .. }));
    }

    #[test]
    fn mutation_locks_serialize_one_common_directory_and_release_it() {
        let locks = MutationLocks::new();
        let first = locks.lock(Path::new("/repo/.git"));
        // A different repository never waits on this one.
        let other = locks.lock(Path::new("/other/.git"));
        assert_eq!(locks.tracked_keys(), 2);
        drop(other);

        let contender = locks.clone();
        let entered = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&entered);
        let waiter = thread::spawn(move || {
            let guard = contender.lock(Path::new("/repo/.git"));
            flag.store(true, Ordering::Release);
            drop(guard);
        });
        thread::sleep(Duration::from_millis(50));
        assert!(
            !entered.load(Ordering::Acquire),
            "a second mutation on one common directory must wait"
        );
        drop(first);
        waiter.join().unwrap();
        assert!(entered.load(Ordering::Acquire));
        // Released keys are not retained for the life of the session.
        assert_eq!(locks.tracked_keys(), 0);
    }

    #[test]
    fn compact_line_counts_only_display_measured_nonzero_sides() {
        assert_eq!(line_change_counts(Some(881), Some(0)), "+881");
        assert_eq!(line_change_counts(Some(0), Some(3)), "−3");
        assert_eq!(line_change_counts(Some(12), Some(4)), "+12 −4");
        assert_eq!(line_change_counts(Some(0), Some(0)), "");
        assert_eq!(line_change_counts(None, None), "");
        assert_eq!(line_change_counts(Some(7), None), "+7");
    }

    #[test]
    fn snapshot_collects_status_counts_files_remotes_and_refs() {
        let worktree = tempfile::tempdir().unwrap();
        let mut status = status_headers();
        status.extend_from_slice(
            b"1 M. N... 100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 src/main.rs\0",
        );
        let untracked: &[(&str, &[u8], Option<u64>)] = &[
            ("empty.txt", b"", Some(0)),
            ("lines.txt", b"first\nsecond\n", Some(2)),
            ("unterminated.txt", b"first\nsecond", Some(2)),
            ("binary.dat", b"first\0second\n", None),
            ("invalid.txt", b"\xff\n", None),
        ];
        for (name, content, _) in untracked {
            fs::write(worktree.path().join(name), content).unwrap();
            status.extend_from_slice(format!("? {name}\0").as_bytes());
        }
        let mut refs = Vec::new();
        refs.extend_from_slice(b"refs/heads/main\0");
        refs.extend_from_slice(oid(1).as_str().as_bytes());
        refs.extend_from_slice(b"\0\0\0*\n");
        let service = fake_service(vec![
            exit(1, b""), // no configured fsmonitor
            success(&status),
            success(b"4\t1\tsrc/main.rs\0"),
            success(b""),
            success(b"H src/main.rs\0H Cargo.toml\0"),
            // No remotes configured.
            exit(1, b""),
            success(&refs),
            success(b""),
        ]);
        let snapshot = service
            .snapshot(worktree.path(), &CancellationToken::new())
            .unwrap();

        assert_eq!(snapshot.status.entries[0].added_lines, Some(4));
        assert_eq!(snapshot.status.entries[0].removed_lines, Some(1));
        for (name, _, added) in untracked {
            let entry = snapshot
                .status
                .entries
                .iter()
                .find(|entry| entry.path == path(name))
                .unwrap();
            assert_eq!(entry.added_lines, *added, "{name}");
            assert_eq!(entry.removed_lines, added.map(|_| 0), "{name}");
        }
        assert_eq!(
            snapshot.files.paths,
            vec![path("Cargo.toml"), path("src/main.rs")]
        );
        assert_eq!(snapshot.files.tracked_count, 2);
        assert!(snapshot.remotes.is_empty());
        assert_eq!(snapshot.local_refs.len(), 1);
        let commands = service.executor.commands.lock().unwrap();
        assert!(commands[2].arguments.contains(&OsString::from("--cached")));
        assert!(!commands[3].arguments.contains(&OsString::from("--cached")));
        assert_eq!(commands[4].arguments.last(), Some(&OsString::from("-z")));
    }

    #[test]
    fn untracked_counts_refuse_unsafe_nonregular_and_oversize_paths() {
        use std::os::unix::fs::symlink;

        let worktree = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"secret\n").unwrap();
        symlink(outside.path(), worktree.path().join("link")).unwrap();
        symlink(
            outside.path().join("secret.txt"),
            worktree.path().join("direct"),
        )
        .unwrap();
        fs::create_dir(worktree.path().join("nested-repository")).unwrap();
        fs::File::create(worktree.path().join("large"))
            .unwrap()
            .set_len(MAX_DIFF_SIDE_BYTES + 1)
            .unwrap();
        let mut status = parse_status(
            b"# branch.oid (initial)\0# branch.head main\0? link/secret.txt\0? direct\0? nested-repository/\0? large\0? vanished\0",
        )
        .unwrap();
        measure_untracked(
            worktree.path(),
            &mut status.entries,
            &CancellationToken::new(),
        );
        for entry in status.entries {
            assert_eq!(entry.added_lines, None, "{}", entry.path);
            assert_eq!(entry.removed_lines, None, "{}", entry.path);
        }
    }

    #[test]
    fn untracked_counts_obey_read_budget_and_cancellation() {
        let worktree = tempfile::tempdir().unwrap();
        fs::write(worktree.path().join("text"), b"one\ntwo").unwrap();
        let root = fs_safety::Root::open(worktree.path()).unwrap();
        let cancellation = CancellationToken::new();
        let mut bytes = Vec::new();
        let mut remaining = 4;
        assert_eq!(
            untracked_line_count(
                &root,
                &path("text"),
                &cancellation,
                &mut remaining,
                &mut bytes
            ),
            None
        );
        remaining = 16;
        assert_eq!(
            untracked_line_count(
                &root,
                &path("text"),
                &cancellation,
                &mut remaining,
                &mut bytes
            ),
            Some(2)
        );
        assert_eq!(remaining, 9);
        cancellation.cancel();
        assert_eq!(
            untracked_line_count(
                &root,
                &path("text"),
                &cancellation,
                &mut remaining,
                &mut bytes
            ),
            None
        );
        assert_eq!(remaining, 9);
    }

    /// `git ls-files -o` lists *others*, i.e. untracked files. The combined
    /// listing is what the file tree wants — it shows untracked files — but
    /// counting all of it as tracked claimed one extra tracked file for every
    /// untracked one, which is how "6 files tracked" appeared beside a
    /// `git ls-files` that returned five.
    #[test]
    fn a_snapshot_counts_only_the_files_git_tracks() {
        let mut status = status_headers();
        status.extend_from_slice(b"? new.txt\0");
        let service = fake_service(vec![
            exit(1, b""), // no configured fsmonitor
            success(&status),
            success(b""),
            success(b""),
            success(b"? new.txt\0H Cargo.toml\0H src/main.rs\0"),
            // No remotes configured.
            exit(1, b""),
            success(b""),
            success(b""), // stash list
        ]);
        let snapshot = service
            .snapshot(Path::new("/repo"), &CancellationToken::new())
            .unwrap();

        // The tree still shows the untracked file.
        assert_eq!(
            snapshot.files.paths,
            vec![path("Cargo.toml"), path("new.txt"), path("src/main.rs")]
        );
        assert_eq!(snapshot.files.tracked_count, 2);
        assert_eq!(snapshot.status.dirty.untracked, 1);
    }

    #[test]
    fn working_files_are_classified_by_tag_and_counted_once_per_path() {
        // An unmerged path arrives once per index stage, and `S` is an index
        // entry marked skip-worktree, which is tracked like any other.
        let files =
            parse_working_files(b"M c.txt\0M c.txt\0M c.txt\0? u.txt\0S skipped.txt\0").unwrap();

        assert_eq!(
            files.paths,
            vec![path("c.txt"), path("skipped.txt"), path("u.txt")]
        );
        assert_eq!(files.tracked_count, 2);
    }

    #[test]
    fn a_working_file_record_without_a_status_tag_is_rejected() {
        // Without `-t` the record is the bare path, and reading its first byte
        // as a tag would silently truncate every filename by two characters.
        assert!(parse_working_files(b"untagged.txt\0").is_err());
        assert!(parse_working_files(b"H\0").is_err());
        assert!(parse_working_files(b"H \0").is_err());

        // Only the tag and its separator are stripped; the path stays exact.
        let files = parse_working_files(b"H sp ace.txt\0").unwrap();
        assert_eq!(files.paths, vec![path("sp ace.txt")]);
        assert_eq!(files.tracked_count, 1);
    }

    /// `git status` rewrites `.git/index` whenever it finds the stat cache
    /// stale, and `git diff` against the working tree rewrites it whatever the
    /// optional-lock setting says. The card's freshness poll sees either write
    /// as a change and asks for another state read, which runs both commands
    /// again: an idle window spinning Git processes by the hundred. Both
    /// suppressions therefore belong on every read — and on no mutation, which
    /// is entitled to the index lock and to the refresh.
    #[test]
    fn every_read_declines_the_index_lock_and_the_index_refresh() {
        let service = fake_service(Vec::new());
        let worktree = PathBuf::from("/repo");
        let reads = [
            service.status_command(&worktree),
            service.numstat_command(&worktree, false),
            service.numstat_command(&worktree, true),
            service.working_files_command(&worktree),
            service.remotes_command(&worktree),
            service.refs_command(&worktree),
            service.worktree_list_command(&worktree),
            service.worktree_root_command(&worktree),
            service.history_command(&worktree, &oid(2), 0, None),
        ];
        let mutations = [
            service
                .stage_command(&worktree, &[path("new.txt")])
                .unwrap(),
            service
                .commit_command(&CommitRequest {
                    worktree: worktree.clone(),
                    message: "subject".into(),
                })
                .unwrap(),
            service.push_command(&publish_request()).unwrap(),
        ];

        for command in &reads {
            let lock = command
                .arguments
                .iter()
                .position(|argument| argument == OsStr::new("--no-optional-locks"))
                .unwrap_or_else(|| panic!("{:?} takes the index lock", command.arguments));
            let refresh = command
                .arguments
                .iter()
                .position(|argument| argument == OsStr::new("diff.autoRefreshIndex=false"))
                .unwrap_or_else(|| panic!("{:?} refreshes the index", command.arguments));
            // Git accepts a global option only ahead of the subcommand, so both
            // have to sit in the global option block rather than anywhere in
            // the argv.
            assert_eq!(command.arguments[refresh - 1], OsString::from("-c"));
            assert!(lock < refresh);
            assert!(
                refresh <= 7,
                "the index-refresh suppression left the global option block: {:?}",
                command.arguments
            );
        }
        for command in &mutations {
            assert!(
                !command
                    .arguments
                    .contains(&OsString::from("--no-optional-locks")),
                "{:?} must be allowed the index lock",
                command.arguments
            );
            assert!(
                !command
                    .arguments
                    .contains(&OsString::from("diff.autoRefreshIndex=false")),
                "{:?} must be allowed to refresh the index",
                command.arguments
            );
        }
    }

    /// Subscriber interest is cached across threads. Give logging assertions
    /// their own process so other tests cannot change the active dispatcher set.
    fn isolated_logging_test(name: &str) -> bool {
        const CHILD: &str = "CAGENTS_REPOSITORY_LOGGING_TEST";
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("repository::tests::{name}"),
                "--nocapture",
            ])
            .env(CHILD, name)
            .output()
            .expect("start isolated logging test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "isolated logging test {name} failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
        true
    }

    /// Collects one subscriber's output so a test can read what was logged.
    #[derive(Clone)]
    struct LogSink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Runs `body` with every log line captured, on this thread only.
    fn captured_logs(body: impl FnOnce()) -> String {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || LogSink(writer.clone()))
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        String::from_utf8(sink.lock().unwrap().clone()).expect("UTF-8 log output")
    }

    /// A whole run under `RUST_LOG=cagents=trace` produced no `cagents::` line
    /// at all, so the `git: process exited with status Some(128)` banner named
    /// neither the command that failed nor Git's own reason for failing.
    /// Attributing it needed a shim on `PATH`. Every failure now carries its
    /// argv, its status and Git's message.
    #[test]
    fn a_failing_git_command_is_logged_with_its_argv_status_and_reason() {
        if isolated_logging_test("a_failing_git_command_is_logged_with_its_argv_status_and_reason")
        {
            return;
        }
        let refusal = Ok(ProcessOutput {
            stdout_truncated: false,
            exit_code: Some(128),
            signal: None,
            success: false,
            stdout: Vec::new(),
            stderr: b"fatal: this operation must be run in a work tree\n".to_vec(),
        });
        let logs = captured_logs(|| {
            let service = fake_service(vec![refusal]);
            service
                .discover(Path::new("/repo/.git"), &CancellationToken::new())
                .expect_err("a refused query is a failure");
        });

        assert!(logs.contains("git command failed"), "{logs}");
        assert!(logs.contains("--is-bare-repository"), "{logs}");
        assert!(logs.contains("128"), "{logs}");
        assert!(logs.contains("must be run in a work tree"), "{logs}");
    }

    /// Superseding an in-flight read is how this application refreshes, so a
    /// cancellation is routine rather than a fault. Logging one would bury the
    /// failures worth reading under the noise of ordinary typing.
    #[test]
    fn a_cancelled_git_command_is_not_logged_as_a_failure() {
        if isolated_logging_test("a_cancelled_git_command_is_not_logged_as_a_failure") {
            return;
        }
        let cancelled = ProcessFailure {
            kind: ProcessFailureKind::Cancelled,
            program: "git".into(),
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: b"cancelled".into(),
            message: "cancelled".into(),
            started: true,
        };
        let logs = captured_logs(|| {
            let service = fake_service(vec![Err(cancelled)]);
            service
                .discover(Path::new("/repo"), &CancellationToken::new())
                .expect_err("a cancelled query is an error");
        });

        assert!(logs.is_empty(), "{logs}");
    }
}

/// These states map to the generic journal: Prepared/Running => Pending,
/// Created => Applied, NotCreated => RolledBack, all unresolved recovery
/// states => NeedsReconciliation. Unknown never releases the identity lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeOperationState {
    Prepared,
    Running,
    ReconcileRequired,
    Created,
    NotCreated,
    OutcomeUnknown,
    MalformedPayload,
    JournalWriteFailed,
    LeaseConflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeProgressStage {
    Validating,
    ResolvingBase,
    RunningGit,
    Reconciling,
    Finished,
}

#[derive(Debug, Clone)]
pub struct WorktreeProgressEvent {
    pub operation_id: OperationId,
    pub generation: u64,
    pub stage: WorktreeProgressStage,
}

#[derive(Debug, Clone)]
pub struct WorktreeOperationReport {
    pub operation_id: OperationId,
    pub generation: u64,
    pub request: Option<CreateWorktreeRequest>,
    pub state: WorktreeOperationState,
    pub outcome: Option<WorktreeCreationOutcome>,
    pub detail: String,
    pub created_at_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorktreeJournal {
    version: u32,
    request: CreateWorktreeRequest,
    common_dir: PathBuf,
    generation: u64,
    state: WorktreeOperationState,
    // Held across every filesystem/Git effect and inherited by child processes.
    // Unlike a pid, the kernel lease cannot be confused by pid reuse or reboot.
    lease_path: PathBuf,
    lease_identity: Option<(u64, u64)>,
    effects_started: bool,
    checkout_completed: bool,
    base_oid: Option<String>,
    #[serde(default)]
    existing_branch_oid: Option<String>,
    /// The administrative directory Git keeps for the created worktree,
    /// `<common dir>/worktrees/<name>`. Unlike its path, it stays with the
    /// worktree when the worktree is moved, and no two live worktrees share
    /// one. Absent until the worktree is known to be created, and in
    /// journals written before it was recorded.
    #[serde(default)]
    admin_dir: Option<PathBuf>,
    outcome: Option<WorktreeCreationOutcome>,
    warnings: Vec<ProcessFailure>,
    detail: String,
}

fn operation_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

/// A managed destination lies strictly inside the managed root. A branch with
/// slashes nests its directory under intermediate ones, so the destination
/// need not be the root's direct child, but it can never be the root itself
/// or leave it.
fn destination_within_managed_root(destination: &Path, managed_root: &Path) -> bool {
    destination != managed_root && destination.starts_with(managed_root)
}

fn journal_identity(common: &Path, request: &CreateWorktreeRequest) -> String {
    use std::os::unix::ffi::OsStrExt;
    // Length-delimited raw path bytes avoid aliases from lossy UTF-8 or separators.
    serde_json::to_string(&(
        common.as_os_str().as_bytes(),
        request.branch.as_str(),
        request.destination.as_os_str().as_bytes(),
    ))
    .expect("byte slices and strings serialize")
}

pub use crate::worktree_cleanup::RemovedWorktree;

/// Releases the identities held by the applied creations of `removed`.
///
/// An applied creation keeps its identity so that a replay of the request
/// cannot run while what it made exists. Once the user has deleted the
/// worktree there is nothing left to protect, and holding on would refuse
/// creating the same worktree again for good (#269).
///
/// A creation is known to have made `removed` by the administrative
/// directory it recorded, which follows the worktree wherever it was moved
/// and is never shared by two live worktrees, so a creation whose worktree
/// still exists is never released. A creation recorded before that directory
/// was must have been made at the removed path on the branch the removed
/// worktree had checked out; Git refuses checking a branch out twice, so its
/// worktree cannot be live elsewhere on that branch. A worktree removed
/// outside the app keeps its creation's identity: nothing here saw the user
/// ask for that.
///
/// Every matching creation is attempted; the failures are returned.
pub fn release_deleted_worktree_creations(
    store: &StateStore,
    removed: &RemovedWorktree,
) -> Result<(), Vec<StoreError>> {
    let mut failures = Vec::new();
    let operations = store
        .journaled_worktree_operations()
        .map_err(|error| vec![error])?;
    for operation in operations {
        if operation.status != PendingOperationStatus::Applied {
            continue;
        }
        let Ok(journal) = serde_json::from_value::<WorktreeJournal>(operation.payload) else {
            continue;
        };
        if journal.created(removed)
            && let Err(error) = store.release_worktree_creation(operation.id)
        {
            failures.push(error);
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures)
    }
}

// Configured roots may intentionally be siblings (../worktrees). Resolve
// symlinks before each parent step, then hand the journal an absolute path
// without traversal components. Explicit destinations keep the strict guard.
fn canonical_managed_directory(path: &Path) -> io::Result<PathBuf> {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        if component == std::path::Component::ParentDir {
            resolved = canonical_future_path(&resolved)?;
            resolved.pop();
        } else {
            resolved.push(component.as_os_str());
        }
    }
    canonical_future_path(&resolved)
}

fn canonical_future_path(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must be absolute without parent traversal",
        ));
    }
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(error)?;
            let name = path
                .file_name()
                .ok_or_else(|| io::Error::other("missing path name"))?;
            Ok(canonical_future_path(parent)?.join(name))
        }
        Err(error) => Err(error),
    }
}

fn worktree_lease(path: &Path, create: bool) -> io::Result<fs::File> {
    worktree_lease_with_grace(path, create, Duration::ZERO)
}

fn worktree_lease_with_grace(path: &Path, create: bool, grace: Duration) -> io::Result<fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other("unsafe worktree operation lease file"));
    }
    let deadline = Instant::now() + grace;
    loop {
        // SAFETY: the descriptor belongs to the live file returned to the caller.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock || Instant::now() >= deadline {
            return Err(error);
        }
        // CLOEXEC descriptors remain inherited by unrelated concurrent forks
        // until exec, and a `posix_spawn` suspends only the spawning thread;
        // any other thread's `flock` meets the copy in the meantime. Allow
        // that interval to finish without weakening evidence. The grace is
        // consumed only while the lease is actually contended.
        thread::sleep(
            Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    Ok(file)
}

/// Git lock files are evidence, never debris this service is entitled to remove.
fn git_locks_present(common: &Path) -> io::Result<bool> {
    fn walk(path: &Path, depth: usize) -> io::Result<bool> {
        if depth > 64 {
            return Err(io::Error::other("Git lock inspection depth exceeded"));
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if entry.file_name().as_encoded_bytes().ends_with(b".lock") {
                return Ok(true);
            }
            if kind.is_dir() && walk(&entry.path(), depth + 1)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    for entry in fs::read_dir(common)? {
        let entry = entry?;
        if entry.file_name().as_encoded_bytes().ends_with(b".lock") {
            return Ok(true);
        }
    }
    for subtree in ["refs", "worktrees"] {
        let path = common.join(subtree);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                if walk(&path, 0)? {
                    return Ok(true);
                }
            }
            Ok(_) => return Err(io::Error::other("unsafe Git lock directory")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

impl WorktreeJournal {
    /// Whether this creation made `removed`; see
    /// [`release_deleted_worktree_creations`].
    fn created(&self, removed: &RemovedWorktree) -> bool {
        if self.common_dir != removed.common_dir {
            return false;
        }
        match &self.admin_dir {
            Some(admin_dir) => *admin_dir == removed.admin_dir,
            None => {
                self.request.destination == removed.path
                    && removed.branch.as_deref() == Some(self.request.branch.full_ref().as_str())
            }
        }
    }

    fn report(&self, created_at_unix_ms: i64) -> WorktreeOperationReport {
        let mut outcome = self.outcome.clone();
        // A worktree this operation created must never be reported without
        // its creation time. The record carries the destination's filesystem
        // birth time, which several filesystems - overlay and network mounts
        // among them - do not keep, and a caller that orders worktrees by
        // creation can only put a timeless row last. This journal already
        // holds the instant the operation began, which is the creation
        // instant to within the seconds Git spent on it, so fill the gap from
        // the record this operation owns rather than by re-stating a
        // filesystem that has already answered.
        if let Some(WorktreeCreationOutcome::Created { worktree, .. }) = &mut outcome {
            worktree
                .created_at_unix_ms
                .get_or_insert(created_at_unix_ms);
        }
        WorktreeOperationReport {
            operation_id: self.request.operation_id,
            generation: self.generation,
            request: Some(self.request.clone()),
            state: self.state,
            outcome,
            detail: self.detail.clone(),
            created_at_unix_ms,
        }
    }

    fn persist(
        &self,
        store: &StateStore,
        prior: &serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        let payload = serde_json::to_value(self)?;
        let status = match self.state {
            WorktreeOperationState::Prepared | WorktreeOperationState::Running => {
                PendingOperationStatus::Pending
            }
            WorktreeOperationState::Created => PendingOperationStatus::Applied,
            WorktreeOperationState::NotCreated => PendingOperationStatus::RolledBack,
            _ => PendingOperationStatus::NeedsReconciliation,
        };
        store.update_operation(self.request.operation_id, prior, &payload, status)?;
        Ok(payload)
    }
}

impl<E: ProcessExecutor> RepositoryService<E> {
    /// The canonical common directory of `repository`, the one a creation
    /// journal records and its identity is keyed by.
    pub fn common_dir(
        &self,
        repository: &Path,
        cancellation: &CancellationToken,
    ) -> Result<PathBuf, RepositoryError> {
        let output = self.run_checked(
            &self.git_read(
                repository,
                ["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ),
            cancellation,
        )?;
        Ok(
            fs::canonicalize(parse_path_line(&output.stdout, "common_dir")?)
                .map_err(|e| ValidationError::Io(e.to_string()))?,
        )
    }

    /// The canonical Git directory of `worktree`: for a linked worktree, the
    /// administrative directory Git keeps for it in the common directory.
    pub fn git_dir(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<PathBuf, RepositoryError> {
        let output = self.run_checked(
            &self.git_read(
                worktree,
                ["rev-parse", "--path-format=absolute", "--git-dir"],
            ),
            cancellation,
        )?;
        Ok(
            fs::canonicalize(parse_path_line(&output.stdout, "git_dir")?)
                .map_err(|e| ValidationError::Io(e.to_string()))?,
        )
    }

    /// The production entrypoint. No caller may create the managed root first.
    /// The retained token and generation belong to the UI; this emits at most
    /// five events, so a per-operation bounded(8) channel cannot drop a stage.
    #[cfg(feature = "ui")]
    pub fn create_worktree_journaled(
        &self,
        store: &StateStore,
        request: &CreateWorktreeRequest,
        generation: u64,
        cancellation: &CancellationToken,
        progress: &async_channel::Sender<WorktreeProgressEvent>,
    ) -> WorktreeOperationReport {
        let created_at = operation_timestamp();
        let emit = |stage| {
            let _ = progress.try_send(WorktreeProgressEvent {
                operation_id: request.operation_id,
                generation,
                stage,
            });
        };
        emit(WorktreeProgressStage::Validating);
        let failure_report = |state, detail: String| WorktreeOperationReport {
            operation_id: request.operation_id,
            generation,
            request: Some(request.clone()),
            state,
            outcome: None,
            detail,
            created_at_unix_ms: created_at,
        };
        let mut journal = match self.prepare_worktree_journal(request, generation, cancellation) {
            Ok(journal) => journal,
            Err(error) => {
                if cancellation.is_cancelled() {
                    // Discovery is read-only and begin_operation has not run:
                    // service control flow proves that no mutation was attempted.
                    emit(WorktreeProgressStage::Reconciling);
                    emit(WorktreeProgressStage::Finished);
                    let mut report = failure_report(
                        WorktreeOperationState::NotCreated,
                        "Cancelled during read-only validation; no mutation was accepted".into(),
                    );
                    report.outcome = Some(WorktreeCreationOutcome::NotCreated {
                        failure: repository_warning(&self.git, error),
                    });
                    return report;
                }
                emit(WorktreeProgressStage::Finished);
                return failure_report(WorktreeOperationState::MalformedPayload, error.to_string());
            }
        };
        let mut payload = match serde_json::to_value(&journal) {
            Ok(payload) => payload,
            Err(error) => {
                return failure_report(WorktreeOperationState::MalformedPayload, error.to_string());
            }
        };
        let operation = PendingOperation {
            id: request.operation_id,
            entity_kind: "worktree_creation".into(),
            entity_id: journal_identity(&journal.common_dir, &journal.request),
            action: "create".into(),
            base_generation: generation,
            payload: payload.clone(),
            status: PendingOperationStatus::Pending,
            created_at_unix_ms: created_at,
            updated_at_unix_ms: created_at,
        };
        if let Err(error) = store.begin_operation(&operation) {
            emit(WorktreeProgressStage::Finished);
            let state = if error.to_string().contains("lease is occupied") {
                WorktreeOperationState::LeaseConflict
            } else {
                WorktreeOperationState::JournalWriteFailed
            };
            return failure_report(state, error.to_string());
        }
        // From this commit onward every effect is recoverable through this row.
        let lease = match worktree_lease(&journal.lease_path, true) {
            Ok(lease) => lease,
            Err(error) => {
                journal.state = WorktreeOperationState::OutcomeUnknown;
                journal.detail = format!("Cannot establish process lease: {error}");
                return finish_worktree_journal(store, &journal, &payload, created_at, &emit);
            }
        };
        journal.lease_identity = match lease_file_identity(&lease) {
            Ok(identity) => Some(identity),
            Err(error) => {
                emit(WorktreeProgressStage::Finished);
                return failure_report(
                    WorktreeOperationState::JournalWriteFailed,
                    error.to_string(),
                );
            }
        };
        journal.state = WorktreeOperationState::Running;
        if let Err(error) = journal.persist(store, &payload).map(|next| payload = next) {
            emit(WorktreeProgressStage::Finished);
            return failure_report(
                WorktreeOperationState::JournalWriteFailed,
                error.to_string(),
            );
        }
        emit(WorktreeProgressStage::ResolvingBase);
        let mut journal_write_failed = false;
        let attempt = (|| -> Result<(), RepositoryError> {
            if cancellation.is_cancelled() {
                return Err(process_failure(
                    &CommandSpec::new(&self.git),
                    ProcessFailureKind::Cancelled,
                    "termination requested before Git",
                    false,
                )
                .into());
            }
            // The durable identity lease must win over changed repository state:
            // a replay is still a conflict after an earlier attempt made a branch.
            if journal.existing_branch_oid.is_some()
                && !(matches!(&journal.request.base, BaseRevision::LocalBranch(branch) if branch == &journal.request.branch)
                    || (journal.request.track
                        && matches!(&journal.request.base, BaseRevision::RemoteRef(_))))
            {
                return Err(ValidationError::InvalidBranch(
                    "Branch already exists; resolve it as an existing branch instead of overriding its base".into()
                ).into());
            }
            if let Some(worktree) = self
                .list_worktrees(&journal.request.repository, cancellation)?
                .iter()
                .find(|worktree| {
                    worktree.branch.as_deref() == Some(journal.request.branch.full_ref().as_str())
                })
            {
                return Err(ValidationError::InvalidBranch(format!(
                    "Branch is already checked out at {}; no branch was changed",
                    worktree.path.display()
                ))
                .into());
            }
            // Fetch is a mutation too: journal its intention before updating any
            // remote-tracking ref. Never fetch into a local branch or force it.
            journal.effects_started = true;
            journal
                .persist(store, &payload)
                .map(|next| payload = next)
                .map_err(|error| {
                    journal_write_failed = true;
                    ValidationError::Io(format!("journal write failed: {error}"))
                })?;
            if let BaseRevision::RemoteRef(reference) = &journal.request.base {
                let remotes = self.remotes(&journal.request.repository, cancellation)?;
                let (remote, branch) = remotes
                    .iter()
                    .filter_map(|remote| {
                        reference
                            .strip_prefix(&format!("refs/remotes/{}/", remote.name))
                            .map(|branch| (&remote.name, branch))
                    })
                    .max_by_key(|(remote, _)| remote.len())
                    .ok_or_else(|| {
                        ValidationError::InvalidBase(format!(
                            "No configured remote owns {reference}"
                        ))
                    })?;
                let _remote =
                    self.checked_remote(&journal.request.repository, remote, cancellation)?;
                ValidatedBranchName::parse(branch)?;
                let mut fetch = self.git_mutation(
                    &journal.request.repository,
                    [
                        "fetch",
                        "--no-tags",
                        "--no-recurse-submodules",
                        remote.as_str(),
                        &format!("refs/heads/{branch}:{reference}"),
                    ],
                );
                fetch.inherited_lease = Some(lease.as_raw_fd());
                self.run_checked(&fetch, cancellation)?;
            }
            let resolve = self.git_read(
                &journal.request.repository,
                [
                    OsString::from("rev-parse"),
                    OsString::from("--verify"),
                    OsString::from("--end-of-options"),
                    format!("{}^{{commit}}", journal.request.base.revision_argument()).into(),
                ],
            );
            let output = self.run_checked(&resolve, cancellation)?;
            let mut oid = parse_utf8_line(&output.stdout, "base_oid")?;
            if !is_hex_oid(&oid) {
                return Err(ValidationError::InvalidBase(oid).into());
            }
            if let Some(existing) = &journal.existing_branch_oid {
                let current = self.resolve_optional_revision(
                    &journal.request.repository,
                    &journal.request.branch.full_ref(),
                    cancellation,
                )?;
                if current.as_ref().map(Oid::as_str) != Some(existing.as_str()) {
                    return Err(ValidationError::InvalidBranch(
                        "Branch changed while preparing; submit again".into(),
                    )
                    .into());
                }
                if existing != &oid {
                    let ahead = self.git_read(
                        &journal.request.repository,
                        [
                            "merge-base",
                            "--is-ancestor",
                            oid.as_str(),
                            existing.as_str(),
                        ],
                    );
                    let output = self.run(&ahead, cancellation)?;
                    if output.success {
                        // Locally ahead: keep every local commit.
                        oid.clone_from(existing);
                    } else if output.exit_code == Some(1) {
                        self.run_checked(&self.git_read(&journal.request.repository,
                            ["merge-base", "--is-ancestor", existing.as_str(), oid.as_str()]), cancellation)
                            .map_err(|_| ValidationError::InvalidBranch(
                                "Local and upstream branches diverged; reconcile them before creating a worktree".into()))?;
                    } else {
                        return Err(git_failure(&ahead, output).into());
                    }
                }
            }
            journal.base_oid = Some(oid.clone());
            // Persist intention before the first managed-directory or Git effect.
            journal.effects_started = true;
            journal
                .persist(store, &payload)
                .map(|next| payload = next)
                .map_err(|error| {
                    journal_write_failed = true;
                    ValidationError::Io(format!("journal write failed: {error}"))
                })?;
            if cancellation.is_cancelled() {
                return Err(process_failure(
                    &resolve,
                    ProcessFailureKind::Cancelled,
                    "termination requested before Git",
                    false,
                )
                .into());
            }
            if git_locks_present(&journal.common_dir)
                .map_err(|e| ValidationError::Io(e.to_string()))?
            {
                return Err(ValidationError::Io(
                    "Git lock contention; no mutation launched".into(),
                )
                .into());
            }
            fs::create_dir_all(&journal.request.managed_root)
                .map_err(|e| ValidationError::Io(e.to_string()))?;
            let plan = self.plan_worktree_creation(&journal.request)?;
            self.run_checked(&plan.validate_branch, cancellation)?;
            let branch = self.run(&plan.check_branch_absent, cancellation)?;
            let mut command = if journal.existing_branch_oid.is_some() {
                if !branch.success {
                    return Err(ValidationError::InvalidBranch(
                        "Existing branch disappeared; submit again".into(),
                    )
                    .into());
                }
                self.git_mutation(
                    &journal.request.repository,
                    [
                        OsString::from("worktree"),
                        OsString::from("add"),
                        OsString::from("--lock"),
                        OsString::from("--reason"),
                        format!("cagents:{}", journal.request.operation_id).into(),
                        OsString::from("--"),
                        journal.request.destination.as_os_str().to_owned(),
                        journal.request.branch.as_str().into(),
                    ],
                )
            } else {
                if branch.exit_code != Some(1) {
                    return Err(ValidationError::InvalidBranch(
                        "Branch appeared during creation; submit again".into(),
                    )
                    .into());
                }
                self.create_worktree_command(&journal.request, &oid)?
            };
            command.inherited_lease = Some(lease.as_raw_fd());
            emit(WorktreeProgressStage::RunningGit);
            self.run_checked(&command, cancellation)?;
            if journal
                .existing_branch_oid
                .as_ref()
                .is_some_and(|existing| existing != &oid)
            {
                let mut advance = self.git_mutation(
                    &journal.request.destination,
                    ["merge", "--ff-only", "--no-edit", oid.as_str()],
                );
                advance.inherited_lease = Some(lease.as_raw_fd());
                self.run_checked(&advance, cancellation)?;
            }
            journal.checkout_completed = true;
            journal
                .persist(store, &payload)
                .map(|next| payload = next)
                .map_err(|error| {
                    journal_write_failed = true;
                    ValidationError::Io(format!("journal write failed: {error}"))
                })?;
            if journal.request.track && !cancellation.is_cancelled() {
                let mut upstream = self.set_upstream_command(&journal.request)?;
                upstream.inherited_lease = Some(lease.as_raw_fd());
                self.run_checked(&upstream, cancellation)?;
            }
            // Keep Git's operation-id lock until after durable settlement. It is
            // ownership evidence, not permission to replay or mutate on restart.
            Ok(())
        })();
        journal.detail = match attempt {
            Ok(()) => "Git exited; reconciling durable outcome".into(),
            Err(error) => {
                let detail = error.to_string();
                journal.warnings.push(repository_warning(&self.git, error));
                detail
            }
        };
        if journal_write_failed {
            emit(WorktreeProgressStage::Finished);
            return failure_report(WorktreeOperationState::JournalWriteFailed, journal.detail);
        }
        emit(WorktreeProgressStage::Reconciling);
        journal.state = WorktreeOperationState::ReconcileRequired;
        if let Err(error) = journal.persist(store, &payload).map(|next| payload = next) {
            emit(WorktreeProgressStage::Finished);
            return failure_report(
                WorktreeOperationState::JournalWriteFailed,
                error.to_string(),
            );
        }
        // Closing our reference allows a fresh independent flock to prove that
        // no inherited child/escaped hook reference is still live.
        drop(lease);
        self.settle_worktree_journal(&mut journal, CLEANUP_GRACE);
        if journal.state == WorktreeOperationState::Created {
            // Never discard the ownership marker before success is durable.
            if let Err(error) = journal.persist(store, &payload).map(|next| payload = next) {
                emit(WorktreeProgressStage::Finished);
                return failure_report(
                    WorktreeOperationState::JournalWriteFailed,
                    error.to_string(),
                );
            }
            self.unlock_created_worktree(&mut journal);
        }
        finish_worktree_journal(store, &journal, &payload, created_at, &emit)
    }

    fn prepare_worktree_journal(
        &self,
        request: &CreateWorktreeRequest,
        generation: u64,
        cancellation: &CancellationToken,
    ) -> Result<WorktreeJournal, RepositoryError> {
        ValidatedBranchName::parse(request.branch.as_str())?;
        request.base.validate()?;
        if request.track && matches!(request.base, BaseRevision::ObjectId(_)) {
            return Err(ValidationError::TrackingRequiresBranch.into());
        }
        let common_dir = self.common_dir(&request.repository, cancellation)?;
        let mut request = request.clone();
        request.managed_root = canonical_future_path(&request.managed_root)
            .map_err(|e| ValidationError::Io(e.to_string()))?;
        request.destination = canonical_future_path(&request.destination)
            .map_err(|e| ValidationError::Io(e.to_string()))?;
        if !destination_within_managed_root(&request.destination, &request.managed_root) {
            return Err(ValidationError::DestinationOutsideRoot.into());
        }
        let lease_path =
            common_dir.join(format!("cagents-operation-{}.lease", request.operation_id));
        let existing_branch_oid = self
            .resolve_optional_revision(
                &request.repository,
                &request.branch.full_ref(),
                cancellation,
            )?
            .map(|oid| oid.to_string());
        Ok(WorktreeJournal {
            version: 1,
            request,
            common_dir,
            generation,
            state: WorktreeOperationState::Prepared,
            lease_path,
            lease_identity: None,
            effects_started: false,
            checkout_completed: false,
            base_oid: None,
            existing_branch_oid,
            admin_dir: None,
            outcome: None,
            warnings: Vec::new(),
            detail: "Prepared durable worktree request".into(),
        })
    }

    fn settle_worktree_journal(&self, journal: &mut WorktreeJournal, lease_grace: Duration) {
        journal.outcome = None;
        let evidence = worktree_lease_with_grace(&journal.lease_path, false, lease_grace);
        let _lease = match evidence {
            Ok(lease) => {
                if journal.lease_identity.is_some()
                    && lease_file_identity(&lease).ok() != journal.lease_identity
                {
                    journal.state = WorktreeOperationState::OutcomeUnknown;
                    journal.detail =
                        "Process lease identity changed; a prior child may still be live".into();
                    return;
                }
                Some(lease)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && !journal.effects_started => {
                None
            }
            Err(error) => {
                journal.state = WorktreeOperationState::OutcomeUnknown;
                journal.detail = format!(
                    "A live/orphaned child or unavailable lease prevents settlement: {error}"
                );
                return;
            }
        };
        match git_locks_present(&journal.common_dir) {
            Ok(false) => {}
            result => {
                journal.state = WorktreeOperationState::OutcomeUnknown;
                journal.detail = format!("Git lock evidence prevents settlement: {result:?}");
                return;
            }
        }
        // A `rev-parse` that could not run says nothing about the repository:
        // a spawn refused under load, or a Git that is gone, leaves the
        // outcome unknown until the next settlement, and is reported as that
        // rather than as evidence that the repository moved.
        let current_common = match self.run_checked(
            &self.git_read(
                &journal.request.repository,
                ["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ),
            &CancellationToken::new(),
        ) {
            Ok(output) => parse_path_line(&output.stdout, "common_dir")
                .ok()
                .and_then(|path| fs::canonicalize(path).ok()),
            Err(failure) => {
                journal.state = WorktreeOperationState::OutcomeUnknown;
                journal.detail = format!(
                    "Could not read the repository's common directory; settlement will be \
                     retried: {failure}"
                );
                return;
            }
        };
        if current_common.as_ref() != Some(&journal.common_dir)
            || canonical_future_path(&journal.request.managed_root)
                .ok()
                .as_ref()
                != Some(&journal.request.managed_root)
            || canonical_future_path(&journal.request.destination)
                .ok()
                .as_ref()
                != Some(&journal.request.destination)
        {
            journal.state = WorktreeOperationState::OutcomeUnknown;
            journal.detail =
                "Canonical repository or destination identity changed; no mutation performed"
                    .into();
            return;
        }
        let failure = process_failure(
            &CommandSpec::new(&self.git),
            ProcessFailureKind::Io,
            &journal.detail,
            journal.effects_started,
        );
        let residual = match self.reconcile_worktree(&journal.request, &CancellationToken::new()) {
            Ok(residual) => residual,
            Err(error) => {
                journal.state = WorktreeOperationState::OutcomeUnknown;
                journal.detail = format!("Read-only reconciliation failed: {error}");
                return;
            }
        };
        // Canonical paths and the .git backlink must agree, not just the list entry.
        let owned = residual
            .registered_worktree
            .as_ref()
            .is_some_and(|worktree| {
                worktree.branch.as_deref() == Some(journal.request.branch.full_ref().as_str())
                    && journal
                        .base_oid
                        .as_ref()
                        .is_some_and(|oid| worktree.head_oid.as_ref() == Some(oid))
                    && worktree.lock.reason.as_deref()
                        == Some(format!("cagents:{}", journal.request.operation_id).as_str())
            });
        let admin_dir = self.created_admin_dir(journal);
        if residual.branch_exists && owned && admin_dir.is_some() {
            journal.admin_dir = admin_dir;
            journal.state = WorktreeOperationState::Created;
            journal.outcome = Some(WorktreeCreationOutcome::Created {
                worktree: residual.registered_worktree.expect("ownership checked"),
                warnings: journal.warnings.clone(),
            });
            journal.detail = if journal.warnings.is_empty() {
                "Created; branch, registration, destination and process lease reconciled".into()
            } else {
                format!("Created with warning: {}", journal.detail)
            };
        } else if (!residual.branch_exists
            || journal.existing_branch_oid.as_ref().is_some_and(|before| {
                self.resolve_optional_revision(
                    &journal.request.repository,
                    &journal.request.branch.full_ref(),
                    &CancellationToken::new(),
                )
                .ok()
                .flatten()
                .is_some_and(|current| current.as_str() == before)
            }))
            && residual.registered_worktree.is_none()
            && residual.destination == DestinationResidualState::Absent
        {
            journal.state = WorktreeOperationState::NotCreated;
            journal.outcome = Some(WorktreeCreationOutcome::NotCreated { failure });
        } else {
            journal.state = WorktreeOperationState::OutcomeUnknown;
            journal.outcome = Some(WorktreeCreationOutcome::OutcomeUnknown { failure, residual });
            journal.detail = "Partial or unowned state; inspect manually, do not retry".into();
        }
    }

    /// The created worktree's administrative directory, when the destination
    /// is evidently the worktree this journal created.
    fn created_admin_dir(&self, journal: &WorktreeJournal) -> Option<PathBuf> {
        let token = CancellationToken::new();
        let destination = &journal.request.destination;
        if fs::canonicalize(destination).ok().as_ref() != Some(destination) {
            return None;
        }
        let git_dir = self.git_dir(destination, &token).ok()?;
        // A linked worktree must resolve to its own administrative directory,
        // whose backlink names exactly this destination's .git file.
        if git_dir.parent() != Some(journal.common_dir.join("worktrees").as_path())
            || fs::read(git_dir.join("gitdir"))
                .ok()
                .map(|bytes| bytes_to_path(trim_one_line_ending(&bytes)))
                .as_ref()
                != Some(&destination.join(".git"))
            || fs::read(git_dir.join("commondir"))
                .ok()
                .map(|bytes| git_dir.join(bytes_to_path(trim_one_line_ending(&bytes))))
                .and_then(|path| fs::canonicalize(path).ok())
                .as_ref()
                != Some(&journal.common_dir)
        {
            return None;
        }
        if journal.checkout_completed {
            return Some(git_dir);
        }
        // HEAD and backlinks precede checkout in Git. Without a recorded
        // successful exit, require a real index and a clean index/worktree
        // comparison; partial checkout and subsequent edits remain unknown.
        let clean = fs::symlink_metadata(git_dir.join("index"))
            .is_ok_and(|metadata| metadata.is_file())
            && self
                .run_checked(
                    &self.git_read(
                        destination,
                        ["status", "--porcelain=v2", "-z", "--untracked-files=all"],
                    ),
                    &token,
                )
                .is_ok_and(|output| output.stdout.is_empty());
        clean.then_some(git_dir)
    }

    /// Only the original live request may clean up its own Git lock. Startup
    /// reconciliation deliberately never calls this mutating helper.
    fn unlock_created_worktree(&self, journal: &mut WorktreeJournal) {
        let cleanup = (|| -> Result<(), RepositoryError> {
            let lease = worktree_lease_with_grace(&journal.lease_path, false, CLEANUP_GRACE)
                .map_err(|error| ValidationError::Io(error.to_string()))?;
            if lease_file_identity(&lease).ok() != journal.lease_identity {
                return Err(ValidationError::Io(
                    "operation lease identity changed; left worktree locked".into(),
                )
                .into());
            }
            let worktree = self
                .list_worktrees(&journal.request.repository, &CancellationToken::new())?
                .into_iter()
                .find(|worktree| worktree.path == journal.request.destination)
                .ok_or_else(|| {
                    ValidationError::Io("created worktree is no longer registered".into())
                })?;
            if worktree.lock.reason.as_deref()
                != Some(format!("cagents:{}", journal.request.operation_id).as_str())
                || worktree.branch.as_deref() != Some(journal.request.branch.full_ref().as_str())
            {
                return Err(ValidationError::Io(
                    "worktree lock ownership changed; left lock untouched".into(),
                )
                .into());
            }
            let mut unlock = self.git_mutation(
                &journal.request.repository,
                [
                    OsString::from("worktree"),
                    OsString::from("unlock"),
                    journal.request.destination.as_os_str().to_owned(),
                ],
            );
            unlock.inherited_lease = Some(lease.as_raw_fd());
            self.run_checked(&unlock, &CancellationToken::new())?;
            if let Some(WorktreeCreationOutcome::Created { worktree, .. }) = &mut journal.outcome {
                worktree.lock = WorktreeLock::default();
            }
            Ok(())
        })();
        if let Err(error) = cleanup {
            journal.detail = format!("{}; worktree unlock warning: {error}", journal.detail);
            journal.warnings.push(repository_warning(&self.git, error));
        }
        if let Some(WorktreeCreationOutcome::Created { warnings, .. }) = &mut journal.outcome {
            warnings.clone_from(&journal.warnings);
        }
    }

    /// Rehydrates all journaled rows, including durable terminal results. Only
    /// unresolved well-formed rows are inspected; nothing is ever replayed.
    pub fn rehydrate_worktree_operations(
        &self,
        store: &StateStore,
    ) -> Result<Vec<WorktreeOperationReport>, StoreError> {
        let mut reports = Vec::new();
        for operation in store.journaled_worktree_operations()? {
            let parsed = serde_json::from_value::<WorktreeJournal>(operation.payload.clone());
            let mut journal = match parsed {
                Ok(journal)
                    if journal.version == 1
                        && operation.action == "create"
                        && match operation.status {
                            PendingOperationStatus::Applied => {
                                journal.state == WorktreeOperationState::Created
                                    && matches!(
                                        &journal.outcome,
                                        Some(WorktreeCreationOutcome::Created { .. })
                                    )
                            }
                            PendingOperationStatus::RolledBack => {
                                journal.state == WorktreeOperationState::NotCreated
                                    && matches!(
                                        &journal.outcome,
                                        Some(WorktreeCreationOutcome::NotCreated { .. })
                                    )
                            }
                            _ => matches!(
                                journal.state,
                                WorktreeOperationState::Prepared
                                    | WorktreeOperationState::Running
                                    | WorktreeOperationState::ReconcileRequired
                                    | WorktreeOperationState::OutcomeUnknown
                            ),
                        }
                        && journal.request.operation_id == operation.id
                        && journal.generation == operation.base_generation
                        && operation.entity_id
                            == journal_identity(&journal.common_dir, &journal.request)
                        && journal.lease_path
                            == journal
                                .common_dir
                                .join(format!("cagents-operation-{}.lease", operation.id))
                        && destination_within_managed_root(
                            &journal.request.destination,
                            &journal.request.managed_root,
                        )
                        && journal.common_dir.is_absolute()
                        && journal.request.managed_root.is_absolute()
                        && journal.request.destination.is_absolute()
                        && [
                            &journal.common_dir,
                            &journal.request.managed_root,
                            &journal.request.destination,
                        ]
                        .iter()
                        .all(|path| {
                            !path
                                .components()
                                .any(|part| matches!(part, std::path::Component::ParentDir))
                        })
                        // Fetch can start before the checkout target is resolved.
                        // A missing target cannot establish Created ownership.
                        && journal.base_oid.as_ref().is_none_or(|oid| is_hex_oid(oid))
                        && journal
                            .existing_branch_oid
                            .as_ref()
                            .is_none_or(|oid| is_hex_oid(oid))
                        && (!journal.checkout_completed
                            || (journal.effects_started && journal.base_oid.is_some()))
                        && ValidatedBranchName::parse(journal.request.branch.as_str()).is_ok()
                        && journal.request.base.validate().is_ok() =>
                {
                    journal
                }
                _ => {
                    reports.push(WorktreeOperationReport {
                        operation_id: operation.id, generation: operation.base_generation, request: None,
                        state: WorktreeOperationState::MalformedPayload, outcome: None,
                        detail: "Unsupported, malformed or identity-mismatched worktree journal; no mutation performed".into(),
                        created_at_unix_ms: operation.created_at_unix_ms,
                    });
                    continue;
                }
            };
            if matches!(
                operation.status,
                PendingOperationStatus::Pending | PendingOperationStatus::NeedsReconciliation
            ) {
                // Settlement at launch races every process the daemon is
                // starting - restored terminals, status polls - and each of
                // those briefly holds a copy of this lease between fork and
                // exec. Read without grace, that copy was "a live child" and
                // an operation that had in fact settled was handed to the
                // user to reconcile by hand. The grace is the same bound the
                // in-process path accepts, and costs nothing uncontended.
                self.settle_worktree_journal(&mut journal, CLEANUP_GRACE);
                reports.push(finish_worktree_journal(
                    store,
                    &journal,
                    &operation.payload,
                    operation.created_at_unix_ms,
                    &|_| {},
                ));
            } else {
                reports.push(journal.report(operation.created_at_unix_ms));
            }
        }
        Ok(reports)
    }
}

fn finish_worktree_journal(
    store: &StateStore,
    journal: &WorktreeJournal,
    prior: &serde_json::Value,
    created: i64,
    emit: &impl Fn(WorktreeProgressStage),
) -> WorktreeOperationReport {
    let mut report = journal.report(created);
    if let Err(error) = journal.persist(store, prior) {
        report.state = WorktreeOperationState::JournalWriteFailed;
        report.outcome = None;
        report.detail = format!("Journal settlement failed; durable recovery is required: {error}");
    }
    emit(WorktreeProgressStage::Finished);
    report
}

#[cfg(all(test, feature = "ui"))]
mod worktree_journal_tests {
    use super::*;
    use crate::model::BootId;

    struct Fixture {
        directory: tempfile::TempDir,
        store: StateStore,
        request: CreateWorktreeRequest,
        service: RepositoryService,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let repository = directory.path().join("repo");
            fs::create_dir(&repository).unwrap();
            let git = |arguments: &[&str]| {
                let output = Command::new("git")
                    .arg("-C")
                    .arg(&repository)
                    .args(arguments)
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            };
            git(&["init", "-b", "main"]);
            fs::write(repository.join("tracked.txt"), b"committed content\n").unwrap();
            git(&["add", "tracked.txt"]);
            git(&[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "base",
            ]);
            let store = StateStore::open(
                directory.path().join("state.sqlite"),
                BootId::new("journal-test-boot"),
            )
            .unwrap();
            let root = directory.path().join("managed");
            let request = CreateWorktreeRequest {
                repository,
                managed_root: root.clone(),
                destination: root.join("feature"),
                branch: ValidatedBranchName::parse("feature").unwrap(),
                base: BaseRevision::LocalBranch(ValidatedBranchName::parse("main").unwrap()),
                track: false,
                operation_id: OperationId::new(),
            };
            Self {
                directory,
                store,
                request,
                service: RepositoryService::system("git"),
            }
        }

        /// The worktree at `path` as the app describes it before deleting it.
        fn removal(&self, path: &Path) -> RemovedWorktree {
            let cancellation = CancellationToken::new();
            let branch = self
                .service
                .list_worktrees(&self.request.repository, &cancellation)
                .unwrap()
                .into_iter()
                .find(|record| record.path == path)
                .unwrap()
                .branch;
            RemovedWorktree {
                common_dir: self
                    .service
                    .common_dir(&self.request.repository, &cancellation)
                    .unwrap(),
                admin_dir: self.service.git_dir(path, &cancellation).unwrap(),
                path: path.to_path_buf(),
                branch,
            }
        }

        /// The journal of the creation `id`.
        fn journal(&self, id: OperationId) -> WorktreeJournal {
            let operation = self
                .store
                .journaled_worktree_operations()
                .unwrap()
                .into_iter()
                .find(|operation| operation.id == id)
                .unwrap();
            serde_json::from_value(operation.payload).unwrap()
        }

        /// Whether the creation `id` still holds its identity.
        fn holds(&self, id: OperationId) -> bool {
            self.store
                .journaled_worktree_operations()
                .unwrap()
                .iter()
                .any(|operation| operation.id == id)
        }

        fn git(&self, arguments: &[&str]) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.request.repository)
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(arguments)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        }

        fn input(&self, name: &str) -> WorktreeInput {
            WorktreeInput {
                repository: self.request.repository.clone(),
                input: name.into(),
                base: String::new(),
                managed_directory: "worktrees/".into(),
                destination: String::new(),
                operation_id: OperationId::new(),
            }
        }

        fn resolve(&mut self, input: WorktreeInput) {
            self.request = self
                .service
                .resolve_worktree_request(&input, &CancellationToken::new())
                .unwrap();
        }

        fn add_remote(&self, name: &str, remote: &Fixture) {
            self.git(&[
                "remote",
                "add",
                name,
                remote.request.repository.to_str().unwrap(),
            ]);
        }

        fn prepare(&self) -> (WorktreeJournal, serde_json::Value) {
            let journal = self
                .service
                .prepare_worktree_journal(&self.request, 7, &CancellationToken::new())
                .unwrap();
            let payload = serde_json::to_value(&journal).unwrap();
            self.store
                .begin_operation(&PendingOperation {
                    id: self.request.operation_id,
                    entity_kind: "worktree_creation".into(),
                    entity_id: journal_identity(&journal.common_dir, &journal.request),
                    action: "create".into(),
                    base_generation: 7,
                    payload: payload.clone(),
                    status: PendingOperationStatus::Pending,
                    created_at_unix_ms: 1,
                    updated_at_unix_ms: 1,
                })
                .unwrap();
            (journal, payload)
        }

        fn run(&self, cancellation: &CancellationToken) -> WorktreeOperationReport {
            let (sender, _) = async_channel::bounded(8);
            self.service.create_worktree_journaled(
                &self.store,
                &self.request,
                7,
                cancellation,
                &sender,
            )
        }
    }

    /// Deleting a created worktree in the app frees its identity, so the same
    /// worktree can be created again on the branch the deletion kept (#269).
    #[test]
    fn a_deleted_worktree_can_be_created_again_on_its_kept_branch() {
        let mut fixture = Fixture::new();
        fixture.resolve(fixture.input("foo"));
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        let destination = fixture.request.destination.clone();
        let removed = fixture.removal(&destination);
        fixture.git(&["worktree", "remove", destination.to_str().unwrap()]);

        release_deleted_worktree_creations(&fixture.store, &removed).unwrap();
        assert!(
            !fixture.holds(report.operation_id),
            "the applied creation still holds its identity"
        );

        fixture.resolve(fixture.input("foo"));
        assert_eq!(
            fixture.request.base,
            BaseRevision::LocalBranch(ValidatedBranchName::parse("foo").unwrap()),
            "the kept branch is checked out as it is"
        );
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        assert!(destination.join("tracked.txt").is_file());
    }

    /// Only the creation that made the deleted worktree is released: not one
    /// of another repository, nor one whose worktree was moved away from the
    /// deleted path and is still live, while one moved and then deleted is.
    #[test]
    fn a_deletion_releases_only_the_creation_of_the_worktree_it_removed() {
        let mut fixture = Fixture::new();
        fixture.resolve(fixture.input("foo"));
        let doomed = fixture.run(&CancellationToken::new());
        assert_eq!(doomed.state, WorktreeOperationState::Created, "{doomed:?}");
        let destination = fixture.request.destination.clone();
        fixture.resolve(fixture.input("bar"));
        let moved = fixture.run(&CancellationToken::new());
        assert_eq!(moved.state, WorktreeOperationState::Created, "{moved:?}");
        let moved_from = fixture.request.destination.clone();

        let mut other_repository = fixture.removal(&destination);
        other_repository.common_dir = PathBuf::from("/elsewhere/.git");
        release_deleted_worktree_creations(&fixture.store, &other_repository).unwrap();
        assert!(
            fixture.holds(doomed.operation_id),
            "another repository's deletion released this creation"
        );

        // `bar` is moved outside the app, and another worktree takes its old
        // path. Deleting that one leaves the live `bar`'s identity alone.
        let moved_to = moved_from.with_file_name("moved");
        fixture.git(&[
            "worktree",
            "move",
            moved_from.to_str().unwrap(),
            moved_to.to_str().unwrap(),
        ]);
        fixture.git(&[
            "worktree",
            "add",
            "-b",
            "squatter",
            moved_from.to_str().unwrap(),
        ]);
        let squatter = fixture.removal(&moved_from);
        fixture.git(&["worktree", "remove", moved_from.to_str().unwrap()]);
        release_deleted_worktree_creations(&fixture.store, &squatter).unwrap();
        assert!(
            fixture.holds(moved.operation_id),
            "deleting the worktree at its old path released the live creation"
        );

        let removed = fixture.removal(&destination);
        fixture.git(&["worktree", "remove", destination.to_str().unwrap()]);
        release_deleted_worktree_creations(&fixture.store, &removed).unwrap();
        assert!(!fixture.holds(doomed.operation_id));
        assert!(fixture.holds(moved.operation_id));

        let removed = fixture.removal(&moved_to);
        fixture.git(&["worktree", "remove", moved_to.to_str().unwrap()]);
        release_deleted_worktree_creations(&fixture.store, &removed).unwrap();
        assert!(
            !fixture.holds(moved.operation_id),
            "deleting the moved worktree kept its creation's identity"
        );
    }

    /// A creation journaled before administrative directories were recorded
    /// is the removed worktree's only at the same path on the same branch.
    #[test]
    fn a_creation_without_its_admin_dir_is_matched_by_path_and_branch() {
        let mut fixture = Fixture::new();
        fixture.resolve(fixture.input("foo"));
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        let destination = fixture.request.destination.clone();
        let mut journal = fixture.journal(report.operation_id);
        let removed = fixture.removal(&destination);
        assert_eq!(journal.admin_dir.as_ref(), Some(&removed.admin_dir));

        journal.admin_dir = None;
        assert!(journal.created(&removed));
        let mut elsewhere = removed.clone();
        elsewhere.admin_dir = PathBuf::from("/nonexistent");
        elsewhere.branch = Some("refs/heads/squatter".into());
        assert!(
            !journal.created(&elsewhere),
            "another branch at the same path was taken for this creation"
        );
    }

    #[test]
    fn creation_from_project_common_directory_keeps_project_identity() {
        let mut fixture = Fixture::new();
        let root = fixture.request.repository.clone();
        let common = root.join(".git");
        let mut input = fixture.input("feature/from-sidebar");
        input.repository = common.clone();
        fixture.resolve(input);
        assert_eq!(fixture.request.repository, common);
        assert_eq!(fixture.request.managed_root, root.join("worktrees"));
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        assert_eq!(
            fs::read_to_string(fixture.request.destination.join("tracked.txt")).unwrap(),
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
        );
    }

    #[test]
    fn managed_directory_can_create_a_sibling_worktree() {
        let mut fixture = Fixture::new();
        let mut input = fixture.input("feature/sibling");
        input.managed_directory = "../managed trees/".into();
        fixture.resolve(input);
        let expected = fixture
            .request
            .repository
            .parent()
            .unwrap()
            .join("managed trees");
        assert_eq!(fixture.request.managed_root, expected);
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        assert_eq!(
            fs::read_to_string(fixture.request.destination.join("tracked.txt")).unwrap(),
            fs::read_to_string(fixture.request.repository.join("tracked.txt")).unwrap(),
        );
    }

    #[test]
    fn a_new_worktree_uses_the_checked_out_branch_when_nothing_advertises_a_default() {
        let mut fixture = Fixture::new();
        fixture.git(&["switch", "-c", "develop"]);
        fixture.resolve(fixture.input("feature/from-develop"));
        assert!(
            matches!(
                &fixture.request.base,
                BaseRevision::LocalBranch(branch) if branch.as_str() == "develop"
            ),
            "a local branch named main must not win over the checked-out branch: {:?}",
            fixture.request.base
        );

        fixture.git(&["switch", "--detach"]);
        fixture.resolve(fixture.input("feature/from-detached"));
        assert!(
            matches!(&fixture.request.base, BaseRevision::ObjectId(_)),
            "a detached HEAD must not fall back to a guessed branch name: {:?}",
            fixture.request.base
        );
    }

    #[test]
    fn intelligent_creation_uses_live_default_branch_and_keeps_source_changes() {
        let mut fixture = Fixture::new();
        let remote = Fixture::new();
        remote.git(&["commit", "--allow-empty", "-m", "remote default advanced"]);
        fixture.add_remote("origin", &remote);
        fs::write(
            fixture.request.repository.join("tracked.txt"),
            "local dirty content",
        )
        .unwrap();
        fixture.resolve(fixture.input("feature/new"));
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        assert_eq!(
            fixture.git(&["rev-parse", "feature/new"]),
            remote.git(&["rev-parse", "main"])
        );
        assert_eq!(
            fs::read_to_string(fixture.request.repository.join("tracked.txt")).unwrap(),
            "local dirty content"
        );
        // A branch named with a slash lives under the directory its prefix
        // names, so `feature/new` is next to `feature/other` rather than
        // beside every other branch with the separator escaped.
        assert_eq!(
            fixture.request.destination,
            fixture.request.repository.join("worktrees/feature/new")
        );
        assert_eq!(
            fixture.git(&[
                "for-each-ref",
                "--format=%(upstream)",
                "refs/heads/feature/new"
            ]),
            ""
        );
    }

    #[test]
    fn intelligent_creation_fetches_existing_upstream_and_tracks_without_new_history() {
        let mut fixture = Fixture::new();
        let remote = Fixture::new();
        remote.git(&["branch", "topic"]);
        fixture.add_remote("origin", &remote);
        fixture.git(&["fetch", "origin"]);
        fixture.git(&["branch", "--track", "topic", "origin/topic"]);
        remote.git(&["switch", "topic"]);
        remote.git(&["commit", "--allow-empty", "-m", "new upstream work"]);
        fixture.resolve(fixture.input("topic"));
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        assert_eq!(
            fixture.git(&["rev-parse", "topic"]),
            remote.git(&["rev-parse", "topic"])
        );
        assert_eq!(
            fixture.git(&["rev-parse", "topic@{upstream}"]),
            remote.git(&["rev-parse", "topic"])
        );
    }

    #[test]
    fn intelligent_creation_preserves_ahead_branch_and_rejects_divergence() {
        let mut fixture = Fixture::new();
        let remote = Fixture::new();
        remote.git(&["branch", "topic"]);
        fixture.add_remote("origin", &remote);
        fixture.git(&["fetch", "origin"]);
        fixture.git(&["switch", "-c", "topic", "--track", "origin/topic"]);
        fixture.git(&["commit", "--allow-empty", "-m", "local work"]);
        let local = fixture.git(&["rev-parse", "topic"]);
        fixture.git(&["switch", "main"]);
        fixture.resolve(fixture.input("topic"));
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        assert_eq!(fixture.git(&["rev-parse", "topic"]), local);
        fixture.git(&[
            "worktree",
            "remove",
            fixture.request.destination.to_str().unwrap(),
        ]);
        remote.git(&["switch", "topic"]);
        remote.git(&["commit", "--allow-empty", "-m", "different remote work"]);
        fixture.resolve(fixture.input("topic"));
        assert_eq!(
            fixture.run(&CancellationToken::new()).state,
            WorktreeOperationState::LeaseConflict
        );
        // Removing a created worktree outside the app does not release its
        // replay-proof identity; only a deletion the user asked the app for does.
        // A distinct destination permits an independent divergence attempt.
        let mut input = fixture.input("topic");
        input.destination = "worktrees/topic-diverged".into();
        fixture.resolve(input);
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(
            report.state,
            WorktreeOperationState::NotCreated,
            "{report:?}"
        );
        assert!(report.detail.contains("diverged"), "{report:?}");
        assert_eq!(fixture.git(&["rev-parse", "topic"]), local);
        assert!(!fixture.request.destination.exists());
    }

    #[test]
    fn intelligent_creation_rejects_checked_out_dirty_branch_without_reset() {
        let mut fixture = Fixture::new();
        fixture.git(&["branch", "topic"]);
        fixture.resolve(fixture.input("topic"));
        // A checkout after resolution must also be caught by the journaled service.
        fixture.git(&["switch", "topic"]);
        let before = fixture.git(&["rev-parse", "topic"]);
        fs::write(
            fixture.request.repository.join("tracked.txt"),
            "unsaved work",
        )
        .unwrap();
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(
            report.state,
            WorktreeOperationState::NotCreated,
            "{report:?}"
        );
        assert!(report.detail.contains("already checked out"), "{report:?}");
        assert_eq!(fixture.git(&["rev-parse", "topic"]), before);
        assert_eq!(
            fs::read_to_string(fixture.request.repository.join("tracked.txt")).unwrap(),
            "unsaved work"
        );
        assert!(!fixture.request.destination.exists());
    }

    #[test]
    fn intelligent_creation_disambiguates_remotes_before_fetching() {
        let mut fixture = Fixture::new();
        let first = Fixture::new();
        let second = Fixture::new();
        first.git(&["branch", "topic"]);
        second.git(&["branch", "topic"]);
        second.git(&["switch", "topic"]);
        second.git(&["commit", "--allow-empty", "-m", "second remote"]);
        fixture.add_remote("first", &first);
        fixture.add_remote("second", &second);
        let error = fixture
            .service
            .resolve_worktree_request(&fixture.input("topic"), &CancellationToken::new())
            .unwrap_err();
        assert!(error.to_string().contains("first/topic"));
        assert!(error.to_string().contains("second/topic"));
        fixture.resolve(fixture.input("second/topic"));
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        assert_eq!(
            fixture.git(&["rev-parse", "topic"]),
            second.git(&["rev-parse", "topic"])
        );
        assert_eq!(fixture.git(&["config", "branch.topic.remote"]), "second");
    }

    #[test]
    fn intelligent_creation_accepts_linear_and_safe_managed_path_overrides() {
        let mut fixture = Fixture::new();
        let mut input =
            fixture.input("https://linear.app/team/issue/APP-42/fix-worktree-paths?view=all");
        input.managed_directory = fixture
            .directory
            .path()
            .join("absolute managed")
            .to_str()
            .unwrap()
            .into();
        fixture.resolve(input);
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        assert_eq!(
            fixture.git(&["rev-parse", "APP-42-fix-worktree-paths"]),
            fixture.git(&["rev-parse", "main"])
        );
        let mut input = fixture.input("APP-43");
        input.destination = "custom destination/ticket".into();
        input.base = "main".into();
        fixture.resolve(input);
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        assert_eq!(
            fixture.request.destination,
            fixture.request.repository.join("custom destination/ticket")
        );
        assert_eq!(
            fixture.git(&["rev-parse", "APP-43"]),
            fixture.git(&["rev-parse", "main"])
        );
        let mut input = fixture.input("APP-44");
        input.destination = fixture.request.destination.to_str().unwrap().into();
        assert!(matches!(
            fixture
                .service
                .resolve_worktree_request(&input, &CancellationToken::new()),
            Err(RepositoryError::Validation(
                ValidationError::DestinationOccupied
            ))
        ));
        input.destination = "../escape".into();
        assert!(
            fixture
                .service
                .resolve_worktree_request(&input, &CancellationToken::new())
                .is_err()
        );
        assert_ne!(
            branch_destination_component("a/b"),
            branch_destination_component("a%2Fb")
        );
        assert_eq!(
            branch_destination_component("carson/main-5717-fix"),
            Path::new("carson").join("main-5717-fix")
        );
        assert_eq!(
            branch_destination_component("release/v1.2"),
            Path::new("release").join("v1.2")
        );
        assert_eq!(
            branch_destination_component("odd name"),
            PathBuf::from("odd%20name")
        );
        // Git refuses these, and the fallback refuses them again so a
        // destination can never climb out of the managed root.
        assert_eq!(
            branch_destination_component("a//b"),
            PathBuf::from("a%2F%2Fb")
        );
        assert_eq!(
            branch_destination_component("a/../b"),
            PathBuf::from("a%2F%2E%2E%2Fb")
        );
    }

    #[test]
    fn prepared_interruption_rehydrates_not_created_and_retry_needs_fresh_id() {
        let mut fixture = Fixture::new();
        fixture.prepare();
        assert!(!fixture.request.managed_root.exists());
        let reopened = StateStore::open(
            fixture.directory.path().join("state.sqlite"),
            BootId::new("journal-test-boot"),
        )
        .unwrap();
        let reports = fixture
            .service
            .rehydrate_worktree_operations(&reopened)
            .unwrap();
        assert_eq!(reports[0].state, WorktreeOperationState::NotCreated);
        assert!(!fixture.request.managed_root.exists());
        assert_eq!(
            fixture.run(&CancellationToken::new()).state,
            WorktreeOperationState::JournalWriteFailed
        );
        fixture.request.operation_id = OperationId::new();
        let report = fixture.run(&CancellationToken::new());
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
    }

    #[test]
    fn interrupted_base_resolution_requires_quiescence_and_unchanged_branch_identity() {
        for (existing, moved, expected) in [
            (false, false, WorktreeOperationState::NotCreated),
            (true, false, WorktreeOperationState::NotCreated),
            (true, true, WorktreeOperationState::OutcomeUnknown),
        ] {
            let mut fixture = Fixture::new();
            if existing {
                fixture.git(&["branch", "feature"]);
                fixture.request.base = BaseRevision::LocalBranch(fixture.request.branch.clone());
            }
            let (mut journal, payload) = fixture.prepare();
            let lease = worktree_lease(&journal.lease_path, true).unwrap();
            journal.lease_identity = Some(lease_file_identity(&lease).unwrap());
            journal.state = WorktreeOperationState::Running;
            // Fetch intention is durable before base_oid is known.
            journal.effects_started = true;
            journal.persist(&fixture.store, &payload).unwrap();
            if moved {
                fixture.git(&["commit", "--allow-empty", "-m", "concurrent branch work"]);
                fixture.git(&["branch", "-f", "feature", "main"]);
            }
            let branch_tip = fixture.git(&[
                "for-each-ref",
                "--format=%(objectname)",
                "refs/heads/feature",
            ]);
            let report = fixture
                .service
                .rehydrate_worktree_operations(&fixture.store)
                .unwrap()
                .remove(0);
            assert_eq!(
                report.state,
                WorktreeOperationState::OutcomeUnknown,
                "{report:?}"
            );
            drop(lease);
            // Other suite cases deliberately pause unrelated forks before exec.
            // Wait for their CLOEXEC references before checking branch evidence.
            fixture
                .service
                .settle_worktree_journal(&mut journal, Duration::from_secs(5));
            assert_eq!(journal.state, expected, "{journal:?}");
            assert_eq!(
                fixture.git(&[
                    "for-each-ref",
                    "--format=%(objectname)",
                    "refs/heads/feature"
                ]),
                branch_tip
            );
            assert!(!fixture.request.managed_root.exists());
        }
    }

    #[test]
    fn active_duplicate_and_stale_payload_cannot_mutate() {
        let fixture = Fixture::new();
        let (mut journal, payload) = fixture.prepare();
        assert_eq!(
            fixture.run(&CancellationToken::new()).state,
            WorktreeOperationState::LeaseConflict
        );
        assert!(!fixture.request.managed_root.exists());
        journal.state = WorktreeOperationState::Running;
        journal.persist(&fixture.store, &payload).unwrap();
        journal.state = WorktreeOperationState::NotCreated;
        assert!(journal.persist(&fixture.store, &payload).is_err());
        assert_eq!(
            fixture
                .store
                .operations_requiring_reconciliation()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn live_lease_and_orphaned_child_prevent_false_not_created() {
        let fixture = Fixture::new();
        let (mut journal, payload) = fixture.prepare();
        let lease = worktree_lease(&journal.lease_path, true).unwrap();
        journal.lease_identity = Some(lease_file_identity(&lease).unwrap());
        journal.state = WorktreeOperationState::Running;
        journal.effects_started = true;
        journal.base_oid = Some("a".repeat(40));
        journal.persist(&fixture.store, &payload).unwrap();
        let mut command = CommandSpec::new("/bin/sh").args(["-c", "read line"]);
        command.inherited_lease = Some(lease.as_raw_fd());
        let mut child_command = command.to_command();
        child_command.stdin(Stdio::piped());
        let mut child = child_command.spawn().unwrap();
        drop(lease);
        let reports = fixture
            .service
            .rehydrate_worktree_operations(&fixture.store)
            .unwrap();
        assert_eq!(
            reports[0].state,
            WorktreeOperationState::OutcomeUnknown,
            "{}",
            reports[0].detail
        );
        assert!(
            reports[0]
                .detail
                .starts_with("A live/orphaned child or unavailable lease prevents settlement"),
            "the child's inherited lease is what stops settlement, not {:?}",
            reports[0].detail
        );
        child.stdin.take().unwrap().write_all(b"exit\n").unwrap();
        assert!(child.wait().unwrap().success());
        let reports = fixture
            .service
            .rehydrate_worktree_operations(&fixture.store)
            .unwrap();
        // Every branch that leaves the outcome unknown writes its own detail,
        // so a failure here names which one fired rather than only the state.
        assert_eq!(
            reports[0].state,
            WorktreeOperationState::NotCreated,
            "{}",
            reports[0].detail
        );
    }

    #[test]
    fn git_lock_contention_is_unknown_and_never_deleted_or_replayed() {
        let fixture = Fixture::new();
        let (journal, _) = fixture.prepare();
        let lock = journal.common_dir.join("index.lock");
        fs::write(&lock, b"external writer").unwrap();
        let report = fixture
            .service
            .rehydrate_worktree_operations(&fixture.store)
            .unwrap()
            .remove(0);
        assert_eq!(report.state, WorktreeOperationState::OutcomeUnknown);
        assert_eq!(fs::read(lock).unwrap(), b"external writer");
        assert!(!fixture.request.managed_root.exists());
    }

    #[test]
    fn git_success_before_resolution_is_reconciled_after_restart() {
        let fixture = Fixture::new();
        let (mut journal, payload) = fixture.prepare();
        let lease = worktree_lease(&journal.lease_path, true).unwrap();
        journal.lease_identity = Some(lease_file_identity(&lease).unwrap());
        let output = Command::new("git")
            .arg("-C")
            .arg(&fixture.request.repository)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let oid = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        journal.state = WorktreeOperationState::Running;
        journal.effects_started = true;
        journal.base_oid = Some(oid.clone());
        journal.persist(&fixture.store, &payload).unwrap();
        fs::create_dir(&fixture.request.managed_root).unwrap();
        let mut command = fixture
            .service
            .create_worktree_command(&fixture.request, &oid)
            .unwrap();
        command.inherited_lease = Some(lease.as_raw_fd());
        assert!(
            SystemProcessExecutor
                .execute(&command, &CancellationToken::new())
                .unwrap()
                .success
        );
        drop(lease);
        let reopened = StateStore::open(
            fixture.directory.path().join("state.sqlite"),
            BootId::new("journal-test-boot"),
        )
        .unwrap();
        let report = fixture
            .service
            .rehydrate_worktree_operations(&reopened)
            .unwrap()
            .remove(0);
        assert_eq!(report.state, WorktreeOperationState::Created);
        assert_eq!(
            fixture.run(&CancellationToken::new()).state,
            WorktreeOperationState::LeaseConflict
        );
        assert_eq!(
            fixture
                .service
                .rehydrate_worktree_operations(&reopened)
                .unwrap()[0]
                .state,
            WorktreeOperationState::Created
        );
    }

    #[test]
    fn malformed_or_legacy_journal_payload_is_inspectable_without_effects() {
        let fixture = Fixture::new();
        let (_, payload) = fixture.prepare();
        fixture
            .store
            .update_operation(
                fixture.request.operation_id,
                &payload,
                &serde_json::json!({"version": 0}),
                PendingOperationStatus::NeedsReconciliation,
            )
            .unwrap();
        let report = fixture
            .service
            .rehydrate_worktree_operations(&fixture.store)
            .unwrap()
            .remove(0);
        assert_eq!(report.state, WorktreeOperationState::MalformedPayload);
        assert!(report.request.is_none());
        assert!(!fixture.request.managed_root.exists());
    }

    #[test]
    fn failed_begin_does_not_create_managed_root() {
        let fixture = Fixture::new();
        fixture.store.shutdown().unwrap();
        assert_eq!(
            fixture.run(&CancellationToken::new()).state,
            WorktreeOperationState::JournalWriteFailed
        );
        assert!(!fixture.request.managed_root.exists());
    }

    #[test]
    fn cancelled_before_git_is_reconciled_and_cancellation_is_pushed_once() {
        let fixture = Fixture::new();
        let cancellation = CancellationToken::new();
        let (sender, receiver) = mpsc::channel();
        let _waker = cancellation.register(sender);
        cancellation.cancel();
        cancellation.cancel();
        assert!(matches!(receiver.try_recv(), Ok(WaitEvent::Cancelled)));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let (progress, stages) = async_channel::bounded(8);
        let report = fixture.service.create_worktree_journaled(
            &fixture.store,
            &fixture.request,
            7,
            &cancellation,
            &progress,
        );
        assert_eq!(report.state, WorktreeOperationState::NotCreated);
        let events: Vec<_> = std::iter::from_fn(|| stages.try_recv().ok()).collect();
        assert!(
            events
                .iter()
                .all(|event| event.operation_id == report.operation_id && event.generation == 7)
        );
        assert!(
            !events
                .iter()
                .any(|event| event.stage == WorktreeProgressStage::RunningGit)
        );
        assert!(
            events
                .iter()
                .any(|event| event.stage == WorktreeProgressStage::Reconciling)
        );
        assert_eq!(
            events.last().unwrap().stage,
            WorktreeProgressStage::Finished
        );
        assert!(!fixture.request.managed_root.exists());
    }

    struct CancelAtGit {
        after_exit: bool,
    }

    impl ProcessExecutor for CancelAtGit {
        fn execute(
            &self,
            command: &CommandSpec,
            token: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            let is_creation = command
                .arguments
                .windows(2)
                .any(|pair| pair[0] == "worktree" && pair[1] == "add");
            if is_creation && !self.after_exit {
                token.cancel();
            }
            let result = SystemProcessExecutor.execute(command, token);
            if is_creation && self.after_exit {
                token.cancel();
            }
            result
        }
    }

    #[test]
    fn cancel_at_git_and_after_exit_both_reconcile_instead_of_promising_rollback() {
        for (after_exit, expected) in [
            (false, WorktreeOperationState::NotCreated),
            (true, WorktreeOperationState::Created),
        ] {
            let fixture = Fixture::new();
            let service = RepositoryService::new("git", Arc::new(CancelAtGit { after_exit }));
            let (sender, _) = async_channel::bounded(8);
            let report = service.create_worktree_journaled(
                &fixture.store,
                &fixture.request,
                7,
                &CancellationToken::new(),
                &sender,
            );
            assert_eq!(report.state, expected);
            assert_eq!(
                service
                    .rehydrate_worktree_operations(&fixture.store)
                    .unwrap()[0]
                    .state,
                expected
            );
        }
    }
    #[test]
    fn a_partial_branch_is_never_a_retryable_not_created() {
        let fixture = Fixture::new();
        fixture.prepare();
        let output = Command::new("git")
            .arg("-C")
            .arg(&fixture.request.repository)
            .args(["branch", "feature"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let report = fixture
            .service
            .rehydrate_worktree_operations(&fixture.store)
            .unwrap()
            .remove(0);
        assert_eq!(report.state, WorktreeOperationState::OutcomeUnknown);
        assert_eq!(
            fixture.run(&CancellationToken::new()).state,
            WorktreeOperationState::LeaseConflict
        );
    }

    #[test]
    fn concurrent_store_connections_only_accept_one_identity_lease() {
        let fixture = Fixture::new();
        let journal = fixture
            .service
            .prepare_worktree_journal(&fixture.request, 7, &CancellationToken::new())
            .unwrap();
        let second = StateStore::open(
            fixture.directory.path().join("state.sqlite"),
            BootId::new("journal-test-boot"),
        )
        .unwrap();
        let barrier = std::sync::Barrier::new(2);
        let submit = |store: &StateStore, mut journal: WorktreeJournal| {
            journal.request.operation_id = OperationId::new();
            journal.lease_path = journal.common_dir.join(format!(
                "cagents-operation-{}.lease",
                journal.request.operation_id
            ));
            let operation = PendingOperation {
                id: journal.request.operation_id,
                entity_kind: "worktree_creation".into(),
                entity_id: journal_identity(&journal.common_dir, &journal.request),
                action: "create".into(),
                base_generation: 7,
                payload: serde_json::to_value(journal).unwrap(),
                status: PendingOperationStatus::Pending,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
            };
            barrier.wait();
            store.begin_operation(&operation).is_ok()
        };
        let accepted = thread::scope(|scope| {
            let first = scope.spawn(|| submit(&fixture.store, journal.clone()));
            let second = scope.spawn(|| submit(&second, journal.clone()));
            usize::from(first.join().unwrap()) + usize::from(second.join().unwrap())
        });
        assert_eq!(accepted, 1);
        assert!(!fixture.request.managed_root.exists());
    }

    #[test]
    fn cancellation_during_real_git_hook_preserves_reconciled_created_worktree() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let fixture = Fixture::new();
        let marker = fixture.directory.path().join("hook-running");
        let name = std::ffi::CString::new(marker.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is NUL-terminated and belongs to this isolated test fixture.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let ready = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&marker)
            .unwrap();
        let hook = fixture.request.repository.join(".git/hooks/post-checkout");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf ready > '{}'\nsleep 30\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        let waiter = thread::spawn(move || {
            let mut descriptor = libc::pollfd {
                fd: ready.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: descriptor points to one initialized entry; ready owns its fd.
            let signalled = unsafe { libc::poll(&mut descriptor, 1, 10_000) };
            cancel.cancel();
            assert!(
                signalled > 0 && descriptor.revents & libc::POLLIN != 0,
                "Git never entered its checkout hook"
            );
        });
        let report = fixture.run(&cancellation);
        waiter.join().unwrap();
        assert!(cancellation.is_cancelled());
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        assert_eq!(
            fixture
                .service
                .rehydrate_worktree_operations(&fixture.store)
                .unwrap()[0]
                .state,
            WorktreeOperationState::Created
        );
    }

    struct FailCreationFollowup {
        unlock: bool,
    }

    impl ProcessExecutor for FailCreationFollowup {
        fn execute(
            &self,
            command: &CommandSpec,
            token: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            let fail = if self.unlock {
                command
                    .arguments
                    .windows(2)
                    .any(|pair| pair[0] == "worktree" && pair[1] == "unlock")
            } else {
                command
                    .arguments
                    .iter()
                    .any(|arg| arg.as_encoded_bytes().starts_with(b"--set-upstream-to="))
            };
            if fail {
                return Err(process_failure(
                    command,
                    ProcessFailureKind::Spawn,
                    "injected followup failure",
                    false,
                ));
            }
            SystemProcessExecutor.execute(command, token)
        }
    }

    #[test]
    fn created_followup_failures_remain_durable_visible_warnings() {
        for unlock in [false, true] {
            let mut fixture = Fixture::new();
            fixture.request.track = true;
            let service = RepositoryService::new("git", Arc::new(FailCreationFollowup { unlock }));
            let (sender, _) = async_channel::bounded(8);
            let report = service.create_worktree_journaled(
                &fixture.store,
                &fixture.request,
                7,
                &CancellationToken::new(),
                &sender,
            );
            assert_eq!(report.state, WorktreeOperationState::Created);
            let Some(WorktreeCreationOutcome::Created { worktree, warnings }) = &report.outcome
            else {
                panic!("missing created outcome");
            };
            assert_eq!(worktree.lock.locked, unlock);
            assert!(
                warnings
                    .iter()
                    .any(|warning| warning.message == "injected followup failure")
            );
            let restored = service
                .rehydrate_worktree_operations(&fixture.store)
                .unwrap()
                .remove(0);
            assert_eq!(restored.outcome, report.outcome);
        }
    }

    #[test]
    fn incomplete_checkout_and_replaced_destination_never_rehydrate_created() {
        for replace_destination in [false, true] {
            let fixture = Fixture::new();
            let (mut journal, payload) = fixture.prepare();
            let lease = worktree_lease(&journal.lease_path, true).unwrap();
            journal.lease_identity = Some(lease_file_identity(&lease).unwrap());
            journal.state = WorktreeOperationState::Running;
            journal.effects_started = true;
            let output = Command::new("git")
                .arg("-C")
                .arg(&fixture.request.repository)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            let oid = String::from_utf8(output.stdout).unwrap().trim().to_owned();
            journal.base_oid = Some(oid.clone());
            journal.persist(&fixture.store, &payload).unwrap();
            fs::create_dir(&fixture.request.managed_root).unwrap();
            let mut command = fixture
                .service
                .create_worktree_command(&fixture.request, &oid)
                .unwrap();
            if !replace_destination {
                let add = command
                    .arguments
                    .iter()
                    .position(|arg| arg == "add")
                    .unwrap();
                command.arguments.insert(add + 1, "--no-checkout".into());
            }
            command.inherited_lease = Some(lease.as_raw_fd());
            assert!(
                SystemProcessExecutor
                    .execute(&command, &CancellationToken::new())
                    .unwrap()
                    .success
            );
            if replace_destination {
                fs::rename(
                    &fixture.request.destination,
                    fixture.directory.path().join("original"),
                )
                .unwrap();
                fs::create_dir(&fixture.request.destination).unwrap();
                fs::write(
                    fixture.request.destination.join(".git"),
                    format!("gitdir: {}\n", journal.common_dir.display()),
                )
                .unwrap();
            }
            drop(lease);
            let report = fixture
                .service
                .rehydrate_worktree_operations(&fixture.store)
                .unwrap()
                .remove(0);
            assert_eq!(report.state, WorktreeOperationState::OutcomeUnknown);
            assert_eq!(
                fixture.run(&CancellationToken::new()).state,
                WorktreeOperationState::LeaseConflict
            );
        }
    }

    #[test]
    fn post_executor_settlement_waits_for_unrelated_pre_exec_descriptor_inheritance() {
        use std::os::unix::{net::UnixStream, process::CommandExt};
        let fixture = Fixture::new();
        let (journal, _) = fixture.prepare();
        let lease = worktree_lease(&journal.lease_path, true).unwrap();
        let (ready_write, mut ready_read) = UnixStream::pair().unwrap();
        let (gate_read, mut gate_write) = UnixStream::pair().unwrap();
        let mut command = Command::new("/bin/true");
        // SAFETY: child uses only async-signal-safe read/write on inherited
        // descriptors, with initialized one-byte stack buffers.
        unsafe {
            command.pre_exec(move || {
                let byte = [1_u8];
                if libc::write(ready_write.as_raw_fd(), byte.as_ptr().cast(), 1) != 1 {
                    return Err(io::Error::last_os_error());
                }
                let mut released = [0_u8];
                if libc::read(gate_read.as_raw_fd(), released.as_mut_ptr().cast(), 1) != 1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = thread::spawn(move || command.status().unwrap());
        ready_read.read_exact(&mut [0_u8]).unwrap();
        drop(lease);
        assert_eq!(
            worktree_lease(&journal.lease_path, false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            gate_write.write_all(&[1]).unwrap();
        });
        let settled = worktree_lease_with_grace(&journal.lease_path, false, CLEANUP_GRACE);
        release.join().unwrap();
        assert!(child.join().unwrap().success());
        assert!(settled.is_ok(), "{settled:?}");
    }
}

fn lease_file_identity(file: &fs::File) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}
