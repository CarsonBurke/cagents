//! The accounts an installation has added, and which one each provider uses
//! for a new conversation.
//!
//! `accounts.toml` in the instance's configuration directory holds only
//! what the reader chose and what the provider's CLI reported as display
//! metadata: a key per registered account, its provider, the name the reader
//! gave it, the email and organization its CLI named, when it was added, and
//! the default account per provider. It never holds a token, a key or a
//! path for homes cagents created. A linked account records the absolute path
//! the reader explicitly chose; cagents never deletes or signs out that home.
//!
//! Every window of the installation shares the file. A change takes an
//! advisory lock, reads the file afresh, applies itself, and writes the whole
//! file to a temporary sibling that is renamed over the original, so two
//! windows never lose each other's changes and a reader never sees half a
//! file.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{Account, AccountHome, AccountId, Identity, home::Homes};
use crate::agent::Provider;
use crate::model::ProjectId;

const FILE_NAME: &str = "accounts.toml";
const LOCK_NAME: &str = "accounts.toml.lock";
const VERSION: u32 = 4;
/// A registry larger than this is not one cagents wrote.
const MAX_BYTES: u64 = 256 * 1024;

/// One account cagents added, as the registry records it. An account with
/// `linked_home` belongs to another application or the reader, not cagents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registered {
    pub key: Uuid,
    #[serde(with = "provider_name")]
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_home: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Unix milliseconds.
    #[serde(default)]
    pub added_at: u64,
}

impl Registered {
    pub fn identity(&self) -> Identity {
        Identity {
            email: self.email.clone(),
            organization: self.organization.clone(),
            plan: None,
        }
    }
}

/// A home created for a sign-in that has not finished: registered before the
/// provider's login runs in it, so that no window, and no later run, takes it
/// for a directory nothing knows about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub key: Uuid,
    #[serde(with = "provider_name")]
    pub provider: Provider,
    /// Unix milliseconds.
    #[serde(default)]
    pub started_at: u64,
}

/// The whole registry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    version: u32,
    /// Monotonic file revision. Async window callbacks may arrive out of
    /// order; an older snapshot must never replace a newer account choice.
    #[serde(default)]
    revision: u64,
    /// The account each provider starts new conversations on, by provider
    /// executable name: `"system"` or a registered account's key. A provider
    /// with no entry, or an entry naming an account that is gone, uses its
    /// system account.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    defaults: BTreeMap<String, String>,
    /// A project's explicit provider choice, keyed by the canonical Git
    /// directory's path bytes. ProjectId is local to one window and cannot
    /// name the same project in another window. Version 3 entries used that
    /// local ID and are migrated when the window's project catalog loads.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    project_defaults: BTreeMap<String, BTreeMap<String, String>>,
    /// Names the reader gave the system accounts, by provider executable.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    system_labels: BTreeMap<String, String>,
    #[serde(default, rename = "account", skip_serializing_if = "Vec::is_empty")]
    accounts: Vec<Registered>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending: Vec<Pending>,
}

const SYSTEM: &str = "system";

fn project_key(repository: &Path) -> String {
    let digest = Sha256::digest(repository.as_os_str().as_bytes());
    let mut key = String::with_capacity(5 + digest.len() * 2);
    key.push_str("repo:");
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(&mut key, "{byte:02x}");
    }
    key
}

