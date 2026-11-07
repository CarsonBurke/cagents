//! Separate a provider from the commands it starts.
//!
//! systemd-oomd only chooses leaf cgroups, or cgroups with
//! `memory.oom.group=1`, and kills every process in the one it chooses
//! (systemd-oomd.service(8)). A provider scope used to be a single leaf, so
//! one command that exhausted memory took the provider, the conversation's
//! live process and every other command down with it. The provider's scope is
//! delegated, so it can be split here into a `provider` leaf, holding only the
//! provider process, and a `command-N` leaf for each process tree the provider
//! starts. oomd then chooses the command that used the memory.
//!
//! `ManagedOOMPreference=` cannot protect the provider instead: oomd ignores
//! it for swap kills in cgroups the user owns. Memory limits on the scope
//! would throttle or kill the provider together with its command.
//!
//! The provider starts its commands, so they begin in its leaf. A sweeper
//! moves each one into its own leaf shortly after it starts: within
//! [`FAST`] while the provider is active, and [`SLOW`] otherwise. Memory a
//! command allocated before it moved stays charged to the provider leaf,
//! because cgroup v2 does not move charges, so the provider is woken early
//! whenever it reports activity. Command leaves are checked for memory kills
//! and collected once they empty every [`SLOW`].
//!
//! The scope's `OOMPolicy=continue` keeps an OOM kill in one leaf from
//! stopping the whole scope, which `stop` would do.
//!
//! Every move names a process by number, so each is pinned with a pidfd and
//! confirmed to be in the provider leaf first. PIDs are issued cyclically,
//! so a process that exits between that check and the move cannot have its
//! number reissued in the meantime; the pidfd confirms afterwards that the
//! moved process was the one checked.
use std::{
    collections::HashMap,
    ffi::CString,
    fs, io,
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::DirBuilderExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const ROOT: &str = "/sys/fs/cgroup";
const PROVIDER: &str = "provider";
const COMMAND: &str = "command-";
/// Sweep interval while the provider reports activity, and for a while
/// after a command moves.
const FAST: Duration = Duration::from_millis(50);
const SLOW: Duration = Duration::from_secs(1);
const FAST_FOR: Duration = Duration::from_secs(5);
/// Ancestors examined to find the command a process belongs to.
const DEPTH: usize = 64;

/// What ended processes in a cgroup because memory ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Killer {
    /// systemd-oomd, because the system's memory, swap or memory pressure
    /// passed its limits.
    Oomd,
    /// The kernel's OOM killer.
    Kernel,
}

/// A command that memory exhaustion stopped while its provider kept running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandStop {
    pub(crate) killer: Killer,
    /// Processes ended, when the killer reports it.
    pub(crate) processes: Option<u64>,
}

/// A provider's cgroup, kept open so its OOM mark stays readable after
/// systemd removes it, and the sweeper that keeps its commands apart.
pub(crate) struct ProviderScope {
    provider: fs::File,
    /// Where the leaves go, once the scope is divided.
    layout: Option<Layout>,
    sweeper: Option<Sweeper>,
}

struct Layout {
    scope: PathBuf,
    /// The scope's cgroup, which ends the provider's commands with it.
    directory: fs::File,
    scope_membership: String,
    provider_membership: String,
}

impl ProviderScope {
    /// Divide a direct provider's scope and keep the commands `root`
    /// starts apart from it. `scope` is the provider's confirmed unit cgroup,
    /// relative to the cgroup2 root. A failed split leaves the scope as one
    /// leaf, the existing layout, with its reason for the caller to report.
    pub(crate) fn new(
        scope: &str,
        root: u32,
        wake: Option<UnixStream>,
    ) -> io::Result<(Self, Option<io::Error>)> {
        let (mut divided, unsplit) = Self::divide(scope, root)?;
        if unsplit.is_none() {
            divided.keep_commands_apart(root, Vec::new(), wake)?;
        }
        Ok((divided, unsplit))
    }

    /// A scope systemd did not delegate stays one leaf, the existing layout:
    /// its cgroups belong to systemd, not the provider's owner.
    pub(crate) fn whole(scope: &str) -> io::Result<Self> {
        Ok(Self {
            provider: open_directory(&cgroup_path(scope)?)?,
            layout: None,
            sweeper: None,
        })
    }

