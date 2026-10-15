//! Durable-before-export membership across independent installation roots.
//! Lock order: physical ancestors -> namespace N -> shared metadata F -> G.
//! Registration is census evidence; it is not an ActualStore read grant.
use super::{Error, Result, FileIdentity, UnixPath, BindingData, fs::{Directory, Locked, Mode},
    types::{BindingRecord, Phase, VERSION, MAX_RECORDS, MAX_TOTAL}};
use crate::account::home::NewHomeCreationReceipt;
use serde::{Deserialize, Serialize};
use std::{ffi::OsStr, fs::{File, OpenOptions}, os::{fd::AsRawFd, unix::fs::{MetadataExt, OpenOptionsExt}},
    path::{Path, PathBuf}, sync::Arc, time::{Duration, Instant}};
use uuid::Uuid;

const POINTER: &str = ".cagents-transcript-exports-v1";
const AUTHORITIES: &str = ".cagents-transcript-export-authorities-v1";
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootHeader { format: u32, directory: FileIdentity }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pointer { format: u32, authority: Uuid, directory: FileIdentity, home: FileIdentity, locator: UnixPath }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FederationHeader {
    format: u32, authority: Uuid, directory: FileIdentity, home: FileIdentity,
    home_locator: UnixPath, ancestors: Vec<FileIdentity>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexHeader { format: u32, installation: Uuid, directory: FileIdentity }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    format: u32, phase: Phase, commit: Uuid, association: Uuid,
    installation: Uuid, index: FileIdentity, index_locator: UnixPath,
}

struct Ancestors { _files: Vec<File>, identities: Vec<FileIdentity> }
fn file_identity(file: &File) -> Result<FileIdentity> {
    let m = file.metadata()?;
    Ok(FileIdentity { device: m.dev(), inode: m.ino() })
}
fn lock_file(file: &File, exclusive: bool, deadline: Instant) -> Result<()> {
    loop {
        if Instant::now() >= deadline { return Err(Error::Deadline); }
        let mode = if exclusive { libc::LOCK_EX } else { libc::LOCK_SH };
        if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } == 0 {
            return if Instant::now() < deadline { Ok(()) } else { Err(Error::Deadline) };
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::WouldBlock && e.kind() != std::io::ErrorKind::Interrupted { return Err(e.into()); }
        std::thread::sleep(Duration::from_millis(2));
    }
}
impl Ancestors {
    fn acquire(home: &Path, expected: FileIdentity, exclusive_home: bool, deadline: Instant) -> Result<Self> {
        let canonical = std::fs::canonicalize(home)?;
        if canonical != home { return Err(Error::Incomplete("Export home locator changed or is not canonical")); }
        let paths = canonical.ancestors().map(Path::to_path_buf).collect::<Vec<_>>();
        if paths.len() > 64 { return Err(Error::Capacity); }
        let mut result = Self { _files: Vec::new(), identities: Vec::new() };
        for path in paths.iter().rev() {
            let file = OpenOptions::new().read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
            let id = file_identity(&file)?;
            if result.identities.contains(&id) { return Err(Error::Unsupported("Ambiguous physical export ancestry")); }
            lock_file(&file, exclusive_home && path == &canonical, deadline)?;
            if file_identity(&file)? != id || identity_at(path)? != id {
                return Err(Error::Incomplete("Export home ancestry changed during admission"));
            }
            result.identities.push(id);
            result._files.push(file);
        }
        if result.identities.last() != Some(&expected) {
            return Err(Error::Incomplete("Export home is not the captured physical account"));
        }
        Ok(result)
    }
}
fn identity_at(path: &Path) -> Result<FileIdentity> {
    let m = std::fs::symlink_metadata(path)?;
    if !m.is_dir() { return Err(Error::Incomplete("Export locator is not a physical directory")); }
    Ok(FileIdentity { device: m.dev(), inode: m.ino() })
}
fn record_name(id: Uuid) -> String { format!("{id}.json") }
fn record_id(name: &OsStr) -> Result<Uuid> {
    let text = name.to_str().ok_or(Error::Incomplete("Unknown export record filename"))?;
    let id = text.strip_suffix(".json").and_then(|s| Uuid::parse_str(s).ok())
        .ok_or(Error::Incomplete("Unknown export record filename"))?;
    if id.is_nil() || text != record_name(id) { return Err(Error::Incomplete("Noncanonical export record filename")); }
    Ok(id)
}
fn shape(dir: &Directory, expected: &[&str]) -> Result<()> {
    let names = dir.names()?;
    if names.len() != expected.len() || names.iter().any(|n| !expected.iter().any(|e| n == OsStr::new(e))) {
        return Err(Error::Incomplete("Export metadata directory has incomplete or unknown contents"));
    }
    Ok(())
}

