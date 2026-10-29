//! The new Create path captures sources before consuming Planned, then writes
//! only captured bytes through its actual fresh-directory receipt. Metadata
//! checks detect observed changes; they are not an atomic filesystem snapshot.
use super::{CONFIG_LIMIT, CodexConfig, NewHomeCreationReceipt, Provider, shared_entries};
use std::{
    ffi::{CStr, CString},
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
};

const CODEX_CONFIG: &CStr = c"config.toml";
const CLAUDE_SETTINGS: &CStr = c"settings.json";
const CLAUDE_GLOBAL: &CStr = c".claude.json";

fn changed() -> io::Error {
    io::Error::other("An account configuration source changed; reload accounts and retry")
}

fn invalid_operand() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "Invalid account seed operand")
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid_operand())
}

#[derive(PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    mode: u32,
}

#[derive(PartialEq, Eq)]
struct Stamp {
    identity: Identity,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Stamp {
    fn metadata(value: &fs::Metadata) -> Self {
        Self {
            identity: Identity {
                device: value.dev(),
                inode: value.ino(),
                mode: value.mode() & libc::S_IFMT,
            },
            length: value.len(),
            modified: (value.mtime(), value.mtime_nsec()),
            changed: (value.ctime(), value.ctime_nsec()),
        }
    }

    fn stat(value: &libc::stat) -> Self {
        Self {
            identity: Identity {
                device: value.st_dev,
                inode: value.st_ino,
                mode: value.st_mode & libc::S_IFMT,
            },
            length: value.st_size.max(0) as u64,
            modified: (value.st_mtime, value.st_mtime_nsec),
            changed: (value.st_ctime, value.st_ctime_nsec),
        }
    }

    fn same_directory_entry(&self, other: &Self) -> bool {
        self.identity == other.identity && (self.identity.mode != libc::S_IFLNK || self == other)
    }
}

fn stat_at(parent: &File, name: &CStr, follow: bool) -> io::Result<Option<Stamp>> {
    let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the retained descriptor, fixed name and writable stat storage
    // all outlive this call. No source contents are read here.
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            value.as_mut_ptr(),
            if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW },
        )
    };
    if result == 0 {
        return Ok(Some(Stamp::stat(&unsafe { value.assume_init() })));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        Ok(None)
    } else {
        Err(error)
    }
}

fn open_at(parent: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    // SAFETY: parent/name remain alive and successful openat returns an owned
    // descriptor. Source callers deliberately permit a final symlink.
    let raw = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if raw < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(raw) })
    }
}

enum DirectoryCapture<'a> {
    Open,
    Original(&'a File),
    Absent,
}

/// A missing component retains its first missing name under the original
/// existing ancestor. Descendants never adopt an entry that appears later.
struct DirectoryOperand {
    parent: Option<Box<Self>>,
    name: Option<CString>,
    entry: Option<Stamp>,
    directory: Option<File>,
}

impl DirectoryOperand {
    fn capture(path: &Path, capture: DirectoryCapture<'_>) -> io::Result<Self> {
        if !path.is_absolute() || path.components().count() > 256 {
            return Err(invalid_operand());
        }
        let Some(parent_path) = path.parent() else {
            if matches!(capture, DirectoryCapture::Absent) {
                return Err(changed());
            }
            let directory = match capture {
                DirectoryCapture::Original(pin) => pin.try_clone()?,
                DirectoryCapture::Open => File::open(path)?,
                DirectoryCapture::Absent => unreachable!(),
            };
            let metadata = directory.metadata()?;
            if !metadata.is_dir()
                || Stamp::metadata(&metadata).identity
                    != Stamp::metadata(&fs::metadata(path)?).identity
            {
                return Err(changed());
            }
            return Ok(Self {
                parent: None,
                name: None,
                entry: Some(Stamp::metadata(&metadata)),
                directory: Some(directory),
            });
        };
        let parent = Box::new(Self::capture(parent_path, DirectoryCapture::Open)?);
        let leaf = match path.components().next_back() {
            Some(Component::Normal(name)) => Path::new(name),
            Some(Component::ParentDir) => Path::new(".."),
            _ => return Err(invalid_operand()),
        };
        let name = c_path(leaf)?;
        let Some(parent_fd) = parent.directory.as_ref() else {
            if matches!(capture, DirectoryCapture::Original(_)) {
                return Err(changed());
            }
            return Ok(Self {
                parent: Some(parent),
                name: Some(name),
                entry: None,
                directory: None,
            });
        };
        let entry = stat_at(parent_fd, &name, false)?;
        let directory = match (&entry, capture) {
            (None, DirectoryCapture::Open | DirectoryCapture::Absent) => None,
            (Some(_), DirectoryCapture::Absent) | (None, DirectoryCapture::Original(_)) => {
                return Err(changed());
            }
            (Some(_), DirectoryCapture::Original(pin)) => Some(pin.try_clone()?),
            (Some(_), DirectoryCapture::Open) => Some(open_at(
                parent_fd,
                &name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )?),
        };
        if let Some(directory) = &directory {
            let metadata = directory.metadata()?;
            if !metadata.is_dir()
                || stat_at(parent_fd, &name, true)?
                    .is_none_or(|named| named.identity != Stamp::metadata(&metadata).identity)
            {
                return Err(changed());
            }
        }
        let operand = Self {
            parent: Some(parent),
            name: Some(name),
            entry,
            directory,
        };
        operand.revalidate()?;
        Ok(operand)
    }