impl Registry {
    pub fn contains_managed_home(&self, homes: &Homes, provider: Provider, key: Uuid) -> bool {
        let path = homes.path(provider, key);
        self.accounts.iter().any(|account| {
            (account.provider == provider && account.key == key && account.linked_home.is_none())
                || account
                    .linked_home
                    .as_ref()
                    .is_some_and(|linked| linked == &path)
        }) || self
            .pending
            .iter()
            .any(|pending| pending.provider == provider && pending.key == key)
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn registered(&self) -> &[Registered] {
        &self.accounts
    }

    pub fn find(&self, key: Uuid) -> Option<&Registered> {
        self.accounts.iter().find(|account| account.key == key)
    }

    /// Every account, system accounts first and then registered ones in the
    /// order they were added.
    pub fn accounts(&self, homes: Option<&Homes>) -> Vec<Account> {
        let mut accounts: Vec<Account> = Account::system_accounts().into();
        accounts.extend(self.accounts.iter().filter_map(|registered| {
            if let Some(path) = &registered.linked_home {
                Some(Account::linked(
                    registered.provider,
                    registered.key,
                    path.clone(),
                ))
            } else {
                let homes = homes?;
                Some(Account::managed(
                    registered.provider,
                    registered.key,
                    homes.path(registered.provider, registered.key),
                ))
            }
        }));
        accounts
    }

    /// The account `provider` starts new conversations on.
    pub fn default_account(&self, provider: Provider, homes: Option<&Homes>) -> Account {
        self.chosen_account(provider, self.defaults.get(provider.executable()), homes)
            .unwrap_or_else(|| Account::system(provider))
    }

    /// The account used for new work in `project`, inheriting the global
    /// choice when this project has no usable explicit choice.
    pub fn project_default_account(
        &self,
        repository: &Path,
        provider: Provider,
        homes: Option<&Homes>,
    ) -> Account {
        self.project_defaults
            .get(&project_key(repository))
            .and_then(|choices| choices.get(provider.executable()))
            .and_then(|chosen| self.chosen_account(provider, Some(chosen), homes))
            .unwrap_or_else(|| self.default_account(provider, homes))
    }

    fn chosen_account(
        &self,
        provider: Provider,
        chosen: Option<&String>,
        homes: Option<&Homes>,
    ) -> Option<Account> {
        let chosen = chosen?;
        if chosen == SYSTEM {
            return Some(Account::system(provider));
        }
        let registered = Some(chosen)
            .and_then(|key| Uuid::parse_str(key).ok())
            .and_then(|key| self.find(key))
            .filter(|managed| managed.provider == provider);
        match registered {
            Some(Registered {
                key,
                linked_home: Some(path),
                ..
            }) => Some(Account::linked(provider, *key, path.clone())),
            Some(registered) => homes.map(|homes| {
                Account::managed(
                    provider,
                    registered.key,
                    homes.path(provider, registered.key),
                )
            }),
            None => None,
        }
    }

    pub fn project_choice(&self, repository: &Path, provider: Provider) -> Option<&str> {
        self.project_defaults
            .get(&project_key(repository))?
            .get(provider.executable())
            .map(String::as_str)
    }

    pub fn is_project_default(&self, repository: &Path, account: &Account) -> bool {
        let Some(chosen) = self.project_choice(repository, account.provider) else {
            return false;
        };
        match account.registry_key() {
            Some(key) => Uuid::parse_str(chosen).ok() == Some(key),
            None => account.is_system() && chosen == SYSTEM,
        }
    }

    pub fn set_project_default(&mut self, repository: &Path, account: &Account) {
        let chosen = match account.registry_key() {
            Some(key) => key.hyphenated().to_string(),
            None if account.is_system() => SYSTEM.to_owned(),
            None => return,
        };
        self.project_defaults
            .entry(project_key(repository))
            .or_default()
            .insert(account.provider.executable().to_owned(), chosen);
    }

    pub fn clear_project_default(&mut self, repository: &Path, provider: Provider) {
        let project = project_key(repository);
        if let Some(choices) = self.project_defaults.get_mut(&project) {
            choices.remove(provider.executable());
            if choices.is_empty() {
                self.project_defaults.remove(&project);
            }
        }
    }

    /// Convert version 3 choices once the window knows the repository behind
    /// each of its old local IDs. A newer path-based choice wins per provider.
    pub fn migrate_project_ids(&mut self, projects: &[(ProjectId, PathBuf)]) -> bool {
        let mut changed = false;
        for (id, repository) in projects {
            let Some(choices) = self.project_defaults.remove(&id.to_string()) else {
                continue;
            };
            let current = self
                .project_defaults
                .entry(project_key(repository))
                .or_default();
            for (provider, chosen) in choices {
                current.entry(provider).or_insert(chosen);
            }
            changed = true;
        }
        changed
    }

    /// Whether new conversations of `account`'s provider start on it. A
    /// brokered account never is: its CLI picks between its accounts itself.
    pub fn is_default(&self, account: &Account) -> bool {
        let chosen = self.defaults.get(account.provider.executable());
        match &account.home {
            AccountHome::System => chosen.is_none_or(|chosen| {
                chosen == SYSTEM
                    || Uuid::parse_str(chosen)
                        .ok()
                        .and_then(|key| self.find(key))
                        .is_none()
            }),
            AccountHome::Managed { key, .. } | AccountHome::Linked { key, .. } => {
                chosen.is_some_and(|chosen| Uuid::parse_str(chosen).ok() == Some(*key))
            }
            AccountHome::Brokered { .. } => false,
        }
    }

    pub fn set_default(&mut self, account: &Account) {
        let value = match &account.home {
            AccountHome::System => SYSTEM.to_owned(),
            AccountHome::Managed { key, .. } | AccountHome::Linked { key, .. } => {
                key.hyphenated().to_string()
            }
            AccountHome::Brokered { .. } => return,
        };
        self.defaults
            .insert(account.provider.executable().to_owned(), value);
    }

    /// The name the reader gave `account`, if any. A brokered account is
    /// named by its CLI, not here.
    pub fn label(&self, account: &Account) -> Option<&str> {
        match &account.home {
            AccountHome::System => self
                .system_labels
                .get(account.provider.executable())
                .map(String::as_str),
            AccountHome::Managed { key, .. } | AccountHome::Linked { key, .. } => {
                self.find(*key)?.label.as_deref()
            }
            AccountHome::Brokered { .. } => None,
        }
    }

    /// Names `account`, or clears its name with `None`.
    pub fn set_label(&mut self, account: &Account, label: Option<String>) {
        let label = label.as_deref().and_then(super::clean_label);
        match &account.home {
            AccountHome::Brokered { .. } => {}
            AccountHome::System => {
                let provider = account.provider.executable().to_owned();
                match label {
                    Some(label) => {
                        self.system_labels.insert(provider, label);
                    }
                    None => {
                        self.system_labels.remove(&provider);
                    }
                }
            }
            AccountHome::Managed { key, .. } | AccountHome::Linked { key, .. } => {
                if let Some(managed) = self.accounts.iter_mut().find(|managed| managed.key == *key)
                {
                    managed.label = label;
                }
            }
        }
    }

    /// Records a managed account that has signed in, unless one already
    /// names the same login; returns the account it duplicates, if any.
    pub fn register(
        &mut self,
        managed: Registered,
        system: &[(Provider, Identity)],
    ) -> Result<(), AccountId> {
        let identity = managed.identity();
        if let Some((provider, _)) = system
            .iter()
            .find(|(provider, known)| *provider == managed.provider && known.same_login(&identity))
        {
            return Err(Account::system(*provider).id());
        }
        if let Some(existing) = self.accounts.iter().find(|existing| {
            existing.key != managed.key
                && existing.linked_home.is_none()
                && existing.provider == managed.provider
                && existing.identity().same_login(&identity)
        }) {
            return Err(Account::managed(existing.provider, existing.key, PathBuf::new()).id());
        }
        self.abandon(managed.key);
        match self
            .accounts
            .iter_mut()
            .find(|existing| existing.key == managed.key)
        {
            Some(existing) => *existing = managed,
            None => self.accounts.push(managed),
        }
        Ok(())
    }

    /// Records an existing provider home without taking ownership of it.
    /// Its canonical path, not its reported email, determines whether it is
    /// already registered: separate homes can hold the same login but have
    /// different histories.
    pub fn link(
        &mut self,
        provider: Provider,
        key: Uuid,
        path: PathBuf,
        system_home: Option<&Path>,
        homes: Option<&Homes>,
        added_at: u64,
    ) -> Result<(), String> {
        if !path.is_absolute() {
            return Err("Choose an absolute account home path".into());
        }
        if system_home
            .and_then(|home| fs::canonicalize(home).ok())
            .is_some_and(|home| home == path)
            || self.accounts(homes).iter().any(|account| {
                account.provider == provider
                    && account
                        .resolved_home()
                        .and_then(|home| fs::canonicalize(home).ok())
                        .is_some_and(|home| home == path)
            })
        {
            return Err("That account home is already listed".into());
        }
        self.accounts.push(Registered {
            key,
            provider,
            linked_home: Some(path),
            label: None,
            email: None,
            organization: None,
            added_at,
        });
        Ok(())
    }

    /// Records a home created for a sign-in that is about to start.
    pub fn begin(&mut self, provider: Provider, key: Uuid, started_at: u64) {
        if self.find(key).is_none() && !self.pending.iter().any(|pending| pending.key == key) {
            self.pending.push(Pending {
                key,
                provider,
                started_at,
            });
        }
    }

    /// Sign-ins that have not finished, oldest first.
    pub fn pending(&self) -> &[Pending] {
        &self.pending
    }

    /// Forgets a sign-in whose home is gone.
    pub fn abandon(&mut self, key: Uuid) {
        self.pending.retain(|pending| pending.key != key);
    }

    /// Updates what the provider last reported about a managed account, so
    /// the table and the dedupe of the next sign-in know who it is.
    pub fn observe_identity(&mut self, key: Uuid, identity: &Identity) -> bool {
        let Some(managed) = self.accounts.iter_mut().find(|managed| managed.key == key) else {
            return false;
        };
        let email = identity.email.clone().or_else(|| managed.email.clone());
        let organization = identity
            .organization
            .clone()
            .or_else(|| managed.organization.clone());
        if managed.email == email && managed.organization == organization {
            return false;
        }
        managed.email = email;
        managed.organization = organization;
        true
    }

    /// Forgets a managed account, and its place as a default.
    pub fn unregister(&mut self, key: Uuid) -> Option<Registered> {
        let index = self
            .accounts
            .iter()
            .position(|managed| managed.key == key)?;
        let removed = self.accounts.remove(index);
        let key = key.hyphenated().to_string();
        self.defaults.retain(|_, chosen| *chosen != key);
        self.project_defaults.retain(|_, choices| {
            choices.retain(|_, chosen| *chosen != key);
            !choices.is_empty()
        });
        Some(removed)
    }

    /// Whether a newer cagents wrote this registry, in which case this one
    /// changes nothing it does not understand.
    pub fn is_newer(&self) -> bool {
        self.version > VERSION
    }
}

/// The registry file of one installation.
#[derive(Debug, Clone)]
pub struct Store {
    directory: PathBuf,
}

#[derive(Debug)]
pub enum LaunchAccountError {
    Removed { provider: Provider, key: Uuid },
    Unreadable(io::Error),
}

impl std::fmt::Display for LaunchAccountError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Removed { provider, .. } => write!(
                formatter,
                "This {} account was removed. Choose another account before continuing.",
                provider.label()
            ),
            Self::Unreadable(error) => {
                write!(formatter, "Cannot verify the selected account: {error}")
            }
        }
    }
}

