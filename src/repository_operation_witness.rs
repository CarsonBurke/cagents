//! Local custody for recovery actions captured from one Git operation.
//!
//! Layout observations follow git/git b9e5cff1803ab24582201ef059516eb16014b9d9:
//! sequencer.c (sequencer paths, rebase paths, save_todo and pick_commits),
//! builtin/rebase.c (read_basic_state), and builtin/am.c (am_load/am_next).
//! These source contracts still require the owning real-Git qualification for
//! both rebase backends and multi-step cherry-pick/revert. Unknown layouts
//! refuse recovery actions rather than manufacturing operation authority.

use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString},
    fs::{self, File, Metadata},
    io::Read,
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use super::{
    CancellationToken, CommandSpec, FileIdentity, GitOperationKind, GitOperationState, Oid,
    ProcessExecutor, ProcessFailureKind, RepositoryError, RepositoryService, file_identity,
    fs_safety, parse_path_line,
};

const SCALAR_LIMIT: u64 = 4096;
const MESSAGE_LIMIT: u64 = 64 * 1024;
const MAX_OBSERVATIONS: usize = 1024;
const MAX_PATH_STEPS: usize = 1024;
const MAX_SYMLINKS: usize = 40;
const REFRESH: &str =
    "Git operation changed or could not be verified; reload Git state before retrying";

type DirectoryIdentity = (u64, u64);

fn directory_identity(metadata: &Metadata) -> DirectoryIdentity {
    let (device, inode, ..) = file_identity(metadata);
    (device, inode)
}

fn failure(reason: impl std::fmt::Display) -> String {
    format!("{REFRESH}: {reason}")
}

fn cancelled(cancellation: &CancellationToken) -> Result<(), String> {
    if cancellation.is_cancelled() {
        Err(failure(
            "the request was cancelled before recovery dispatch",
        ))
    } else {
        Ok(())
    }
}

enum ProofError {
    Cancelled(String),
    Unavailable(String),
}

impl From<String> for ProofError {
    fn from(reason: String) -> Self {
        Self::Unavailable(reason)
    }
}

impl From<RepositoryError> for ProofError {
    fn from(error: RepositoryError) -> Self {
        if matches!(&error, RepositoryError::Process(process) if process.kind == ProcessFailureKind::Cancelled)
        {
            Self::Cancelled(error.to_string())
        } else {
            Self::Unavailable(failure(error))
        }
    }
}

impl std::fmt::Display for ProofError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled(reason) | Self::Unavailable(reason) => formatter.write_str(reason),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadMode {
    Full(u64),
    Prefix(u64),
    Metadata,
}

fn read(file: &mut File, mode: ReadMode) -> Result<Option<Vec<u8>>, String> {
    match mode {
        ReadMode::Full(limit) => {
            let bytes = fs_safety::read_bounded(file, limit).map_err(failure)?;
            if bytes.len() as u64 > limit {
                return Err(failure("operation metadata exceeds its read limit"));
            }
            Ok(Some(bytes))
        }
        ReadMode::Prefix(limit) => {
            let mut bytes = Vec::new();
            file.take(limit).read_to_end(&mut bytes).map_err(failure)?;
            Ok(Some(bytes))
        }
        ReadMode::Metadata => Ok(None),
    }
}

