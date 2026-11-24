use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs, io,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

use super::holder_registry::{Process, exited_as_absent};

const PREFIX: &str = "cagents-codex-";
const RANDOM_BYTES: usize = 6;

struct Owner {
    namespace: u64,
    process: Process,
    created: u64,
}

pub(crate) struct PrivateDir {
    directory: Option<tempfile::TempDir>,
    launched: bool,
    created: u64,
}

impl PrivateDir {
    /// Codex makes TMPDIR a writable sandbox root. Keep its synthetic files
    /// private while inheriting the filesystem selected for temporary files.
    pub(crate) fn create(inherited: Option<&OsStr>, cwd: Option<&Path>) -> io::Result<Self> {
        let base = base(inherited, cwd)?;
        let namespace = pid_namespace()?;
        let owner = Process::read(std::process::id() as i32)?;
        // PID and start time are published atomically with mkdir, so a
        // concurrent sweep cannot mistake a new directory for an orphan.
        let _ = sweep_base(&base, namespace);
        let created = boot_ticks()?;
        let prefix = format!(
            "{PREFIX}{namespace:x}-{:x}-{:x}-{created:x}-",
            owner.pid, owner.start
        );
        Ok(Self {
            directory: Some(
                tempfile::Builder::new()
                    .prefix(&prefix)
                    .rand_bytes(RANDOM_BYTES)
                    .permissions(fs::Permissions::from_mode(0o700))
                    .tempdir_in(base)?,
            ),
            launched: false,
            created,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        self.directory.as_ref().unwrap().path()
    }

    /// Call immediately after spawn, even if later launch setup can fail.
    pub(crate) fn launched(&mut self) {
        self.launched = true;
    }

    #[cfg(feature = "host")]
    pub(crate) fn close_after_teardown(mut self) -> io::Result<()> {
        self.directory.take().unwrap().close()
    }

    #[cfg(feature = "host")]
    pub(crate) fn keep(mut self) {
        let _ = self.directory.take().unwrap().keep();
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let Some(directory) = self.directory.take() else {
            return;
        };
        // Reaping the leader does not rule out tools which left its group.
        if self.launched
            && !unused(&HashMap::from([(
                directory.path().to_path_buf(),
                self.created,
            )]))
            .is_ok_and(|paths| !paths.is_empty())
        {
            let _ = directory.keep();
        }
    }
}

fn base(inherited: Option<&OsStr>, cwd: Option<&Path>) -> io::Result<PathBuf> {
    let path = match inherited.filter(|value| !value.is_empty()) {
        Some(value) => cwd.unwrap_or_else(|| Path::new(".")).join(value),
        None => {
            let path = std::env::temp_dir();
            if path.as_os_str().is_empty() {
                PathBuf::from("/tmp")
            } else {
                path
            }
        }
    };
    // A cagents command run from inside Codex must not tie its new provider
    // to the lifetime of the outer provider's temporary directory.
    let path = fs::canonicalize(&path).or_else(|error| {
        // A saved launch may name a private directory that just went away.
        managed_base(&path).ok_or(error)?.canonicalize()
    })?;
    Ok(managed_base(&path).unwrap_or(&path).to_path_buf())
}

fn managed_base(path: &Path) -> Option<&Path> {
    path.ancestors()
        .filter(|part| owner(part).is_some())
        .last()?
        .parent()
}

fn pid_namespace() -> io::Result<u64> {
    Ok(fs::metadata("/proc/self/ns/pid")?.ino())
}

fn owner(path: &Path) -> Option<Owner> {
    let (namespace, rest) = path
        .file_name()?
        .to_str()?
        .strip_prefix(PREFIX)?
        .split_once('-')?;
    let (pid, rest) = rest.split_once('-')?;
    let (start, rest) = rest.split_once('-')?;
    let (created, suffix) = rest.split_once('-')?;
    if suffix.len() != RANDOM_BYTES || !suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }
    let pid = i32::from_str_radix(pid, 16).ok()?;
    if pid <= 0 {
        return None;
    }
    Some(Owner {
        namespace: u64::from_str_radix(namespace, 16).ok()?,
        process: Process {
            pid,
            start: u64::from_str_radix(start, 16).ok()?,
        },
        created: u64::from_str_radix(created, 16).ok()?,
    })
}

fn boot_ticks() -> io::Result<u64> {
    // SAFETY: sysconf takes a scalar. clock_gettime initializes this timespec.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let mut now = std::mem::MaybeUninit::<libc::timespec>::uninit();
    if ticks <= 0 || unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, now.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let now = unsafe { now.assume_init() };
    Ok(now.tv_sec as u64 * ticks as u64 + now.tv_nsec as u64 * ticks as u64 / 1_000_000_000)
}

#[cfg(feature = "host")]
pub(crate) fn sweep() -> io::Result<()> {
    sweep_base(
        &base(std::env::var_os("TMPDIR").as_deref(), None)?,
        pid_namespace()?,
    )
}

fn sweep_base(base: &Path, namespace: u64) -> io::Result<()> {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut stale = HashMap::new();
    for entry in fs::read_dir(base)? {
        let entry = entry?;
        let path = entry.path();
        let Some(owner) = owner(&path) else {
            continue;
        };
        if owner.namespace != namespace {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
            continue;
        }
        let dead = match Process::read(owner.process.pid) {
            Ok(current) => current != owner.process,
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        };
        if dead {
            stale.insert(path, owner.created);
        }
    }
    for path in unused(&stale)? {
        match fs::remove_dir_all(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

fn process_ids() -> io::Result<HashSet<i32>> {
    fs::read_dir("/proc")?
        .map(|entry| {
            Ok(entry?
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok()))
        })
        .filter_map(Result::transpose)
        .collect()
}

/// Only collect directories absent from a stable process census. A process
/// may fork, leave its provider's group, and exit during the scan. If the
/// process set changes, scan again or leave the candidates for a later sweep.
fn unused(candidates: &HashMap<PathBuf, u64>) -> io::Result<HashSet<PathBuf>> {
    if candidates.is_empty() {
        return Ok(HashSet::new());
    }
    for _ in 0..3 {
        let processes = process_ids()?;
        let mut unused: HashSet<_> = candidates.keys().cloned().collect();
        for pid in &processes {
            let path = format!("/proc/{pid}/environ");
            match fs::read(path).map_err(exited_as_absent) {
                Ok(environment) => {
                    // A tool might clear TMPDIR after changing into it.
                    let cwd = fs::read_link(format!("/proc/{pid}/cwd")).ok();
                    let temp_dirs = environment.split(|byte| *byte == 0).filter_map(|entry| {
                        entry
                            .strip_prefix(b"TMPDIR=")
                            .map(OsStr::from_bytes)
                            .map(Path::new)
                    });
                    for path in temp_dirs.chain(cwd.as_deref()) {
                        for part in path.ancestors() {
                            unused.remove(part);
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    if process_may_share_uid(*pid)? {
                        match Process::read(*pid) {
                            // A process older than a random private path
                            // could not have inherited it from this launch.
                            Ok(process) => unused.retain(|path| candidates[path] > process.start),
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
                Err(error) => return Err(error),
            }
            if unused.is_empty() {
                return Ok(unused);
            }
        }
        if processes == process_ids()? {
            return Ok(unused);
        }
    }
    Ok(HashSet::new())
}

fn process_may_share_uid(pid: i32) -> io::Result<bool> {
    // A non-dumpable process can have a root-owned proc directory even
    // when it runs as us, so inspect its credentials instead.
    let status = match fs::read_to_string(format!("/proc/{pid}/status")).map_err(exited_as_absent) {
        Ok(status) => status,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    Ok(status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .is_none_or(|line| {
            line.split_whitespace()
                .any(|value| value.parse() == Ok(uid))
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn relative_tmpdir_uses_launch_cwd_and_managed_paths_do_not_nest() {
        let fixture = tempfile::tempdir().unwrap();
        let base = fixture.path().join("temp");
        fs::create_dir(&base).unwrap();
        let outer = PrivateDir::create(Some(OsStr::new("temp")), Some(fixture.path())).unwrap();
        let inner = PrivateDir::create(Some(outer.path().as_os_str()), None).unwrap();
        let nested = outer.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let nested = PrivateDir::create(Some(nested.as_os_str()), None).unwrap();

        let base = base.canonicalize().unwrap();
        assert_eq!(outer.path().parent(), Some(base.as_path()));
        assert_eq!(inner.path().parent(), Some(base.as_path()));
        assert_eq!(nested.path().parent(), Some(base.as_path()));
        assert_ne!(outer.path(), inner.path());
        assert_eq!(fs::metadata(inner.path()).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn sweep_reclaims_dead_owners_after_their_last_user_exits() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let namespace = pid_namespace().unwrap();
        let owner = Process::read(std::process::id() as i32).unwrap();
        let active = PrivateDir::create(Some(root.as_os_str()), None).unwrap();
        // A recycled PID must not keep a dead owner's directory alive.
        let orphan = root.join(format!(
            "{PREFIX}{namespace:x}-{:x}-{:x}-{:x}-AbCd12",
            owner.pid,
            owner.start + 1,
            boot_ticks().unwrap()
        ));
        fs::create_dir(&orphan).unwrap();
        fs::set_permissions(&orphan, fs::Permissions::from_mode(0o700)).unwrap();
        let unrelated = root.join("codex-unrelated");
        fs::create_dir(&unrelated).unwrap();
        let mut child = Command::new("/bin/cat")
            .env_remove("TMPDIR")
            .current_dir(&orphan)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();

        sweep_base(&root, namespace).unwrap();
        assert!(active.path().is_dir());
        assert!(orphan.is_dir());
        assert!(unrelated.is_dir());

        assert!(child.wait().unwrap().success());
        crate::test_timing::eventually("the unused Codex temp directory was swept", || {
            sweep_base(&root, namespace).unwrap();
            !orphan.exists()
        });
        assert!(active.path().is_dir());
        assert!(unrelated.is_dir());
    }
}