impl std::error::Error for LaunchAccountError {}

/// Keep this lease only across the child-spawn step, never a provider turn or
/// handshake. Removal uses the same lifecycle lock before forgetting a home.
pub struct LaunchAccountLease {
    _lock: crate::instance::OwnedLock,
}

/// Called immediately around an actual process creation, including probes.
/// Live attachment never needs this lease and remains usable after removal.
pub(crate) fn authorize_spawn(
    environment: &std::collections::BTreeMap<String, String>,
    provider: Option<Provider>,
) -> Result<Option<LaunchAccountLease>, LaunchAccountError> {
    match (Store::installation(), Homes::installation()) {
        (Some(store), Some(homes)) => store.authorize_environment(&homes, environment, provider),
        _ => Ok(None),
    }
}

/// Commands have already selected their explicit provider home. Inspect only
/// those non-secret path overrides; inherited credentials are never collected.
pub(crate) fn authorize_command(
    command: &std::process::Command,
) -> Result<Option<LaunchAccountLease>, LaunchAccountError> {
    let environment = command
        .get_envs()
        .filter_map(|(name, value)| {
            let name = name.to_str()?;
            if !Account::PROVIDERS
                .iter()
                .any(|provider| super::home_variable(*provider) == Some(name))
            {
                return None;
            }
            Some((name.to_owned(), value?.to_str()?.to_owned()))
        })
        .collect();
    authorize_spawn(&environment, None)
}