    fn revalidate(&self) -> io::Result<()> {
        let Some(parent) = &self.parent else {
            let held = Stamp::metadata(&self.directory.as_ref().ok_or_else(changed)?.metadata()?);
            if self
                .entry
                .as_ref()
                .is_none_or(|entry| entry.identity != held.identity)
                || !self
                    .directory
                    .as_ref()
                    .ok_or_else(changed)?
                    .metadata()?
                    .is_dir()
            {
                return Err(changed());
            }
            return Ok(());
        };
        parent.revalidate()?;
        let Some(parent_fd) = &parent.directory else {
            return if self.entry.is_none() && self.directory.is_none() {
                Ok(())
            } else {
                Err(changed())
            };
        };
        let name = self.name.as_ref().ok_or_else(changed)?;
        let current = stat_at(parent_fd, name, false)?;
        match (&self.entry, &current) {
            (None, None) if self.directory.is_none() => return Ok(()),
            (Some(original), Some(current)) if original.same_directory_entry(current) => {}
            _ => return Err(changed()),
        }
        let held = Stamp::metadata(&self.directory.as_ref().ok_or_else(changed)?.metadata()?);
        if stat_at(parent_fd, name, true)?.is_none_or(|named| named.identity != held.identity) {
            return Err(changed());
        }
        Ok(())
    }
}

struct Configuration {
    name: &'static CStr,
    entry: Option<Stamp>,
    file: Option<File>,
    stamp: Option<Stamp>,
}

impl Configuration {
    fn capture(parent: Option<&File>, name: &'static CStr) -> io::Result<(Self, Option<Vec<u8>>)> {
        let entry = parent
            .map(|parent| stat_at(parent, name, false))
            .transpose()?
            .flatten();
        if entry.is_none() {
            return Ok((
                Self {
                    name,
                    entry,
                    file: None,
                    stamp: None,
                },
                None,
            ));
        }
        let parent = parent.ok_or_else(changed)?;
        // A final source symlink is intentional. ENOENT here is dangling or
        // changed, not the true fixed-leaf absence established above.
        let file = open_at(
            parent,
            name,
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )?;
        let before_metadata = file.metadata()?;
        if !before_metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Configuration must be a regular file",
            ));
        }
        if before_metadata.len() > CONFIG_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Configuration exceeds the 8 MiB limit",
            ));
        }
        let before = Stamp::metadata(&before_metadata);
        let mut bytes = Vec::new();
        (&file).take(CONFIG_LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > CONFIG_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Configuration exceeds the 8 MiB limit",
            ));
        }
        let captured = Self {
            name,
            entry,
            file: Some(file),
            stamp: Some(before),
        };
        captured.revalidate(Some(parent))?;
        Ok((captured, Some(bytes)))
    }

    fn revalidate(&self, parent: Option<&File>) -> io::Result<()> {
        let entry = parent
            .map(|parent| stat_at(parent, self.name, false))
            .transpose()?
            .flatten();
        if entry != self.entry {
            return Err(changed());
        }
        let Some(file) = &self.file else {
            return if self.entry.is_none() {
                Ok(())
            } else {
                Err(changed())
            };
        };
        let parent = parent.ok_or_else(changed)?;
        let current = Stamp::metadata(&file.metadata()?);
        if self.stamp.as_ref() != Some(&current)
            || stat_at(parent, self.name, true)?.as_ref() != Some(&current)
        {
            return Err(changed());
        }
        Ok(())
    }
}

