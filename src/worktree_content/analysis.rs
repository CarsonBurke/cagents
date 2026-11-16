use super::*;
use crate::process::{
    CancellationToken, CommandSpec, ProcessExecutor, ProcessOutput, SystemProcessExecutor,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// `merge-tree --merge-base` arrived in 2.40.
const MINIMUM_GIT: (u32, u32) = (2, 40);
/// Content an analysis hashes or compares beyond what is committed. Past this
/// the worktree needs a human.
const CONTENT_LIMIT: u64 = 1 << 30;
/// Git LFS files are only hashed, never copied, so they get more room.
const LFS_LIMIT: u64 = 16 << 30;
/// Directory entries the nested-repository walk visits.
const WALK_LIMIT: u64 = 5_000_000;
const LISTED: usize = 100;
const TIMEOUT: Duration = Duration::from_secs(120);
const SCRATCH_PREFIX: &str = "cagents-content-";
/// A crashed inspection's scratch directory is removed by a later one.
const SCRATCH_RETENTION: Duration = Duration::from_secs(60 * 60);
const OPERATIONS: [(&str, Operation); 7] = [
    ("MERGE_HEAD", Operation::Merge),
    ("rebase-merge", Operation::Rebase),
    ("rebase-apply", Operation::Rebase),
    ("CHERRY_PICK_HEAD", Operation::CherryPick),
    ("REVERT_HEAD", Operation::Revert),
    ("BISECT_LOG", Operation::Bisect),
    ("sequencer", Operation::Sequencer),
];
/// Inherited variables that would change what Git reads, ignores, merges or
/// fetches. The daemon's environment is not the user's repository config.
const SCRUBBED: [&str; 16] = [
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_ATTR_SOURCE",
    "GIT_SHALLOW_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_QUARANTINE_PATH",
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
    "GIT_TEST_SPLIT_INDEX",
    "GIT_LFS_SKIP_SMUDGE",
];
const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "cagents"),
    ("GIT_AUTHOR_EMAIL", "cagents@localhost"),
    ("GIT_COMMITTER_NAME", "cagents"),
    ("GIT_COMMITTER_EMAIL", "cagents@localhost"),
];

/// Judges what removing the worktree at `request.path` would lose. Writes no
/// object, ref or index of the repository. A clean filter such as Git LFS
/// may still fill its own cache while `add` runs.
pub fn inspect(
    request: &Request,
    cancellation: &CancellationToken,
) -> Result<Inspection, ContentError> {
    Ok(analyze(request, Objects::Quarantine, cancellation)?.inspection)
}

/// Stores the worktree's uncommitted content as a [`Snapshot`], provided it
/// still matches the inspection that issued `token`.
pub fn snapshot(
    request: &Request,
    token: &str,
    cancellation: &CancellationToken,
) -> Result<Snapshot, ContentError> {
    let analysis = analyze(request, Objects::Repository, cancellation)?;
    if let Verdict::Blocked(blocker) = analysis.inspection.verdict {
        return Err(ContentError::Blocked(blocker));
    }
    if analysis.inspection.token.as_deref() != Some(token) {
        return Err(ContentError::Changed);
    }
    let content = analysis.content.expect("an unblocked analysis has content");
    content.store(analysis.inspection.branch.as_deref())
}

/// Where the trees' objects go. An inspection quarantines them; a snapshot
/// keeps them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Objects {
    Quarantine,
    Repository,
}

struct Analysis {
    inspection: Inspection,
    content: Option<Content>,
}

/// The trees an unblocked analysis computed, with the scratch directory that
/// holds its index copy and, for an inspection, its quarantined objects.
struct Content {
    scratch: tempfile::TempDir,
    worktree: PathBuf,
    head: String,
    index: String,
    work: String,
    full: String,
    untracked: Vec<Vec<u8>>,
    cancellation: CancellationToken,
}

struct Git<'a> {
    worktree: &'a Path,
    index: Option<&'a Path>,
    /// A quarantine object directory, which reads the repository's objects
    /// through its `info/alternates`.
    quarantine: Option<&'a Path>,
    cancellation: &'a CancellationToken,
}