#[derive(Debug)]
enum Retained {
    Absent,
    Directory {
        root: Arc<fs_safety::Root>,
        identity: DirectoryIdentity,
    },
    File {
        file: File,
        identity: FileIdentity,
        bytes: Option<Vec<u8>>,
        mode: ReadMode,
    },
    Link {
        entry: fs_safety::RetainedEntry,
        identity: FileIdentity,
        target: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EntryIdentity {
    Absent,
    Directory(DirectoryIdentity),
    File(FileIdentity, Option<Vec<u8>>, ReadMode),
    Link(FileIdentity, PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservationIdentity {
    parent: DirectoryIdentity,
    path: PathBuf,
    entry: EntryIdentity,
}

#[derive(Debug)]
struct Observation {
    parent: Arc<fs_safety::Root>,
    relative: PathBuf,
    identity: ObservationIdentity,
    retained: Retained,
}

impl Observation {
    fn new(
        parent: &Arc<fs_safety::Root>,
        relative: &Path,
        retained: Retained,
    ) -> Result<Self, String> {
        let entry = match &retained {
            Retained::Absent => EntryIdentity::Absent,
            Retained::Directory { identity, .. } => EntryIdentity::Directory(*identity),
            Retained::File {
                identity,
                bytes,
                mode,
                ..
            } => EntryIdentity::File(*identity, bytes.clone(), *mode),
            Retained::Link {
                identity, target, ..
            } => EntryIdentity::Link(*identity, target.clone()),
        };
        Ok(Self {
            parent: parent.clone(),
            relative: relative.to_owned(),
            identity: ObservationIdentity {
                parent: directory_identity(&parent.metadata().map_err(failure)?),
                path: parent.path().join(relative),
                entry,
            },
            retained,
        })
    }

    fn validate(&self) -> Result<(), String> {
        if directory_identity(&self.parent.metadata().map_err(failure)?) != self.identity.parent {
            return Err(failure("a retained parent directory changed"));
        }
        let current = fs_safety::retain_entry_beneath(&self.parent, &self.relative);
        let entry = match (&self.retained, current) {
            (Retained::Absent, Err(error)) if error.is_not_found() => return Ok(()),
            (Retained::Absent, _) => return Err(failure("an absent operation entry appeared")),
            (_, Ok(entry)) => entry,
            (_, Err(error)) => return Err(failure(error)),
        };
        let metadata = entry.metadata().map_err(failure)?;
        match &self.retained {
            Retained::Absent => unreachable!(),
            Retained::Directory { root, identity } => {
                if !metadata.is_dir()
                    || directory_identity(&metadata) != *identity
                    || directory_identity(&root.metadata().map_err(failure)?) != *identity
                {
                    return Err(failure("an operation directory was replaced"));
                }
            }
            Retained::Link {
                entry: original,
                identity,
                target,
            } => {
                if !metadata.file_type().is_symlink()
                    || file_identity(&metadata) != *identity
                    || file_identity(&original.metadata().map_err(failure)?) != *identity
                    || entry.read_link().map_err(failure)? != *target
                    || file_identity(&entry.metadata().map_err(failure)?) != *identity
                {
                    return Err(failure("a Git directory alias was replaced or changed"));
                }
            }
            Retained::File {
                file,
                identity,
                bytes,
                mode,
            } => {
                if !metadata.is_file()
                    || file_identity(&metadata) != *identity
                    || file_identity(&file.metadata().map_err(failure)?) != *identity
                {
                    return Err(failure("an operation file was replaced or changed"));
                }
                // Read a fresh descriptor. Cloning an existing File shares its
                // offset; queue clones must not affect each other's checks.
                let mut current =
                    fs_safety::open_beneath(&self.parent, &self.relative).map_err(failure)?;
                if file_identity(&current.metadata().map_err(failure)?) != *identity
                    || read(&mut current, *mode)? != *bytes
                    || file_identity(&current.metadata().map_err(failure)?) != *identity
                    || file_identity(&entry.metadata().map_err(failure)?) != *identity
                    || file_identity(&file.metadata().map_err(failure)?) != *identity
                {
                    return Err(failure("operation metadata changed while being verified"));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Capture {
    observations: Vec<Observation>,
}

impl Capture {
    fn push(
        &mut self,
        parent: &Arc<fs_safety::Root>,
        relative: &Path,
        retained: Retained,
    ) -> Result<(), String> {
        if self.observations.len() >= MAX_OBSERVATIONS {
            return Err(failure(
                "Git directory bindings exceed their observation limit",
            ));
        }
        self.observations
            .push(Observation::new(parent, relative, retained)?);
        Ok(())
    }

    fn optional_entry(
        &mut self,
        parent: &Arc<fs_safety::Root>,
        relative: &Path,
    ) -> Result<Option<fs_safety::RetainedEntry>, String> {
        match fs_safety::retain_entry_beneath(parent, relative) {
            Ok(entry) => Ok(Some(entry)),
            Err(error) if error.is_not_found() => {
                self.push(parent, relative, Retained::Absent)?;
                Ok(None)
            }
            Err(error) => Err(failure(error)),
        }
    }

    fn file_entry(
        &mut self,
        parent: &Arc<fs_safety::Root>,
        relative: &Path,
        entry: fs_safety::RetainedEntry,
        mode: ReadMode,
    ) -> Result<Option<Vec<u8>>, String> {
        let metadata = entry.metadata().map_err(failure)?;
        if !metadata.is_file() {
            return Err(failure(format!(
                "{} is not a regular operation file",
                relative.display()
            )));
        }
        let identity = file_identity(&metadata);
        let mut file = fs_safety::open_beneath(parent, relative).map_err(failure)?;
        if file_identity(&file.metadata().map_err(failure)?) != identity {
            return Err(failure("an operation entry changed before it was opened"));
        }
        let bytes = read(&mut file, mode)?;
        if file_identity(&file.metadata().map_err(failure)?) != identity
            || file_identity(&entry.metadata().map_err(failure)?) != identity
        {
            return Err(failure("an operation entry changed while being captured"));
        }
        self.push(
            parent,
            relative,
            Retained::File {
                file,
                identity,
                bytes: bytes.clone(),
                mode,
            },
        )?;
        Ok(bytes)
    }

    fn file(
        &mut self,
        parent: &Arc<fs_safety::Root>,
        relative: &str,
        mode: ReadMode,
        required: bool,
    ) -> Result<Option<Vec<u8>>, String> {
        let path = Path::new(relative);
        let Some(entry) = self.optional_entry(parent, path)? else {
            return if required {
                Err(failure(format!(
                    "required operation file {relative} is missing"
                )))
            } else {
                Ok(None)
            };
        };
        self.file_entry(parent, path, entry, mode)
    }

    fn directory_entry(
        &mut self,
        parent: &Arc<fs_safety::Root>,
        relative: &Path,
        entry: fs_safety::RetainedEntry,
    ) -> Result<Arc<fs_safety::Root>, String> {
        let metadata = entry.metadata().map_err(failure)?;
        if !metadata.is_dir() {
            return Err(failure(format!(
                "{} is not an operation directory",
                relative.display()
            )));
        }
        let identity = directory_identity(&metadata);
        // This opens the parent of a fixed child without creating that child.
        let (root, _) =
            fs_safety::open_parent_beneath(parent, &relative.join("operation-witness-entry"))
                .map_err(failure)?;
        let root = Arc::new(root);
        if directory_identity(&root.metadata().map_err(failure)?) != identity
            || directory_identity(&entry.metadata().map_err(failure)?) != identity
        {
            return Err(failure(
                "an operation directory changed while being captured",
            ));
        }
        self.push(
            parent,
            relative,
            Retained::Directory {
                root: root.clone(),
                identity,
            },
        )?;
        Ok(root)
    }

    fn directory(
        &mut self,
        parent: &Arc<fs_safety::Root>,
        relative: &str,
    ) -> Result<Option<Arc<fs_safety::Root>>, String> {
        let path = Path::new(relative);
        self.optional_entry(parent, path)?
            .map(|entry| self.directory_entry(parent, path, entry))
            .transpose()
    }

    fn validate(&self, cancellation: &CancellationToken) -> Result<(), String> {
        for observation in &self.observations {
            cancelled(cancellation)?;
            observation.validate()?;
        }
        cancelled(cancellation)
    }

    fn retained_directory(
        &self,
        parent: &Arc<fs_safety::Root>,
        relative: &str,
    ) -> Option<&Arc<fs_safety::Root>> {
        self.observations.iter().find_map(|observation| {
            if Arc::ptr_eq(&observation.parent, parent)
                && observation.relative == Path::new(relative)
                && let Retained::Directory { root, .. } = &observation.retained
            {
                Some(root)
            } else {
                None
            }
        })
    }
}

#[derive(Debug)]
enum Part {
    Root,
    Parent,
    Name(OsString),
}

fn parts(path: &Path) -> Result<VecDeque<Part>, String> {
    path.components()
        .filter_map(|component| match component {
            Component::RootDir => Some(Ok(Part::Root)),
            Component::ParentDir => Some(Ok(Part::Parent)),
            Component::Normal(name) => Some(Ok(Part::Name(name.to_owned()))),
            Component::CurDir => None,
            Component::Prefix(_) => Some(Err(failure("unsupported Git directory path prefix"))),
        })
        .collect()
}

#[derive(Clone)]
struct DirectoryPath(Vec<Arc<fs_safety::Root>>);

impl DirectoryPath {
    fn root(&self) -> &Arc<fs_safety::Root> {
        self.0
            .last()
            .expect("a directory path retains its filesystem root")
    }
}

enum Target {
    Directory(DirectoryPath),
    File {
        parent: DirectoryPath,
        name: OsString,
        entry: fs_safety::RetainedEntry,
    },
}

/// Resolve named Git bindings through retained nofollow entries. In particular,
/// never read a symbolic link by reopening its name after classifying it.
fn resolve(
    capture: &mut Capture,
    mut directory: DirectoryPath,
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<Target, String> {
    let mut pending = parts(path)?;
    let mut links = 0;
    let mut steps = 0;
    while let Some(part) = pending.pop_front() {
        cancelled(cancellation)?;
        steps += 1;
        if steps > MAX_PATH_STEPS {
            return Err(failure("Git directory path exceeds its resolution limit"));
        }
        let name = match part {
            Part::Root => {
                directory.0.truncate(1);
                continue;
            }
            Part::Parent => {
                if directory.0.len() > 1 {
                    directory.0.pop();
                }
                continue;
            }
            Part::Name(name) => name,
        };
        let relative = Path::new(&name);
        let entry = fs_safety::retain_entry_beneath(directory.root(), relative).map_err(failure)?;
        let metadata = entry.metadata().map_err(failure)?;
        if metadata.file_type().is_symlink() {
            links += 1;
            if links > MAX_SYMLINKS {
                return Err(failure(
                    "Git directory aliases exceed their resolution limit",
                ));
            }
            let identity = file_identity(&metadata);
            let target = entry.read_link().map_err(failure)?;
            if target.as_os_str().is_empty()
                || file_identity(&entry.metadata().map_err(failure)?) != identity
            {
                return Err(failure(
                    "a Git directory alias changed while being captured",
                ));
            }
            let mut target_parts = parts(&target)?;
            target_parts.append(&mut pending);
            pending = target_parts;
            capture.push(
                directory.root(),
                relative,
                Retained::Link {
                    entry,
                    identity,
                    target,
                },
            )?;
        } else if metadata.is_dir() {
            let root = capture.directory_entry(directory.root(), relative, entry)?;
            directory.0.push(root);
        } else if metadata.is_file() && pending.is_empty() {
            return Ok(Target::File {
                parent: directory,
                name,
                entry,
            });
        } else {
            return Err(failure(
                "Git directory binding is not a directory or a regular gitfile",
            ));
        }
    }
    Ok(Target::Directory(directory))
}

fn directory_target(target: Target) -> Result<DirectoryPath, String> {
    match target {
        Target::Directory(directory) => Ok(directory),
        Target::File { .. } => Err(failure("Git administrative target is not a directory")),
    }
}

fn path_line(bytes: &[u8]) -> Result<&Path, String> {
    let mut bytes = bytes;
    while bytes
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        bytes = &bytes[..bytes.len() - 1];
    }
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(failure(
            "Git directory binding contains an empty or invalid path",
        ));
    }
    Ok(Path::new(OsStr::from_bytes(bytes)))
}

fn text(bytes: Option<Vec<u8>>) -> Option<String> {
    bytes
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn scalar(
    capture: &mut Capture,
    directory: &Arc<fs_safety::Root>,
    path: &str,
    required: bool,
) -> Result<Option<Vec<u8>>, String> {
    capture.file(directory, path, ReadMode::Full(SCALAR_LIMIT), required)
}

fn oid_value(bytes: &[u8]) -> Result<Oid, String> {
    let value = std::str::from_utf8(bytes).map_err(failure)?.trim();
    Oid::parse_object(value.to_owned()).map_err(failure)
}

fn oid(
    capture: &mut Capture,
    directory: &Arc<fs_safety::Root>,
    path: &str,
    required: bool,
) -> Result<Option<Vec<u8>>, String> {
    let bytes = scalar(capture, directory, path, required)?;
    if let Some(bytes) = bytes.as_ref() {
        oid_value(bytes)?;
    }
    Ok(bytes)
}

fn count(bytes: Option<Vec<u8>>, path: &str) -> Result<u64, String> {
    let bytes = bytes.ok_or_else(|| failure(format!("{path} is missing")))?;
    std::str::from_utf8(&bytes)
        .map_err(failure)?
        .trim()
        .parse::<u64>()
        .map_err(|_| failure(format!("{path} is not a sequencer counter")))
}

fn sequence_kind(bytes: &[u8]) -> Result<GitOperationKind, String> {
    let first = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map(|start| &bytes[start..])
        .unwrap_or_default();
    if first
        .strip_prefix(b"pick")
        .is_some_and(|tail| tail.first().is_some_and(|b| matches!(b, b' ' | b'\t')))
    {
        Ok(GitOperationKind::CherryPick)
    } else if first
        .strip_prefix(b"revert")
        .is_some_and(|tail| tail.first().is_some_and(|b| matches!(b, b' ' | b'\t')))
    {
        Ok(GitOperationKind::Revert)
    } else {
        Err(failure("unrecognized cherry-pick/revert sequencer step"))
    }
}

fn read_state(
    capture: &mut Capture,
    admin: &Arc<fs_safety::Root>,
) -> Result<Option<GitOperationState>, String> {
    let apply = capture.directory(admin, "rebase-apply")?;
    let merge = capture.directory(admin, "rebase-merge")?;
    let sequencer = capture.directory(admin, "sequencer")?;
    let merge_head = capture.file(admin, "MERGE_HEAD", ReadMode::Full(MESSAGE_LIMIT), false)?;
    let cherry_head = oid(capture, admin, "CHERRY_PICK_HEAD", false)?;
    let revert_head = oid(capture, admin, "REVERT_HEAD", false)?;
    oid(capture, admin, "REBASE_HEAD", false)?;

    if apply.is_some() && merge.is_some() {
        return Err(failure("multiple rebase backends are present"));
    }
    let (kind, incoming_ref) = if let Some(directory) = apply.or(merge.clone()) {
        let apply = merge.is_none();
        if apply {
            if scalar(capture, &directory, "applying", false)?.is_some()
                || scalar(capture, &directory, "rebasing", true)?.is_none()
            {
                return Err(failure("git-am recovery is not a supported rebase layout"));
            }
            let next = count(
                scalar(capture, &directory, "next", true)?,
                "rebase-apply/next",
            )?;
            let last = count(
                scalar(capture, &directory, "last", true)?,
                "rebase-apply/last",
            )?;
            if next == 0 || next > last {
                return Err(failure("rebase-apply is between recognized active steps"));
            }
            oid(capture, &directory, "original-commit", false)?;
            oid(capture, &directory, "abort-safety", false)?;
        } else {
            if capture.directory(&directory, "rewritten")?.is_some() {
                return Err(failure(
                    "legacy preserve-merges rebase layout is unsupported",
                ));
            }
            count(
                scalar(capture, &directory, "msgnum", true)?,
                "rebase-merge/msgnum",
            )?;
            capture.file(&directory, "done", ReadMode::Metadata, true)?;
            capture.file(&directory, "git-rebase-todo", ReadMode::Metadata, true)?;
            oid(capture, &directory, "stopped-sha", false)?;
            oid(capture, &directory, "amend", false)?;
        }
        if text(scalar(capture, &directory, "head-name", true)?).is_none() {
            return Err(failure("rebase original branch is empty or unreadable"));
        }
        let incoming = oid(capture, &directory, "onto", true)?;
        if oid(capture, &directory, "orig-head", false)?.is_none() {
            oid(capture, &directory, "head", true)?;
        }
        oid(capture, &directory, "autostash", false)?;
        (GitOperationKind::Rebase, text(incoming))
    } else if let Some(head) = merge_head {
        if sequencer.is_some() || cherry_head.is_some() || revert_head.is_some() {
            return Err(failure(
                "overlapping merge and sequencer state is unsupported",
            ));
        }
        let value = std::str::from_utf8(&head).map_err(failure)?.trim();
        if value.is_empty() {
            return Err(failure("merge incoming object list is empty"));
        }
        for value in value.lines() {
            oid_value(value.as_bytes())?;
        }
        oid(capture, admin, "ORIG_HEAD", true)?;
        oid(capture, admin, "MERGE_AUTOSTASH", false)?;
        (GitOperationKind::Merge, text(Some(head)))
    } else if cherry_head.is_some() || revert_head.is_some() || sequencer.is_some() {
        if cherry_head.is_some() && revert_head.is_some() {
            return Err(failure("both cherry-pick and revert markers are present"));
        }
        let marker_kind = if cherry_head.is_some() {
            Some(GitOperationKind::CherryPick)
        } else if revert_head.is_some() {
            Some(GitOperationKind::Revert)
        } else {
            None
        };
        let kind = if let Some(directory) = sequencer {
            oid(capture, &directory, "head", true)?;
            oid(capture, &directory, "abort-safety", false)?;
            capture.file(&directory, "opts", ReadMode::Metadata, false)?;
            let todo = capture
                .file(&directory, "todo", ReadMode::Prefix(SCALAR_LIMIT), true)?
                .ok_or_else(|| failure("sequencer todo is missing"))?;
            let kind = sequence_kind(&todo)?;
            if marker_kind.is_some_and(|marker| marker != kind) {
                return Err(failure("sequencer kind does not match its current marker"));
            }
            kind
        } else {
            marker_kind.ok_or_else(|| failure("sequencer operation is unidentified"))?
        };
        let incoming = cherry_head.or(revert_head);
        (kind, text(incoming))
    } else {
        return Ok(None);
    };
    Ok(Some(GitOperationState {
        kind,
        incoming_ref,
        message: None,
        witness: None,
        recovery_unavailable: None,
    }))
}

/// Generated messages are optional bounded presentation, never proof inputs.
fn display_text(directory: &Arc<fs_safety::Root>, path: &str) -> Option<String> {
    let mut presentation = Capture::default();
    text(
        presentation
            .file(directory, path, ReadMode::Full(MESSAGE_LIMIT), false)
            .ok()
            .flatten(),
    )
}

fn display_metadata(
    directory: &Arc<fs_safety::Root>,
    path: &str,
) -> Result<Option<Metadata>, String> {
    match fs_safety::retain_entry_beneath(directory, Path::new(path)) {
        Ok(entry) => entry.metadata().map(Some).map_err(failure),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(failure(error)),
    }
}

/// Reading a view must not require enough proof to run a recovery command.
/// Selector observations remain descriptor based; optional draft and incoming
/// text are bounded and may be omitted independently of recovery authority.
fn observe_display(admin: &Arc<fs_safety::Root>) -> Result<Option<GitOperationState>, String> {
    let apply = display_metadata(admin, "rebase-apply")?;
    let (kind, incoming_ref, message) = if apply.is_some_and(|metadata| metadata.is_dir()) {
        let directory = Capture::default()
            .directory(admin, "rebase-apply")?
            .ok_or_else(|| failure("patch application state changed while being read"))?;
        let kind = if display_metadata(&directory, "applying")?.is_some() {
            GitOperationKind::Am
        } else {
            GitOperationKind::Rebase
        };
        (
            kind,
            display_text(&directory, "onto"),
            display_text(&directory, "final-commit").or_else(|| display_text(admin, "MERGE_MSG")),
        )
    } else if display_metadata(admin, "rebase-merge")?.is_some_and(|metadata| metadata.is_dir()) {
        let directory = Capture::default()
            .directory(admin, "rebase-merge")?
            .ok_or_else(|| failure("rebase state changed while being read"))?;
        (
            GitOperationKind::Rebase,
            display_text(&directory, "onto"),
            display_text(&directory, "message").or_else(|| display_text(admin, "MERGE_MSG")),
        )
    } else if display_metadata(admin, "MERGE_HEAD")?.is_some() {
        (
            GitOperationKind::Merge,
            display_text(admin, "MERGE_HEAD"),
            display_text(admin, "MERGE_MSG"),
        )
    } else if display_metadata(admin, "CHERRY_PICK_HEAD")?.is_some() {
        (
            GitOperationKind::CherryPick,
            display_text(admin, "CHERRY_PICK_HEAD"),
            display_text(admin, "MERGE_MSG"),
        )
    } else if display_metadata(admin, "REVERT_HEAD")?.is_some() {
        (
            GitOperationKind::Revert,
            display_text(admin, "REVERT_HEAD"),
            display_text(admin, "MERGE_MSG"),
        )
    } else if display_metadata(admin, "sequencer")?.is_some_and(|metadata| metadata.is_dir()) {
        let directory = Capture::default()
            .directory(admin, "sequencer")?
            .ok_or_else(|| failure("sequencer state changed while being read"))?;
        let todo = Capture::default()
            .file(&directory, "todo", ReadMode::Prefix(SCALAR_LIMIT), true)?
            .ok_or_else(|| failure("sequencer operation cannot be identified"))?;
        (
            sequence_kind(&todo)?,
            None,
            display_text(admin, "MERGE_MSG"),
        )
    } else {
        return Ok(None);
    };
    Ok(Some(GitOperationState {
        kind,
        incoming_ref,
        message,
        witness: None,
        recovery_unavailable: None,
    }))
}

fn capture_head(
    capture: &mut Capture,
    admin: &Arc<fs_safety::Root>,
    common: &Arc<fs_safety::Root>,
) -> Result<(), String> {
    // The refs backend itself must remain known. A reftable repository needs a
    // different retained-ref contract rather than a files-backend guess.
    if capture.directory(common, "reftable")?.is_some() {
        return Err(failure(
            "reftable operation recovery has not been qualified",
        ));
    }
    capture.file(common, "packed-refs", ReadMode::Metadata, false)?;
    let mut value =
        scalar(capture, admin, "HEAD", true)?.ok_or_else(|| failure("HEAD is missing"))?;
    for _ in 0..8 {
        let value_text = std::str::from_utf8(&value).map_err(failure)?.trim();
        if let Some(reference) = value_text.strip_prefix("ref: ") {
            if !reference.starts_with("refs/heads/")
                || Path::new(reference)
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_)))
            {
                return Err(failure("HEAD uses an unsupported symbolic reference"));
            }
            match scalar(capture, common, reference, false)? {
                Some(next) => value = next,
                // Packed or unborn branch. Git's own HEAD check below supplies
                // its value; packed-refs identity is retained above.
                None => return Ok(()),
            }
        } else {
            oid_value(&value)?;
            return Ok(());
        }
    }
    Err(failure(
        "HEAD symbolic reference chain exceeds its read limit",
    ))
}

#[derive(Debug)]
struct Witness {
    kind: GitOperationKind,
    worktree_argument: PathBuf,
    worktree: Arc<fs_safety::Root>,
    admin: Arc<fs_safety::Root>,
    common: Arc<fs_safety::Root>,
    head: Option<Oid>,
    capture: Capture,
    identity: Vec<ObservationIdentity>,
}

/// Opaque, local-only authority for the original operation and sequencer step.
/// Only a bound service read can create one; cloning retains the same handles.
#[derive(Debug, Clone)]
pub struct GitOperationWitness(Arc<Witness>);

impl PartialEq for GitOperationWitness {
    fn eq(&self, other: &Self) -> bool {
        self.0.kind == other.0.kind
            && self.0.head == other.0.head
            && self.0.worktree_argument == other.0.worktree_argument
            && self.0.worktree.path() == other.0.worktree.path()
            && self.0.admin.path() == other.0.admin.path()
            && self.0.common.path() == other.0.common.path()
            && self.0.identity == other.0.identity
    }
}
impl Eq for GitOperationWitness {}

impl GitOperationWitness {
    pub(super) fn kind(&self) -> GitOperationKind {
        self.0.kind
    }

    pub(super) fn check_git_context<E: ProcessExecutor>(
        &self,
        service: &RepositoryService<E>,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        self.check_git_context_for_capture(service, worktree, cancellation)
            .map_err(|error| error.to_string())
    }

    fn check_git_context_for_capture<E: ProcessExecutor>(
        &self,
        service: &RepositoryService<E>,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), ProofError> {
        cancelled(cancellation)?;
        let output = service
            .run_checked(&service.worktree_root_command(worktree), cancellation)
            .map_err(RepositoryError::from)
            .map_err(ProofError::from)?;
        let effective = parse_path_line(&output.stdout, "effective worktree").map_err(failure)?;
        let effective = fs::canonicalize(effective).map_err(failure)?;
        let effective = fs_safety::Root::open(&effective).map_err(failure)?;
        if directory_identity(&effective.metadata().map_err(failure)?)
            != directory_identity(&self.0.worktree.metadata().map_err(failure)?)
        {
            return Err(failure("Git now selects a different effective worktree").into());
        }
        if service
            .git_dir(worktree, cancellation)
            .map_err(ProofError::from)?
            != self.0.admin.path()
            || service
                .common_dir(worktree, cancellation)
                .map_err(ProofError::from)?
                != self.0.common.path()
            || service
                .resolve_head(worktree, cancellation)
                .map_err(ProofError::from)?
                != self.0.head
        {
            return Err(failure("HEAD or the administrative repository binding changed").into());
        }
        Ok(())
    }

    pub(super) fn pin_worktree(&self, command: CommandSpec) -> CommandSpec {
        // CommandSpec applies explicit environment values after removals, so
        // this overrides git_base's inherited GIT_WORK_TREE removal as well as
        // later core.worktree configuration. No name lookup occurs here.
        command.env("GIT_WORK_TREE", self.0.worktree.path().as_os_str())
    }

    fn generated_message(&self) -> Option<String> {
        if self.0.kind == GitOperationKind::Rebase {
            for (directory, message) in [
                ("rebase-apply", "final-commit"),
                ("rebase-merge", "message"),
            ] {
                if let Some(root) = self.0.capture.retained_directory(&self.0.admin, directory)
                    && let Some(message) = display_text(root, message)
                {
                    return Some(message);
                }
            }
        }
        display_text(&self.0.admin, "MERGE_MSG")
    }

    pub(super) fn revalidate(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        cancelled(cancellation)?;
        if worktree != self.0.worktree_argument {
            return Err(failure("the request names a different original worktree"));
        }
        self.0.capture.validate(cancellation)
    }
}

pub(super) fn display_state(directory: &Path) -> Result<Option<GitOperationState>, String> {
    let directory = fs::canonicalize(directory).map_err(failure)?;
    let admin = Arc::new(fs_safety::Root::open(&directory).map_err(failure)?);
    observe_display(&admin)
}

pub(super) fn capture_state<E: ProcessExecutor>(
    service: &RepositoryService<E>,
    worktree: &Path,
    directory: &Path,
    cancellation: &CancellationToken,
) -> Result<Option<GitOperationState>, String> {
    cancelled(cancellation)?;
    let Some(mut display) = display_state(directory)? else {
        cancelled(cancellation)?;
        return Ok(None);
    };
    cancelled(cancellation)?;
    let proof = capture_strict_state(service, worktree, directory, cancellation);
    // Cancellation must not be converted into an apparently usable display
    // result. Other proof failures cannot erase safely observed Changes data.
    cancelled(cancellation)?;
    match proof {
        Err(ProofError::Cancelled(reason)) => Err(reason),
        Ok(Some(proved)) => Ok(Some(proved)),
        Ok(None) | Err(_) => {
            display.witness = None;
            display.recovery_unavailable = Some(format!(
                "{} recovery is unavailable because the current Git state could not be verified. Reload Changes, or inspect and finish the operation in a terminal.",
                display.kind.label(),
            ));
            Ok(Some(display))
        }
    }
}

fn capture_strict_state<E: ProcessExecutor>(
    service: &RepositoryService<E>,
    worktree: &Path,
    directory: &Path,
    cancellation: &CancellationToken,
) -> Result<Option<GitOperationState>, ProofError> {
    cancelled(cancellation)?;
    if !worktree.is_absolute() {
        return Err(failure("the original worktree path is not absolute").into());
    }
    let mut capture = Capture::default();
    let filesystem_root = Arc::new(fs_safety::Root::open(Path::new("/")).map_err(failure)?);
    let base = DirectoryPath(vec![filesystem_root]);
    let worktree_path = directory_target(resolve(&mut capture, base, worktree, cancellation)?)?;
    let worktree_root = worktree_path.root().clone();
    // Git resolves a relative gitdir value from the requested gitfile parent,
    // even when the gitfile itself is reached through a symbolic link.
    let admin_path = match resolve(
        &mut capture,
        worktree_path.clone(),
        Path::new(".git"),
        cancellation,
    )? {
        Target::Directory(directory) => directory,
        Target::File {
            parent,
            name,
            entry,
        } => {
            let bytes = capture
                .file_entry(
                    parent.root(),
                    Path::new(&name),
                    entry,
                    ReadMode::Full(SCALAR_LIMIT),
                )?
                .ok_or_else(|| failure("Git binding is unreadable"))?;
            let value = bytes
                .strip_prefix(b"gitdir: ")
                .ok_or_else(|| failure("Git binding is not a recognized gitfile"))?;
            directory_target(resolve(
                &mut capture,
                worktree_path,
                path_line(value)?,
                cancellation,
            )?)?
        }
    };
    let admin = admin_path.root().clone();
    let expected_admin = fs::canonicalize(directory).map_err(failure)?;
    if admin.path() != expected_admin {
        return Err(failure("Git administrative binding changed during capture").into());
    }
    let common_path = if capture
        .optional_entry(&admin, Path::new("commondir"))?
        .is_some()
    {
        match resolve(
            &mut capture,
            admin_path.clone(),
            Path::new("commondir"),
            cancellation,
        )? {
            Target::File {
                parent,
                name,
                entry,
            } => {
                let bytes = capture
                    .file_entry(
                        parent.root(),
                        Path::new(&name),
                        entry,
                        ReadMode::Full(SCALAR_LIMIT),
                    )?
                    .ok_or_else(|| failure("Git common-directory binding is unreadable"))?;
                directory_target(resolve(
                    &mut capture,
                    admin_path,
                    path_line(&bytes)?,
                    cancellation,
                )?)?
            }
            Target::Directory(_) => {
                return Err(failure("commondir is not a regular binding file").into());
            }
        }
    } else {
        admin_path
    };
    let common = common_path.root().clone();
    let Some(mut state) = read_state(&mut capture, &admin)? else {
        return Err(failure("the operation ended while its authority was captured").into());
    };
    capture_head(&mut capture, &admin, &common)?;
    let head = service
        .resolve_head(worktree, cancellation)
        .map_err(ProofError::from)?;
    if head.is_none() {
        return Err(failure("operation recovery requires a resolvable original HEAD").into());
    }
    let identity = capture
        .observations
        .iter()
        .map(|observation| observation.identity.clone())
        .collect();
    let witness = GitOperationWitness(Arc::new(Witness {
        kind: state.kind,
        worktree_argument: worktree.to_owned(),
        worktree: worktree_root,
        admin,
        common,
        head,
        capture,
        identity,
    }));
    witness.check_git_context_for_capture(service, worktree, cancellation)?;
    // Read optional text through the original retained operation directory,
    // after its step was captured and before validating that same step again.
    // A proof for a later step must never inherit an earlier display's draft.
    state.message = witness.generated_message();
    witness.revalidate(worktree, cancellation)?;
    state.witness = Some(witness);
    Ok(Some(state))
}
