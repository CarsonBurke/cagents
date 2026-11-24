use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
};

use tempfile::TempDir;
use tokio::process::Command;

use crate::{account::probe::provider_version_with, agent::Provider};

const STAGE_PREFIX: &str = ".cagents-provider-update-";
const OWNER: &str = ".cagents-stage";
const RECOVERY: &str = ".cagents-update-incomplete";

pub(super) struct Stage {
    directory: TempDir,
    lease: Arc<File>,
}

impl Stage {
    pub(super) fn path(&self) -> &Path {
        self.directory.path()
    }

    pub(super) fn keep(self) -> PathBuf {
        self.directory.keep()
    }

    /// Inherit the directory lease so a daemon crash cannot expose the
    /// updater's working directory to another check while it is still in use.
    pub(super) fn protect(&self, command: &mut Command) {
        let lease = Arc::clone(&self.lease);
        // SAFETY: The callback owns the lease and adjusts descriptor flags
        // only in the forked child.
        unsafe {
            command.pre_exec(move || {
                let lease = lease.as_raw_fd();
                let flags = libc::fcntl(lease, libc::F_GETFD);
                if flags == -1 || libc::fcntl(lease, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    pub(super) async fn version(
        &self,
        provider: Provider,
        executable: &Path,
        search_path: Option<&OsStr>,
    ) -> Result<String, String> {
        provider_version_with(provider, executable, search_path, |command| {
            self.protect(command);
        })
        .await
    }

    pub(super) fn begin_apply(&self) -> io::Result<()> {
        File::create_new(self.path().join(RECOVERY)).map(drop)
    }

    pub(super) fn finish_apply(&self) {
        if let Err(error) = fs::remove_file(self.path().join(RECOVERY))
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%error, path = %self.path().display(), "Could not clear the provider update recovery marker");
        }
    }
}

pub(super) fn stage_in(parent: &Path) -> Result<Stage, String> {
    let create = || -> io::Result<Stage> {
        let directory = tempfile::Builder::new()
            .prefix(STAGE_PREFIX)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)?;
        let lease = open_directory(directory.path())?;
        try_lock(&lease)?;
        File::create_new(directory.path().join(OWNER))?;
        Ok(Stage {
            directory,
            lease: Arc::new(lease),
        })
    };
    create().map_err(|error| {
        format!(
            "Cannot stage provider update at {}: {error}",
            parent.display()
        )
    })
}

pub(super) fn reap_stages(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten().filter(|entry| {
        entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(STAGE_PREFIX.as_bytes())
    }) {
        let path = entry.path();
        if let Err(error) = reap(&path) {
            tracing::debug!(%error, path = %path.display(), "Could not clean up provider update staging files");
        }
    }
}

fn reap(path: &Path) -> io::Result<()> {
    let lease = open_directory(path)?;
    let directory = lease.metadata()?;
    // SAFETY: geteuid takes no arguments and has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let owned = || {
        fs::symlink_metadata(path.join(OWNER))
            .is_ok_and(|marker| marker.is_file() && marker.uid() == uid)
    };
    if directory.uid() != uid || directory.mode() & 0o077 != 0 || !owned() {
        return Ok(());
    }
    try_lock(&lease)?;
    if owned()
        && fs::symlink_metadata(path.join(RECOVERY))
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        && fs::symlink_metadata(path).is_ok_and(|current| {
            current.dev() == directory.dev() && current.ino() == directory.ino()
        })
    {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

fn try_lock(file: &File) -> io::Result<()> {
    // SAFETY: flock receives a live descriptor and no pointers.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use super::*;

    #[tokio::test]
    async fn only_collects_owned_unleased_stages_without_recovery_data() {
        let root = tempfile::tempdir().unwrap();
        let active = stage_in(root.path()).unwrap();
        let stale = stage_in(root.path()).unwrap().keep();
        let recovery = stage_in(root.path()).unwrap();
        recovery.begin_apply().unwrap();
        let recovery = recovery.keep();
        let unmarked = root.path().join(format!("{STAGE_PREFIX}unmarked"));
        fs::create_dir(&unmarked).unwrap();
        let link = root.path().join(format!("{STAGE_PREFIX}link"));
        std::os::unix::fs::symlink(&stale, &link).unwrap();
        let orphan = stage_in(root.path()).unwrap();
        let mut command = Command::new("/bin/cat");
        command.stdin(Stdio::piped()).kill_on_drop(true);
        orphan.protect(&mut command);
        let mut child = command.spawn().unwrap();
        drop(command);
        // Simulate the daemon disappearing before its update subprocess.
        let orphan = orphan.keep();

        reap_stages(root.path());
        assert!(!stale.exists());
        assert!(active.path().is_dir());
        assert!(orphan.is_dir());
        assert!(recovery.is_dir());
        assert!(unmarked.is_dir());
        assert!(link.is_symlink());

        drop(child.stdin.take());
        assert!(child.wait().await.unwrap().success());
        reap_stages(root.path());
        assert!(!orphan.exists());
    }
}