impl Git<'_> {
    fn command<I, S>(&self, arguments: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut command = crate::repository::RepositoryService::system("git")
            .git_base(self.worktree, false)
            .timeout(TIMEOUT)
            .args([
                "-c",
                "core.splitIndex=false",
                "-c",
                "index.sparse=false",
                "-c",
                "core.untrackedCache=false",
                // A case-folding repository copied onto a case-sensitive
                // filesystem would hide files that differ only in case.
                "-c",
                "core.ignoreCase=false",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
            ])
            .env("GIT_OPTIONAL_LOCKS", "0")
            // Replace refs could fake both ancestry and HEAD's tree.
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            // A partial clone must not fetch while it is judged.
            .env("GIT_NO_LAZY_FETCH", "1");
        for name in SCRUBBED {
            command = command.env_remove(name);
        }
        if let Some(index) = self.index {
            command = command.env("GIT_INDEX_FILE", index);
        }
        if let Some(objects) = self.quarantine {
            command = command.env("GIT_OBJECT_DIRECTORY", objects);
        }
        command.args(arguments)
    }

    /// Runs `command`, leaving its exit code to the caller.
    fn output(&self, command: &CommandSpec) -> Result<ProcessOutput, ContentError> {
        SystemProcessExecutor
            .execute(command, self.cancellation)
            .map_err(|failure| ContentError::Git(failure.to_string()))
    }

    fn checked(&self, command: &CommandSpec) -> Result<Vec<u8>, ContentError> {
        let output = self.output(command)?;
        if output.success {
            return Ok(output.stdout);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().find(|line| !line.is_empty()).unwrap_or("");
        Err(ContentError::Git(format!(
            "git exited with {:?}: {}",
            output.exit_code,
            detail.chars().take(240).collect::<String>()
        )))
    }

    fn read<I, S>(&self, arguments: I) -> Result<Vec<u8>, ContentError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.checked(&self.command(arguments))
    }

    /// One line of output, without its newline.
    fn line<I, S>(&self, arguments: I) -> Result<String, ContentError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let output = self.read(arguments)?;
        String::from_utf8(output)
            .map(|text| text.trim_end_matches('\n').to_owned())
            .map_err(|_| ContentError::Git("git printed non-UTF-8 where a name was due".into()))
    }

    /// The answer of a command that says no with exit status 1.
    fn answer<I, S>(&self, arguments: I) -> Result<Option<Vec<u8>>, ContentError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let command = self.command(arguments);
        let output = self.output(&command)?;
        match output.exit_code {
            Some(0) => Ok(Some(output.stdout)),
            Some(1) => Ok(None),
            _ => self.checked(&command).map(Some),
        }
    }

    fn tree_of(&self, commit: &str) -> Result<String, ContentError> {
        self.line(["rev-parse", "--verify", &format!("{commit}^{{tree}}")])
    }

    /// Paths whose entries differ between two trees.
    fn changed(&self, from: &str, to: &str) -> Result<Vec<Vec<u8>>, ContentError> {
        self.changed_by(from, to, None)
    }

    fn changed_by(
        &self,
        from: &str,
        to: &str,
        filter: Option<&str>,
    ) -> Result<Vec<Vec<u8>>, ContentError> {
        let mut arguments = vec!["diff-tree", "-r", "-z", "--name-only", "--no-renames"];
        arguments.extend(filter);
        arguments.extend([from, to]);
        let output = self.read(arguments)?;
        Ok(fields(&output).map(<[u8]>::to_vec).collect())
    }

    /// The tree a clean merge produced, or `None` for a conflicted one or a
    /// merge Git refuses, such as one between unrelated histories.
    fn merged_tree(&self, arguments: &[&str]) -> Result<Option<String>, ContentError> {
        let command = self.command(
            ["merge-tree", "--write-tree", "-z", "--no-messages"]
                .iter()
                .chain(arguments),
        );
        let output = self.output(&command)?;
        if output.exit_code != Some(0) {
            return Ok(None);
        }
        Ok(fields(&output.stdout)
            .next()
            .and_then(|tree| std::str::from_utf8(tree).ok())
            .map(str::to_owned))
    }

    /// Each of `names` for each of `paths`.
    fn attributes(
        &self,
        paths: &[Vec<u8>],
        names: &[&str],
        source: Option<&str>,
    ) -> Result<Vec<Attribute>, ContentError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut command = self.command(["check-attr", "-z", "--stdin"]);
        if let Some(source) = source {
            command = command.args(["--source", source]);
        }
        let output = self.checked(&command.args(names).stdin(nul_joined(paths)))?;
        let fields = output
            .strip_suffix(b"\0")
            .unwrap_or(&output)
            .split(|byte| *byte == 0)
            .collect::<Vec<_>>();
        Ok(fields
            .as_chunks::<3>()
            .0
            .iter()
            .map(|[path, name, value]| (path.to_vec(), name.to_vec(), value.to_vec()))
            .collect())
    }
}

/// `(path, attribute, value)`, as `check-attr` prints them.
type Attribute = (Vec<u8>, Vec<u8>, Vec<u8>);

/// The merge drivers the repository's config defines. Git consults them before
/// its built-in drivers, so one may hide behind a built-in name.
struct Drivers {
    configured: BTreeSet<Vec<u8>>,
    default: Option<Vec<u8>>,
}

impl Drivers {
    fn load(git: &Git) -> Result<Self, ContentError> {
        let listing = git
            .answer(["config", "--name-only", "--get-regexp", r"^merge\..+\..+$"])?
            .unwrap_or_default();
        let configured = listing
            .split(|byte| *byte == b'\n')
            .filter_map(|key| {
                let rest = key.strip_prefix(b"merge.")?;
                let dot = rest.iter().rposition(|byte| *byte == b'.')?;
                Some(rest[..dot].to_vec())
            })
            .collect();
        let default = git
            .answer(["config", "--get", "merge.default"])?
            .map(|name| name.trim_ascii_end().to_vec());
        Ok(Self {
            configured,
            default,
        })
    }

    /// Whether a `merge` attribute value selects a driver that could report a
    /// clean merge it never computed. `union` is built in but does.
    fn custom(&self, value: &[u8]) -> bool {
        // `check-attr` prints a literal `merge=unspecified` or `merge=unset`
        // like the states, but Git looks those up as driver names.
        if self.configured.contains(value) {
            return true;
        }
        let name = match value {
            b"unspecified" => match &self.default {
                Some(name) => name.as_slice(),
                None => return false,
            },
            b"unset" => return false,
            name => name,
        };
        name == b"union" || self.configured.contains(name)
    }