    /// Move everything in the scope, `owner` included, into the provider
    /// leaf. A holder that starts its provider later divides its scope first,
    /// so the provider begins in that leaf, then keeps commands apart.
    pub(crate) fn divide(scope: &str, owner: u32) -> io::Result<(Self, Option<io::Error>)> {
        let path = cgroup_path(scope)?;
        let directory = open_directory(&path)?;
        Ok(match split(&path, &directory, scope, owner) {
            Ok((provider, provider_membership)) => (
                Self {
                    provider,
                    layout: Some(Layout {
                        scope: path,
                        directory,
                        scope_membership: scope.to_owned(),
                        provider_membership,
                    }),
                    sweeper: None,
                },
                None,
            ),
            Err(error) => (
                Self {
                    provider: directory,
                    layout: None,
                    sweeper: None,
                },
                Some(error),
            ),
        })
    }

    /// Move each command `root` starts, with its own commands, into a leaf
    /// of its own. `kept` are other processes that stay with the provider,
    /// such as the holder that started it. Does nothing to an undivided
    /// scope or once started.
    pub(crate) fn keep_commands_apart(
        &mut self,
        root: u32,
        kept: Vec<u32>,
        wake: Option<UnixStream>,
    ) -> io::Result<()> {
        let Some(layout) = &self.layout else {
            return Ok(());
        };
        if self.sweeper.is_some() {
            return Ok(());
        }
        let leaves = Leaves {
            scope: layout.scope.clone(),
            scope_membership: layout.scope_membership.clone(),
            provider: self.provider.try_clone()?,
            provider_membership: layout.provider_membership.clone(),
            root,
            kept,
            next: 0,
            by_head: HashMap::new(),
        };
        self.sweeper = Some(Sweeper::start(leaves, wake)?);
        Ok(())
    }

    /// Whether memory exhaustion ended the provider's own processes. Call
    /// after the provider has exited; oomd marks a cgroup before killing it.
    pub(crate) fn provider_killer(&self) -> Option<Killer> {
        killer(&self.provider)
    }

    /// The provider reported activity: it may be about to start a command.
    pub(crate) fn poke(&self) {
        if let Some(sweeper) = &self.sweeper {
            sweeper.poke();
        }
    }

    /// Commands that memory exhaustion stopped since the last call.
    pub(crate) fn take_command_stops(&self) -> Vec<CommandStop> {
        self.sweeper
            .as_ref()
            .map(|sweeper| {
                std::mem::take(
                    &mut *sweeper
                        .shared
                        .stops
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()),
                )
            })
            .unwrap_or_default()
    }

    /// Whether the provider's commands are kept in cgroups of their own.
    pub(crate) fn keeps_commands_apart(&self) -> bool {
        self.sweeper.is_some()
    }

    /// End the commands a divided scope's provider left: call once the
    /// provider was force-stopped. A memory kill of the provider's leaf
    /// spares its command leaves, and a command in a process group of its own
    /// outlives the provider's group signal, so it would run on, holding its
    /// memory, while the chat reads it as stopped. An undivided scope is left
    /// to systemd, as before.
    pub(crate) fn end(&mut self) -> io::Result<()> {
        // Stop moving processes before the scope ends under the sweeper.
        self.sweeper = None;
        let Some(layout) = &self.layout else {
            return Ok(());
        };
        match write_at(&layout.directory, "cgroup.kill", "1") {
            // systemd removed the scope once it emptied.
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ENODEV) =>
            {
                Ok(())
            }
            result => result,
        }
    }
}

pub(super) fn cgroup_path(relative: &str) -> io::Result<PathBuf> {
    let relative = relative.strip_prefix('/').unwrap_or(relative);
    if relative.is_empty()
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(io::Error::other("unexpected cgroup path"));
    }
    Ok(Path::new(ROOT).join(relative))
}

