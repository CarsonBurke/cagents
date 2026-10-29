//! Private local intent only: no RPC, launch, cancellation ACK or retirement.
//! Call these blocking operations on a worker, never on the UI thread. Each
//! operation releases its process lock before returning to the effect owner.
use crate::account::preparation::{
    ExistingPendingBinding, ExistingPendingTarget, PreparationBinding, PreparationPayload,
};
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

const VERSION: u32 = 2;
// Migration shares the original namespace and lock: a saved Cancel must never
// disappear behind a newly missing V2 filename.
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
    pub(super) target: Target,
    pub(super) original_route: RouteSnapshot,
    pub(super) device_code: bool,
    pub(super) blocked_login: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Target {
    Prepared { payload: PreparationPayload },
    ExistingPending { original: ExistingPendingTarget },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Binding {
    Prepared { binding: PreparationBinding },
    ExistingPending { binding: ExistingPendingBinding },
}

impl Binding {
    fn home(&self) -> (Uuid, Uuid) {
        match self {
            Self::Prepared { binding } => (binding.installation_id, binding.key),
            Self::ExistingPending { binding } => (binding.installation_id, binding.key),
        }
    }
}

fn supported_provider(provider: crate::agent::Provider) -> bool {
    matches!(
        provider,
        crate::agent::Provider::Claude | crate::agent::Provider::Codex
    )
}

fn validate_existing_binding(binding: &ExistingPendingBinding) -> io::Result<()> {
    if !supported_provider(binding.provider)
        || binding.installation_id.is_nil()
        || binding.key.is_nil()
        || binding.generation_id.is_nil()
    {
        return Err(invalid(
            "The existing account setup binding is inconsistent",
        ));
    }
    Ok(())
}

impl Intent {
    fn validate(&self) -> io::Result<()> {
        if self.request_id.is_nil()
            || self.blocked_login.is_some_and(|id| id.is_nil())
            || self.original_route.route_revision != route_revision(&self.original_route.route)?
        {
            return Err(invalid("The account setup intent is inconsistent"));
        }
        match &self.target {
            Target::Prepared { payload } if !supported_provider(payload.provider) => {
                return Err(invalid("The account setup provider is unsupported"));
            }
            Target::ExistingPending { original } => match original {
                ExistingPendingTarget::Observed {
                    provider,
                    key,
                    expected_installation,
                    ..
                } => {
                    if !supported_provider(*provider)
                        || key.is_nil()
                        || expected_installation.is_some_and(|id| id.is_nil())
                    {
                        return Err(invalid(
                            "The existing account setup observation is inconsistent",
                        ));
                    }
                }
                ExistingPendingTarget::Bound { binding } => validate_existing_binding(binding)?,
            },
            _ => {}
        }
        Ok(())
    }

    fn initial_binding(&self) -> Option<Binding> {
        match &self.target {
            Target::ExistingPending {
                original: ExistingPendingTarget::Bound { binding },
            } => Some(Binding::ExistingPending {
                binding: binding.clone(),
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Desired {
    /// ExistingPending only resumes metadata; it never plans or creates a home.
    Prepare,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub(super) intent: Intent,
    pub(super) desired: Desired,
    pub(super) binding: Option<Binding>,
    pub(super) revision: u64,
}

impl Record {
    fn validate(&self) -> io::Result<()> {
        self.intent.validate()?;
        if self.revision == 0 {
            return Err(invalid("The account setup intent has no local revision"));
        }
        if let Target::ExistingPending {
            original: ExistingPendingTarget::Bound { binding: original },
        } = &self.intent.target
            && !matches!(&self.binding, Some(Binding::ExistingPending { binding }) if binding == original)
        {
            return Err(invalid(
                "The original account setup binding was not retained",
            ));
        }
        if let Some(binding) = &self.binding {
            validate_binding(&self.intent, binding)?;
        }
        Ok(())
    }
}

fn validate_binding(intent: &Intent, binding: &Binding) -> io::Result<()> {
    let matches = match (&intent.target, binding) {
        (Target::Prepared { payload }, Binding::Prepared { binding }) => {
            binding.request_id == intent.request_id
                && binding.payload == *payload
                && !binding.installation_id.is_nil()
                && !binding.key.is_nil()
        }
        (Target::ExistingPending { original }, Binding::ExistingPending { binding }) => {
            validate_existing_binding(binding)?;
            match original {
                ExistingPendingTarget::Observed {
                    provider,
                    key,
                    started_at,
                    expected_installation,
                } => {
                    binding.provider == *provider
                        && binding.key == *key
                        && binding.started_at == *started_at
                        && expected_installation
                            .is_none_or(|expected| binding.installation_id == expected)
                }
                ExistingPendingTarget::Bound { binding: original } => binding == original,
            }
        }
        _ => false,
    };
    if !matches {
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
                && !homes.insert(binding.home())
            {
                return Err(invalid("The account setup outbox repeats a bound home"));
            }
        }
        Ok(())
    }
}

// Keep the original wire schema intact until its exact fields are checked.
// In particular, mapping first would discard unknown nested backend fields.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1Intent {
    request_id: Uuid,
    payload: PreparationPayload,
    original_route: RouteSnapshot,
    device_code: bool,
    blocked_login: Option<Uuid>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1Record {
    intent: V1Intent,
    desired: Desired,
    binding: Option<PreparationBinding>,
    revision: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1Journal {
    version: u32,
    records: Vec<V1Record>,
}

impl V1Journal {
    fn into_current(self) -> Journal {
        Journal {
            version: VERSION,
            records: self
                .records
                .into_iter()
                .map(|record| Record {
                    intent: Intent {
                        request_id: record.intent.request_id,
                        target: Target::Prepared {
                            payload: record.intent.payload,
                        },
                        original_route: record.intent.original_route,
                        device_code: record.intent.device_code,
                        blocked_login: record.intent.blocked_login,
                    },
                    desired: record.desired,
                    binding: record.binding.map(|binding| Binding::Prepared { binding }),
                    revision: record.revision,
                })
                .collect(),
        }
    }
}

// Reject duplicates in the original document, including inside internally
// tagged backend DTOs. A Value-only round trip would silently keep one value.
struct UniqueJson(serde_json::Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> serde::de::Visitor<'de> for UniqueVisitor {
            type Value = UniqueJson;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON without duplicate fields")
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| UniqueJson(serde_json::Value::Number(number)))
                    .ok_or_else(|| E::custom("The account setup outbox has an invalid number"))
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(serde_json::Value::Null))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<UniqueJson>()? {
                    values.push(value.0);
                }
                Ok(UniqueJson(serde_json::Value::Array(values)))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom(
                            "The account setup outbox repeats a field",
                        ));
                    }
                    values.insert(key, map.next_value::<UniqueJson>()?.0);
                }
                Ok(UniqueJson(serde_json::Value::Object(values)))
            }
        }
        deserializer.deserialize_any(UniqueVisitor)
    }
}

