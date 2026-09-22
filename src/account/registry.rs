//! The accounts an installation has added, and which one each provider uses
//! for a new conversation.
//!
//! `accounts.toml` in the instance's configuration directory holds only
//! what the reader chose and what the provider's CLI reported as display
//! metadata: a key per managed account, its provider, the name the reader
//! gave it, the email and organization its CLI named, when it was added, and
//! the default account per provider. It never holds a token, a key or a
//! path: a home's path is derived from its key ([`super::home::Homes::path`]),
//! so nothing read back from this file can point cagents at another
//! directory.
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
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Account, AccountId, Identity, home::Homes};
use crate::agent::Provider;

const FILE_NAME: &str = "accounts.toml";
const LOCK_NAME: &str = "accounts.toml.lock";
const VERSION: u32 = 1;
/// A registry larger than this is not one cagents wrote.
const MAX_BYTES: u64 = 256 * 1024;

/// One account cagents added, as the registry records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Managed {
    pub key: Uuid,
    #[serde(with = "provider_name")]
    pub provider: Provider,
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

impl Managed {
    pub fn identity(&self) -> Identity {
        Identity {
            email: self.email.clone(),
            organization: self.organization.clone(),
            plan: None,
        }
    }
}

/// The whole registry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    version: u32,
    /// The account each provider starts new conversations on, by provider
    /// executable name: `"system"` or a managed account's key. A provider
    /// with no entry, or an entry naming an account that is gone, uses its
    /// system account.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    defaults: BTreeMap<String, String>,
    /// Names the reader gave the system accounts, by provider executable.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    system_labels: BTreeMap<String, String>,
    #[serde(default, rename = "account", skip_serializing_if = "Vec::is_empty")]
    accounts: Vec<Managed>,
}

const SYSTEM: &str = "system";

impl Registry {
    pub fn managed(&self) -> &[Managed] {
        &self.accounts
    }

    pub fn find(&self, key: Uuid) -> Option<&Managed> {
        self.accounts.iter().find(|account| account.key == key)
    }

    /// Every account, system accounts first and then managed ones in the
    /// order they were added, with each managed home placed under `homes`.
    pub fn accounts(&self, homes: Option<&Homes>) -> Vec<Account> {
        let mut accounts: Vec<Account> = Account::system_accounts().into();
        if let Some(homes) = homes {
            accounts.extend(self.accounts.iter().map(|managed| {
                Account::managed(
                    managed.provider,
                    managed.key,
                    homes.path(managed.provider, managed.key),
                )
            }));
        }
        accounts
    }

    /// The account `provider` starts new conversations on.
    pub fn default_account(&self, provider: Provider, homes: Option<&Homes>) -> Account {
        let managed = self
            .defaults
            .get(provider.executable())
            .and_then(|key| Uuid::parse_str(key).ok())
            .and_then(|key| self.find(key))
            .filter(|managed| managed.provider == provider);
        match (managed, homes) {
            (Some(managed), Some(homes)) => {
                Account::managed(provider, managed.key, homes.path(provider, managed.key))
            }
            _ => Account::system(provider),
        }
    }

    pub fn is_default(&self, account: &Account) -> bool {
        let chosen = self.defaults.get(account.provider.executable());
        match account.managed_key() {
            None => chosen.is_none_or(|chosen| {
                chosen == SYSTEM
                    || Uuid::parse_str(chosen)
                        .ok()
                        .and_then(|key| self.find(key))
                        .is_none()
            }),
            Some(key) => chosen.is_some_and(|chosen| Uuid::parse_str(chosen).ok() == Some(key)),
        }
    }

    pub fn set_default(&mut self, account: &Account) {
        let value = account
            .managed_key()
            .map_or_else(|| SYSTEM.to_owned(), |key| key.hyphenated().to_string());
        self.defaults
            .insert(account.provider.executable().to_owned(), value);
    }

    /// The name the reader gave `account`, if any.
    pub fn label(&self, account: &Account) -> Option<&str> {
        match account.managed_key() {
            None => self
                .system_labels
                .get(account.provider.executable())
                .map(String::as_str),
            Some(key) => self.find(key)?.label.as_deref(),
        }
    }

    /// Names `account`, or clears its name with `None`.
    pub fn set_label(&mut self, account: &Account, label: Option<String>) {
        let label = label.as_deref().and_then(super::clean_label);
        match account.managed_key() {
            None => {
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
            Some(key) => {
                if let Some(managed) = self.accounts.iter_mut().find(|managed| managed.key == key) {
                    managed.label = label;
                }
            }
        }
    }

    /// Records a managed account that has signed in, unless one already
    /// names the same login; returns the account it duplicates, if any.
    pub fn register(
        &mut self,
        managed: Managed,
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
                && existing.provider == managed.provider
                && existing.identity().same_login(&identity)
        }) {
            return Err(Account::managed(existing.provider, existing.key, PathBuf::new()).id());
        }
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
    pub fn unregister(&mut self, key: Uuid) -> Option<Managed> {
        let index = self
            .accounts
            .iter()
            .position(|managed| managed.key == key)?;
        let removed = self.accounts.remove(index);
        let key = key.hyphenated().to_string();
        self.defaults.retain(|_, chosen| *chosen != key);
        Some(removed)
    }

    /// Every managed home the registry names, for [`Homes::sweep`].
    pub fn registered(&self) -> Vec<(Provider, Uuid)> {
        self.accounts
            .iter()
            .map(|managed| (managed.provider, managed.key))
            .collect()
    }
}

/// The registry file of one installation.
#[derive(Debug, Clone)]
pub struct Store {
    directory: PathBuf,
}

impl Store {
    pub fn installation() -> Option<Self> {
        crate::instance::project_dirs().map(|dirs| Self::at(dirs.config_dir()))
    }

    pub fn at(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    fn path(&self) -> PathBuf {
        self.directory.join(FILE_NAME)
    }

    fn lock(&self) -> io::Result<fs::File> {
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
        let result = change(&mut registry);
        if registry != before {
            registry.version = VERSION;
            write(&self.directory, &path, &registry)?;
        }
        Ok((registry, result))
    }

    /// Loads the registry and, under the same lock so no window registers an
    /// account meanwhile, sweeps `homes` of what no account names.
    pub fn load_and_sweep(&self, homes: &Homes) -> io::Result<Registry> {
        let _lock = self.lock()?;
        let registry = read(&self.path())?;
        homes.sweep(&registry.registered(), std::time::SystemTime::now());
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

    fn managed(provider: Provider, email: &str) -> Managed {
        Managed {
            key: Uuid::new_v4(),
            provider,
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
        assert_eq!(default.managed_key(), Some(key));
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

    /// The policy this file exists under, held as a test: whatever the
    /// registry is asked to remember, what it writes is only the fields named
    /// here, none of which is a credential or a path.
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
            })
            .unwrap();
        let text = fs::read_to_string(directory.path().join(FILE_NAME)).unwrap();
        let document: toml::Table = toml::from_str(&text).unwrap();
        let allowed = [
            "version",
            "defaults",
            "system_labels",
            "account",
            "key",
            "provider",
            "label",
            "email",
            "organization",
            "added_at",
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
        assert_eq!(registry.managed().len(), 2);
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
