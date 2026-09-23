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
//! history, caches and per-account state - stays the account's own.
//!
//! An entry is only on the list once the CLI has been seen to keep the link:
//!
//! - Codex writes `config.toml` through a link (verified with `codex features
//!   enable` on codex-cli 0.155.1: the link stayed and its target changed),
//!   so an account's configuration edits land in the reader's one file.
//! - Claude Code 2.1.280 refuses to write its settings through a link (its
//!   atomic writer opens the destination with `O_NOFOLLOW` and does not fall
//!   back), so `settings.json` is *not* linked: a linked copy would make every
//!   settings change in that account fail. Its instruction file and
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
//! the path is rebuilt from the account's key rather than taken from
//! anywhere, every component of it is checked not to be a link, the directory
//! must be a real one owned by this user, and the standard library's
//! `remove_dir_all` unlinks symbolic links inside the tree rather than
//! descending into them - so the shared entries above are unlinked, not
//! emptied.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use uuid::Uuid;

use crate::agent::Provider;

/// Where a removal parks a home while it is deleted, so a removal that is
/// interrupted leaves a name nothing else uses, and the next start finishes
/// it.
const REMOVING_PREFIX: &str = ".removing-";
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
            link_shared(provider, system_home, &path);
        }
        Ok((key, path))
    }

    /// Deletes the home of account `key`. A home that is already gone is not
    /// an error: removing an account twice leaves it just as removed.
    pub fn remove(&self, provider: Provider, key: Uuid) -> io::Result<()> {
        let path = self.path(provider, key);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        crate::secure_file::reject_symlink_components(&path)?;
        crate::secure_file::validate_owned_directory(&path)?;
        let parked = self
            .provider_directory(provider)
            .join(format!("{REMOVING_PREFIX}{}", key.hyphenated()));
        fs::rename(&path, &parked)?;
        remove_parked(&parked)
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
        // Claude refuses to write settings through a link, so they are not
        // shared, and nothing the system home lacks is linked.
        assert!(fs::symlink_metadata(home.join("settings.json")).is_err());
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
