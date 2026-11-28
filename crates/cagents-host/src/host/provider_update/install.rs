use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
};

use semver::Version;

use crate::{
    account::provider_install::{ProviderInstallMethod, ProviderStatus, ProviderUpdateResult},
    agent::Provider,
    instance::OwnedLock,
    secure_file,
};

use super::release::parse_version;

const ENTRY: &[u8] = b"#!/bin/sh\n\
    target=$(/usr/bin/readlink -- \"${0%/*}/.claude-executable\") || exit 1\n\
    DISABLE_UPDATES=1 exec \"$target\" \"$@\"\n";

pub(super) struct Install {
    pub(super) root: PathBuf,
}

impl Install {
    pub(super) fn discover() -> Option<Self> {
        crate::instance::project_dirs().map(|dirs| Self {
            root: dirs.data_dir().join("provider-clis").join("claude"),
        })
    }

    pub(super) fn launcher(&self) -> PathBuf {
        self.root.join("bin/claude")
    }

    fn selection(&self) -> PathBuf {
        self.root.join("bin/.claude-executable")
    }

    fn versions(&self) -> PathBuf {
        self.root.join("versions")
    }

    pub(super) fn active(&self) -> io::Result<Option<PathBuf>> {
        secure_file::reject_symlink_components(&self.root)?;
        for dir in [self.root.join("bin"), self.versions()] {
            match secure_file::validate_owned_directory(&dir) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        let target = match fs::read_link(self.selection()) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some(version) = target
            .parent()
            .filter(|parent| parent.parent() == Some(self.versions().as_path()))
            .and_then(Path::file_name)
            .and_then(OsStr::to_str)
        else {
            return Err(io::Error::other("Invalid managed Claude Code selection"));
        };
        if target.file_name() != Some(OsStr::new("claude")) || parse_version(version).is_err() {
            return Err(io::Error::other("Invalid managed Claude Code selection"));
        }
        secure_file::validate_owned_directory(target.parent().unwrap())?;
        owned_executable(&target)?;
        Ok(Some(target))
    }

    pub(super) fn selected(&self, executable: Option<&Path>) -> bool {
        // Compare the entry rather than its resolved target. An alias to a
        // particular version is a pin (even if it happens to be current).
        executable == Some(self.launcher().as_path())
    }

    pub(super) fn lock(&self) -> io::Result<OwnedLock> {
        secure_file::prepare_private_directory(&self.root)?;
        let file = secure_file::open_private_append_file(&self.root.join("update.lock"))?;
        crate::instance::owned_file_lock(file, libc::LOCK_EX | libc::LOCK_NB)
    }

    pub(super) fn stage(&self) -> io::Result<tempfile::TempDir> {
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_name().as_encoded_bytes().starts_with(b".stage-")
                && entry.file_type()?.is_dir()
            {
                // Every producer owns the same update lock. These belong to
                // an interrupted check and cannot contain a selected release.
                fs::remove_dir_all(entry.path())?;
            }
        }
        tempfile::Builder::new()
            .prefix(".stage-")
            .tempdir_in(&self.root)
    }

