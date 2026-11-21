//! Scoped admission for account-home mutations. Install reservations under the
//! short control writer; never retain a map or HostState lock during I/O.
//!
//! This foundation issues no deletion permit. Managed removal refuses and
//! pending/failed homes remain intact. Historical namespace enrollment and
//! complete actor/reader retirement census belong to that separate cut.
use super::state::SharedHost;
use crate::chat::protocol::SetupError;
use crate::{
    account::{
        Account,
        registry::{Registry, Store},
    },
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

type Map = BTreeMap<uuid::Uuid, Arc<Reserved>>;
#[derive(Clone)]
pub(super) struct Reserved {
    homes: Vec<Home>,
    /// The unregistered home this reservation restores, if that is what it
    /// is for. A restore changes no home and writes the list only under its
    /// locks, so it does not hold up a shutdown, an upgrade or a chat's
    /// attachment the way a sign-in or a removal does.
    restores: Option<(crate::agent::Provider, uuid::Uuid)>,
}
pub(super) type Reservations = Arc<Mutex<Map>>;

/// Replacing one immutable scope also invalidates prepared observations with
/// the same operation token. A newly enrolled inode must never hide behind an
/// unchanged token-only CAS.
fn same_basis(observed: &Map, current: &Map) -> bool {
    observed.len() == current.len()
        && observed.iter().all(|(token, scope)| {
            current
                .get(token)
                .is_some_and(|current| Arc::ptr_eq(scope, current))
        })
}

#[derive(Clone)]
pub(super) struct Home {
    path: PathBuf,
    canonical: PathBuf,
    pin: Option<Arc<File>>,
    identity: Option<(u64, u64)>,
    ancestors: BTreeSet<(u64, u64)>,
}

fn refusal(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}

fn pending_operation(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, true)
}

fn startup_failure(error: SetupError) -> ProtocolError {
    match error {
        SetupError::Invalid(message) => refusal(message),
        SetupError::Unavailable(message) => {
            ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, true)
        }
    }
}

fn filesystem_failure(message: String, error: &std::io::Error) -> SetupError {
    if matches!(
        error.raw_os_error(),
        Some(libc::EINVAL | libc::ENAMETOOLONG)
    ) {
        SetupError::Invalid(message)
    } else {
        SetupError::Unavailable(message)
    }
}

impl Home {
    fn capture(path: PathBuf) -> Result<Self, String> {
        Self::capture_for_start(path).map_err(|error| error.to_string())
    }

    fn capture_for_start(path: PathBuf) -> Result<Self, SetupError> {
        if !path.is_absolute() {
            return Err("Account homes must be absolute".into());
        }
        let canonical = normalize_for_start(&path)?;
        let pin = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&canonical)
        {
            Ok(file) => Some(Arc::new(file)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(filesystem_failure(
                    format!("Cannot reserve account home: {error}"),
                    &error,
                ));
            }
        };
        let identity = pin
            .as_ref()
            .map(|pin| pin.metadata())
            .transpose()
            .map_err(|error| filesystem_failure(error.to_string(), &error))?
            .map(|metadata| (metadata.dev(), metadata.ino()));
        let ancestors = physical_ancestors_for_start(&canonical, pin.as_deref())?;
        let home = Self {
            path,
            canonical,
            pin,
            identity,
            ancestors,
        };
        home.verify_for_start()?;
        Ok(home)
    }

    fn verify(&self) -> Result<(), String> {
        self.verify_for_start().map_err(|error| error.to_string())
    }

    fn verify_for_start(&self) -> Result<(), SetupError> {
        if normalize_for_start(&self.path)? != self.canonical {
            return Err("The account home changed while its operation was pending; reload accounts and retry".into());
        }
        match (&self.pin, std::fs::metadata(&self.path)) {
            (Some(pin), Ok(current)) => {
                let original = pin.metadata().map_err(|error| filesystem_failure(error.to_string(), &error))?;
                if current.is_dir() && (current.dev(), current.ino()) == (original.dev(), original.ino()) { Ok(()) }
                else { Err("The account home was replaced while its operation was pending".into()) }
            }
            (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            (_, Err(error)) => Err(filesystem_failure("The account home appeared, disappeared or became inaccessible during its operation".into(), &error)),
            _ => Err("The account home appeared, disappeared or became inaccessible during its operation".into()),
        }
    }

    fn refresh_ancestors(&mut self) -> Result<(), String> {
        self.refresh_ancestors_for_start()
            .map_err(|error| error.to_string())
    }

    fn refresh_ancestors_for_start(&mut self) -> Result<(), SetupError> {
        if let Some(pin) = &self.pin {
            // Once cleanup unlinks this exact root, it has no current
            // descendants; retain the original lexical guard until Drop.
            if pin
                .metadata()
                .map_err(|error| filesystem_failure(error.to_string(), &error))?
                .nlink()
                == 0
            {
                return Ok(());
            }
            self.ancestors.extend(physical_ancestors_for_start(
                &self.canonical,
                Some(pin.as_ref()),
            )?);
        }
        Ok(())
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.canonical.starts_with(&other.canonical)
            || other.canonical.starts_with(&self.canonical)
            || self
                .identity
                .is_some_and(|identity| other.ancestors.contains(&identity))
            || other
                .identity
                .is_some_and(|identity| self.ancestors.contains(&identity))
    }
}

/// Walk the actual directory parent chain through descriptors. Lexical
/// aliases can change when a reserved home/provider parent is renamed.
pub(super) fn physical_ancestors(
    path: &Path,
    pin: Option<&File>,
) -> Result<BTreeSet<(u64, u64)>, String> {
    physical_ancestors_for_start(path, pin).map_err(|error| error.to_string())
}

fn physical_ancestors_for_start(
    path: &Path,
    pin: Option<&File>,
) -> Result<BTreeSet<(u64, u64)>, SetupError> {
    let mut cursor = if let Some(pin) = pin {
        pin.try_clone()
            .map_err(|error| filesystem_failure(error.to_string(), &error))?
    } else {
        let mut existing = path;
        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(existing)
            {
                Ok(file) => break file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    existing = existing
                        .parent()
                        .ok_or("Directory scope has no existing directory ancestor")?;
                }
                Err(error) => {
                    return Err(filesystem_failure(
                        format!("Cannot capture directory scope ancestry: {error}"),
                        &error,
                    ));
                }
            }
        }
    };
    let mut identities = BTreeSet::new();
    for _ in 0..256 {
        let metadata = cursor
            .metadata()
            .map_err(|error| filesystem_failure(error.to_string(), &error))?;
        let identity = (metadata.dev(), metadata.ino());
        if !identities.insert(identity) {
            return Ok(identities);
        }
        // SAFETY: the parent component is fixed and the owned descriptor
        // outlives openat; the returned descriptor is newly owned.
        let parent = unsafe {
            libc::openat(
                cursor.as_raw_fd(),
                c"..".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if parent < 0 {
            let error = std::io::Error::last_os_error();
            return Err(filesystem_failure(
                format!("Cannot capture directory scope parent: {error}"),
                &error,
            ));
        }
        cursor = unsafe { File::from_raw_fd(parent) };
    }
    Err("Directory scope ancestry exceeds the directory bound".into())
}

/// Resolve aliases even for a not-yet-created descendant without interpreting
/// `..` through a missing directory. Missing-home cleanup remains possible.
fn normalize_for_start(path: &Path) -> Result<PathBuf, SetupError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("Account home must be an absolute path without parent traversal".into());
    }
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut resolved) => {
                for part in suffix.into_iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    existing
                        .file_name()
                        .ok_or("Account home has no existing parent")?
                        .to_owned(),
                );
                existing = existing
                    .parent()
                    .ok_or("Account home has no existing parent")?;
            }
            Err(error) => {
                return Err(filesystem_failure(
                    format!("Cannot resolve account home: {error}"),
                    &error,
                ));
            }
        }
    }
}

pub(crate) struct Reservation {
    map: Reservations,
    token: uuid::Uuid,
    host: std::sync::Weak<super::state::HostLock>,
    accounts: Vec<Account>,
    create: Option<(crate::agent::Provider, uuid::Uuid)>,
    store: Store,
    /// How long each step asks again for an account-list lock another
    /// process holds. See [`crate::account::registry::patiently`].
    patience: std::time::Duration,
    /// Only relevant registrations, not the global revision: unrelated edits
    /// must be preserved while a provider is taking time to sign out.
    registered: Vec<(
        uuid::Uuid,
        Option<crate::account::registry::Registered>,
        Option<crate::account::registry::Pending>,
        bool,
    )>,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.map
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.token);
    }
}
impl Reservation {
    /// Observe filesystem and registry state without global control admission.
    /// The short writer compares the account map's exact cached token basis
    /// and installs the immutable home token. Existing admitted starts
    /// prevent this writer from overtaking their check/effect boundary.
    fn install_restoring(
        host: &SharedHost,
        accounts: &[Account],
        restores: Option<(crate::agent::Provider, uuid::Uuid)>,
    ) -> Result<Self, ProtocolError> {
        let store = Store::installation().ok_or_else(|| refusal("Account registry unavailable"))?;
        Self::install_with_store(
            host,
            accounts,
            store,
            crate::account::home::Homes::installation().as_ref(),
            restores,
            crate::account::registry::LIST_PATIENCE,
        )
    }