struct Federation { dir: Directory, members: Directory, header: FederationHeader }
#[derive(Clone)]
pub(crate) struct Namespace(Arc<Federation>);
pub(crate) struct NamespaceRead { namespace: Namespace, _ancestors: Ancestors, _namespace: Locked }
/// No Clone/Deserialize/from_path constructor: this borrows real namespace
/// admission through publication, but still grants no payload access itself.
pub(crate) struct RegisteredBinding {
    admission: NamespaceRead,
    record: BindingRecord,
    index: IndexHeader,
}
impl RegisteredBinding {
    pub(crate) fn metadata(&self) -> &BindingData { &self.record.binding }
    pub(crate) fn commit(&self) -> Uuid { self.record.commit }
}

impl Namespace {
    /// Only the private successful-mkdir receipt can bootstrap an empty root.
    /// No existing/empty-directory observation is a legacy inventory receipt.
    pub(crate) fn bootstrap_created_home(created: &NewHomeCreationReceipt, deadline: Instant) -> Result<Self> {
        created.revalidate()?;
        let home = Directory::from_file(created.directory().try_clone()?)?;
        let ancestors = Ancestors::acquire(created.home(), home.identity(), true, deadline)?;
        created.revalidate()?;
        let parent = Directory::from_file(created.parent().try_clone()?)?;
        let roots = match parent.create_dir(OsStr::new(AUTHORITIES)) {
            Ok(root) => {
                root.create_lock(OsStr::new(".metadata.lock"))?;
                root.atomic(OsStr::new("root.json"), &RootHeader { format: VERSION, directory: root.identity() }, false)?;
                root
            }
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => parent.open_dir(OsStr::new(AUTHORITIES))?,
            Err(error) => return Err(error),
        };
        let (root, _): (RootHeader, _) = roots.read(OsStr::new("root.json"))?;
        if root.format != VERSION || root.directory != roots.identity() { return Err(Error::Unsupported("Export authority root identity/version")); }
        // This is unpublished fresh-home creation: no existing namespace lock
        // is being awaited while holding this shared-parent metadata lock.
        let root_lock = roots.lock(OsStr::new(".metadata.lock"), Mode::Exclusive, deadline)?;
        let names = roots.names()?;
        let mut count = 0;
        for name in names {
            if name == OsStr::new("root.json") || name == OsStr::new(".metadata.lock") { continue; }
            if !name.to_str().and_then(|n| Uuid::parse_str(n).ok())
                .is_some_and(|id| !id.is_nil() && name == OsStr::new(&id.to_string())) {
                return Err(Error::Incomplete("Unknown export authority root entry"));
            }
            roots.open_dir(&name)?;
            count += 1;
        }
        if count >= MAX_RECORDS { return Err(Error::Capacity); }
        let authority = Uuid::new_v4();
        let dir = roots.create_dir(OsStr::new(&authority.to_string()))?;
        dir.create_lock(OsStr::new(".namespace.lock"))?;
        dir.create_lock(OsStr::new(".catalog.lock"))?;
        let members = dir.create_dir(OsStr::new("members"))?;
        let header = FederationHeader { format: VERSION, authority, directory: dir.identity(),
            home: home.identity(), home_locator: UnixPath::capture(created.home())?, ancestors: ancestors.identities.clone() };
        dir.atomic(OsStr::new("header.json"), &header, false)?;
        drop(root_lock);
        let pointer = Pointer { format: VERSION, authority, directory: dir.identity(), home: home.identity(),
            locator: UnixPath::capture(&dir.current_path()?)? };
        home.atomic(OsStr::new(POINTER), &pointer, false)?;
        created.revalidate()?;
        drop(ancestors);
        Ok(Self(Arc::new(Federation { dir, members, header })))
    }

