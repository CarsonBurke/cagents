//! The directories managed accounts keep their logins in.
//!
//! A managed account is a provider CLI pointed at a home cagents created for
//! it, `<data dir>/accounts/<provider>/<key>`, through the CLI's own variable
//! (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`). The CLI owns everything inside: its
//! login, its history, its caches. cagents creates the directory, links the
//! reader's own instructions and extensions into it, and deletes it when the
//! account is removed - and never opens a file the CLI wrote there.
//!
//! # What a managed home shares with the system home
//!
//! A home the CLI starts from nothing knows none of the reader's global
//! instructions, skills or agents, so a second account would behave unlike
//! the first in every conversation. A fixed list of entries the reader
//! authors is therefore linked from the system home into each new managed
//! home ([`shared_entries`]); everything else - the login above all, but also
//! history, caches and per-account state - stays the account's own. Claude
//! settings and user MCP definitions are seeded as independent copies.
//!
//! An entry is only on the list once the CLI has been seen to keep the link:
//!
//! - Codex writes `config.toml` through a link (verified with `codex features
//!   enable` on codex-cli 0.155.1: the link stayed and its target changed),
//!   so an account's configuration edits land in the reader's one file.
//! - Claude Code 2.1.280 refuses to write its settings through a link (its
//!   atomic writer opens the destination with `O_NOFOLLOW` and does not fall
//!   back), so `settings.json` is *not* linked: a linked copy would make every
//!   settings change in that account fail. A new home instead gets a copy.
//!   Its instruction file and
//!   extension directories are linked, because the CLI only ever writes
//!   inside those directories, which a link to a directory does not affect.
//!
//! A link is only ever created where the system home has the entry and the
//! managed home does not, and never replaces anything, so the reader can
//! unlink an entry to give one account its own copy.
//!
//! # Removal
//!
//! The current host admission foundation retains managed files until the
//! complete ownership/deletion permit is composed. Existing local UI cleanup
//! APIs below are outside this daemon admission cut; discovery grants no new
//! cleanup authority.
//!
//! A home is removed by renaming it aside within its provider directory and
//! then deleting the renamed tree. Nothing is followed on the way:
//! the path is rebuilt from the account's key rather than taken from
//! anywhere, every component of it is checked not to be a link, the directory
//! must be a real one owned by this user, and the standard library's
//! `remove_dir_all` unlinks symbolic links inside the tree rather than
//! descending into them - so the shared entries above are unlinked, not
//! emptied.

use std::{
    fs, io,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use uuid::Uuid;

use crate::agent::Provider;

mod creation;
pub(crate) use creation::NewHomeCreationReceipt;
mod seed;
pub(crate) use seed::PreparedSeed;

/// Where a removal parks a home while it is deleted, so a removal that is
/// interrupted leaves a name nothing else uses, and the next start finishes
/// it.
const REMOVING_PREFIX: &str = ".removing-";
const CONFIG_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CodexConfig {
    #[default]
    Shared,
    Copy,
}
/// How long a pending sign-in is left alone before a home that turns out to
/// be signed in to nothing is taken for the remains of one that never
/// finished. Long enough that no sign-in in progress - in this window or
/// another - is mistaken for one.
pub const ORPHAN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Entries of the system home a new managed home links to. See the module
/// documentation for why each is, or is not, on the list. None of them holds
/// a credential.
pub fn shared_entries(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::Claude => &["CLAUDE.md", "skills", "agents", "commands"],
        Provider::Codex => &["AGENTS.md", "config.toml", "skills", "prompts", "rules"],
        Provider::Omp => &[],
    }
}

/// The directory every managed home of an installation lives under.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Homes {
    root: PathBuf,
}

