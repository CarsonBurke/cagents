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
const VERSION: u32 = 6;
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

    /// Whether `reported` is a different login from the one this account
    /// was added with. A home cagents keeps for one login that reports
    /// another has had its credentials replaced, which is never a change
    /// to take quietly.
    pub fn conflicts_with(&self, reported: &Identity) -> bool {
        self.email.is_some() && reported.email.is_some() && !self.identity().same_login(reported)
    }
}

/// How an account's reported login compares with the one it was added with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    /// Nothing new to record.
    Unchanged,
    /// Details the registry lacked were recorded.
    Recorded,
    /// The home reports a different login. The recorded one is kept.
    Conflict,
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
    #[serde(default, skip_serializing_if = "PendingOrigin::is_sign_in")]
    pub origin: PendingOrigin,
}

/// Import-owned pending homes require the dedicated expected-identity path.
/// Legacy entries remain ordinary sign-ins; writing an import requires format6
/// so old managed-home launch authorization refuses the newer registry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PendingOrigin {
    #[default]
    SignIn,
    CacheImport {
        operation: Uuid,
    },
}

impl PendingOrigin {
    pub fn is_sign_in(&self) -> bool {
        matches!(self, Self::SignIn)
    }
}

impl Pending {
    pub fn is_import(&self) -> bool {
        !self.origin.is_sign_in()
    }
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
    /// Hidden duplicate homes remain registered so saved sessions can resume
    /// and orphan cleanup never deletes their credentials or history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    archived: Vec<Uuid>,
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
    /// Credential homes affected by an externally proposed preference edit.
    /// Include physical aliases and both ends of changed routing preferences.
    #[cfg(feature = "host")]
    pub(crate) fn changed_account_homes(&self, next: &Self, homes: Option<&Homes>) -> Vec<PathBuf> {
        let mut keys = std::collections::BTreeSet::new();
        for account in self.accounts.iter().chain(&next.accounts) {
            if self.find(account.key) != next.find(account.key)
                || self.is_archived(account.key) != next.is_archived(account.key)
            {
                keys.insert(account.key);
            }
        }
        let mut paths = Vec::new();
        for registry in [self, next] {
            paths.extend(
                registry
                    .all_accounts(homes)
                    .into_iter()
                    .filter(|account| {
                        account
                            .registry_key()
                            .is_some_and(|key| keys.contains(&key))
                    })
                    .filter_map(|account| account.resolved_home()),
            );
        }
        for provider in Account::PROVIDERS {
            let name = provider.executable();
            if self.defaults.get(name) != next.defaults.get(name)
                || self.system_labels.get(name) != next.system_labels.get(name)
            {
                paths.extend(
                    [
                        self.default_account(provider, homes),
                        next.default_account(provider, homes),
                    ]
                    .into_iter()
                    .filter_map(|account| account.resolved_home()),
                );
            }
            for repository in self
                .project_defaults
                .keys()
                .chain(next.project_defaults.keys())
            {
                let old = self
                    .project_defaults
                    .get(repository)
                    .and_then(|choices| choices.get(name));
                let new = next
                    .project_defaults
                    .get(repository)
                    .and_then(|choices| choices.get(name));
                if old != new {
                    for value in [old, new].into_iter().flatten() {
                        paths.extend(
                            self.all_accounts(homes)
                                .into_iter()
                                .chain(next.all_accounts(homes))
                                .filter(|account| {
                                    account.provider == provider
                                        && (account
                                            .registry_key()
                                            .map(|key| key.to_string())
                                            .as_ref()
                                            == Some(value)
                                            || (value == "system" && account.is_system()))
                                })
                                .filter_map(|account| account.resolved_home()),
                        );
                    }
                }
            }
        }
        paths
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
        self.all_accounts(homes)
            .into_iter()
            .filter(|account| {
                !account
                    .registry_key()
                    .is_some_and(|key| self.is_archived(key))
            })
            .collect()
    }

    pub fn is_archived(&self, key: Uuid) -> bool {
        self.archived.contains(&key)
    }

