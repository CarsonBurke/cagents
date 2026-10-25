//! Fixed-file, destination-only cache publication. No provider or Store I/O.
use std::{
    fmt,
    fs::{File, Metadata, Permissions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

use super::{PhysicalIdentity, PublicationReceipt, SourceObservation};
use crate::{
    account::home::NewHomeCreationReceipt,
    agent::Provider,
    fs_safety::{self, Root},
};

pub(crate) const MAX_AUTH_BYTES: usize = super::MAX_CACHE_BYTES as usize;
const AUTH_FILE: &str = "auth.json";

/// Fixed codes deliberately exclude parser, path and operating-system text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    InvalidPath,
    UnsafeSource,
    SourceChanged,
    ReadFailed,
    TooLarge,
    InvalidFormat,
    UnsupportedFormat,
    UnsafeDestination,
    PrepareFailed,
    InspectFailed,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidPath => "Choose an absolute file path without symbolic links",
            Self::UnsafeSource => {
                "The staged cache must be an owned, private, singly linked regular file"
            }
            Self::SourceChanged => "The staged cache changed; inspect it again",
            Self::ReadFailed => "The staged cache could not be read",
            Self::TooLarge => "The staged cache exceeds the import size limit",
            Self::InvalidFormat => "The staged cache is not valid supported JSON",
            Self::UnsupportedFormat => "Only a managed ChatGPT file cache is supported",
            Self::UnsafeDestination => "The newly created account directory changed",
            Self::PrepareFailed => "The private cache could not be prepared",
            Self::InspectFailed => "The original cache publication could not be inspected",
        })
    }
}
impl std::error::Error for Error {}

pub(crate) struct Snapshot {
    bytes: Vec<u8>,
}
impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Snapshot([redacted])")
    }
}

/// Constructed only while an actual fresh-home creation receipt is borrowed.
/// Owns the original descriptors after that short-lived initializer returns.
pub(crate) struct Prepared {
    anonymous: File,
    root: Root,
    directory: File,
    parent: File,
    home: PathBuf,
    parent_path: PathBuf,
    receipt: PublicationReceipt,
}
impl fmt::Debug for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Prepared([redacted])")
    }
}
impl Prepared {
    pub(crate) fn receipt(&self) -> PublicationReceipt {
        self.receipt.clone()
    }