    /// Whether any of `paths` selects such a driver, by the worktree's
    /// attributes or `target`'s.
    fn any(&self, git: &Git, paths: &[Vec<u8>], target: &str) -> Result<bool, ContentError> {
        for source in [None, Some(target)] {
            if git
                .attributes(paths, &["merge"], source)?
                .iter()
                .any(|(_, _, value)| self.custom(value))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// NUL-terminated fields, the final terminator optional.
fn fields(output: &[u8]) -> impl Iterator<Item = &[u8]> {
    output
        .strip_suffix(b"\0")
        .unwrap_or(output)
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
}

fn nul_joined(paths: &[Vec<u8>]) -> Vec<u8> {
    paths.iter().fold(Vec::new(), |mut input, path| {
        input.extend_from_slice(path);
        input.push(0);
        input
    })
}

fn display(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn oid(output: Vec<u8>) -> String {
    display(output.trim_ascii_end())
}

pub(super) fn supported(version: &str) -> bool {
    let mut numbers = version
        .strip_prefix("git version ")
        .unwrap_or(version)
        .split(|character: char| !character.is_ascii_digit())
        .map(str::parse::<u32>);
    match (numbers.next(), numbers.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => (major, minor) >= MINIMUM_GIT,
        _ => false,
    }
}

fn blocked(path: &Path, blocker: Blocker) -> Analysis {
    Analysis {
        inspection: Inspection {
            path: path.to_owned(),
            branch: None,
            head: None,
            verdict: Verdict::Blocked(blocker),
            landed_in: None,
            staged: 0,
            modified: 0,
            untracked: 0,
            untracked_bytes: 0,
            filtered: Vec::new(),
            nested: Vec::new(),
            ignored: Vec::new(),
            ignored_total: 0,
            token: None,
        },
        content: None,
    }
}

/// Copies the index keeping its mtime. An entry modified within the index's
/// own timestamp is "racily clean": Git rechecks its content only because the
/// index is not newer than it. A fresh mtime would make Git trust its stat.
/// Both come from one open file, since Git replaces the index by rename.
fn copy_index(source: &Path, destination: &Path) -> Result<(), ContentError> {
    let mut original = match std::fs::File::open(source) {
        Ok(file) => file,
        // A `--no-checkout` worktree has no index; Git reads that as empty.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let modified = original.metadata()?.modified()?;
    let mut copy = std::fs::File::create(destination)?;
    std::io::copy(&mut original, &mut copy)?;
    copy.set_modified(modified)?;
    Ok(())
}

/// Removes scratch directories a crashed inspection left behind.
fn prune_scratch(common: &Path) {
    let Ok(entries) = std::fs::read_dir(common) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .file_name()
            .as_bytes()
            .starts_with(SCRATCH_PREFIX.as_bytes())
            && entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| {
                    now.duration_since(modified)
                        .is_ok_and(|age| age > SCRATCH_RETENTION)
                });
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// What `git worktree remove --force` would destroy without Git knowing.
#[derive(Default)]
struct Walk {
    /// Repositories anywhere below the top level, bare ones and those in
    /// ignored directories included.
    nested: Vec<String>,
    /// A directory removal could not empty, or a tree too large to walk.
    blocker: Option<Blocker>,
}

fn walk(root: &Path, cancellation: &CancellationToken) -> Result<Walk, ContentError> {
    let mut walk = Walk::default();
    // A mount below the root is emptied by a recursive removal, though no
    // file in it belongs to the worktree.
    let own = filesystem(root)?.ok_or_else(|| ContentError::Path(root.to_owned()))?;
    let mut pending = vec![PathBuf::new()];
    let mut visited = 0u64;
    let mut reported = Vec::<PathBuf>::new();
    while let Some(relative) = pending.pop() {
        let directory = root.join(&relative);
        let unremovable = || Some(Blocker::Unremovable(format!("{}/", relative.display())));
        // Removal lists, enters and unlinks inside every directory.
        let path = std::ffi::CString::new(directory.as_os_str().as_bytes())
            .map_err(|_| ContentError::Path(directory.clone()))?;
        if unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK | libc::X_OK) } != 0 {
            walk.blocker = unremovable();
            return Ok(walk);
        }
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                walk.blocker = unremovable();
                return Ok(walk);
            }
            Err(error) => return Err(error.into()),
        };
        let mut directories = Vec::new();
        let (mut dot_git, mut head, mut objects, mut refs) = (false, false, false, false);
        for entry in entries {
            let entry = entry?;
            visited += 1;
            if visited > WALK_LIMIT {
                walk.blocker = Some(Blocker::TooManyFiles { limit: WALK_LIMIT });
                return Ok(walk);
            }
            if visited.is_multiple_of(4096) && cancellation.is_cancelled() {
                return Err(ContentError::Cancelled);
            }
            let name = entry.file_name();
            let is_directory = entry.file_type()?.is_dir();
            match name.as_bytes() {
                b".git" => dot_git = !relative.as_os_str().is_empty(),
                b"HEAD" => head = !is_directory,
                b"objects" => objects = is_directory,
                b"refs" => refs = is_directory,
                _ => {}
            }
            if is_directory && !(name == ".git" && relative.as_os_str().is_empty()) {
                let child = relative.join(name);
                match filesystem(&root.join(&child))? {
                    None => continue,
                    Some(other) if other != own => {
                        walk.blocker = Some(Blocker::MountPoint(format!("{}/", child.display())));
                        return Ok(walk);
                    }
                    Some(_) => directories.push(child),
                }
            }
        }
        // A nested repository is reported once, not again for its own git
        // directory, but still walked: removal must be able to empty it too.
        let repository = dot_git || (head && objects && refs && !relative.as_os_str().is_empty());
        if repository && !reported.iter().any(|outer| relative.starts_with(outer)) {
            walk.nested.push(format!("{}/", relative.display()));
            reported.push(relative.clone());
        }
        pending.extend(directories);
    }
    walk.nested.sort();
    Ok(walk)
}

/// The device and mount a directory is on, or `None` once it is gone or is
/// no longer a directory. The
/// mount tells apart a bind mount of the same file system; the device, a
/// Btrfs subvolume, which is no mount. Kernels before 5.8 report no mount.
pub(super) fn filesystem(path: &Path) -> std::io::Result<Option<(u64, u64, Option<u64>)>> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let mut metadata = std::mem::MaybeUninit::<libc::statx>::zeroed();
    let result = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT | libc::AT_STATX_DONT_SYNC,
            libc::STATX_MNT_ID,
            metadata.as_mut_ptr(),
        )
    };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTDIR) => Ok(None),
            // A sandbox may filter statx; the device still finds most mounts.
            Some(libc::ENOSYS | libc::EPERM) => {
                let metadata =
                    std::fs::symlink_metadata(std::ffi::OsStr::from_bytes(path.as_bytes()))?;
                Ok(Some((
                    libc::major(metadata.dev()).into(),
                    libc::minor(metadata.dev()).into(),
                    None,
                )))
            }
            _ => Err(error),
        };
    }
    let metadata = unsafe { metadata.assume_init() };
    let mount = (metadata.stx_mask & libc::STATX_MNT_ID != 0).then_some(metadata.stx_mnt_id);
    Ok(Some((
        u64::from(metadata.stx_dev_major),
        u64::from(metadata.stx_dev_minor),
        mount,
    )))
}