    pub(crate) fn open_existing(home_path: &Path, expected: FileIdentity, deadline: Instant) -> Result<Self> {
        let ancestors = Ancestors::acquire(home_path, expected, false, deadline)?;
        let home = Directory::open(home_path)?;
        let (pointer, _): (Pointer, _) = match home.read(OsStr::new(POINTER)) {
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Err(Error::LegacyInventoryRequired),
            other => other?,
        };
        pointer.locator.validate()?;
        if pointer.format != VERSION || pointer.home != expected || pointer.authority.is_nil() {
            return Err(Error::Unsupported("Export federation pointer identity/version"));
        }
        let dir = Directory::open(&pointer.locator.path())?;
        shape(&dir, &[".namespace.lock", ".catalog.lock", "members", "header.json"])?;
        let (header, _): (FederationHeader, _) = dir.read(OsStr::new("header.json"))?;
        if header.format != VERSION || header.authority != pointer.authority || header.directory != pointer.directory
            || header.directory != dir.identity() || header.home != expected || header.ancestors != ancestors.identities {
            return Err(Error::Incomplete("Export federation physical ancestry or identity changed"));
        }
        header.home_locator.validate()?;
        if header.home_locator != UnixPath::capture(home_path)? {
            return Err(Error::Incomplete("Export federation home locator changed"));
        }
        let members = dir.open_dir(OsStr::new("members"))?;
        Ok(Self(Arc::new(Federation { dir, members, header })))
    }

    pub(crate) fn read_admission(&self, deadline: Instant) -> Result<NamespaceRead> {
        let ancestors = Ancestors::acquire(&self.0.header.home_locator.path(), self.0.header.home, false, deadline)?;
        if ancestors.identities != self.0.header.ancestors { return Err(Error::Incomplete("Export ancestry requires an explicit Move admission")); }
        let namespace = self.0.dir.lock(OsStr::new(".namespace.lock"), Mode::Shared, deadline)?;
        // Validate under the ALREADY-held ancestry plan. Calling open_existing
        // here would reacquire ancestor flocks after N, reversing the order.
        let home = Directory::open(&self.0.header.home_locator.path())?;
        let (pointer, _): (Pointer, _) = home.read(OsStr::new(POINTER))?;
        let (header, _): (FederationHeader, _) = self.0.dir.read(OsStr::new("header.json"))?;
        if home.identity() != self.0.header.home || pointer.format != VERSION
            || pointer.home != self.0.header.home || pointer.authority != self.0.header.authority
            || pointer.directory != self.0.header.directory || header != self.0.header {
            return Err(Error::Incomplete("Export namespace changed during admission"));
        }
        pointer.locator.validate()?;
        if Directory::open(&pointer.locator.path())?.identity() != self.0.header.directory {
            return Err(Error::Incomplete("Export federation locator changed during admission"));
        }
        Ok(NamespaceRead { namespace: self.clone(), _ancestors: ancestors, _namespace: namespace })
    }

