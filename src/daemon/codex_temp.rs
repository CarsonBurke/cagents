#[cfg(target_os = "linux")]
use std::{collections::HashMap, os::unix::ffi::OsStrExt};
use std::{
    collections::HashSet,
    ffi::OsStr,
    fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt},
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
    /// Relative temporary directories are resolved from the provider's held
    /// launch directory. An absolute path needs no working-directory lookup.
    #[cfg(feature = "host")]
    pub(crate) fn create_for_launch(
        inherited: Option<&OsStr>,
        directory: Option<&fs::File>,
        fallback: &Path,
    ) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        let cwd = directory.map(crate::descriptor::path_of).transpose()?;
        #[cfg(target_os = "macos")]
        let cwd = if inherited.is_some_and(|path| !path.is_empty() && Path::new(path).is_relative())
            || std::env::temp_dir().is_relative()
        {
            directory.map(crate::fs_safety::reaching_path).transpose()?
        } else {
            None
        };
        Self::create(inherited, Some(cwd.as_deref().unwrap_or(fallback)))
    }

    /// Codex makes TMPDIR a writable sandbox root. Keep its synthetic files
    /// private while inheriting the filesystem selected for temporary files.
    pub(crate) fn create(inherited: Option<&OsStr>, cwd: Option<&Path>) -> io::Result<Self> {
        let base = base(inherited, cwd)?;
        let namespace = pid_namespace()?;
        let owner = Process::read(std::process::id() as i32)?;
        // PID and start time are published atomically with mkdir, so a
        // concurrent sweep cannot mistake a new directory for an orphan.
        let _ = sweep_base(&base, namespace, Path::new(PROC));
        let created = crate::process::clock::start_ticks_now()?;
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

#[cfg(target_os = "linux")]
fn pid_namespace() -> io::Result<u64> {
    Ok(fs::metadata("/proc/self/ns/pid")?.ino())
}

/// Darwin has no PID namespaces: every process shares one PID space, so every
/// directory name carries the same namespace.
#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_wraps)]
fn pid_namespace() -> io::Result<u64> {
    Ok(0)
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

#[cfg(feature = "host")]
pub fn sweep() -> io::Result<()> {
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
    let proc = Path::new(PROC);
    #[cfg(all(test, target_os = "linux", feature = "host"))]
    let fixture = cleanup_fixture::census_root()?;
    #[cfg(all(test, target_os = "linux", feature = "host"))]
    let proc = fixture.as_deref().unwrap_or(proc);
    for path in unused(proc, &HashSet::from([path]))? {
        remove(path)?;
    }
    Ok(())
}

fn remove(path: PathBuf) -> io::Result<()> {
    proc_entry(fs::remove_dir_all(path)).map(|_| ())
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct ProcessCensus {
    identities: HashMap<(i32, u64), bool>,
    vanished_during_read: bool,
}

#[cfg(target_os = "linux")]
fn process_census(proc: &Path) -> io::Result<ProcessCensus> {
    let mut census = ProcessCensus::default();
    for entry in fs::read_dir(proc)? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let Some(stat) = proc_entry(fs::read(entry.path().join("stat")))? else {
            census.vanished_during_read = true;
            continue;
        };
        let (start, live) = process_identity_and_liveness(&stat)
            .ok_or_else(|| io::Error::other("invalid process stat record"))?;
        census.identities.insert((pid, start), live);
    }
    Ok(census)
}

#[cfg(target_os = "linux")]
fn process_identity_and_liveness(stat: &[u8]) -> Option<(u64, bool)> {
    let mut fields = crate::model::agent_recovery::process_stat_fields(stat)?;
    let state = fields.next()?;
    // Skip from field 3 (state) to 20 (num_threads), then to 22 (starttime).
    let threads: u32 = fields.nth(16)?.parse().ok()?;
    let start = fields.nth(1)?.parse().ok()?;
    // A zombie leader can still have live siblings using the directory.
    Some((start, state != "Z" || threads != 1))
}

/// Reaping a group that had already exited cannot transfer a reference.
/// A newly observed group could have passed a reference to an inspected
/// process before becoming a zombie, so it still invalidates the scan.
#[cfg(target_os = "linux")]
fn census_is_stable(before: &ProcessCensus, after: &ProcessCensus) -> bool {
    // A PID that disappeared between readdir and stat may also have been new.
    !after.vanished_during_read
        && after
            .identities
            .iter()
            .all(|(identity, live)| before.identities.get(identity) == Some(live))
        && before
            .identities
            .iter()
            .filter(|&(_, live)| *live)
            .all(|(identity, _)| after.identities.contains_key(identity))
}

#[cfg(target_os = "linux")]
fn unused(proc: &Path, candidates: &HashSet<PathBuf>) -> io::Result<HashSet<PathBuf>> {
    unused_with_census(proc, candidates, || process_census(proc))
}

#[cfg(target_os = "linux")]
fn unused_with_census(
    proc: &Path,
    candidates: &HashSet<PathBuf>,
    mut census: impl FnMut() -> io::Result<ProcessCensus>,
) -> io::Result<HashSet<PathBuf>> {
    if candidates.is_empty() {
        return Ok(HashSet::new());
    }
    let mut unused = candidates.clone();
    let mut processes = census()?;
    for _ in 0..3 {
        for (&(pid, _), _) in processes.identities.iter().filter(|&(_, live)| *live) {
            let process = proc.join(pid.to_string());
            match inspect_process(&process, &mut unused) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    if unreadable_process_may_hold_references(&process)? {
                        tracing::debug!(
                            pid,
                            "Deferring Codex temporary cleanup for unreadable process"
                        );
                        return Ok(HashSet::new());
                    }
                }
                Err(error) => return Err(error),
            }
            if unused.is_empty() {
                return Ok(unused);
            }
        }
        let after = census()?;
        if census_is_stable(&processes, &after) {
            return Ok(unused);
        }
        processes = after;
    }
    tracing::debug!("Deferring Codex temporary cleanup after changes among possible users");
    Ok(HashSet::new())
}

