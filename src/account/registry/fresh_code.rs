//! Stable ownership for a separately consented fresh Codex code login. This
//! collection lives in accounts.toml; visibility never mints a spawn permit.
use super::*;
use crate::account::fresh_code::{CodeOwner, CodeState, ConditionalPlan, FreshCodeStart};
use crate::model::HostSessionProof;

const FRESH_CODE_MIN_VERSION: u32 = 7;

pub(crate) enum CodeClaim {
    Fresh(CodeOwner),
    /// The exact attempt already owns this home. This is metadata only.
    Retained(CodeOwner),
}

pub(crate) enum CodeRegistration {
    Registered(CodeOwner),
    DuplicateRetained {
        owner: CodeOwner,
        existing: AccountId,
    },
}

/// The actual first store outcome, never recomputed during a retained replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RegistrationOutcome {
    Registered,
    DuplicateRetained {
        existing: AccountId,
        account: DuplicateAccount,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum DuplicateAccount {
    System { identity: Identity },
    Registered { account: Registered },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SystemIdentity {
    #[serde(with = "super::provider_name")]
    provider: Provider,
    identity: Identity,
}

/// Noncredential inputs and the exact outcome are retained together. Keeping
/// the original Started owner avoids treating a later phase or observation as
/// a registration receipt. No live probe or capability is serialized here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RegistrationRecord {
    started: CodeOwner,
    managed: Registered,
    system: Vec<SystemIdentity>,
    outcome: RegistrationOutcome,
}

impl RegistrationRecord {
    fn settled_owner(&self) -> CodeOwner {
        let mut settled = self.started.clone();
        settled.state = match self.outcome {
            RegistrationOutcome::Registered => CodeState::Registered,
            RegistrationOutcome::DuplicateRetained { .. } => CodeState::DuplicateRetained,
        };
        settled
    }

    fn result(&self) -> CodeRegistration {
        let owner = self.settled_owner();
        match &self.outcome {
            RegistrationOutcome::Registered => CodeRegistration::Registered(owner),
            RegistrationOutcome::DuplicateRetained { existing, .. } => {
                CodeRegistration::DuplicateRetained {
                    owner,
                    existing: existing.clone(),
                }
            }
        }
    }
}

fn conflict(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, message)
}

fn same_registration_identity(left: &Registered, right: &Registered) -> bool {
    left.key == right.key
        && left.provider == right.provider
        && left.linked_home == right.linked_home
        && left.email == right.email
        && left.organization == right.organization
        && left.added_at == right.added_at
}

fn registration_bytes(file: &fs::File) -> io::Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    reader.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the accounts file is too large",
        ));
    }
    Ok(bytes)
}

fn registration_registry(bytes: &[u8]) -> io::Result<Registry> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let registry: Registry =
        toml::from_str(text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    registry.validate_preparations()?;
    if registry.is_newer() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "The accounts file is newer than this application",
        ));
    }
    Ok(registry)
}

fn revalidate_registration_file(path: &Path, file: &fs::File, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    crate::secure_file::reject_symlink_components(path)?;
    let held = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    if !held.is_file()
        || !named.is_file()
        || (held.dev(), held.ino()) != (named.dev(), named.ino())
        || held.uid() != unsafe { libc::geteuid() }
        || named.uid() != held.uid()
        || held.mode() & 0o077 != 0
        || named.mode() & 0o077 != 0
        || held.nlink() != 1
        || named.nlink() != 1
        || registration_bytes(file)? != bytes
    {
        return Err(conflict(
            "The retained registration file changed; its first outcome was preserved",
        ));
    }
    Ok(())
}

fn registration_barrier(
    path: &Path,
    file: &fs::File,
    bytes: &[u8],
    namespace: &crate::account::durability::Directories,
    sync: &mut impl FnMut(&crate::account::durability::Directories) -> io::Result<()>,
) -> io::Result<()> {
    namespace.revalidate()?;
    revalidate_registration_file(path, file, bytes)?;
    file.sync_all()?;
    sync(namespace)?;
    namespace.revalidate()?;
    revalidate_registration_file(path, file, bytes)
}

impl Registry {
    pub(crate) fn fresh_code_owner(&self, request_id: Uuid) -> Option<&CodeOwner> {
        self.fresh_code_owners
            .iter()
            .find(|owner| owner.binding.request_id == request_id)
    }