    fn install_with_store(
        host: &SharedHost,
        accounts: &[Account],
        store: Store,
        installed_homes: Option<&crate::account::home::Homes>,
        restores: Option<(crate::agent::Provider, uuid::Uuid)>,
        patience: std::time::Duration,
    ) -> Result<Self, ProtocolError> {
        let gate = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .control_gate
            .clone();
        super::client::ensure_control_open(host)?;
        let map = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .account_lifecycle
            .clone();
        let homes = accounts
            .iter()
            .map(|account| {
                account
                    .resolved_home()
                    .ok_or_else(|| "This account has no credential directory".to_owned())
                    .and_then(Home::capture)
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(refusal)?;
        // Nothing is held yet: no lease, no control admission.
        let registry = crate::account::registry::patiently(patience, || store.try_load())
            .map_err(|error| super::client::lifecycle_refusal(error.into()))?;
        // The request's account lookup happened before this snapshot. Never
        // pair an old linked path with a newer registration under the same ID.
        let current_accounts = registry.all_accounts(installed_homes);
        for account in accounts {
            if let Some(key) = account.registry_key() {
                if registry.find(key).is_some()
                    && !current_accounts.iter().any(|current| current == account)
                {
                    return Err(refusal(
                        "The selected account registration changed during admission; reload and retry",
                    ));
                }
                if registry
                    .pending()
                    .iter()
                    .any(|pending| pending.key == key && pending.provider != account.provider)
                {
                    return Err(refusal(
                        "The selected pending provider changed during admission; reload and retry",
                    ));
                }
            }
        }
        let registered = accounts
            .iter()
            .filter_map(Account::registry_key)
            .map(|key| {
                (
                    key,
                    registry.find(key).cloned(),
                    registry
                        .pending()
                        .iter()
                        .find(|pending| pending.key == key)
                        .cloned(),
                    registry.is_archived(key),
                )
            })
            .collect();
        let token = uuid::Uuid::new_v4();
        let (previous_basis, mut previous_homes) = {
            let entries = map.lock().unwrap_or_else(|p| p.into_inner());
            (
                entries.clone(),
                entries
                    .values()
                    .flat_map(|entry| entry.homes.iter().cloned())
                    .collect::<Vec<_>>(),
            )
        };
        for home in &mut previous_homes {
            home.refresh_ancestors().map_err(refusal)?;
        }
        let writer = gate.try_exclusive().ok_or_else(super::control_gate::busy)?;
        super::client::ensure_control_open(host)?;
        let mut entries = map.lock().unwrap_or_else(|p| p.into_inner());
        if !same_basis(&previous_basis, &entries) {
            return Err(refusal(
                "Account operations changed during reservation; retry",
            ));
        }
        if entries.len() >= 64 {
            return Err(refusal(
                "Too many account operations are already pending; wait and retry",
            ));
        }
        if previous_homes
            .iter()
            .any(|other| homes.iter().any(|home| home.overlaps(other)))
        {
            return Err(refusal(
                "An operation is already changing this account home. Wait for it to finish, then retry.",
            ));
        }
        entries.insert(token, Arc::new(Reserved { homes, restores }));
        drop(entries);
        drop(writer);
        let reservation = Self {
            map,
            token,
            host: Arc::downgrade(host),
            accounts: accounts.to_vec(),
            create: None,
            registered,
            store,
            patience,
        };
        // No effects have happened. Refuse any filesystem/registration change
        // across observation and installation while the installed token fences
        // subsequent related ingress.
        reservation
            .verify()
            .map_err(|error| super::client::lifecycle_refusal(error.into()))?;
        reservation.ensure_no_active_owners(host)?;
        Ok(reservation)
    }

    #[cfg(test)]
    pub(crate) fn test_host() -> SharedHost {
        tests::host()
    }

    #[cfg(test)]
    pub(crate) fn test_install(
        host: &SharedHost,
        accounts: &[Account],
        store: Store,
        homes: &crate::account::home::Homes,
    ) -> Result<Self, ProtocolError> {
        Self::install_with_store(
            host,
            accounts,
            store,
            Some(homes),
            None,
            crate::account::registry::LIST_PATIENCE,
        )
    }

    /// Asks again for a held account-list lock for only `patience`.
    #[cfg(test)]
    pub(crate) fn with_patience(mut self, patience: std::time::Duration) -> Self {
        self.patience = patience;
        self
    }

    fn ensure_no_active_owners(&self, host: &SharedHost) -> Result<(), ProtocolError> {
        // The installed token prevents new related startups. Copy only actual
        // execution-root metadata while holding HostState, then resolve paths
        // outside that lock and outside the global control writer.
        // An agent observation reading a reserved home was admitted before the
        // token, since later ones are refused. Wait for it to end.
        let probes = Arc::clone(&host.lock().unwrap_or_else(|p| p.into_inner()).agent_probes);
        let reading = probes
            .running()
            .into_iter()
            .filter(|(_, probe)| {
                ensure_credential_roots(
                    host,
                    &probe.working_directory,
                    &probe.environment,
                    &probe.unset,
                    false,
                )
                .is_err()
            })
            .map(|(id, _)| id)
            .collect();
        if !probes.wait_for(&reading).is_empty() {
            return Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "Cagents is still checking an agent that uses this account. Try again in a moment.",
                true,
            ));
        }
        let owners = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let mut owners = state
                .terminals
                .values()
                .filter(|record| !record.handle.is_completed())
                .map(|record| {
                    let launch = record.execution_launch();
                    (
                        launch.working_directory.clone(),
                        record
                            .execution_environment
                            .as_ref()
                            .map(|value| value.0.clone())
                            .unwrap_or_else(|| launch.environment.clone()),
                        crate::account::launch_credential_overrides(launch),
                        record.execution_environment.is_none(),
                    )
                })
                .collect::<Vec<_>>();
            owners.extend(
                state
                    .chats
                    .values()
                    .filter(|record| record.handle.owns_work())
                    .map(|record| {
                        (
                            record.session.launch.recipe.working_directory.clone(),
                            record.handle.execution_environment().0,
                            crate::account::launch_credential_overrides(
                                &record.session.launch.recipe,
                            ),
                            false,
                        )
                    }),
            );
            owners
        };
        for (cwd, environment, unset, inherit) in owners {
            require_absolute_roots(&environment, &unset, inherit)?;
            ensure_credential_roots(host, &cwd, &environment, &unset, inherit)?;
        }
        Ok(())
    }

    /// Reserve the exact future home BEFORE mkdir. The random key is only a
    /// negative admission target; creation still requires a successful actual
    /// nonrecursive mkdir and its original receipt, never an existing home.
    pub(super) fn for_operation(
        host: &SharedHost,
        operation: &crate::account::remote::Operation,
    ) -> Result<Self, ProtocolError> {
        let mut accounts = crate::account::remote::affected_accounts(operation)
            .map_err(super::client::lifecycle_refusal)?;
        let create = if let crate::account::remote::Operation::Create { provider, .. } = operation {
            let homes = crate::account::home::Homes::installation()
                .ok_or_else(|| refusal("Account homes unavailable"))?;
            let key = uuid::Uuid::new_v4();
            accounts.push(Account::managed(*provider, key, homes.path(*provider, key)));
            Some((*provider, key))
        } else {
            None
        };
        let restores = match operation {
            crate::account::remote::Operation::Restore { provider, key } => Some((*provider, *key)),
            _ => None,
        };
        let mut reservation = Self::install_restoring(host, &accounts, restores)?;
        if create.is_some()
            && reservation
                .current()
                .map_err(refusal)?
                .homes
                .last()
                .is_some_and(|home| home.pin.is_some())
        {
            return Err(refusal(
                "The planned new home already exists; it was not adopted or changed",
            ));
        }
        reservation.create = create;
        Ok(reservation)
    }

    fn current(&self) -> Result<Arc<Reserved>, String> {
        self.map
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&self.token)
            .cloned()
            .ok_or_else(|| {
                "The account mutation reservation was released; reload accounts and retry".into()
            })
    }

    /// Creates the reserved fresh home and records its pending sign-in.
    ///
    /// Enrolling the home needs the control gate's writer for a moment, and
    /// a person who asked for the account is waiting for the answer, so a
    /// busy gate is waited out for up to [`ENROLLMENT_PATIENCE`]. Each busy
    /// attempt has already withdrawn its directory and released the registry
    /// and lifecycle leases, so the wait holds nothing a request admitted to
    /// the gate could be blocked on. A refusal that outlasts the patience is
    /// still [`EnrollmentRefused::Busy`](crate::account::home::EnrollmentRefused),
    /// with nothing left behind.
    pub(crate) fn create_pending_home(
        &self,
        store: &Store,
        homes: &crate::account::home::Homes,
        provider: crate::agent::Provider,
        started_at: u64,
        system: Option<&Path>,
        config: crate::account::home::CodexConfig,
    ) -> std::io::Result<(uuid::Uuid, PathBuf)> {
        self.create_pending_home_within(
            store,
            homes,
            provider,
            started_at,
            system,
            config,
            ENROLLMENT_PATIENCE,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the same operands as create_pending_home, plus its patience"
    )]
    fn create_pending_home_within(
        &self,
        store: &Store,
        homes: &crate::account::home::Homes,
        provider: crate::agent::Provider,
        started_at: u64,
        system: Option<&Path>,
        config: crate::account::home::CodexConfig,
        patience: std::time::Duration,
    ) -> std::io::Result<(uuid::Uuid, PathBuf)> {
        self.retry_busy_enrollment(std::time::Instant::now() + patience, || {
            // A held lock refuses before the home is made, so asking again
            // for it leaves nothing behind.
            self.patiently(|| {
                self.verify_once()?;
                store.try_create_pending_reserved(self, homes, provider, started_at, || {
                    self.create_home_initialized(homes, provider, system, config, |_| Ok(()))
                })
            })
        })
    }

    /// Makes `attempt` again while its enrollment is refused as busy, at most
    /// [`ENROLLMENT_ATTEMPTS`] times in all and never after `deadline`. The
    /// gate can be free each time it is looked at and taken again before
    /// enrollment asks, so neither bound alone ends the retries.
    fn retry_busy_enrollment<T>(
        &self,
        deadline: std::time::Instant,
        mut attempt: impl FnMut() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        use crate::account::home::EnrollmentRefused;
        let mut attempts = 1;
        loop {
            match attempt() {
                Err(error)
                    if matches!(EnrollmentRefused::of(&error), Some(EnrollmentRefused::Busy))
                        && attempts < ENROLLMENT_ATTEMPTS
                        && std::time::Instant::now() < deadline
                        && self.await_writer(deadline) =>
                {
                    attempts += 1;
                }
                result => return result,
            }
        }
    }

    /// Waits, holding no lease, until the control gate would admit a writer,
    /// or `false` at `deadline`. It does not queue, so it holds back no other
    /// request, and it keeps the admission only for the instant it is
    /// observed: enrollment takes its own when it is ready.
    fn await_writer(&self, deadline: std::time::Instant) -> bool {
        let Some(host) = self.host.upgrade() else {
            return false;
        };
        let gate = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .control_gate
            .clone();
        drop(host);
        gate.exclusive_when_free_until(deadline).is_some()
    }

    fn create_home_initialized(
        &self,
        homes: &crate::account::home::Homes,
        provider: crate::agent::Provider,
        system: Option<&Path>,
        config: crate::account::home::CodexConfig,
        initialized: impl FnOnce(&crate::account::home::NewHomeCreationReceipt) -> std::io::Result<()>,
    ) -> std::io::Result<(uuid::Uuid, PathBuf)> {
        let (planned_provider, key) = self
            .create
            .ok_or_else(|| std::io::Error::other("Account creation has no reserved fresh home"))?;
        let path = homes.path(provider, key);
        if provider != planned_provider
            || !self.accounts.iter().any(|account| {
                account.provider == provider
                    && account.registry_key() == Some(key)
                    && account.resolved_home().as_ref() == Some(&path)
            })
        {
            return Err(std::io::Error::other(
                "Account creation does not match its planned home",
            ));
        }
        let current = self.current().map_err(std::io::Error::other)?;
        let original_system = self
            .accounts
            .iter()
            .position(|account| account.provider == provider && account.is_system())
            .and_then(|index| current.homes.get(index))
            .map(|home| home.path.as_path());
        if system != original_system {
            return Err(std::io::Error::other(
                "Account seeding does not match its reserved System home",
            ));
        }
        homes.create_planned_with_config_initialized(provider, key, system, config, |created| {
            self.enroll_created_home(created)?;
            initialized(created)?;
            // The initializer can pause before seeding. Revalidate every
            // original operand again without reentering the held Store lease.
            for home in &self.current().map_err(std::io::Error::other)?.homes {
                home.verify().map_err(std::io::Error::other)?;
            }
            Ok(())
        })
    }

    /// Runs before any seed under the already-held registry/lifecycle leases.
    /// Never load the registry here. Observe original FDs/maps off the control
    /// writer, then atomically replace this immutable roster under a brief
    /// try-write. It never waits for that writer: a request admitted to the
    /// gate may be blocking on the very leases held here. A busy gate,
    /// reservations that changed meanwhile, or a daemon that stopped
    /// admitting changes is an
    /// [`EnrollmentRefused`](crate::account::home::EnrollmentRefused), on
    /// which creation withdraws the untouched directory; any other failure
    /// retains the fresh files. Neither publishes a row.
    fn enroll_created_home(
        &self,
        created: &crate::account::home::NewHomeCreationReceipt,
    ) -> std::io::Result<()> {
        use std::io;
        if self.create != Some((created.provider(), created.key())) {
            return Err(io::Error::other(
                "Fresh home does not match its creation reservation",
            ));
        }
        created.revalidate()?;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| io::Error::other("The account host is no longer available"))?;
        let basis = self.map.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let original = basis
            .get(&self.token)
            .ok_or_else(|| io::Error::other("The creation reservation was released"))?;
        let index = self
            .accounts
            .iter()
            .position(|account| {
                account.provider == created.provider()
                    && account.registry_key() == Some(created.key())
            })
            .ok_or_else(|| {
                io::Error::other("The planned home is absent from its creation reservation")
            })?;
        let planned = original
            .homes
            .get(index)
            .ok_or_else(|| io::Error::other("The planned home scope changed"))?;
        if planned.pin.is_some() || planned.canonical != created.home() {
            return Err(io::Error::other(
                "Fresh home differs from its original planned directory",
            ));
        }
        let pin = Arc::new(created.directory().try_clone()?);
        let metadata = pin.metadata()?;
        let actual = Home {
            path: planned.path.clone(),
            canonical: created.home().to_owned(),
            ancestors: physical_ancestors(created.home(), Some(&pin)).map_err(io::Error::other)?,
            identity: Some((metadata.dev(), metadata.ino())),
            pin: Some(pin),
        };
        actual.verify().map_err(io::Error::other)?;
        for (i, home) in original.homes.iter().enumerate() {
            if i != index {
                home.verify().map_err(io::Error::other)?;
            }
        }
        let mut other_homes = basis
            .iter()
            .filter(|(token, _)| **token != self.token)
            .flat_map(|(_, scope)| scope.homes.iter().cloned())
            .collect::<Vec<_>>();
        for home in &mut other_homes {
            home.refresh_ancestors().map_err(io::Error::other)?;
        }
        if other_homes.iter().any(|home| actual.overlaps(home)) {
            return Err(io::Error::other(
                "Another account operation overlaps the new home",
            ));
        }
        let mut updated = (**original).clone();
        updated.homes[index] = actual;
        let gate = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .control_gate
            .clone();
        let writer = gate
            .try_exclusive()
            .ok_or_else(|| crate::account::home::EnrollmentRefused::Busy.into_error())?;
        super::client::ensure_control_open(&host).map_err(|error| {
            crate::account::home::EnrollmentRefused::Closed(error.message).into_error()
        })?;
        let mut entries = self.map.lock().unwrap_or_else(|p| p.into_inner());
        if !same_basis(&basis, &entries) {
            // Another operation reserved or released in the meantime. Like a
            // busy gate, that leaves this home untouched and worth another try.
            return Err(crate::account::home::EnrollmentRefused::Busy.into_error());
        }
        entries.insert(self.token, Arc::new(updated));
        drop(entries);
        drop(writer);
        created.revalidate()?;
        Ok(())
    }

    /// Match the exact request operand/provider/key captured by this host
    /// reservation. The original alias must still name its pinned directory;
    /// return the captured canonical operand, never resolve a new request path.
    pub(crate) fn link_home(
        &self,
        provider: crate::agent::Provider,
        key: uuid::Uuid,
        operand: &Path,
    ) -> Result<PathBuf, String> {
        let current = self.current()?;
        let home = self.accounts.iter().zip(&current.homes).find_map(|(account, home)| {
            (account.provider == provider && matches!(&account.home,
                crate::account::AccountHome::Linked { key: captured, path } if *captured == key && path == operand)).then_some(home)
        }).ok_or("The linked account operand does not match its physical reservation")?;
        home.verify()?;
        Ok(home.canonical.clone())
    }

    /// The immutable physical token outlives validation and registry effects.
    /// Related ingress refuses that token. Shutdown/upgrade refuses any installed token. No
    /// global reader/writer spans slow storage, so unrelated Input remains
    /// independent even when another scoped writer is awaiting admission.
    /// Store revalidates relevant metadata under its actual mutation lease.
    ///
    /// `effect` takes its locks without waiting. While another process holds
    /// one, verification and `effect` are made again for a moment, holding
    /// nothing in between; past that the refusal is
    /// [`LifecycleError::ListHeld`](crate::account::remote::LifecycleError).
    pub(crate) fn commit<T>(
        &self,
        mut effect: impl FnMut() -> std::io::Result<T>,
    ) -> Result<T, crate::account::remote::LifecycleError> {
        self.patiently(|| {
            self.verify_once()?;
            effect()
        })
        .map_err(Into::into)
    }

    /// Makes `attempt` again while another process holds an account-list
    /// lock it asked for, for as long as this reservation's patience.
    pub(crate) fn patiently<T>(
        &self,
        attempt: impl FnMut() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        crate::account::registry::patiently(self.patience, attempt)
    }

    /// Call outside HostState and before each destructive stage/final commit.
    pub(crate) fn verify(&self) -> std::io::Result<()> {
        self.patiently(|| self.verify_once())
    }

    /// [`Self::verify`], refusing at once if the registry lock is held.
    fn verify_once(&self) -> std::io::Result<()> {
        if !self
            .map
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&self.token)
        {
            return Err(std::io::Error::other(
                "The account mutation reservation was released; reload accounts and retry",
            ));
        }
        let registry = self.store.try_load()?;
        self.verify_registry(&registry)
            .map_err(std::io::Error::other)
    }

    /// Invoked by Store under the SAME registry lease as its mutation; never
    /// reacquire the registry flock here. Physical validation precedes that
    /// lease in commit(), while this cut closes concurrent metadata races.
    pub(crate) fn verify_registry(&self, registry: &Registry) -> Result<(), String> {
        let current = self.current()?;
        for home in &current.homes {
            home.verify()?;
        }
        for (key, account, pending, archived) in &self.registered {
            if registry.find(*key) != account.as_ref()
                || registry.pending().iter().find(|value| value.key == *key) != pending.as_ref()
                || registry.is_archived(*key) != *archived
            {
                return Err(
                    "The affected account registration changed; reload accounts before retrying"
                        .into(),
                );
            }
        }
        Ok(())
    }
}

