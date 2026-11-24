use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, Read},
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

use super::Replacement;

const STAGE_PREFIX: &str = ".cagents-provider-update-";
const OWNER: &str = ".cagents-stage";
const RECOVERY: &str = ".cagents-update-incomplete";
const JOURNAL_LIMIT: u64 = 64 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
struct StagePath(#[serde(with = "crate::selected_file_path::required")] PathBuf);

#[derive(serde::Serialize, serde::Deserialize)]
enum Journal {
    Primary {
        provider: Provider,
        companions: Vec<StagePath>,
        replacements: Vec<Replacement>,
    },
    Companion {
        provider: Provider,
        primary: StagePath,
    },
}

impl Journal {
    fn provider(&self) -> Provider {
        match *self {
            Self::Primary { provider, .. } | Self::Companion { provider, .. } => provider,
        }
    }
}

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

    pub(super) fn finish_apply(&self) -> io::Result<()> {
        clear_journal(self.path())
    }
}

pub(super) fn begin_apply(
    stages: &[Stage],
    provider: Provider,
    replacements: &[Replacement],
) -> io::Result<()> {
    let (primary, companions) = stages
        .split_first()
        .ok_or_else(|| io::Error::other("provider update has no stage"))?;
    // The primary marker authorizes mutation. Write it last and remove it
    // first so an interrupted setup or cleanup leaves only disposable stages.
    for stage in companions {
        write_journal(
            stage.path(),
            &Journal::Companion {
                provider,
                primary: StagePath(primary.path().to_owned()),
            },
        )?;
    }
    write_journal(
        primary.path(),
        &Journal::Primary {
            provider,
            companions: companions
                .iter()
                .map(|stage| StagePath(stage.path().to_owned()))
                .collect(),
            replacements: replacements.to_vec(),
        },
    )
}

fn write_journal(path: &Path, journal: &Journal) -> io::Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path)?;
    serde_json::to_writer(&mut file, journal)?;
    file.persist_noclobber(path.join(RECOVERY))?;
    Ok(())
}

fn read_journal(path: &Path) -> io::Result<Option<Journal>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.join(RECOVERY))
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid() || metadata.len() > JOURNAL_LIMIT {
        return Err(io::Error::other("invalid recovery journal file"));
    }
    if metadata.len() == 0 {
        return Err(io::Error::other(
            "the older recovery marker contains no rollback journal",
        ));
    }
    serde_json::from_reader(file.take(JOURNAL_LIMIT))
        .map(Some)
        .map_err(io::Error::other)
}

fn staged_provider(path: &Path) -> Option<Provider> {
    let exists = |entry| fs::symlink_metadata(path.join(entry)).is_ok();
    [
        (Provider::Claude, "claude"),
        (Provider::Claude, ".local/share/claude"),
        (Provider::Codex, super::npm::SCOPE_DIRECTORY),
        (Provider::Muse, ".muse-version"),
        (Provider::Omp, ".local/bin/omp"),
        (Provider::Antigravity, ".local/bin/agy"),
    ]
    .into_iter()
    .find_map(|(provider, entry)| exists(entry).then_some(provider))
}

fn clear_journal(path: &Path) -> io::Result<()> {
    match fs::remove_file(path.join(RECOVERY)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
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

fn stage_paths(parent: &Path) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_stage(path))
}

fn is_stage(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.as_encoded_bytes().starts_with(STAGE_PREFIX.as_bytes()))
}

pub(super) fn collect_stages(parent: &Path) {
    for path in stage_paths(parent) {
        let clean = || -> io::Result<()> {
            if let Some(lease) = owned_lease(&path)?
                && fs::symlink_metadata(path.join(RECOVERY))
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
            {
                clean_stage(&path, &lease)?;
            }
            Ok(())
        };
        if let Err(error) = clean()
            && error.kind() != io::ErrorKind::WouldBlock
        {
            tracing::debug!(%error, path = %path.display(), "Could not clean up provider update staging files");
        }
    }
}