    /// Includes preserved homes for resolving existing session recipes.
    pub fn all_accounts(&self, homes: Option<&Homes>) -> Vec<Account> {
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
            .filter(|managed| managed.provider == provider && !self.is_archived(managed.key));
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
            Some(key) => !self.is_archived(key) && Uuid::parse_str(chosen).ok() == Some(key),
            None => account.is_system() && chosen == SYSTEM,
        }
    }

    pub fn set_project_default(&mut self, repository: &Path, account: &Account) {
        if account
            .registry_key()
            .is_some_and(|key| self.is_archived(key))
        {
            return;
        }
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
            AccountHome::System | AccountHome::SystemAt { .. } => chosen.is_none_or(|chosen| {
                chosen == SYSTEM
                    || Uuid::parse_str(chosen)
                        .ok()
                        .and_then(|key| self.find(key))
                        .filter(|account| !self.is_archived(account.key))
                        .is_none()
            }),
            AccountHome::Managed { key, .. } | AccountHome::Linked { key, .. } => {
                !self.is_archived(*key)
                    && chosen.is_some_and(|chosen| Uuid::parse_str(chosen).ok() == Some(*key))
            }
            AccountHome::Brokered { .. } => false,
        }
    }

    pub fn set_default(&mut self, account: &Account) {
        if account
            .registry_key()
            .is_some_and(|key| self.is_archived(key))
        {
            return;
        }
        let value = match &account.home {
            AccountHome::System | AccountHome::SystemAt { .. } => SYSTEM.to_owned(),
            AccountHome::Managed { key, .. } | AccountHome::Linked { key, .. } => {
                key.hyphenated().to_string()
            }
            AccountHome::Brokered { .. } => return,
        };
        self.defaults
            .insert(account.provider.executable().to_owned(), value);
    }

    pub fn automatic(&self, provider: Provider) -> bool {
        !self.defaults.contains_key(provider.executable())
    }

    pub fn set_automatic(&mut self, provider: Provider) {
        self.defaults.remove(provider.executable());
    }

    /// The name the reader gave `account`, if any. A brokered account is
    /// named by its CLI, not here.
    pub fn label(&self, account: &Account) -> Option<&str> {
        match &account.home {
            AccountHome::System | AccountHome::SystemAt { .. } => self
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
            AccountHome::System | AccountHome::SystemAt { .. } => {
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
                && !self.archived.contains(&existing.key)
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
    /// different histories. Repeating an unambiguous active linked home returns
    /// its original key without changing metadata. Linking an archived home
    /// restores its row.
    pub fn link(
        &mut self,
        provider: Provider,
        key: Uuid,
        path: PathBuf,
        system_home: Option<&Path>,
        homes: Option<&Homes>,
        added_at: u64,
    ) -> Result<Uuid, String> {
        if !path.is_absolute() {
            return Err("Choose an absolute account home path".into());
        }
        if system_home
            .and_then(|home| fs::canonicalize(home).ok())
            .is_some_and(|home| home == path)
        {
            return Err("That account home is already listed".into());
        }
        let matching = self
            .all_accounts(homes)
            .into_iter()
            .filter(|account| {
                account.provider == provider
                    && account
                        .resolved_home()
                        .and_then(|home| fs::canonicalize(home).ok())
                        .is_some_and(|home| home == path)
            })
            .collect::<Vec<_>>();
        if matching.iter().any(|account| {
            !account
                .registry_key()
                .is_some_and(|key| self.is_archived(key))
        }) {
            // A reply may have been lost after the registration committed.
            // Recover only one exact linked home, never a System/managed home
            // or an ambiguous active/archived registration sharing its path.
            if let [account] = matching.as_slice()
                && let AccountHome::Linked { key, .. } = &account.home
            {
                return Ok(*key);
            }
            return Err("That account home is already listed".into());
        }
        if let Some(existing) = self.accounts.iter().find(|account| {
            account.provider == provider
                && self.archived.contains(&account.key)
                && account
                    .linked_home
                    .as_ref()
                    .cloned()
                    .or_else(|| homes.map(|homes| homes.path(provider, account.key)))
                    .and_then(|home| fs::canonicalize(home).ok())
                    .as_ref()
                    == Some(&path)
        }) {
            let key = existing.key;
            self.archived.retain(|archived| *archived != key);
            return Ok(key);
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
        Ok(key)
    }

    /// Records a home created for a sign-in that is about to start.
    pub fn begin(&mut self, provider: Provider, key: Uuid, started_at: u64) {
        if self.find(key).is_none() && !self.pending.iter().any(|pending| pending.key == key) {
            self.pending.push(Pending {
                key,
                provider,
                started_at,
                origin: PendingOrigin::SignIn,
            });
        }
    }

    /// True only for the exact import operation and its original managed key.
    pub(crate) fn is_import_pending(&self, operation: Uuid) -> bool {
        !operation.is_nil()
            && self.find(operation).is_none()
            && !self.is_archived(operation)
            && self
                .pending
                .iter()
                .filter(|pending| pending.key == operation)
                .count()
                == 1
            && self.pending.iter().any(|pending| {
                pending.key == operation
                    && pending.provider == Provider::Codex
                    && pending.origin == PendingOrigin::CacheImport { operation }
            })
    }

    fn import_key_referenced(&self, key: Uuid) -> bool {
        self.defaults
            .values()
            .chain(self.project_defaults.values().flat_map(|v| v.values()))
            .any(|value| Uuid::parse_str(value).ok() == Some(key))
    }

    fn require_fresh_import_key(&self, key: Uuid) -> io::Result<()> {
        if key.is_nil()
            || self.find(key).is_some()
            || self.pending.iter().any(|pending| pending.key == key)
            || self.is_archived(key)
            || self.import_key_referenced(key)
        {
            return Err(io::Error::other(
                "The import destination key is already used or referenced",
            ));
        }
        Ok(())
    }

    /// Sign-ins that have not finished, oldest first.
    pub fn pending(&self) -> &[Pending] {
        &self.pending
    }

    /// Forgets a sign-in whose home is gone.
    pub fn abandon(&mut self, key: Uuid) {
        self.pending.retain(|pending| pending.key != key);
    }

    /// Records what the provider reported about a managed account that the
    /// registry does not know yet, so the table and the dedupe of the next
    /// sign-in know who it is.
    ///
    /// The login an account was added with is its identity: a home that
    /// later reports a different one is a [`Observed::Conflict`], and the
    /// recorded login is kept for the reader to see. Only an explicit sign-in
    /// to that account changes it (see [`Self::adopt_identity`]).
    pub fn observe_identity(&mut self, key: Uuid, identity: &Identity) -> Observed {
        let Some(managed) = self.accounts.iter_mut().find(|managed| managed.key == key) else {
            return Observed::Unchanged;
        };
        if managed.conflicts_with(identity) {
            return Observed::Conflict;
        }
        let email = managed.email.clone().or_else(|| identity.email.clone());
        let organization = managed
            .organization
            .clone()
            .or_else(|| identity.organization.clone());
        if managed.email == email && managed.organization == organization {
            return Observed::Unchanged;
        }
        managed.email = email;
        managed.organization = organization;
        Observed::Recorded
    }

    /// Replaces the login recorded for a managed account with the one the
    /// reader just signed it in to.
    pub fn adopt_identity(&mut self, key: Uuid, identity: &Identity) -> bool {
        let Some(managed) = self.accounts.iter_mut().find(|managed| managed.key == key) else {
            return false;
        };
        if identity.email.is_none() || managed.identity().same_login(identity) {
            return false;
        }
        managed.email = identity.email.clone();
        managed.organization = identity.organization.clone();
        true
    }

    /// Forgets a managed account, and its place as a default.
    pub fn unregister(&mut self, key: Uuid) -> Option<Registered> {
        let index = self
            .accounts
            .iter()
            .position(|managed| managed.key == key)?;
        let removed = self.accounts.remove(index);
        self.archived.retain(|archived| *archived != key);
        let key = key.hyphenated().to_string();
        self.defaults.retain(|_, chosen| *chosen != key);
        self.project_defaults.retain(|_, choices| {
            choices.retain(|_, chosen| *chosen != key);
            !choices.is_empty()
        });
        Some(removed)
    }

    /// Hides a redundant home while preserving its files and saved-session
    /// identity. New work uses the retained account instead.
    pub fn archive_duplicate(&mut self, key: Uuid, retained: &Account) -> bool {
        let Some(duplicate) = self.find(key) else {
            return false;
        };
        if duplicate.provider != retained.provider
            || self.archived.contains(&key)
            || retained.registry_key() == Some(key)
            || retained.is_brokered()
        {
            return false;
        }
        let replacement = match retained.registry_key() {
            Some(keep)
                if self
                    .find(keep)
                    .is_some_and(|account| account.provider == retained.provider)
                    && !self.archived.contains(&keep) =>
            {
                keep.hyphenated().to_string()
            }
            None if retained.is_system() => SYSTEM.to_owned(),
            _ => return false,
        };
        let removed = key.hyphenated().to_string();
        for chosen in self.defaults.values_mut().chain(
            self.project_defaults
                .values_mut()
                .flat_map(|choices| choices.values_mut()),
        ) {
            if *chosen == removed {
                *chosen = replacement.clone();
            }
        }
        self.archived.push(key);
        true
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

/// The value of [`super::SELECTION_MARKER`] for a process about to start
/// with the provider homes `home_of` names: each home that belongs to an
/// account this installation manages or links, as `VARIABLE=path` lines.
/// `None` when it runs on no such account - the system account included.
///
/// The registry is read without its lock: it is replaced by rename, and a
/// launch may already hold the account lifecycle lock.
#[cfg(feature = "host")]
pub(crate) fn selection_marker(home_of: impl Fn(&str) -> Option<String>) -> Option<String> {
    let linked: Vec<PathBuf> = Store::installation()
        .and_then(|store| read(&store.path()).ok())
        .map(|registry| {
            registry
                .accounts
                .into_iter()
                .filter_map(|account| account.linked_home)
                .collect()
        })
        .unwrap_or_default();
    let lines: Vec<String> = Account::PROVIDERS
        .into_iter()
        .filter_map(|provider| {
            let variable = super::home_variable(provider)?;
            let value = home_of(variable).filter(|value| !value.is_empty())?;
            let path = Path::new(&value);
            (super::is_account_namespace(provider, path) || linked.iter().any(|home| home == path))
                .then(|| format!("{variable}={value}"))
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
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
            if registry
                .pending
                .iter()
                .any(|pending| pending.key == key && pending.is_import())
            {
                return Err(LaunchAccountError::Unreadable(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "The imported cache requires explicit identity verification before this account can launch",
                )));
            }
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
        self.create_pending_with_mode(homes, provider, started_at, create, false, |_| Ok(()))
    }

    #[cfg(feature = "host")]
    pub(crate) fn try_create_pending(
        &self,
        homes: &Homes,
        provider: Provider,
        started_at: u64,
        create: impl FnOnce() -> io::Result<(Uuid, PathBuf)>,
    ) -> io::Result<(Uuid, PathBuf)> {
        self.create_pending_with_mode(homes, provider, started_at, create, true, |_| Ok(()))
    }

    #[cfg(feature = "host")]
    pub(crate) fn try_create_pending_reserved(
        &self,
        reservation: &crate::host::AccountReservation,
        homes: &Homes,
        provider: Provider,
        started_at: u64,
        create: impl FnOnce() -> io::Result<(Uuid, PathBuf)>,
    ) -> io::Result<(Uuid, PathBuf)> {
        self.create_pending_with_mode(homes, provider, started_at, create, true, |registry| {
            reservation
                .verify_registry(registry)
                .map_err(io::Error::other)
        })
    }

    fn create_pending_with_mode(
        &self,
        homes: &Homes,
        provider: Provider,
        started_at: u64,
        create: impl FnOnce() -> io::Result<(Uuid, PathBuf)>,
        nonblocking: bool,
        validate: impl FnOnce(&Registry) -> io::Result<()>,
    ) -> io::Result<(Uuid, PathBuf)> {
        let _lifecycle = if nonblocking {
            homes.try_lifecycle_lock()?
        } else {
            homes.lifecycle_lock()?
        };
        let _lock = if nonblocking {
            self.try_lock()?
        } else {
            self.lock()?
        };
        let mut registry = read(&self.path())?;
        if registry.is_newer() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the accounts were saved by a newer cagents",
            ));
        }
        validate(&registry)?;
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
            return Err(io::Error::other(format!(
                "{error}; the new account home was retained at {} for recovery",
                home.display()
            )));
        }
        Ok((key, home))
    }

    // Legacy destructive fixture support only. Product lifecycle operations
    // require daemon admission and preserve managed/abandoned home files.
    // Registry absence or an old parked name grants no deletion authority.
    #[cfg(test)]
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
        self.register_pending_with_mode(homes, account, system, false, |_| Ok(()))
    }

    #[cfg(feature = "host")]
    pub(crate) fn try_register_pending(
        &self,
        homes: &Homes,
        account: Registered,
        system: &[(Provider, Identity)],
    ) -> io::Result<(Registry, Result<(), AccountId>)> {
        self.register_pending_with_mode(homes, account, system, true, |_| Ok(()))
    }

    #[cfg(feature = "host")]
    pub(crate) fn try_register_pending_reserved(
        &self,
        reservation: &crate::host::AccountReservation,
        homes: &Homes,
        account: Registered,
        system: &[(Provider, Identity)],
    ) -> io::Result<(Registry, Result<(), AccountId>)> {
        self.register_pending_with_mode(homes, account, system, true, |registry| {
            reservation
                .verify_registry(registry)
                .map_err(io::Error::other)
        })
    }

    fn register_pending_with_mode(
        &self,
        homes: &Homes,
        account: Registered,
        system: &[(Provider, Identity)],
        nonblocking: bool,
        validate: impl FnOnce(&Registry) -> io::Result<()>,
    ) -> io::Result<(Registry, Result<(), AccountId>)> {
        let _lifecycle = if nonblocking {
            homes.try_lifecycle_lock()?
        } else {
            homes.lifecycle_lock()?
        };
        self.update_checked_with_mode(
            |registry| {
                validate(registry)?;
                if registry
                    .pending
                    .iter()
                    .any(|pending| pending.key == account.key && pending.is_import())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Use the imported cache's explicit expected-identity verification",
                    ));
                }
                if registry.find(account.key).is_some_and(|existing| {
                    existing.provider == account.provider && existing.linked_home.is_none()
                }) {
                    return Ok(Ok(()));
                }
                if !registry.pending.iter().any(|pending| {
                    pending.key == account.key && pending.provider == account.provider
                }) {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "The pending sign-in was removed before it completed",
                    ));
                }
                Ok(registry.register(account, system))
            },
            nonblocking,
        )
    }

    fn path(&self) -> PathBuf {
        self.directory.join(FILE_NAME)
    }

    fn lock(&self) -> io::Result<crate::instance::OwnedLock> {
        crate::secure_file::prepare_private_directory(&self.directory)?;
        crate::instance::owned_lock(&self.directory.join(LOCK_NAME), libc::LOCK_EX)
    }

    #[cfg(all(test, feature = "host"))]
    pub(crate) fn hold_registry_lock_for_account_test(
        &self,
    ) -> io::Result<crate::instance::OwnedLock> {
        self.lock()
    }

    fn try_lock(&self) -> io::Result<crate::instance::OwnedLock> {
        crate::secure_file::prepare_private_directory(&self.directory)?;
        crate::instance::owned_lock(
            &self.directory.join(LOCK_NAME),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    }

    #[cfg(feature = "host")]
    pub(crate) fn try_load(&self) -> io::Result<Registry> {
        let _lock = self.try_lock()?;
        read(&self.path())
    }

    #[cfg(feature = "host")]
    pub(crate) fn try_update<T>(
        &self,
        change: impl FnOnce(&mut Registry) -> T,
    ) -> io::Result<(Registry, T)> {
        self.update_checked_with_mode(|registry| Ok(change(registry)), true)
    }

    #[cfg(feature = "host")]
    pub(crate) fn try_update_reserved<T>(
        &self,
        reservation: &crate::host::AccountReservation,
        change: impl FnOnce(&mut Registry) -> T,
    ) -> io::Result<(Registry, T)> {
        self.update_checked_with_mode(
            |registry| {
                reservation
                    .verify_registry(registry)
                    .map_err(io::Error::other)?;
                Ok(change(registry))
            },
            true,
        )
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
        self.update_checked_with_mode(change, false)
    }

    fn update_checked_with_mode<T>(
        &self,
        change: impl FnOnce(&mut Registry) -> io::Result<T>,
        nonblocking: bool,
    ) -> io::Result<(Registry, T)> {
        let _lock = if nonblocking {
            self.try_lock()?
        } else {
            self.lock()?
        };
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

    /// Discovery grants no authority to rename or delete orphan/parked homes.
    /// Explicit cleanup requires the separate complete deletion permit cut.
    pub fn load_and_sweep(&self, _homes: &Homes) -> io::Result<Registry> {
        self.load()
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
    if document.len() as u64 > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the accounts update is too large; the existing registry was preserved",
        ));
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
    fn oversized_update_preserves_the_readable_registry() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path());
        let (before, ()) = store
            .update(|registry| {
                registry.set_default(&Account::system(Provider::Claude));
            })
            .unwrap();
        let bytes = fs::read(store.path()).unwrap();
        let result = store.update(|registry| {
            for index in 0..5000 {
                registry.set_project_default(
                    &PathBuf::from(format!("/projects/repository-{index}/.git")),
                    &Account::system(Provider::Codex),
                );
            }
        });
        assert!(
            result.is_err(),
            "an update cannot write a registry this build rejects"
        );
        assert_eq!(fs::read(store.path()).unwrap(), bytes);
        assert_eq!(store.load().unwrap(), before);
        store
            .update(|registry| registry.set_automatic(Provider::Claude))
            .unwrap();
        assert!(store.load().unwrap().automatic(Provider::Claude));
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
    fn discovery_preserves_pending_registered_linked_recent_and_unproved_orphan_homes() {
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
        for index in [0, 1, 2, 3, 4] {
            assert!(homes_created[index].1.is_dir());
        }
        assert!(
            homes_created[3].1.is_dir(),
            "orphan status grants no deletion proof"
        );
        let recipe = account_recipe(&directory.path().join("unmanaged"));
        assert!(
            store
                .authorize_launch(&homes, &recipe, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn discovery_retains_interrupted_removal_and_recreated_home_without_permit() {
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
        // A supported registry still provides no actor/reader retirement proof.
        store.load().unwrap();
        write(&store.directory, &store.path(), &Registry::default()).unwrap();
        store.load_and_sweep(&homes).unwrap();
        assert_eq!(fs::read(home.join("recreated")).unwrap(), b"fixture");
        assert_eq!(fs::read(parked.join("old-history")).unwrap(), b"fixture");
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
            Ok(key)
        );
        registry.unregister(key).unwrap();
        assert!(history.is_file());
    }

    #[test]
    fn repeating_a_link_recovers_its_key_without_rewriting_registry_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("selected");
        let other = directory.path().join("other");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&other).unwrap();
        let home = fs::canonicalize(home).unwrap();
        let other = fs::canonicalize(other).unwrap();
        let store = Store::at(directory.path().join("config"));
        let homes = Homes::at(directory.path().join("managed"));
        let key = Uuid::new_v4();
        let account = Account::linked(Provider::Codex, key, home.clone());
        let (before, ()) = store
            .update(|registry| {
                assert_eq!(
                    registry.link(Provider::Codex, key, home.clone(), None, Some(&homes), 7),
                    Ok(key)
                );
                registry.set_label(&account, Some("Selected home".into()));
                registry.observe_identity(
                    key,
                    &Identity {
                        email: Some("selected@example.test".into()),
                        organization: Some("Fixture organization".into()),
                        plan: None,
                    },
                );
                registry.set_default(&account);
                registry.set_project_default(directory.path(), &account);
                registry.begin(Provider::Claude, Uuid::new_v4(), 8);
            })
            .unwrap();
        let bytes = fs::read(store.path()).unwrap();
        // The first response was lost: repeat explicitly with a fresh request
        // key, as the real Link callers do. No transport/provider is exercised.
        let requested = Uuid::new_v4();
        let (after, result) = store
            .update(|registry| {
                registry.link(
                    Provider::Codex,
                    requested,
                    home.clone(),
                    None,
                    Some(&homes),
                    99,
                )
            })
            .unwrap();
        assert_eq!(result, Ok(key));
        assert_eq!(after, before);
        assert_eq!(fs::read(store.path()).unwrap(), bytes);
        assert_eq!(after.revision(), before.revision());
        assert_eq!(after.registered().len(), 1);
        assert!(after.find(requested).is_none());
        assert_eq!(after.find(key).unwrap().added_at, 7);
        assert_eq!(after.label(&account), Some("Selected home"));
        assert_eq!(
            after.default_account(Provider::Codex, Some(&homes)),
            account
        );
        assert_eq!(
            after.project_default_account(directory.path(), Provider::Codex, Some(&homes)),
            account
        );

        // Identity alone cannot merge a different home. It remains a separate
        // registration with the independently requested key.
        let other_key = Uuid::new_v4();
        let (after, result) = store
            .update(|registry| {
                let result = registry.link(
                    Provider::Codex,
                    other_key,
                    other.clone(),
                    None,
                    Some(&homes),
                    100,
                );
                registry.observe_identity(other_key, &before.find(key).unwrap().identity());
                result
            })
            .unwrap();
        assert_eq!(result, Ok(other_key));
        assert_eq!(after.registered().len(), 2);
        assert_eq!(after.find(key), before.find(key));
        assert_eq!(
            after.default_account(Provider::Codex, Some(&homes)),
            account
        );
    }

    #[test]
    fn link_recovery_refuses_conflicting_home_kinds_and_ambiguous_registrations() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("selected");
        fs::create_dir(&home).unwrap();
        let home = fs::canonicalize(home).unwrap();
        let key = Uuid::new_v4();
        let mut registry = Registry::default();
        registry
            .link(Provider::Codex, key, home.clone(), None, None, 1)
            .unwrap();
        let original = registry.clone();
        let listed: Result<Uuid, String> = Err("That account home is already listed".into());
        assert_eq!(
            registry.link(
                Provider::Codex,
                Uuid::new_v4(),
                home.clone(),
                Some(&home),
                None,
                2
            ),
            listed
        );
        assert_eq!(
            registry, original,
            "System conflict wins over linked recovery"
        );

        // A stored linked path can alias a managed home. Retain the managed
        // registration's existing refusal even when the linked row is first.
        let homes = Homes::at(directory.path().join("managed"));
        let managed_key = Uuid::new_v4();
        let managed_home = homes.path(Provider::Codex, managed_key);
        fs::create_dir_all(&managed_home).unwrap();
        registry.accounts[0].linked_home = Some(managed_home.clone());
        registry.accounts.push(Registered {
            key: managed_key,
            provider: Provider::Codex,
            linked_home: None,
            label: None,
            email: None,
            organization: None,
            added_at: 2,
        });
        let before = registry.clone();
        assert_eq!(
            registry.link(
                Provider::Codex,
                Uuid::new_v4(),
                managed_home,
                None,
                Some(&homes),
                3
            ),
            listed
        );
        assert_eq!(registry, before);

        // Multiple linked registrations (including an archived collision)
        // must not choose the first row as the recovered identity.
        for archived in [false, true] {
            registry = original.clone();
            let duplicate_key = Uuid::new_v4();
            let mut duplicate = registry.accounts[0].clone();
            duplicate.key = duplicate_key;
            registry.accounts.push(duplicate);
            if archived {
                registry.archived.push(duplicate_key);
            }
            let before = registry.clone();
            assert_eq!(
                registry.link(Provider::Codex, Uuid::new_v4(), home.clone(), None, None, 4),
                listed
            );
            assert_eq!(registry, before);
        }

        registry = original.clone();
        registry.archived.push(key);
        let before = registry.clone();
        assert_eq!(
            registry.link(Provider::Codex, Uuid::new_v4(), home.clone(), None, None, 5),
            Ok(key)
        );
        let mut restored = before;
        restored.archived.clear();
        assert_eq!(
            registry, restored,
            "archive restoration keeps all row metadata"
        );
        let other_provider_key = Uuid::new_v4();
        assert_eq!(
            registry.link(Provider::Claude, other_provider_key, home, None, None, 6),
            Ok(other_provider_key),
            "path uniqueness remains scoped to the chosen provider"
        );
    }

    #[test]
    fn a_new_managed_login_cannot_duplicate_a_linked_identity() {
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
        assert_eq!(
            registry.register(managed(Provider::Codex, "same@example.com"), &[]),
            Err(Account::linked(Provider::Codex, linked, home).id())
        );
        assert_eq!(registry.registered().len(), 1);
    }

    #[test]
    fn archiving_preserves_home_launches_and_defaults_and_can_be_restored() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("config"));
        let homes = Homes::at(directory.path().join("accounts"));
        let (key, home) = homes.create(Provider::Claude, None).unwrap();
        let history = home.join("history.jsonl");
        fs::write(&history, "saved conversation").unwrap();
        fs::File::open(&home)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        let duplicate = Account::managed(Provider::Claude, key, home.clone());
        let retained = Account::system(Provider::Claude);
        let repository = directory.path().join("project");
        store
            .update(|registry| {
                let mut registered = managed(Provider::Claude, "same@example.com");
                registered.key = key;
                registry.register(registered, &[]).unwrap();
                registry.set_default(&duplicate);
                registry.set_project_default(&repository, &duplicate);
                assert!(registry.archive_duplicate(key, &retained));
            })
            .unwrap();
        let mut registry = store.load_and_sweep(&homes).unwrap();
        assert!(registry.is_archived(key));
        assert!(registry.find(key).is_some());
        assert!(registry.contains_managed_home(&homes, Provider::Claude, key));
        assert!(!registry.accounts(Some(&homes)).contains(&duplicate));
        assert!(registry.all_accounts(Some(&homes)).contains(&duplicate));
        assert_eq!(
            registry.default_account(Provider::Claude, Some(&homes)),
            retained
        );
        assert_eq!(
            registry.project_default_account(&repository, Provider::Claude, Some(&homes)),
            retained
        );
        registry.set_default(&duplicate);
        registry.set_project_default(&repository, &duplicate);
        assert!(registry.is_default(&retained));
        assert_eq!(fs::read_to_string(&history).unwrap(), "saved conversation");
        let environment = [(
            "CLAUDE_CONFIG_DIR".into(),
            home.to_string_lossy().into_owned(),
        )]
        .into();
        assert!(
            store
                .authorize_environment(&homes, &environment, Some(Provider::Claude))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            registry.link(
                Provider::Claude,
                Uuid::new_v4(),
                home.clone(),
                Some(&home),
                Some(&homes),
                2
            ),
            Err("That account home is already listed".into())
        );
        assert!(registry.is_archived(key));
        assert_eq!(
            registry.link(
                Provider::Claude,
                Uuid::new_v4(),
                home,
                None,
                Some(&homes),
                2
            ),
            Ok(key)
        );
        assert!(!registry.is_archived(key));
        assert!(registry.accounts(Some(&homes)).contains(&duplicate));
        assert_eq!(registry.registered().len(), 1);
    }

    #[test]
    fn archiving_requires_a_distinct_active_account_of_the_same_provider() {
        let mut registry = Registry::default();
        let registered = managed(Provider::Claude, "me@example.com");
        let key = registered.key;
        registry.register(registered, &[]).unwrap();
        assert!(!registry.archive_duplicate(key, &Account::system(Provider::Codex)));
        assert!(!registry.archive_duplicate(
            key,
            &Account::managed(Provider::Claude, key, PathBuf::new())
        ));
        assert!(!registry.archive_duplicate(
            key,
            &Account::managed(Provider::Claude, Uuid::new_v4(), PathBuf::new())
        ));
        assert!(registry.archive_duplicate(key, &Account::system(Provider::Claude)));
        assert!(!registry.archive_duplicate(key, &Account::system(Provider::Claude)));
    }

    #[test]
    fn a_home_reporting_another_login_keeps_the_login_it_was_added_with() {
        let reported = |email: &str, organization: Option<&str>| Identity {
            email: Some(email.into()),
            organization: organization.map(str::to_owned),
            plan: None,
        };
        let mut registry = Registry::default();
        let added = managed(Provider::Claude, "work@example.com");
        let key = added.key;
        registry.register(added, &[]).unwrap();

        // What the registry lacked is learned; the same login is no change.
        assert_eq!(
            registry.observe_identity(key, &reported("work@example.com", Some("Acme"))),
            Observed::Recorded
        );
        assert_eq!(
            registry.observe_identity(key, &reported(" WORK@example.com", None)),
            Observed::Unchanged
        );
        // Another login - or the same address in another organization - is
        // a home whose credentials were replaced, never a quiet rename.
        for other in [
            reported("personal@example.com", Some("Acme")),
            reported("work@example.com", Some("Personal")),
        ] {
            assert!(registry.find(key).unwrap().conflicts_with(&other));
            assert_eq!(registry.observe_identity(key, &other), Observed::Conflict);
            assert_eq!(
                registry.find(key).unwrap().identity(),
                reported("work@example.com", Some("Acme"))
            );
        }
        // A home that names nobody says nothing about who it is.
        assert!(
            !registry
                .find(key)
                .unwrap()
                .conflicts_with(&Identity::default())
        );

        // Signing that account in again is the reader's choice of login.
        assert!(registry.adopt_identity(key, &reported("personal@example.com", None)));
        assert_eq!(
            registry.find(key).unwrap().identity(),
            reported("personal@example.com", None)
        );
        assert!(!registry.adopt_identity(key, &reported("personal@example.com", None)));
        assert!(!registry.adopt_identity(key, &Identity::default()));
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

#[cfg(feature = "host")]
pub(crate) use cache_import::ImportLaunchLease;

#[cfg(feature = "host")]
mod cache_import {
    use super::*;
    use crate::{account::import::PhysicalIdentity, fs_safety::Root};
    use std::{ffi::OsStr, os::unix::fs::MetadataExt};

    /// Held only across the actual identity-probe spawn, never across its I/O.
    /// Both locks come from the real Store; metadata DTOs cannot mint this lease.
    pub(crate) struct ImportLaunchLease {
        _lifecycle: crate::instance::OwnedLock,
        _registry: crate::instance::OwnedLock,
    }

    fn refuse(message: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, message)
    }

    fn current_home(
        homes: &Homes,
        operation: Uuid,
        expected: PhysicalIdentity,
    ) -> io::Result<Root> {
        let path = homes.path(Provider::Codex, operation);
        crate::secure_file::reject_symlink_components(&path)
            .map_err(|_| refuse("The imported account home changed"))?;
        let root =
            Root::open(&path).map_err(|_| refuse("The imported account home is unavailable"))?;
        let metadata = root
            .metadata()
            .map_err(|_| refuse("The imported account home is unavailable"))?;
        if metadata.dev() != expected.device
            || metadata.ino() != expected.inode
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() == 0
        {
            return Err(refuse("The imported account home changed"));
        }
        Ok(root)
    }

    /// Resolve missing suffixes without treating a dangling link as absence.
    /// Existing roots come from the bounded Registry/System metadata census.
    fn potential_home(path: &Path) -> io::Result<PathBuf> {
        if !path.is_absolute()
            || path.as_os_str().as_bytes().len() > 16 * 1024
            || path.components().count() > 128
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(refuse("Account home independence could not be confirmed"));
        }
        let mut ancestor = path;
        let mut suffix = Vec::new();
        loop {
            match ancestor.canonicalize() {
                Ok(mut base) => {
                    for part in suffix.iter().rev() {
                        base.push(part);
                    }
                    return Ok(base);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    match fs::symlink_metadata(ancestor) {
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        _ => {
                            return Err(refuse("Account home independence could not be confirmed"));
                        }
                    }
                    let name = ancestor.file_name().ok_or_else(|| {
                        refuse("Account home independence could not be confirmed")
                    })?;
                    suffix.push(name.to_os_string());
                    ancestor = ancestor.parent().ok_or_else(|| {
                        refuse("Account home independence could not be confirmed")
                    })?;
                }
                Err(_) => return Err(refuse("Account home independence could not be confirmed")),
            }
        }
    }

    fn independent_home(registry: &Registry, homes: &Homes, operation: Uuid) -> io::Result<()> {
        if registry.accounts.len() + registry.pending.len() > 128 {
            return Err(refuse(
                "Too many account homes to confirm import independence",
            ));
        }
        let destination = potential_home(&homes.path(Provider::Codex, operation))?;
        let known = registry
            .accounts
            .iter()
            .filter(|row| row.key != operation)
            .map(|row| {
                row.linked_home
                    .clone()
                    .unwrap_or_else(|| homes.path(row.provider, row.key))
            })
            .chain(
                registry
                    .pending
                    .iter()
                    .filter(|row| row.key != operation)
                    .map(|row| homes.path(row.provider, row.key)),
            )
            .chain(
                Account::system_accounts()
                    .into_iter()
                    .filter_map(|account| account.resolved_home()),
            );
        for operand in known {
            let other = potential_home(&operand)?;
            if destination.starts_with(&other) || other.starts_with(&destination) {
                return Err(refuse("Another account refers to the import destination"));
            }
        }
        Ok(())
    }

    fn require_pending(registry: &Registry, operation: Uuid) -> io::Result<()> {
        if registry.is_newer() || !registry.is_import_pending(operation) {
            return Err(refuse("The original import-owned pending account changed"));
        }
        if registry.import_key_referenced(operation) {
            return Err(refuse(
                "The import destination was selected as a default before registration",
            ));
        }
        Ok(())
    }

    fn require_verifiable(
        registry: &Registry,
        operation: Uuid,
        expected_email: &str,
    ) -> io::Result<()> {
        if registry.is_newer()
            || operation.is_nil()
            || !super::super::import::expected_email_is_valid(expected_email)
        {
            return Err(refuse(
                "The original import verification identity is invalid",
            ));
        }
        if registry.accounts.iter().any(|row| row.key == operation) {
            let matches: Vec<_> = registry
                .accounts
                .iter()
                .filter(|row| row.key == operation)
                .collect();
            if matches.len() != 1
                || registry.pending.iter().any(|row| row.key == operation)
                || registry.is_archived(operation)
                || matches[0].provider != Provider::Codex
                || matches[0].linked_home.is_some()
                || !matches[0]
                    .email
                    .as_deref()
                    .is_some_and(|email| email.eq_ignore_ascii_case(expected_email))
            {
                return Err(refuse("The original imported registration changed"));
            }
            return Ok(());
        }
        require_pending(registry, operation)
    }

    fn registry_directory(store: &Store) -> io::Result<Root> {
        let root = Root::open(&store.directory)
            .map_err(|_| refuse("The import registry directory is unavailable"))?;
        let metadata = root
            .metadata()
            .map_err(|_| refuse("The import registry directory is unavailable"))?;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() == 0
        {
            return Err(refuse("The import registry directory is not private"));
        }
        Ok(root)
    }

    fn sync_registry(store: &Store, root: &Root) -> io::Result<()> {
        let uncertain = || {
            refuse(
                "The import registry acknowledgement is uncertain; inspect the original operation",
            )
        };
        let original = root.metadata().map_err(|_| uncertain())?;
        let same = || -> io::Result<bool> {
            let named = registry_directory(store)?.metadata()?;
            Ok((named.dev(), named.ino()) == (original.dev(), original.ino()))
        };
        if !same().map_err(|_| uncertain())? {
            return Err(uncertain());
        }
        crate::fs_safety::sync_directory(root).map_err(|_| uncertain())?;
        if !same().map_err(|_| uncertain())? {
            return Err(uncertain());
        }
        Ok(())
    }

    fn check_command(
        command: &std::process::Command,
        homes: &Homes,
        operation: Uuid,
    ) -> io::Result<()> {
        if !Path::new(command.get_program()).is_absolute()
            || command.get_args().collect::<Vec<_>>() != [OsStr::new("app-server")]
        {
            return Err(refuse(
                "Only the selected official identity-probe command is admitted",
            ));
        }
        let expected = homes.path(Provider::Codex, operation);
        let env: Vec<_> = command.get_envs().collect();
        let selected = env
            .iter()
            .find(|(name, _)| *name == OsStr::new("CODEX_HOME"))
            .and_then(|(_, value)| *value);
        if selected.map(Path::new) != Some(expected.as_path()) {
            return Err(refuse(
                "The identity probe must use the original imported account home",
            ));
        }
        for name in super::super::credential_overrides(Provider::Codex) {
            if !env
                .iter()
                .any(|(key, value)| *key == OsStr::new(name) && value.is_none())
            {
                return Err(refuse(
                    "The identity probe must remove inherited provider credentials",
                ));
            }
        }
        if env
            .iter()
            .any(|(name, value)| *name == OsStr::new("CLAUDE_CONFIG_DIR") && value.is_some())
        {
            return Err(refuse(
                "The identity probe cannot select another provider home",
            ));
        }
        Ok(())
    }

    impl Store {
        /// Read-only atomic-rename snapshot for Status, never an admission permit.
        /// Missing metadata is empty; unsafe metadata is refused, not repaired.
        pub(crate) fn read_import_metadata(&self) -> io::Result<Registry> {
            let mut file =
                match crate::secure_file::open_existing_private_file_readonly(&self.path()) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Ok(Registry::default());
                    }
                    Err(_) => return Err(refuse("The import registry metadata is unavailable")),
                };
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| refuse("The import registry metadata is unavailable"))?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(refuse("The import registry metadata is too large"));
            }
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| refuse("The import registry metadata is invalid"))?;
            toml::from_str(text).map_err(|_| refuse("The import registry metadata is invalid"))
        }

        /// The create callback must use the reservation's actual fresh-home
        /// initializer. It may persist the journal's prepared inode receipt,
        /// but must never reenter Store. No credential link precedes this ACK.
        pub(crate) fn try_create_import_pending_reserved(
            &self,
            reservation: &crate::host::AccountReservation,
            homes: &Homes,
            operation: Uuid,
            started_at: u64,
            create: impl FnOnce() -> io::Result<(Uuid, PathBuf)>,
        ) -> io::Result<(Uuid, PathBuf)> {
            self.create_import_pending(homes, operation, started_at, create, |registry| {
                reservation
                    .verify_registry(registry)
                    .map_err(io::Error::other)
            })
        }

        fn create_import_pending(
            &self,
            homes: &Homes,
            operation: Uuid,
            started_at: u64,
            create: impl FnOnce() -> io::Result<(Uuid, PathBuf)>,
            validate: impl FnOnce(&Registry) -> io::Result<()>,
        ) -> io::Result<(Uuid, PathBuf)> {
            let _lifecycle = homes.try_lifecycle_lock()?;
            let _lock = self.try_lock()?;
            let registry_root = registry_directory(self)?;
            let mut registry = read(&self.path())?;
            if registry.is_newer() {
                return Err(refuse("The account registry requires a newer cagents"));
            }
            validate(&registry)?;
            registry.require_fresh_import_key(operation)?;
            independent_home(&registry, homes, operation)?;
            let revision = registry
                .revision
                .checked_add(1)
                .ok_or_else(|| refuse("Account registry revision exhausted"))?;
            let (key, home) = create()?;
            if key != operation || home != homes.path(Provider::Codex, operation) {
                return Err(refuse(
                    "The created home does not match the original import operation",
                ));
            }
            // Recheck metadata aliases after mkdir; no named credential exists yet.
            independent_home(&registry, homes, operation)?;
            registry.pending.push(Pending {
                key: operation,
                provider: Provider::Codex,
                started_at,
                origin: PendingOrigin::CacheImport { operation },
            });
            registry.version = VERSION;
            registry.revision = revision;
            write(&self.directory, &self.path(), &registry)?;
            sync_registry(self, &registry_root)?;
            Ok((key, home))
        }

        /// Exact pending/physical admission at a short file publication boundary.
        /// The effect may link/fsync one already-prepared auth inode, never Store.
        pub(crate) fn try_with_import_pending_reserved<T>(
            &self,
            reservation: &crate::host::AccountReservation,
            homes: &Homes,
            operation: Uuid,
            expected_home: PhysicalIdentity,
            effect: impl FnOnce() -> io::Result<T>,
        ) -> io::Result<T> {
            let _lifecycle = homes.try_lifecycle_lock()?;
            let _lock = self.try_lock()?;
            let registry = read(&self.path())?;
            reservation
                .verify_import_home(operation, expected_home)
                .map_err(io::Error::other)?;
            require_pending(&registry, operation)?;
            independent_home(&registry, homes, operation)?;
            let _home = current_home(homes, operation, expected_home)?;
            effect()
        }

        /// Dedicated verification spawn only. The caller's SpawnOwner retains
        /// process/cleanup custody; dropping an async waiter is not admission.
        pub(crate) fn authorize_import_command_reserved(
            &self,
            reservation: &crate::host::AccountReservation,
            homes: &Homes,
            operation: Uuid,
            expected_home: PhysicalIdentity,
            expected_email: &str,
            command: &std::process::Command,
        ) -> io::Result<ImportLaunchLease> {
            self.import_launch(
                homes,
                operation,
                expected_home,
                expected_email,
                command,
                |_registry| {
                    reservation
                        .verify_import_home(operation, expected_home)
                        .map_err(io::Error::other)?;
                    Ok(())
                },
            )
        }

        fn import_launch(
            &self,
            homes: &Homes,
            operation: Uuid,
            expected_home: PhysicalIdentity,
            expected_email: &str,
            command: &std::process::Command,
            validate: impl FnOnce(&Registry) -> io::Result<()>,
        ) -> io::Result<ImportLaunchLease> {
            let lifecycle = homes.try_lifecycle_lock()?;
            let lock = self.try_lock()?;
            let registry = read(&self.path())?;
            validate(&registry)?;
            require_verifiable(&registry, operation, expected_email)?;
            independent_home(&registry, homes, operation)?;
            let _home = current_home(homes, operation, expected_home)?;
            check_command(command, homes, operation)?;
            Ok(ImportLaunchLease {
                _lifecycle: lifecycle,
                _registry: lock,
            })
        }

        /// A matching fresh provider result is required by the host before this
        /// call. Repeat the expected-email/key/home checks under actual leases;
        /// this method never probes System or selects a duplicate/default.
        pub(crate) fn try_register_import_pending_reserved(
            &self,
            reservation: &crate::host::AccountReservation,
            homes: &Homes,
            operation: Uuid,
            expected_home: PhysicalIdentity,
            expected_email: &str,
            account: Registered,
            known_system: &[(Provider, Identity)],
        ) -> io::Result<(Registry, Result<(), AccountId>)> {
            self.register_import_pending(
                homes,
                operation,
                expected_home,
                expected_email,
                account,
                known_system,
                |registry| {
                    reservation
                        .verify_import_home(operation, expected_home)
                        .map_err(io::Error::other)?;
                    if registry.find(operation).is_none() {
                        require_pending(registry, operation)?;
                    }
                    Ok(())
                },
            )
        }

        fn register_import_pending(
            &self,
            homes: &Homes,
            operation: Uuid,
            expected_home: PhysicalIdentity,
            expected_email: &str,
            account: Registered,
            known_system: &[(Provider, Identity)],
            validate: impl FnOnce(&Registry) -> io::Result<()>,
        ) -> io::Result<(Registry, Result<(), AccountId>)> {
            if operation.is_nil()
                || account.key != operation
                || account.provider != Provider::Codex
                || account.linked_home.is_some()
                || !super::super::import::expected_email_is_valid(expected_email)
                || !account
                    .email
                    .as_deref()
                    .is_some_and(|email| email.eq_ignore_ascii_case(expected_email))
            {
                return Err(refuse(
                    "The verified ChatGPT identity does not match the original import",
                ));
            }
            let _lifecycle = homes.try_lifecycle_lock()?;
            let registry_root = registry_directory(self)?;
            let result = self.update_checked_with_mode(
                |registry| {
                    validate(registry)?;
                    let _home = current_home(homes, operation, expected_home)?;
                    independent_home(registry, homes, operation)?;
                    if let Some(existing) = registry.find(operation) {
                        if registry
                            .accounts
                            .iter()
                            .filter(|row| row.key == operation)
                            .count()
                            != 1
                            || registry
                                .pending
                                .iter()
                                .any(|pending| pending.key == operation)
                            || existing.provider != Provider::Codex
                            || existing.linked_home.is_some()
                            || !existing
                                .email
                                .as_deref()
                                .is_some_and(|email| email.eq_ignore_ascii_case(expected_email))
                        {
                            return Err(refuse("The original imported registration changed"));
                        }
                        return Ok(Ok(()));
                    }
                    require_pending(registry, operation)?;
                    Ok(registry.register(account, known_system))
                },
                true,
            )?;
            sync_registry(self, &registry_root)?;
            Ok(result)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn fixture() -> (tempfile::TempDir, Homes, Store) {
            let root = tempfile::tempdir().unwrap();
            let homes = Homes::at(root.path().join("homes"));
            let store = Store::at(root.path().join("registry"));
            (root, homes, store)
        }

        fn prepare(store: &Store, homes: &Homes, key: Uuid) -> PathBuf {
            store
                .create_import_pending(
                    homes,
                    key,
                    1,
                    || {
                        homes.create_planned_with_config_initialized(
                            Provider::Codex,
                            key,
                            None,
                            super::super::super::home::CodexConfig::Shared,
                            |_| Ok(()),
                        )
                    },
                    |_| Ok(()),
                )
                .unwrap()
                .1
        }

        fn stamp(home: &Path) -> PhysicalIdentity {
            let metadata = fs::metadata(home).unwrap();
            PhysicalIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        }

        fn account(key: Uuid, email: &str) -> Registered {
            Registered {
                key,
                provider: Provider::Codex,
                linked_home: None,
                label: None,
                email: Some(email.into()),
                organization: None,
                added_at: 1,
            }
        }

        fn command(home: &Path) -> std::process::Command {
            let mut command = std::process::Command::new("/never-executed");
            command.arg("app-server");
            command.env("CODEX_HOME", home);
            for name in super::super::super::credential_overrides(Provider::Codex) {
                command.env_remove(name);
            }
            command
        }

        #[test]
        fn status_metadata_does_not_create_storage_or_repair_existing_permissions() {
            let (root, _homes, store) = fixture();
            assert_eq!(store.read_import_metadata().unwrap(), Registry::default());
            assert!(!store.directory.exists());
            fs::create_dir(&store.directory).unwrap();
            fs::write(store.path(), "version = 6\n").unwrap();
            fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644)).unwrap();
            assert!(store.read_import_metadata().is_err());
            assert_eq!(fs::metadata(store.path()).unwrap().mode() & 0o777, 0o644);
            assert!(!store.directory.join(LOCK_NAME).exists());
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        }

        #[test]
        fn old_pending_defaults_to_sign_in_but_import_publishes_format_six_without_seeding() {
            let old: Registry = toml::from_str("version = 5\n[[pending]]\nkey = '00000000-0000-4000-8000-000000000001'\nprovider = 'codex'\nstarted_at = 1\n").unwrap();
            assert!(!old.pending[0].is_import());
            let (_root, homes, store) = fixture();
            let key = Uuid::from_u128(2);
            let home = prepare(&store, &homes, key);
            let registry = store.load().unwrap();
            assert!(registry.is_import_pending(key));
            assert_eq!(registry.version, 6);
            assert!(
                registry.version > 5,
                "the old managed-home reader must refuse its future version"
            );
            assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
            assert_eq!(
                fs::metadata(&home).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(store.load().unwrap(), registry);
        }

        #[test]
        fn default_references_and_existing_keys_refuse_before_home_creation_without_changing_bytes()
        {
            for mode in 0..6 {
                let (_root, homes, store) = fixture();
                let key = Uuid::parse_str("00000000-0000-4000-8000-000000abcdef").unwrap();
                store
                    .update(|registry| match mode {
                        0 => {
                            registry
                                .defaults
                                .insert("codex".into(), key.to_string().to_uppercase());
                        }
                        1 => {
                            registry.project_defaults.insert(
                                "project".into(),
                                BTreeMap::from([("codex".into(), key.to_string())]),
                            );
                        }
                        2 => registry.accounts.push(account(key, "fixture@example.test")),
                        3 => registry.begin(Provider::Codex, key, 1),
                        4 => registry.archived.push(key),
                        _ => {}
                    })
                    .unwrap();
                let before = fs::read(store.path()).unwrap_or_default();
                let mut called = false;
                let chosen = if mode == 5 { Uuid::nil() } else { key };
                assert!(
                    store
                        .create_import_pending(
                            &homes,
                            chosen,
                            2,
                            || {
                                called = true;
                                unreachable!()
                            },
                            |_| Ok(())
                        )
                        .is_err()
                );
                assert!(!called);
                assert_eq!(fs::read(store.path()).unwrap_or_default(), before);
                assert!(!homes.path(Provider::Codex, key).exists());
            }
        }

        #[test]
        fn generic_launch_and_registration_refuse_import_but_the_exact_probe_lease_is_scoped() {
            let (_root, homes, store) = fixture();
            let key = Uuid::from_u128(4);
            let home = prepare(&store, &homes, key);
            let mut recipe = crate::model::LaunchRecipe::shell("/".into()).unwrap();
            recipe
                .environment
                .insert("CODEX_HOME".into(), home.to_str().unwrap().into());
            assert!(
                store
                    .authorize_launch(&homes, &recipe, Some(Provider::Codex))
                    .is_err()
            );
            let before = fs::read(store.path()).unwrap();
            assert!(
                store
                    .try_register_pending(&homes, account(key, "fixture@example.test"), &[])
                    .is_err()
            );
            assert_eq!(fs::read(store.path()).unwrap(), before);
            let mut probe = command(&home);
            probe.env("OPENAI_API_KEY", "synthetic-not-a-credential");
            assert!(
                store
                    .import_launch(
                        &homes,
                        key,
                        stamp(&home),
                        "fixture@example.test",
                        &probe,
                        |_| Ok(())
                    )
                    .is_err()
            );
            probe.env_remove("OPENAI_API_KEY");
            probe.env("CODEX_HOME", home.parent().unwrap());
            assert!(
                store
                    .import_launch(
                        &homes,
                        key,
                        stamp(&home),
                        "fixture@example.test",
                        &probe,
                        |_| Ok(())
                    )
                    .is_err()
            );
            probe.env("CODEX_HOME", &home);
            let lease = store
                .import_launch(
                    &homes,
                    key,
                    stamp(&home),
                    "fixture@example.test",
                    &probe,
                    |_| Ok(()),
                )
                .unwrap();
            assert!(
                store.try_lock().is_err(),
                "actual Store lease outlives validation through spawn"
            );
            assert!(homes.try_lifecycle_lock().is_err());
            drop(lease);
            assert!(store.try_lock().is_ok());
            assert_eq!(fs::read(store.path()).unwrap(), before);
        }

        #[test]
        fn wrong_identity_and_duplicate_never_register_or_select_and_same_reply_is_idempotent() {
            let (_root, homes, store) = fixture();
            let key = Uuid::from_u128(5);
            let home = prepare(&store, &homes, key);
            let physical = stamp(&home);
            let before = fs::read(store.path()).unwrap();
            assert!(
                store
                    .register_import_pending(
                        &homes,
                        key,
                        physical,
                        "fixture@example.test",
                        account(key, "wrong@example.test"),
                        &[],
                        |_| Ok(())
                    )
                    .is_err()
            );
            assert_eq!(fs::read(store.path()).unwrap(), before);
            let (registered, result) = store
                .register_import_pending(
                    &homes,
                    key,
                    physical,
                    "fixture@example.test",
                    account(key, "fixture@example.test"),
                    &[],
                    |_| Ok(()),
                )
                .unwrap();
            assert_eq!(result, Ok(()));
            assert!(!registered.is_import_pending(key));
            store
                .update(|registry| {
                    registry.set_label(
                        &Account::managed(Provider::Codex, key, home.clone()),
                        Some("My label".into()),
                    );
                    registry.set_default(&Account::system(Provider::Claude));
                })
                .unwrap();
            let current = store.load().unwrap();
            let bytes = fs::read(store.path()).unwrap();
            let (again, result) = store
                .register_import_pending(
                    &homes,
                    key,
                    physical,
                    "fixture@example.test",
                    account(key, "fixture@example.test"),
                    &[],
                    |_| Ok(()),
                )
                .unwrap();
            assert_eq!(result, Ok(()));
            assert_eq!(again, current);
            assert_eq!(fs::read(store.path()).unwrap(), bytes);
            let other = Uuid::from_u128(6);
            let other_home = prepare(&store, &homes, other);
            let before_duplicate = fs::read(store.path()).unwrap();
            let (_, result) = store
                .register_import_pending(
                    &homes,
                    other,
                    stamp(&other_home),
                    "fixture@example.test",
                    account(other, "fixture@example.test"),
                    &[],
                    |_| Ok(()),
                )
                .unwrap();
            assert!(result.is_err());
            assert!(store.load().unwrap().is_import_pending(other));
            assert_eq!(fs::read(store.path()).unwrap(), before_duplicate);
        }

        #[test]
        fn explicit_verification_can_reconcile_registered_store_with_a_lost_journal_ack() {
            let (_root, homes, store) = fixture();
            let key = Uuid::from_u128(31);
            let home = prepare(&store, &homes, key);
            let physical = stamp(&home);
            let probe = command(&home);
            let (_, result) = store
                .register_import_pending(
                    &homes,
                    key,
                    physical,
                    "fixture@example.test",
                    account(key, "fixture@example.test"),
                    &[],
                    |_| Ok(()),
                )
                .unwrap();
            assert_eq!(result, Ok(()));
            // Simulate only the Store side of an acknowledged registration. The
            // host must still hold the original Published journal record and run
            // a fresh identity probe before marking that journal Registered.
            let registered = store.load().unwrap();
            let bytes = fs::read(store.path()).unwrap();
            let lease = store
                .import_launch(
                    &homes,
                    key,
                    physical,
                    "fixture@example.test",
                    &probe,
                    |_| Ok(()),
                )
                .unwrap();
            assert!(store.try_lock().is_err());
            drop(lease);
            assert_eq!(fs::read(store.path()).unwrap(), bytes);
            let mut wrong_email = registered.clone();
            wrong_email.accounts[0].email = Some("other@example.test".into());
            let mut linked = registered.clone();
            linked.accounts[0].linked_home = Some(home.clone());
            let mut archived = registered.clone();
            archived.archived.push(key);
            let mut pending = registered.clone();
            pending.pending.push(Pending {
                key,
                provider: Provider::Codex,
                started_at: 1,
                origin: PendingOrigin::CacheImport { operation: key },
            });
            let mut wrong_key = registered.clone();
            wrong_key.accounts[0].key = Uuid::from_u128(32);
            let mut ambiguous = registered.clone();
            ambiguous.accounts.push(ambiguous.accounts[0].clone());
            for invalid in [wrong_email, linked, archived, pending, wrong_key, ambiguous] {
                write(&store.directory, &store.path(), &invalid).unwrap();
                let before = fs::read(store.path()).unwrap();
                assert!(
                    store
                        .import_launch(
                            &homes,
                            key,
                            physical,
                            "fixture@example.test",
                            &probe,
                            |_| Ok(())
                        )
                        .is_err()
                );
                assert_eq!(fs::read(store.path()).unwrap(), before);
            }
            write(&store.directory, &store.path(), &registered).unwrap();
            assert!(
                store
                    .import_launch(&homes, key, physical, "wrong@example.test", &probe, |_| Ok(
                        ()
                    ))
                    .is_err()
            );
            let mut wrong_home = physical;
            wrong_home.inode += 1;
            assert!(
                store
                    .import_launch(
                        &homes,
                        key,
                        wrong_home,
                        "fixture@example.test",
                        &probe,
                        |_| Ok(())
                    )
                    .is_err()
            );
            let (_, result) = store
                .register_import_pending(
                    &homes,
                    key,
                    physical,
                    "fixture@example.test",
                    account(key, "fixture@example.test"),
                    &[],
                    |_| Ok(()),
                )
                .unwrap();
            assert_eq!(result, Ok(()));
            assert_eq!(fs::read(store.path()).unwrap(), bytes);
        }

        #[test]
        fn pending_store_failure_retains_empty_home_without_a_durable_acknowledgement() {
            let (root, homes, store) = fixture();
            store
                .update(|registry| registry.set_default(&Account::system(Provider::Claude)))
                .unwrap();
            let before = fs::read(store.path()).unwrap();
            let moved = root.path().join("retained-registry");
            let key = Uuid::from_u128(10);
            let result = store.create_import_pending(
                &homes,
                key,
                1,
                || {
                    let created = homes.create_planned_with_config_initialized(
                        Provider::Codex,
                        key,
                        None,
                        super::super::super::home::CodexConfig::Shared,
                        |_| Ok(()),
                    )?;
                    fs::rename(&store.directory, &moved)?;
                    Ok(created)
                },
                |_| Ok(()),
            );
            assert!(result.is_err());
            assert_eq!(fs::read(moved.join(FILE_NAME)).unwrap(), before);
            assert!(!store.path().exists());
            let home = homes.path(Provider::Codex, key);
            assert!(home.is_dir());
            assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
            assert!(!home.join("auth.json").exists());
        }

        #[test]
        fn dangling_link_to_the_new_home_is_not_activated() {
            use std::os::unix::fs::symlink;
            let (_root, homes, store) = fixture();
            let key = Uuid::from_u128(7);
            let target = homes.path(Provider::Codex, key);
            let alias = homes.path(Provider::Claude, Uuid::from_u128(8));
            fs::create_dir_all(alias.parent().unwrap()).unwrap();
            symlink(&target, &alias).unwrap();
            store
                .update(|registry| {
                    let mut row = account(Uuid::from_u128(9), "other@example.test");
                    row.linked_home = Some(alias);
                    registry.accounts.push(row);
                })
                .unwrap();
            let before = fs::read(store.path()).unwrap();
            let mut created = false;
            assert!(
                store
                    .create_import_pending(
                        &homes,
                        key,
                        1,
                        || {
                            created = true;
                            unreachable!()
                        },
                        |_| Ok(())
                    )
                    .is_err()
            );
            assert!(!created);
            assert!(!target.exists());
            assert_eq!(fs::read(store.path()).unwrap(), before);
        }
    }
}

