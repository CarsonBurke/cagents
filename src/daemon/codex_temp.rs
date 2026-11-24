use std::{
    collections::HashSet,
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
pub(crate) const TEMP_ENV_KEYS: [&str; 3] = ["TMPDIR", "TMP", "TEMP"];
const PROC: &str = "/proc";

struct Owner {
    namespace: u64,
    process: Process,
}

pub(crate) struct PrivateDir {
    directory: Option<tempfile::TempDir>,
    launched: bool,
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
        let _ = sweep_base(&base, namespace, Path::new(PROC));
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
    pub(crate) fn close_if_unused(mut self) -> io::Result<()> {
        reclaim(self.directory.take().unwrap().keep())
    }

    pub(crate) fn defer_cleanup(mut self) -> Option<tokio::task::JoinHandle<io::Result<()>>> {
        // Retain only a path in the blocking job. Dropping a queued job or
        // shutting down the runtime leaves it for the orphan sweep.
        let path = self.directory.take().unwrap().keep();
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        Some(runtime.spawn_blocking(move || reclaim(path)))
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let Some(directory) = self.directory.take() else {
            return;
        };
        // Reaping the leader does not rule out tools which left its group.
        if self.launched {
            let _ = reclaim(directory.keep());
        }
    }
}

fn base(inherited: Option<&OsStr>, cwd: Option<&Path>) -> io::Result<PathBuf> {
    // On Linux temp_dir() reads our own TMPDIR, which can be the same
    // missing directory the launch inherited.
    let fallback = std::env::temp_dir();
    let cwd = cwd.unwrap_or_else(|| Path::new("."));
    for path in inherited.into_iter().chain([fallback.as_os_str()]) {
        if !path.is_empty() {
            match existing_base(&cwd.join(path)) {
                Ok(base) => return Ok(base),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    }
    existing_base(Path::new("/tmp"))
}

fn existing_base(path: &Path) -> io::Result<PathBuf> {
    // A cagents command run from inside Codex must not tie its new provider
    // to the lifetime of the outer provider's temporary directory.
    let path = fs::canonicalize(path).or_else(|error| {
        // A saved launch may name a private directory that just went away.
        managed_base(path).ok_or(error)?.canonicalize()
    })?;
    let base = managed_base(&path).unwrap_or(&path);
    if !fs::metadata(base)?.is_dir() {
        return Err(io::ErrorKind::NotADirectory.into());
    }
    Ok(base.to_path_buf())
}

pub(crate) fn managed_base(path: &Path) -> Option<&Path> {
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
    u64::from_str_radix(created, 16).ok()?;
    Some(Owner {
        namespace: u64::from_str_radix(namespace, 16).ok()?,
        process: Process {
            pid,
            start: u64::from_str_radix(start, 16).ok()?,
        },
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
        Path::new(PROC),
    )
}

fn sweep_base(base: &Path, namespace: u64, proc: &Path) -> io::Result<()> {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut stale = HashSet::new();
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
            stale.insert(path);
        }
    }
    for path in unused(proc, &stale)? {
        remove(path)?;
    }
    Ok(())
}

fn reclaim(path: PathBuf) -> io::Result<()> {
    for path in unused(Path::new(PROC), &HashSet::from([path]))? {
        remove(path)?;
    }
    Ok(())
}

fn remove(path: PathBuf) -> io::Result<()> {
    proc_entry(fs::remove_dir_all(path)).map(|_| ())
}

fn process_ids(proc: &Path) -> io::Result<HashSet<i32>> {
    fs::read_dir(proc)?
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
fn unused(proc: &Path, candidates: &HashSet<PathBuf>) -> io::Result<HashSet<PathBuf>> {
    if candidates.is_empty() {
        return Ok(HashSet::new());
    }
    for _ in 0..3 {
        let processes = process_ids(proc)?;
        let mut unused = candidates.clone();
        for pid in &processes {
            let process = proc.join(pid.to_string());
            match inspect_process(&process, &mut unused) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    if process_may_share_uid(&process)? {
                        // Even older processes can open or receive a file
                        // descriptor after the directory was created.
                        return Ok(HashSet::new());
                    }
                }
                Err(error) => return Err(error),
            }
            if unused.is_empty() {
                return Ok(unused);
            }
        }
        if processes == process_ids(proc)? {
            return Ok(unused);
        }
    }
    Ok(HashSet::new())
}

fn proc_entry<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result.map_err(exited_as_absent) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn protect_path(unused: &mut HashSet<PathBuf>, path: &[u8]) {
    for part in Path::new(OsStr::from_bytes(path)).ancestors() {
        unused.remove(part);
    }
}

fn without_deleted_suffix(path: &[u8]) -> &[u8] {
    path.strip_suffix(b" (deleted)").unwrap_or(path)
}

fn mapped_path(mut line: &[u8]) -> Option<&[u8]> {
    for _ in 0..5 {
        line = line.trim_ascii_start();
        line = &line[line.iter().position(u8::is_ascii_whitespace)?..];
    }
    let path = line.trim_ascii_start();
    path.starts_with(b"/").then_some(path)
}

fn inspect_link(link: &Path, unused: &mut HashSet<PathBuf>) -> io::Result<()> {
    if let Some(path) = proc_entry(fs::read_link(link))? {
        let bytes = path.as_os_str().as_bytes();
        protect_path(unused, without_deleted_suffix(bytes));
    }
    Ok(())
}

fn inspect_fds(path: &Path, unused: &mut HashSet<PathBuf>) -> io::Result<()> {
    if let Some(fds) = proc_entry(fs::read_dir(path))? {
        for fd in fds {
            inspect_link(&fd?.path(), unused)?;
        }
    }
    Ok(())
}

fn inspect_maps(path: &Path, unused: &mut HashSet<PathBuf>) -> io::Result<bool> {
    let Some(maps) = proc_entry(fs::read(path))?.filter(|maps| !maps.is_empty()) else {
        return Ok(false);
    };
    for path in maps.split(|byte| *byte == b'\n').filter_map(mapped_path) {
        let path = without_deleted_suffix(path);
        protect_path(unused, path);
        // procfs escapes newlines in mapped filenames. Compare that spelling
        // too, since the escape can also appear literally in a filename.
        if path.windows(4).any(|part| part == b"\\012") {
            unused.retain(|candidate| {
                let escaped: Vec<_> = candidate
                    .as_os_str()
                    .as_bytes()
                    .iter()
                    .flat_map(|byte| {
                        if *byte == b'\n' {
                            b"\\012"
                        } else {
                            std::slice::from_ref(byte)
                        }
                    })
                    .copied()
                    .collect();
                !Path::new(OsStr::from_bytes(path))
                    .starts_with(Path::new(OsStr::from_bytes(&escaped)))
            });
        }
    }
    Ok(true)
}

fn inspect_process(process: &Path, unused: &mut HashSet<PathBuf>) -> io::Result<()> {
    if let Some(environment) = proc_entry(fs::read(process.join("environ")))? {
        for entry in environment.split(|byte| *byte == 0) {
            for key in TEMP_ENV_KEYS {
                if let Some(value) = entry
                    .strip_prefix(key.as_bytes())
                    .and_then(|rest| rest.strip_prefix(b"="))
                {
                    protect_path(unused, value);
                }
            }
        }
    }
    inspect_fds(&process.join("fd"), unused)?;
    let mut has_maps = inspect_maps(&process.join("maps"), unused)?;
    // Threads can unshare their cwd and file table. When the leader exits
    // the shared mappings can also become visible only through a live thread.
    if let Some(tasks) = proc_entry(fs::read_dir(process.join("task")))? {
        for task in tasks {
            let task = task?;
            let path = task.path();
            inspect_link(&path.join("cwd"), unused)?;
            if Some(task.file_name().as_os_str()) != process.file_name() {
                inspect_fds(&path.join("fd"), unused)?;
                if !has_maps {
                    has_maps = inspect_maps(&path.join("maps"), unused)?;
                }
            }
        }
    }
    Ok(())
}

fn process_may_share_uid(process: &Path) -> io::Result<bool> {
    // A non-dumpable process can have a root-owned proc directory even
    // when it runs as us, so inspect its credentials instead.
    let Some(status) = proc_entry(fs::read_to_string(process.join("status")))? else {
        // An unreadable resource can race with process exit, but a missing
        // status alone does not establish that its process is gone.
        return process.try_exists();
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
    use std::{
        os::unix::fs::symlink,
        process::{Command, Stdio},
    };

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
    fn missing_tmpdir_uses_the_system_fallback() {
        const MISSING: &str = "CAGENTS_TEST_MISSING_TMPDIR";
        if let Some(missing) = std::env::var_os(MISSING) {
            assert_eq!(std::env::temp_dir(), Path::new(&missing));
            assert_eq!(
                base(Some(&missing), None).unwrap(),
                Path::new("/tmp").canonicalize().unwrap()
            );
            return;
        }

        let fixture = tempfile::tempdir().unwrap();
        let missing = fixture.path().join("missing");
        assert_eq!(
            base(Some(missing.as_os_str()), None).unwrap(),
            base(None, None).unwrap()
        );
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "daemon::codex_temp::tests::missing_tmpdir_uses_the_system_fallback",
            ])
            .env(MISSING, &missing)
            .env("TMPDIR", &missing)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn open_fds_and_mappings_keep_directories_without_environment_or_cwd_references() {
        let fixture = tempfile::tempdir().unwrap();
        let proc = fixture.path();
        fs::create_dir_all(proc.join("1/fd")).unwrap();
        let open = PathBuf::from("/base/open");
        let mapped = PathBuf::from("/base/mapped with space\nand newline");
        let unrelated = PathBuf::from("/base/mapped with space");
        symlink(open.join("file"), proc.join("1/fd/8")).unwrap();
        fs::write(proc.join("1/maps"), b"7fa-7fb r--p 00000000 08:07 12345  /base/mapped with space\\012and newline/deleted file (deleted)\n").unwrap();

        let candidates = HashSet::from([open, mapped, unrelated.clone()]);
        assert_eq!(
            unused(proc, &candidates).unwrap(),
            HashSet::from([unrelated])
        );
    }

    #[test]
    fn a_non_leader_threads_working_directory_is_a_live_reference() {
        let fixture = tempfile::tempdir().unwrap();
        let proc = fixture.path();
        let candidate = PathBuf::from("/base/in-use");
        fs::create_dir_all(proc.join("1/task/2")).unwrap();
        symlink(candidate.join("working"), proc.join("1/task/2/cwd")).unwrap();
        assert!(
            unused(proc, &HashSet::from([candidate]))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_unreadable_same_user_process_keeps_the_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let proc = fixture.path();
        let process = proc.join("1");
        fs::create_dir(&process).unwrap();
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        fs::write(
            process.join("status"),
            format!("Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
        let environ = process.join("environ");
        fs::write(&environ, b"").unwrap();
        fs::set_permissions(&environ, fs::Permissions::from_mode(0o000)).unwrap();
        let candidate = PathBuf::from("/base/might-be-in-use");
        let candidates = HashSet::from([candidate]);
        match fs::read(environ) {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                assert!(unused(proc, &candidates).unwrap().is_empty());
            }
            // A privileged test process can bypass the mode bits. An
            // unexpected procfs error must also prevent any cleanup.
            Ok(_) => {
                fs::write(process.join("fd"), b"").unwrap();
                assert_eq!(
                    unused(proc, &candidates).unwrap_err().kind(),
                    io::ErrorKind::NotADirectory
                );
            }
            Err(error) => panic!("{error}"),
        }
    }

    #[test]
    fn deferred_cleanup_waits_for_the_blocking_pool() {
        let fixture = tempfile::tempdir().unwrap();
        let mut temp = PrivateDir::create(Some(fixture.path().as_os_str()), None).unwrap();
        temp.launched();
        let path = temp.path().to_owned();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async move {
            let (release, wait) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = wait.recv();
            });
            let cleanup = temp.defer_cleanup().unwrap();
            let existed_while_queued = path.is_dir();
            release.send(()).unwrap();
            blocker.await.unwrap();
            cleanup.await.unwrap().unwrap();
            assert!(existed_while_queued);
        });
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
        let proc = root.join("proc");
        fs::create_dir(&proc).unwrap();
        let mut child = Command::new("/bin/cat")
            .env_remove("TMPDIR")
            .current_dir(&orphan)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        symlink(
            format!("/proc/{}", child.id()),
            proc.join(child.id().to_string()),
        )
        .unwrap();

        sweep_base(&root, namespace, &proc).unwrap();
        assert!(active.path().is_dir());
        assert!(orphan.is_dir());
        assert!(unrelated.is_dir());

        assert!(child.wait().unwrap().success());
        sweep_base(&root, namespace, &proc).unwrap();
        assert!(!orphan.exists());
        assert!(active.path().is_dir());
        assert!(unrelated.is_dir());
    }
}
