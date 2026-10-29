//! Scoped admission for account-home mutations. Install reservations under the
//! short control writer; never retain a map or HostState lock during I/O.
//!
//! This foundation issues no deletion permit. Managed removal refuses and
//! pending/failed homes remain intact. Historical namespace enrollment and
//! complete actor/reader retirement census belong to that separate cut.
use super::state::SharedHost;
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

impl Home {
    fn capture(path: PathBuf) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("Account homes must be absolute".into());
        }
        let canonical = normalize(&path)?;
        let pin = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&canonical)
        {
            Ok(file) => Some(Arc::new(file)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("Cannot reserve account home: {error}")),
        };
        let identity = pin
            .as_ref()
            .map(|pin| pin.metadata())
            .transpose()
            .map_err(|e| e.to_string())?
            .map(|metadata| (metadata.dev(), metadata.ino()));
        let ancestors = physical_ancestors(&canonical, pin.as_deref())?;
        let home = Self {
            path,
            canonical,
            pin,
            identity,
            ancestors,
        };
        home.verify()?;
        Ok(home)
    }

    fn verify(&self) -> Result<(), String> {
        if normalize(&self.path)? != self.canonical {
            return Err("The account home changed while its operation was pending; reload accounts and retry".into());
        }
        match (&self.pin, std::fs::metadata(&self.path)) {
            (Some(pin), Ok(current)) => {
                let original = pin.metadata().map_err(|error| error.to_string())?;
                if current.is_dir() && (current.dev(), current.ino()) == (original.dev(), original.ino()) { Ok(()) }
                else { Err("The account home was replaced while its operation was pending".into()) }
            }
            (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err("The account home appeared, disappeared or became inaccessible during its operation".into()),
        }
    }

    fn refresh_ancestors(&mut self) -> Result<(), String> {
        if let Some(pin) = &self.pin {
            // Once cleanup unlinks this exact root, it has no current
            // descendants; retain the original lexical guard until Drop.
            if pin.metadata().map_err(|error| error.to_string())?.nlink() == 0 {
                return Ok(());
            }
            self.ancestors
                .extend(physical_ancestors(&self.canonical, Some(pin.as_ref()))?);
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
    let mut cursor = if let Some(pin) = pin {
        pin.try_clone().map_err(|error| error.to_string())?
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
                    return Err(format!("Cannot capture directory scope ancestry: {error}"));
                }
            }
        }
    };
    let mut identities = BTreeSet::new();
    for _ in 0..256 {
        let metadata = cursor.metadata().map_err(|error| error.to_string())?;
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
            return Err(format!(
                "Cannot capture directory scope parent: {}",
                std::io::Error::last_os_error()
            ));
        }
        cursor = unsafe { File::from_raw_fd(parent) };
    }
    Err("Directory scope ancestry exceeds the directory bound".into())
}