    pub(super) fn validate_fresh_code(&self) -> io::Result<()> {
        let mut requests = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        let mut attempts = std::collections::HashSet::new();
        let mut terminals = std::collections::HashSet::new();
        for owner in &self.fresh_code_owners {
            owner
                .validate()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if self.version < FRESH_CODE_MIN_VERSION
                || !requests.insert(owner.binding.request_id)
                || !keys.insert(owner.binding.key)
                || !attempts.insert(owner.attempt.id)
                || !terminals.insert(owner.attempt.terminal_id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "The fresh code ownership is inconsistent",
                ));
            }
            let preparation = self.exact_preparation(&owner.binding)?;
            if !matches!(
                preparation.state,
                PreparationState::Created
                    | PreparationState::CancelledRetained
                    | PreparationState::Registered
            ) || ((owner.state == CodeState::Registered)
                != (preparation.state == PreparationState::Registered))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "The fresh code owner lost its preparation",
                ));
            }
        }
        let mut registrations = std::collections::HashSet::new();
        for record in &self.fresh_code_registrations {
            record
                .started
                .validate()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if self.version < FRESH_CODE_MIN_VERSION
                || record.started.state != CodeState::Started
                || !registrations.insert(record.started.binding.request_id)
                || record.managed.key != record.started.binding.key
                || record.managed.provider != record.started.binding.payload.provider
                || record.managed.linked_home.is_some()
                || record
                    .system
                    .iter()
                    .any(|known| known.provider != record.managed.provider)
                || self.fresh_code_owner(record.started.binding.request_id)
                    != Some(&record.settled_owner())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "The retained code registration is inconsistent",
                ));
            }
            if let RegistrationOutcome::DuplicateRetained { existing, account } = &record.outcome {
                if existing.as_str().parse::<AccountId>().is_err()
                    || !existing
                        .as_str()
                        .starts_with(&format!("{}/", record.managed.provider.executable()))
                    || *existing
                        == Account::managed(
                            record.managed.provider,
                            record.managed.key,
                            PathBuf::new(),
                        )
                        .id()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "The retained duplicate account identity is inconsistent",
                    ));
                }
                let identity = record.managed.identity();
                let exact_target = match account {
                    DuplicateAccount::System { identity: known } => {
                        *existing == Account::system(record.managed.provider).id()
                            && known.same_login(&identity)
                            && record.system.iter().any(|system| system.identity == *known)
                    }
                    DuplicateAccount::Registered { account: known } => {
                        known.provider == record.managed.provider
                            && Account::managed(known.provider, known.key, PathBuf::new()).id()
                                == *existing
                            && known.identity().same_login(&identity)
                    }
                };
                if !exact_target {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "The retained duplicate lost its original account identity",
                    ));
                }
            }
        }
        Ok(())
    }

    fn registration_record(&self, request: Uuid) -> Option<&RegistrationRecord> {
        self.fresh_code_registrations
            .iter()
            .find(|record| record.started.binding.request_id == request)
    }

    /// A replay must still describe the same relevant current identity and
    /// phase. Labels are preferences, so editing a label does not rewrite or
    /// invalidate the original registration outcome.
    fn validate_registration_replay(&self, record: &RegistrationRecord) -> io::Result<()> {
        if self.fresh_code_owner(record.started.binding.request_id) != Some(&record.settled_owner())
        {
            return Err(conflict("The retained code registration owner changed"));
        }
        let preparation = self.exact_preparation(&record.started.binding)?;
        match &record.outcome {
            RegistrationOutcome::Registered => {
                let current = self
                    .find(record.managed.key)
                    .ok_or_else(|| conflict("The registered code account is missing"))?;
                if preparation.state != PreparationState::Registered
                    || !same_registration_identity(current, &record.managed)
                    || self.archived.contains(&current.key)
                {
                    return Err(conflict("The registered code account identity changed"));
                }
            }
            RegistrationOutcome::DuplicateRetained { account, .. } => {
                if preparation.state != PreparationState::Created
                    || !self.pending_is_eligible(record.managed.provider, record.managed.key)
                {
                    return Err(conflict(
                        "The duplicate code preparation is no longer eligible",
                    ));
                }
                let same_duplicate = match account {
                    DuplicateAccount::System { identity } => record.system.iter().any(|known| {
                        known.provider == record.managed.provider && known.identity == *identity
                    }),
                    DuplicateAccount::Registered { account } => {
                        self.find(account.key).is_some_and(|current| {
                            !self.archived.contains(&current.key)
                                && same_registration_identity(current, account)
                        })
                    }
                };
                if !same_duplicate || self.find(record.managed.key).is_some() {
                    return Err(conflict("The retained duplicate account identity changed"));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn require_code_owner(&self, owner: &CodeOwner) -> io::Result<()> {
        if self.fresh_code_owner(owner.binding.request_id) != Some(owner) {
            return Err(conflict("The fresh code attempt changed"));
        }
        let preparation = self.exact_preparation(&owner.binding)?;
        if preparation.state != PreparationState::Created
            || !self.pending_is_eligible(owner.binding.payload.provider, owner.binding.key)
        {
            return Err(conflict("The fresh code preparation is no longer eligible"));
        }
        Ok(())
    }
}

impl Store {
    /// Observation must not use the ordinary registry reader: that reader
    /// intentionally repairs legacy file permissions for mutation callers.
    fn fresh_code_observation(&self) -> io::Result<Registry> {
        let directory = if self.directory.as_os_str().is_empty() {
            Path::new(".")
        } else {
            &self.directory
        };
        let path = self.path();
        crate::secure_file::reject_symlink_components(&path)?;
        match crate::secure_file::validate_owned_directory(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Registry::default());
            }
            Err(error) => return Err(error),
        }
        let file = match crate::secure_file::open_existing_private_file_readonly(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Registry::default());
            }
            Err(error) => return Err(error),
        };
        let mut text = String::new();
        file.take(MAX_BYTES + 1).read_to_string(&mut text)?;
        if text.len() as u64 > MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the accounts file is too large",
            ));
        }
        let registry: Registry = toml::from_str(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        registry.validate_preparations()?;
        Ok(registry)
    }

    /// Resolve a lost Plan reply by its actual original request UUID. This
    /// reads existing metadata only; it never repeats Plan or invents a key.
    pub(crate) fn inspect_fresh_code_plan(
        &self,
        request: &ConditionalPlan,
    ) -> io::Result<Option<Preparation>> {
        request.validate()?;
        let registry = self.fresh_code_observation()?;
        if registry.is_newer() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The accounts file is newer than this application",
            ));
        }
        if registry.installation_id() != Some(request.expected_installation) {
            return Err(conflict(
                "The original installation cannot be confirmed for this plan observation",
            ));
        }
        let observed = registry.preparation(request.request_id);
        if observed.is_some_and(|record| record.binding.payload != request.payload) {
            return Err(conflict(
                "The observed fresh-code plan has different options",
            ));
        }
        Ok(observed.cloned())
    }

    /// Plan only in the already observed installation. No absent or raced
    /// identity is initialized by this separate physical action.
    pub(crate) fn plan_fresh_code(&self, request: &ConditionalPlan) -> io::Result<Preparation> {
        request.validate()?;
        if self.inspect_installation_id()? != Some(request.expected_installation) {
            return Err(conflict(
                "The original installation changed before the fresh code plan",
            ));
        }
        self.preparation_transaction(true, |registry| {
            if registry.installation != Some(request.expected_installation) {
                return Err(conflict(
                    "The original installation changed before the fresh code plan",
                ));
            }
            if let Some(existing) = registry.preparation(request.request_id) {
                if existing.binding.payload != request.payload {
                    return Err(conflict(
                        "The fresh code request was reused with different options",
                    ));
                }
                return Ok(existing.clone());
            }
            let planned = Preparation {
                binding: PreparationBinding {
                    request_id: request.request_id,
                    installation_id: request.expected_installation,
                    key: Uuid::new_v4(),
                    payload: request.payload.clone(),
                },
                state: PreparationState::Planned,
            };
            registry.preparations.push(planned.clone());
            Ok(planned)
        })
    }

    /// A new owner is persisted before entering the terminal constructor. The
    /// returned metadata must be paired with the live original creation pin;
    /// neither this result nor an equal replay alone grants spawn authority.
    pub(crate) fn claim_fresh_code(
        &self,
        request: &FreshCodeStart,
        host: &HostSessionProof,
    ) -> io::Result<CodeClaim> {
        request.validate()?;
        if !host.is_complete() {
            return Err(conflict(
                "The original code-login host proof is unavailable",
            ));
        }
        self.claim_fresh_code_with_sync(request, host, |namespace| namespace.sync())
    }

    fn claim_fresh_code_with_sync(
        &self,
        request: &FreshCodeStart,
        host: &HostSessionProof,
        sync: impl FnMut(&crate::account::durability::Directories) -> io::Result<()>,
    ) -> io::Result<CodeClaim> {
        self.preparation_transaction_with_sync(
            true,
            |registry| {
                request.validate()?;
                if !host.is_complete() {
                    return Err(conflict(
                        "The original code-login host proof is unavailable",
                    ));
                }
                let preparation = registry.exact_preparation(&request.binding)?;
                if preparation.state != PreparationState::Created
                    || !registry
                        .pending_is_eligible(request.binding.payload.provider, request.binding.key)
                {
                    return Err(conflict(
                        "The fresh code home is not currently Created and pending",
                    ));
                }
                if let Some(existing) = registry.fresh_code_owner(request.binding.request_id) {
                    if !existing.matches(request) || &existing.host != host {
                        return Err(conflict(
                            "Another code-login attempt already owns this preparation",
                        ));
                    }
                    return Ok(CodeClaim::Retained(existing.clone()));
                }
                if registry.fresh_code_owners.iter().any(|owner| {
                    owner.binding.key == request.binding.key
                        || owner.attempt.id == request.attempt.id
                        || owner.attempt.terminal_id == request.attempt.terminal_id
                }) {
                    return Err(conflict(
                        "The fresh code attempt or terminal is already owned",
                    ));
                }
                let owner = CodeOwner {
                    binding: request.binding.clone(),
                    attempt: request.attempt.clone(),
                    host: host.clone(),
                    state: CodeState::Owned,
                };
                registry.fresh_code_owners.push(owner.clone());
                Ok(CodeClaim::Fresh(owner))
            },
            sync,
        )
    }

    /// Exact read reconciliation grants no constructor or durability receipt.
    pub(crate) fn observe_fresh_code(
        &self,
        request: &FreshCodeStart,
    ) -> io::Result<Option<CodeOwner>> {
        request.validate()?;
        let registry = self.fresh_code_observation()?;
        if registry.is_newer() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The accounts file is newer than this application",
            ));
        }
        registry.exact_preparation(&request.binding)?;
        let owner = registry.fresh_code_owner(request.binding.request_id);
        if owner.is_some_and(|owner| !owner.matches(request)) {
            return Err(conflict(
                "The retained code-login owner differs from this attempt",
            ));
        }
        Ok(owner.cloned())
    }

    pub(crate) fn code_started(&self, owner: &CodeOwner) -> io::Result<CodeOwner> {
        self.preparation_transaction(true, |registry| {
            let mut started = owner.clone();
            started.state = CodeState::Started;
            let saved = registry
                .fresh_code_owner(owner.binding.request_id)
                .ok_or_else(|| conflict("The fresh code owner is missing"))?;
            if saved == &started {
                return Ok(started);
            }
            if owner.state != CodeState::Owned || saved != owner {
                return Err(conflict(
                    "The code-login start belongs to another owner phase",
                ));
            }
            // A cancellation after actual spawn does not erase that physical
            // fact. It still fences every later probe/registration operation.
            let index = registry
                .fresh_code_owners
                .iter()
                .position(|saved| saved.binding.request_id == owner.binding.request_id)
                .unwrap();
            registry.fresh_code_owners[index] = started.clone();
            Ok(started)
        })
    }

    /// Official probe data reaches this method only through the bound host
    /// owner. Retained replay supplies the SAME original inputs; it never
    /// probes or registers again. Validation runs before the operation and
    /// after its exact file/ancestor barrier under the same two leases.
    pub(crate) fn register_fresh_code(
        &self,
        homes: &Homes,
        owner: &CodeOwner,
        managed: Registered,
        system: &[(Provider, Identity)],
        validate_live: impl FnMut(&Registry) -> io::Result<()>,
    ) -> io::Result<CodeRegistration> {
        self.register_fresh_code_with_sync(
            homes,
            owner,
            managed,
            system,
            validate_live,
            |namespace| namespace.sync(),
        )
    }

    fn register_fresh_code_with_sync(
        &self,
        homes: &Homes,
        owner: &CodeOwner,
        managed: Registered,
        system: &[(Provider, Identity)],
        mut validate_live: impl FnMut(&Registry) -> io::Result<()>,
        mut sync: impl FnMut(&crate::account::durability::Directories) -> io::Result<()>,
    ) -> io::Result<CodeRegistration> {
        owner.validate()?;
        if owner.state != CodeState::Started
            || managed.key != owner.binding.key
            || managed.provider != owner.binding.payload.provider
            || managed.linked_home.is_some()
        {
            return Err(conflict(
                "The code registration does not match its actual started owner",
            ));
        }
        let original_system: Vec<_> = system
            .iter()
            .filter(|(provider, _)| *provider == managed.provider)
            .map(|(provider, identity)| SystemIdentity {
                provider: *provider,
                identity: identity.clone(),
            })
            .collect();
        let _lifecycle = homes.try_lifecycle_lock()?;
        let _registry = self.try_lock()?;
        let namespace = crate::account::durability::Directories::capture(&self.directory)?;
        let path = self.path();
        let original = crate::secure_file::open_existing_private_file_readonly(&path)?;
        let original_bytes = registration_bytes(&original)?;
        let mut registry = registration_registry(&original_bytes)?;
        revalidate_registration_file(&path, &original, &original_bytes)?;
        namespace.revalidate()?;

        let record = if let Some(record) = registry.registration_record(owner.binding.request_id) {
            if record.started != *owner
                || record.managed != managed
                || record.system != original_system
            {
                return Err(conflict("The original code registration inputs changed"));
            }
            registry.validate_registration_replay(record)?;
            validate_live(&registry)?;
            let record = record.clone();
            // Exact retained-result replay changes no metadata and performs
            // no rename/revision/business operation. Visibility alone cannot
            // return the stored result without this same-file barrier.
            registration_barrier(&path, &original, &original_bytes, &namespace, &mut sync)?;
            registry.validate_registration_replay(&record)?;
            validate_live(&registry)?;
            namespace.revalidate()?;
            revalidate_registration_file(&path, &original, &original_bytes)?;
            record
        } else {
            registry.require_code_owner(owner)?;
            validate_live(&registry)?;
            let outcome = match registry.register_managed(managed.clone(), system) {
                Ok(()) => RegistrationOutcome::Registered,
                Err(RegistrationError::Duplicate(existing)) => {
                    let account = if existing == Account::system(managed.provider).id() {
                        let identity = original_system
                            .iter()
                            .find(|known| known.identity.same_login(&managed.identity()))
                            .ok_or_else(|| conflict("The actual system duplicate disappeared"))?
                            .identity
                            .clone();
                        DuplicateAccount::System { identity }
                    } else {
                        let account = registry
                            .accounts
                            .iter()
                            .find(|known| {
                                Account::managed(known.provider, known.key, PathBuf::new()).id()
                                    == existing
                            })
                            .ok_or_else(|| conflict("The actual registered duplicate disappeared"))?
                            .clone();
                        DuplicateAccount::Registered { account }
                    };
                    RegistrationOutcome::DuplicateRetained { existing, account }
                }
                Err(error) => return Err(io::Error::other(error)),
            };
            let record = RegistrationRecord {
                started: owner.clone(),
                managed,
                system: original_system,
                outcome,
            };
            let index = registry
                .fresh_code_owners
                .iter()
                .position(|saved| saved.binding.request_id == owner.binding.request_id)
                .unwrap();
            registry.fresh_code_owners[index] = record.settled_owner();
            registry.fresh_code_registrations.push(record.clone());
            registry.version = VERSION;
            registry.revision = registry
                .revision
                .checked_add(1)
                .ok_or_else(|| io::Error::other("Accounts revision exhausted"))?;
            registry.validate_preparations()?;
            registry.validate_registration_replay(&record)?;
            let mut published = None;
            write_with_published_io(
                &self.directory,
                &path,
                &registry,
                || {
                    namespace.revalidate()?;
                    revalidate_registration_file(&path, &original, &original_bytes)
                },
                |written_namespace, file| {
                    namespace.revalidate()?;
                    let bytes = registration_bytes(file)?;
                    if registration_registry(&bytes)? != registry {
                        return Err(conflict("The published code registration changed"));
                    }
                    registration_barrier(&path, file, &bytes, written_namespace, &mut sync)?;
                    published = Some((file.try_clone()?, bytes));
                    Ok(())
                },
            )?;
            registry.validate_registration_replay(&record)?;
            validate_live(&registry)?;
            let (file, bytes) = published.ok_or_else(|| {
                io::Error::other("The actual registration publication receipt is unavailable")
            })?;
            namespace.revalidate()?;
            revalidate_registration_file(&path, &file, &bytes)?;
            record
        };
        Ok(record.result())
    }

    /// Extend the actual process-start lease. The caller's root/connection
    /// checks run while this SAME lock fences Cancel through process creation.
    #[cfg(feature = "host")]
    pub(crate) fn authorize_fresh_code_spawn(
        &self,
        homes: &Homes,
        owner: &CodeOwner,
        validate_live: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<LaunchAccountLease> {
        if owner.state != CodeState::Owned {
            return Err(conflict(
                "Only a newly owned code attempt can enter process creation",
            ));
        }
        let lock = homes.lifecycle_lock()?;
        let registry = read(&self.path())?;
        if registry.is_newer() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The accounts file is newer than this application",
            ));
        }
        registry.require_code_owner(owner)?;
        validate_live()?;
        Ok(LaunchAccountLease { _lock: lock })
    }
}

#[cfg(test)]
mod tests;