    pub(super) fn materialize(
        &self,
        stage: &Path,
        verified: &Path,
        version: &Version,
    ) -> io::Result<PathBuf> {
        secure_file::prepare_private_directory(&self.versions())?;
        let release = self.versions().join(version.to_string());
        secure_file::prepare_private_directory(&release)?;
        let binary = release.join("claude");
        File::open(verified)?.sync_all()?;
        match fs::hard_link(verified, &binary) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                owned_executable(&binary)?;
                // An earlier check may have stopped after staging. Reuse
                // exactly the verified bytes, with no in-place replacement.
                if !same_file(verified, &binary)? {
                    return Err(io::Error::other(format!(
                        "Managed Claude Code {version} already exists with different bytes"
                    )));
                }
            }
            Err(error) => return Err(error),
        }
        fs::remove_file(verified)?;
        File::open(&release)?.sync_all()?;
        File::open(self.versions())?.sync_all()?;
        File::open(stage)?.sync_all()?;
        Ok(binary)
    }

    pub(super) fn validate_launcher(&self) -> io::Result<()> {
        let launcher = self.launcher();
        owned_executable(&launcher)?;
        let mut contents = Vec::with_capacity(ENTRY.len() + 1);
        File::open(launcher)?
            .take(ENTRY.len() as u64 + 1)
            .read_to_end(&mut contents)?;
        if contents != ENTRY {
            return Err(io::Error::other(
                "The managed Claude Code launcher was replaced",
            ));
        }
        Ok(())
    }

    /// Begin exposing the first installation only after the new link is valid.
    /// Later updates only rename the selector. A new invocation reads a whole
    /// version path and the binaries behind prior invocations stay on disk.
    pub(super) fn activate(&self, stage: &Path, binary: &Path) -> io::Result<()> {
        let bin = self.root.join("bin");
        secure_file::prepare_private_directory(&bin)?;
        let launcher = self.launcher();
        let create_launcher = match fs::symlink_metadata(&launcher) {
            Ok(_) => {
                self.validate_launcher()?;
                false
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(error) => return Err(error),
        };
        let pending_launcher = stage.join("launcher");
        if create_launcher {
            fs::write(&pending_launcher, ENTRY)?;
            fs::set_permissions(&pending_launcher, fs::Permissions::from_mode(0o500))?;
            File::open(&pending_launcher)?.sync_all()?;
        }
        let selection = self.selection();
        let pending_link = stage.join("selection");
        let rollback_link = stage.join("previous-selection");
        let previous = match fs::read_link(&selection) {
            Ok(previous) => {
                symlink(previous, &rollback_link)?;
                true
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        symlink(binary, &pending_link)?;
        let bin = File::open(bin)?;
        File::open(&self.root)?.sync_all()?;
        File::open(stage)?.sync_all()?;
        fs::rename(pending_link, &selection)?;

        let mut published_launcher = false;
        let synced = (|| {
            if create_launcher {
                fs::hard_link(pending_launcher, &launcher)?;
                published_launcher = true;
            }
            bin.sync_all()
        })();
        if let Err(error) = synced {
            let rollback = (|| {
                if published_launcher {
                    fs::remove_file(launcher)?;
                }
                if previous {
                    fs::rename(rollback_link, selection)?;
                } else {
                    fs::remove_file(selection)?;
                }
                bin.sync_all()
            })();
            return Err(match rollback {
                Ok(()) => error,
                Err(rollback) => io::Error::other(format!(
                    "Could not activate Claude Code ({error}) or restore the prior selection ({rollback})"
                )),
            });
        }
        Ok(())
    }

    pub(super) fn status(&self, binary: PathBuf) -> ProviderStatus {
        ProviderStatus {
            provider: Provider::Claude,
            executable: Some(self.launcher()),
            resolved_executable: Some(binary),
            version: None,
            install_method: ProviderInstallMethod::CagentsManaged,
            last_checked_at: None,
            last_result: None,
            error: None,
        }
    }

    pub(super) fn history(&self, status: &mut ProviderStatus) -> io::Result<()> {
        let file = match secure_file::open_existing_private_file_readonly(
            &self.root.join("last-check.json"),
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let history: Check = serde_json::from_reader(file.take(16 * 1024))?;
        status.last_checked_at = history.checked_at;
        status.last_result = history.result;
        if status.error.is_none() {
            status.error = history.detail;
        }
        Ok(())
    }

    pub(super) fn catalog_is_current(&self, version: &str) -> bool {
        let stamp = self.root.join("catalog-version");
        let mut stored = String::new();
        secure_file::open_existing_private_file_readonly(&stamp)
            .and_then(|file| file.take(65).read_to_string(&mut stored))
            .is_ok()
            && stored == version
    }

    pub(super) fn record_catalog_version(&self, version: &str) -> io::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(version.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(self.root.join("catalog-version"))?;
        File::open(&self.root)?.sync_all()
    }

    pub(super) fn save(&self, status: &ProviderStatus) -> io::Result<()> {
        let check = Check {
            checked_at: status.last_checked_at,
            result: status.last_result,
            detail: status.error.clone(),
        };
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut file, &check)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(self.root.join("last-check.json"))?;
        File::open(&self.root)?.sync_all()
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Check {
    checked_at: Option<u64>,
    result: Option<ProviderUpdateResult>,
    detail: Option<String>,
}

fn owned_executable(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    // SAFETY: geteuid takes no arguments and has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o777 != 0o500
        || metadata.len() == 0
    {
        return Err(io::Error::other(
            "Expected an owned, read-only Claude Code executable",
        ));
    }
    Ok(())
}

fn same_file(first: &Path, second: &Path) -> io::Result<bool> {
    let mut first = File::open(first)?;
    let mut second = File::open(second)?;
    if first.metadata()?.len() != second.metadata()?.len() {
        return Ok(false);
    }
    let mut left = [0; 32 * 1024];
    let mut right = [0; 32 * 1024];
    loop {
        let read = first.read(&mut left)?;
        if read == 0 {
            return Ok(true);
        }
        second.read_exact(&mut right[..read])?;
        if left[..read] != right[..read] {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn materialize(install: &Install, stage: &Path, version: Version, script: &str) -> PathBuf {
        let staged = stage.join("claude");
        fs::write(&staged, script).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o500)).unwrap();
        install.materialize(stage, &staged, &version).unwrap()
    }

    #[test]
    fn interrupted_staging_preserves_launches_and_never_overwrites_a_version() {
        let root = crate::fixture_tempdir().unwrap();
        let install = Install {
            root: root.path().join("managed space"),
        };
        let lock = install.lock().unwrap();
        assert_eq!(
            install.lock().err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        let stage = install.stage().unwrap();
        let v1 = Version::new(1, 0, 0);
        let v2 = Version::new(2, 0, 0);
        let first = materialize(
            &install,
            stage.path(),
            v1.clone(),
            "#!/bin/sh\necho first\n",
        );
        install.activate(stage.path(), &first).unwrap();
        drop(stage);

        let launch = || {
            let output = Command::new(install.launcher()).output().unwrap();
            assert!(output.status.success(), "{output:?}");
            output.stdout
        };
        assert_eq!(launch(), b"first\n");
        let stage = install.stage().unwrap();
        let second = materialize(
            &install,
            stage.path(),
            v2.clone(),
            "#!/bin/sh\necho second\n",
        );
        let abandoned = stage.keep();
        drop(lock);

        let _retry = install.lock().unwrap();
        let stage = install.stage().unwrap();
        assert!(!abandoned.exists());
        assert_eq!(install.active().unwrap(), Some(first.clone()));
        assert_eq!(launch(), b"first\n");
        assert_eq!(
            materialize(&install, stage.path(), v2, "#!/bin/sh\necho second\n"),
            second
        );
        install.activate(stage.path(), &second).unwrap();
        assert_eq!(launch(), b"second\n");
        assert_eq!(Command::new(first).output().unwrap().stdout, b"first\n");
    }
}