/// Whether two files hold the same bytes, read in chunks.
fn same_content(left: &Path, right: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    if left.metadata()?.len() != right.metadata()?.len() {
        return Ok(false);
    }
    let (mut left, mut right) = (std::fs::File::open(left)?, std::fs::File::open(right)?);
    let (mut left_buffer, mut right_buffer) = (vec![0; 1 << 16], vec![0; 1 << 16]);
    loop {
        let read = left.read(&mut left_buffer)?;
        if read == 0 {
            return Ok(true);
        }
        right.read_exact(&mut right_buffer[..read])?;
        if left_buffer[..read] != right_buffer[..read] {
            return Ok(false);
        }
    }
}

/// Whether a worktree file is what a Git LFS pointer names, or the pointer
/// itself, as a checkout that skipped smudging leaves it. Hashing the file
/// avoids the smudge filter, which would download a missing object.
fn lfs_matches(file: &Path, pointer: &[u8]) -> std::io::Result<bool> {
    use std::io::Read;
    let length = file.metadata()?.len();
    if length == pointer.len() as u64 && std::fs::read(file)? == pointer {
        return Ok(true);
    }
    let field = |key: &[u8]| {
        pointer
            .split(|byte| *byte == b'\n')
            .find_map(|line| line.strip_prefix(key))
            .map(<[u8]>::to_vec)
    };
    let (Some(oid), Some(size)) = (field(b"oid sha256:"), field(b"size ")) else {
        return Ok(false);
    };
    if size != length.to_string().as_bytes() {
        return Ok(false);
    }
    let mut hasher = Sha256::new();
    let mut reader = std::fs::File::open(file)?;
    let mut buffer = vec![0; 1 << 16];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()).as_bytes() == oid.as_slice())
}

/// `$Id: <oid> $` as `$Id$`: the form `ident` stores, so a file committed
/// with a stale expansion still matches.
fn collapse_ident(content: &[u8]) -> Vec<u8> {
    let mut collapsed = Vec::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.windows(4).position(|window| window == b"$Id:") {
        collapsed.extend_from_slice(&rest[..start]);
        let after = &rest[start + 4..];
        match after.iter().position(|byte| matches!(byte, b'$' | b'\n')) {
            Some(end) if after[end] == b'$' => {
                collapsed.extend_from_slice(b"$Id$");
                rest = &after[end + 1..];
            }
            _ => {
                collapsed.extend_from_slice(b"$Id:");
                rest = after;
            }
        }
    }
    collapsed.extend_from_slice(rest);
    collapsed
}

