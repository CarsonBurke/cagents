use std::os::unix::fs::symlink;

use super::*;

/// Claude updates its own symlink and keeps binaries under immutable version
/// names. Hold a link to the prior executable in case its cleanup removed it.
pub(super) struct Backup {
    stage: Option<TempDir>,
    launcher: PathBuf,
    previous_link: PathBuf,
    previous_binary: PathBuf,
    previous_stamp: EntryStamp,
    armed: bool,
}

pub(super) fn prepare(
    status: &ProviderStatus,
    search_path: Option<&OsStr>,
) -> Result<Preparation, String> {
    let base = directories::BaseDirs::new().ok_or("Cannot find the Claude installation home")?;
    let directory = base
        .home_dir()
        .join(".local/bin")
        .canonicalize()
        .map_err(fs_error)?;
    prepare_for_launcher(status, directory.join("claude"), search_path)
}

fn prepare_for_launcher(
    status: &ProviderStatus,
    launcher: PathBuf,
    search_path: Option<&OsStr>,
) -> Result<Preparation, String> {
    let previous_link = fs::read_link(&launcher).map_err(fs_error)?;
    let previous_binary = launcher.canonicalize().map_err(fs_error)?;
    if status.resolved_executable.as_ref() != Some(&previous_binary) {
        return Err("Claude's native launcher changed during its update check".into());
    }
    let stage = stage_in(parent(parent(&previous_binary)?)?)?;
    fs::hard_link(&previous_binary, stage.path().join("previous")).map_err(fs_error)?;
    // Creating a hard link changes ctime. Read the identity after pinning the
    // previous binary but before invoking the native updater.
    let installation = SelectedInstall::capture(status, search_path)?;
    let previous_stamp = EntryStamp::read(&launcher)
        .map_err(fs_error)?
        .ok_or("Claude's native launcher is missing")?;
    let backup = Backup {
        stage: Some(stage),
        launcher,
        previous_link,
        previous_binary,
        previous_stamp,
        armed: false,
    };
    Ok(Preparation::Ready(PreparedUpdate {
        installation,
        activation: Box::new(Activation::Claude(backup)),
        muse_lock: None,
    }))
}

impl Backup {
    pub(super) async fn update(
        mut self,
        installation: &SelectedInstall,
        context: &CommandContext<'_>,
        cancelled: impl Fn() -> bool,
    ) -> Result<Applied, String> {
        if EntryStamp::read(&self.launcher).map_err(fs_error)?.as_ref()
            != Some(&self.previous_stamp)
        {
            return Err("Claude's native launcher changed during its update check".into());
        }
        let mut command = context.command(&self.launcher);
        command.arg("update");
        self.armed = true;
        let update = async {
            let (status, _) = crate::account::probe::run_provider_command_cancellable(
                command,
                UPDATE_TIMEOUT,
                cancelled,
            )
            .await?;
            if !status.success() {
                return Err(format!("Claude's updater exited with {status}"));
            }
            let updated = self.launcher.canonicalize().map_err(fs_error)?;
            if updated.parent() != self.previous_binary.parent()
                || installation.executable.canonicalize().map_err(fs_error)? != updated
            {
                return Err(
                    "Claude's update did not select a binary in its versions directory".into(),
                );
            }
            let version = installation.version(context.search_path).await?;
            if updated != self.previous_binary {
                let selected = updated.file_name().and_then(OsStr::to_str);
                if selected != Some(version.as_str()) {
                    return Err(format!(
                        "Claude's native update selected {} but reports {version}",
                        updated.display()
                    ));
                }
            }
            Ok(if updated == self.previous_binary {
                Applied::Current(version)
            } else {
                Applied::Updated(version)
            })
        }
        .await;

        if update.is_ok() {
            self.armed = false;
            return update;
        }
        match (update, self.restore()) {
            (Err(error), Err(restore)) => Err(format!("{error}. {restore}")),
            (result, _) => result,
        }
    }

    fn restore(&mut self) -> Result<(), String> {
        if !std::mem::take(&mut self.armed) {
            return Ok(());
        }
        let Some(stage) = self.stage.take() else {
            return Ok(());
        };
        let backup = stage.path().join("previous");
        let restore = || -> io::Result<()> {
            let previous = EntryStamp::read(&self.previous_binary)?;
            let saved = EntryStamp::read(&backup)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Previous Claude binary is missing")
            })?;
            if !previous.is_some_and(|previous| previous.same_file(&saved)) {
                // Keep the backup until the previous launcher is restored.
                let old_path = stage.path().join("restore");
                fs::hard_link(&backup, &old_path)?;
                fs::rename(&old_path, &self.previous_binary)?;
            }
            if fs::read_link(&self.launcher).ok().as_ref() != Some(&self.previous_link) {
                let directory = self
                    .launcher
                    .parent()
                    .expect("native launcher has a parent");
                let link_stage = tempfile::tempdir_in(directory)?;
                let old_link = link_stage.path().join("claude");
                symlink(&self.previous_link, &old_link)?;
                fs::rename(old_link, &self.launcher)?;
            }
            Ok(())
        };
        restore().map_err(|error| {
            format!(
                "Cannot restore the previous Claude launcher: {error}. Backup retained at {}",
                stage.keep().display()
            )
        })
    }
}

impl Drop for Backup {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            tracing::error!(%error, "Could not restore the previous Claude launcher");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::provider_update::install::tests::{
        fixture_path, fixture_status, write_executable,
    };

    #[tokio::test]
    async fn rollback_reinstates_native_symlink_and_pruned_previous_binary() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("share/claude/versions");
        fs::create_dir_all(&versions).unwrap();
        let binary = versions.join("1.0.0");
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let launcher = bin.join("claude");
        symlink(&binary, &launcher).unwrap();
        write_executable(
            &binary,
            &format!(
                r#"#!/bin/sh
case "$1" in
    --version) echo '1.0.0 (Claude Code)' ;;
    update)
        rm "{}"
        printf '#!/bin/sh\nexit 9\n' > "{}"
        chmod +x "{}"
        ln -sfn "{}" "$0"
        ;;
    *) exit 1 ;;
esac
"#,
                binary.display(),
                versions.join("2.0.0").display(),
                versions.join("2.0.0").display(),
                versions.join("2.0.0").display(),
            ),
        );

        let status = fixture_status(Provider::Claude, &launcher, ProviderInstallMethod::Native);
        let path = fixture_path(&bin);
        let Preparation::Ready(update) =
            prepare_for_launcher(&status, launcher.clone(), Some(&path)).unwrap()
        else {
            panic!("expected staged Claude update");
        };
        assert!(update.apply(Some(&path), || false).await.is_err());
        assert_eq!(launcher.canonicalize().unwrap(), binary);
        assert_eq!(
            read_version(Provider::Claude, &launcher, Some(&path))
                .await
                .unwrap(),
            "1.0.0"
        );
    }
}