    fn revalidate(&self) -> Result<(), Error> {
        let held = self
            .directory
            .metadata()
            .map_err(|_| Error::UnsafeDestination)?;
        let parent = self
            .parent
            .metadata()
            .map_err(|_| Error::UnsafeDestination)?;
        let named_parent =
            absolute_directory(&self.parent_path).map_err(|_| Error::UnsafeDestination)?;
        let named = absolute_directory(&self.home).map_err(|_| Error::UnsafeDestination)?;
        let root = self.root.metadata().map_err(|_| Error::UnsafeDestination)?;
        let named_metadata = named.metadata().map_err(|_| Error::UnsafeDestination)?;
        let named_parent_metadata = named_parent
            .metadata()
            .map_err(|_| Error::UnsafeDestination)?;
        for metadata in [
            &held,
            &parent,
            &root,
            &named_metadata,
            &named_parent_metadata,
        ] {
            require_private_directory(metadata)?;
        }
        if identity(&held) != self.receipt.home
            || identity(&root) != self.receipt.home
            || identity(&named_metadata) != self.receipt.home
            || identity(&named_parent_metadata) != identity(&parent)
        {
            return Err(Error::UnsafeDestination);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublishFailure {
    /// No link was attempted by this invocation; the anonymous inode drops.
    NotPublished(Error),
    /// A link was attempted. Keep the original operation and inspect it.
    Uncertain,
}
impl fmt::Display for PublishFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPublished(error) => write!(f, "{error}"),
            Self::Uncertain => {
                f.write_str("Cache publication is uncertain; check the original import")
            }
        }
    }
}
impl std::error::Error for PublishFailure {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationState {
    Missing,
    /// Physical entry matches the receipt; not an authentication assertion.
    Present,
    Different,
}

pub(crate) fn inspect(path: &Path) -> Result<SourceObservation, Error> {
    read_snapshot(path, None, || {}).map(|(_, observation)| observation)
}

pub(crate) fn snapshot(path: &Path, expected: &SourceObservation) -> Result<Snapshot, Error> {
    if expected.validate().is_err() || expected.path != path {
        return Err(Error::SourceChanged);
    }
    read_snapshot(path, Some(expected), || {}).map(|(snapshot, _)| snapshot)
}

fn read_snapshot(
    path: &Path,
    expected: Option<&SourceObservation>,
    after_read: impl FnOnce(),
) -> Result<(Snapshot, SourceObservation), Error> {
    let (parent, mut file, before) = open_source(path)?;
    if expected.is_some_and(|expected| expected != &before) {
        return Err(Error::SourceChanged);
    }
    let mut bytes = Vec::with_capacity(before.size as usize);
    Read::by_ref(&mut file)
        .take((MAX_AUTH_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::ReadFailed)?;
    if bytes.len() > MAX_AUTH_BYTES {
        return Err(Error::TooLarge);
    }
    after_read();
    let after = source_observation(path, &parent, &file)?;
    let (_, _, named) = open_source(path)?;
    if before != after || before != named || bytes.len() as u64 != before.size {
        return Err(Error::SourceChanged);
    }
    validate_format(&bytes)?;
    Ok((Snapshot { bytes }, before))
}

fn absolute_relative(path: &Path) -> Result<&Path, Error> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(Error::InvalidPath);
    }
    let relative = path.strip_prefix("/").map_err(|_| Error::InvalidPath)?;
    if relative.as_os_str().is_empty() {
        return Err(Error::InvalidPath);
    }
    Ok(relative)
}

fn absolute_directory(path: &Path) -> Result<Root, Error> {
    let slash = Root::open(Path::new("/")).map_err(|_| Error::InvalidPath)?;
    fs_safety::open_directory_beneath(&slash, absolute_relative(path)?)
        .map_err(|_| Error::InvalidPath)
}

fn open_source(path: &Path) -> Result<(Root, File, SourceObservation), Error> {
    let slash = Root::open(Path::new("/")).map_err(|_| Error::ReadFailed)?;
    let (parent, name) = fs_safety::open_parent_beneath(&slash, absolute_relative(path)?)
        .map_err(|_| Error::UnsafeSource)?;
    let file =
        fs_safety::open_beneath(&parent, Path::new(&name)).map_err(|_| Error::UnsafeSource)?;
    let observation = source_observation(path, &parent, &file)?;
    Ok((parent, file, observation))
}

fn source_observation(path: &Path, parent: &Root, file: &File) -> Result<SourceObservation, Error> {
    let metadata = file.metadata().map_err(|_| Error::ReadFailed)?;
    if !private_regular(&metadata) || metadata.len() == 0 {
        return Err(Error::UnsafeSource);
    }
    if metadata.len() > MAX_AUTH_BYTES as u64 {
        return Err(Error::TooLarge);
    }
    Ok(SourceObservation {
        path: path.to_path_buf(),
        parent: identity(&parent.metadata().map_err(|_| Error::ReadFailed)?),
        file: identity(&metadata),
        size: metadata.len(),
        mtime_seconds: metadata.mtime(),
        mtime_nanoseconds: metadata.mtime_nsec(),
        ctime_seconds: metadata.ctime(),
        ctime_nanoseconds: metadata.ctime_nsec(),
    })
}

fn private_regular(metadata: &Metadata) -> bool {
    metadata.is_file()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0
        && metadata.nlink() == 1
}

fn require_private_directory(metadata: &Metadata) -> Result<(), Error> {
    if metadata.is_dir()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0
    {
        Ok(())
    } else {
        Err(Error::UnsafeDestination)
    }
}

