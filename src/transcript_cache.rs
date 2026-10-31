//! Transcript cache and retained semantic-journal ownership, shared with diagnostics.
//! A creation gate closes the mkdir/lease race; per-cache leases protect other daemons.
#[cfg(feature = "host")]
mod actor_journal;
#[cfg(feature = "host")]
pub(crate) use actor_journal::ActorJournalRetention;
use std::{
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
#[cfg(feature = "host")]
use std::{
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt},
    },
};

#[cfg(feature = "host")]
const LEASE: &str = "lease.lock";

#[derive(Debug, Default, serde::Serialize)]
pub struct Usage {
    pub count: u64,
    pub bytes: u64,
}

#[cfg(feature = "host")]
fn private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.nlink() == 0 {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }
    if !metadata.is_file()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe transcript lease",
        ));
    }
    Ok(file)
}

#[cfg(feature = "host")]
fn lock(file: &File, operation: i32) -> io::Result<()> {
    // SAFETY: the descriptor remains live and flock accepts scalar flags.
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(feature = "host")]
pub(crate) fn create(root: &Path) -> io::Result<(PathBuf, File)> {
    let gate = private_file(&root.join("transcript-cache.lock"))?;
    lock(&gate, libc::LOCK_SH)?;
    let directory = root.join(format!("chat-{}", uuid::Uuid::new_v4()));
    fs::DirBuilder::new().mode(0o700).create(&directory)?;
    let result = (|| {
        let lease = private_file(&directory.join(LEASE))?;
        lock(&lease, libc::LOCK_EX | libc::LOCK_NB)?;
        Ok((directory.clone(), lease))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&directory);
    }
    result
}

/// The caller owns this cache's lease. A dead helper's semantic journal is
/// evidence, not disposable transcript data, until explicitly sealed/drained.
#[cfg(feature = "host")]
pub(crate) fn remove_if_reclaimable(directory: &Path) -> io::Result<bool> {
    if !actor_journal::reclaimable(directory)? {
        return Ok(false);
    }
    match fs::remove_dir_all(directory) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn cache_directories(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(directories),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_prefix("chat-")) else {
            continue;
        };
        if uuid::Uuid::parse_str(id).is_err() {
            continue;
        }
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.is_dir() && metadata.uid() == crate::daemon::current_uid() {
            directories.push(entry.path());
        }
    }
    Ok(directories)
}