fn open_directory(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let mut statfs = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: the open descriptor and correctly sized output remain live.
    if unsafe { libc::fstatfs(file.as_raw_fd(), statfs.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatfs initialized the complete statfs above.
    if unsafe { statfs.assume_init() }.f_type != libc::CGROUP2_SUPER_MAGIC {
        return Err(io::Error::other("not a cgroup2 directory"));
    }
    Ok(file)
}

/// Move the scope's processes into a `provider` leaf, then enable the
/// memory controller for its children so each leaf reports its own use.
/// Returns the provider leaf and its membership path.
fn split(
    path: &Path,
    scope: &fs::File,
    membership: &str,
    root: u32,
) -> io::Result<(fs::File, String)> {
    let controllers = read_at(scope, "cgroup.controllers")?;
    let wanted: Vec<_> = ["memory", "pids"]
        .into_iter()
        .filter(|name| controllers.split_whitespace().any(|have| have == *name))
        .collect();
    if !wanted.contains(&"memory") {
        return Err(io::Error::other("memory controller not delegated"));
    }
    let provider_path = path.join(PROVIDER);
    fs::DirBuilder::new().mode(0o755).create(&provider_path)?;
    let provider = open_directory(&provider_path)?;
    let provider_membership = format!("{membership}/{PROVIDER}");
    let result = (|| {
        // Processes the provider starts meanwhile land in the scope too;
        // enabling controllers fails until none remain there.
        for _ in 0..16 {
            for pid in procs(scope)? {
                // The provider is the caller's unreaped child, and its
                // early helpers are moved under the same pidfd proof.
                if let Err(error) = migrate(pid, membership, &provider) {
                    if pid == root {
                        return Err(error);
                    }
                }
            }
            let request = wanted
                .iter()
                .map(|name| format!("+{name}"))
                .collect::<Vec<_>>()
                .join(" ");
            match write_at(scope, "cgroup.subtree_control", &request) {
                Ok(()) => return Ok(()),
                Err(error) if error.raw_os_error() == Some(libc::EBUSY) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other("provider scope kept gaining processes"))
    })();
    if let Err(error) = result {
        // Restore the single leaf. No controller was enabled, so the scope
        // may hold processes again.
        if let Ok(pids) = procs(&provider) {
            for pid in pids {
                let _ = migrate(pid, &provider_membership, scope);
            }
        }
        let _ = fs::remove_dir(&provider_path);
        return Err(error);
    }
    Ok((provider, provider_membership))
}

fn read_at(directory: &fs::File, name: &str) -> io::Result<String> {
    let file = open_at(directory, name, libc::O_RDONLY)?;
    let mut contents = String::new();
    io::Read::read_to_string(&mut fs::File::from(file), &mut contents)?;
    Ok(contents)
}

fn write_at(directory: &fs::File, name: &str, contents: &str) -> io::Result<()> {
    let file = open_at(directory, name, libc::O_WRONLY)?;
    // cgroupfs takes one request per write.
    fs::File::from(file).write_all(contents.as_bytes())
}

fn open_at(directory: &fs::File, name: &str, flags: i32) -> io::Result<OwnedFd> {
    let name = CString::new(name).map_err(io::Error::other)?;
    // SAFETY: the directory descriptor and NUL-terminated name remain live.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat transferred this new owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

fn procs(directory: &fs::File) -> io::Result<Vec<u32>> {
    Ok(read_at(directory, "cgroup.procs")?
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect())
}

/// The cgroup a process belongs to, relative to the cgroup2 root.
fn membership(pid: u32) -> io::Result<String> {
    let contents = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    contents
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("unified cgroup membership unavailable"))
}

fn parent(pid: u32) -> io::Result<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // The command name is parenthesized and may itself contain ") ".
    let after = stat
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::other("malformed process status"))?
        .1;
    after
        .split_whitespace()
        .nth(1)
        .and_then(|ppid| ppid.parse().ok())
        .ok_or_else(|| io::Error::other("malformed process status"))
}

struct Pidfd(OwnedFd);

impl Pidfd {
    fn open(pid: u32) -> io::Result<Self> {
        // SAFETY: pidfd_open takes a PID and flags and returns a new descriptor.
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful pidfd_open returned a new descriptor owned here.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(descriptor as i32) }))
    }

    /// Whether the process has not yet exited.
    fn running(&self) -> bool {
        let mut descriptor = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd and the owned pidfd remain live.
        unsafe { libc::poll(&mut descriptor, 1, 0) == 0 }
    }
}

/// Move one process from the cgroup `from` to `to`, only while it is
/// provably the process seen in `from`.
fn migrate(pid: u32, from: &str, to: &fs::File) -> io::Result<()> {
    let process = Pidfd::open(pid)?;
    // Read while the pidfd shows it running, so the number named it.
    let current = membership(pid)?;
    if !process.running() {
        return Err(io::Error::from_raw_os_error(libc::ESRCH));
    }
    if current != from {
        return Err(io::Error::other("process left the expected cgroup"));
    }
    write_at(to, "cgroup.procs", &pid.to_string())?;
    if !process.running() {
        tracing::warn!(
            pid,
            "a process exited while it was moved to its command cgroup"
        );
    }
    Ok(())
}

fn killer(directory: &fs::File) -> Option<Killer> {
    if oomd_marked(directory) {
        return Some(Killer::Oomd);
    }
    kernel_kills(directory)
        .filter(|count| *count > 0)
        .map(|_| Killer::Kernel)
}