fn identity(metadata: &Metadata) -> PhysicalIdentity {
    PhysicalIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn validate_format(bytes: &[u8]) -> Result<(), Error> {
    #[derive(serde::Deserialize)]
    struct Cache {
        #[serde(default)]
        auth_mode: Option<String>,
        #[serde(default, rename = "OPENAI_API_KEY")]
        api_key: Option<String>,
        #[serde(default)]
        tokens: Option<Tokens>,
    }
    #[derive(serde::Deserialize)]
    struct Tokens {
        #[serde(default)]
        refresh_token: Option<String>,
    }
    let cache: Cache = serde_json::from_slice(bytes).map_err(|_| Error::InvalidFormat)?;
    if cache.auth_mode.as_deref() != Some("chatgpt")
        || cache.api_key.as_ref().is_some_and(|key| !key.is_empty())
        || cache
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.refresh_token.as_deref())
            .is_none_or(|refresh| refresh.trim().is_empty())
    {
        return Err(Error::UnsupportedFormat);
    }
    Ok(())
}

pub(crate) fn prepare(
    snapshot: Snapshot,
    created: &NewHomeCreationReceipt,
) -> Result<Prepared, Error> {
    prepare_with_mode(snapshot, created, 0o600)
}

fn prepare_with_mode(
    snapshot: Snapshot,
    created: &NewHomeCreationReceipt,
    initial_mode: u32,
) -> Result<Prepared, Error> {
    if created.provider() != Provider::Codex || created.key().is_nil() {
        return Err(Error::UnsafeDestination);
    }
    created.revalidate().map_err(|_| Error::UnsafeDestination)?;
    let root = absolute_directory(created.home()).map_err(|_| Error::UnsafeDestination)?;
    let directory = created
        .directory()
        .try_clone()
        .map_err(|_| Error::PrepareFailed)?;
    let parent = created
        .parent()
        .try_clone()
        .map_err(|_| Error::PrepareFailed)?;
    let home = identity(&directory.metadata().map_err(|_| Error::PrepareFailed)?);
    if identity(&root.metadata().map_err(|_| Error::PrepareFailed)?) != home {
        return Err(Error::UnsafeDestination);
    }
    let mut anonymous =
        fs_safety::create_anonymous_in(&root, initial_mode).map_err(|_| Error::PrepareFailed)?;
    anonymous
        .set_permissions(Permissions::from_mode(0o600))
        .map_err(|_| Error::PrepareFailed)?;
    anonymous
        .write_all(&snapshot.bytes)
        .map_err(|_| Error::PrepareFailed)?;
    anonymous.sync_all().map_err(|_| Error::PrepareFailed)?;
    let metadata = anonymous.metadata().map_err(|_| Error::PrepareFailed)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 0
        || metadata.mode() & 0o7777 != 0o600
        || metadata.len() != snapshot.bytes.len() as u64
    {
        return Err(Error::PrepareFailed);
    }
    let prepared = Prepared {
        anonymous,
        root,
        directory,
        parent,
        home: created.home().to_path_buf(),
        parent_path: created
            .home()
            .parent()
            .ok_or(Error::UnsafeDestination)?
            .to_path_buf(),
        receipt: PublicationReceipt {
            operation: created.key(),
            home,
            file: identity(&metadata),
            size: metadata.len(),
        },
    };
    created.revalidate().map_err(|_| Error::UnsafeDestination)?;
    prepared.revalidate()?;
    Ok(prepared)
}

/// Caller must first persist the exact receipt and confirm pending Store commit.
pub(crate) fn publish(prepared: Prepared) -> Result<PublicationReceipt, PublishFailure> {
    publish_with(prepared, fs_safety::sync_directory)
}