enum ConfigurationParent {
    System,
    Global,
}

struct CapturedConfiguration {
    parent: ConfigurationParent,
    source: Configuration,
}

struct SeedFile {
    name: &'static CStr,
    bytes: Vec<u8>,
}

struct SharedLink {
    name: CString,
    target: CString,
}

/// Source contents remain local and may contain user-authored MCP secrets.
/// Deliberately has no Debug, Clone, serialization, or content accessors.
pub(crate) struct PreparedSeed {
    provider: Provider,
    system: DirectoryOperand,
    global: Option<DirectoryOperand>,
    configurations: Vec<CapturedConfiguration>,
    files: Vec<SeedFile>,
    links: Vec<SharedLink>,
}

impl PreparedSeed {
    pub(crate) fn capture(
        provider: Provider,
        codex_config: CodexConfig,
        system: &Path,
        system_pin: Option<&File>,
        claude_global: Option<&Path>,
    ) -> io::Result<Self> {
        if !crate::account::Account::PROVIDERS.contains(&provider)
            || (provider != Provider::Claude && claude_global.is_some())
        {
            return Err(invalid_operand());
        }
        let system_operand = DirectoryOperand::capture(
            system,
            system_pin.map_or(DirectoryCapture::Absent, DirectoryCapture::Original),
        )?;
        let mut seed = Self {
            provider,
            system: system_operand,
            global: None,
            configurations: Vec::new(),
            files: Vec::new(),
            links: Vec::new(),
        };
        if provider == Provider::Codex && codex_config == CodexConfig::Copy {
            seed.capture_configuration(ConfigurationParent::System, CODEX_CONFIG)?;
        } else if provider == Provider::Claude {
            seed.capture_configuration(ConfigurationParent::System, CLAUDE_SETTINGS)?;
            if let Some(global) = claude_global {
                if global.file_name() != Some(std::ffi::OsStr::new(".claude.json")) {
                    return Err(invalid_operand());
                }
                let parent = global.parent().ok_or_else(invalid_operand)?;
                let configuration_parent = if parent == system {
                    ConfigurationParent::System
                } else {
                    seed.global = Some(DirectoryOperand::capture(parent, DirectoryCapture::Open)?);
                    ConfigurationParent::Global
                };
                seed.capture_configuration(configuration_parent, CLAUDE_GLOBAL)?;
            }
        }
        if let Some(directory) = &seed.system.directory {
            for name in shared_entries(provider) {
                if provider == Provider::Codex
                    && codex_config == CodexConfig::Copy
                    && *name == "config.toml"
                {
                    continue;
                }
                let fixed_name = CString::new(*name).map_err(|_| invalid_operand())?;
                // Existing shared entries, including dangling links, remain
                // best effort and intentionally live namespace references.
                if matches!(stat_at(directory, &fixed_name, false), Ok(Some(_))) {
                    seed.links.push(SharedLink {
                        name: fixed_name,
                        target: c_path(&system.join(name))?,
                    });
                }
            }
        }
        seed.revalidate()?;
        Ok(seed)
    }

    fn configuration_parent(&self, parent: &ConfigurationParent) -> Option<&File> {
        match parent {
            ConfigurationParent::System => self.system.directory.as_ref(),
            ConfigurationParent::Global => self
                .global
                .as_ref()
                .and_then(|global| global.directory.as_ref()),
        }
    }

