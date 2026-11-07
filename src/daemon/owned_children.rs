//! Descendant cleanup for dedicated process owners, never the daemon itself.
use std::{
    io,
    os::fd::{FromRawFd, OwnedFd},
    process::Child,
    time::{Duration, Instant},
};

/// What closing a provider does with work it had already detached from
/// itself, such as a server a command started with `nohup` or `setsid`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DetachedWork {
    /// Stop it with the provider. Forced kills and unattended executions do,
    /// and so does every close that has not decided otherwise.
    #[default]
    Stop,
    /// Leave running what had left the provider before the close began.
    Spare,
}

/// Children of a subreaper that its teardown leaves running: work the
/// provider had detached before the close began. Each is pinned by a pidfd,
/// so a process that later reuses a spared pid is not spared.
#[derive(Default)]
pub(crate) struct Spared(Vec<(i32, OwnedFd)>);

impl Spared {
    pub(crate) const NOTHING: Self = Self(Vec::new());

    /// Pins `children`, which must be unreaped children of this process. One
    /// that has already been reaped is not spared.
    pub(crate) fn pin(children: impl IntoIterator<Item = i32>) -> io::Result<Self> {
        let mut spared = Vec::new();
        for pid in children {
            // SAFETY: pidfd_open takes scalar arguments and returns a new
            // owned descriptor or -1 without writing through any pointer.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return Err(error);
            }
            // SAFETY: a successful pidfd_open returned this fresh descriptor.
            spared.push((pid, unsafe { OwnedFd::from_raw_fd(fd as i32) }));
        }
        Ok(Self(spared))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `pid` is still the process pinned under it. A spared child
    /// that has exited but is unreaped still is; once reaped it no longer is.
    pub(crate) fn spares(&self, pid: i32) -> bool {
        self.0.iter().any(|(spared, fd)| {
            use std::os::fd::AsRawFd;
            // SAFETY: the descriptor is live; signal 0 only checks existence,
            // and a null siginfo with zero flags is supported.
            *spared == pid
                && unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        0,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                } == 0
        })
    }

    /// Whether every child is spared, so teardown has nothing left to stop.
    pub(crate) fn spares_all(&self, children: &std::collections::BTreeSet<i32>) -> bool {
        children.iter().all(|pid| self.spares(*pid))
    }
}

/// Used only in the dedicated, single-threaded subreaper process. It owns every
/// child of that process; this cleanup must never run inside the daemon.
pub(crate) struct OwnedChildren {
    child: Option<Child>,
    group_signalled: bool,
    /// Pinned once, before the first teardown signal; `None` stops them all.
    spared: Option<Spared>,
}

impl OwnedChildren {
    /// The caller must be the sole child reaper in a dedicated subreaper process.
    /// No other thread or signal handler may reap this child or descendants.
    pub(crate) fn new(child: Child) -> Self {
        Self {
            child: Some(child),
            group_signalled: false,
            spared: None,
        }
    }

    /// Decides, before teardown first signals anything, what it does with
    /// the children this process had adopted from the provider: work the
    /// provider detached. Sparing pins them as they are now, so children
    /// adopted once the provider is stopping, such as its running commands,
    /// are still stopped; a caller pins as its close begins, before it
    /// interrupts the provider. A later decision cannot spare more than the
    /// first. `provider` names more of the provider's own processes, such as
    /// a server its launcher started, which are never spared.
    ///
    /// A provider something outside this process killed, such as the system
    /// for memory, spares nothing, even after its work was pinned: what it
    /// left would hold the memory it needed, with no agent left to use it.
    pub(crate) fn decide_detached(
        &mut self,
        detached: DetachedWork,
        provider: &[i32],
    ) -> io::Result<()> {
        match detached {
            DetachedWork::Stop => self.spared = None,
            DetachedWork::Spare if self.group_signalled => {}
            DetachedWork::Spare if self.killed(provider) => self.spared = None,
            DetachedWork::Spare if self.spared.is_none() => {
                let root = self.child.as_ref().map(|child| child.id() as i32);
                self.spared =
                    Some(Spared::pin(direct_children()?.into_iter().filter(|pid| {
                        Some(*pid) != root && !provider.contains(pid)
                    }))?);
            }
            DetachedWork::Spare => {}
        }
        Ok(())
    }