/// How many times a sweep asks again about a home whose check was refused by
/// something busy - a window holding the registry lock, another probe of the
/// same provider, a reservation that overlaps it.
const RESTORE_ATTEMPTS: u32 = 3;
const RESTORE_RETRY: std::time::Duration = std::time::Duration::from_secs(2);
/// How often the daemon looks for homes the account list has lost since it
/// last looked. A look is two directory listings and a registry read; only a
/// home not yet examined is asked about.
const RESTORE_SCAN: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a requested restore waits out other control operations - the
/// retirement of the start its account refused, typically - before saying the
/// daemon is busy.
const RESTORE_ADMISSION: std::time::Duration = std::time::Duration::from_secs(5);
/// How long it waits for another restore of the same home - the daemon's own
/// sweep, or another window - to finish. Once that one has listed the home,
/// this one answers without asking the provider; if it did not, this one
/// still asks twice, and the whole answer must reach the window before its
/// request deadline.
const RESTORE_OVERLAP: std::time::Duration = std::time::Duration::from_secs(
    crate::control::DEVICE_OPERATION_DEADLINE.as_secs()
        - 2 * crate::account::probe::TIMEOUT.as_secs()
        - 10,
);
/// How long creating an account waits for the control gate to admit the
/// moment's writer that enrolls its new home. The person who asked is
/// waiting for the answer either way; past this, they are told the daemon is
/// busy and nothing was created.
const ENROLLMENT_PATIENCE: std::time::Duration = std::time::Duration::from_secs(2);
/// How many times in all creating an account tries to enroll its new home
/// while the daemon is busy. Each try makes and withdraws a directory.
const ENROLLMENT_ATTEMPTS: usize = 3;