pub(super) fn recovery_issues(parent: &Path, provider: Provider) -> Vec<String> {
    stage_paths(parent)
        .filter_map(|path| {
            // Status samples recovery evidence without competing for the
            // stage lease. The caller discards samples that overlap a swap.
            let _directory = owned_directory(&path).ok()??;
            match read_journal(&path) {
                Ok(Some(journal)) if journal.provider() == provider => {
                    if let Journal::Primary { replacements, .. } = journal
                        && let Some(error) = replacements
                            .iter()
                            .find_map(|replacement| replacement.rollback_operation().err())
                    {
                        return Some(recovery_error(&path, &error));
                    }
                    Some(format!(
                        "Provider update recovery files found at {}",
                        path.display()
                    ))
                }
                Err(error) if staged_provider(&path).is_none_or(|owner| owner == provider) => {
                    Some(recovery_error(&path, &error))
                }
                _ => None,
            }
        })
        .collect()
}

pub(super) fn reap_stages(parent: &Path, provider: Provider) -> Vec<(io::ErrorKind, String)> {
    let mut issues = Vec::new();
    for path in stage_paths(parent) {
        let result = (|| -> io::Result<()> {
            let Some(lease) = owned_lease(&path)? else {
                return Ok(());
            };
            let journal = match read_journal(&path) {
                Err(_) if staged_provider(&path).is_some_and(|owner| owner != provider) => {
                    return Ok(());
                }
                result => result?,
            };
            match journal {
                None => clean_stage(&path, &lease),
                Some(journal) if journal.provider() != provider => Ok(()),
                Some(Journal::Primary {
                    companions,
                    replacements,
                    ..
                }) => recover_primary(&path, lease, provider, companions, replacements),
                Some(Journal::Companion {
                    primary: StagePath(primary),
                    ..
                }) => {
                    // Group recovery always takes the primary lease first.
                    drop(lease);
                    recover_companion(&path, &primary, provider)
                }
            }
        })();
        match result {
            Ok(()) => {}
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    && read_journal(&path).is_ok_and(|journal| {
                        journal.is_none_or(|journal| journal.provider() != provider)
                    }) => {}
            Err(error) if fs::symlink_metadata(path.join(RECOVERY)).is_ok() => {
                issues.push((error.kind(), recovery_error(&path, &error)));
            }
            Err(error) => {
                tracing::debug!(%error, path = %path.display(), "Could not clean up provider update staging files");
            }
        }
    }
    issues
}

fn recover_companion(path: &Path, primary: &Path, provider: Provider) -> io::Result<()> {
    validate_stage_path(primary)?;
    match owned_lease(primary)? {
        Some(lease) => match read_journal(primary)? {
            Some(Journal::Primary {
                provider: owner,
                companions,
                replacements,
            }) if provider == owner && companions.iter().any(|stage| stage.0 == path) => {
                return recover_primary(primary, lease, provider, companions, replacements);
            }
            None => {}
            Some(_) => {
                return Err(io::Error::other(
                    "the primary recovery journal does not match",
                ));
            }
        },
        None if fs::symlink_metadata(primary).is_ok() => {
            return Err(io::Error::other("the primary update stage is unavailable"));
        }
        None => {}
    }
    // No primary journal means that publication never began or had finished.
    if let Some(lease) = owned_lease(path)?
        && matches!(
            read_journal(path)?,
            Some(Journal::Companion { provider: owner, primary: StagePath(group) })
                if owner == provider && group == primary
        )
    {
        clean_stage(path, &lease)?;
    }
    Ok(())
}