fn canonical_fields(original: &serde_json::Value, wire: &impl Serialize) -> io::Result<()> {
    // Shared backend DTOs have broader serde compatibility. Their omitted or
    // unsupported fields must not be discarded by a private journal rewrite.
    if *original != serde_json::to_value(wire).map_err(invalid)? {
        return Err(invalid("The account setup outbox has unsupported fields"));
    }
    Ok(())
}

fn decode_journal(bytes: &[u8]) -> io::Result<(Journal, u32)> {
    let original = serde_json::from_slice::<UniqueJson>(bytes)
        .map_err(invalid)?
        .0;
    let version = original
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| invalid("The account setup outbox version is invalid"))?;
    let journal = match version {
        1 => {
            let wire: V1Journal = serde_json::from_slice(bytes).map_err(invalid)?;
            canonical_fields(&original, &wire)?;
            wire.into_current()
        }
        version if version == u64::from(VERSION) => {
            let wire: Journal = serde_json::from_slice(bytes).map_err(invalid)?;
            canonical_fields(&original, &wire)?;
            wire
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The account setup outbox version is unsupported",
            ));
        }
    };
    journal.validate()?;
    Ok((journal, version as u32))
}

pub(super) struct Store {
    directory: PathBuf,
}

impl Store {
    pub(super) fn at(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// None means the journal is missing, not an empty/acknowledged outbox.
    /// A read normalizes V1 in memory without writing. It establishes visibility
    /// only, never a durability acknowledgement or a completed migration.
    pub(super) fn load(&self) -> io::Result<Option<Vec<Record>>> {
        let Some(held) = self.lock(false)? else {
            return Ok(None);
        };
        let result = self.read(&held)?.map(|(journal, _, _)| journal.records);
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
        binding: &Binding,
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
            .map(|(journal, _, _)| journal.clone())
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
                    binding: intent.initial_binding(),
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
        let persisted = if previous
            .as_ref()
            .is_some_and(|(old, _, version)| *version == VERSION && old == &journal)
        {
            // Equality and a successful reread prove visibility, not durable
            // persistence after an earlier rename/directory-sync failure.
            let file = previous.as_ref().unwrap().1.try_clone()?;
            self.revalidate_journal(&held, Some(&file))?;
            file.sync_all()?;
            file
        } else {
            // An equal V1 replay still rewrites V2 atomically. Only encoding
            // changes in that case; the record's local revision stays put.
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
            self.revalidate_journal(&held, previous.as_ref().map(|(_, file, _)| file))?;
            temporary
                .persist(held.namespace.path().join(JOURNAL))
                .map_err(|error| error.error)?
        };
        sync(&held.namespace)?;
        self.revalidate_journal(&held, Some(&persisted))?;
        held.revalidate()?;
        Ok(record)
    }

    fn read(&self, held: &Locked) -> io::Result<Option<(Journal, File, u32)>> {
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
        let (journal, original_version) = decode_journal(&bytes)?;
        self.revalidate_journal(held, Some(&file))?;
        Ok(Some((journal, file, original_version)))
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
    Bind(u64, &'a Binding),
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