/// Darwin has no procfs, and no other census here can prove that nothing
/// still uses a directory: a tool that left its provider's group may hold it
/// as its cwd, an open file or its TMPDIR. A directory that might be in use
/// must survive, so nothing is reclaimed: each launch leaves its private
/// (0700) directory behind until a Darwin process census can vouch for it.
#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_wraps)]
fn unused(_proc: &Path, _candidates: &HashSet<PathBuf>) -> io::Result<HashSet<PathBuf>> {
    Ok(HashSet::new())
}

fn proc_entry<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result.map_err(exited_as_absent) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn protect_path(unused: &mut HashSet<PathBuf>, path: &[u8]) {
    for part in Path::new(OsStr::from_bytes(path)).ancestors() {
        unused.remove(part);
    }
}

#[cfg(target_os = "linux")]
fn without_deleted_suffix(path: &[u8]) -> &[u8] {
    path.strip_suffix(b" (deleted)").unwrap_or(path)
}

#[cfg(target_os = "linux")]
fn mapped_path(mut line: &[u8]) -> Option<&[u8]> {
    for _ in 0..5 {
        line = line.trim_ascii_start();
        line = &line[line.iter().position(u8::is_ascii_whitespace)?..];
    }
    let path = line.trim_ascii_start();
    path.starts_with(b"/").then_some(path)
}

