//! Private local intent only: no RPC, launch, cancellation ACK or retirement.
//! Call these blocking operations on a worker, never on the UI thread. Each
//! operation releases its process lock before returning to the effect owner.
use crate::account::preparation::{PreparationBinding, PreparationPayload};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

const VERSION: u32 = 1;
const JOURNAL: &str = "preparations-v1.json";
const LOCK: &str = ".preparations.lock";
const MAX_BYTES: u64 = 1024 * 1024;

/// These are public routing fields, never credentials or a launch recipe.
/// Equality cannot prove SSH destination or first-Plan installation continuity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Route {
    Local {
        application: String,
        state_directory: PathBuf,
    },
    Remote {
        name: String,
        host: String,
        executable: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RouteSnapshot {
    route: Route,
    route_revision: String,
}

impl RouteSnapshot {
    pub(super) fn capture(route: Route) -> io::Result<Self> {
        let route_revision = route_revision(&route)?;
        Ok(Self {
            route,
            route_revision,
        })
    }
}

fn route_revision(route: &Route) -> io::Result<String> {
    let text = |value: &str| {
        !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
    };
    let valid = match route {
        Route::Local {
            application,
            state_directory,
        } => {
            text(application)
                && state_directory.is_absolute()
                && state_directory.to_str().is_some_and(text)
        }
        Route::Remote {
            name,
            host,
            executable,
        } => text(name) && text(host) && text(executable),
    };
    if !valid {
        return Err(invalid("The original account setup route is invalid"));
    }
    let bytes = serde_json::to_vec(route).map_err(invalid)?;
    Ok(format!("route-v1:{:x}", Sha256::digest(bytes)))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Intent {
    pub(super) request_id: Uuid,
    pub(super) payload: PreparationPayload,
    pub(super) original_route: RouteSnapshot,
    pub(super) device_code: bool,
    pub(super) blocked_login: Option<Uuid>,
}

impl Intent {
    fn validate(&self) -> io::Result<()> {
        if self.request_id.is_nil()
            || !matches!(
                self.payload.provider,
                crate::agent::Provider::Claude | crate::agent::Provider::Codex
            )
            || self.blocked_login.is_some_and(|id| id.is_nil())
            || self.original_route.route_revision != route_revision(&self.original_route.route)?
        {
            return Err(invalid("The account setup intent is inconsistent"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Desired {
    Prepare,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub(super) intent: Intent,
    pub(super) desired: Desired,
    pub(super) binding: Option<PreparationBinding>,
    pub(super) revision: u64,
}

impl Record {
    fn validate(&self) -> io::Result<()> {
        self.intent.validate()?;
        if self.revision == 0 {
            return Err(invalid("The account setup intent has no local revision"));
        }
        if let Some(binding) = &self.binding {
            validate_binding(&self.intent, binding)?;
        }
        Ok(())
    }
}

fn validate_binding(intent: &Intent, binding: &PreparationBinding) -> io::Result<()> {
    if binding.request_id != intent.request_id
        || binding.payload != intent.payload
        || binding.installation_id.is_nil()
        || binding.key.is_nil()
    {
        return Err(invalid(
            "The account setup binding does not match its intent",
        ));
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    records: Vec<Record>,
}

impl Journal {
    fn validate(&self) -> io::Result<()> {
        if self.version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The account setup outbox version is unsupported",
            ));
        }
        let mut ids = BTreeSet::new();
        let mut homes = BTreeSet::new();
        for record in &self.records {
            record.validate()?;
            if !ids.insert(record.intent.request_id) {
                return Err(invalid("The account setup outbox repeats an intent"));
            }
            if let Some(binding) = &record.binding
                && !homes.insert((binding.installation_id, binding.key))
            {
                return Err(invalid("The account setup outbox repeats a bound home"));
            }
        }
        Ok(())
    }
}

pub(super) struct Store {
    directory: PathBuf,
}

impl Store {
    pub(super) fn at(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// None means the journal is missing, not an empty/acknowledged outbox.
    /// A read establishes visibility only, never a durability acknowledgement.
    pub(super) fn load(&self) -> io::Result<Option<Vec<Record>>> {
        let Some(held) = self.lock(false)? else {
            return Ok(None);
        };
        let result = self.read(&held)?.map(|(journal, _)| journal.records);
        held.revalidate()?;
        Ok(result)
    }

    /// A complete Cancel may arrive before the first Prepare worker. An old
    /// Prepare can confirm persistence but can never downgrade that Cancel.
    pub(super) fn upsert(&self, intent: &Intent, desired: Desired) -> io::Result<Record> {
        self.change(intent, Change::Intent(desired), Namespace::sync)
    }

    /// Attach only an exact binding to an existing intent at its current local
    /// revision. A concurrent Cancel requires rereading and retrying this CAS.
    pub(super) fn bind(
        &self,
        intent: &Intent,
        expected_revision: u64,
        binding: &PreparationBinding,
    ) -> io::Result<Record> {
        self.change(
            intent,
            Change::Bind(expected_revision, binding),
            Namespace::sync,
        )
    }

    fn change(
        &self,
        intent: &Intent,
        change: Change<'_>,
        sync: impl FnOnce(&Namespace) -> io::Result<()>,
    ) -> io::Result<Record> {
        intent.validate()?;
        if let Change::Bind(_, binding) = change {
            validate_binding(intent, binding)?;
        }
        let held = self
            .lock(matches!(change, Change::Intent(_)))?
            .ok_or_else(missing_intent)?;
        let previous = self.read(&held)?;
        let mut journal = previous
            .as_ref()
            .map(|(journal, _)| journal.clone())
            .unwrap_or(Journal {
                version: VERSION,
                records: Vec::new(),
            });
        let index = journal
            .records
            .iter()
            .position(|record| record.intent.request_id == intent.request_id);
        let record = match index {
            Some(index) => {
                let record = &mut journal.records[index];
                if &record.intent != intent {
                    return Err(conflict(
                        "The account setup intent or original route changed",
                    ));
                }
                let changed = match change {
                    Change::Intent(desired) => {
                        let cancel = desired == Desired::Cancel && record.desired != desired;
                        if cancel {
                            record.desired = Desired::Cancel;
                        }
                        cancel
                    }
                    Change::Bind(expected, binding) => {
                        if record
                            .binding
                            .as_ref()
                            .is_some_and(|saved| saved != binding)
                        {
                            return Err(conflict("The account setup binding changed"));
                        }
                        if record.revision != expected {
                            return Err(io::Error::new(
                                io::ErrorKind::WouldBlock,
                                "The account setup intent has a newer local revision",
                            ));
                        }
                        let unbound = record.binding.is_none();
                        record.binding = Some(binding.clone());
                        unbound
                    }
                };
                if changed {
                    record.revision = record
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| invalid("The account setup revision is exhausted"))?;
                }
                record.clone()
            }
            None => {
                let Change::Intent(desired) = change else {
                    return Err(missing_intent());
                };
                let record = Record {
                    intent: intent.clone(),
                    desired,
                    binding: None,
                    revision: 1,
                };
                journal.records.push(record.clone());
                journal
                    .records
                    .sort_by_key(|record| record.intent.request_id);
                record
            }
        };
        journal.validate()?;
        let persisted = if previous.as_ref().is_some_and(|(old, _)| old == &journal) {
            // Equality and a successful reread prove visibility, not durable
            // persistence after an earlier rename/directory-sync failure.
            let file = previous.as_ref().unwrap().1.try_clone()?;
            self.revalidate_journal(&held, Some(&file))?;
            file.sync_all()?;
            file
        } else {
            let bytes = serde_json::to_vec(&journal).map_err(invalid)?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(invalid("The account setup outbox is too large"));
            }
            let mut temporary = tempfile::Builder::new()
                .prefix(".preparations-")
                .permissions(fs::Permissions::from_mode(0o600))
                .tempfile_in(held.namespace.path())?;
            temporary.write_all(&bytes)?;
            temporary.as_file().sync_all()?;
            held.revalidate()?;
            self.revalidate_journal(&held, previous.as_ref().map(|(_, file)| file))?;
            temporary
                .persist(held.namespace.path().join(JOURNAL))
                .map_err(|error| error.error)?
        };
        sync(&held.namespace)?;
        self.revalidate_journal(&held, Some(&persisted))?;
        held.revalidate()?;
        Ok(record)
    }

    fn read(&self, held: &Locked) -> io::Result<Option<(Journal, File)>> {
        let path = held.namespace.path().join(JOURNAL);
        let mut file = match crate::secure_file::open_existing_private_file_readonly(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        require_private_file(&file)?;
        let mut bytes = Vec::new();
        (&mut file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid("The account setup outbox is too large"));
        }
        let journal: Journal = serde_json::from_slice(&bytes).map_err(invalid)?;
        journal.validate()?;
        // Shared backend DTOs deliberately have broader serde compatibility.
        // This private journal must reject unknown or omitted fields there,
        // too, instead of discarding them during its next write.
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).map_err(invalid)?;
        if parsed != serde_json::to_value(&journal).map_err(invalid)? {
            return Err(invalid("The account setup outbox has unsupported fields"));
        }
        self.revalidate_journal(held, Some(&file))?;
        Ok(Some((journal, file)))
    }

    fn revalidate_journal(&self, held: &Locked, previous: Option<&File>) -> io::Result<()> {
        let named = fs::symlink_metadata(held.namespace.path().join(JOURNAL));
        match (previous, named) {
            (None, Err(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            (Some(file), Ok(named)) if same_file(&file.metadata()?, &named) => {
                require_private_file(file)
            }
            (_, Err(error)) => Err(error),
            _ => Err(invalid("The account setup journal changed during storage")),
        }
    }

    fn lock(&self, create: bool) -> io::Result<Option<Locked>> {
        if !self.directory.is_absolute()
            || self
                .directory
                .components()
                .any(|part| part == Component::ParentDir)
        {
            return Err(invalid(
                "The account setup outbox needs an absolute local directory",
            ));
        }
        crate::secure_file::reject_symlink_components(&self.directory)?;
        match fs::symlink_metadata(&self.directory) {
            Ok(metadata) => {
                crate::secure_file::validate_owned_directory(&self.directory)?;
                if metadata.mode() & 0o7777 != 0o700 {
                    return Err(invalid("The account setup outbox directory is not private"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && !create => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                crate::secure_file::prepare_private_directory(&self.directory)?;
            }
            Err(error) => return Err(error),
        }
        let namespace = Namespace::capture(&self.directory)?;
        let path = namespace.path().join(LOCK);
        let file = match OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                crate::secure_file::open_existing_private_file_readonly(&path)?
            }
            Err(error) => return Err(error),
        };
        require_private_file(&file)?;
        let lock = crate::instance::owned_file_lock(file, libc::LOCK_EX | libc::LOCK_NB)?;
        let held = Locked {
            lock,
            lock_path: path,
            namespace,
        };
        held.revalidate()?;
        Ok(Some(held))
    }
}

#[derive(Clone, Copy)]
enum Change<'a> {
    Intent(Desired),
    Bind(u64, &'a PreparationBinding),
}

struct Locked {
    lock: crate::instance::OwnedLock,
    lock_path: PathBuf,
    namespace: Namespace,
}

impl Locked {
    fn revalidate(&self) -> io::Result<()> {
        self.namespace.revalidate()?;
        require_private_file(&self.lock)?;
        if !same_file(
            &self.lock.metadata()?,
            &fs::symlink_metadata(&self.lock_path)?,
        ) {
            return Err(invalid("The account setup outbox lock changed"));
        }
        Ok(())
    }
}

// Retain every containing binding so recursively created directories also
// reach disk. This local namespace has no relation to the selected device.
struct Namespace(Vec<(PathBuf, File)>);

impl Namespace {
    fn capture(directory: &Path) -> io::Result<Self> {
        let held = directory
            .ancestors()
            .map(|path| {
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)?;
                Ok((path.to_owned(), file))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let namespace = Self(held);
        namespace.revalidate()?;
        Ok(namespace)
    }

    fn revalidate(&self) -> io::Result<()> {
        for (index, (path, file)) in self.0.iter().enumerate() {
            let named = fs::symlink_metadata(path)?;
            if !named.is_dir()
                || !same_file(&file.metadata()?, &named)
                || (index == 0 && (named.uid() != current_uid() || named.mode() & 0o7777 != 0o700))
            {
                return Err(invalid("The account setup outbox directory changed"));
            }
        }
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        self.revalidate()?;
        for (_, file) in &self.0 {
            file.sync_all()?;
        }
        self.revalidate()
    }

    fn path(&self) -> PathBuf {
        // Resolve journal, lock and temporary names through our retained local
        // directory, even if another process replaces an ancestor path.
        PathBuf::from(format!("/proc/self/fd/{}", self.0[0].1.as_raw_fd()))
    }
}

fn require_private_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != current_uid()
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(invalid(
            "The account setup outbox file is not owned and private",
        ));
    }
    Ok(())
}

fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn conflict(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, message)
}

fn missing_intent() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "The account setup intent is missing; it was not acknowledged",
    )
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not change process state.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests;
