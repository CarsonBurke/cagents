//! A source receipt can only be captured from the actual Store's opaque,
//! borrowed read seed. It contains no account/session/peer admission and is NOT
//! an exported read grant. Index metadata cannot recreate this constructor.
use super::{Error, Result, CacheIdentity, FileIdentity, SourceReadProofData, UnixPath,
    fs::Directory, lifetime::{self, RevocationProbe}, sqlite};
use std::{ffi::OsStr, fs::File, os::unix::fs::MetadataExt, time::Instant};

pub(crate) struct StoreSourceReceipt {
    pub(super) directory: Directory,
    pub(super) database: File,
    pub(super) probe: RevocationProbe,
    pub(super) cache: CacheIdentity,
    pub(super) cache_root: UnixPath,
    pub(super) cache_directory: UnixPath,
    pub(super) source: SourceReadProofData,
}
fn id(file: &File) -> Result<FileIdentity> {
    let metadata = file.metadata()?;
    Ok(FileIdentity { device: metadata.dev(), inode: metadata.ino() })
}
impl StoreSourceReceipt {
    /// The Store calls this INSIDE with_source_read, then explicitly releases
    /// its snapshot before returning the owned receipt. No N/F/G/flock wait is
    /// allowed while borrowing that Store read snapshot. Receipt capture alone
    /// does not extend provider/helper lifetime or retain its EX owner lease.
    pub(crate) fn capture(seed: &crate::chat::runtime::StoreExportSeed<'_>, expires: Instant) -> Result<Self> {
        if Instant::now() >= expires { return Err(Error::Deadline); }
        let directory = Directory::open(seed.directory())?;
        let path = directory.current_path()?;
        let root = Directory::open(path.parent().ok_or(Error::Incomplete("Store cache has no physical parent"))?)?;
        let named_owner = directory.open_file(OsStr::new("lease.lock"), false)?;
        if id(&named_owner)? != id(seed.owner_lease())? {
            return Err(Error::Incomplete("Store cache owner lease changed before source capture"));
        }
        let database = directory.open_file(OsStr::new("transcript.sqlite"), false)?;
        let source = SourceReadProofData { source_incarnation: seed.source_incarnation(),
            raw_owner_nonce: seed.raw_owner_nonce(), raw_schema: 1, reset_schema: 1, text_schema: 1 };
        let probe = seed.source_revocation().probe()?;
        if probe.incarnation() != source.source_incarnation { return Err(Error::Revoked); }
        probe.check()?;
        sqlite::guard(seed.connection(), &source)?;
        sqlite::verify_open_connection(seed.connection(), &directory, &database, true)?;
        let (nonce, readers_lock) = lifetime::describe(&directory)?;
        let cache = CacheIdentity { nonce, root: root.identity(), directory: directory.identity(),
            database: id(&database)?, readers_lock };
        let cache_root = UnixPath::capture(&root.current_path()?)?;
        let cache_directory = UnixPath::capture(&path)?;
        probe.check()?;
        if Instant::now() >= expires { return Err(Error::Deadline); }
        Ok(Self { directory, database, probe, cache, cache_root, cache_directory, source })
    }
}