/// Resolve aliases even for a not-yet-created descendant without interpreting
/// `..` through a missing directory. Missing-home cleanup remains possible.
fn normalize(path: &Path) -> Result<PathBuf, String> {
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
            Err(error) => return Err(format!("Cannot resolve account home: {error}")),
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
    /// Only relevant registrations, not the global revision: unrelated edits
    /// must be preserved while a provider is taking time to sign out.
    registered: Vec<(
        uuid::Uuid,
        Option<crate::account::registry::Registered>,
        Option<crate::account::registry::Pending>,
        Option<crate::account::registry::Preparation>,
        Option<crate::account::registry::ExistingPending>,
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
    pub(super) fn install(host: &SharedHost, accounts: &[Account]) -> Result<Self, ProtocolError> {
        let store = Store::installation().ok_or_else(|| refusal("Account registry unavailable"))?;
        Self::install_with_store(
            host,
            accounts,
            store,
            crate::account::home::Homes::installation().as_ref(),
        )
    }

    fn install_with_store(
        host: &SharedHost,
        accounts: &[Account],
        store: Store,
        installed_homes: Option<&crate::account::home::Homes>,
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
        let registry = store
            .try_load()
            .map_err(|error| refusal(error.to_string()))?;
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
                    registry.preparation_for_home(key).cloned(),
                    registry.existing_pending(key).cloned(),
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
        let writer = super::client::try_scoped_control_write(&gate)?;
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
        entries.insert(token, Arc::new(Reserved { homes }));
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
        };
        // No effects have happened. Refuse any filesystem/registration change
        // across observation and installation while the installed token fences
        // subsequent related ingress.
        reservation.verify().map_err(refusal)?;
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
        Self::install_with_store(host, accounts, store, Some(homes))
    }

    fn ensure_no_active_owners(&self, host: &SharedHost) -> Result<(), ProtocolError> {
        // The installed token prevents new related startups. Copy only actual
        // execution-root metadata while holding HostState, then resolve paths
        // outside that lock and outside the global control writer.
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
        let mut accounts = crate::account::remote::affected_accounts(operation).map_err(refusal)?;
        let create = if let crate::account::remote::Operation::Create { provider, .. } = operation {
            let homes = crate::account::home::Homes::installation()
                .ok_or_else(|| refusal("Account homes unavailable"))?;
            let key = uuid::Uuid::new_v4();
            accounts.push(Account::managed(*provider, key, homes.path(*provider, key)));
            Some((*provider, key))
        } else {
            None
        };
        let mut reservation = Self::install(host, &accounts)?;
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

    pub(crate) fn create_home(
        &self,
        homes: &crate::account::home::Homes,
        provider: crate::agent::Provider,
        system: Option<&Path>,
        config: crate::account::home::CodexConfig,
    ) -> std::io::Result<(uuid::Uuid, PathBuf)> {
        self.create_home_initialized(homes, provider, system, config, |_| Ok(()))
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
    /// try-write. Busy/failure retains the fresh files and publishes no row.
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
        let writer = super::client::try_scoped_control_write(&gate)
            .map_err(|error| io::Error::new(io::ErrorKind::WouldBlock, error.message))?;
        super::client::ensure_control_open(&host)
            .map_err(|error| io::Error::other(error.message))?;
        let mut entries = self.map.lock().unwrap_or_else(|p| p.into_inner());
        if !same_basis(&basis, &entries) {
            return Err(io::Error::other(
                "Account reservations changed during home enrollment; retry creation",
            ));
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
    pub(crate) fn commit<T>(
        &self,
        effect: impl FnOnce() -> std::io::Result<T>,
    ) -> Result<T, String> {
        self.verify()?;
        effect().map_err(|error| error.to_string())
    }

    /// Call outside HostState and before each destructive stage/final commit.
    pub(crate) fn verify(&self) -> Result<(), String> {
        if !self
            .map
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&self.token)
        {
            return Err(
                "The account mutation reservation was released; reload accounts and retry".into(),
            );
        }
        let registry = self.store.try_load().map_err(|error| error.to_string())?;
        self.verify_registry(&registry)
    }

    /// Invoked by Store under the SAME registry lease as its mutation; never
    /// reacquire the registry flock here. Physical validation precedes that
    /// lease in commit(), while this cut closes concurrent metadata races.
    pub(crate) fn verify_registry(&self, registry: &Registry) -> Result<(), String> {
        let current = self.current()?;
        for home in &current.homes {
            home.verify()?;
        }
        for (key, account, pending, preparation, existing_pending, archived) in &self.registered {
            if registry.find(*key) != account.as_ref()
                || registry.pending().iter().find(|value| value.key == *key) != pending.as_ref()
                || registry.preparation_for_home(*key) != preparation.as_ref()
                || registry.existing_pending(*key) != existing_pending.as_ref()
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
        home.refresh_ancestors().map_err(refusal)?;
    }
    for path in paths {
        let home = Home::capture(path).map_err(refusal)?;
        if reserved.iter().any(|other| home.overlaps(other)) {
            return Err(refusal(
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
        return Err(refusal(
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
            refusal(format!(
                "Cannot inspect the pinned terminal directory: {error}"
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
        .map_err(|error| refusal(error.to_string()))?;
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
    if map.lock().unwrap_or_else(|p| p.into_inner()).is_empty() {
        Ok(())
    } else {
        Err(refusal(
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
        assert!(ensure_paths(&host, [directory.join("child")]).is_err());
        let moved = root.path().join("moved");
        std::fs::rename(&directory, &moved).unwrap();
        std::fs::create_dir(&directory).unwrap();
        assert!(ensure_paths(&host, [moved.join("child")]).is_err());
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
        let reader = gate.try_read().is_ok();
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
    #[test]
    fn failed_enrollment_retains_unseeded_home_without_publication() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let host = host();
        let key = uuid::Uuid::new_v4();
        let target = homes.path(Provider::Codex, key);
        let mut reservation = Reservation::test_install(
            &host,
            &[Account::managed(Provider::Codex, key, target.clone())],
            store.clone(),
            &homes,
        )
        .unwrap();
        reservation.create = Some((Provider::Codex, key));
        let gate = host.lock().unwrap().control_gate.clone();
        let held = gate.read().unwrap();
        assert!(
            store
                .try_create_pending_reserved(&reservation, &homes, Provider::Codex, 1, || {
                    reservation.create_home(&homes, Provider::Codex, None, CodexConfig::Shared)
                })
                .is_err()
        );
        assert!(target.is_dir());
        assert!(std::fs::read_dir(&target).unwrap().next().is_none());
        assert!(store.try_load().unwrap().pending().is_empty());
        drop(held);
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
            Reservation::test_install(&host, &[account.clone()], store.clone(), &homes).unwrap();
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
    fn scoped_registry_mutation_detects_retained_identity_and_receipt_changes() {
        use crate::account::registry::ExistingPendingTarget;
        for stage in ["adopt", "cancel", "receipt"] {
            let root = tempfile::tempdir().unwrap();
            let homes = Homes::at(root.path().join("homes"));
            let store = Store::at(root.path().join("registry"));
            let (key, path) = store
                .create_pending(&homes, Provider::Codex, 1, || {
                    homes.create(Provider::Codex, None)
                })
                .unwrap();
            let target = ExistingPendingTarget::Observed {
                provider: Provider::Codex,
                key,
                started_at: 1,
                expected_installation: None,
            };
            let adopted = if stage == "adopt" {
                None
            } else {
                Some(store.resume_existing_pending(&target).unwrap())
            };
            if stage == "receipt" {
                store
                    .cancel_existing_pending(
                        &homes,
                        uuid::Uuid::new_v4(),
                        &adopted.as_ref().unwrap().binding,
                    )
                    .unwrap();
            }
            let account = Account::managed(Provider::Codex, key, path);
            let host = host();
            let reservation =
                Reservation::test_install(&host, &[account], store.clone(), &homes).unwrap();
            store
                .update(|registry| {
                    registry.set_label(&Account::system(Provider::Claude), Some("unrelated".into()))
                })
                .unwrap();
            let mut effects = 0;
            store
                .try_update_reserved(&reservation, |_| effects += 1)
                .unwrap();
            assert_eq!(
                effects, 1,
                "unrelated preference edit is not a global revision veto"
            );
            let pending_before = store.load().unwrap().pending().to_vec();
            if stage == "adopt" {
                store.resume_existing_pending(&target).unwrap();
            } else {
                store
                    .cancel_existing_pending(
                        &homes,
                        uuid::Uuid::new_v4(),
                        &adopted.as_ref().unwrap().binding,
                    )
                    .unwrap();
            }
            assert_eq!(store.load().unwrap().pending(), pending_before);
            effects = 0;
            assert!(
                store
                    .try_update_reserved(&reservation, |_| effects += 1)
                    .is_err()
            );
            assert_eq!(effects, 0);
            let registered = crate::account::registry::Registered {
                key,
                provider: Provider::Codex,
                linked_home: None,
                label: None,
                email: Some("cas@example.invalid".into()),
                organization: None,
                added_at: 1,
            };
            assert!(
                store
                    .try_register_pending_reserved(&reservation, &homes, registered, &[])
                    .is_err()
            );
        }
    }

    #[test]
    fn scoped_registry_mutation_detects_preparation_only_cancellation() {
        use crate::account::registry::PreparationPayload;
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let planned = store
            .plan_preparation(
                uuid::Uuid::new_v4(),
                PreparationPayload {
                    provider: Provider::Codex,
                    copy_codex_config: true,
                },
            )
            .unwrap();
        store
            .create_prepared(
                &homes,
                &planned.binding,
                false,
                |_| Ok(()),
                || {
                    homes.create_planned_with_config_initialized(
                        Provider::Codex,
                        planned.binding.key,
                        None,
                        CodexConfig::Copy,
                        |_| Ok(()),
                    )
                },
            )
            .unwrap();
        let host = host();
        let account = Account::managed(
            Provider::Codex,
            planned.binding.key,
            homes.path(Provider::Codex, planned.binding.key),
        );
        let reservation =
            Reservation::test_install(&host, &[account], store.clone(), &homes).unwrap();
        let pending = store.load().unwrap().pending().to_vec();
        store.cancel_preparation(&homes, &planned.binding).unwrap();
        assert_eq!(store.load().unwrap().pending(), pending);
        let mut effects = 0;
        assert!(
            store
                .try_update_reserved(&reservation, |_| effects += 1)
                .is_err()
        );
        assert_eq!(effects, 0);
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
        assert!(
            ensure_launch(
                &host,
                root.path(),
                &BTreeMap::new(),
                &BTreeSet::new(),
                false
            )
            .is_err()
        );
        let directory = File::open(root.path()).unwrap();
        assert!(
            ensure_launch_at_directory(
                &host,
                root.path(),
                &BTreeMap::from([("HOME".into(), "relative".into())]),
                &BTreeSet::new(),
                false,
                Some(&directory)
            )
            .is_err()
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
        let _writer = super::super::client::try_scoped_control_write(&gate).unwrap();
        assert!(ensure_idle(&host).is_err());
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
}
