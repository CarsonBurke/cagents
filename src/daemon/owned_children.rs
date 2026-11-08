//! Descendant cleanup for dedicated process owners, never the daemon itself.
use crate::daemon::holder_registry::Process;
use std::{
    io,
    os::fd::{FromRawFd, OwnedFd},
    process::Child,
    time::{Duration, Instant},
};

/// What closing a provider does with the work it leaves behind, such as a
/// server a command started with `nohup` or `setsid`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DetachedWork {
    /// Stop everything the provider started. Forced kills and unattended
    /// executions do, and so does every close that has not decided otherwise.
    #[default]
    Stop,
    /// End only the provider, the way quitting its own CLI does, and leave
    /// running whatever it leaves running.
    Spare,
}

/// Processes pinned by pidfd, so a process that later reuses one's pid is
/// never signalled in its place.
#[derive(Default)]
struct Pinned {
    fds: Vec<OwnedFd>,
    pids: Vec<i32>,
}

impl Pinned {
    /// Pins each of `processes` that is still that exact process, exited or
    /// not; one whose pid now names another process is skipped.
    fn pin(processes: &[Process]) -> io::Result<Self> {
        let mut pinned = Self::default();
        for process in processes {
            if !process.is_current() {
                continue;
            }
            // SAFETY: pidfd_open takes scalar arguments and returns a new
            // owned descriptor or -1 without writing through any pointer.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, process.pid, 0) };
            if fd == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return Err(error);
            }
            // SAFETY: a successful pidfd_open returned this fresh descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
            // The pid may have been reused between the check and the open.
            if process.is_current() {
                pinned.fds.push(fd);
                pinned.pids.push(process.pid);
            }
        }
        Ok(pinned)
    }

    /// Whether every pinned process has exited, reaped or not.
    fn exited(&self) -> bool {
        use std::os::fd::AsRawFd;
        self.fds.iter().all(|fd| {
            let mut event = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one live pollfd; a pidfd is readable once its process
            // has exited.
            let ready = unsafe { libc::poll(&mut event, 1, 0) };
            ready == 1
        })
    }

    /// Sends `signal` to each pinned process; signal 0 only reports which
    /// still exist, unreaped zombies included.
    fn signal(&self, signal: libc::c_int) -> bool {
        use std::os::fd::AsRawFd;
        let mut any = false;
        for fd in &self.fds {
            // SAFETY: the descriptor is live, and a null siginfo with zero
            // flags is supported.
            any |= unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            } == 0;
        }
        any
    }
}

/// Used only in the dedicated, single-threaded subreaper process. It owns every
/// child of that process; this cleanup must never run inside the daemon.
pub(crate) struct OwnedChildren {
    child: Option<Child>,
    group_signalled: bool,
    detached: DetachedWork,
    /// More of the provider's own processes, such as a server its launcher
    /// started, which end with it even when its detached work is spared.
    provider: Pinned,
    /// A server its launcher left to this process, and whether reaping found
    /// it killed by `SIGKILL` before teardown decided anything.
    server: Option<Process>,
    server_killed: std::cell::Cell<bool>,
}

impl OwnedChildren {
    /// The caller must be the sole child reaper in a dedicated subreaper process.
    /// No other thread or signal handler may reap this child or descendants.
    pub(crate) fn new(child: Child) -> Self {
        Self {
            child: Some(child),
            group_signalled: false,
            detached: DetachedWork::Stop,
            provider: Pinned::default(),
            server: None,
            server_killed: Default::default(),
        }
    }

    /// Names the server the provider's launcher started. If it becomes this
    /// process's child and is killed, the kill is remembered when it is
    /// reaped, as its status is gone afterwards.
    pub(crate) fn watch_server(&mut self, server: Process) {
        self.server = Some(server);
    }

    /// Decides, before teardown first signals anything, what it does with
    /// the work the provider leaves behind. `provider` names more of the
    /// provider's own processes, such as a server its launcher started.
    ///
    /// A provider something outside this process killed, such as the system
    /// for memory, spares nothing: what it left would hold the memory it
    /// needed, with no agent left to use it.
    pub(crate) fn decide_detached(
        &mut self,
        detached: DetachedWork,
        provider: &[Process],
    ) -> io::Result<()> {
        // Stopping can follow a sparing teardown; sparing can't follow one.
        if detached == DetachedWork::Spare && self.group_signalled {
            return Ok(());
        }
        self.provider = Pinned::pin(provider)?;
        self.detached = if self.server_killed.get() || self.killed() {
            DetachedWork::Stop
        } else {
            detached
        };
        Ok(())
    }