impl Store {
    pub fn authorize_launch(
        &self,
        homes: &Homes,
        recipe: &crate::model::LaunchRecipe,
        provider: Option<Provider>,
    ) -> Result<Option<LaunchAccountLease>, LaunchAccountError> {
        self.authorize_environment(homes, &recipe.environment, provider)
    }

    fn authorize_environment(
        &self,
        homes: &Homes,
        environment: &std::collections::BTreeMap<String, String>,
        provider: Option<Provider>,
    ) -> Result<Option<LaunchAccountLease>, LaunchAccountError> {
        let selected: Vec<_> = Account::PROVIDERS
            .into_iter()
            .filter(|candidate| provider.is_none_or(|provider| provider == *candidate))
            .filter_map(|provider| {
                environment
                    .get(super::home_variable(provider)?)
                    .and_then(|path| homes.managed_identity(Path::new(path)))
            })
            .collect();
        if selected.is_empty() {
            return Ok(None);
        }
        let lock = homes
            .lifecycle_lock()
            .map_err(LaunchAccountError::Unreadable)?;
        let registry = self.load().map_err(LaunchAccountError::Unreadable)?;
        if registry.is_newer() {
            return Err(LaunchAccountError::Unreadable(io::Error::new(
                io::ErrorKind::Unsupported,
                "the accounts were saved by a newer cagents",
            )));
        }
        for (provider, key) in selected {
            if !registry.contains_managed_home(homes, provider, key) {
                return Err(LaunchAccountError::Removed { provider, key });
            }
        }
        Ok(Some(LaunchAccountLease { _lock: lock }))
    }
    pub fn installation() -> Option<Self> {
        crate::instance::project_dirs().map(|dirs| Self::at(dirs.config_dir()))
    }