#[cfg(feature = "host")]
pub(crate) use coding_config_init::CodingConfigRefusal;

/// Only the selected daemon initializes an account file. UI/CLI callers use
/// typed lifecycle RPCs; no shared reservation trait or lease is exported.
#[cfg(feature = "host")]
mod coding_config_init {
    use super::*;
    use crate::{
        account::{
            coding_config::{self, aliases},
            readiness::PhysicalIdentity,
        },
        fs_safety::{self, Root},
    };
    use std::os::unix::fs::MetadataExt;

    const MAX_HOMES: usize = 128;
    const MAX_OBSERVATIONS: usize = 256;
    const MAX_HOME_COMPONENTS: usize = 128;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum CodingConfigRefusal {
        RegistryUnavailable,
        RegistryBusy,
        RegistryNewer,
        InvalidAccount,
        ContextChanged,
        SharedHome,
        SharedConfig,
        CensusUnavailable,
        CensusLimit,
    }
    type Refusal = CodingConfigRefusal;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Stamp {
        device: u64,
        inode: u64,
        uid: u32,
        mode: u32,
    }
    impl Stamp {
        fn directory(root: &Root) -> Result<Self, Refusal> {
            let metadata = root.metadata().map_err(|_| Refusal::CensusUnavailable)?;
            Self::metadata(&metadata)
        }
        fn metadata(metadata: &fs::Metadata) -> Result<Self, Refusal> {
            if !metadata.is_dir() || metadata.nlink() == 0 {
                return Err(Refusal::CensusUnavailable);
            }
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                uid: metadata.uid(),
                mode: metadata.mode(),
            })
        }
        fn is_selected(&self, selected: PhysicalIdentity) -> bool {
            (self.device, self.inode) == (selected.device, selected.inode)
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct HomeKey {
        operand: PathBuf,
        stamp: Stamp,
        /// Empty for an existing home; otherwise the first proved-absent
        /// component and remaining suffix under the retained nearest parent.
        missing: PathBuf,
    }
    struct HomeObservation {
        key: HomeKey,
        root: Root,
        anchor: PathBuf,
    }
    impl HomeObservation {
        fn capture(path: PathBuf) -> Result<Self, Refusal> {
            use std::path::Component;
            if !path.is_absolute()
                || path.as_os_str().as_bytes().len() > 16 * 1024
                || path.components().count() > MAX_HOME_COMPONENTS
                || path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir))
            {
                return Err(Refusal::CensusUnavailable);
            }
            let mut ancestor = path.as_path();
            let mut missing = Vec::new();
            loop {
                match ancestor.canonicalize() {
                    Ok(canonical) => {
                        let root =
                            Root::open(&canonical).map_err(|_| Refusal::CensusUnavailable)?;
                        let stamp = Stamp::directory(&root)?;
                        let named =
                            fs::metadata(ancestor).map_err(|_| Refusal::CensusUnavailable)?;
                        if Stamp::metadata(&named)? != stamp {
                            return Err(Refusal::ContextChanged);
                        }
                        let suffix: PathBuf = missing.into_iter().rev().collect();
                        if let Some(first) = suffix.components().next() {
                            // canonicalize(NotFound) alone is insufficient:
                            // an existing dangling symlink is not absence.
                            match fs_safety::metadata_beneath(&root, Path::new(first.as_os_str())) {
                                Err(error) if error.is_not_found() => {}
                                _ => return Err(Refusal::CensusUnavailable),
                            }
                        }
                        return Ok(Self {
                            key: HomeKey {
                                operand: path.clone(),
                                stamp,
                                missing: suffix,
                            },
                            root,
                            anchor: ancestor.to_path_buf(),
                        });
                    }
                    Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                        missing.push(
                            ancestor
                                .file_name()
                                .ok_or(Refusal::CensusUnavailable)?
                                .to_owned(),
                        );
                        ancestor = ancestor.parent().ok_or(Refusal::CensusUnavailable)?;
                    }
                    Err(_) => return Err(Refusal::CensusUnavailable),
                }
            }
        }
        fn revalidate(&self) -> bool {
            if Stamp::directory(&self.root).ok().as_ref() != Some(&self.key.stamp)
                || fs::metadata(&self.anchor)
                    .ok()
                    .and_then(|metadata| Stamp::metadata(&metadata).ok())
                    .as_ref()
                    != Some(&self.key.stamp)
            {
                return false;
            }
            self.key.missing.components().next().is_none_or(|first| {
                matches!(fs_safety::metadata_beneath(&self.root, Path::new(first.as_os_str())), Err(error) if error.is_not_found())
            })
        }
    }

    struct Census {
        homes: Vec<HomeObservation>,
        witnesses: Vec<aliases::Witness>,
        observations: usize,
    }
    impl Census {
        fn revalidate(&self) -> bool {
            self.homes.iter().all(HomeObservation::revalidate)
                && self.witnesses.iter().all(aliases::Witness::revalidate)
        }
        fn same_homes(&self, fresh: &Self) -> bool {
            self.homes
                .iter()
                .map(|home| &home.key)
                .eq(fresh.homes.iter().map(|home| &home.key))
        }
    }

    fn census(
        registry: &Registry,
        homes: &Homes,
        selected: &AccountId,
        root: &Root,
        expected: PhysicalIdentity,
        system: &[Account],
        observation_budget: usize,
    ) -> Result<Census, Refusal> {
        if registry
            .registered()
            .len()
            .checked_add(registry.pending().len())
            .and_then(|count| count.checked_add(system.len()))
            .is_none_or(|count| count > MAX_HOMES)
        {
            return Err(Refusal::CensusLimit);
        }
        let mut accounts: Vec<_> = registry
            .registered()
            .iter()
            .map(|row| {
                row.linked_home.as_ref().map_or_else(
                    || Account::managed(row.provider, row.key, homes.path(row.provider, row.key)),
                    |path| Account::linked(row.provider, row.key, path.clone()),
                )
            })
            .collect();
        accounts.extend(
            registry.pending().iter().map(|row| {
                Account::managed(row.provider, row.key, homes.path(row.provider, row.key))
            }),
        );
        accounts.extend_from_slice(system);
        let mut result = Census {
            homes: Vec::new(),
            witnesses: Vec::new(),
            observations: 0,
        };
        for account in accounts {
            if account.id() == *selected {
                continue;
            }
            let path = account.resolved_home().ok_or(Refusal::CensusUnavailable)?;
            let home = HomeObservation::capture(path)?;
            if home.key.missing.as_os_str().is_empty() {
                if home.key.stamp.is_selected(expected) {
                    return Err(Refusal::SharedHome);
                }
                if account.provider == Provider::Codex {
                    let observed = aliases::observe(&home.root, root);
                    match observed.verdict {
                        aliases::Verdict::Reference => return Err(Refusal::SharedConfig),
                        aliases::Verdict::Unknown => return Err(Refusal::CensusUnavailable),
                        aliases::Verdict::Independent => {}
                    }
                    let witness = observed.witness.ok_or(Refusal::CensusUnavailable)?;
                    result.observations = result
                        .observations
                        .checked_add(witness.observation_count())
                        .ok_or(Refusal::CensusLimit)?;
                    if result.observations > observation_budget {
                        return Err(Refusal::CensusLimit);
                    }
                    result.witnesses.push(witness);
                }
            }
            result.homes.push(home);
        }
        if !result.revalidate() {
            return Err(Refusal::ContextChanged);
        }
        Ok(result)
    }

    impl Store {
        /// Publish only the selected fixed config, under one private registry
        /// lease and the caller's existing account lifecycle reservation.
        /// An earlier inspection is context, not permission: selection, home
        /// identity and every relevant config target are checked afresh here.
        /// No account/default/revision mutation or provider invocation occurs.
        pub(crate) fn try_initialize_codex_config_reserved(
            &self,
            reservation: &crate::host::AccountReservation,
            homes: &Homes,
            id: &AccountId,
            expected: PhysicalIdentity,
            network_access: bool,
        ) -> Result<coding_config::Outcome, CodingConfigRefusal> {
            self.initialize_codex_config_with_system(
                reservation,
                homes,
                id,
                expected,
                network_access,
                &Account::system_accounts(),
                |_| {},
            )
        }

        fn initialize_codex_config_with_system(
            &self,
            reservation: &crate::host::AccountReservation,
            homes: &Homes,
            id: &AccountId,
            expected: PhysicalIdentity,
            network_access: bool,
            system: &[Account],
            mut before_validation: impl FnMut(bool),
        ) -> Result<coding_config::Outcome, Refusal> {
            // The exact existing private lock serializes every cooperating
            // registry writer. No Registry mutation, save or revision occurs.
            let _lease = self.try_lock().map_err(|error| {
                if error.kind() == io::ErrorKind::WouldBlock {
                    Refusal::RegistryBusy
                } else {
                    Refusal::RegistryUnavailable
                }
            })?;
            let latest = read(&self.path()).map_err(|_| Refusal::RegistryUnavailable)?;
            if latest.is_newer() {
                return Err(Refusal::RegistryNewer);
            }
            let key = latest
                .registered()
                .iter()
                .find(|row| {
                    let account = row.linked_home.as_ref().map_or_else(
                        || {
                            Account::managed(
                                row.provider,
                                row.key,
                                homes.path(row.provider, row.key),
                            )
                        },
                        |path| Account::linked(row.provider, row.key, path.clone()),
                    );
                    account.id() == *id
                })
                .ok_or(Refusal::InvalidAccount)?;
            if key.provider != Provider::Codex
                || latest.is_archived(key.key)
                || latest.pending().iter().any(|row| row.key == key.key)
                // Deserialization accepts duplicate rows. A second home with
                // this UUID must not disappear through the census's selected
                // account exclusion, even if its provider differs.
                || latest.registered().iter().filter(|row| row.key == key.key).count() != 1
            {
                return Err(Refusal::InvalidAccount);
            }
            let root = reservation
                .coding_config_root(&latest, id, expected)
                .map_err(|_| Refusal::ContextChanged)?;
            let baseline = census(
                &latest,
                homes,
                id,
                &root,
                expected,
                system,
                MAX_OBSERVATIONS,
            )?;
            let prepared = match coding_config::prepare(&root, network_access) {
                Ok(prepared) => prepared,
                Err(reason) => return Ok(coding_config::Outcome::NotCreated(reason)),
            };
            let mut post = false;
            let mut refused = None;
            let outcome = prepared.publish(|| {
                before_validation(post);
                post = true;
                let validation = (|| {
                    reservation
                        .coding_config_root(&latest, id, expected)
                        .map_err(|_| Refusal::ContextChanged)?;
                    if !baseline.revalidate() {
                        return Err(Refusal::ContextChanged);
                    }
                    // The earlier witnesses stay live during this pass. Bound
                    // both sets together rather than allowing each a full
                    // independent descriptor budget. The leaf walk also has
                    // its own fixed bound for its in-progress observation.
                    let fresh = census(
                        &latest,
                        homes,
                        id,
                        &root,
                        expected,
                        system,
                        MAX_OBSERVATIONS - baseline.observations,
                    )?;
                    if !baseline.same_homes(&fresh) || !baseline.revalidate() || !fresh.revalidate()
                    {
                        return Err(Refusal::ContextChanged);
                    }
                    Ok(())
                })();
                if let Err(reason) = validation {
                    refused = Some(reason);
                    false
                } else {
                    true
                }
            });
            if matches!(
                outcome,
                coding_config::Outcome::NotCreated(coding_config::Refusal::ContextChanged)
            ) {
                return Err(refused.unwrap_or(Refusal::ContextChanged));
            }
            Ok(outcome)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::{account::AccountHome, host::AccountReservation};
        use std::os::unix::fs::symlink;

        struct Fixture {
            directory: tempfile::TempDir,
            store: Store,
            homes: Homes,
            host: crate::host::TestSharedHost,
            account: Account,
            expected: PhysicalIdentity,
            system: Vec<Account>,
        }
        impl Fixture {
            fn new() -> Self {
                let directory = tempfile::tempdir().unwrap();
                let selected = directory.path().join("selected");
                fs::create_dir(&selected).unwrap();
                let account = Account::linked(Provider::Codex, Uuid::new_v4(), selected.clone());
                let metadata = fs::metadata(&selected).unwrap();
                let store = Store::at(directory.path().join("registry"));
                let homes = Homes::at(directory.path().join("managed"));
                store
                    .update(|registry| {
                        registry.accounts.push(registered(&account));
                        registry.set_default(&account);
                        registry.set_project_default(Path::new("/fixture-project"), &account);
                    })
                    .unwrap();
                let system = [Provider::Codex, Provider::Claude]
                    .into_iter()
                    .map(|provider| {
                        let path = directory
                            .path()
                            .join(format!("system-{}", provider.executable()));
                        fs::create_dir(&path).unwrap();
                        Account {
                            provider,
                            home: AccountHome::SystemAt { path },
                        }
                    })
                    .collect();
                Self {
                    directory,
                    store,
                    homes,
                    host: AccountReservation::test_host(),
                    account,
                    expected: PhysicalIdentity {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                    },
                    system,
                }
            }
            fn path(&self) -> PathBuf {
                self.account.resolved_home().unwrap().join("config.toml")
            }
            fn reserve(&self) -> AccountReservation {
                AccountReservation::test_install(
                    &self.host,
                    std::slice::from_ref(&self.account),
                    self.store.clone(),
                    &self.homes,
                )
                .unwrap()
            }
            fn other(&self, name: &str) -> Account {
                let path = self.directory.path().join(name);
                fs::create_dir(&path).unwrap();
                let account = Account::linked(Provider::Codex, Uuid::new_v4(), path);
                self.store
                    .update(|registry| registry.accounts.push(registered(&account)))
                    .unwrap();
                account
            }
            fn initialize(
                &self,
                reservation: &AccountReservation,
                hook: impl FnMut(bool),
            ) -> Result<coding_config::Outcome, Refusal> {
                self.store.initialize_codex_config_with_system(
                    reservation,
                    &self.homes,
                    &self.account.id(),
                    self.expected,
                    false,
                    &self.system,
                    hook,
                )
            }
        }
        fn registered(account: &Account) -> Registered {
            Registered {
                provider: account.provider,
                key: account.registry_key().unwrap(),
                linked_home: account.resolved_home(),
                label: Some("fixture".into()),
                email: Some("fixture@example.test".into()),
                organization: None,
                added_at: 1,
            }
        }

        #[test]
        fn publication_holds_the_real_store_lease_and_preserves_registry_defaults_and_revision() {
            let fixture = Fixture::new();
            let reservation = fixture.reserve();
            let before = fs::read(fixture.store.path()).unwrap();
            let saved = fixture.store.load().unwrap();
            let mut passes = 0;
            let outcome = fixture.initialize(&reservation, |_| {
                passes += 1;
                assert!(matches!(fixture.store.try_lock(), Err(error) if error.kind() == io::ErrorKind::WouldBlock));
            }).unwrap();
            assert_eq!(outcome, coding_config::Outcome::Created);
            assert_eq!(passes, 2);
            assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
            assert_eq!(fixture.store.load().unwrap(), saved);
            assert_eq!(fixture.store.load().unwrap().revision(), saved.revision());
            let profile: toml::Value =
                toml::from_str(&fs::read_to_string(fixture.path()).unwrap()).unwrap();
            assert_eq!(
                profile["sandbox_workspace_write"]["network_access"].as_bool(),
                Some(false)
            );
        }

        #[test]
        fn reverse_config_links_from_registered_archived_system_and_pending_homes_refuse() {
            for variant in [
                "absolute",
                "relative",
                "parent-alias",
                "chain",
                "archived",
                "system",
                "pending",
            ] {
                let mut fixture = Fixture::new();
                let selected = fixture.account.resolved_home().unwrap();
                let other = if variant == "system" {
                    fixture.system[0].clone()
                } else if variant == "pending" {
                    let key = Uuid::new_v4();
                    let path = fixture.homes.path(Provider::Codex, key);
                    fs::create_dir_all(&path).unwrap();
                    fixture
                        .store
                        .update(|registry| registry.begin(Provider::Codex, key, 1))
                        .unwrap();
                    Account::managed(Provider::Codex, key, path)
                } else {
                    fixture.other("other")
                };
                if variant == "archived" {
                    fixture
                        .store
                        .update(|registry| registry.archived.push(other.registry_key().unwrap()))
                        .unwrap();
                }
                let parent = other.resolved_home().unwrap();
                let target = match variant {
                    "relative" => PathBuf::from("../selected/config.toml"),
                    "parent-alias" => {
                        let alias = fixture.directory.path().join("selected-alias");
                        symlink(&selected, &alias).unwrap();
                        alias.join("config.toml")
                    }
                    "chain" => {
                        symlink(fixture.path(), parent.join("next-link")).unwrap();
                        PathBuf::from("next-link")
                    }
                    _ => fixture.path(),
                };
                let config = parent.join("config.toml");
                symlink(&target, &config).unwrap();
                let inode = fs::symlink_metadata(&config).unwrap().ino();
                let before = fs::read(fixture.store.path()).unwrap();
                let reservation = fixture.reserve();
                assert_eq!(
                    fixture.initialize(&reservation, |_| {}),
                    Err(Refusal::SharedConfig),
                    "{variant}"
                );
                assert!(!fixture.path().exists());
                assert_eq!(fs::read_link(&config).unwrap(), target);
                assert_eq!(fs::symlink_metadata(&config).unwrap().ino(), inode);
                assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
            }
        }

        #[test]
        fn a_physically_distinct_dangling_config_and_absent_home_remain_untouched() {
            let mut fixture = Fixture::new();
            let other = fixture.other("other").resolved_home().unwrap();
            let target = PathBuf::from("independent.toml");
            symlink(&target, other.join("config.toml")).unwrap();
            let inode = fs::symlink_metadata(other.join("config.toml"))
                .unwrap()
                .ino();
            fixture.system[1].home = AccountHome::SystemAt {
                path: fixture.directory.path().join("absent-system-home"),
            };
            let reservation = fixture.reserve();
            assert_eq!(
                fixture.initialize(&reservation, |_| {}).unwrap(),
                coding_config::Outcome::Created
            );
            assert_eq!(fs::read_link(other.join("config.toml")).unwrap(), target);
            assert_eq!(
                fs::symlink_metadata(other.join("config.toml"))
                    .unwrap()
                    .ino(),
                inode
            );
        }

        #[test]
        fn a_home_alias_introduced_after_reservation_is_caught_by_the_latest_census() {
            let fixture = Fixture::new();
            let other = fixture.other("other").resolved_home().unwrap();
            let reservation = fixture.reserve();
            fs::rename(&other, fixture.directory.path().join("old-other")).unwrap();
            symlink(fixture.account.resolved_home().unwrap(), &other).unwrap();
            assert_eq!(
                fixture.initialize(&reservation, |_| {}),
                Err(Refusal::SharedHome)
            );
            assert!(!fixture.path().exists());
        }

        #[test]
        fn a_new_registered_reverse_alias_after_reservation_is_not_hidden_by_selected_validation() {
            let fixture = Fixture::new();
            let reservation = fixture.reserve();
            let other = fixture.other("new-other").resolved_home().unwrap();
            symlink(fixture.path(), other.join("config.toml")).unwrap();
            let latest = fixture.store.load().unwrap();
            assert!(reservation.verify_registry(&latest).is_ok());
            assert_eq!(
                fixture.initialize(&reservation, |_| {}),
                Err(Refusal::SharedConfig)
            );
            assert!(!fixture.path().exists());
        }

        #[test]
        fn accepted_duplicate_uuid_rows_refuse_without_rewriting_or_touching_either_config() {
            for provider in [Provider::Codex, Provider::Claude] {
                let fixture = Fixture::new();
                let reservation = fixture.reserve();
                let other = fixture.directory.path().join("duplicate-home");
                fs::create_dir(&other).unwrap();
                let config = other.join("config.toml");
                symlink(fixture.path(), &config).unwrap();
                let link_before = fs::symlink_metadata(&config).unwrap();
                let original = fixture.store.load().unwrap();
                let mut duplicate = original.clone();
                let mut row = duplicate.accounts[0].clone();
                row.provider = provider;
                row.linked_home = Some(other);
                duplicate.accounts.push(row);

                // Exercise the actual accepted on-disk preimage rather than
                // relying on a normal writer to construct malformed metadata.
                let bytes = toml::to_string_pretty(&duplicate).unwrap().into_bytes();
                fs::write(fixture.store.path(), &bytes).unwrap();
                let accepted = fixture.store.load().unwrap();
                assert_eq!(accepted, duplicate);
                assert_eq!(accepted.defaults, original.defaults);
                assert_eq!(accepted.project_defaults, original.project_defaults);
                assert_eq!(accepted.revision(), original.revision());
                // Selected-only validation sees the first row unchanged;
                // the new lease-time uniqueness guard must close this gap.
                assert!(reservation.verify_registry(&accepted).is_ok());
                assert_eq!(
                    fixture.initialize(&reservation, |_| panic!("publication must not start")),
                    Err(Refusal::InvalidAccount)
                );
                assert_eq!(fs::read(fixture.store.path()).unwrap(), bytes);
                assert_eq!(fixture.store.load().unwrap(), accepted);
                assert!(matches!(
                    fs::symlink_metadata(fixture.path()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound
                ));
                let link_after = fs::symlink_metadata(&config).unwrap();
                assert_eq!(
                    (link_after.dev(), link_after.ino()),
                    (link_before.dev(), link_before.ino())
                );
                assert_eq!(fs::read_link(&config).unwrap(), fixture.path());
            }
        }

        #[test]
        fn an_observed_target_changed_before_or_after_link_is_refused_or_uncertain() {
            for after_link in [false, true] {
                let fixture = Fixture::new();
                let other = fixture.other("other").resolved_home().unwrap();
                let config = other.join("config.toml");
                symlink("independent.toml", &config).unwrap();
                let reservation = fixture.reserve();
                let before = fs::read(fixture.store.path()).unwrap();
                let result = fixture.initialize(&reservation, |post| {
                    if post == after_link {
                        fs::remove_file(&config).unwrap();
                        symlink(fixture.path(), &config).unwrap();
                    }
                });
                if after_link {
                    assert_eq!(
                        result,
                        Ok(coding_config::Outcome::CreationUncertain(
                            coding_config::Uncertainty::ContextChanged
                        ))
                    );
                    assert!(fixture.path().is_file());
                } else {
                    assert_eq!(result, Err(Refusal::ContextChanged));
                    assert!(!fixture.path().exists());
                }
                assert_eq!(fs::read_link(config).unwrap(), fixture.path());
                assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
            }
        }

        #[test]
        fn stale_home_identity_busy_registry_and_occupied_configuration_never_publish() {
            let fixture = Fixture::new();
            let reservation = fixture.reserve();
            let lease = fixture.store.try_lock().unwrap();
            assert_eq!(
                fixture.initialize(&reservation, |_| {}),
                Err(Refusal::RegistryBusy)
            );
            drop(lease);
            let wrong = PhysicalIdentity {
                inode: fixture.expected.inode.wrapping_add(1),
                ..fixture.expected
            };
            assert_eq!(
                fixture.store.initialize_codex_config_with_system(
                    &reservation,
                    &fixture.homes,
                    &fixture.account.id(),
                    wrong,
                    true,
                    &fixture.system,
                    |_| {}
                ),
                Err(Refusal::ContextChanged)
            );
            assert!(!fixture.path().exists());
            fs::write(fixture.path(), b"existing user configuration").unwrap();
            assert_eq!(
                fixture.initialize(&reservation, |_| {}),
                Ok(coding_config::Outcome::NotCreated(
                    coding_config::Refusal::ExistingConfiguration
                ))
            );
            assert_eq!(
                fs::read(fixture.path()).unwrap(),
                b"existing user configuration"
            );
        }

        #[test]
        fn alias_walks_have_an_aggregate_budget_without_publishing_a_partial_result() {
            let fixture = Fixture::new();
            for index in 0..100 {
                fixture.other(&format!("other-{index}"));
            }
            let reservation = fixture.reserve();
            assert_eq!(
                fixture.initialize(&reservation, |_| {}),
                Err(Refusal::CensusLimit)
            );
            assert!(!fixture.path().exists());
        }

        #[test]
        fn fresh_census_shares_the_budget_with_the_still_retained_baseline() {
            let fixture = Fixture::new();
            for index in 0..55 {
                fixture.other(&format!("other-{index}"));
            }
            let reservation = fixture.reserve();
            let mut validations = 0;
            assert_eq!(
                fixture.initialize(&reservation, |_| validations += 1),
                Err(Refusal::CensusLimit)
            );
            // Staging was reached, so the baseline alone fit. The first
            // pre-publication pass must still account for its retained roots.
            assert_eq!(validations, 1);
            assert!(!fixture.path().exists());
        }
    }
}