impl Homes {
    /// The running installation's homes, under its data directory, which the
    /// development runner isolates along with the rest of its state.
    pub fn installation() -> Option<Self> {
        crate::instance::project_dirs().map(|dirs| Self::at(dirs.data_dir().join("accounts")))
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Recognizes only this installation's managed namespace. A UUID in an
    /// arbitrary linked or system home never makes that directory ours.
    pub fn managed_identity(&self, path: &Path) -> Option<(Provider, Uuid)> {
        super::managed_home_identity(&self.root, path)
    }

    /// Existing physical aliases participate in eligibility, not provenance
    /// or permission to remove a home.
    pub(crate) fn canonical_managed_identity(&self, path: &Path) -> Option<(Provider, Uuid)> {
        let root = self.root.canonicalize().ok()?;
        let path = path.canonicalize().ok()?;
        super::managed_home_identity(&root, &path)
    }

    /// Serializes account creation/removal with the actual child-spawn
    /// boundary. Never wait for this while holding the daemon's state lock.
    pub(crate) fn lifecycle_lock(&self) -> io::Result<crate::instance::OwnedLock> {
        crate::secure_file::prepare_private_directory(&self.root)?;
        crate::instance::owned_lock(&self.root.join(".lifecycle.lock"), libc::LOCK_EX)
    }

    /// Host short admission must never wait behind another process's flock.
    pub(crate) fn try_lifecycle_lock(&self) -> io::Result<crate::instance::OwnedLock> {
        crate::secure_file::prepare_private_directory(&self.root)?;
        crate::instance::owned_lock(
            &self.root.join(".lifecycle.lock"),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    }

    fn provider_directory(&self, provider: Provider) -> PathBuf {
        self.root.join(provider.executable())
    }

    /// Where the account `key` of `provider` keeps its login. Derived from
    /// the key alone, which is a UUID and so cannot name anything outside
    /// its provider's directory.
    pub fn path(&self, provider: Provider, key: Uuid) -> PathBuf {
        self.provider_directory(provider)
            .join(key.hyphenated().to_string())
    }

    /// Creates a new, empty home for `provider`, private to this user, and
    /// links the reader's shared entries from `system_home` into it.
    ///
    /// The directory is created without `recursive`, so an existing entry of
    /// that name - which a fresh UUID makes practically impossible - is an
    /// error rather than something adopted.
    pub fn create(
        &self,
        provider: Provider,
        system_home: Option<&Path>,
    ) -> io::Result<(Uuid, PathBuf)> {
        self.create_with_config(provider, system_home, CodexConfig::Shared)
    }

    pub fn create_with_config(
        &self,
        provider: Provider,
        system_home: Option<&Path>,
        codex_config: CodexConfig,
    ) -> io::Result<(Uuid, PathBuf)> {
        self.create_with_config_initialized(provider, system_home, codex_config, |_| Ok(()))
    }

    /// Genuine fresh mkdir provenance, before seeded configuration, shared
    /// links or publication. Existing directories are never adopted here.
    /// Store callers hold registry/lifecycle leases: initialize must not
    /// reenter Store::load/update or acquire export federation locks. The host
    /// callback only enrolls its actual directory in the existing reservation;
    /// no production export bootstrap or read grant is enabled here.
    pub(crate) fn create_with_config_initialized(
        &self,
        provider: Provider,
        system_home: Option<&Path>,
        codex_config: CodexConfig,
        initialize: impl FnOnce(&NewHomeCreationReceipt) -> io::Result<()>,
    ) -> io::Result<(Uuid, PathBuf)> {
        self.create_planned_with_config_initialized(
            provider,
            Uuid::new_v4(),
            system_home,
            codex_config,
            initialize,
        )
    }

    /// Host admission chooses the fresh UUID before mkdir so its exact future
    /// home can be reserved. This argument grants no creation/read authority;
    /// the nonrecursive mkdir must still succeed and mint the actual receipt.
    pub(crate) fn create_planned_with_config_initialized(
        &self,
        provider: Provider,
        key: Uuid,
        system_home: Option<&Path>,
        codex_config: CodexConfig,
        initialize: impl FnOnce(&NewHomeCreationReceipt) -> io::Result<()>,
    ) -> io::Result<(Uuid, PathBuf)> {
        let parent = self.provider_directory(provider);
        crate::secure_file::prepare_private_directory(&self.root)?;
        crate::secure_file::prepare_private_directory(&parent)?;
        let path = self.path(provider, key);
        let created = creation::create(&parent, provider, key)?;
        if let Err(error) = initialize(&created).and_then(|()| created.revalidate()) {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "Account initialization failed; files were retained at {} for explicit recovery: {error}",
                    path.display(),
                ),
            ));
        }
        if let Some(system_home) = system_home {
            let seeded = match provider {
                Provider::Claude => seed_claude(system_home, &path),
                Provider::Codex if codex_config == CodexConfig::Copy => {
                    copy_configuration(&system_home.join("config.toml"), &path.join("config.toml"))
                }
                _ => Ok(()),
            };
            if let Err(error) = seeded {
                return Err(io::Error::other(format!(
                    "{error}; the new home was retained at {} for recovery",
                    path.display()
                )));
            }
            link_shared(provider, system_home, &path, codex_config);
        }
        created.sync_completed()?;
        Ok((key, path))
    }

    /// Applies an already captured seed only through the fresh creation
    /// receipt. The owning Store validates the seed before consuming Planned;
    /// no source is reopened after mkdir, and enrollment still precedes writes.
    pub(crate) fn create_prepared_seeded_initialized(
        &self,
        provider: Provider,
        key: Uuid,
        seed: &PreparedSeed,
        initialize: impl FnOnce(&NewHomeCreationReceipt) -> io::Result<()>,
    ) -> io::Result<(Uuid, PathBuf)> {
        let parent = self.provider_directory(provider);
        crate::secure_file::prepare_private_directory(&self.root)?;
        crate::secure_file::prepare_private_directory(&parent)?;
        let path = self.path(provider, key);
        let created = creation::create(&parent, provider, key)?;
        if let Err(error) = initialize(&created).and_then(|()| created.revalidate()) {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "Account initialization failed; files were retained at {} for explicit recovery: {error}",
                    path.display(),
                ),
            ));
        }
        if let Err(error) = seed.apply_to(&created) {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "Account seeding failed; files were retained at {} for explicit recovery: {error}",
                    path.display(),
                ),
            ));
        }
        created.sync_completed()?;
        Ok((key, path))
    }

    /// Deletes the home of account `key`. A home that is already gone is not
    /// an error: removing an account twice leaves it just as removed.
    pub fn remove(&self, provider: Provider, key: Uuid) -> io::Result<()> {
        if let Some(parked) = self.park(provider, key)? {
            remove_parked(&parked)?;
        }
        Ok(())
    }

    /// Rename under the lifecycle/registry locks; delete the parked tree only
    /// after releasing them, since provider caches can be large.
    pub(crate) fn park(&self, provider: Provider, key: Uuid) -> io::Result<Option<PathBuf>> {
        let path = self.path(provider, key);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        crate::secure_file::reject_symlink_components(&path)?;
        crate::secure_file::validate_owned_directory(&path)?;
        let parked = self
            .provider_directory(provider)
            // An interrupted removal may still hold a parked tree when an
            // older CLI recreates the original home. Each retirement gets a
            // unique name, so neither tree prevents the other being removed.
            .join(format!("{REMOVING_PREFIX}{}", Uuid::new_v4().hyphenated()));
        fs::rename(&path, &parked)?;
        Ok(Some(parked))
    }

    /// Called only with a supported, existing registry and both locks held.
    /// A grace period also protects homes being created by an older window
    /// that predates atomic creation/registration.
    #[cfg(test)]
    pub(crate) fn park_orphans(&self, registry: &super::registry::Registry) -> io::Result<()> {
        for provider in super::Account::PROVIDERS {
            let directory = self.provider_directory(provider);
            if !directory.exists() {
                continue;
            }
            crate::secure_file::reject_symlink_components(&directory)?;
            crate::secure_file::validate_owned_directory(&directory)?;
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                let Some(key) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| Uuid::parse_str(name).ok())
                else {
                    continue;
                };
                if entry.path() != self.path(provider, key) {
                    continue;
                }
                if registry.contains_managed_home(self, provider, key) {
                    continue;
                }
                let metadata = entry.path().symlink_metadata()?;
                if !metadata.is_dir()
                    || metadata.modified()?.elapsed().unwrap_or_default() < ORPHAN_AGE
                {
                    continue;
                }
                self.park(provider, key)?;
            }
        }
        Ok(())
    }

    /// Finishes removals an earlier run was interrupted in.
    ///
    /// Nothing else here is ever deleted on its own: a home the registry
    /// does not name may belong to a registry this run cannot read - a newer
    /// cagents, a reset configuration directory - and holds a login and its
    /// history. Sign-ins that never finished are the registry's pending
    /// entries, which the accounts view verifies before removing anything.
    pub fn finish_removals(&self) {
        for provider in super::Account::PROVIDERS {
            let directory = self.provider_directory(provider);
            if crate::secure_file::reject_symlink_components(&directory).is_err()
                || crate::secure_file::validate_owned_directory(&directory).is_err()
            {
                continue;
            }
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(key) = name
                    .to_str()
                    .and_then(|name| name.strip_prefix(REMOVING_PREFIX))
                    .and_then(|key| Uuid::parse_str(key).ok())
                else {
                    continue;
                };
                let _ = remove_parked(
                    &directory.join(format!("{REMOVING_PREFIX}{}", key.hyphenated())),
                );
            }
        }
    }
}