    /// Whether the provider, or one of its `provider` processes this process
    /// adopted, has exited by `SIGKILL`. Before teardown has signalled it,
    /// that came from outside this process.
    fn killed(&self) -> bool {
        let Some(child) = &self.child else {
            return false;
        };
        std::iter::once(child.id() as i32)
            .chain(self.provider.pids.iter().copied())
            .any(killed_unreaped)
    }

    pub(crate) fn id(&self) -> i32 {
        self.child.as_ref().expect("owned provider").id() as i32
    }

    pub(crate) fn exited(&self) -> bool {
        let Some(child) = &self.child else {
            return true;
        };
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut status,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            ) == 0
                && status.si_pid() != 0
        }
    }

    /// Reap adopted tools without consuming the provider leader's exit status.
    /// Keeping that leader unreaped also reserves its process-group identifier
    /// until `terminate` has sent the group's final signal.
    pub(crate) fn reap_exited_descendants(&self) -> io::Result<()> {
        for pid in direct_children()? {
            if self
                .child
                .as_ref()
                .is_some_and(|child| child.id() == pid as u32)
            {
                continue;
            }
            // Its status is about to be consumed: remember a killed server.
            if self.server.as_ref().is_some_and(|server| {
                server.pid == pid && killed_unreaped(pid) && server.is_current()
            }) {
                self.server_killed.set(true);
            }
            loop {
                // This dedicated owner is the only reaper. Live children are
                // untouched, and an exited child's PID cannot be reused before
                // this wait consumes its status.
                match unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } {
                    result if result >= 0 => break,
                    _ => {
                        let error = io::Error::last_os_error();
                        match error.raw_os_error() {
                            Some(libc::EINTR) => continue,
                            Some(libc::ECHILD) => break,
                            _ => return Err(error),
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn terminate(&mut self, timeout: Duration) -> io::Result<bool> {
        if !self.group_signalled {
            if self.detached == DetachedWork::Spare {
                // Let the provider end its own session, as quitting its CLI
                // would, before anything is forced.
                self.signal_provider(libc::SIGTERM);
                let grace = Instant::now() + crate::chat::TERMINATE_GRACE.min(timeout);
                while !(self.exited() && self.provider.exited()) && Instant::now() < grace {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            self.signal_provider(libc::SIGKILL);
            self.group_signalled = true;
        }
        // The grace above is the provider's own; reaping gets the timeout.
        let deadline = Instant::now() + timeout;
        match self.detached {
            DetachedWork::Stop => self.stop_children(deadline),
            DetachedWork::Spare => self.end_provider(deadline),
        }
    }

    fn signal_provider(&self, signal: libc::c_int) {
        if let Some(child) = &self.child {
            // The group leader is still our unreaped child, so this number
            // cannot have been recycled. Never use it again after reaping.
            unsafe {
                libc::kill(-(child.id() as i32), signal);
            }
        }
        self.provider.signal(signal);
    }

    fn stop_children(&mut self, deadline: Instant) -> io::Result<bool> {
        loop {
            for pid in direct_children()? {
                // No other thread or handler reaps children in this process.
                // A child that exits stays a zombie until the loop below.
                if unsafe { libc::kill(pid, libc::SIGKILL) } == -1
                    && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                {
                    return Err(io::Error::last_os_error());
                }
            }
            if self.reap()? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Reaps the provider and its own processes, and every other child that
    /// has exited, without signalling what is still running: when this
    /// process exits, that work is adopted further up and keeps running.
    fn end_provider(&mut self, deadline: Instant) -> io::Result<bool> {
        loop {
            // A server its launcher left behind may be adopted here only now.
            self.provider.signal(libc::SIGKILL);
            if self.reap()? || (self.child.is_none() && !self.provider.signal(0)) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Reaps every child that has exited, and whether none remain.
    fn reap(&mut self) -> io::Result<bool> {
        loop {
            let mut status = 0;
            match unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) } {
                pid if pid > 0 => {
                    if self
                        .child
                        .as_ref()
                        .is_some_and(|child| child.id() == pid as u32)
                    {
                        self.child.take();
                    }
                }
                0 => return Ok(false),
                _ if io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) => {
                    return Ok(true);
                }
                _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                _ => return Err(io::Error::last_os_error()),
            }
        }
    }
}

/// Whether `pid`, an unreaped child of this process, exited by `SIGKILL`.
/// WNOWAIT leaves its status for the reap; any other process fails with
/// ECHILD.
fn killed_unreaped(pid: i32) -> bool {
    let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer is live and writable.
    unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut status,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) == 0
            && status.si_pid() != 0
            && status.si_code == libc::CLD_KILLED
            && status.si_status() == libc::SIGKILL
    }
}

pub(crate) fn direct_children() -> io::Result<std::collections::BTreeSet<i32>> {
    let mut children = std::collections::BTreeSet::new();
    for task in std::fs::read_dir("/proc/self/task")? {
        let path = task?.path().join("children");
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        children.extend(
            raw.split_whitespace()
                .filter_map(|id| id.parse::<i32>().ok()),
        );
    }
    Ok(children)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{os::unix::process::CommandExt, path::Path, process::Command};

    /// A provider in its own process group, as a provider's CLI runs:
    /// - `detached`: work it detached at once, adopted by this subreaper;
    /// - `running`: a command it runs in a session of its own, as Claude Code
    ///   runs each Bash command, and stops itself when it is told to quit;
    /// - `left`: a process it started and leaves running when it quits.
    pub(crate) fn provider_with_detached_work(root: &Path) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(
                r#"record() { echo "$2" > "$1.tmp"; mv "$1.tmp" "$1"; }
(setsid /bin/sleep 600 & record "$1" $!)
setsid /bin/sleep 601 & running=$!
setsid /bin/sleep 604 & left=$!
trap 'kill $running; exit' TERM
record "$3" $left; record "$2" $running
/bin/sleep 602 & wait"#,
            )
            .args([
                "provider",
                root.join("detached").to_str().unwrap(),
                root.join("running").to_str().unwrap(),
                root.join("left").to_str().unwrap(),
            ])
            .process_group(0);
        command
    }

    pub(crate) struct Work {
        pub(crate) detached: i32,
        pub(crate) running: i32,
        pub(crate) left: i32,
    }

    pub(crate) fn parent(pid: i32) -> Option<i32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat[stat.rfind(')')? + 2..].split(' ').nth(1)?.parse().ok()
    }

    fn session(pid: i32) -> Option<i32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat[stat.rfind(')')? + 2..].split(' ').nth(3)?.parse().ok()
    }

    /// The provider's work, once its detached process is adopted by `owner`
    /// and it is ready to stop its command.
    pub(crate) fn detached_work_under(root: &Path, owner: i32) -> Work {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let pid = |name| {
                std::fs::read_to_string(root.join(name))
                    .ok()?
                    .trim()
                    .parse::<i32>()
                    .ok()
            };
            // Each is out of the provider's group only once it has called
            // setsid, which may come after its pid is recorded.
            if let (Some(detached), Some(running), Some(left)) =
                (pid("detached"), pid("running"), pid("left"))
                && parent(detached) == Some(owner)
                && [detached, running, left]
                    .into_iter()
                    .all(|pid| session(pid) == Some(pid))
            {
                return Work {
                    detached,
                    running,
                    left,
                };
            }
            assert!(Instant::now() < deadline, "the provider did not detach");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn detached_work(root: &Path) -> Work {
        detached_work_under(root, std::process::id() as i32)
    }

    pub(crate) fn exists(pid: i32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }

    /// Whether `pid` is still running: neither gone nor an unreaped zombie.
    pub(crate) fn alive(pid: i32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|stat| !stat[stat.rfind(')').unwrap() + 2..].starts_with('Z'))
    }

    /// Whether `pid` is gone once `reap` has collected what has exited: a
    /// signalled process exits, and is adopted here, only shortly after.
    pub(crate) fn ends(pid: i32, mut reap: impl FnMut()) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while exists(pid) && Instant::now() < deadline {
            reap();
            std::thread::sleep(Duration::from_millis(10));
        }
        !exists(pid)
    }

    /// A process orphaned to this subreaper once it has been adopted here.
    fn orphan(root: &Path, name: &str) -> i32 {
        let file = root.join(name);
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(r#"setsid /bin/sleep 603 & echo $! > "$1.tmp"; mv "$1.tmp" "$1""#)
            .args(["orphan", file.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(status.success());
        let pid = std::fs::read_to_string(&file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while parent(pid) != Some(std::process::id() as i32) {
            assert!(Instant::now() < deadline, "the orphan was not adopted");
            std::thread::sleep(Duration::from_millis(10));
        }
        pid
    }

    fn subreaper() {
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    }

    #[test]
    fn an_ordinary_teardown_leaves_what_the_provider_leaves() {
        if crate::retained_fixture::run_isolated() {
            return;
        }
        // Stands in for the native holder: the subreaper owning the provider.
        subreaper();
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let provider = provider_with_detached_work(root.path()).spawn().unwrap();
        let work = detached_work(root.path());
        let mut children = OwnedChildren::new(provider);
        children.decide_detached(DetachedWork::Spare, &[]).unwrap();
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
        assert!(
            alive(work.detached) && alive(work.left),
            "what the provider left keeps running"
        );
        assert!(
            ends(work.running, || children.reap_exited_descendants().unwrap()),
            "the provider stopped its own command"
        );
        // A later decision to stop everything still can.
        children.decide_detached(DetachedWork::Stop, &[]).unwrap();
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
        assert!(!exists(work.detached) && !exists(work.left));
    }

    #[test]
    fn a_server_its_launcher_left_here_ends_with_the_provider() {
        if crate::retained_fixture::run_isolated() {
            return;
        }
        subreaper();
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let provider = provider_with_detached_work(root.path()).spawn().unwrap();
        // The provider's detached process stands in for the server its
        // launcher started and left to this subreaper.
        let work = detached_work(root.path());
        let mut children = OwnedChildren::new(provider);
        children
            .decide_detached(
                DetachedWork::Spare,
                &[Process::read(work.detached).unwrap()],
            )
            .unwrap();
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
        assert!(!exists(work.detached), "the server ends with its provider");
        assert!(alive(work.left));
        children.decide_detached(DetachedWork::Stop, &[]).unwrap();
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
    }

    #[test]
    fn a_force_stopped_provider_spares_nothing() {
        if crate::retained_fixture::run_isolated() {
            return;
        }
        subreaper();
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let provider = provider_with_detached_work(root.path()).spawn().unwrap();
        let work = detached_work(root.path());
        // As the system kills a provider for memory.
        unsafe { libc::kill(provider.id() as i32, libc::SIGKILL) };
        let mut children = OwnedChildren::new(provider);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !children.exited() {
            assert!(Instant::now() < deadline, "the provider outlived its kill");
            std::thread::sleep(Duration::from_millis(10));
        }
        children.decide_detached(DetachedWork::Spare, &[]).unwrap();
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
        assert!(!exists(work.detached) && !exists(work.running) && !exists(work.left));
    }

    #[test]
    fn a_killed_server_its_launcher_left_here_spares_nothing() {
        if crate::retained_fixture::run_isolated() {
            return;
        }
        subreaper();
        // Whether or not the idle reaper collected the server before the close.
        for reaped_first in [false, true] {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let provider = provider_with_detached_work(root.path()).spawn().unwrap();
            let work = detached_work(root.path());
            let server = Process::read(work.detached).unwrap();
            let detached = orphan(root.path(), "detached-work");
            let mut children = OwnedChildren::new(provider);
            children.watch_server(server.clone());
            unsafe { libc::kill(server.pid, libc::SIGKILL) };
            let deadline = Instant::now() + Duration::from_secs(10);
            while alive(server.pid) {
                assert!(Instant::now() < deadline, "the server outlived its kill");
                std::thread::sleep(Duration::from_millis(10));
            }
            if reaped_first {
                children.reap_exited_descendants().unwrap();
                assert!(!exists(server.pid));
            }
            children
                .decide_detached(DetachedWork::Spare, std::slice::from_ref(&server))
                .unwrap();
            assert!(children.terminate(Duration::from_secs(10)).unwrap());
            assert!(
                !exists(detached) && !exists(work.left) && !exists(server.pid),
                "reaped first: {reaped_first}"
            );
        }
    }

    #[test]
    fn a_teardown_that_did_not_decide_stops_everything() {
        if crate::retained_fixture::run_isolated() {
            return;
        }
        subreaper();
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let provider = provider_with_detached_work(root.path()).spawn().unwrap();
        let work = detached_work(root.path());
        let mut children = OwnedChildren::new(provider);
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
        assert!(!exists(work.detached) && !exists(work.running) && !exists(work.left));
    }
}