/// Never follows symlinks and never opens SQLite or contacts a daemon.
pub fn usage(root: &Path) -> io::Result<Usage> {
    let mut usage = Usage::default();
    for directory in cache_directories(root)? {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        usage.count += 1;
        for entry in entries {
            let metadata = match fs::symlink_metadata(entry?.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if metadata.is_file() {
                usage.bytes = usage.bytes.saturating_add(metadata.len());
            }
        }
    }
    Ok(usage)
}

/// Legacy caches predate leases. Do not reclaim them if any same-user process
/// has a file open beneath them, or if procfs cannot establish that safely.
#[cfg(feature = "host")]
fn legacy_open_directories(
    root: &Path,
    proc_root: &Path,
) -> io::Result<std::collections::HashSet<PathBuf>> {
    // Linux renders FD targets using resolved filesystem paths. RuntimePaths
    // intentionally preserves the caller's spelling, including symlinked
    // parents and `..`; compare against the same resolved root identity.
    // A resolution error propagates so the caller conservatively keeps legacy
    // caches instead of interpreting a failed census as proof of abandonment.
    let canonical_root = fs::canonicalize(root)?;
    let mut open = std::collections::HashSet::new();
    for process in fs::read_dir(proc_root)? {
        let process = process?;
        if process
            .file_name()
            .to_str()
            .is_none_or(|name| name.parse::<u32>().is_err())
        {
            continue;
        }
        let metadata = match process.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.uid() != crate::daemon::current_uid() {
            continue;
        }
        let fds = match fs::read_dir(process.path().join("fd")) {
            Ok(fds) => fds,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for fd in fds {
            let target = match fs::read_link(fd?.path()) {
                Ok(target) => target,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if let Ok(relative) = target.strip_prefix(&canonical_root)
                && let Some(component) = relative.components().next()
            {
                open.insert(root.join(component));
            }
        }
    }
    Ok(open)
}

#[cfg(feature = "host")]
pub(crate) fn sweep(root: &Path) -> io::Result<Usage> {
    sweep_with_procfs(root, Path::new("/proc"))
}

#[cfg(feature = "host")]
fn sweep_with_procfs(root: &Path, proc_root: &Path) -> io::Result<Usage> {
    let gate = private_file(&root.join("transcript-cache.lock"))?;
    lock(&gate, libc::LOCK_EX)?;
    let directories = cache_directories(root)?;
    let legacy_open = if directories.iter().any(|path| !path.join(LEASE).exists()) {
        // Failure is conservative: leave legacy caches for a later startup.
        legacy_open_directories(root, proc_root).ok()
    } else {
        Some(Default::default())
    };
    let mut removed = Usage::default();
    for directory in directories {
        if !directory.join(LEASE).exists()
            && legacy_open
                .as_ref()
                .is_none_or(|open| open.contains(&directory))
        {
            continue;
        }
        let lease = match private_file(&directory.join(LEASE)) {
            Ok(lease) => lease,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        match lock(&lease, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
        if !actor_journal::reclaimable(&directory)? {
            continue;
        }
        let mut entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let bytes = entries.try_fold(0u64, |total, entry| {
            let metadata = match fs::symlink_metadata(entry?.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(total),
                Err(error) => return Err(error),
            };
            Ok::<_, io::Error>(total.saturating_add(if metadata.is_file() {
                metadata.len()
            } else {
                0
            }))
        })?;
        match fs::remove_dir_all(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        removed.count += 1;
        removed.bytes = removed.bytes.saturating_add(bytes);
    }
    Ok(removed)
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;

    #[test]
    fn sweep_reclaims_orphans_and_preserves_live_and_legacy_open_caches() {
        let root = tempfile::tempdir().unwrap();
        let (live, lease) = create(root.path()).unwrap();
        fs::write(live.join("transcript.sqlite"), b"live").unwrap();
        let (orphan, abandoned) = create(root.path()).unwrap();
        fs::write(orphan.join("transcript.sqlite"), b"orphan").unwrap();
        drop(abandoned); // Represents process exit closing its lease without cleanup.
        let legacy = root.path().join(format!("chat-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&legacy).unwrap();
        let held = File::create(legacy.join("transcript.sqlite")).unwrap();
        // A deterministic procfs fixture: the host may have same-user
        // nondumpable processes whose fd directories cannot be inspected.
        let proc_root = tempfile::tempdir().unwrap();
        let fd = proc_root.path().join("123/fd");
        fs::create_dir_all(&fd).unwrap();
        std::os::unix::fs::symlink(
            fs::canonicalize(&legacy).unwrap().join("transcript.sqlite"),
            fd.join("4"),
        )
        .unwrap();
        let removed = sweep_with_procfs(root.path(), proc_root.path()).unwrap();
        assert_eq!(removed.count, 1);
        assert!(!orphan.exists());
        assert!(legacy.exists());
        assert_eq!(fs::read(live.join("transcript.sqlite")).unwrap(), b"live");
        drop(held);
        fs::remove_file(fd.join("4")).unwrap();
        assert_eq!(
            sweep_with_procfs(root.path(), proc_root.path())
                .unwrap()
                .count,
            1
        );
        assert!(!legacy.exists());
        assert_eq!(usage(root.path()).unwrap().count, 1);
        drop(lease);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn aliased_runtime_roots_preserve_real_open_legacy_files() {
        for symlinked in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let parent = fixture.path().join("actual");
            let root = parent.join("runtime");
            fs::create_dir_all(&root).unwrap();
            let alias = if symlinked {
                let link = fixture.path().join("alias-parent");
                std::os::unix::fs::symlink(&parent, &link).unwrap();
                link.join("runtime")
            } else {
                root.join("..").join("runtime")
            };
            let live_name = format!("chat-{}", uuid::Uuid::new_v4());
            let abandoned_name = format!("chat-{}", uuid::Uuid::new_v4());
            fs::create_dir(root.join(&live_name)).unwrap();
            fs::create_dir(root.join(&abandoned_name)).unwrap();
            let held = File::create(alias.join(&live_name).join("transcript.sqlite")).unwrap();
            // Use the actual kernel FD census for this process alone, making
            // the test independent of unrelated nondumpable host processes.
            let proc_root = fixture.path().join("proc");
            fs::create_dir_all(proc_root.join("123")).unwrap();
            std::os::unix::fs::symlink(
                format!("/proc/{}/fd", std::process::id()),
                proc_root.join("123/fd"),
            )
            .unwrap();
            let removed = sweep_with_procfs(&alias, &proc_root).unwrap();
            assert_eq!(
                removed.count, 1,
                "an unrelated abandoned legacy directory is reclaimed"
            );
            assert!(!root.join(&abandoned_name).exists());
            assert!(
                root.join(&live_name).exists(),
                "the real open FD protects either alias spelling"
            );
            drop(held);
            assert_eq!(sweep_with_procfs(&alias, &proc_root).unwrap().count, 1);
            assert!(!root.join(&live_name).exists());
        }
    }

    #[test]
    fn uncertain_legacy_liveness_preserves_history_without_blocking_leased_orphans() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(format!("chat-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&legacy).unwrap();
        fs::write(legacy.join("transcript.sqlite"), b"unproven owner").unwrap();
        let (orphan, lease) = create(root.path()).unwrap();
        drop(lease);
        let unavailable_procfs = root.path().join("not-a-proc-directory");
        fs::write(&unavailable_procfs, b"inaccessible scan fixture").unwrap();
        assert_eq!(
            sweep_with_procfs(root.path(), &unavailable_procfs)
                .unwrap()
                .count,
            1
        );
        assert!(legacy.exists());
        assert!(!orphan.exists());
    }

    #[test]
    fn concurrent_sweeper_cannot_remove_another_holder() {
        let root = tempfile::tempdir().unwrap();
        let (directory, lease) = create(root.path()).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..32 {
                    assert_eq!(sweep(root.path()).unwrap().count, 0);
                }
            });
            for _ in 0..32 {
                fs::write(directory.join("transcript.sqlite"), b"still live").unwrap();
            }
        });
        assert!(directory.exists());
        drop(lease);
        assert_eq!(sweep(root.path()).unwrap().count, 1);
    }

    #[test]
    fn subprocess_lease_fixture() {
        let Some(root) = std::env::var_os("CAGENTS_CACHE_LEASE_FIXTURE") else {
            return;
        };
        let root = PathBuf::from(root);
        let (directory, _lease) = create(&root).unwrap();
        fs::write(
            root.join("ready.tmp"),
            directory.to_string_lossy().as_bytes(),
        )
        .unwrap();
        fs::rename(root.join("ready.tmp"), root.join("ready")).unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn killed_second_process_is_reclaimed_but_its_live_lease_is_not() {
        let root = tempfile::tempdir().unwrap();
        let (live, _lease) = create(root.path()).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "transcript_cache::tests::subprocess_lease_fixture",
                "--nocapture",
            ])
            .env("CAGENTS_CACHE_LEASE_FIXTURE", root.path())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !root.path().join("ready").exists() {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("second cache holder never became ready");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let second = PathBuf::from(fs::read_to_string(root.path().join("ready")).unwrap());
        let protected = sweep(root.path());
        let _ = child.kill();
        child.wait().unwrap();
        assert_eq!(protected.unwrap().count, 0);
        assert!(second.exists());
        assert_eq!(sweep(root.path()).unwrap().count, 1);
        assert!(!second.exists());
        assert!(live.exists());
    }

    #[test]
    fn cache_scan_ignores_symlinks_and_non_cache_names() {
        let root = tempfile::tempdir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        fs::write(unrelated.path().join("keep"), "safe").unwrap();
        std::os::unix::fs::symlink(
            unrelated.path(),
            root.path().join(format!("chat-{}", uuid::Uuid::new_v4())),
        )
        .unwrap();
        fs::create_dir(root.path().join("chat-not-a-uuid")).unwrap();
        assert_eq!(usage(root.path()).unwrap().count, 0);
        assert_eq!(sweep(root.path()).unwrap().count, 0);
        assert!(unrelated.path().join("keep").exists());
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn usage_counts_only_owned_cache_direct_regular_files() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let external = outside.path().join("private-payload");
        fs::write(&external, b"outside private bytes").unwrap();
        let cache = root
            .path()
            .join(format!("chat-{}", uuid::Uuid::from_u128(1)));
        let empty = root
            .path()
            .join(format!("chat-{}", uuid::Uuid::from_u128(2)));
        fs::create_dir(&cache).unwrap();
        fs::create_dir(&empty).unwrap();
        fs::write(cache.join("transcript.sqlite"), b"abc").unwrap();
        fs::write(cache.join("actor-journal.sqlite"), b"12345").unwrap();
        fs::create_dir(cache.join("nested")).unwrap();
        fs::write(
            cache.join("nested/not-direct"),
            b"not part of the byte total",
        )
        .unwrap();
        symlink(&external, cache.join("linked-file")).unwrap();
        symlink(
            outside.path(),
            root.path()
                .join(format!("chat-{}", uuid::Uuid::from_u128(3))),
        )
        .unwrap();
        let ordinary_file = root
            .path()
            .join(format!("chat-{}", uuid::Uuid::from_u128(4)));
        fs::write(&ordinary_file, b"not a cache directory").unwrap();
        let invalid = root.path().join("chat-not-a-uuid");
        fs::create_dir(&invalid).unwrap();
        fs::write(invalid.join("ignored"), b"not a cache").unwrap();
        let unrelated = root.path().join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("ignored"), b"not a cache").unwrap();

        let observed = usage(root.path()).unwrap();
        assert_eq!(observed.count, 2);
        assert_eq!(observed.bytes, 8);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 6);
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 4);
        assert_eq!(fs::read_dir(&empty).unwrap().count(), 0);
        assert_eq!(fs::read(cache.join("transcript.sqlite")).unwrap(), b"abc");
        assert_eq!(
            fs::read(cache.join("actor-journal.sqlite")).unwrap(),
            b"12345"
        );
        assert_eq!(fs::read(&external).unwrap(), b"outside private bytes");
        assert_eq!(fs::read_link(cache.join("linked-file")).unwrap(), external);
        let missing = root.path().join("missing");
        let absent = usage(&missing).unwrap();
        assert_eq!(absent.count, 0);
        assert_eq!(absent.bytes, 0);
        assert!(!missing.exists());
        assert!(usage(&ordinary_file).is_err());
        assert_eq!(fs::read(&ordinary_file).unwrap(), b"not a cache directory");
    }
}