/// Configuration is user-authored and can contain MCP environment/header
/// values. It is copied only into the new private home, never logged.
fn open_configuration(path: &Path) -> io::Result<Option<fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Configuration must be a regular file",
        ));
    }
    if metadata.len() > CONFIG_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Configuration exceeds the 8 MiB limit",
        ));
    }
    Ok(Some(file))
}

fn read_configuration(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let Some(file) = open_configuration(path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(CONFIG_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > CONFIG_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Configuration exceeds the 8 MiB limit",
        ));
    }
    Ok(Some(bytes))
}

fn write_configuration(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn copy_configuration(source: &Path, destination: &Path) -> io::Result<()> {
    if let Some(bytes) = read_configuration(source)? {
        write_configuration(destination, &bytes)?;
    }
    Ok(())
}

fn seed_claude(system_home: &Path, home: &Path) -> io::Result<()> {
    copy_configuration(
        &system_home.join("settings.json"),
        &home.join("settings.json"),
    )?;
    let global = claude_global_path(
        system_home,
        std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|value| !value.is_empty()),
    );
    seed_claude_mcp(&global, home)
}

fn claude_global_path(system_home: &Path, overridden: bool) -> PathBuf {
    if overridden {
        system_home.join(".claude.json")
    } else {
        system_home
            .parent()
            .unwrap_or(system_home)
            .join(".claude.json")
    }
}