fn recover_primary(
    primary: &Path,
    lease: File,
    provider: Provider,
    companions: Vec<StagePath>,
    replacements: Vec<Replacement>,
) -> io::Result<()> {
    let mut stages = vec![(primary, lease)];
    for companion in &companions {
        let path = &*companion.0;
        validate_stage_path(path)?;
        if stages.iter().any(|(stage, _)| *stage == path) {
            return Err(io::Error::other("duplicate stage in the recovery journal"));
        }
        let lease = owned_lease(path)?.ok_or_else(|| {
            io::Error::other(format!("update stage {} is unavailable", path.display()))
        })?;
        if !matches!(
            read_journal(path)?,
            Some(Journal::Companion { provider: owner, primary: StagePath(group) })
                if owner == provider && group == primary
        ) {
            return Err(io::Error::other(
                "the companion recovery journal does not match",
            ));
        }
        stages.push((path, lease));
    }
    // Check every path before mutating any of them. If another installer took
    // over, leave the original files available for repair.
    if replacements.is_empty() {
        return Err(io::Error::other(
            "the recovery journal contains no replacements",
        ));
    }
    let mut changed = Vec::new();
    for replacement in &replacements {
        let valid = stages.iter().any(|(stage, _)| {
            replacement.staged.starts_with(stage)
                && replacement.staged != *stage
                && replacement.live.parent() == stage.parent()
        }) && replacement.staged.parent().is_some_and(|parent| {
            parent
                .canonicalize()
                .is_ok_and(|physical| physical == parent)
        });
        if !valid {
            return Err(io::Error::other(
                "unexpected installation paths in the recovery journal",
            ));
        }
        if replacement.rollback_operation()?.is_some() {
            changed.push(replacement);
        }
    }
    ensure_idle(provider, &changed)?;
    for replacement in replacements.iter().rev() {
        replacement.restore()?;
    }
    clear_journal(primary)?;
    for (path, lease) in stages {
        if let Err(error) = clean_stage(path, &lease) {
            tracing::debug!(%error, path = %path.display(), "Could not remove a recovered provider update stage");
        }
    }
    Ok(())
}

fn ensure_idle(provider: Provider, changed: &[&Replacement]) -> io::Result<()> {
    if changed.is_empty() {
        return Ok(());
    }
    let in_use = match provider {
        Provider::Codex => {
            let roots = changed
                .iter()
                .filter(|entry| entry.candidate.mode & libc::S_IFMT == libc::S_IFDIR)
                .map(|entry| entry.live.as_path())
                .collect::<Vec<_>>();
            super::npm_running_in(&roots).map_err(io::Error::other)?
        }
        Provider::Omp | Provider::Antigravity => {
            let paths = changed
                .iter()
                .map(|entry| entry.live.as_path())
                .collect::<Vec<_>>();
            let stamps = changed
                .iter()
                .flat_map(|entry| std::iter::once(&entry.candidate).chain(entry.original.iter()))
                .collect::<Vec<_>>();
            super::standalone_running(&paths, &stamps)?
        }
        _ => false,
    };
    if in_use {
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "a running {} process is using this installation",
                provider.label()
            ),
        ))
    } else {
        Ok(())
    }
}

fn recovery_error(path: &Path, error: &io::Error) -> String {
    format!(
        "Cannot recover interrupted provider update at {} ({error})",
        path.display()
    )
}

fn validate_stage_path(path: &Path) -> io::Result<()> {
    if is_stage(path)
        && path.is_absolute()
        && path.parent().is_some_and(|parent| {
            parent
                .canonicalize()
                .is_ok_and(|physical| physical == parent)
        })
    {
        Ok(())
    } else {
        Err(io::Error::other("invalid update stage path"))
    }
}

fn owned_lease(path: &Path) -> io::Result<Option<File>> {
    let Some((lease, directory)) = owned_directory(path)? else {
        return Ok(None);
    };
    try_lock(&lease).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            io::Error::new(error.kind(), "the update stage is still in use")
        } else {
            error
        }
    })?;
    Ok((owned_marker(path)
        && fs::symlink_metadata(path).is_ok_and(|current| {
            current.dev() == directory.dev() && current.ino() == directory.ino()
        }))
    .then_some(lease))
}

fn owned_directory(path: &Path) -> io::Result<Option<(File, fs::Metadata)>> {
    let lease = match open_directory(path) {
        Ok(lease) => lease,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ENOTDIR) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let directory = lease.metadata()?;
    Ok(
        (directory.uid() == uid() && directory.mode() & 0o077 == 0 && owned_marker(path))
            .then_some((lease, directory)),
    )
}

fn owned_marker(path: &Path) -> bool {
    fs::symlink_metadata(path.join(OWNER))
        .is_ok_and(|marker| marker.is_file() && marker.uid() == uid())
}