    fn capture_configuration(
        &mut self,
        parent: ConfigurationParent,
        name: &'static CStr,
    ) -> io::Result<()> {
        let (source, bytes) = Configuration::capture(self.configuration_parent(&parent), name)?;
        if let Some(mut bytes) = bytes {
            if name == CLAUDE_GLOBAL {
                #[derive(serde::Deserialize, serde::Serialize)]
                struct UserServers {
                    #[serde(rename = "mcpServers", default)]
                    servers: serde_json::Map<String, serde_json::Value>,
                }
                let servers: UserServers = serde_json::from_slice(&bytes).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Claude user configuration is not valid JSON with an MCP server map",
                    )
                })?;
                if !servers.servers.is_empty() {
                    bytes = serde_json::to_vec(&servers).map_err(io::Error::other)?;
                    self.files.push(SeedFile { name, bytes });
                }
            } else {
                self.files.push(SeedFile { name, bytes });
            }
        }
        self.configurations
            .push(CapturedConfiguration { parent, source });
        Ok(())
    }

    /// The Store validator calls this before consuming Planned. It uses only
    /// the captured handles and fixed-name metadata, never another file open.
    pub(crate) fn revalidate(&self) -> io::Result<()> {
        self.system.revalidate()?;
        if let Some(global) = &self.global {
            global.revalidate()?;
        }
        for configuration in &self.configurations {
            configuration
                .source
                .revalidate(self.configuration_parent(&configuration.parent))?;
        }
        Ok(())
    }

    pub(super) fn apply_to(&self, created: &NewHomeCreationReceipt) -> io::Result<()> {
        if created.provider() != self.provider {
            return Err(invalid_operand());
        }
        created.revalidate()?;
        for file in &self.files {
            write_configuration_at(created.directory(), file.name, &file.bytes, |file| {
                file.sync_all()
            })?;
        }
        for link in &self.links {
            // Match legacy best effort linking; collisions are never removed.
            let _ = symlink_at(created.directory(), &link.name, &link.target);
        }
        created.directory().sync_all()
    }
}

fn write_configuration_at(
    directory: &File,
    name: &CStr,
    bytes: &[u8],
    sync: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<()> {
    // SAFETY: the actual receipt directory and fixed basename outlive openat;
    // successful creation yields one owned descriptor. Never follow a collision.
    let raw = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(raw) };
    file.write_all(bytes)?;
    sync(&file)
}