/// The most sweeps a home whose check did not finish - its provider timed
/// out, say - is passed over before it is asked about again.
const RESTORE_BACKOFF_LIMIT: u32 = 64;

/// Reserves a requested restore. The chat asks for one as soon as a start is
/// refused, alongside its own cleanup of that start, so a busy control gate
/// is waited out rather than reported, as is another restore of the same
/// home already asking its provider: the restore changes nothing until the
/// provider confirms the login, so arriving a moment later is harmless, and
/// once the other one has listed the home this one reports it registered.
pub(super) fn reserve_restore(
    host: &SharedHost,
    operation: &crate::account::remote::Operation,
) -> Result<Reservation, ProtocolError> {
    let target = match operation {
        crate::account::remote::Operation::Restore { provider, key } => Some((*provider, *key)),
        _ => None,
    };
    let restoring = || {
        target.is_some_and(|target| {
            let map = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .account_lifecycle
                .clone();
            map.lock()
                .unwrap_or_else(|p| p.into_inner())
                .values()
                .any(|reserved| reserved.restores == Some(target))
        })
    };
    let started = std::time::Instant::now();
    loop {
        let overlapped = restoring();
        match Reservation::for_operation(host, operation) {
            Err(error)
                if (error.retryable && started.elapsed() < RESTORE_ADMISSION)
                    || ((overlapped || restoring()) && started.elapsed() < RESTORE_OVERLAP) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            result => return result,
        }
    }
}

/// Keeps the managed homes a lost or reset account list no longer names
/// usable, so their accounts and the conversations pinned to them work
/// without any manual step: at startup, and when the list loses rows while
/// the daemon runs.
///
/// Each home is verified by asking its provider CLI, so this runs on its own
/// thread, one home at a time, each under its own reservation and through
/// the same operation an explicit restore uses. A home that was examined and
/// left alone - its provider reports no login, say - is not asked about again
/// by this daemon; an explicit restore still can. One whose check did not
/// finish is asked about again after a number of sweeps that doubles each
/// time; so is one whose check panicked.
pub(super) fn restore_unregistered_homes(
    host: std::sync::Weak<super::state::HostLock>,
) -> std::io::Result<()> {
    use super::service::Next;
    let mut sweep = RestoreSweep::default();
    super::service::periodic("cagents-account-restore", RESTORE_SCAN, move || {
        if restore_pass(&host, &mut sweep) {
            Next::Continue
        } else {
            Next::Stop
        }
    })
    .map(drop)
}

/// What the sweep remembers between passes.
#[derive(Default)]
pub(super) struct RestoreSweep {
    /// Homes examined and left as they are.
    pub(super) examined: BTreeSet<(crate::agent::Provider, uuid::Uuid)>,
    /// Homes whose check did not finish: how many times in a row, and how
    /// many more sweeps pass them over.
    pub(super) deferred: BTreeMap<(crate::agent::Provider, uuid::Uuid), (u32, u32)>,
}

impl RestoreSweep {
    /// Whether this pass skips the home, counting the pass if so.
    fn defers(&mut self, home: (crate::agent::Provider, uuid::Uuid)) -> bool {
        if self.examined.contains(&home) {
            return true;
        }
        match self.deferred.get_mut(&home) {
            Some((_, remaining)) if *remaining > 0 => {
                *remaining -= 1;
                true
            }
            _ => false,
        }
    }

    fn unfinished(&mut self, home: (crate::agent::Provider, uuid::Uuid)) {
        let (failures, remaining) = self.deferred.entry(home).or_default();
        *failures = failures.saturating_add(1);
        *remaining = 1u32
            .checked_shl(*failures - 1)
            .unwrap_or(u32::MAX)
            .min(RESTORE_BACKOFF_LIMIT);
    }
}

/// One sweep. `false` once the daemon has stopped.
pub(super) fn restore_pass(
    host: &std::sync::Weak<super::state::HostLock>,
    sweep: &mut RestoreSweep,
) -> bool {
    let (Some(store), Some(homes)) = (
        Store::installation(),
        crate::account::home::Homes::installation(),
    ) else {
        return false;
    };
    let registry = match store.load() {
        Ok(registry) if !registry.is_newer() => registry,
        Ok(_) => return host.strong_count() > 0,
        Err(error) => {
            tracing::debug!(%error, "the account list could not be read to restore lost accounts");
            return host.strong_count() > 0;
        }
    };
    for (provider, key, _) in homes.unregistered(&registry) {
        if sweep.defers((provider, key)) {
            continue;
        }
        if registry.is_released(key) {
            // Released before releases were marked beside their homes, or its
            // marker could not be written then. The list never forgets a
            // release whose home is kept, so record it where it survives the
            // list's loss. A home whose marker cannot be written is retried
            // by the next daemon.
            if let Err(error) = homes.mark_released(provider, key) {
                sweep.examined.insert((provider, key));
                tracing::warn!(
                    provider = provider.executable(),
                    %key,
                    %error,
                    "could not mark a released sign-in's home"
                );
            }
            continue;
        }
        let operation = crate::account::remote::Operation::Restore { provider, key };
        for attempt in 1..=RESTORE_ATTEMPTS {
            // Never keep a stopping daemon's state alive, not even while the
            // provider is being asked: the reservation holds only its token.
            let Some(strong) = host.upgrade() else {
                return false;
            };
            // A home whose restore panics is passed over like one whose check
            // did not finish, so it cannot keep the homes after it waiting.
            let result = super::service::contain("account restore", || {
                let reservation = Reservation::for_operation(&strong, &operation);
                drop(strong);
                reservation
                    .map_err(|error| crate::account::remote::RestoreFailure::from(error.message))
                    .and_then(|reservation| {
                        crate::account::remote::restore_installation_home(
                            &reservation,
                            provider,
                            key,
                        )
                    })
            });
            let result = match result {
                Ok(result) => result,
                Err(panicked) => {
                    sweep.unfinished((provider, key));
                    tracing::error!(
                        provider = provider.executable(),
                        %key,
                        %panicked,
                        "an unlisted account home could not be checked"
                    );
                    break;
                }
            };
            match result {
                Ok(restored) => {
                    sweep.deferred.remove(&(provider, key));
                    tracing::info!(
                        provider = provider.executable(),
                        %key,
                        restored = ?restored.restored,
                        "restored an account the account list had lost"
                    );
                    break;
                }
                Err(failure) if failure.settled => {
                    sweep.deferred.remove(&(provider, key));
                    sweep.examined.insert((provider, key));
                    tracing::info!(
                        provider = provider.executable(),
                        %key,
                        reason = %failure.error,
                        "an unlisted account home was left unchanged"
                    );
                    break;
                }
                Err(failure) if attempt == RESTORE_ATTEMPTS => {
                    sweep.unfinished((provider, key));
                    tracing::debug!(
                        provider = provider.executable(),
                        %key,
                        reason = %failure.error,
                        "an unlisted account home could not be checked yet"
                    );
                    break;
                }
                Err(_) => std::thread::sleep(RESTORE_RETRY),
            }
        }
    }
    host.strong_count() > 0
}