    /// Whether the provider, or one of its `provider` processes this process
    /// adopted, has exited by `SIGKILL`. Before teardown has signalled it,
    /// that came from outside this process.
    fn killed(&self, provider: &[i32]) -> bool {
        let Some(child) = &self.child else {
            return false;
        };
        std::iter::once(child.id() as i32)
            .chain(provider.iter().copied())
            .any(|pid| {
                let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
                // SAFETY: the buffer is live and writable; WNOWAIT leaves the
                // status for teardown's own reap. A process that isn't this
                // one's child fails with ECHILD.
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
            })
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
            // The group leader is still our unreaped child, so this number
            // cannot have been recycled. Never use it again after reaping.
            unsafe {
                libc::kill(-self.id(), libc::SIGKILL);
            }
            self.group_signalled = true;
        }
        let deadline = Instant::now() + timeout;
        let spared = self.spared.take().unwrap_or_default();
        let result = self.stop_children(&spared, deadline);
        self.spared = Some(spared).filter(|spared| !spared.is_empty());
        result
    }

    fn stop_children(&mut self, spared: &Spared, deadline: Instant) -> io::Result<bool> {
        loop {
            for pid in direct_children()? {
                if spared.spares(pid) {
                    continue;
                }
                // No other thread or handler reaps children in this process.
                // A child that exits stays a zombie until the loop below.
                if unsafe { libc::kill(pid, libc::SIGKILL) } == -1
                    && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                {
                    return Err(io::Error::last_os_error());
                }
            }
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
                    0 => break,
                    _ if io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) => {
                        return Ok(true);
                    }
                    _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {
                        continue;
                    }
                    _ => return Err(io::Error::last_os_error()),
                }
            }
            // Every child still running is one the close spared.
            if !spared.is_empty() && spared.spares_all(&direct_children()?) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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

    /// A provider in its own process group that has detached one process
    /// already, adopted by this subreaper, and is still running a command in
    /// a session of its own, as Claude Code runs each Bash command.
    pub(crate) fn provider_with_detached_work(root: &Path) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(
                r#"(setsid /bin/sleep 600 & echo $! > "$1.tmp"; mv "$1.tmp" "$1")
setsid /bin/sleep 601 & echo $! > "$2.tmp"; mv "$2.tmp" "$2"
exec /bin/sleep 602"#,
            )
            .args([
                "provider",
                root.join("detached").to_str().unwrap(),
                root.join("running").to_str().unwrap(),
            ])
            .process_group(0);
        command
    }

    pub(crate) fn parent(pid: i32) -> Option<i32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat[stat.rfind(')')? + 2..].split(' ').nth(1)?.parse().ok()
    }

    /// The detached and running pids, once the detached one is adopted here.
    pub(crate) fn detached_work(root: &Path) -> (i32, i32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let pid = |name| {
                std::fs::read_to_string(root.join(name))
                    .ok()?
                    .trim()
                    .parse::<i32>()
                    .ok()
            };
            if let (Some(detached), Some(running)) = (pid("detached"), pid("running"))
                && parent(detached) == Some(std::process::id() as i32)
            {
                return (detached, running);
            }
            assert!(Instant::now() < deadline, "the provider did not detach");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn exists(pid: i32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }

    /// A process orphaned to this subreaper, as interrupting a turn can
    /// orphan a command's descendants, once it has been adopted here.
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

    fn wait_exited(children: &OwnedChildren) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !children.exited() {
            assert!(Instant::now() < deadline, "the provider outlived its kill");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn an_ordinary_teardown_spares_only_work_detached_before_it() {
        if crate::retained_fixture::run_isolated() {
            return;
        }
        // Stands in for the native holder: the subreaper owning the provider.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let provider = provider_with_detached_work(root.path()).spawn().unwrap();
        let (detached, running) = detached_work(root.path());
        let mut children = OwnedChildren::new(provider);
        children.decide_detached(DetachedWork::Spare, &[]).unwrap();
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
        assert_eq!(
            parent(detached),
            Some(std::process::id() as i32),
            "work detached before the close keeps running"
        );
        assert!(
            !exists(running),
            "a command the provider was running when it stopped is stopped and reaped"
        );
        // Deciding again cannot spare what was adopted since, and a later
        // decision to stop everything does.
        children.decide_detached(DetachedWork::Spare, &[]).unwrap();
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
        assert!(exists(detached));
        children.decide_detached(DetachedWork::Stop, &[]).unwrap();
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
        assert!(!exists(detached), "stopping everything reaps it too");
    }

    #[test]
    fn a_force_stopped_provider_spares_nothing() {
        if crate::retained_fixture::run_isolated() {
            return;
        }
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let provider = provider_with_detached_work(root.path()).spawn().unwrap();
        let (detached, running) = detached_work(root.path());
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
        assert!(!exists(detached) && !exists(running));
    }

    #[test]
    fn what_a_close_orphans_after_pinning_still_stops() {
        if crate::retained_fixture::run_isolated() {
            return;
        }
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let provider = provider_with_detached_work(root.path()).spawn().unwrap();
        let (detached, running) = detached_work(root.path());
        let mut children = OwnedChildren::new(provider);
        // Pinned as the close begins, before the turn is interrupted.
        children.decide_detached(DetachedWork::Spare, &[]).unwrap();
        let interrupted = orphan(root.path(), "interrupted");
        children.decide_detached(DetachedWork::Spare, &[]).unwrap();
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
        assert!(
            exists(detached),
            "work detached before the close keeps running"
        );
        assert!(
            !exists(interrupted) && !exists(running),
            "what the close orphaned is stopped"
        );
        children.decide_detached(DetachedWork::Stop, &[]).unwrap();
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
        assert!(!exists(detached));
    }

    #[test]
    fn a_provider_killed_after_its_work_was_pinned_spares_nothing() {
        if crate::retained_fixture::run_isolated() {
            return;
        }
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let provider = provider_with_detached_work(root.path()).spawn().unwrap();
        let (detached, running) = detached_work(root.path());
        let mut children = OwnedChildren::new(provider);
        children.decide_detached(DetachedWork::Spare, &[]).unwrap();
        // The system kills it for memory while the close waits on its turn.
        unsafe { libc::kill(children.id(), libc::SIGKILL) };
        wait_exited(&children);
        children.decide_detached(DetachedWork::Spare, &[]).unwrap();
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
        assert!(!exists(detached) && !exists(running));
    }

    #[test]
    fn a_killed_server_its_launcher_left_here_spares_nothing() {
        if crate::retained_fixture::run_isolated() {
            return;
        }
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let provider = provider_with_detached_work(root.path()).spawn().unwrap();
        // The provider's detached process stands in for the server its
        // launcher started and left to this subreaper.
        let (server, running) = detached_work(root.path());
        let detached = orphan(root.path(), "detached-work");
        let mut children = OwnedChildren::new(provider);
        unsafe { libc::kill(server, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{server}/stat")).unwrap();
            if stat[stat.rfind(')').unwrap() + 2..].starts_with('Z') {
                break;
            }
            assert!(Instant::now() < deadline, "the server outlived its kill");
            std::thread::sleep(Duration::from_millis(10));
        }
        children
            .decide_detached(DetachedWork::Spare, &[server])
            .unwrap();
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
        assert!(!exists(detached) && !exists(running) && !exists(server));
    }

    #[test]
    fn a_teardown_that_did_not_decide_stops_detached_work() {
        if crate::retained_fixture::run_isolated() {
            return;
        }
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let provider = provider_with_detached_work(root.path()).spawn().unwrap();
        let (detached, running) = detached_work(root.path());
        let mut children = OwnedChildren::new(provider);
        assert!(children.terminate(Duration::from_secs(10)).unwrap());
        assert!(!exists(detached) && !exists(running));
    }
}
