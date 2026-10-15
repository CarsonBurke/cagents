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
//! A home is removed by renaming it aside within its provider directory and
//! then deleting the renamed tree. Nothing is followed on the way:
//! the current operation retains the exact directory and parent descriptors.
//! Cleanup walks beneath those descriptors and unlinks symlinks themselves.
//! Prior parked names and unregistered homes are retained until an explicit
//! scoped operation authenticates them; naming/age alone never authorizes a
//! delete or rename.

use std::{
    fs, io,
    io::{Read, Write},
    path::{Path, PathBuf},
    os::{fd::AsRawFd, unix::{ffi::OsStrExt, fs::{MetadataExt, OpenOptionsExt}}},
    time::Duration,
};

use uuid::Uuid;

use crate::agent::Provider;

/// Where a removal parks a home while it is deleted, so a removal that is
/// interrupted leaves the files intact; a later loader must not infer that
/// no actor can still use that inode.
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

/// Exact current-operation retirement, never reconstructed from a parked name.
/// Both descriptors survive pathname replacement and are held until cleanup.
#[derive(Debug)]
pub(crate) struct ParkedRemoval {
    #[cfg(feature = "host")]
    provider: Provider,
    #[cfg(feature = "host")]
    key: Uuid,
    parent: fs::File,
    directory: fs::File,
    name: std::ffi::OsString,
}

impl ParkedRemoval {
    #[cfg(feature = "host")]
    pub(crate) fn account(&self) -> (Provider, Uuid) { (self.provider, self.key) }
    #[cfg(any(feature = "host", test))]
    pub(crate) fn current_path(&self) -> io::Result<PathBuf> {
        fs::read_link(descriptor_path(&self.directory))
    }
    #[cfg(test)]
    pub(crate) fn path_for_test(&self) -> PathBuf { self.current_path().unwrap() }
    #[cfg(feature = "host")]
    pub(crate) fn identity(&self) -> io::Result<(u64, u64)> {
        let metadata = self.directory.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }
    /// Delete only the tree captured by this operation. Old parked names carry
    /// no ownership proof and are deliberately never enumerated here.
    pub(crate) fn finish(self) -> io::Result<()> {
        let parent = descriptor_path(&self.parent);
        let named = parent.join(&self.name);
        require_same_directory(&self.directory, &named)?;
        let mut remaining = 1_000_000usize;
        remove_contents(&self.directory, 0, &mut remaining)?;
        require_same_directory(&self.directory, &named)?;
        unlink_entry(&self.parent, &self.name, libc::AT_REMOVEDIR)
    }
}

fn descriptor_path(file: &fs::File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn open_directory(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)
}

fn require_same_directory(file: &fs::File, path: &Path) -> io::Result<()> {
    let held = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    if !named.is_dir() || (held.dev(), held.ino()) != (named.dev(), named.ino()) {
        return Err(io::Error::other("The parked account directory changed; its files were retained"));
    }
    Ok(())
}

fn unlink_entry(parent: &fs::File, name: &std::ffi::OsStr, flags: libc::c_int) -> io::Result<()> {
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::other("Account entry contains a zero byte"))?;
    // SAFETY: the owned directory descriptor and CString outlive unlinkat.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_child_without_mounts(parent: &fs::File, name: &std::ffi::OsStr) -> io::Result<fs::File> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::FromRawFd;
        #[repr(C)]
        struct OpenHow { flags: u64, mode: u64, resolve: u64 }
        let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| io::Error::other("Account entry contains a zero byte"))?;
        // linux/openat2.h: NO_XDEV rejects all mount transitions, including
        // bind mounts on the same device; the inode/device check alone cannot.
        let how = OpenHow {
            flags: (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) as u64,
            mode: 0, resolve: 0x01 | 0x02 | 0x04 | 0x08,
        };
        // SAFETY: the owned directory, CString and exact open_how layout
        // outlive the syscall; a successful result is one newly owned FD.
        let raw = unsafe { libc::syscall(libc::SYS_openat2, parent.as_raw_fd(), name.as_ptr(), &how, std::mem::size_of::<OpenHow>()) };
        if raw < 0 { return Err(io::Error::last_os_error()); }
        Ok(unsafe { fs::File::from_raw_fd(raw as libc::c_int) })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (parent, name);
        Err(io::Error::new(io::ErrorKind::Unsupported, "This platform has no scoped account cleanup backend"))
    }
}