/// Caller already owns shared admission through its ensuing startup/write.
/// Clone reservation metadata, then perform path resolution outside all locks.
pub(super) fn ensure_paths(
    host: &SharedHost,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<(), ProtocolError> {
    let map = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .account_lifecycle
        .clone();
    let mut reserved: Vec<_> = map
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .flat_map(|entry| entry.homes.iter())
        .cloned()
        .collect();
    if reserved.is_empty() {
        return Ok(());
    }
    for home in &mut reserved {
        home.refresh_ancestors_for_start()
            .map_err(startup_failure)?;
    }
    for path in paths {
        let home = Home::capture_for_start(path).map_err(startup_failure)?;
        if reserved.iter().any(|other| home.overlaps(other)) {
            return Err(pending_operation(
                "This account home is being changed. Wait for the account operation to finish before using or editing it.",
            ));
        }
    }
    Ok(())
}

/// Admits a launch at `cwd`: outside any worktree being removed, and with
/// credential roots clear of pending account operations.
pub(super) fn ensure_launch(
    host: &SharedHost,
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    unset: &BTreeSet<String>,
    inherit: bool,
) -> Result<(), ProtocolError> {
    super::worktree_removal::ensure_outside(host, cwd)?;
    ensure_credential_roots(host, cwd, environment, unset, inherit)
}

fn ensure_credential_roots(
    host: &SharedHost,
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    unset: &BTreeSet<String>,
    inherit: bool,
) -> Result<(), ProtocolError> {
    let mut paths = Vec::new();
    let mut unknown_root = false;
    let effective = |name: &str| {
        if unset.contains(name) {
            None
        } else {
            environment
                .get(name)
                .map(std::ffi::OsString::from)
                .or_else(|| inherit.then(|| std::env::var_os(name)).flatten())
        }
    };
    // Terminals can invoke either provider later. Never infer credential-root
    // ownership from a program basename or the provider of a registry alias.
    for (variable, fallback) in [("CLAUDE_CONFIG_DIR", ".claude"), ("CODEX_HOME", ".codex")] {
        let selected = effective(variable);
        // The main Codex constructor treats an explicit empty home as cwd.
        // Do not prove another physical home from HOME fallback while an
        // account operation is pending. No reservation keeps legacy behavior.
        if variable == "CODEX_HOME" && selected.as_ref().is_some_and(|value| value.is_empty()) {
            unknown_root = true;
        }
        if let Some(selected) = selected.filter(|value| !value.is_empty()) {
            paths.push(cwd.join(selected));
        } else if let Some(home) = effective("HOME").filter(|value| !value.is_empty()) {
            // Main's Claude saved-history fallback resolves a relative HOME
            // against daemon cwd, unlike the spawned provider's launch cwd.
            // Admission cannot infer a single physical root from that value.
            if variable == "CLAUDE_CONFIG_DIR" && Path::new(&home).is_relative() {
                unknown_root = true;
            }
            paths.push(cwd.join(home).join(fallback));
        } else {
            unknown_root = true;
        }
    }
    ensure_paths(host, paths)?;
    if unknown_root && reservations_active(host) {
        return Err(pending_operation(
            "Cannot establish both provider credential directories while an account operation is pending. Set absolute HOME or provider homes, or wait for that operation to finish.",
        ));
    }
    Ok(())
}

/// An operational directory FD can outlive or move away from the recipe's
/// issued cwd. Relative credential roots would then be checked against a
/// different directory from the one the child uses. Refuse that ambiguity
/// before spawn; absolute roots still enter the ordinary reservation checks.
pub(super) fn ensure_launch_at_directory(
    host: &SharedHost,
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    unset: &BTreeSet<String>,
    inherit: bool,
    execution_directory: Option<&File>,
) -> Result<(), ProtocolError> {
    if let Some(directory) = execution_directory {
        let metadata = directory.metadata().map_err(|error| {
            startup_failure(filesystem_failure(
                format!("Cannot inspect the pinned terminal directory: {error}"),
                &error,
            ))
        })?;
        if !metadata.is_dir() || metadata.nlink() == 0 {
            return Err(refusal(
                "The pinned terminal directory is no longer available",
            ));
        }
        require_absolute_roots(environment, unset, inherit)?;
        super::worktree_removal::ensure_outside_directory(host, directory)?;
    }
    // Keep the normal admission path,  authoritative for every absolute or unspecified root.
    ensure_launch(host, cwd, environment, unset, inherit)
}

fn require_absolute_roots(
    environment: &BTreeMap<String, String>,
    unset: &BTreeSet<String>,
    inherit: bool,
) -> Result<(), ProtocolError> {
    let effective = |name: &str| {
        if unset.contains(name) {
            None
        } else {
            environment
                .get(name)
                .map(std::ffi::OsString::from)
                .or_else(|| inherit.then(|| std::env::var_os(name)).flatten())
        }
    };
    // Both providers remain usable from an ordinary terminal, regardless
    // of which provider initiated the handoff or owns a reserved account.
    for variable in ["CLAUDE_CONFIG_DIR", "CODEX_HOME"] {
        let selected = effective(variable).filter(|value| !value.is_empty());
        let (name, root) = match selected {
            Some(root) => (variable, Some(root)),
            None => ("HOME", effective("HOME").filter(|value| !value.is_empty())),
        };
        if root
            .as_ref()
            .is_some_and(|root| Path::new(root).is_relative())
        {
            return Err(refusal(format!(
                "Cannot launch with relative {name} while using a pinned working directory. Set an absolute credential directory and retry."
            )));
        }
    }
    Ok(())
}

fn reservations_active(host: &SharedHost) -> bool {
    let map = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .account_lifecycle
        .clone();
    !map.lock().unwrap_or_else(|p| p.into_inner()).is_empty()
}

/// Legacy adopted terminals have no newly observed process environment or cwd.
/// Absolute captured roots are usable; unknown/relative provenance waits until
/// account mutation completes rather than guessing from today's daemon HOME.
pub(super) fn ensure_terminal_adoption(
    host: &SharedHost,
    environment: Option<&BTreeMap<String, String>>,
    unset: &BTreeSet<String>,
) -> Result<(), ProtocolError> {
    if !reservations_active(host) {
        return Ok(());
    }
    let Some(environment) = environment else {
        return Err(refusal(
            "The retained terminal has no captured account environment; wait for the account operation to finish",
        ));
    };
    require_absolute_roots(environment, unset, false)
}

pub(super) fn ensure_update(host: &SharedHost, proposed: &Registry) -> Result<(), ProtocolError> {
    let current = Store::installation()
        .ok_or_else(|| refusal("Account registry unavailable"))?
        .try_load()
        // Under shared control admission: refuse at once rather than wait.
        .map_err(|error| super::client::lifecycle_refusal(error.into()))?;
    let homes = crate::account::home::Homes::installation();
    let changed = current.changed_account_homes(proposed, homes.as_ref());
    ensure_paths(host, changed)
}

pub(super) fn ensure_idle(host: &SharedHost) -> Result<(), ProtocolError> {
    let map = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .account_lifecycle
        .clone();
    if map
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .all(|reserved| reserved.restores.is_some())
    {
        Ok(())
    } else {
        Err(pending_operation(
            "An account operation is still finishing. Wait for it before shutting down or upgrading this daemon.",
        ))
    }
}

/// Reattaching a completed pane must not add a reader while its retained
/// account history is being deleted. Copy only nonsecret home metadata.
pub(super) fn ensure_terminal(
    host: &SharedHost,
    id: crate::model::TerminalId,
) -> Result<(), ProtocolError> {
    let selected = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.terminals.get(&id).map(|record| {
            let launch = record.execution_launch();
            let environment = record
                .execution_environment
                .as_ref()
                .map(|value| &value.0)
                .unwrap_or(&launch.environment);
            let environment = ["HOME", "CLAUDE_CONFIG_DIR", "CODEX_HOME"]
                .into_iter()
                .filter_map(|name| {
                    environment
                        .get(name)
                        .map(|value| (name.into(), value.clone()))
                })
                .collect();
            (
                launch.working_directory.clone(),
                environment,
                crate::account::launch_credential_overrides(launch),
                record.execution_environment.is_none(),
            )
        })
    };
    if let Some((cwd, environment, unset, inherit)) = selected {
        ensure_terminal_adoption(host, (!inherit).then_some(&environment), &unset)?;
        ensure_launch(host, &cwd, &environment, &unset, inherit)?;
    }
    Ok(())
}