fn publish_with(
    prepared: Prepared,
    sync: impl FnOnce(&Root) -> std::io::Result<()>,
) -> Result<PublicationReceipt, PublishFailure> {
    prepared
        .revalidate()
        .map_err(PublishFailure::NotPublished)?;
    let metadata = prepared
        .anonymous
        .metadata()
        .map_err(|_| PublishFailure::NotPublished(Error::PrepareFailed))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 0
        || metadata.mode() & 0o7777 != 0o600
        || identity(&metadata) != prepared.receipt.file
        || metadata.len() != prepared.receipt.size
    {
        return Err(PublishFailure::NotPublished(Error::PrepareFailed));
    }
    // Once link is attempted, even an error is conservatively reconciled by
    // original-operation status. Nothing ever unlinks an existing entry.
    fs_safety::link_anonymous_new(&prepared.root, &prepared.anonymous, AUTH_FILE.as_ref())
        .map_err(|_| PublishFailure::Uncertain)?;
    sync(&prepared.root).map_err(|_| PublishFailure::Uncertain)?;
    prepared
        .revalidate()
        .map_err(|_| PublishFailure::Uncertain)?;
    if publication_state(&prepared.root, &prepared.receipt)
        .map_err(|_| PublishFailure::Uncertain)?
        != PublicationState::Present
    {
        return Err(PublishFailure::Uncertain);
    }
    Ok(prepared.receipt)
}

/// Caller supplies its revalidated managed-home root. Read-only status is
/// physical presence evidence, never permission to seed or register.
pub(crate) fn publication_state(
    root: &Root,
    receipt: &PublicationReceipt,
) -> Result<PublicationState, Error> {
    publication_state_with(root, receipt, || {})
}

fn publication_state_with(
    root: &Root,
    receipt: &PublicationReceipt,
    after_open: impl FnOnce(),
) -> Result<PublicationState, Error> {
    let directory = root.metadata().map_err(|_| Error::InspectFailed)?;
    if receipt.validate().is_err() {
        return Err(Error::InspectFailed);
    }
    if root.path().file_name() != Some(std::ffi::OsStr::new(&receipt.operation.to_string()))
        || identity(&directory) != receipt.home
        || require_private_directory(&directory).is_err()
    {
        return Ok(PublicationState::Different);
    }
    if named_home(root, receipt)?.is_none() {
        return Ok(PublicationState::Different);
    }
    let file = match fs_safety::open_beneath(root, Path::new(AUTH_FILE)) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => return Ok(PublicationState::Missing),
        Err(_) => return Ok(PublicationState::Different),
    };
    after_open();
    let metadata = file.metadata().map_err(|_| Error::InspectFailed)?;
    // Held original descriptors alone do not prove what auth.json now names.
    let Some(named_home) = named_home(root, receipt)? else {
        return Ok(PublicationState::Different);
    };
    let named = match fs_safety::open_beneath(&named_home, Path::new(AUTH_FILE)) {
        Ok(named) => named.metadata().map_err(|_| Error::InspectFailed)?,
        Err(_) => return Ok(PublicationState::Different),
    };
    Ok(
        if private_regular(&metadata)
            && metadata.mode() & 0o7777 == 0o600
            && identity(&metadata) == receipt.file
            && metadata.len() == receipt.size
            && private_regular(&named)
            && named.mode() & 0o7777 == 0o600
            && identity(&named) == receipt.file
            && named.len() == receipt.size
        {
            PublicationState::Present
        } else {
            PublicationState::Different
        },
    )
}

fn named_home(root: &Root, receipt: &PublicationReceipt) -> Result<Option<Root>, Error> {
    let named = match absolute_directory(root.path()) {
        Ok(home) => home,
        Err(_) => return Ok(None),
    };
    let metadata = named.metadata().map_err(|_| Error::InspectFailed)?;
    Ok(
        if identity(&metadata) == receipt.home && require_private_directory(&metadata).is_ok() {
            Some(named)
        } else {
            None
        },
    )
}

#[cfg(test)]
#[path = "cache/tests.rs"]
mod tests;
