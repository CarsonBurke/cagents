use super::{ABANDONED, STAGED_PREFIX, identity, still_named};
use crate::{
    account::{Account, home::Homes, registry::Store},
    agent::Provider,
    daemon::holder_registry::{Process, exited_as_absent},
    fs_safety,
};
use std::{collections::BTreeSet, ffi::OsStr, fs, io, os::unix::fs::MetadataExt, path::Path};
use uuid::Uuid;

pub(super) fn stage_name() -> io::Result<String> {
    let owner = Process::read(std::process::id() as i32)?;
    let namespace = namespace()?;
    Ok(format!(
        "{STAGED_PREFIX}{namespace:x}-{:x}-{:x}-{}",
        owner.pid,
        owner.start,
        Uuid::new_v4()
    ))
}

#[cfg(target_os = "linux")]
fn namespace() -> io::Result<u64> {
    Ok(fs::metadata("/proc/self/ns/pid")?.ino())
}

#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_wraps)]
fn namespace() -> io::Result<u64> {
    Ok(0)
}

fn v4_uuid(suffix: &str) -> bool {
    suffix.len() == 36 && Uuid::parse_str(suffix).is_ok_and(|id| id.get_version_num() == 4)
}

fn abandoned_name(name: &OsStr, namespace: u64) -> bool {
    let Some(suffix) = name
        .to_str()
        .and_then(|name| name.strip_prefix(STAGED_PREFIX))
    else {
        return false;
    };
    // Older daemons named copies with a bare UUID. Only the age check below
    // can protect an outstanding copy from an old daemon.
    if v4_uuid(suffix) {
        return true;
    }
    let mut fields = suffix.splitn(4, '-');
    let (Some(owner_ns), Some(pid), Some(start), Some(uuid)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return false;
    };
    if u64::from_str_radix(owner_ns, 16) != Ok(namespace) || !v4_uuid(uuid) {
        return false;
    }
    let (Ok(pid), Ok(start)) = (i32::from_str_radix(pid, 16), u64::from_str_radix(start, 16))
    else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    match Process::read(pid).map_err(exited_as_absent) {
        Ok(current) => current.start != start,
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    }
}

fn owned(metadata: &fs::Metadata) -> bool {
    // SAFETY: geteuid has no preconditions.
    metadata.uid() == unsafe { libc::geteuid() }
}

pub(super) fn require_owned(directory: &fs_safety::Root) -> io::Result<()> {
    let metadata = directory.metadata()?;
    if !owned(&metadata) || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "Transcript directory must be owned and writable only by the current user: {}",
                directory.path().display()
            ),
        ));
    }
    Ok(())
}

/// Sweep each registered Codex home, including the system and archived
/// accounts. The account registry lease is released before scanning history.
pub fn sweep_codex() -> io::Result<()> {
    let mut homes: BTreeSet<_> = Account::system(Provider::Codex)
        .resolved_home()
        .into_iter()
        .collect();
    let registry = Store::installation()
        .map(|store| store.try_load())
        .transpose();
    if let Ok(Some(registry)) = &registry {
        let managed = Homes::installation();
        homes.extend(
            registry
                .all_accounts(managed.as_ref())
                .into_iter()
                .filter(|account| account.provider == Provider::Codex)
                .filter_map(|account| account.resolved_home()),
        );
    }
    let mut result = registry.map(|_| ());
    for home in homes.into_iter().filter(|home| home.is_absolute()) {
        if let Err(error) = codex_home(&home)
            && result.is_ok()
        {
            result = Err(error);
        }
    }
    result
}

fn codex_home(home: &Path) -> io::Result<()> {
    let mut result = Ok(());
    // Match the rollout readers' dated, legacy flat and archived layouts.
    for (relative, depth) in [("sessions", 3), ("archived_sessions", 0)] {
        let path = home.join(relative);
        let root = crate::secure_file::reject_symlink_components(&path)
            .and_then(|()| fs_safety::Root::open_local(&path));
        let scanned = match root {
            Ok(root) => copies_in(&root, depth),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        result = result.and(scanned);
    }
    result
}

pub(super) fn copies_in(directory: &fs_safety::Root, depth: usize) -> io::Result<()> {
    require_owned(directory)?;
    collect(directory, depth, namespace()?)
}

fn collect(directory: &fs_safety::Root, depth: usize, namespace: u64) -> io::Result<()> {
    // Enumeration supplies names only. Opening children and removing entries
    // always go through the retained descriptor.
    for entry in fs::read_dir(directory.path())?.flatten() {
        let name = entry.file_name();
        let relative = Path::new(&name);
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if depth > 0
            && kind.is_dir()
            && let Ok(child) = fs_safety::open_directory_beneath(directory, relative)
            && require_owned(&child).is_ok()
        {
            let _ = collect(&child, depth - 1, namespace);
        } else if kind.is_file()
            && name
                .as_encoded_bytes()
                .starts_with(STAGED_PREFIX.as_bytes())
            && let Ok(metadata) = fs_safety::metadata_beneath(directory, relative)
            && metadata.is_file()
            && owned(&metadata)
            && metadata.nlink() == 1
            && metadata.mode() & 0o077 == 0
            && metadata
                .modified()
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age >= ABANDONED)
            && abandoned_name(&name, namespace)
            && still_named(directory, &name, identity(&metadata))
        {
            let _ = fs_safety::unlink_within(directory, &name);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::{OpenOptionsExt, symlink},
        time::SystemTime,
    };

    #[test]
    fn sweeps_old_stages_across_dates_without_touching_active_or_unrelated_entries() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let day = home.join("sessions/2026/11/01");
        let other_day = home.join("sessions/2026/11/28");
        let archive = home.join("archived_sessions");
        let outside = home.join("outside");
        for directory in [&day, &other_day, &archive, &outside] {
            fs::create_dir_all(directory).unwrap();
        }
        symlink(&outside, home.join("sessions/2025")).unwrap();

        let write = |path: &Path, age| {
            fs::File::options()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::now() - age)
                .unwrap();
        };
        let legacy = || format!("{STAGED_PREFIX}{}", Uuid::new_v4());
        let old = [
            day.join(legacy()),
            other_day.join(legacy()),
            archive.join(legacy()),
        ];
        for path in &old {
            write(path, ABANDONED);
        }
        let live = day.join(stage_name().unwrap());
        let owner = Process::read(std::process::id() as i32).unwrap();
        let dead = day.join(format!(
            "{STAGED_PREFIX}{:x}-{:x}-{:x}-{}",
            namespace().unwrap(),
            owner.pid,
            owner.start + 1,
            Uuid::new_v4()
        ));
        write(&dead, ABANDONED);
        let retained = [
            (live, ABANDONED),
            (other_day.join(legacy()), Default::default()),
            (day.join(format!("{STAGED_PREFIX}personal")), ABANDONED),
            (day.join("rollout-2026-11-01.jsonl"), ABANDONED),
            (outside.join(legacy()), ABANDONED),
        ];
        for (path, age) in &retained {
            write(path, *age);
        }
        let alias = day.join(legacy());
        symlink(&retained[4].0, &alias).unwrap();
        let hard_link = day.join(legacy());
        fs::hard_link(&retained[3].0, &hard_link).unwrap();

        codex_home(home).unwrap();

        for path in old.into_iter().chain([dead]) {
            assert!(!path.exists(), "old stage survived at {}", path.display());
        }
        for path in retained
            .into_iter()
            .map(|(path, _)| path)
            .chain([alias, hard_link])
        {
            assert!(path.exists(), "unexpectedly removed {}", path.display());
        }
    }
}