/// Present files whose content a clean filter, `ident` or a working-tree
/// encoding changes on the way in, so that what Git stores does not give back
/// the file. Every such file counts when together they are too large to
/// compare: Git LFS files past [`LFS_LIMIT`], others past [`CONTENT_LIMIT`].
fn filtered(
    git: &Git,
    worktree: &Path,
    tree: &str,
    scratch: &Path,
) -> Result<Vec<Vec<u8>>, ContentError> {
    let filters = git
        .answer([
            "config",
            "--name-only",
            "--get-regexp",
            r"^filter\..+\.(clean|process)$",
        ])?
        .unwrap_or_default()
        .split(|byte| *byte == b'\n')
        .filter_map(|key| {
            let rest = key.strip_prefix(b"filter.")?;
            let dot = rest.iter().rposition(|byte| *byte == b'.')?;
            Some(rest[..dot].to_vec())
        })
        .collect::<BTreeSet<_>>();
    // "<mode> <type> <oid>\t<path>"; symlinks and gitlinks are never
    // filtered, and a sparse checkout's absent files have nothing to lose.
    let listing = git.read(["ls-tree", "-r", "-z", "--full-tree", tree])?;
    let blobs = fields(&listing)
        .filter(|record| record.starts_with(b"100"))
        .filter_map(|record| {
            let tab = record.iter().position(|byte| *byte == b'\t')?;
            let oid = record[..tab].rsplit(|byte| *byte == b' ').next()?;
            Some((record[tab + 1..].to_vec(), oid.to_vec()))
        })
        .filter(|(path, _)| {
            worktree
                .join(OsStr::from_bytes(path))
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.is_file())
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let files = blobs.keys().cloned().collect::<Vec<_>>();
    // Per candidate: whether `filter=lfs`, whether `ident`.
    let mut candidates = std::collections::BTreeMap::<Vec<u8>, (bool, bool)>::new();
    for (path, name, value) in
        git.attributes(&files, &["filter", "ident", "working-tree-encoding"], None)?
    {
        let flags = match name.as_slice() {
            b"filter" if filters.contains(&value) => (value == b"lfs", false),
            b"ident" if value == b"set" => (false, true),
            b"working-tree-encoding" if !matches!(value.as_slice(), b"unspecified" | b"unset") => {
                (false, false)
            }
            _ => continue,
        };
        let entry = candidates.entry(path).or_insert((true, false));
        // Only a file LFS alone filters is judged by its pointer.
        entry.0 &= flags.0 && name == b"filter";
        entry.1 |= flags.1;
    }
    let (lfs, checkout): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|(_, (lfs, ident))| *lfs && !*ident);
    let bytes = |set: &[(Vec<u8>, (bool, bool))]| {
        set.iter()
            .filter_map(|(path, _)| worktree.join(OsStr::from_bytes(path)).metadata().ok())
            .map(|metadata| metadata.len())
            .sum::<u64>()
    };
    let mut lossy = Vec::new();
    let mut within = |set: Vec<(Vec<u8>, (bool, bool))>, limit: u64| {
        if bytes(&set) <= limit {
            return set;
        }
        lossy.extend(set.into_iter().map(|(path, _)| path));
        Vec::new()
    };
    let lfs = within(lfs, LFS_LIMIT);
    let checkout = within(checkout, CONTENT_LIMIT);
    if !lfs.is_empty() {
        let input = lfs.iter().fold(Vec::new(), |mut input, (path, _)| {
            input.extend_from_slice(&blobs[path]);
            input.push(b'\n');
            input
        });
        let output = git.checked(&git.command(["cat-file", "--batch"]).stdin(input))?;
        let mut rest = output.as_slice();
        for (path, _) in &lfs {
            let newline = rest
                .iter()
                .position(|byte| *byte == b'\n')
                .ok_or_else(|| ContentError::Git("unexpected cat-file output".into()))?;
            let size = std::str::from_utf8(&rest[..newline])
                .ok()
                .and_then(|header| header.rsplit(' ').next()?.parse::<usize>().ok())
                .filter(|size| newline + 1 + size < rest.len())
                .ok_or_else(|| ContentError::Git("unexpected cat-file output".into()))?;
            let pointer = &rest[newline + 1..newline + 1 + size];
            rest = &rest[newline + 2 + size..];
            if !lfs_matches(&worktree.join(OsStr::from_bytes(path)), pointer)? {
                lossy.push(path.clone());
            }
        }
    }
    if !checkout.is_empty() {
        // Check the stored content out through the same filters, beside the
        // index copy rather than into the worktree.
        let directory = scratch.join("checkout");
        let mut argument = OsString::from("--prefix=");
        argument.push(directory.as_os_str());
        argument.push("/");
        let paths = checkout
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        git.checked(
            &git.command([
                OsString::from("checkout-index"),
                OsString::from("-z"),
                OsString::from("--stdin"),
                argument,
            ])
            .stdin(nul_joined(&paths)),
        )?;
        for (path, (_, ident)) in checkout {
            let relative = OsStr::from_bytes(&path);
            let (file, stored) = (worktree.join(relative), directory.join(relative));
            let same = if ident {
                collapse_ident(&std::fs::read(&file)?) == collapse_ident(&std::fs::read(&stored)?)
            } else {
                same_content(&file, &stored)?
            };
            if !same {
                lossy.push(path);
            }
        }
        std::fs::remove_dir_all(&directory)?;
    }
    lossy.sort();
    Ok(lossy)
}

/// The verdict's class, which the token binds: a removal planned for one
/// class never runs as another.
fn class(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Landed => "landed",
        Verdict::CommitsKept => "commits-kept",
        Verdict::Unsaved { .. } => "unsaved",
        Verdict::Blocked(_) => "blocked",
    }
}