    pub fn at(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    /// Creates and records a pending sign-in as one lifecycle transaction.
    /// The closure may seed bounded local configuration, never launch a CLI.
    pub fn create_pending(
        &self,
        homes: &Homes,
        provider: Provider,
        started_at: u64,
        create: impl FnOnce() -> io::Result<(Uuid, PathBuf)>,
    ) -> io::Result<(Uuid, PathBuf)> {
        let _lifecycle = homes.lifecycle_lock()?;
        let _lock = self.lock()?;
        let mut registry = read(&self.path())?;
        if registry.is_newer() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the accounts were saved by a newer cagents",
            ));
        }
        let (key, home) = create()?;
        registry.begin(provider, key, started_at);
        registry.version = VERSION;
        let result = registry
            .revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("accounts revision exhausted"))
            .and_then(|revision| {
                registry.revision = revision;
                write(&self.directory, &self.path(), &registry)
            });
        if let Err(error) = result {
            let _ = homes.remove(provider, key);
            return Err(error);
        }
        Ok((key, home))
    }

    /// Forget and park under the same lifecycle lock as spawning. A failed
    /// deletion cannot make an unregistered home launchable again; the sweep
    /// finishes parked trees later. Linked aliases still protect their home.
    pub fn remove_home(
        &self,
        homes: &Homes,
        provider: Provider,
        key: Uuid,
        registered: bool,
    ) -> io::Result<(Registry, Option<String>)> {
        let (registry, error) = {
            let _lifecycle = homes.lifecycle_lock()?;
            let (registry, ()) = self.update(|registry| {
                if registered {
                    registry.unregister(key);
                } else {
                    registry.abandon(key);
                }
            })?;
            let error = if registry.contains_managed_home(homes, provider, key) {
                None
            } else {
                homes
                    .park(provider, key)
                    .err()
                    .map(|error| error.to_string())
            };
            (registry, error)
        };
        homes.finish_removals();
        Ok((registry, error))
    }

    pub fn register_pending(
        &self,
        homes: &Homes,
        account: Registered,
        system: &[(Provider, Identity)],
    ) -> io::Result<(Registry, Result<(), AccountId>)> {
        let _lifecycle = homes.lifecycle_lock()?;
        self.update_checked(|registry| {
            if registry.find(account.key).is_some_and(|existing| {
                existing.provider == account.provider && existing.linked_home.is_none()
            }) {
                return Ok(Ok(()));
            }
            if !registry
                .pending
                .iter()
                .any(|pending| pending.key == account.key && pending.provider == account.provider)
            {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "The pending sign-in was removed before it completed",
                ));
            }
            Ok(registry.register(account, system))
        })
    }

    fn path(&self) -> PathBuf {
        self.directory.join(FILE_NAME)
    }

    fn lock(&self) -> io::Result<crate::instance::OwnedLock> {
        crate::secure_file::prepare_private_directory(&self.directory)?;
        crate::instance::owned_lock(&self.directory.join(LOCK_NAME), libc::LOCK_EX)
    }

    /// The registry as it stands. A missing file is an empty registry; a
    /// damaged one is an error, so a change never replaces what it could not
    /// read.
    pub fn load(&self) -> io::Result<Registry> {
        let _lock = self.lock()?;
        read(&self.path())
    }

    /// Applies `change` to the registry as it stands on disk and writes the
    /// result, all under the lock; returns the registry written and what
    /// `change` returned.
    pub fn update<T>(&self, change: impl FnOnce(&mut Registry) -> T) -> io::Result<(Registry, T)> {
        self.update_checked(|registry| Ok(change(registry)))
    }

    fn update_checked<T>(
        &self,
        change: impl FnOnce(&mut Registry) -> io::Result<T>,
    ) -> io::Result<(Registry, T)> {
        let _lock = self.lock()?;
        let path = self.path();
        let mut registry = read(&path)?;
        if registry.version > VERSION {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the accounts were saved by a newer cagents",
            ));
        }
        let before = registry.clone();
        let result = change(&mut registry)?;
        if registry != before {
            registry.version = VERSION;
            registry.revision = before
                .revision
                .checked_add(1)
                .ok_or_else(|| io::Error::other("accounts revision exhausted"))?;
            write(&self.directory, &path, &registry)?;
        }
        Ok((registry, result))
    }

    /// Loads the registry, then finishes any removal an earlier run was
    /// interrupted in - outside the lock, since deleting a home can take a
    /// while and a parked home is no account's any more.
    pub fn load_and_sweep(&self, homes: &Homes) -> io::Result<Registry> {
        let registry = {
            let _lifecycle = homes.lifecycle_lock()?;
            let _lock = self.lock()?;
            let exists = self.path().try_exists()?;
            let registry = read(&self.path())?;
            if exists && !registry.is_newer() {
                homes.park_orphans(&registry)?;
            }
            registry
        };
        homes.finish_removals();
        Ok(registry)
    }
}