fn seed_claude_mcp(source: &Path, home: &Path) -> io::Result<()> {
    // Deserialize only user-scoped server definitions. Serde skips every
    // other field, including login and project trust state; none is copied.
    #[derive(serde::Deserialize, serde::Serialize)]
    struct UserServers {
        #[serde(rename = "mcpServers", default)]
        servers: serde_json::Map<String, serde_json::Value>,
    }
    let Some(file) = open_configuration(source)? else {
        return Ok(());
    };
    let servers: UserServers =
        serde_json::from_reader(io::BufReader::new(file.take(CONFIG_LIMIT + 1))).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Claude user configuration is not valid JSON with an MCP server map",
            )
        })?;
    if !servers.servers.is_empty() {
        let bytes = serde_json::to_vec(&servers).map_err(io::Error::other)?;
        write_configuration(&home.join(".claude.json"), &bytes)?;
    }
    Ok(())
}

/// Deletes a home that has been renamed aside, after checking it is still a
/// real directory of this user's.
fn remove_parked(parked: &Path) -> io::Result<()> {
    crate::secure_file::reject_symlink_components(parked)?;
    crate::secure_file::validate_owned_directory(parked)?;
    fs::remove_dir_all(parked)
}

/// Links each of `provider`'s shared entries that `system_home` has and
/// `home` does not. Best effort: an entry that cannot be linked leaves the
/// account without it, which is how every managed home started before, and
/// is not worth refusing the account over.
fn link_shared(provider: Provider, system_home: &Path, home: &Path, codex_config: CodexConfig) {
    if system_home == home {
        return;
    }
    for name in shared_entries(provider) {
        // Copy is a policy choice, including when the source was absent or
        // changed after seeding. Never fall back to a shared configuration.
        if provider == Provider::Codex
            && codex_config == CodexConfig::Copy
            && *name == "config.toml"
        {
            continue;
        }
        let source = system_home.join(name);
        if fs::symlink_metadata(&source).is_err() {
            continue;
        }
        let destination = home.join(name);
        if fs::symlink_metadata(&destination).is_ok() {
            continue;
        }
        let _ = std::os::unix::fs::symlink(&source, &destination);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    use super::*;

    #[test]
    fn a_new_home_is_private_and_links_only_the_shared_entries_that_exist() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system");
        fs::create_dir_all(system.join("skills")).unwrap();
        fs::write(system.join("CLAUDE.md"), "be brief").unwrap();
        fs::write(system.join("settings.json"), "{}").unwrap();
        let homes = Homes::at(directory.path().join("accounts"));
        let (key, home) = homes.create(Provider::Claude, Some(&system)).unwrap();
        assert_eq!(home, homes.path(Provider::Claude, key));
        assert!(home.starts_with(directory.path().join("accounts/claude")));
        let metadata = fs::symlink_metadata(&home).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        assert_eq!(
            fs::read_link(home.join("CLAUDE.md")).unwrap(),
            system.join("CLAUDE.md")
        );
        assert_eq!(
            fs::read_link(home.join("skills")).unwrap(),
            system.join("skills")
        );
        // Claude can atomically replace its independent settings copy.
        assert!(
            !fs::symlink_metadata(home.join("settings.json"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::write(home.join("settings.json"), "{\"theme\":\"dark\"}").unwrap();
        assert_eq!(
            fs::read_to_string(system.join("settings.json")).unwrap(),
            "{}"
        );
        assert!(fs::symlink_metadata(home.join("agents")).is_err());
        // Nothing that holds a login is ever on the list.
        for provider in Provider::ALL {
            for name in shared_entries(provider) {
                assert!(
                    !name.contains("cred") && !name.starts_with("auth"),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn claude_mcp_seed_excludes_account_and_project_state() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.json");
        let home = directory.path().join("managed");
        fs::create_dir(&home).unwrap();
        fs::write(
            &source,
            include_str!("../../tests/fixtures/claude-user-config.json"),
        )
        .unwrap();
        seed_claude_mcp(&source, &home).unwrap();
        let copied = fs::read_to_string(home.join(".claude.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&copied).unwrap();
        assert_eq!(parsed.as_object().unwrap().len(), 1);
        assert_eq!(parsed["mcpServers"]["example"]["command"], "example-mcp");
        assert!(!copied.contains("fixture-do-not-copy"));
        assert_eq!(
            fs::metadata(home.join(".claude.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(
            seed_claude_mcp(&source, &home).is_err(),
            "never overwrite an existing provider file"
        );
    }

    #[test]
    fn claude_mcp_source_matches_default_and_overridden_homes() {
        assert_eq!(
            claude_global_path(Path::new("/user/.claude"), false),
            Path::new("/user/.claude.json")
        );
        assert_eq!(
            claude_global_path(Path::new("/custom"), true),
            Path::new("/custom/.claude.json")
        );
        assert_eq!(
            claude_global_path(Path::new("/user/.claude"), true),
            Path::new("/user/.claude/.claude.json")
        );
    }

    #[test]
    fn codex_copy_is_independent_and_shared_remains_a_link() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system");
        fs::create_dir(&system).unwrap();
        fs::write(
            system.join("config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n",
        )
        .unwrap();
        let homes = Homes::at(directory.path().join("accounts"));
        let (_, copied) = homes
            .create_with_config(Provider::Codex, Some(&system), CodexConfig::Copy)
            .unwrap();
        let (_, shared) = homes.create(Provider::Codex, Some(&system)).unwrap();
        assert!(
            !fs::symlink_metadata(copied.join("config.toml"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::symlink_metadata(shared.join("config.toml"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::write(copied.join("config.toml"), "model = \"fixture\"\n").unwrap();
        assert!(
            fs::read_to_string(system.join("config.toml"))
                .unwrap()
                .contains("keyring")
        );
        assert_ne!(
            fs::canonicalize(copied).unwrap(),
            fs::canonicalize(shared).unwrap(),
            "Codex derives its keyring key from each canonical home"
        );
    }

    #[test]
    fn codex_copy_never_links_a_dangling_configuration_or_its_later_target() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system");
        fs::create_dir_all(system.join("skills")).unwrap();
        fs::write(system.join("AGENTS.md"), "fixture instructions").unwrap();
        let target = directory.path().join("later-config.toml");
        symlink(&target, system.join("config.toml")).unwrap();
        let homes = Homes::at(directory.path().join("accounts"));
        let (_, copied) = homes
            .create_with_config(Provider::Codex, Some(&system), CodexConfig::Copy)
            .unwrap();
        let (_, shared) = homes.create(Provider::Codex, Some(&system)).unwrap();
        let config = copied.join("config.toml");
        assert_eq!(
            fs::symlink_metadata(&config).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            fs::read_link(copied.join("AGENTS.md")).unwrap(),
            system.join("AGENTS.md")
        );
        assert_eq!(
            fs::read_link(copied.join("skills")).unwrap(),
            system.join("skills")
        );
        assert_eq!(
            fs::read_link(shared.join("config.toml")).unwrap(),
            system.join("config.toml")
        );

        let original = b"approval_policy = \"on-request\"\n";
        fs::write(&target, original).unwrap();
        assert_eq!(
            fs::symlink_metadata(&config).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(fs::read(shared.join("config.toml")).unwrap(), original);
        write_configuration(&config, b"model = \"managed-fixture\"\n").unwrap();
        assert!(fs::symlink_metadata(&config).unwrap().is_file());
        assert_eq!(fs::read(&target).unwrap(), original);
        assert_eq!(fs::read_link(system.join("config.toml")).unwrap(), target);
    }

    #[test]
    fn codex_copy_policy_survives_changes_between_seeding_and_linking() {
        for remove_copy in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let system = directory.path().join("system");
            let copied = directory.path().join("copied");
            let shared = directory.path().join("shared");
            for path in [&system, &copied, &shared] {
                fs::create_dir(path).unwrap();
            }
            let source = system.join("config.toml");
            let destination = copied.join("config.toml");
            if remove_copy {
                fs::write(&source, b"fixture original").unwrap();
            }
            copy_configuration(&source, &destination).unwrap();
            if remove_copy {
                fs::remove_file(&destination).unwrap();
            } else {
                fs::write(&source, b"fixture arrived after copy").unwrap();
            }
            // Exercise the actual phase boundary without sleeps or a race hook.
            link_shared(Provider::Codex, &system, &copied, CodexConfig::Copy);
            assert_eq!(
                fs::symlink_metadata(&destination).unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
            link_shared(Provider::Codex, &system, &shared, CodexConfig::Shared);
            assert_eq!(fs::read_link(shared.join("config.toml")).unwrap(), source);
        }
    }

    #[test]
    fn codex_copy_of_a_source_symlink_preserves_private_opaque_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system");
        fs::create_dir(&system).unwrap();
        let target = directory.path().join("source-config.toml");
        let original = b"# opaque fixture\n\xff\x00\r\n";
        fs::write(&target, original).unwrap();
        symlink(&target, system.join("config.toml")).unwrap();
        let homes = Homes::at(directory.path().join("accounts"));
        let (_, copied) = homes
            .create_with_config(Provider::Codex, Some(&system), CodexConfig::Copy)
            .unwrap();
        let config = copied.join("config.toml");
        let metadata = fs::symlink_metadata(&config).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_ne!(metadata.ino(), fs::metadata(&target).unwrap().ino());
        assert_eq!(fs::read(&config).unwrap(), original);
        fs::write(&config, b"independent fixture edit").unwrap();
        assert_eq!(fs::read(&target).unwrap(), original);
    }

    #[test]
    fn invalid_mcp_source_does_not_write_partial_or_leak_its_contents() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.json");
        fs::write(&source, r#"{"mcpServers":"private-fixture"}"#).unwrap();
        let error = seed_claude_mcp(&source, directory.path()).unwrap_err();
        assert!(!error.to_string().contains("private-fixture"));
        assert!(!directory.path().join(".claude.json").exists());
    }

    #[test]
    fn a_failed_seed_retains_the_unregistered_home_for_proved_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system");
        fs::create_dir_all(system.join("settings.json")).unwrap();
        let homes = Homes::at(directory.path().join("accounts"));
        assert!(homes.create(Provider::Claude, Some(&system)).is_err());
        assert_eq!(
            fs::read_dir(directory.path().join("accounts/claude"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn removing_a_home_unlinks_shared_entries_without_touching_their_targets() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system");
        fs::create_dir_all(system.join("skills/review")).unwrap();
        fs::write(system.join("skills/review/SKILL.md"), "keep me").unwrap();
        fs::write(system.join("config.toml"), "model = \"x\"").unwrap();
        let homes = Homes::at(directory.path().join("accounts"));
        let (key, home) = homes.create(Provider::Codex, Some(&system)).unwrap();
        fs::create_dir_all(home.join("sessions")).unwrap();
        fs::write(home.join("sessions/one.jsonl"), "{}").unwrap();
        // A link the CLI, or anyone, left inside the home is unlinked too.
        let outside = directory.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("precious"), "x").unwrap();
        symlink(&outside, home.join("escape")).unwrap();

        homes.remove(Provider::Codex, key).unwrap();
        assert!(fs::symlink_metadata(&home).is_err());
        assert_eq!(
            fs::read_to_string(system.join("skills/review/SKILL.md")).unwrap(),
            "keep me"
        );
        assert!(system.join("config.toml").exists());
        assert!(outside.join("precious").exists());
        // Removing it again is not an error.
        homes.remove(Provider::Codex, key).unwrap();
        let parent = directory.path().join("accounts/codex");
        assert_eq!(fs::read_dir(parent).unwrap().count(), 0);
    }

    #[test]
    fn a_home_replaced_by_a_link_is_refused_and_its_target_survives() {
        let directory = tempfile::tempdir().unwrap();
        let homes = Homes::at(directory.path().join("accounts"));
        let (key, home) = homes.create(Provider::Claude, None).unwrap();
        fs::remove_dir(&home).unwrap();
        let target = directory.path().join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("file"), "x").unwrap();
        symlink(&target, &home).unwrap();
        assert!(homes.remove(Provider::Claude, key).is_err());
        assert!(target.join("file").exists());
        assert!(
            fs::symlink_metadata(&home)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn a_provider_directory_behind_a_link_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(directory.path().join("accounts")).unwrap();
        symlink(&real, directory.path().join("accounts/claude")).unwrap();
        let homes = Homes::at(directory.path().join("accounts"));
        assert!(homes.create(Provider::Claude, None).is_err());
        assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
    }

    #[test]
    fn only_interrupted_removals_are_finished_on_their_own() {
        let directory = tempfile::tempdir().unwrap();
        let homes = Homes::at(directory.path().join("accounts"));
        let (_, unknown_home) = homes.create(Provider::Claude, None).unwrap();
        let (_, codex_home) = homes.create(Provider::Codex, None).unwrap();
        let parked = directory.path().join(format!(
            "accounts/codex/{REMOVING_PREFIX}{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(parked.join("inner")).unwrap();
        let foreign = directory.path().join("accounts/claude/notes");
        fs::create_dir_all(&foreign).unwrap();

        homes.finish_removals();
        assert!(!parked.exists(), "an interrupted removal is finished");
        assert!(
            unknown_home.exists() && codex_home.exists(),
            "a home no registry names is never taken for garbage"
        );
        assert!(foreign.exists());
        assert_eq!(fs::metadata(&unknown_home).unwrap().uid(), unsafe {
            libc::geteuid()
        });
    }
}