fn token(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

fn analyze(
    request: &Request,
    objects: Objects,
    cancellation: &CancellationToken,
) -> Result<Analysis, ContentError> {
    let path = request.path.as_path();
    if !path.is_absolute() || path.canonicalize()? != path {
        return Err(ContentError::Path(path.to_owned()));
    }
    let plain = Git {
        worktree: path,
        index: None,
        quarantine: None,
        cancellation,
    };
    let version = plain.line(["version"])?;
    if !supported(&version) {
        return Ok(blocked(path, Blocker::GitTooOld { version }));
    }
    let top_level = plain.line(["rev-parse", "--show-toplevel"])?;
    if Path::new(&top_level) != path {
        return Err(ContentError::NotTopLevel(path.to_owned()));
    }
    let admin = PathBuf::from(plain.line(["rev-parse", "--path-format=absolute", "--git-dir"])?);
    let common =
        PathBuf::from(plain.line(["rev-parse", "--path-format=absolute", "--git-common-dir"])?);
    // Files-backend locks fence removal; reftable needs its own guarantee.
    if plain
        .answer(["config", "--get", "extensions.refStorage"])?
        .is_some_and(|storage| storage.as_slice() != b"files\n")
    {
        return Ok(blocked(path, Blocker::RefStorage));
    }

    let worktrees = crate::repository::RepositoryService::system("git")
        .list_worktrees(path, cancellation)
        .map_err(|error| ContentError::Git(error.to_string()))?;
    let Some(position) = worktrees.iter().position(|record| {
        record.path == path || record.path.canonicalize().is_ok_and(|real| real == path)
    }) else {
        return Ok(blocked(path, Blocker::NotRegistered));
    };
    let record = &worktrees[position];
    let refusal = if position == 0 {
        Some(Blocker::Primary)
    } else if record.bare {
        Some(Blocker::Bare)
    } else if record.lock.locked {
        Some(Blocker::Locked)
    } else if record.prunable_reason.is_some() {
        Some(Blocker::Prunable)
    } else {
        None
    };
    if let Some(blocker) = refusal {
        return Ok(blocked(path, blocker));
    }

    let symref = plain.answer(["symbolic-ref", "-q", "HEAD"])?.map(oid);
    let Some(head) = plain
        .answer(["rev-parse", "-q", "--verify", "HEAD^{commit}"])?
        .map(oid)
    else {
        return Ok(blocked(path, Blocker::UnbornHead));
    };
    for (name, operation) in OPERATIONS {
        if admin.join(name).symlink_metadata().is_ok() {
            return Ok(blocked(path, Blocker::Operation(operation)));
        }
    }
    // Per-worktree refs live in the admin directory removal deletes.
    let own_ref = plain.read([
        "for-each-ref",
        "--count=1",
        "--format=%(refname)",
        "refs/worktree/",
        "refs/bisect/",
        "refs/rewritten/",
    ])?;
    if !own_ref.is_empty() {
        return Ok(blocked(path, Blocker::WorktreeRefs(oid(own_ref))));
    }

    // "<tag> <mode> <oid> <stage>\t<path>"; a lowercase tag is assume-unchanged.
    let mut assumed = Vec::new();
    for record in fields(&plain.read(["ls-files", "-z", "-v", "-s"])?) {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return Err(ContentError::Git("unexpected ls-files output".into()));
        };
        let (meta, entry) = (&record[..tab], &record[tab + 1..]);
        let mut parts = meta.split(|byte| *byte == b' ');
        let (tag, mode, _, stage) = (parts.next(), parts.next(), parts.next(), parts.next());
        if stage != Some(b"0") {
            return Ok(blocked(path, Blocker::Unmerged));
        }
        let file = path.join(OsStr::from_bytes(entry));
        // Git sees nothing inside a gitlink's directory, checked out or not.
        if mode == Some(b"160000")
            && std::fs::read_dir(&file).is_ok_and(|mut entries| entries.next().is_some())
        {
            return Ok(blocked(path, Blocker::Submodule(display(entry))));
        }
        let tag = tag.and_then(|tag| tag.first()).copied().unwrap_or(b'H');
        if tag.eq_ignore_ascii_case(&b'S') && file.symlink_metadata().is_ok() {
            return Ok(blocked(path, Blocker::SkipWorktree(display(entry))));
        }
        if tag.is_ascii_lowercase() {
            assumed.push(entry.to_vec());
        }
    }
    // Removing the outer worktree would orphan a registered inner one.
    if let Some(inner) = worktrees
        .iter()
        .find(|record| record.path != path && record.path.starts_with(path))
    {
        let inner = inner.path.strip_prefix(path).unwrap_or(&inner.path);
        return Ok(blocked(
            path,
            Blocker::NestedWorktree(format!("{}/", inner.display())),
        ));
    }
    let walk = walk(path, cancellation)?;
    if let Some(blocker) = walk.blocker {
        return Ok(blocked(path, blocker));
    }

    prune_scratch(&common);
    let scratch = tempfile::Builder::new()
        .prefix(SCRATCH_PREFIX)
        .tempdir_in(&common)?;
    let index = scratch.path().join("index");
    copy_index(&admin.join("index"), &index)?;
    let quarantine = scratch.path().join("objects");
    if objects == Objects::Quarantine {
        std::fs::create_dir_all(quarantine.join("pack"))?;
        std::fs::create_dir_all(quarantine.join("info"))?;
        // The file, unlike the environment variable, takes any path.
        let mut alternate = common.join("objects").as_os_str().as_bytes().to_vec();
        alternate.push(b'\n');
        std::fs::write(quarantine.join("info/alternates"), alternate)?;
    }
    let git = Git {
        worktree: path,
        index: Some(&index),
        quarantine: (objects == Objects::Quarantine).then_some(quarantine.as_path()),
        cancellation,
    };
    // Assume-unchanged hides edits from `add`; the copy forgets it.
    if !assumed.is_empty() {
        git.checked(
            &git.command(["update-index", "--no-assume-unchanged", "-z", "--stdin"])
                .stdin(nul_joined(&assumed)),
        )?;
    }

    // Bound what `add -A` hashes before it runs.
    let others = git.read(["ls-files", "-z", "--others", "--exclude-standard"])?;
    let untracked_bytes = fields(&others)
        .filter(|entry| !entry.ends_with(b"/"))
        .filter_map(|entry| path.join(OsStr::from_bytes(entry)).symlink_metadata().ok())
        .map(|metadata| metadata.len())
        .sum::<u64>();
    if untracked_bytes > CONTENT_LIMIT {
        return Ok(blocked(
            path,
            Blocker::TooLarge {
                bytes: untracked_bytes,
            },
        ));
    }
    let ignored_listing = git.read([
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
    ])?;
    let ignored_total = u32::try_from(fields(&ignored_listing).count()).unwrap_or(u32::MAX);
    let ignored = fields(&ignored_listing)
        .take(LISTED)
        .map(|entry| IgnoredEntry {
            path: display(entry),
            directory: entry.ends_with(b"/"),
        })
        .collect();

    // `--sparse` keeps absent skip-worktree entries and still adds untracked
    // files outside the sparse cone.
    let index_tree = git.line(["write-tree"])?;
    git.read(["add", "-u", "--sparse", "--", ":/"])?;
    let work_tree = git.line(["write-tree"])?;
    // Untracked nested repositories, the only directories this listing
    // shows, are reported apart; `add` would record them as gitlinks, or
    // refuse one without a commit.
    let mut pathspecs = b":/\0".to_vec();
    for nested in fields(&others).filter(|entry| entry.ends_with(b"/")) {
        pathspecs.extend_from_slice(b":(top,literal,exclude)");
        pathspecs.extend_from_slice(&nested[..nested.len() - 1]);
        pathspecs.push(0);
    }
    git.checked(
        &git.command([
            "add",
            "-A",
            "--sparse",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ])
        .stdin(pathspecs),
    )?;
    let full_tree = git.line(["write-tree"])?;
    let head_tree = git.tree_of(&head)?;
    let staged = git.changed(&head_tree, &index_tree)?.len();
    let modified = git.changed(&index_tree, &work_tree)?.len();
    // What `add -A` added, rather than what an earlier listing saw.
    let untracked = git.changed_by(&work_tree, &full_tree, Some("--diff-filter=A"))?;
    let lossy = filtered(&git, path, &full_tree, scratch.path())?;

    // A remote-tracking ref disappears with a pruning fetch once its branch
    // is deleted, so it never keeps commits alive.
    let reachable = !git
        .read([
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            "--contains",
            &head,
            "refs/heads/",
            "refs/tags/",
            SNAPSHOT_REFS,
        ])?
        .is_empty();

    let mut targets = crate::repository::RepositoryService::system("git")
        .default_branch_ref(path, cancellation)
        .map_err(|error| ContentError::Git(error.to_string()))?
        .into_iter()
        .collect::<Vec<_>>();
    for target in &request.targets {
        if !targets.contains(target) {
            targets.push(target.clone());
        }
    }
    let drivers = Drivers::load(&git)?;
    // Each uncommitted state that differs from HEAD, as a commit on HEAD with
    // the paths it changes: the staged index can hold content the worktree
    // has since reverted.
    let mut probes = Vec::new();
    for tree in [&index_tree, &full_tree] {
        if *tree == head_tree || probes.iter().any(|(probe_tree, _, _)| probe_tree == tree) {
            continue;
        }
        let mut command = git.command([
            "commit-tree",
            tree.as_str(),
            "-p",
            &head,
            "-m",
            "cagents content probe",
        ]);
        for (name, value) in IDENTITY {
            command = command.env(name, value);
        }
        for name in ["GIT_AUTHOR_DATE", "GIT_COMMITTER_DATE"] {
            command = command.env(name, "@0 +0000");
        }
        let commit = oid(git.checked(&command)?);
        probes.push((tree.clone(), commit, git.changed(&head_tree, tree)?));
    }
    let mut landed_in = None;
    // Neither Git nor a snapshot keeps filtered-away content or nested
    // repositories.
    let unkept = !lossy.is_empty() || !walk.nested.is_empty();
    let mut uncommitted_landed = !unkept && probes.is_empty();
    for target in targets {
        if unkept {
            break;
        }
        let Some(target_oid) = git
            .answer([
                "rev-parse",
                "-q",
                "--verify",
                &format!("{target}^{{commit}}"),
            ])?
            .map(oid)
        else {
            continue;
        };
        let target_tree = git.tree_of(&target_oid)?;
        let merge_base = format!("--merge-base={head}");
        let mut here = true;
        for (_, probe, paths) in &probes {
            here = !drivers.any(&git, paths, &target_oid)?
                && git
                    .merged_tree(&[&merge_base, &target_oid, probe])?
                    .is_some_and(|tree| tree == target_tree);
            if !here {
                break;
            }
        }
        uncommitted_landed |= here;
        if !here || landed_in.is_some() {
            continue;
        }
        let commits = request.merged_head.as_deref() == Some(head.as_str())
            || git
                .answer(["merge-base", "--is-ancestor", &head, &target_oid])?
                .is_some()
            // Every path either side could have changed, merge commits'
            // resolutions included.
            || (!drivers.any(&git, &git.changed(&target_oid, &head)?, &target_oid)?
                && git
                    .merged_tree(&[&target_oid, &head])?
                    .is_some_and(|tree| tree == target_tree));
        if commits {
            landed_in = Some(target);
        }
    }
    let verdict = if landed_in.is_some() {
        Verdict::Landed
    } else if reachable && uncommitted_landed {
        Verdict::CommitsKept
    } else {
        Verdict::Unsaved {
            uncommitted: !uncommitted_landed,
            unreachable: !reachable,
            beyond_snapshot: unkept,
        }
    };

    let identity = std::fs::metadata(path)?;
    let token = token(&[
        common.as_os_str().as_bytes(),
        admin.as_os_str().as_bytes(),
        path.as_os_str().as_bytes(),
        &identity.dev().to_le_bytes(),
        &identity.ino().to_le_bytes(),
        symref.as_deref().unwrap_or("").as_bytes(),
        head.as_bytes(),
        index_tree.as_bytes(),
        work_tree.as_bytes(),
        full_tree.as_bytes(),
        &nul_joined(&lossy),
        walk.nested.join("\0").as_bytes(),
        &ignored_listing,
        class(&verdict).as_bytes(),
    ]);
    let branch = symref
        .as_deref()
        .map(|name| name.strip_prefix("refs/heads/").unwrap_or(name).to_owned());
    let count = |length: usize| u32::try_from(length).unwrap_or(u32::MAX);
    Ok(Analysis {
        inspection: Inspection {
            path: path.to_owned(),
            branch,
            head: Some(head.clone()),
            verdict,
            landed_in,
            staged: count(staged),
            modified: count(modified),
            untracked: count(untracked.len()),
            untracked_bytes,
            filtered: lossy
                .iter()
                .take(LISTED)
                .map(|path| display(path))
                .collect(),
            nested: walk.nested.into_iter().take(LISTED).collect(),
            ignored,
            ignored_total,
            token: Some(token),
        },
        content: Some(Content {
            scratch,
            worktree: path.to_owned(),
            head,
            index: index_tree,
            work: work_tree,
            full: full_tree,
            untracked,
            cancellation: cancellation.clone(),
        }),
    })
}