fn symlink_at(directory: &File, name: &CStr, target: &CStr) -> io::Result<()> {
    // SAFETY: both strings and the actual receipt descriptor outlive the call.
    if unsafe { libc::symlinkat(target.as_ptr(), directory.as_raw_fd(), name.as_ptr()) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{
        home::Homes,
        preparation::{PreparationPayload, PreparationState},
        registry::Store,
    };
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        path::PathBuf,
    };
    use uuid::Uuid;

    struct Fixture {
        root: tempfile::TempDir,
        system: PathBuf,
        homes: Homes,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let system = root.path().join("system");
            fs::create_dir(&system).unwrap();
            let homes = Homes::at(root.path().join("managed"));
            Self {
                root,
                system,
                homes,
            }
        }

        fn capture(
            &self,
            provider: Provider,
            config: CodexConfig,
            global: Option<&Path>,
        ) -> PreparedSeed {
            PreparedSeed::capture(
                provider,
                config,
                &self.system,
                Some(&File::open(&self.system).unwrap()),
                global,
            )
            .unwrap()
        }

        fn create(&self, provider: Provider, seed: &PreparedSeed) -> PathBuf {
            let key = Uuid::new_v4();
            let (actual, path) = self
                .homes
                .create_prepared_seeded_initialized(provider, key, seed, |created| {
                    assert!(fs::read_dir(created.home())?.next().is_none());
                    Ok(())
                })
                .unwrap();
            assert_eq!(actual, key);
            assert_eq!(path, self.homes.path(provider, key));
            path
        }
    }

    #[test]
    fn copied_symlink_configuration_is_bounded_opaque_private_and_independent() {
        let fixture = Fixture::new();
        let source = fixture.root.path().join("opaque.toml");
        let bytes = b"# opaque fixture\n\xff\x00\r\n";
        fs::write(&source, bytes).unwrap();
        symlink(&source, fixture.system.join("config.toml")).unwrap();
        fs::write(fixture.system.join("AGENTS.md"), "fixture instructions").unwrap();
        let alias = fixture.root.path().join("source-alias");
        symlink(&fixture.system, &alias).unwrap();
        let pin = File::open(&fixture.system).unwrap();
        let seed =
            PreparedSeed::capture(Provider::Codex, CodexConfig::Copy, &alias, Some(&pin), None)
                .unwrap();
        seed.revalidate().unwrap();
        let home = fixture.create(Provider::Codex, &seed);
        let copied = home.join("config.toml");
        let metadata = fs::symlink_metadata(&copied).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read(&copied).unwrap(), bytes);
        assert_eq!(
            fs::read_link(home.join("AGENTS.md")).unwrap(),
            alias.join("AGENTS.md")
        );
        fs::write(&copied, b"independent fixture").unwrap();
        assert_eq!(fs::read(source).unwrap(), bytes);
    }

    #[test]
    fn original_system_pin_refuses_an_alias_retarget_before_capture_or_consumption() {
        let fixture = Fixture::new();
        let replacement = fixture.root.path().join("replacement");
        fs::create_dir(&replacement).unwrap();
        fs::write(fixture.system.join("config.toml"), "original fixture").unwrap();
        fs::write(replacement.join("config.toml"), "replacement fixture").unwrap();
        let alias = fixture.root.path().join("source-alias");
        symlink(&fixture.system, &alias).unwrap();
        let pin = File::open(&fixture.system).unwrap();
        let seed =
            PreparedSeed::capture(Provider::Codex, CodexConfig::Copy, &alias, Some(&pin), None)
                .unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&replacement, &alias).unwrap();
        assert!(seed.revalidate().is_err());
        assert!(
            PreparedSeed::capture(Provider::Codex, CodexConfig::Copy, &alias, Some(&pin), None)
                .is_err()
        );
        assert!(!fixture.root.path().join("managed").exists());
    }

    #[test]
    fn missing_system_still_captures_independent_default_claude_global() {
        let fixture = Fixture::new();
        let missing = fixture.root.path().join("home/.claude");
        fs::create_dir(missing.parent().unwrap()).unwrap();
        let global = missing.parent().unwrap().join(".claude.json");
        fs::write(&global, r#"{"mcpServers":{"fixture":{"command":"fixture-mcp"}},"oauthAccount":"fixture-do-not-copy","projects":{"private":"fixture-do-not-copy"}}"#).unwrap();
        let seed = PreparedSeed::capture(
            Provider::Claude,
            CodexConfig::Shared,
            &missing,
            None,
            Some(&global),
        )
        .unwrap();
        let home = fixture.create(Provider::Claude, &seed);
        assert!(!home.join("settings.json").exists());
        let copied = fs::read_to_string(home.join(".claude.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&copied).unwrap();
        assert_eq!(parsed.as_object().unwrap().len(), 1);
        assert_eq!(parsed["mcpServers"]["fixture"]["command"], "fixture-mcp");
        assert!(!copied.contains("fixture-do-not-copy"));
        fs::create_dir(&missing).unwrap();
        assert!(seed.revalidate().is_err());
        assert!(
            PreparedSeed::capture(
                Provider::Claude,
                CodexConfig::Shared,
                &missing,
                None,
                Some(&global)
            )
            .is_err()
        );
    }

    #[test]
    fn default_global_parent_and_overridden_global_use_their_own_pins() {
        let fixture = Fixture::new();
        let home = fixture.root.path().join("home");
        fs::create_dir(&home).unwrap();
        let default_global = home.join(".claude.json");
        fs::write(
            &default_global,
            r#"{"mcpServers":{"default":{"command":"default-fixture"}}}"#,
        )
        .unwrap();
        fs::write(
            fixture.system.join(".claude.json"),
            r#"{"mcpServers":{"override":{"command":"override-fixture"}}}"#,
        )
        .unwrap();
        let source_alias = home.join(".claude");
        symlink(&fixture.system, &source_alias).unwrap();
        let override_global = source_alias.join(".claude.json");
        let pin = File::open(&fixture.system).unwrap();
        for (global, expected) in [(&default_global, "default"), (&override_global, "override")] {
            let seed = PreparedSeed::capture(
                Provider::Claude,
                CodexConfig::Shared,
                &source_alias,
                Some(&pin),
                Some(global),
            )
            .unwrap();
            let created = fixture.create(Provider::Claude, &seed);
            let copied: serde_json::Value =
                serde_json::from_slice(&fs::read(created.join(".claude.json")).unwrap()).unwrap();
            assert!(copied["mcpServers"].get(expected).is_some());
        }
    }

    #[test]
    fn empty_mcp_map_never_copies_other_global_fields() {
        let fixture = Fixture::new();
        let global = fixture.root.path().join(".claude.json");
        for contents in [
            r#"{"mcpServers":{},"oauthAccount":"fixture-do-not-copy"}"#,
            r#"{"oauthAccount":"fixture-do-not-copy"}"#,
        ] {
            fs::write(&global, contents).unwrap();
            let seed = fixture.capture(Provider::Claude, CodexConfig::Shared, Some(&global));
            let home = fixture.create(Provider::Claude, &seed);
            assert!(fs::symlink_metadata(home.join(".claude.json")).is_err());
        }
    }

    #[test]
    fn captured_bytes_are_not_reopened_after_mkdir() {
        let fixture = Fixture::new();
        let original = fixture.root.path().join("original.toml");
        let replacement = fixture.root.path().join("replacement.toml");
        fs::write(&original, "captured fixture").unwrap();
        fs::write(&replacement, "replacement fixture").unwrap();
        let config = fixture.system.join("config.toml");
        symlink(&original, &config).unwrap();
        let seed = fixture.capture(Provider::Codex, CodexConfig::Copy, None);
        seed.revalidate().unwrap();
        let (_, home) = fixture
            .homes
            .create_prepared_seeded_initialized(Provider::Codex, Uuid::new_v4(), &seed, |_| {
                fs::remove_file(&config)?;
                symlink(&replacement, &config)
            })
            .unwrap();
        assert_eq!(
            fs::read_to_string(home.join("config.toml")).unwrap(),
            "captured fixture"
        );
        assert!(seed.revalidate().is_err());
    }

    #[test]
    fn copy_absence_never_adopts_appearance_or_falls_back_to_shared() {
        let fixture = Fixture::new();
        let seed = fixture.capture(Provider::Codex, CodexConfig::Copy, None);
        fs::write(fixture.system.join("config.toml"), "arrived fixture").unwrap();
        assert!(seed.revalidate().is_err());
        let home = fixture.create(Provider::Codex, &seed);
        assert!(fs::symlink_metadata(home.join("config.toml")).is_err());
        let shared = fixture.capture(Provider::Codex, CodexConfig::Shared, None);
        let shared_home = fixture.create(Provider::Codex, &shared);
        assert_eq!(
            fs::read_link(shared_home.join("config.toml")).unwrap(),
            fixture.system.join("config.toml")
        );
    }

    #[test]
    fn copied_dangling_sources_fail_closed_but_shared_links_keep_legacy_policy() {
        let fixture = Fixture::new();
        symlink(
            fixture.root.path().join("missing-config"),
            fixture.system.join("config.toml"),
        )
        .unwrap();
        let pin = File::open(&fixture.system).unwrap();
        assert!(
            PreparedSeed::capture(
                Provider::Codex,
                CodexConfig::Copy,
                &fixture.system,
                Some(&pin),
                None
            )
            .is_err()
        );
        let seed = fixture.capture(Provider::Codex, CodexConfig::Shared, None);
        let home = fixture.create(Provider::Codex, &seed);
        assert_eq!(
            fs::read_link(home.join("config.toml")).unwrap(),
            fixture.system.join("config.toml")
        );
        let dangling_root = fixture.root.path().join("dangling-root");
        symlink(fixture.root.path().join("missing-root"), &dangling_root).unwrap();
        assert!(
            PreparedSeed::capture(
                Provider::Codex,
                CodexConfig::Copy,
                &dangling_root,
                None,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_nonregular_and_oversize_inputs_fail_before_creation_without_contents_in_errors() {
        let fixture = Fixture::new();
        let global = fixture.root.path().join(".claude.json");
        fs::write(&global, r#"{"mcpServers":"fixture-do-not-disclose"}"#).unwrap();
        let pin = File::open(&fixture.system).unwrap();
        let error = PreparedSeed::capture(
            Provider::Claude,
            CodexConfig::Shared,
            &fixture.system,
            Some(&pin),
            Some(&global),
        )
        .err()
        .expect("invalid MCP must fail");
        assert!(!error.to_string().contains("fixture-do-not-disclose"));
        fs::create_dir(fixture.system.join("config.toml")).unwrap();
        assert!(
            PreparedSeed::capture(
                Provider::Codex,
                CodexConfig::Copy,
                &fixture.system,
                Some(&pin),
                None
            )
            .is_err()
        );
        fs::remove_dir(fixture.system.join("config.toml")).unwrap();
        File::create(fixture.system.join("config.toml"))
            .unwrap()
            .set_len(CONFIG_LIMIT + 1)
            .unwrap();
        assert!(
            PreparedSeed::capture(
                Provider::Codex,
                CodexConfig::Copy,
                &fixture.system,
                Some(&pin),
                None
            )
            .is_err()
        );
        assert!(!fixture.root.path().join("managed").exists());
    }

    #[test]
    fn source_file_or_global_parent_retarget_refuses_revalidation() {
        let fixture = Fixture::new();
        fs::write(fixture.system.join("config.toml"), "original fixture").unwrap();
        let seed = fixture.capture(Provider::Codex, CodexConfig::Copy, None);
        fs::write(
            fixture.system.join("config.toml"),
            "changed fixture of another size",
        )
        .unwrap();
        assert!(seed.revalidate().is_err());
        let parent = fixture.root.path().join("global-parent");
        let replacement = fixture.root.path().join("global-replacement");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&replacement).unwrap();
        fs::write(
            parent.join(".claude.json"),
            r#"{"mcpServers":{"original":{}}}"#,
        )
        .unwrap();
        fs::write(
            replacement.join(".claude.json"),
            r#"{"mcpServers":{"replacement":{}}}"#,
        )
        .unwrap();
        let alias = fixture.root.path().join("global-alias");
        symlink(&parent, &alias).unwrap();
        let seed = fixture.capture(
            Provider::Claude,
            CodexConfig::Shared,
            Some(&alias.join(".claude.json")),
        );
        fs::remove_file(&alias).unwrap();
        symlink(&replacement, &alias).unwrap();
        assert!(seed.revalidate().is_err());
    }

    #[test]
    fn receipt_descriptors_confine_writes_and_links_after_child_or_parent_replacement() {
        for replace_parent in [false, true] {
            let fixture = Fixture::new();
            let seed = fixture.capture(Provider::Codex, CodexConfig::Shared, None);
            let key = Uuid::new_v4();
            let mut retained = None;
            let result = fixture.homes.create_prepared_seeded_initialized(
                Provider::Codex,
                key,
                &seed,
                |created| {
                    let original = if replace_parent {
                        let parent = created.home().parent().unwrap();
                        let moved = parent.with_extension("original");
                        fs::rename(parent, &moved)?;
                        fs::create_dir(parent)?;
                        fs::create_dir(created.home())?;
                        moved.join(created.home().file_name().unwrap())
                    } else {
                        let moved = created.home().with_extension("original");
                        fs::rename(created.home(), &moved)?;
                        fs::create_dir(created.home())?;
                        moved
                    };
                    write_configuration_at(
                        created.directory(),
                        CODEX_CONFIG,
                        b"retained original fixture",
                        |file| file.sync_all(),
                    )?;
                    symlink_at(created.directory(), c"AGENTS.md", c"/fixture/instructions")?;
                    retained = Some(original);
                    assert!(created.revalidate().is_err());
                    Ok(())
                },
            );
            assert!(result.is_err());
            let retained = retained.unwrap();
            assert_eq!(
                fs::read(retained.join("config.toml")).unwrap(),
                b"retained original fixture"
            );
            assert_eq!(
                fs::read_link(retained.join("AGENTS.md")).unwrap(),
                Path::new("/fixture/instructions")
            );
            assert!(
                fs::read_dir(fixture.homes.path(Provider::Codex, key))
                    .unwrap()
                    .next()
                    .is_none()
            );
        }
    }

    #[test]
    fn destination_collision_never_overwrites_or_unlinks_a_file_or_symlink() {
        for collision_is_link in [false, true] {
            let fixture = Fixture::new();
            fs::write(fixture.system.join("config.toml"), "source fixture").unwrap();
            let seed = fixture.capture(Provider::Codex, CodexConfig::Copy, None);
            let target = fixture.root.path().join("untouched");
            fs::write(&target, "keep fixture").unwrap();
            let key = Uuid::new_v4();
            let result = fixture.homes.create_prepared_seeded_initialized(
                Provider::Codex,
                key,
                &seed,
                |created| {
                    if collision_is_link {
                        symlink(&target, created.home().join("config.toml"))
                    } else {
                        fs::write(created.home().join("config.toml"), "keep collision")
                    }
                },
            );
            assert!(result.is_err());
            assert_eq!(fs::read_to_string(&target).unwrap(), "keep fixture");
            let collision = fixture.homes.path(Provider::Codex, key).join("config.toml");
            if collision_is_link {
                assert_eq!(fs::read_link(collision).unwrap(), target);
            } else {
                assert_eq!(fs::read_to_string(collision).unwrap(), "keep collision");
            }
        }
    }

    #[test]
    fn injected_file_sync_failure_retains_written_bytes() {
        let fixture = Fixture::new();
        let seed = fixture.capture(Provider::Codex, CodexConfig::Shared, None);
        let key = Uuid::new_v4();
        let result = fixture.homes.create_prepared_seeded_initialized(
            Provider::Codex,
            key,
            &seed,
            |created| {
                write_configuration_at(
                    created.directory(),
                    CODEX_CONFIG,
                    b"written before simulated sync failure",
                    |file| {
                        assert_eq!(
                            file.metadata()?.len(),
                            b"written before simulated sync failure".len() as u64
                        );
                        Err(io::Error::other("fixture file-sync failure"))
                    },
                )
            },
        );
        assert!(result.is_err());
        assert_eq!(
            fs::read(fixture.homes.path(Provider::Codex, key).join("config.toml")).unwrap(),
            b"written before simulated sync failure"
        );
    }

    #[test]
    fn partial_destination_failure_keeps_uncertain_and_never_reseeds_on_replay() {
        let fixture = Fixture::new();
        fs::write(fixture.system.join("settings.json"), b"{\"fixture\":true}").unwrap();
        let global = fixture.system.join(".claude.json");
        fs::write(
            &global,
            r#"{"mcpServers":{"fixture":{"command":"fixture-mcp"}}}"#,
        )
        .unwrap();
        let seed = fixture.capture(Provider::Claude, CodexConfig::Shared, Some(&global));
        let store = Store::at(fixture.root.path().join("registry"));
        let planned = store
            .plan_preparation(
                Uuid::new_v4(),
                PreparationPayload {
                    provider: Provider::Claude,
                    copy_codex_config: false,
                },
            )
            .unwrap();
        let result = store.create_prepared(
            &fixture.homes,
            &planned.binding,
            true,
            |_| seed.revalidate(),
            || {
                fixture.homes.create_prepared_seeded_initialized(
                    Provider::Claude,
                    planned.binding.key,
                    &seed,
                    |created| {
                        fs::write(
                            created.home().join(".claude.json"),
                            b"retained collision fixture",
                        )
                    },
                )
            },
        );
        assert!(result.is_err());
        let registry = store.load().unwrap();
        assert_eq!(
            registry
                .preparation(planned.binding.request_id)
                .unwrap()
                .state,
            PreparationState::Uncertain
        );
        assert!(registry.pending().is_empty());
        let home = fixture.homes.path(Provider::Claude, planned.binding.key);
        assert_eq!(
            fs::read(home.join("settings.json")).unwrap(),
            b"{\"fixture\":true}"
        );
        assert_eq!(
            fs::read(home.join(".claude.json")).unwrap(),
            b"retained collision fixture"
        );
        let replay = store
            .create_prepared(
                &fixture.homes,
                &planned.binding,
                true,
                |_| panic!("consumed replay must skip validation"),
                || panic!("consumed replay must skip creation"),
            )
            .unwrap();
        assert_eq!(replay.state, PreparationState::Uncertain);
        assert!(store.load().unwrap().pending().is_empty());
        assert_eq!(
            fs::read(home.join("settings.json")).unwrap(),
            b"{\"fixture\":true}"
        );
    }
}