fn clean_stage(path: &Path, lease: &File) -> io::Result<()> {
    let directory = lease.metadata()?;
    if fs::symlink_metadata(path)
        .is_ok_and(|current| current.dev() == directory.dev() && current.ino() == directory.ino())
    {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn uid() -> u32 {
    // SAFETY: geteuid takes no arguments and has no preconditions.
    unsafe { libc::geteuid() }
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
        File::create_new(recovery.path().join(RECOVERY)).unwrap();
        fs::create_dir_all(recovery.path().join(".local/bin")).unwrap();
        File::create_new(recovery.path().join(".local/bin/omp")).unwrap();
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

        assert!(recovery_issues(root.path(), Provider::Claude).is_empty());
        assert_eq!(recovery_issues(root.path(), Provider::Omp).len(), 1);
        let issues = reap_stages(root.path(), Provider::Omp);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].1.contains(&*recovery.to_string_lossy()));
        assert!(issues[0].1.contains("no rollback journal"));
        assert!(!stale.exists());
        assert!(active.path().is_dir());
        assert!(orphan.is_dir());
        assert!(recovery.is_dir());
        assert!(unmarked.is_dir());
        assert!(link.is_symlink());

        drop(child.stdin.take());
        assert!(child.wait().await.unwrap().success());
        assert_eq!(reap_stages(root.path(), Provider::Omp).len(), 1);
        assert!(!orphan.exists());
    }

    #[test]
    fn recovers_partial_publication_across_stages_from_the_companion() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        let bin = root.path().join("bin");
        fs::create_dir(&versions).unwrap();
        fs::create_dir(&bin).unwrap();
        let primary = stage_in(&versions).unwrap();
        let companion = stage_in(&bin).unwrap();
        let new_binary = versions.join("2.0.0");
        let staged_binary = primary.path().join("2.0.0");
        let live_launcher = bin.join("claude");
        let staged_launcher = companion.path().join("claude");
        fs::write(&staged_binary, b"new binary").unwrap();
        std::os::unix::fs::symlink(versions.join("1.0.0"), &live_launcher).unwrap();
        std::os::unix::fs::symlink(&new_binary, &staged_launcher).unwrap();
        let replacements = [
            Replacement::new(staged_binary.clone(), new_binary.clone()).unwrap(),
            Replacement::symlink(staged_launcher.clone(), live_launcher.clone()).unwrap(),
        ];
        let stages = [primary, companion];
        begin_apply(&stages, Provider::Claude, &replacements).unwrap();
        super::super::rename(&staged_binary, &new_binary, libc::RENAME_NOREPLACE).unwrap();
        // A crash can interrupt publication after the old entry was moved
        // but before the candidate is available at its live path.
        fs::remove_file(&staged_launcher).unwrap();
        fs::rename(&live_launcher, &staged_launcher).unwrap();
        let active = reap_stages(&bin, Provider::Claude);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, io::ErrorKind::WouldBlock);
        let [primary_path, companion_path] = stages.map(Stage::keep);
        assert!(recovery_issues(&bin, Provider::Omp).is_empty());
        assert_eq!(recovery_issues(&bin, Provider::Claude).len(), 1);
        assert!(reap_stages(&bin, Provider::Claude).is_empty());
        assert_eq!(
            fs::read_link(live_launcher).unwrap(),
            versions.join("1.0.0")
        );
        assert!(!new_binary.exists());
        assert!(!primary_path.exists());
        assert!(!companion_path.exists());
        assert!(reap_stages(&bin, Provider::Claude).is_empty());
    }

    #[test]
    fn retains_journal_and_saved_install_if_an_external_update_took_over() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("omp");
        let stage = stage_in(root.path()).unwrap();
        let staged = stage.path().join("omp");
        fs::write(&live, b"original").unwrap();
        fs::write(&staged, b"update").unwrap();
        let replacements = [Replacement::new(staged.clone(), live.clone()).unwrap()];
        begin_apply(std::slice::from_ref(&stage), Provider::Omp, &replacements).unwrap();
        super::super::rename(&staged, &live, libc::RENAME_EXCHANGE).unwrap();
        let external = root.path().join("external");
        fs::write(&external, b"external update").unwrap();
        fs::rename(external, &live).unwrap();
        let stage = stage.keep();

        let reports = recovery_issues(root.path(), Provider::Omp);
        assert_eq!(reports.len(), 1);
        assert!(reports[0].contains("live or saved installation"));
        let issues = reap_stages(root.path(), Provider::Omp);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].1.contains(&*stage.to_string_lossy()));
        assert!(issues[0].1.contains("live or saved installation"));
        assert!(stage.join(RECOVERY).is_file());
        assert_eq!(fs::read(&staged).unwrap(), b"original");
        assert_eq!(fs::read(&live).unwrap(), b"external update");
    }
}