impl Content {
    /// Writes the stash-shaped commits and a ref that keeps them. Their trees
    /// are already in the repository: a snapshot's analysis wrote no
    /// quarantine.
    fn store(self, branch: Option<&str>) -> Result<Snapshot, ContentError> {
        let git = Git {
            worktree: &self.worktree,
            index: None,
            quarantine: None,
            cancellation: &self.cancellation,
        };
        let short = &self.head[..12.min(self.head.len())];
        let subject = format!("{}: {short}", branch.unwrap_or("(no branch)"));
        let commit = |tree: &str, parents: &[&str], message: &str| {
            let mut command = git.command(["commit-tree", tree]);
            for parent in parents {
                command = command.args(["-p", parent]);
            }
            for (name, value) in IDENTITY {
                command = command.env(name, value);
            }
            Ok::<_, ContentError>(oid(git.checked(&command.args(["-m", message]))?))
        };
        let index = commit(&self.index, &[&self.head], &format!("index on {subject}"))?;
        let untracked = if self.untracked.is_empty() {
            None
        } else {
            let index_path = self.scratch.path().join("untracked-index");
            let only = Git {
                worktree: &self.worktree,
                index: Some(&index_path),
                quarantine: None,
                cancellation: &self.cancellation,
            };
            let wanted = self.untracked.iter().collect::<BTreeSet<_>>();
            // "<mode> <type> <oid>\t<path>" is also `--index-info` input.
            let listing = only.read(["ls-tree", "-r", "-z", "--full-tree", &self.full])?;
            let input = fields(&listing)
                .filter(|record| {
                    record
                        .iter()
                        .position(|byte| *byte == b'\t')
                        .is_some_and(|tab| wanted.contains(&record[tab + 1..].to_vec()))
                })
                .fold(Vec::new(), |mut input, record| {
                    input.extend_from_slice(record);
                    input.push(0);
                    input
                });
            only.read(["read-tree", "--empty"])?;
            only.checked(
                &only
                    .command(["update-index", "-z", "--index-info"])
                    .stdin(input),
            )?;
            let tree = only.line(["write-tree"])?;
            Some(commit(
                &tree,
                &[],
                &format!("untracked files on {subject}"),
            )?)
        };
        let mut parents = vec![self.head.as_str(), index.as_str()];
        parents.extend(untracked.as_deref());
        let message = format!(
            "WIP on {subject}\n\nCagents-Worktree: {}\n",
            self.worktree.display()
        );
        let oid = commit(&self.work, &parents, &message)?;
        let millis = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let refname = format!("{SNAPSHOT_REFS}{millis}-{short}");
        // An empty old value creates the ref only if it does not exist.
        git.read([
            "update-ref",
            "-m",
            "cagents: snapshot before worktree removal",
            &refname,
            &oid,
            "",
        ])?;
        Ok(Snapshot { refname, oid })
    }
}