/// systemd-oomd marks a cgroup before it kills it; the mark stays readable
/// through an open descriptor after systemd removes the cgroup.
fn oomd_marked(directory: &fs::File) -> bool {
    xattr_count(directory, c"user.oomd_ooms").is_some_and(|count| count > 0)
}

/// Processes of this cgroup itself that the kernel's OOM killer ended.
/// Readable only while the cgroup exists.
fn kernel_kills(directory: &fs::File) -> Option<u64> {
    read_at(directory, "memory.events.local")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("oom_kill ")?.trim().parse().ok())
}

fn xattr_count(directory: &fs::File, name: &std::ffi::CStr) -> Option<u64> {
    let mut value = [0u8; 32];
    // SAFETY: the descriptor, NUL-terminated name and bounded buffer remain live.
    let length = unsafe {
        libc::fgetxattr(
            directory.as_raw_fd(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    if length < 0 {
        return None;
    }
    std::str::from_utf8(&value[..length as usize])
        .ok()?
        .trim_end_matches('\0')
        .trim()
        .parse()
        .ok()
}

struct Shared {
    stop: AtomicBool,
    /// Milliseconds since `epoch` until which sweeps stay frequent.
    fast_until: AtomicU64,
    epoch: Instant,
    stops: Mutex<Vec<CommandStop>>,
    wake: Mutex<Option<UnixStream>>,
}

impl Shared {
    fn hasten(&self) -> bool {
        let until = (self.epoch.elapsed() + FAST_FOR).as_millis() as u64;
        let previous = self.fast_until.swap(until, Ordering::AcqRel);
        previous <= self.epoch.elapsed().as_millis() as u64
    }

    fn interval(&self) -> Duration {
        if self.fast_until.load(Ordering::Acquire) > self.epoch.elapsed().as_millis() as u64 {
            FAST
        } else {
            SLOW
        }
    }
}

struct Sweeper {
    shared: Arc<Shared>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Sweeper {
    fn start(mut leaves: Leaves, wake: Option<UnixStream>) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            // The provider is starting and may start commands at once.
            fast_until: AtomicU64::new(FAST_FOR.as_millis() as u64),
            epoch: Instant::now(),
            stops: Mutex::new(Vec::new()),
            wake: Mutex::new(wake),
        });
        let sweep = shared.clone();
        let thread = thread::Builder::new()
            .name("cagents-commands".into())
            .spawn(move || {
                let mut last_collect = Instant::now();
                while !sweep.stop.load(Ordering::Acquire) {
                    let moved = leaves.sweep();
                    // Moving a new command is urgent; noticing one that
                    // ended is not, and reads every leaf.
                    let stops = if last_collect.elapsed() >= SLOW {
                        last_collect = Instant::now();
                        leaves.collect()
                    } else {
                        Vec::new()
                    };
                    if moved {
                        sweep.hasten();
                    }
                    if !stops.is_empty() {
                        sweep
                            .stops
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .extend(stops);
                        if let Some(wake) = sweep
                            .wake
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .as_mut()
                        {
                            let _ = wake.write(&[1]);
                        }
                    }
                    thread::park_timeout(sweep.interval());
                }
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    fn poke(&self) {
        if self.shared.hasten()
            && let Some(thread) = &self.thread
        {
            thread.thread().unpark();
        }
    }
}

impl Drop for Sweeper {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

struct Leaf {
    directory: fs::File,
    path: PathBuf,
    membership: String,
    /// Kernel OOM kills already reported.
    kernel_kills: u64,
}

struct Leaves {
    scope: PathBuf,
    /// Membership paths are relative to the cgroup2 root, as /proc reports.
    scope_membership: String,
    provider: fs::File,
    provider_membership: String,
    root: u32,
    /// Processes other than the provider that stay in its leaf.
    kept: Vec<u32>,
    next: u64,
    /// Command leaves by the provider child that heads them.
    by_head: HashMap<u32, Leaf>,
}

impl Leaves {
    /// Move every process other than the provider out of its leaf. Returns
    /// whether any moved.
    fn sweep(&mut self) -> bool {
        let Ok(pids) = procs(&self.provider) else {
            return false;
        };
        let mut moved = false;
        let commands: Vec<_> = pids
            .into_iter()
            .filter(|pid| *pid != self.root && !self.kept.contains(pid))
            .collect();
        for pid in commands {
            let head = match self.head(pid) {
                Head::Leaf(head) => head,
                Head::Child(head) => {
                    if !self.by_head.contains_key(&head) {
                        match self.create() {
                            Ok(leaf) => {
                                self.by_head.insert(head, leaf);
                            }
                            Err(error) => {
                                tracing::debug!(%error, "command cgroup unavailable");
                                continue;
                            }
                        }
                    }
                    head
                }
            };
            let Some(leaf) = self.by_head.get(&head) else {
                continue;
            };
            match migrate(pid, &self.provider_membership, &leaf.directory) {
                Ok(()) => moved = true,
                Err(error) => tracing::debug!(pid, %error, "command stayed in the provider cgroup"),
            }
        }
        moved
    }

    /// The command a process belongs to: the provider child heading its
    /// ancestry, or the leaf an ancestor already moved to.
    fn head(&self, pid: u32) -> Head {
        let mut current = pid;
        for _ in 0..DEPTH {
            let Ok(next) = parent(current) else {
                break;
            };
            // A holder that reaps orphans adopts commands too.
            if next == self.root || self.kept.contains(&next) {
                return Head::Child(current);
            }
            match membership(next) {
                Ok(path) if path == self.provider_membership => current = next,
                Ok(path) => {
                    return self
                        .by_head
                        .iter()
                        .find(|(_, leaf)| leaf.membership == path)
                        .map_or(Head::Child(pid), |(head, _)| Head::Leaf(*head));
                }
                Err(_) => break,
            }
        }
        // Orphaned or foreign ancestry: its own command.
        Head::Child(pid)
    }

    fn create(&mut self) -> io::Result<Leaf> {
        loop {
            self.next += 1;
            let name = format!("{COMMAND}{}", self.next);
            let path = self.scope.join(&name);
            match fs::DirBuilder::new().mode(0o755).create(&path) {
                Ok(()) => {
                    let directory = open_directory(&path)?;
                    let membership = format!("{}/{name}", self.scope_membership);
                    return Ok(Leaf {
                        directory,
                        path,
                        membership,
                        kernel_kills: 0,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    /// Remove command leaves that emptied, reporting memory kills first.
    fn collect(&mut self) -> Vec<CommandStop> {
        let mut stops = Vec::new();
        self.by_head.retain(|_, leaf| {
            let populated = read_at(&leaf.directory, "cgroup.events")
                .map(|events| events.lines().any(|line| line == "populated 1"))
                .unwrap_or(false);
            if !populated && oomd_marked(&leaf.directory) {
                // oomd ends the whole leaf.
                stops.push(CommandStop {
                    killer: Killer::Oomd,
                    processes: xattr_count(&leaf.directory, c"user.oomd_kill"),
                });
            } else if let Some(kills) = kernel_kills(&leaf.directory)
                && kills > leaf.kernel_kills
            {
                // The kernel ends single processes, which may leave the
                // rest of a command running: report each as it happens.
                stops.push(CommandStop {
                    killer: Killer::Kernel,
                    processes: Some(kills - leaf.kernel_kills),
                });
                leaf.kernel_kills = kills;
            }
            if populated {
                return true;
            }
            match fs::remove_dir(&leaf.path) {
                Ok(()) => false,
                // Repopulated meanwhile: keep it.
                Err(error) if error.raw_os_error() == Some(libc::EBUSY) => true,
                Err(_) => false,
            }
        });
        stops
    }
}

enum Head {
    /// A provider child, heading a new or existing command leaf.
    Child(u32),
    /// The command leaf of the given head, which an ancestor already joined.
    Leaf(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_paths_stay_below_the_cgroup_root() {
        assert_eq!(
            cgroup_path("/user.slice/app.slice/cagents-session-x.scope").unwrap(),
            Path::new("/sys/fs/cgroup/user.slice/app.slice/cagents-session-x.scope")
        );
        for refused in ["/", "", "/a/../b", "/a//b", "/a/./b"] {
            assert!(cgroup_path(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn a_parent_is_read_after_a_command_name_holding_parentheses() {
        let me = std::process::id();
        // SAFETY: getppid has no preconditions.
        let expected = unsafe { libc::getppid() } as u32;
        assert_eq!(parent(me).unwrap(), expected);
    }

    #[test]
    fn sweeps_hasten_after_a_poke_and_settle_afterwards() {
        let shared = Shared {
            stop: AtomicBool::new(false),
            fast_until: AtomicU64::new(0),
            epoch: Instant::now() - FAST_FOR * 2,
            stops: Mutex::new(Vec::new()),
            wake: Mutex::new(None),
        };
        assert_eq!(shared.interval(), SLOW);
        assert!(shared.hasten(), "the first poke wakes a slow sweeper");
        assert!(!shared.hasten(), "later pokes only extend the fast period");
        assert_eq!(shared.interval(), FAST);
    }
}