    /// A complete bounded metadata census, not a deletion permit. Every member
    /// must match a complete local committed record, even across XDG roots.
    pub(crate) fn census(&self, deadline: Instant) -> Result<Vec<BindingData>> {
        let admission = self.read_admission(deadline)?;
        let _f = self.0.dir.lock(OsStr::new(".catalog.lock"), Mode::Shared, deadline)?;
        let (members, _) = self.members()?;
        let mut bindings = Vec::new();
        for member in members { bindings.push(self.complete_member(&member, deadline)?); }
        drop(_f);
        drop(admission);
        Ok(bindings)
    }
    fn complete_member(&self, member: &Member, deadline: Instant) -> Result<BindingData> {
        if member.phase != Phase::Committed { return Err(Error::Incomplete("Export federation contains an unfinished or revoked membership")); }
        let index = InstallationIndex::open(&member.index_locator.path())?;
        let _g = index.dir.lock(OsStr::new(".index.lock"), Mode::Shared, deadline)?;
        let record = index.record(member.association)?;
        if record.phase != Phase::Committed || record.commit != member.commit
            || index.header.installation != member.installation || index.header.directory != member.index
            || record.binding.account.home != self.0.header.home {
            return Err(Error::Incomplete("Export membership does not match its complete installation record"));
        }
        Ok(record.binding)
    }
    fn members(&self) -> Result<(Vec<Member>, usize)> {
        let mut total = 0usize;
        let mut rows = Vec::new();
        for name in self.0.members.names()? {
            let id = record_id(&name)?;
            let (row, bytes): (Member, _) = self.0.members.read(&name)?;
            total = total.checked_add(bytes).ok_or(Error::Capacity)?;
            if rows.len() >= MAX_RECORDS || total > MAX_TOTAL { return Err(Error::Capacity); }
            row.index_locator.validate()?;
            if row.format != VERSION || row.association != id || row.commit.is_nil() || row.installation.is_nil() { return Err(Error::Incomplete("Invalid export federation member")); }
            row.index.validate()?;
            rows.push(row);
        }
        Ok((rows, total))
    }
}