fn remove_contents(directory: &fs::File, depth: usize, remaining: &mut usize) -> io::Result<()> {
    if depth > 256 { return Err(io::Error::other("Account tree exceeds the cleanup depth bound")); }
    let anchored = descriptor_path(directory);
    let device = directory.metadata()?.dev();
    for entry in fs::read_dir(&anchored)? {
        let entry = entry?;
        *remaining = remaining.checked_sub(1)
            .ok_or_else(|| io::Error::other("Account tree exceeds the cleanup entry bound"))?;
        let path = anchored.join(entry.file_name());
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            // Never follow a substituted link or cross a mounted filesystem.
            let child = open_child_without_mounts(directory, &entry.file_name())?;
            require_same_directory(&child, &path)?;
            if child.metadata()?.dev() != device {
                return Err(io::Error::other("Account cleanup refuses a mounted filesystem"));
            }
            remove_contents(&child, depth + 1, remaining)?;
            require_same_directory(&child, &path)?;
            unlink_entry(directory, &entry.file_name(), libc::AT_REMOVEDIR)?;
        } else {
            // Symlinks and ordinary files are unlinked from the pinned parent.
            unlink_entry(directory, &entry.file_name(), 0)?;
        }
    }
    Ok(())
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

    /// Serializes account creation/removal with the actual child-spawn
    /// boundary. Never wait for this while holding the daemon's state lock.
    pub(crate) fn lifecycle_lock(&self) -> io::Result<crate::instance::OwnedLock> {
        crate::secure_file::prepare_private_directory(&self.root)?;
        crate::instance::owned_lock(&self.root.join(".lifecycle.lock"), libc::LOCK_EX)
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
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let parent = self.provider_directory(provider);
        crate::secure_file::prepare_private_directory(&self.root)?;
        crate::secure_file::prepare_private_directory(&parent)?;
        let key = Uuid::new_v4();
        let path = self.path(provider, key);
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        crate::secure_file::reject_symlink_components(&path)?;
        crate::secure_file::validate_owned_directory(&path)?;
        // The mode given at creation is narrowed by the umask, never widened;
        // setting it again states it exactly.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        if let Some(system_home) = system_home {
            let seeded = match provider {
                Provider::Claude => seed_claude(system_home, &path),
                Provider::Codex if codex_config == CodexConfig::Copy => {
                    copy_configuration(&system_home.join("config.toml"), &path.join("config.toml"))
                }
                _ => Ok(()),
            };
            if let Err(error) = seeded {
                let _ = self.remove(provider, key);
                return Err(error);
            }
            link_shared(provider, system_home, &path);
        }
        Ok((key, path))
    }

    /// Deletes the home of account `key`. A home that is already gone is not
    /// an error: removing an account twice leaves it just as removed.
    pub fn remove(&self, provider: Provider, key: Uuid) -> io::Result<()> {
        if let Some(parked) = self.park(provider, key)? {
            parked.finish()?;
        }
        Ok(())
    }

    /// Rename under the lifecycle/registry locks; delete the parked tree only
    /// after releasing them, since provider caches can be large.
    pub(crate) fn park(&self, provider: Provider, key: Uuid) -> io::Result<Option<ParkedRemoval>> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (provider, key);
            return Err(io::Error::new(io::ErrorKind::Unsupported, "Scoped account-tree deletion needs this platform's descriptor cleanup backend; files were retained"));
        }
        #[cfg(target_os = "linux")]
        {
        let path = self.path(provider, key);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        crate::secure_file::reject_symlink_components(&path)?;
        crate::secure_file::validate_owned_directory(&path)?;
        let parent = open_directory(&self.provider_directory(provider))?;
        let source_name = std::ffi::OsString::from(key.hyphenated().to_string());
        let source = descriptor_path(&parent).join(&source_name);
        let directory = open_directory(&source)?;
        require_same_directory(&directory, &source)?;
        let name = std::ffi::OsString::from(format!("{REMOVING_PREFIX}{}", Uuid::new_v4().hyphenated()));
        let from = std::ffi::CString::new(source_name.as_bytes()).unwrap();
        let to = std::ffi::CString::new(name.as_bytes()).unwrap();
        // SAFETY: both single-component names and the owned parent descriptor
        // outlive renameat2. NOREPLACE preserves another operation's evidence.
        if unsafe { libc::renameat2(parent.as_raw_fd(), from.as_ptr(), parent.as_raw_fd(), to.as_ptr(), libc::RENAME_NOREPLACE) } < 0 {
            return Err(io::Error::last_os_error());
        }
        require_same_directory(&directory, &descriptor_path(&parent).join(&name))?;
        parent.sync_all()?;
        Ok(Some(ParkedRemoval {
            #[cfg(feature = "host")] provider,
            #[cfg(feature = "host")] key,
            parent, directory, name,
        }))
        }
    }

    /// Legacy parked names have no retained operation/census authority.
    /// Keep them until an explicit operation authenticates that exact tree.
    pub fn finish_removals(&self) {}

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
    file.write_all(bytes)
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