#[cfg(target_os = "linux")]
fn inspect_link(link: &Path, unused: &mut HashSet<PathBuf>) -> io::Result<()> {
    if let Some(path) = proc_entry(fs::read_link(link))? {
        let bytes = path.as_os_str().as_bytes();
        protect_path(unused, without_deleted_suffix(bytes));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn inspect_fds(path: &Path, unused: &mut HashSet<PathBuf>) -> io::Result<()> {
    if let Some(fds) = proc_entry(fs::read_dir(path))? {
        for fd in fds {
            inspect_link(&fd?.path(), unused)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn status_value<'a>(status: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    status
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(key))
        .map(<[u8]>::trim_ascii)
}

#[cfg(target_os = "linux")]
fn is_exited(status: &[u8]) -> bool {
    // The kernel releases a zombie's resources before its parent reaps it.
    // A zombie leader can still have live siblings using the directory.
    status_value(status, b"State:").is_some_and(|state| state.starts_with(b"Z "))
        && status_value(status, b"Threads:") == Some(b"1")
}

#[cfg(target_os = "linux")]
fn may_share_uid(status: &[u8]) -> bool {
    // A non-dumpable process can have a root-owned proc directory even when
    // it runs as us. Compare credentials instead of proc directory ownership.
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    status_value(status, b"Uid:")
        .and_then(|line| std::str::from_utf8(line).ok())
        .is_none_or(|line| {
            line.split_ascii_whitespace()
                .any(|value| value.parse() == Ok(uid))
        })
}

#[cfg(target_os = "linux")]
fn unreadable_process_may_hold_references(process: &Path) -> io::Result<bool> {
    let Some(status) = proc_entry(fs::read(process.join("status")))? else {
        // An unreadable resource can race with process exit, but a missing
        // status alone does not establish that its process is gone.
        return process.try_exists();
    };
    Ok(!is_exited(&status) && may_share_uid(&status))
}

#[cfg(all(test, target_os = "linux", feature = "host"))]
pub(crate) mod cleanup_fixture {
    use std::{
        fs, io,
        os::unix::fs::symlink,
        path::{Path, PathBuf},
        process::Command,
    };

    pub(crate) const PROC_ROOT: &str = "CAGENTS_TEST_CODEX_TEMP_PROC";

    fn proc_root() -> Option<PathBuf> {
        std::env::var_os(PROC_ROOT).map(PathBuf::from)
    }

    pub(super) fn census_root() -> io::Result<Option<PathBuf>> {
        let Some(proc) = proc_root() else {
            return Ok(None);
        };
        // The synthetic census is a directory of links to real PID entries.
        // Remove links whose PID entry disappeared before the scan starts.
        for entry in fs::read_dir(&proc)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_symlink() && !path.try_exists()? {
                fs::remove_file(path)?;
            }
        }
        Ok(Some(proc))
    }

    /// Run the caller with a procfs census containing only its own provider.
    /// The terminal holder reexecs this test binary and inherits the root.
    /// An unreadable or unstable production census can retain an unused path.
    pub(crate) fn run() -> bool {
        if proc_root().is_some() {
            return false;
        }
        let proc = tempfile::tempdir().unwrap();
        let thread = std::thread::current();
        let test = thread.name().expect("named libtest fixture");
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test])
            .env(PROC_ROOT, proc.path())
            .status()
            .unwrap();
        assert!(status.success(), "Codex cleanup fixture failed: {status}");
        true
    }

    pub(crate) fn include_provider(pid: i32) {
        let pid = pid.to_string();
        symlink(
            Path::new(super::PROC).join(&pid),
            proc_root().unwrap().join(&pid),
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    #[cfg(target_os = "linux")]
    use std::{os::unix::fs::symlink, process::Stdio};

    #[cfg(target_os = "linux")]
    fn fake_process(proc: &Path, pid: i32) -> PathBuf {
        let process = proc.join(pid.to_string());
        fs::create_dir_all(&process).unwrap();
        write_process_stat(&process, "S", 1, 1);
        process
    }

    #[cfg(target_os = "linux")]
    fn write_process_stat(process: &Path, state: &str, threads: u32, start: u64) {
        let skipped = "0 ".repeat(16);
        let mut stat = b"1 (fixture) \xff) ".to_vec();
        stat.extend_from_slice(format!("{state} {skipped}{threads} 0 {start}\n").as_bytes());
        fs::write(process.join("stat"), stat).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn unreaped_zombies_do_not_defer_cleanup_as_their_pids_disappear() {
        let fixture = tempfile::tempdir().unwrap();
        let proc = fixture.path().join("proc");
        fs::create_dir(&proc).unwrap();
        fake_process(&proc, 1);
        for pid in 2..=7 {
            let zombie = fake_process(&proc, pid);
            write_process_stat(&zombie, "Z", 1, 1);
        }
        let candidate = fixture.path().join("unused");
        let candidates = HashSet::from([candidate]);
        let mut pid = 1;
        let unused = unused_with_census(&proc, &candidates, || {
            if pid > 1 {
                fs::remove_dir_all(proc.join(pid.to_string()))?;
            }
            pid += 1;
            process_census(&proc)
        })
        .unwrap();

        assert_eq!(unused, candidates);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_new_process_can_inherit_a_reference_after_the_first_census() {
        let fixture = tempfile::tempdir().unwrap();
        let proc = fixture.path();
        let parent = fake_process(proc, 1);
        let candidate = PathBuf::from("/base/inherited");
        let mut first = true;
        let candidates = HashSet::from([candidate.clone()]);
        let unused = unused_with_census(proc, &candidates, || {
            if !std::mem::replace(&mut first, false) {
                fs::remove_dir_all(&parent)?;
                let child = fake_process(proc, 2);
                fs::create_dir(child.join("fd"))?;
                symlink(candidate.join("file"), child.join("fd/3"))?;
            }
            process_census(proc)
        })
        .unwrap();

        assert!(unused.is_empty());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_surviving_peer_can_receive_a_reference_from_a_departing_process() {
        let fixture = tempfile::tempdir().unwrap();
        let proc = fixture.path();
        let donor = fake_process(proc, 1);
        let recipient = fake_process(proc, 2);
        let candidate = PathBuf::from("/base/received");
        let candidates = HashSet::from([candidate.clone()]);
        let mut first = true;
        let unused = unused_with_census(proc, &candidates, || {
            if !std::mem::replace(&mut first, false) {
                fs::remove_dir_all(&donor)?;
                fs::create_dir(recipient.join("fd"))?;
                symlink(candidate.join("file"), recipient.join("fd/3"))?;
            }
            process_census(proc)
        })
        .unwrap();

        assert!(unused.is_empty());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_new_zombie_can_have_transferred_a_reference_during_the_scan() {
        let fixture = tempfile::tempdir().unwrap();
        let proc = fixture.path();
        let recipient = fake_process(proc, 1);
        let candidate = PathBuf::from("/base/received");
        let candidates = HashSet::from([candidate.clone()]);
        let mut reads = 0;
        let unused = unused_with_census(proc, &candidates, || {
            reads += 1;
            if reads == 2 {
                let donor = fake_process(proc, 2);
                write_process_stat(&donor, "Z", 1, 1);
                fs::create_dir(recipient.join("fd"))?;
                symlink(candidate.join("file"), recipient.join("fd/3"))?;
            }
            process_census(proc)
        })
        .unwrap();

        assert!(unused.is_empty());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_pid_reaped_before_stat_can_have_transferred_a_reference() {
        let fixture = tempfile::tempdir().unwrap();
        let proc = fixture.path();
        let recipient = fake_process(proc, 1);
        let candidate = PathBuf::from("/base/received");
        let candidates = HashSet::from([candidate.clone()]);
        let mut reads = 0;
        let unused = unused_with_census(proc, &candidates, || {
            reads += 1;
            if reads == 2 {
                fs::create_dir(proc.join("2"))?;
                fs::create_dir(recipient.join("fd"))?;
                symlink(candidate.join("file"), recipient.join("fd/3"))?;
            } else if reads == 3 {
                fs::remove_dir(proc.join("2"))?;
            }
            process_census(proc)
        })
        .unwrap();

        assert!(unused.is_empty());
        assert_eq!(reads, 2);
    }

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
    #[cfg(target_os = "linux")] // A procfs census.
    fn open_fds_and_mappings_keep_directories_without_environment_or_cwd_references() {
        let fixture = tempfile::tempdir().unwrap();
        let proc = fixture.path();
        fs::create_dir(fake_process(proc, 1).join("fd")).unwrap();
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
    #[cfg(target_os = "linux")] // A procfs census.
    fn a_non_leader_threads_working_directory_is_a_live_reference() {
        let fixture = tempfile::tempdir().unwrap();
        let proc = fixture.path();
        let candidate = PathBuf::from("/base/in-use");
        let leader = fake_process(proc, 1);
        write_process_stat(&leader, "Z", 2, 1);
        fs::create_dir_all(leader.join("task/2")).unwrap();
        symlink(candidate.join("working"), proc.join("1/task/2/cwd")).unwrap();
        assert!(
            unused(proc, &HashSet::from([candidate]))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    #[cfg(target_os = "linux")] // A procfs census.
    fn an_unreadable_same_user_process_keeps_the_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let proc = fixture.path();
        let process = fake_process(proc, 1);
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
    #[cfg(target_os = "linux")]
    fn an_unreadable_zombie_can_only_keep_references_through_live_threads() {
        let fixture = tempfile::tempdir().unwrap();
        let process = fixture.path();
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        for (threads, can_hold_references) in [(1, false), (2, true)] {
            fs::write(
                process.join("status"),
                format!(
                    "State:\tZ (zombie)\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\nThreads:\t{threads}\n"
                ),
            )
            .unwrap();
            assert_eq!(
                unreadable_process_may_hold_references(process).unwrap(),
                can_hold_references
            );
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
    #[cfg(target_os = "linux")] // A procfs census.
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
            crate::process::clock::start_ticks_now().unwrap()
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
        let child_entry = proc.join(child.id().to_string());
        symlink(format!("/proc/{}", child.id()), &child_entry).unwrap();

        sweep_base(&root, namespace, &proc).unwrap();
        assert!(active.path().is_dir());
        assert!(orphan.is_dir());
        assert!(unrelated.is_dir());

        assert!(child.wait().unwrap().success());
        // The fixture's link persists after the kernel removes the PID entry.
        fs::remove_file(child_entry).unwrap();
        sweep_base(&root, namespace, &proc).unwrap();
        assert!(!orphan.exists());
        assert!(active.path().is_dir());
        assert!(unrelated.is_dir());
    }
}