pub(crate) struct InstallationIndex { dir: Directory, records: Directory, header: IndexHeader }
impl InstallationIndex {
    pub(crate) fn create(parent: &Path, name: &OsStr, installation: Uuid) -> Result<Self> {
        if installation.is_nil() { return Err(Error::InvalidMetadata("Export installation ID is missing")); }
        let parent = Directory::open(parent)?;
        let dir = parent.create_dir(name)?;
        dir.create_lock(OsStr::new(".index.lock"))?;
        let records = dir.create_dir(OsStr::new("bindings"))?;
        let header = IndexHeader { format: VERSION, installation, directory: dir.identity() };
        dir.atomic(OsStr::new("header.json"), &header, false)?;
        Ok(Self { dir, records, header })
    }
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let dir = Directory::open(path)?;
        shape(&dir, &[".index.lock", "bindings", "header.json"])?;
        let (header, _): (IndexHeader, _) = dir.read(OsStr::new("header.json"))?;
        if header.format != VERSION || header.installation.is_nil() || header.directory != dir.identity() {
            return Err(Error::Unsupported("Export installation index identity/version"));
        }
        let records = dir.open_dir(OsStr::new("bindings"))?;
        Ok(Self { dir, records, header })
    }
    fn record(&self, id: Uuid) -> Result<BindingRecord> {
        let (row, _): (BindingRecord, _) = self.records.read(OsStr::new(&record_name(id)))?;
        row.validate()?;
        if row.binding.association != id || row.binding.installation != self.header.installation {
            return Err(Error::Incomplete("Export installation binding identity changed"));
        }
        Ok(row)
    }
    fn records(&self) -> Result<(Vec<BindingRecord>, usize)> {
        let mut rows = Vec::new();
        let mut total = 0usize;
        for name in self.records.names()? {
            let id = record_id(&name)?;
            let (row, bytes): (BindingRecord, _) = self.records.read(&name)?;
            total = total.checked_add(bytes).ok_or(Error::Capacity)?;
            if rows.len() >= MAX_RECORDS || total > MAX_TOTAL { return Err(Error::Capacity); }
            row.validate()?;
            if row.binding.association != id || row.binding.installation != self.header.installation { return Err(Error::Incomplete("Invalid export installation binding")); }
            rows.push(row);
        }
        Ok((rows, total))
    }
    pub(crate) fn register(&self, admission: NamespaceRead, binding: BindingData, deadline: Instant) -> Result<RegisteredBinding> {
        binding.validate()?;
        if binding.installation != self.header.installation || binding.account.home != admission.namespace.0.header.home
            || binding.account.canonical_home != admission.namespace.0.header.home_locator {
            return Err(Error::InvalidMetadata("Export binding is outside its physical namespace/installation"));
        }
        let namespace = &admission.namespace;
        let _f = namespace.0.dir.lock(OsStr::new(".catalog.lock"), Mode::Exclusive, deadline)?;
        let (members, member_bytes) = namespace.members()?;
        // Never hold our G while acquiring another installation's G. The
        // shared F freezes this home's members during these individual reads.
        for member in &members { namespace.complete_member(member, deadline)?; }
        let _g = self.dir.lock(OsStr::new(".index.lock"), Mode::Exclusive, deadline)?;
        let (records, record_bytes) = self.records()?;
        // A partial cross-file publication is never repaired or skipped here.
        if members.iter().any(|row| row.phase != Phase::Committed) || records.iter().any(|row| row.phase != Phase::Committed) {
            return Err(Error::Incomplete("Export registration needs explicit incomplete-state reconciliation"));
        }
        if let Some(record) = records.iter().find(|row| row.binding.association == binding.association) {
            let member = members.iter().find(|row| row.association == binding.association)
                .ok_or(Error::Incomplete("Export binding has no durable federation member"))?;
            if record.binding != binding || member.commit != record.commit || member.installation != self.header.installation || member.index != self.header.directory {
                return Err(Error::Incomplete("Export association cannot be rebound"));
            }
            return Ok(RegisteredBinding { admission, record: record.clone(), index: self.header.clone() });
        }
        if members.iter().any(|row| row.association == binding.association) { return Err(Error::Incomplete("Export federation member has no local record")); }
        if members.len() >= MAX_RECORDS || records.len() >= MAX_RECORDS { return Err(Error::Capacity); }
        let commit = Uuid::new_v4();
        let mut member = Member { format: VERSION, phase: Phase::Prepared, commit, association: binding.association,
            installation: self.header.installation, index: self.header.directory, index_locator: UnixPath::capture(&self.dir.current_path()?)? };
        let mut record = BindingRecord { format: VERSION, phase: Phase::Prepared, commit, binding };
        let name = record_name(record.binding.association);
        let mut committed_member = member.clone(); committed_member.phase = Phase::Committed;
        let mut committed_record = record.clone(); committed_record.phase = Phase::Committed;
        if member_bytes.checked_add(super::fs::encoded_size(&committed_member)?).is_none_or(|n| n > MAX_TOTAL)
            || record_bytes.checked_add(super::fs::encoded_size(&committed_record)?).is_none_or(|n| n > MAX_TOTAL) {
            return Err(Error::Capacity);
        }
        if Instant::now() >= deadline { return Err(Error::Deadline); }
        namespace.0.members.atomic(OsStr::new(&name), &member, false)?;
        self.records.atomic(OsStr::new(&name), &record, false)?;
        member.phase = Phase::Committed;
        namespace.0.members.atomic(OsStr::new(&name), &member, true)?;
        record.phase = Phase::Committed;
        self.records.atomic(OsStr::new(&name), &record, true)?;
        if Instant::now() >= deadline { return Err(Error::Deadline); }
        // Mutual commit identity plus actual N admission survives in this
        // result. It still needs an independently issued ActualStore proof.
        Ok(RegisteredBinding { admission, record, index: self.header.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{account::home::{Homes, CodexConfig}, agent::Provider, model::TerminalId};
    use super::super::{AccountBindingData, CacheIdentity, SessionNamespace, SourceReadProofData};
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn deadline() -> Instant { Instant::now() + Duration::from_secs(20) }
    fn bootstrap(root: &Path) -> Namespace {
        let mut namespace = None;
        Homes::at(root.join("homes")).create_with_config_initialized(
            Provider::Claude, None, CodexConfig::Shared, |created| {
                namespace = Some(Namespace::bootstrap_created_home(created, deadline())
                    .map_err(std::io::Error::other)?);
                Ok(())
            }).unwrap();
        namespace.unwrap()
    }
    // These are registration metadata fixtures, not an ActualStore source
    // proof. No test constructs a payload grant from the serializable record.
    fn binding(root: &Path, namespace: &Namespace, installation: Uuid) -> BindingData {
        let parent = Directory::open(root).unwrap();
        let cache = parent.create_dir(OsStr::new(&Uuid::new_v4().to_string())).unwrap();
        cache.create_lock(OsStr::new("transcript.sqlite")).unwrap();
        cache.create_lock(OsStr::new("readers.lock")).unwrap();
        BindingData {
            format: VERSION, installation, application: "io.cagents.fixture".into(), association: Uuid::new_v4(),
            cache: CacheIdentity { nonce: Uuid::new_v4(), root: parent.identity(), directory: cache.identity(),
                database: file_identity(&cache.open_file(OsStr::new("transcript.sqlite"), false).unwrap()).unwrap(),
                readers_lock: file_identity(&cache.open_file(OsStr::new("readers.lock"), false).unwrap()).unwrap() },
            cache_root: UnixPath::capture(root).unwrap(), cache_directory: UnixPath::capture(&cache.current_path().unwrap()).unwrap(),
            account: AccountBindingData { provider: Provider::Claude,
                canonical_home: namespace.0.header.home_locator.clone(), home: namespace.0.header.home,
                registered_id: None, purge_ids: vec![] },
            source_session: TerminalId::new(), native_namespace: SessionNamespace::Unconfirmed,
            source: SourceReadProofData { source_incarnation: Uuid::new_v4(), raw_owner_nonce: Uuid::new_v4(),
                raw_schema: 1, reset_schema: 1, text_schema: 1 }, export_generation: Uuid::new_v4(),
        }
    }

    #[test]
    fn independent_installations_publish_without_losing_shared_membership() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let namespace = bootstrap(root.path());
        let mut jobs = Vec::new();
        for suffix in ["xdg-one", "xdg-two"] {
            let installation = Uuid::new_v4();
            let index = InstallationIndex::create(root.path(), OsStr::new(suffix), installation).unwrap();
            let binding = binding(root.path(), &namespace, installation);
            let ns = namespace.clone();
            jobs.push(std::thread::spawn(move || {
                let registered = index.register(ns.read_admission(deadline()).unwrap(), binding.clone(), deadline()).unwrap();
                assert_eq!(registered.metadata(), &binding);
                assert!(!registered.commit().is_nil());
                binding
            }));
        }
        let expected = jobs.into_iter().map(|job| job.join().unwrap()).collect::<Vec<_>>();
        let census = namespace.census(deadline()).unwrap();
        assert_eq!(census.len(), 2);
        for record in expected { assert!(census.contains(&record)); }
        let reopened = Namespace::open_existing(&namespace.0.header.home_locator.path(), namespace.0.header.home, deadline()).unwrap();
        assert_eq!(reopened.census(deadline()).unwrap(), census);
    }

    #[test]
    fn existing_empty_home_never_mints_legacy_inventory() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let (_, home) = Homes::at(root.path().join("homes")).create(Provider::Claude, None).unwrap();
        let expected = identity_at(&home).unwrap();
        assert!(matches!(Namespace::open_existing(&home, expected, deadline()), Err(Error::LegacyInventoryRequired)));
        assert!(std::fs::read_dir(home).unwrap().next().is_none());
    }

    #[test]
    fn partial_mutual_commit_is_neither_skipped_nor_repaired() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let namespace = bootstrap(root.path());
        let installation = Uuid::new_v4();
        let index = InstallationIndex::create(root.path(), OsStr::new("xdg"), installation).unwrap();
        let first = binding(root.path(), &namespace, installation);
        drop(index.register(namespace.read_admission(deadline()).unwrap(), first.clone(), deadline()).unwrap());
        let name = record_name(first.association);
        let (mut member, _): (Member, _) = namespace.0.members.read(OsStr::new(&name)).unwrap();
        member.phase = Phase::Prepared;
        namespace.0.members.atomic(OsStr::new(&name), &member, true).unwrap();
        let before = std::fs::read(namespace.0.members.current_path().unwrap().join(&name)).unwrap();
        assert!(matches!(namespace.census(deadline()), Err(Error::Incomplete(_))));
        assert!(index.register(namespace.read_admission(deadline()).unwrap(), binding(root.path(), &namespace, installation), deadline()).is_err());
        assert_eq!(std::fs::read(namespace.0.members.current_path().unwrap().join(name)).unwrap(), before);
        assert_eq!(index.records.names().unwrap().len(), 1);
    }

    #[test]
    fn future_header_and_symlink_record_refuse_without_rewriting_bytes() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let namespace = bootstrap(root.path());
        let installation = Uuid::new_v4();
        let index = InstallationIndex::create(root.path(), OsStr::new("xdg"), installation).unwrap();
        let locator = index.dir.current_path().unwrap();
        let mut header = index.header.clone(); header.format += 1;
        index.dir.atomic(OsStr::new("header.json"), &header, true).unwrap();
        let before = std::fs::read(locator.join("header.json")).unwrap();
        assert!(matches!(InstallationIndex::open(&locator), Err(Error::Unsupported(_))));
        assert_eq!(std::fs::read(locator.join("header.json")).unwrap(), before);
        index.dir.atomic(OsStr::new("header.json"), &index.header, true).unwrap();
        let outside = root.path().join("outside"); std::fs::write(&outside, b"untouched").unwrap();
        symlink(&outside, index.records.current_path().unwrap().join(record_name(Uuid::new_v4()))).unwrap();
        assert!(index.register(namespace.read_admission(deadline()).unwrap(), binding(root.path(), &namespace, installation), deadline()).is_err());
        assert!(namespace.0.members.names().unwrap().is_empty());
        assert_eq!(std::fs::read(outside).unwrap(), b"untouched");
    }

    #[test]
    fn excessive_record_and_association_rebind_refuse_before_new_membership() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let namespace = bootstrap(root.path());
        let installation = Uuid::new_v4();
        let index = InstallationIndex::create(root.path(), OsStr::new("xdg"), installation).unwrap();
        let mut excessive = binding(root.path(), &namespace, installation);
        excessive.account.purge_ids = (0..64).map(|n| format!("{n:02}{}", "a".repeat(500))).collect();
        assert!(index.register(namespace.read_admission(deadline()).unwrap(), excessive, deadline()).is_err());
        assert!(namespace.0.members.names().unwrap().is_empty());
        assert!(index.records.names().unwrap().is_empty());
        let first = binding(root.path(), &namespace, installation);
        drop(index.register(namespace.read_admission(deadline()).unwrap(), first.clone(), deadline()).unwrap());
        let mut rebound = first; rebound.export_generation = Uuid::new_v4();
        assert!(index.register(namespace.read_admission(deadline()).unwrap(), rebound, deadline()).is_err());
        assert_eq!(namespace.census(deadline()).unwrap().len(), 1);
    }

    #[test]
    fn widened_pinned_directory_refuses_further_metadata_mutation() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let namespace = bootstrap(root.path());
        let path = namespace.0.members.current_path().unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(namespace.0.members.atomic(OsStr::new("new.json"), &0, false).is_err());
        assert!(std::fs::read_dir(&path).unwrap().next().is_none());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn failed_provider_seed_retains_already_registered_new_home() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let system = root.path().join("system"); std::fs::create_dir(&system).unwrap();
        std::fs::create_dir(system.join("config.toml")).unwrap();
        let mut saved = None;
        assert!(Homes::at(root.path().join("homes")).create_with_config_initialized(
            Provider::Codex, Some(&system), CodexConfig::Copy, |created| {
                saved = Some(created.home().to_path_buf());
                Namespace::bootstrap_created_home(created, deadline()).map_err(std::io::Error::other)?;
                Ok(())
            }).is_err());
        assert!(saved.unwrap().join(POINTER).is_file());
    }
}