fn read(path: &Path) -> io::Result<Registry> {
    let mut file = match crate::secure_file::open_existing_private_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Registry::default()),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the accounts file is too large",
        ));
    }
    let mut text = String::new();
    Read::by_ref(&mut file)
        .take(MAX_BYTES)
        .read_to_string(&mut text)?;
    toml::from_str(&text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write(directory: &Path, path: &Path, registry: &Registry) -> io::Result<()> {
    let mut document = toml::to_string_pretty(registry).map_err(io::Error::other)?;
    if !document.ends_with('\n') {
        document.push('\n');
    }
    let mut temporary = tempfile::Builder::new()
        .prefix(".accounts.toml.")
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .tempfile_in(directory)?;
    temporary.write_all(document.as_bytes())?;
    temporary.as_file().sync_all()?;
    // The rename replaces whatever is at `path` - a link included - rather
    // than writing through it.
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// A provider written as its executable name, which is what a reader
/// looking at the file would call it.
mod provider_name {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    use crate::agent::Provider;

    pub fn serialize<S: Serializer>(provider: &Provider, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(provider.executable())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Provider, D::Error> {
        let name = String::deserialize(deserializer)?;
        super::super::Account::PROVIDERS
            .into_iter()
            .find(|provider| provider.executable() == name)
            .ok_or_else(|| D::Error::custom(format!("unknown account provider {name:?}")))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn account_recipe(home: &Path) -> crate::model::LaunchRecipe {
        let mut recipe = crate::model::LaunchRecipe::shell("/".into()).unwrap();
        recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), home.to_str().unwrap().into());
        recipe
    }

    #[test]
    fn removal_waits_for_spawn_lease_and_prevents_later_launch_or_registration() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("config"));
        let homes = Homes::at(directory.path().join("accounts"));
        let (key, home) = store
            .create_pending(&homes, Provider::Claude, 1, || {
                homes.create(Provider::Claude, None)
            })
            .unwrap();
        let recipe = account_recipe(&home);
        let lease = store
            .authorize_launch(&homes, &recipe, None)
            .unwrap()
            .unwrap();
        let (started, starts) = std::sync::mpsc::channel();
        let (done, result) = std::sync::mpsc::channel();
        let removal_store = store.clone();
        let removal_homes = homes.clone();
        let removal = std::thread::spawn(move || {
            started.send(()).unwrap();
            done.send(removal_store.remove_home(&removal_homes, Provider::Claude, key, false))
                .unwrap();
        });
        starts.recv().unwrap();
        assert!(
            result
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );
        assert!(home.is_dir());
        drop(lease);
        result
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap()
            .unwrap();
        removal.join().unwrap();
        assert!(!home.exists());
        assert!(
            matches!(store.authorize_launch(&homes, &recipe, None), Err(LaunchAccountError::Removed { key: removed, .. }) if removed == key)
        );
        let mut account = managed(Provider::Claude, "fixture@example.com");
        account.key = key;
        assert!(store.register_pending(&homes, account, &[]).is_err());
        assert!(store.load().unwrap().find(key).is_none());
        assert!(!home.exists());
    }

    #[test]
    fn orphan_sweep_preserves_pending_registered_linked_and_recent_homes() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("config"));
        let homes = Homes::at(directory.path().join("accounts"));
        let mut homes_created = Vec::new();
        for _ in 0..5 {
            homes_created.push(homes.create(Provider::Claude, None).unwrap());
        }
        for (_, path) in &homes_created[..4] {
            fs::File::open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
                .unwrap();
        }
        store
            .update(|registry| {
                registry.begin(Provider::Claude, homes_created[0].0, 1);
                let mut account = managed(Provider::Claude, "registered@example.com");
                account.key = homes_created[1].0;
                registry.accounts.push(account);
                let mut linked = managed(Provider::Claude, "linked@example.com");
                linked.linked_home = Some(homes_created[2].1.clone());
                registry.accounts.push(linked);
            })
            .unwrap();
        store.load_and_sweep(&homes).unwrap();
        for index in [0, 1, 2, 4] {
            assert!(homes_created[index].1.is_dir());
        }
        assert!(!homes_created[3].1.exists());
        let recipe = account_recipe(&directory.path().join("unmanaged"));
        assert!(
            store
                .authorize_launch(&homes, &recipe, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn orphan_sweep_finishes_an_interrupted_removal_and_a_recreated_home() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("config"));
        let homes = Homes::at(directory.path().join("accounts"));
        let (key, home) = homes.create(Provider::Codex, None).unwrap();
        fs::write(home.join("old-history"), "fixture").unwrap();
        let parked = home.parent().unwrap().join(format!(".removing-{key}"));
        fs::rename(&home, &parked).unwrap();
        fs::create_dir(&home).unwrap();
        fs::write(home.join("recreated"), "fixture").unwrap();
        fs::File::open(&home)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        // A supported existing registry authorizes orphan cleanup.
        store.load().unwrap();
        write(&store.directory, &store.path(), &Registry::default()).unwrap();
        store.load_and_sweep(&homes).unwrap();
        assert!(!home.exists());
        assert!(!parked.exists());
    }

    #[test]
    fn unreadable_newer_or_missing_registry_never_sweeps_unknown_homes() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("config"));
        let homes = Homes::at(directory.path().join("accounts"));
        let (_, home) = homes.create(Provider::Claude, None).unwrap();
        fs::File::open(&home)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        store.load_and_sweep(&homes).unwrap();
        assert!(home.is_dir());
        let mut newer = Registry::default();
        newer.version = VERSION + 1;
        write(&store.directory, &store.path(), &newer).unwrap();
        store.load_and_sweep(&homes).unwrap();
        assert!(home.is_dir());
        fs::write(store.path(), "invalid = [").unwrap();
        assert!(store.load_and_sweep(&homes).is_err());
        assert!(home.is_dir());
    }

    fn managed(provider: Provider, email: &str) -> Registered {
        Registered {
            key: Uuid::new_v4(),
            provider,
            linked_home: None,
            label: None,
            email: Some(email.into()),
            organization: None,
            added_at: 1,
        }
    }

    #[test]
    fn a_registry_round_trips_through_a_private_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("config"));
        assert_eq!(store.load().unwrap(), Registry::default());
        let work = managed(Provider::Claude, "work@example.com");
        let key = work.key;
        let (registry, registered) = store
            .update(|registry| {
                let registered = registry.register(work.clone(), &[]);
                let account = Account::managed(Provider::Claude, key, PathBuf::new());
                registry.set_default(&account);
                registry.set_label(&account, Some("  Work ".into()));
                registered
            })
            .unwrap();
        assert_eq!(registered, Ok(()));
        let path = directory.path().join("config").join(FILE_NAME);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let reloaded = store.load().unwrap();
        assert_eq!(reloaded, registry);
        let homes = Homes::at("/data/accounts");
        let default = reloaded.default_account(Provider::Claude, Some(&homes));
        assert_eq!(default.registry_key(), Some(key));
        assert_eq!(
            default.resolved_home(),
            Some(homes.path(Provider::Claude, key))
        );
        assert_eq!(reloaded.label(&default), Some("Work"));
        assert!(reloaded.is_default(&default));
        assert!(!reloaded.is_default(&Account::system(Provider::Claude)));
        assert!(reloaded.is_default(&Account::system(Provider::Codex)));
        assert_eq!(reloaded.accounts(Some(&homes)).len(), 3);
    }

    #[test]
    fn linked_home_is_selected_by_path_and_unlinking_keeps_its_files() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("existing-codex");
        fs::create_dir(&home).unwrap();
        let history = home.join("session_index.jsonl");
        fs::write(&history, "saved session").unwrap();
        let store = Store::at(directory.path().join("config"));
        let homes = Homes::at(directory.path().join("managed"));
        let key = Uuid::new_v4();
        store
            .update(|registry| {
                registry
                    .link(Provider::Codex, key, home.clone(), None, Some(&homes), 1)
                    .unwrap();
                // A second home with the same login identity may have a
                // different history; only the path is unique.
                registry
                    .register(managed(Provider::Codex, "same@example.com"), &[])
                    .unwrap();
                registry.set_default(&Account::linked(Provider::Codex, key, home.clone()));
            })
            .unwrap();
        let mut registry = store.load().unwrap();
        let default = registry.default_account(Provider::Codex, Some(&homes));
        assert_eq!(default.resolved_home(), Some(home.clone()));
        assert!(default.is_linked());
        assert_eq!(
            registry.link(
                Provider::Codex,
                Uuid::new_v4(),
                home.clone(),
                None,
                Some(&homes),
                2
            ),
            Err("That account home is already listed".into())
        );
        registry.unregister(key).unwrap();
        assert!(history.is_file());
    }

    #[test]
    fn linking_and_signing_in_to_the_same_identity_are_order_independent() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("existing");
        fs::create_dir(&home).unwrap();
        let mut registry = Registry::default();
        let linked = Uuid::new_v4();
        registry
            .link(Provider::Codex, linked, home.clone(), None, None, 1)
            .unwrap();
        registry.observe_identity(
            linked,
            &Identity {
                email: Some("same@example.com".into()),
                ..Identity::default()
            },
        );
        registry
            .register(managed(Provider::Codex, "same@example.com"), &[])
            .expect("a managed home has its own history and may share a login with a linked home");
        assert_eq!(registry.registered().len(), 2);
    }

    #[test]
    fn project_choices_override_global_defaults_and_survive_reload() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("config"));
        let homes = Homes::at(directory.path().join("managed"));
        let project = Path::new("/projects/redact/.git");
        let other = Path::new("/projects/other/.git");
        let home = directory.path().join("history");
        fs::create_dir(&home).unwrap();
        let key = Uuid::new_v4();
        let linked = Account::linked(Provider::Codex, key, home.clone());
        store
            .update(|registry| {
                registry
                    .link(Provider::Codex, key, home, None, Some(&homes), 1)
                    .unwrap();
                registry.set_project_default(project, &linked);
            })
            .unwrap();
        let mut registry = store.load().unwrap();
        assert_eq!(
            registry.project_default_account(project, Provider::Codex, Some(&homes)),
            linked
        );
        assert!(registry.is_project_default(project, &linked));
        assert_eq!(
            registry.project_default_account(other, Provider::Codex, Some(&homes)),
            Account::system(Provider::Codex)
        );
        registry.clear_project_default(project, Provider::Codex);
        assert_eq!(registry.project_choice(project, Provider::Codex), None);
        registry.set_project_default(project, &linked);
        registry.unregister(key).unwrap();
        assert_eq!(registry.project_choice(project, Provider::Codex), None);
        assert_eq!(
            registry.project_default_account(project, Provider::Codex, Some(&homes)),
            Account::system(Provider::Codex)
        );
    }

    #[test]
    fn legacy_window_project_ids_migrate_to_the_shared_repository_identity() {
        let mut registry = Registry::default();
        let old_window_id = ProjectId::new();
        let other_window_id = ProjectId::new();
        let repository = PathBuf::from("/projects/redact/.git");
        registry.project_defaults.insert(
            old_window_id.to_string(),
            BTreeMap::from([("codex".into(), SYSTEM.into())]),
        );

        assert!(registry.migrate_project_ids(&[(old_window_id, repository.clone())]));
        assert!(!registry.migrate_project_ids(&[(other_window_id, repository.clone())]));
        assert_eq!(
            registry.project_choice(&repository, Provider::Codex),
            Some(SYSTEM)
        );
        assert!(
            !registry
                .project_defaults
                .contains_key(&old_window_id.to_string())
        );
    }

    /// A home cagents creates records display metadata only, never CLI
    /// credentials or a path to its files.
    #[test]
    fn the_file_holds_display_metadata_and_nothing_token_shaped() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path());
        let mut work = managed(Provider::Codex, "work@example.com");
        work.label = Some("Work".into());
        work.organization = Some("Acme".into());
        store
            .update(|registry| {
                registry.register(work.clone(), &[]).unwrap();
                registry.set_default(&Account::system(Provider::Codex));
                registry.set_label(&Account::system(Provider::Claude), Some("Personal".into()));
                registry.begin(Provider::Claude, Uuid::from_u128(9), 2);
            })
            .unwrap();
        let text = fs::read_to_string(directory.path().join(FILE_NAME)).unwrap();
        let document: toml::Table = toml::from_str(&text).unwrap();
        let allowed = [
            "version",
            "revision",
            "defaults",
            "project_defaults",
            "system_labels",
            "account",
            "linked_home",
            "key",
            "provider",
            "label",
            "email",
            "organization",
            "added_at",
            "pending",
            "started_at",
            // Provider names, as keys of the two maps.
            "claude",
            "codex",
        ];
        fn keys(value: &toml::Value, out: &mut Vec<String>) {
            match value {
                toml::Value::Table(table) => {
                    for (key, value) in table {
                        out.push(key.clone());
                        keys(value, out);
                    }
                }
                toml::Value::Array(values) => values.iter().for_each(|value| keys(value, out)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        keys(&toml::Value::Table(document), &mut found);
        for key in &found {
            assert!(allowed.contains(&key.as_str()), "unexpected field {key:?}");
            let lower = key.to_ascii_lowercase();
            for shape in [
                "token",
                "secret",
                "auth",
                "credential",
                "password",
                "key_",
                "path",
                "home",
            ] {
                assert!(!lower.contains(shape), "{key:?} looks like {shape}");
            }
        }
        assert!(!text.contains('/'), "no path is ever written: {text}");
    }

    #[test]
    fn a_sign_in_stays_pending_until_it_is_registered_or_abandoned() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path());
        let work = managed(Provider::Claude, "work@example.com");
        let (first, second) = (work.key, Uuid::from_u128(77));
        let (registry, ()) = store
            .update(|registry| {
                registry.begin(Provider::Claude, first, 10);
                registry.begin(Provider::Codex, second, 20);
                registry.begin(Provider::Codex, second, 30);
            })
            .unwrap();
        assert_eq!(registry.pending().len(), 2, "a sign-in is recorded once");
        assert_eq!(store.load().unwrap().pending(), registry.pending());
        let (registry, registered) = store
            .update(|registry| registry.register(work.clone(), &[]))
            .unwrap();
        assert_eq!(registered, Ok(()));
        assert_eq!(registry.pending().len(), 1, "registering ends its sign-in");
        let (registry, ()) = store.update(|registry| registry.abandon(second)).unwrap();
        assert!(registry.pending().is_empty());
    }

    #[test]
    fn a_second_home_for_the_same_login_is_refused() {
        let mut registry = Registry::default();
        let first = managed(Provider::Claude, "me@example.com");
        registry.register(first.clone(), &[]).unwrap();
        let again = managed(Provider::Claude, "ME@example.com");
        assert_eq!(
            registry.register(again, &[]),
            Err(Account::managed(Provider::Claude, first.key, PathBuf::new()).id())
        );
        // The same address at the other provider is another account.
        registry
            .register(managed(Provider::Codex, "me@example.com"), &[])
            .unwrap();
        // And the system account counts too.
        let system = [(
            Provider::Codex,
            Identity {
                email: Some("sys@example.com".into()),
                ..Identity::default()
            },
        )];
        assert_eq!(
            registry.register(managed(Provider::Codex, "sys@example.com"), &system),
            Err(Account::system(Provider::Codex).id())
        );
        assert_eq!(registry.registered().len(), 2);
    }

    #[test]
    fn removing_an_account_forgets_it_as_a_default() {
        let mut registry = Registry::default();
        let work = managed(Provider::Claude, "w@example.com");
        registry.register(work.clone(), &[]).unwrap();
        let account = Account::managed(Provider::Claude, work.key, PathBuf::new());
        registry.set_default(&account);
        assert!(registry.unregister(work.key).is_some());
        assert!(registry.is_default(&Account::system(Provider::Claude)));
        assert!(registry.unregister(work.key).is_none());
        assert_eq!(
            registry.default_account(Provider::Claude, Some(&Homes::at("/a"))),
            Account::system(Provider::Claude)
        );
    }

    #[test]
    fn a_damaged_or_newer_file_is_never_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path());
        let path = directory.path().join(FILE_NAME);
        fs::write(&path, "not = [toml").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(store.load().is_err());
        assert!(store.update(|_| ()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "not = [toml");
        fs::write(&path, "version = 99\n").unwrap();
        assert!(
            store
                .update(|registry| registry.set_default(&Account::system(Provider::Claude)))
                .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "version = 99\n");
    }

    #[test]
    fn a_registry_behind_a_link_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("elsewhere.toml");
        fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, directory.path().join(FILE_NAME)).unwrap();
        assert!(Store::at(directory.path()).load().is_err());
    }
}