pub(super) fn admit(
    host: &SharedHost,
    request: &crate::terminal_ipc::ClientRequest,
) -> Result<(), ProtocolError> {
    use crate::terminal_ipc::ClientRequest;
    match request {
        ClientRequest::Attach { terminal_id, .. } => ensure_terminal(host, *terminal_id)?,
        ClientRequest::ChatAttach { .. }
        | ClientRequest::ChatReleaseInspect { .. }
        | ClientRequest::ChatRelease { .. }
        | ClientRequest::ChatReturnTerminal { .. }
        | ClientRequest::ChatRecoverAccount { .. } => ensure_idle(host)?,
        ClientRequest::PrepareUpgrade { .. }
        | ClientRequest::PrepareConfirmedUpgrade { .. }
        | ClientRequest::Shutdown
        | ClientRequest::ShutdownV1 { .. } => ensure_idle(host)?,
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        account::{
            AccountHome,
            home::{CodexConfig, Homes},
        },
        agent::Provider,
        model::{HostIncarnation, HostSessionId, HostSessionProof},
    };
    pub(crate) fn host() -> SharedHost {
        let host = Arc::new(super::super::state::HostLock::new(
            super::super::state::HostState::new(HostSessionProof {
                host_session_id: HostSessionId::new("account-admission"),
                boot_id: crate::daemon::read_boot_id().unwrap(),
                host_incarnation: HostIncarnation::new("fixture"),
            }),
        ));
        host.lock().unwrap().register_client(1);
        host
    }
    #[test]
    fn startup_account_home_absence_can_be_repaired_but_replacement_is_invalid() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("original");
        let parked = root.path().join("parked");
        std::fs::create_dir(&path).unwrap();
        let home = Home::capture_for_start(path.clone()).unwrap();
        std::fs::rename(&path, &parked).unwrap();
        assert!(matches!(
            home.verify_for_start(),
            Err(SetupError::Unavailable(_))
        ));
        std::fs::rename(&parked, &path).unwrap();
        assert!(home.verify_for_start().is_ok());
        std::fs::rename(&path, &parked).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(
            home.verify_for_start(),
            Err(SetupError::Invalid(_))
        ));
        assert!(matches!(
            Home::capture_for_start(PathBuf::from("relative")),
            Err(SetupError::Invalid(_))
        ));
    }

    #[test]
    fn startup_account_path_retries_when_original_parent_replaces_a_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let reserved = root.path().join("reserved");
        let selected_parent = root.path().join("selected-parent");
        let selected = selected_parent.join("home");
        let parked = root.path().join("parked-parent");
        std::fs::create_dir(&reserved).unwrap();
        std::fs::create_dir_all(&selected).unwrap();
        let selected_home = Home::capture_for_start(selected.clone()).unwrap();
        let host = host();
        let _reservation = Reservation::test_install(
            &host,
            &[Account::linked(
                Provider::Claude,
                uuid::Uuid::new_v4(),
                reserved,
            )],
            store,
            &homes,
        )
        .unwrap();
        ensure_paths(&host, [selected.clone()]).unwrap();
        std::fs::rename(&selected_parent, &parked).unwrap();
        std::fs::write(&selected_parent, "temporary filesystem obstruction").unwrap();
        let refused = ensure_paths(&host, [selected.clone()]).unwrap_err();
        assert!(refused.retryable, "{}", refused.message);
        assert!(matches!(
            selected_home.verify_for_start(),
            Err(SetupError::Unavailable(_))
        ));
        std::fs::remove_file(&selected_parent).unwrap();
        std::fs::rename(&parked, &selected_parent).unwrap();
        ensure_paths(&host, [selected.clone()]).unwrap();
        selected_home.verify_for_start().unwrap();
        std::fs::rename(&selected, selected_parent.join("original-home")).unwrap();
        std::fs::create_dir(&selected).unwrap();
        assert!(matches!(
            selected_home.verify_for_start(),
            Err(SetupError::Invalid(_))
        ));
    }

    #[test]
    fn startup_account_path_io_retries_after_descriptor_pressure_is_released() {
        const WORKER: &str = "CAGENTS_ACCOUNT_STARTUP_FD_WORKER";
        if std::env::var_os(WORKER).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "host::account_lifecycle::tests::startup_account_path_io_retries_after_descriptor_pressure_is_released", "--nocapture"])
                .env(WORKER, "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let completed = loop {
                if child.try_wait().unwrap().is_some() {
                    break true;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    break false;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            let output = child.wait_with_output().unwrap();
            assert!(
                completed && output.status.success(),
                "isolated descriptor-pressure fixture failed or exceeded 30 seconds:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let reserved = root.path().join("reserved");
        let selected = root.path().join("selected");
        std::fs::create_dir(&reserved).unwrap();
        std::fs::create_dir(&selected).unwrap();
        let host = host();
        let _reservation = Reservation::test_install(
            &host,
            &[Account::linked(
                Provider::Claude,
                uuid::Uuid::new_v4(),
                reserved,
            )],
            store,
            &homes,
        )
        .unwrap();
        assert!(ensure_paths(&host, [selected.clone()]).is_ok());
        // File-descriptor pressure produces a real filesystem resource error
        // regardless of uid/capabilities. Change the limit only in this exact
        // test's child, never in the parallel host test harness.
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        limit.rlim_cur = limit.rlim_cur.min(64);
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let mut files = Vec::new();
        loop {
            match File::open("/dev/null") {
                Ok(file) => files.push(file),
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                    break;
                }
            }
            assert!(files.len() < 128, "descriptor pressure must remain bounded");
        }
        let refused = ensure_paths(&host, [selected.clone()]).unwrap_err();
        assert!(refused.retryable, "{}", refused.message);
        drop(files);
        ensure_paths(&host, [selected]).unwrap();
        assert!(
            !ensure_paths(&host, [PathBuf::from("relative")])
                .unwrap_err()
                .retryable
        );
    }

    #[test]
    fn original_inode_and_physical_ancestry_block_aliases_after_rename() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let directory = root.path().join("original");
        std::fs::create_dir_all(directory.join("child")).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&directory, &alias).unwrap();
        let host = host();
        let reservation = Reservation::test_install(
            &host,
            &[Account::linked(
                Provider::Claude,
                uuid::Uuid::new_v4(),
                alias,
            )],
            store,
            &homes,
        )
        .unwrap();
        assert!(
            ensure_paths(&host, [directory.join("child")])
                .unwrap_err()
                .retryable
        );
        let moved = root.path().join("moved");
        std::fs::rename(&directory, &moved).unwrap();
        std::fs::create_dir(&directory).unwrap();
        assert!(
            ensure_paths(&host, [moved.join("child")])
                .unwrap_err()
                .retryable
        );
        assert!(reservation.verify().is_err());
        assert!(ensure_paths(&host, [root.path().join("unrelated")]).is_ok());
        drop(reservation);
        ensure_paths(&host, [moved]).unwrap();
    }
    #[test]
    fn create_reserves_future_home_and_actual_inode_before_seeding() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let host = host();
        let system = root.path().join("system");
        std::fs::create_dir(&system).unwrap();
        std::fs::write(system.join("settings.json"), b"{\"fixture\":true}").unwrap();
        let key = uuid::Uuid::new_v4();
        let target = homes.path(Provider::Claude, key);
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let accounts = [
            Account {
                provider: Provider::Claude,
                home: AccountHome::SystemAt {
                    path: system.clone(),
                },
            },
            Account::managed(Provider::Claude, key, target.clone()),
        ];
        let mut reservation =
            Reservation::test_install(&host, &accounts, store.clone(), &homes).unwrap();
        reservation.create = Some((Provider::Claude, key));
        assert!(ensure_paths(&host, [target.clone()]).is_err());
        let (ready_rx, release_tx, worker) = {
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
            let homes = homes.clone();
            let system = system.clone();
            let store = store.clone();
            let worker = std::thread::spawn(move || {
                store.try_create_pending_reserved(&reservation, &homes, Provider::Claude, 1, || {
                    reservation.create_home_initialized(
                        &homes,
                        Provider::Claude,
                        Some(&system),
                        CodexConfig::Shared,
                        |created| {
                            assert!(std::fs::read_dir(created.home())?.next().is_none());
                            ready_tx.send(()).map_err(std::io::Error::other)?;
                            release_rx
                                .recv_timeout(std::time::Duration::from_secs(5))
                                .map_err(std::io::Error::other)?;
                            Ok(())
                        },
                    )
                })
            });
            (ready_rx, release_tx, worker)
        };
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let recipe = crate::model::LaunchRecipe {
            program: "/bin/false".into(),
            arguments: vec![],
            working_directory: root.path().to_owned(),
            environment: BTreeMap::from([
                ("HOME".into(), root.path().display().to_string()),
                ("CLAUDE_CONFIG_DIR".into(), alias.display().to_string()),
            ]),
            unset_environment: Default::default(),
        };
        let terminal = crate::model::TerminalSession {
            id: crate::model::TerminalId::new(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe: recipe.clone(),
                last_exit: None,
            },
            launch: recipe,
            transcript_path: None,
            resume_hint: None,
        };
        let refused = super::super::client::process_request(
            &host,
            1,
            crate::terminal_ipc::ClientRequest::Spawn {
                session: terminal,
                options: Default::default(),
            },
        )
        .unwrap_err()
        .message
        .contains("account home is being changed");
        let unrelated = ensure_paths(&host, [root.path().join("other")]).is_ok();
        let gate = host.lock().unwrap().control_gate.clone();
        let reader = gate.try_shared().is_some();
        let moved = root.path().join("moved");
        std::fs::rename(&target, &moved).unwrap();
        let moved_refused = ensure_paths(&host, [moved.clone()]).is_err();
        std::fs::rename(&moved, &target).unwrap();
        let _ = release_tx.send(());
        let result = worker.join().unwrap().unwrap();
        assert!(refused && unrelated && reader && moved_refused);
        assert_eq!(result, (key, target.clone()));
        assert_eq!(
            std::fs::read(target.join("settings.json")).unwrap(),
            b"{\"fixture\":true}"
        );
        assert!(
            store
                .load()
                .unwrap()
                .pending()
                .iter()
                .any(|pending| pending.key == key)
        );
    }
    fn creating(provider: Provider) -> (tempfile::TempDir, Homes, Store, SharedHost, Reservation) {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let host = host();
        let key = uuid::Uuid::new_v4();
        let mut reservation = Reservation::test_install(
            &host,
            &[Account::managed(provider, key, homes.path(provider, key))],
            store.clone(),
            &homes,
        )
        .unwrap();
        reservation.create = Some((provider, key));
        (root, homes, store, host, reservation)
    }
    #[test]
    fn a_creation_the_gate_stays_too_busy_for_leaves_nothing_behind() {
        let (_root, homes, store, host, reservation) = creating(Provider::Codex);
        let (_, key) = reservation.create.unwrap();
        let target = homes.path(Provider::Codex, key);
        let gate = host.lock().unwrap().control_gate.clone();
        let held = gate.shared();
        let error = reservation
            .create_pending_home_within(
                &store,
                &homes,
                Provider::Codex,
                1,
                None,
                CodexConfig::Shared,
                std::time::Duration::from_millis(50),
            )
            .unwrap_err();
        assert!(
            matches!(
                crate::account::home::EnrollmentRefused::of(&error),
                Some(crate::account::home::EnrollmentRefused::Busy)
            ),
            "{error}"
        );
        assert!(!target.exists(), "the busy attempt withdrew its directory");
        assert!(store.try_load().unwrap().pending().is_empty());
        assert!(
            reservation.current().unwrap().homes[0].pin.is_none(),
            "no directory was enrolled in the reservation"
        );
        assert_eq!(gate.unqueued_waiters(), 0);
        drop(held);
        // The same reservation creates the home once the gate is free.
        assert_eq!(
            reservation
                .create_pending_home(
                    &store,
                    &homes,
                    Provider::Codex,
                    1,
                    None,
                    CodexConfig::Shared
                )
                .unwrap(),
            (key, target)
        );
    }
    #[test]
    fn busy_enrollment_retries_are_bounded_even_when_the_gate_looks_free() {
        use crate::account::home::EnrollmentRefused;
        let (_root, _homes, _store, _host, reservation) = creating(Provider::Codex);
        let busy = || EnrollmentRefused::Busy.into_error();
        // The gate is free at every look, yet each attempt is refused.
        let mut attempts = 0;
        let error = reservation
            .retry_busy_enrollment(
                std::time::Instant::now() + std::time::Duration::from_secs(30),
                || -> std::io::Result<()> {
                    attempts += 1;
                    Err(busy())
                },
            )
            .unwrap_err();
        assert_eq!(attempts, ENROLLMENT_ATTEMPTS);
        assert!(matches!(
            EnrollmentRefused::of(&error),
            Some(EnrollmentRefused::Busy)
        ));
        // Past the deadline nothing is tried again, and only busy is retried.
        let far = std::time::Instant::now() + std::time::Duration::from_secs(30);
        for (deadline, refusal) in [
            (std::time::Instant::now(), EnrollmentRefused::Busy),
            (far, EnrollmentRefused::Closed("closing".into())),
        ] {
            let mut refusal = Some(refusal);
            let mut attempts = 0;
            let error = reservation
                .retry_busy_enrollment(deadline, || -> std::io::Result<()> {
                    attempts += 1;
                    Err(refusal
                        .take()
                        .map_or_else(busy, EnrollmentRefused::into_error))
                })
                .unwrap_err();
            assert_eq!(attempts, 1, "{error}");
        }
    }
    #[test]
    fn a_creation_waits_out_a_busy_gate_holding_no_lease() {
        let (_root, homes, store, host, reservation) = creating(Provider::Codex);
        let (_, key) = reservation.create.unwrap();
        let target = homes.path(Provider::Codex, key);
        let gate = host.lock().unwrap().control_gate.clone();
        let held = gate.shared();
        std::thread::scope(|scope| {
            let creation = scope.spawn(|| {
                reservation.create_pending_home_within(
                    &store,
                    &homes,
                    Provider::Codex,
                    1,
                    None,
                    CodexConfig::Shared,
                    std::time::Duration::from_secs(30),
                )
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while gate.unqueued_waiters() == 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "creation never waited"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            // While it waits, nothing is on disk and no lease is held: the
            // registry and the lifecycle lock are free to anyone else.
            assert!(!target.exists());
            drop(store.try_load().unwrap());
            drop(homes.try_lifecycle_lock().unwrap());
            drop(held);
            assert_eq!(creation.join().unwrap().unwrap(), (key, target.clone()));
        });
        assert!(target.is_dir());
        assert!(
            store
                .load()
                .unwrap()
                .pending()
                .iter()
                .any(|pending| pending.key == key)
        );
    }
    #[test]
    fn a_creation_refused_by_a_held_account_list_leaves_nothing_behind() {
        use crate::account::remote::ListHeld;
        let (_root, homes, store, _host, reservation) = creating(Provider::Codex);
        let reservation = reservation.with_patience(std::time::Duration::from_millis(30));
        let (_, key) = reservation.create.unwrap();
        let target = homes.path(Provider::Codex, key);
        let create = || {
            reservation.create_pending_home(
                &store,
                &homes,
                Provider::Codex,
                1,
                None,
                CodexConfig::Shared,
            )
        };
        for refusal in [ListHeld::Registry, ListHeld::Lifecycle] {
            let held = match refusal {
                ListHeld::Registry => store.lock().unwrap(),
                ListHeld::Lifecycle => homes.lifecycle_lock().unwrap(),
            };
            let error = create().unwrap_err();
            assert_eq!(ListHeld::of(&error), Some(refusal), "{error}");
            assert_eq!(error.to_string(), refusal.to_string());
            drop(held);
            assert!(
                !target.exists(),
                "nothing was made while {refusal:?} was held"
            );
            assert!(store.load().unwrap().pending().is_empty());
            assert!(reservation.current().unwrap().homes[0].pin.is_none());
        }
        assert_eq!(create().unwrap(), (key, target));
    }
    #[test]
    fn a_creation_waits_out_an_account_list_held_for_a_moment() {
        let (_root, homes, store, _host, reservation) = creating(Provider::Codex);
        // Long enough that a slow scheduler cannot outlast it before the
        // lock is released; only waiting, not its bound, is under test.
        let reservation = reservation.with_patience(std::time::Duration::from_secs(30));
        let (_, key) = reservation.create.unwrap();
        let target = homes.path(Provider::Codex, key);
        let held = homes.lifecycle_lock().unwrap();
        std::thread::scope(|scope| {
            let creation = scope.spawn(|| {
                reservation.create_pending_home(
                    &store,
                    &homes,
                    Provider::Codex,
                    1,
                    None,
                    CodexConfig::Shared,
                )
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(!creation.is_finished(), "creation did not wait");
            assert!(!target.exists());
            drop(held);
            assert_eq!(creation.join().unwrap().unwrap(), (key, target.clone()));
        });
        assert!(target.is_dir());
    }
    #[test]
    fn a_reservation_waits_out_and_then_refuses_a_held_account_list_retryably() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path().join("registry"));
        let home = root.path().join("linked");
        std::fs::create_dir(&home).unwrap();
        let accounts = [Account::linked(Provider::Codex, uuid::Uuid::new_v4(), home)];
        let host = host();
        let held = store.lock().unwrap();
        let refusal = match Reservation::install_with_store(
            &host,
            &accounts,
            store.clone(),
            None,
            None,
            std::time::Duration::from_millis(30),
        ) {
            Err(refusal) => refusal,
            Ok(_) => panic!("installed while the account list was held"),
        };
        assert!(refusal.retryable);
        assert_eq!(
            refusal.message,
            crate::account::remote::ListHeld::Registry.to_string()
        );
        assert!(!reservations_active(&host));
        std::thread::scope(|scope| {
            let installing = scope.spawn(|| {
                Reservation::install_with_store(
                    &host,
                    &accounts,
                    store.clone(),
                    None,
                    None,
                    std::time::Duration::from_secs(30),
                )
                .map(drop)
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(!installing.is_finished(), "installation did not wait");
            drop(held);
            installing.join().unwrap().unwrap();
        });
    }
    #[test]
    fn scoped_registry_mutation_refuses_changed_relevant_metadata() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let directory = root.path().join("linked");
        std::fs::create_dir(&directory).unwrap();
        let key = uuid::Uuid::new_v4();
        store
            .update(|registry| {
                registry.link(
                    Provider::Codex,
                    key,
                    directory.clone(),
                    None,
                    Some(&homes),
                    1,
                )
            })
            .unwrap()
            .1
            .unwrap();
        let account = Account::linked(Provider::Codex, key, directory);
        let host = host();
        let reservation =
            Reservation::test_install(&host, std::slice::from_ref(&account), store.clone(), &homes)
                .unwrap();
        store
            .update(|registry| registry.set_label(&account, Some("external edit".into())))
            .unwrap();
        let mut effects = 0;
        assert!(
            store
                .try_update_reserved(&reservation, |_| effects += 1)
                .is_err()
        );
        assert_eq!(effects, 0);
        assert_eq!(store.load().unwrap().label(&account), Some("external edit"));
    }
    #[test]
    fn unknown_and_relative_pinned_roots_cannot_bypass_reservation() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let host = host();
        let reservation = Reservation::test_install(
            &host,
            &[Account::linked(
                Provider::Codex,
                uuid::Uuid::new_v4(),
                root.path().join("reserved"),
            )],
            store,
            &homes,
        )
        .unwrap();
        let unknown = ensure_launch(
            &host,
            root.path(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            false,
        )
        .unwrap_err();
        assert!(
            unknown.retryable,
            "waiting for the operation repairs unknown roots"
        );
        let directory = File::open(root.path()).unwrap();
        let relative = ensure_launch_at_directory(
            &host,
            root.path(),
            &BTreeMap::from([("HOME".into(), "relative".into())]),
            &BTreeSet::new(),
            false,
            Some(&directory),
        )
        .unwrap_err();
        assert!(
            !relative.retryable,
            "a pinned-directory recipe needs an explicit correction"
        );
        drop(reservation);
        ensure_launch(
            &host,
            root.path(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            false,
        )
        .unwrap();
    }
    #[test]
    fn resumed_history_raw_unset_selector_refuses_before_constructor_reads() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let host = host();
        let selected = root.path().join("selected");
        std::fs::create_dir(&selected).unwrap();
        let _reservation = Reservation::test_install(
            &host,
            &[Account::linked(
                Provider::Claude,
                uuid::Uuid::new_v4(),
                selected.clone(),
            )],
            store,
            &homes,
        )
        .unwrap();
        let session = crate::chat::Session {
            id: crate::model::TerminalId::new(),
            native_continuity: Default::default(),
            owned_codex_listener: None,
            proof: None,
            terminal: None,
            launch: crate::chat::Launch {
                provider: Provider::Claude,
                session_id: Some(uuid::Uuid::new_v4().to_string()),
                recipe: crate::model::LaunchRecipe {
                    program: "/does-not-exist/claude".into(),
                    arguments: vec![],
                    working_directory: root.path().to_owned(),
                    environment: BTreeMap::from([
                        ("HOME".into(), root.path().display().to_string()),
                        ("CLAUDE_CONFIG_DIR".into(), selected.display().to_string()),
                    ]),
                    unset_environment: BTreeSet::from(["CLAUDE_CONFIG_DIR".into()]),
                },
            },
        };
        let failure = super::super::client::process_request(
            &host,
            1,
            crate::terminal_ipc::ClientRequest::ChatStart {
                session,
                source: None,
                terminate_source: false,
                execution_environment: Default::default(),
            },
        )
        .unwrap_err();
        assert!(
            failure.message.contains("account home is being changed"),
            "{}",
            failure.message
        );
        assert!(failure.retryable);
        assert!(host.lock().unwrap().chats.is_empty());
        assert!(host.lock().unwrap().chat_spawning.is_empty());
        assert!(std::fs::read_dir(selected).unwrap().next().is_none());
    }

    #[test]
    fn automatic_worktree_cleanup_refuses_while_account_operands_are_reserved() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let host = host();
        let account = Account::linked(
            Provider::Claude,
            uuid::Uuid::new_v4(),
            root.path().join("linked-account"),
        );
        let reservation = Reservation::test_install(&host, &[account], store, &homes).unwrap();
        let gate = host.lock().unwrap().control_gate.clone();
        let _writer = gate.try_exclusive().unwrap();
        assert!(ensure_idle(&host).unwrap_err().retryable);
        let error = host
            .lock()
            .unwrap()
            .allow_automatic_worktree_cleanup(1, &[])
            .unwrap_err();
        assert!(error.contains("account operation"));
        drop(reservation);
        assert!(ensure_idle(&host).is_ok());
        assert_eq!(
            host.lock()
                .unwrap()
                .allow_automatic_worktree_cleanup(1, &[])
                .unwrap_err(),
            "A window has not established directory protection"
        );
    }
    #[test]
    fn a_home_whose_restore_check_keeps_failing_is_asked_about_less_often() {
        let home = (Provider::Claude, uuid::Uuid::new_v4());
        let mut sweep = RestoreSweep::default();
        let mut asked = Vec::new();
        for pass in 0..200 {
            if !sweep.defers(home) {
                asked.push(pass);
                sweep.unfinished(home);
            }
        }
        let gaps: Vec<_> = asked.windows(2).map(|pair| pair[1] - pair[0]).collect();
        assert_eq!(&gaps[..7], [2, 3, 5, 9, 17, 33, 65]);
        assert!(gaps[7..].iter().all(|gap| *gap == 65), "{gaps:?}");
        sweep.examined.insert(home);
        sweep.deferred.clear();
        assert!(sweep.defers(home), "a settled home is not asked again");
    }
    #[test]
    fn empty_codex_home_never_proves_foreign_home_for_a_reserved_working_directory() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let selected = root.path().join("selected");
        let foreign = root.path().join("foreign");
        std::fs::create_dir(&selected).unwrap();
        std::fs::create_dir(&foreign).unwrap();
        let host = host();
        let reservation = Reservation::test_install(
            &host,
            &[Account::linked(
                Provider::Codex,
                uuid::Uuid::new_v4(),
                selected.clone(),
            )],
            store,
            &homes,
        )
        .unwrap();
        let environment = BTreeMap::from([
            ("HOME".into(), foreign.display().to_string()),
            ("CODEX_HOME".into(), String::new()),
        ]);
        assert!(ensure_launch(&host, &selected, &environment, &BTreeSet::new(), false).is_err());
        let unset = BTreeSet::from(["CODEX_HOME".into()]);
        ensure_launch(&host, &selected, &environment, &unset, false).unwrap();
        ensure_launch(
            &host,
            &selected,
            &BTreeMap::from([("HOME".into(), foreign.display().to_string())]),
            &BTreeSet::new(),
            false,
        )
        .unwrap();
        let launch = crate::chat::Launch {
            provider: Provider::Codex,
            session_id: Some(uuid::Uuid::new_v4().to_string()),
            recipe: crate::model::LaunchRecipe {
                program: "/does-not-exist/codex".into(),
                arguments: vec!["--remote".into(), "unix://".into()],
                working_directory: selected.clone(),
                environment: environment.clone(),
                unset_environment: BTreeSet::new(),
            },
        };
        // Private filesystem-only control: the actual constructor resolves
        // the empty selector to cwd, not the foreign HOME fallback. The
        // absent socket makes this stop before any connection/provider work.
        let loader_error = crate::chat::protocol::codex::create(&launch)
            .err()
            .expect("private socket is absent");
        assert!(
            loader_error.contains(
                &selected
                    .join("app-server-control/app-server-control.sock")
                    .display()
                    .to_string()
            ),
            "{loader_error}"
        );
        let directory = File::open(&selected).unwrap();
        assert!(
            ensure_launch_at_directory(
                &host,
                &selected,
                &environment,
                &BTreeSet::new(),
                false,
                Some(&directory)
            )
            .is_err()
        );
        ensure_launch_at_directory(
            &host,
            &selected,
            &environment,
            &unset,
            false,
            Some(&directory),
        )
        .unwrap();
        // An effective unset permits HOME fallback, but the raw constructor
        // operand still needs its own refusal before that constructor runs.
        assert!(
            ensure_launch(
                &host,
                &selected,
                &launch.recipe.environment,
                &BTreeSet::new(),
                false
            )
            .is_err()
        );
        drop(reservation);
        ensure_launch(&host, &selected, &environment, &BTreeSet::new(), false).unwrap();
    }

    #[test]
    fn relative_claude_home_fallback_cannot_prove_a_constructor_root() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let selected = root.path().join("reserved");
        let cwd = root.path().join("working");
        let foreign = root.path().join("foreign");
        for path in [&selected, &cwd, &foreign] {
            std::fs::create_dir(path).unwrap();
        }
        let host = host();
        let reservation = Reservation::test_install(
            &host,
            &[Account::linked(
                Provider::Claude,
                uuid::Uuid::new_v4(),
                selected,
            )],
            store,
            &homes,
        )
        .unwrap();
        let mut environment = BTreeMap::from([
            ("HOME".into(), "relative-home".into()),
            ("CODEX_HOME".into(), foreign.display().to_string()),
        ]);
        // The old guard proved cwd/relative-home, although actual resumed
        // Claude history resolves the fallback against the daemon directory.
        assert!(ensure_launch(&host, &cwd, &environment, &BTreeSet::new(), false).is_err());
        environment.insert("CLAUDE_CONFIG_DIR".into(), String::new());
        assert!(ensure_launch(&host, &cwd, &environment, &BTreeSet::new(), false).is_err());
        environment.insert("CLAUDE_CONFIG_DIR".into(), foreign.display().to_string());
        ensure_launch(&host, &cwd, &environment, &BTreeSet::new(), false).unwrap();
        let unset = BTreeSet::from(["CLAUDE_CONFIG_DIR".into()]);
        assert!(ensure_launch(&host, &cwd, &environment, &unset, false).is_err());
        environment.insert("HOME".into(), foreign.display().to_string());
        ensure_launch(&host, &cwd, &environment, &unset, false).unwrap();
        drop(reservation);
        environment.insert("HOME".into(), "relative-home".into());
        ensure_launch(&host, &cwd, &environment, &unset, false).unwrap();
    }
    #[test]
    fn a_reservation_waits_for_an_observation_reading_its_home_and_refuses_one_that_stays() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let home = root.path().join("claude-home");
        std::fs::create_dir(&home).unwrap();
        let account = Account::linked(Provider::Claude, uuid::Uuid::new_v4(), home.clone());
        let host = host();
        let probes = Arc::clone(&host.lock().unwrap().agent_probes);
        let probe = |home: &Path| super::super::activity::Probe {
            working_directory: root.path().to_path_buf(),
            environment: BTreeMap::from([
                ("HOME".into(), root.path().to_str().unwrap().into()),
                ("CLAUDE_CONFIG_DIR".into(), home.to_str().unwrap().into()),
            ]),
            unset: BTreeSet::new(),
        };

        // A listing that ends soon is waited for; one reading another home
        // is not, however long it runs.
        let elsewhere = root.path().join("other-home");
        std::fs::create_dir(&elsewhere).unwrap();
        let unrelated = probes.admit(probe(&elsewhere));
        let brief = probes.admit(probe(&home));
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(brief);
        });
        let reservation =
            Reservation::test_install(&host, std::slice::from_ref(&account), store.clone(), &homes)
                .unwrap();
        releaser.join().unwrap();
        drop(reservation);

        // One that outlasts the wait refuses the reservation, as retryable.
        let stuck = probes.admit(probe(&home));
        let error = match Reservation::test_install(
            &host,
            std::slice::from_ref(&account),
            store.clone(),
            &homes,
        ) {
            Ok(_) => panic!("a listing reading the home must refuse its reservation"),
            Err(error) => error,
        };
        assert!(error.retryable, "{}", error.message);
        assert!(
            error.message.contains("still checking"),
            "{}",
            error.message
        );
        drop(stuck);
        Reservation::test_install(&host, &[account], store, &homes).unwrap();
        drop(unrelated);
    }
}