/// Links each of `provider`'s shared entries that `system_home` has and
/// `home` does not. Best effort: an entry that cannot be linked leaves the
/// account without it, which is how every managed home started before, and
/// is not worth refusing the account over.
fn link_shared(provider: Provider, system_home: &Path, home: &Path) {
    if system_home == home {
        return;
    }
    for name in shared_entries(provider) {
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
    fn invalid_mcp_source_does_not_write_partial_or_leak_its_contents() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.json");
        fs::write(&source, r#"{"mcpServers":"private-fixture"}"#).unwrap();
        let error = seed_claude_mcp(&source, directory.path()).unwrap_err();
        assert!(!error.to_string().contains("private-fixture"));
        assert!(!directory.path().join(".claude.json").exists());
    }

    #[test]
    fn a_failed_seed_removes_the_unregistered_home() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system");
        fs::create_dir_all(system.join("settings.json")).unwrap();
        let homes = Homes::at(directory.path().join("accounts"));
        assert!(homes.create(Provider::Claude, Some(&system)).is_err());
        assert_eq!(
            fs::read_dir(directory.path().join("accounts/claude"))
                .unwrap()
                .count(),
            0
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
    fn interrupted_removal_names_never_authorize_a_global_sweep() {
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
        assert!(parked.exists(), "a parked name alone never proves final retirement");
        assert!(
            unknown_home.exists() && codex_home.exists(),
            "a home no registry names is never taken for garbage"
        );
        assert!(foreign.exists());
        assert_eq!(fs::metadata(&unknown_home).unwrap().uid(), unsafe {
            libc::geteuid()
        });
    }
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an explicitly invoked private user/mount namespace"]
    fn scoped_cleanup_refuses_a_same_filesystem_bind_mount() {
        const CHILD: &str = "CAGENTS_PRIVATE_ACCOUNT_MOUNT_FIXTURE";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new("unshare")
                .args(["--user", "--map-root-user", "--mount", "--fork"])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", "account::home::tests::scoped_cleanup_refuses_a_same_filesystem_bind_mount", "--ignored", "--nocapture"])
                .env(CHILD,"1").status().unwrap();
            assert!(status.success(),"private mount namespace unavailable or fixture failed; no production mount was requested");
            return;
        }
        // No mount operation occurs outside the explicit private child.
        assert_eq!(unsafe { libc::mount(std::ptr::null(),c"/".as_ptr(),std::ptr::null(),libc::MS_REC | libc::MS_PRIVATE,std::ptr::null()) },0);
        let temporary=tempfile::tempdir().unwrap();
        let homes=Homes::at(temporary.path().join("accounts"));
        let (key,path)=homes.create(Provider::Claude,None).unwrap();
        let external=temporary.path().join("external");fs::create_dir(&external).unwrap();
        fs::write(external.join("keep"),b"outside selected tree").unwrap();
        let target=path.join("mounted");fs::create_dir(&target).unwrap();
        let source=std::ffi::CString::new(external.as_os_str().as_bytes()).unwrap();
        let target_c=std::ffi::CString::new(target.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mount(source.as_ptr(),target_c.as_ptr(),std::ptr::null(),libc::MS_BIND,std::ptr::null()) },0);
        struct Mount(PathBuf);
        impl Drop for Mount { fn drop(&mut self) {
            let target=std::ffi::CString::new(self.0.as_os_str().as_bytes()).unwrap();
            unsafe { libc::umount2(target.as_ptr(),libc::MNT_DETACH); }
        } }
        let mut mounted=Mount(target);
        let receipt=homes.park(Provider::Claude,key).unwrap().unwrap();
        mounted.0=receipt.path_for_test().join("mounted");
        assert_eq!(fs::metadata(&external).unwrap().dev(),fs::metadata(&mounted.0).unwrap().dev(),"same-device case");
        let error=receipt.finish().unwrap_err();
        assert_eq!(error.raw_os_error(),Some(libc::EXDEV));
        assert_eq!(fs::read(external.join("keep")).unwrap(),b"outside selected tree");
        drop(mounted);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn account_cleanup_directory_opens_require_kernel_no_mount_crossing() {
        let root = open_directory(Path::new("/")).unwrap();
        let error = open_child_without_mounts(&root, std::ffi::OsStr::new("proc")).unwrap_err();
        assert!(matches!(error.raw_os_error(), Some(libc::EXDEV | libc::ENOSYS | libc::EINVAL)), "kernel must refuse mount traversal or an unsupported mechanism: {error}");
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir(temporary.path().join("child")).unwrap();
        let directory = open_directory(temporary.path()).unwrap();
        let child = open_child_without_mounts(&directory, std::ffi::OsStr::new("child"));
        match child {
            Ok(child) => assert_eq!(child.metadata().unwrap().ino(),fs::metadata(temporary.path().join("child")).unwrap().ino()),
            Err(error) => assert!(matches!(error.raw_os_error(),Some(libc::ENOSYS | libc::EINVAL))),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scoped_cleanup_deletes_only_its_captured_tree_and_preserves_replacements() {
        let temporary = tempfile::tempdir().unwrap();
        let homes = Homes::at(temporary.path().join("accounts"));
        let (a, path_a) = homes.create(Provider::Claude, None).unwrap();
        fs::write(path_a.join("keep"), b"unrelated").unwrap();
        let abandoned = homes.park(Provider::Claude, a).unwrap().unwrap();
        let parked_a = homes.provider_directory(Provider::Claude).join(&abandoned.name);
        drop(abandoned); // An interrupted earlier operation has no current receipt.
        let (b, path_b) = homes.create(Provider::Codex, None).unwrap();
        fs::write(path_b.join("delete"), b"selected").unwrap();
        homes.park(Provider::Codex, b).unwrap().unwrap().finish().unwrap();
        homes.finish_removals();
        assert_eq!(fs::read(parked_a.join("keep")).unwrap(), b"unrelated");
        assert!(!path_b.exists());

        let (c, path_c) = homes.create(Provider::Codex, None).unwrap();
        fs::write(path_c.join("original"), b"kept").unwrap();
        let receipt = homes.park(Provider::Codex, c).unwrap().unwrap();
        let name = homes.provider_directory(Provider::Codex).join(&receipt.name);
        let moved = temporary.path().join("moved-original");
        fs::rename(&name, &moved).unwrap();
        fs::create_dir(&name).unwrap();
        fs::write(name.join("replacement"), b"untouched").unwrap();
        assert!(receipt.finish().is_err());
        assert_eq!(fs::read(moved.join("original")).unwrap(), b"kept");
        assert_eq!(fs::read(name.join("replacement")).unwrap(), b"untouched");
        let (d, path_d) = homes.create(Provider::Codex, None).unwrap();
        fs::write(path_d.join("selected"), b"original parent").unwrap();
        let receipt = homes.park(Provider::Codex, d).unwrap().unwrap();
        let parked_name = receipt.name.clone();
        let parent = homes.provider_directory(Provider::Codex);
        let original_parent = temporary.path().join("original-provider-directory");
        fs::rename(&parent, &original_parent).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::create_dir(parent.join(&parked_name)).unwrap();
        fs::write(parent.join(&parked_name).join("replacement"), b"foreign parent").unwrap();
        receipt.finish().unwrap();
        assert!(!original_parent.join(&parked_name).exists());
        assert_eq!(fs::read(parent.join(&parked_name).join("replacement")).unwrap(), b"foreign parent");

    }

}
