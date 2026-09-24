use std::{
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MAX_CENSUS_PASSES: usize = 16;
const MAX_PROCESSES: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CloseProcess {
    pub pid: libc::pid_t,
    pub parent: libc::pid_t,
    pub group: libc::pid_t,
    pub session: libc::pid_t,
    pub state: u8,
    pub start: u64,
}

pub(super) struct RootIdentity {
    pub process: CloseProcess,
    pidfd: File,
    executable: Option<File>,
    kill_receipt: Arc<Mutex<bool>>,
}

impl RootIdentity {
    pub(super) fn capture(pid: libc::pid_t, executable: Option<File>) -> io::Result<Self> {
        let process = read_process(pid)?;
        let pidfd = open_target(&process)?;
        if !same_identity(&process, &read_process(pid)?) {
            return Err(changed());
        }
        Ok(Self {
            process,
            pidfd,
            executable,
            kill_receipt: Arc::new(Mutex::new(false)),
        })
    }

    pub(super) fn original_executable(&self) -> bool {
        self.executable.as_ref().is_some_and(|executable| {
            executable.metadata().ok().is_some_and(|expected| {
                executable_identity(self.process.pid).ok() == Some((expected.dev(), expected.ino()))
            })
        })
    }

    pub(super) fn intentionally_killed(&self) -> bool {
        *self
            .kill_receipt
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Pin the resolved launch image, not its pathname or an unowned inode
    /// number. Package replacement may unlink it while this shell keeps running;
    /// holding the file prevents recycling its identity for a different image.
    /// Do not capture /proc immediately after spawn: the exec-error pipe can
    /// close before the kernel exposes the child's new executable image.
    pub(super) fn launch_executable(command: &portable_pty::CommandBuilder) -> Option<File> {
        let program = Path::new(command.get_argv().first()?);
        let cwd = Path::new(command.get_cwd()?);
        if !cwd.is_dir() {
            return None;
        }
        let identity = |path: &Path| {
            let file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
                .open(path)
                .ok()?;
            let metadata = file.metadata().ok()?;
            let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
            // SAFETY: path is NUL-terminated and remains live for access.
            (metadata.is_file() && unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0)
                .then_some(file)
        };
        if program.is_absolute()
            || matches!(
                program.components().next(),
                Some(Component::CurDir | Component::ParentDir)
            )
        {
            return identity(&cwd.join(program));
        }
        std::env::split_paths(command.get_env("PATH")?)
            .find_map(|directory| identity(&cwd.join(directory).join(program)))
    }
}

fn executable_identity(pid: libc::pid_t) -> io::Result<(u64, u64)> {
    let metadata = fs::metadata(format!("/proc/{pid}/exe"))?;
    Ok((metadata.dev(), metadata.ino()))
}

struct FrozenProcess {
    identity: CloseProcess,
    pidfd: File,
    resume: bool,
}

/// Every successfully issued SIGSTOP immediately gains a resume-on-drop guard.
/// Already stopped/traced processes are never resumed by this operation.
impl Drop for FrozenProcess {
    fn drop(&mut self) {
        if self.resume {
            if let Err(error) = signal(&self.pidfd, libc::SIGCONT) {
                if error.raw_os_error() != Some(libc::ESRCH) {
                    tracing::error!(pid = self.identity.pid, %error, "failed to resume terminal process after close inspection");
                }
            }
        }
    }
}

pub(super) struct FrozenFamily {
    targets: Vec<FrozenProcess>,
    root_kill_receipt: Arc<Mutex<bool>>,
}

impl FrozenFamily {
    pub(super) fn capture(root: &RootIdentity, deadline: Instant) -> Result<Self, String> {
        let current = read_process(root.process.pid).map_err(|error| error.to_string())?;
        // The root's parent is its holder, and a holder that died left the
        // root reparented; the pidfd and start time already pin the root.
        let reparented = CloseProcess {
            parent: current.parent,
            ..root.process
        };
        if !same_identity(&current, &reparented) || current.session != current.pid {
            return Err("Terminal child identity changed".into());
        }
        let mut family = Self {
            targets: Vec::new(),
            root_kill_receipt: Arc::clone(&root.kill_receipt),
        };
        family.freeze(
            current,
            root.pidfd.try_clone().map_err(|error| error.to_string())?,
            deadline,
        )?;
        for _ in 0..MAX_CENSUS_PASSES {
            check_deadline(deadline)?;
            let processes = census(deadline)?;
            let owned = process_family(&root.process, &processes, &family.targets);
            let before = family.targets.len();
            for process in owned {
                if family
                    .targets
                    .iter()
                    .any(|known| same_identity(&known.identity, &process))
                {
                    continue;
                }
                let pidfd = match open_target(&process) {
                    Ok(pidfd) => pidfd,
                    Err(error)
                        if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error.to_string()),
                };
                family.freeze(process, pidfd, deadline)?;
            }
            // A complete census after every discovered member was stopped is
            // the fixed point: no member can now fork a job outside this set.
            if family.targets.len() == before {
                for target in &mut family.targets {
                    let process =
                        read_process(target.identity.pid).map_err(|error| error.to_string())?;
                    if !same_identity(&process, &target.identity)
                        || !quiescent(process.state)
                        || !threads_quiescent(process.pid, deadline)?
                    {
                        return Err("Terminal process changed while its family was stopped".into());
                    }
                    target.identity.state = process.state;
                }
                return Ok(family);
            }
        }
        Err("Terminal process family did not stabilize within the close inspection".into())
    }

    fn freeze(
        &mut self,
        process: CloseProcess,
        pidfd: File,
        deadline: Instant,
    ) -> Result<(), String> {
        check_deadline(deadline)?;
        let mut target = FrozenProcess {
            resume: false,
            identity: process,
            pidfd,
        };
        if !quiescent(target.identity.state) {
            signal(&target.pidfd, libc::SIGSTOP).map_err(|error| error.to_string())?;
            target.resume = true;
        }
        // Non-child descendants have no waitid stop notifications. This bounded
        // close-time acquisition loop is not a background process/activity poll.
        // Drop releases the stop on timeout, disappearance, or any read error.
        loop {
            check_deadline(deadline)?;
            let current = read_process(target.identity.pid).map_err(|error| error.to_string())?;
            if !same_identity(&current, &target.identity) {
                return Err("Terminal job identity changed".into());
            }
            if quiescent(current.state) && threads_quiescent(current.pid, deadline)? {
                target.identity.state = current.state;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.targets.push(target);
        Ok(())
    }

    pub(super) fn has_jobs(&self) -> bool {
        // Stopping the shell can delay reaping an already-finished prompt or
        // startup helper. Such zombies are ancestry, not active work. Keep
        // them in the census so live descendants still require confirmation.
        self.targets
            .iter()
            .skip(1)
            .any(|target| !matches!(target.identity.state, b'Z' | b'X'))
    }

    /// A foreground agent cannot authorize discarding unrelated background jobs.
    /// Permit its ancestry (shell/wrapper) and descendants, never sibling trees.
    pub(super) fn only_agent_tree(&self, agent: libc::pid_t) -> bool {
        let parents = self
            .targets
            .iter()
            .map(|target| (target.identity.pid, target.identity.parent))
            .collect::<std::collections::HashMap<_, _>>();
        if !parents.contains_key(&agent) {
            return false;
        }
        let mut ancestors = std::collections::HashSet::new();
        let mut current = agent;
        while let Some(&parent) = parents.get(&current) {
            if !ancestors.insert(current) {
                return false;
            }
            current = parent;
        }
        let mut descendants = std::collections::HashSet::from([agent]);
        let mut chain = Vec::new();
        for &pid in parents.keys() {
            if ancestors.contains(&pid) || descendants.contains(&pid) {
                continue;
            }
            chain.clear();
            current = pid;
            while !descendants.contains(&current) {
                if ancestors.contains(&current) || chain.len() >= parents.len() {
                    return false;
                }
                chain.push(current);
                let Some(&parent) = parents.get(&current) else {
                    return false;
                };
                current = parent;
            }
            descendants.extend(chain.iter().copied());
        }
        true
    }

    pub(super) fn kill(&mut self, deadline: Instant) -> Result<(), String> {
        check_deadline(deadline)?;
        // All targets remain stopped until every signal has been issued. No
        // numeric PID is ever signaled, even after a job exits or is reparented.
        for (index, target) in self.targets.iter_mut().enumerate() {
            // The root is always first. Serialize receipt publication with
            // exit observation, including upgrades which kill off-worker.
            let mut receipt = (index == 0).then(|| {
                self.root_kill_receipt
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
            });
            let live_root = receipt.is_some()
                && read_process(target.identity.pid).is_ok_and(|process| {
                    same_identity(&process, &target.identity)
                        && matches!(process.state, b'T' | b't')
                });
            match signal(&target.pidfd, libc::SIGKILL) {
                Ok(()) => {
                    target.resume = false;
                    if live_root && let Some(receipt) = receipt.as_mut() {
                        **receipt = true;
                    }
                }
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => target.resume = false,
                Err(error) => return Err(format!("Could not close terminal process: {error}")),
            }
        }
        Ok(())
    }
}

impl Drop for FrozenFamily {
    fn drop(&mut self) {
        // Resume children before their shell. Preserve pre-existing stopped jobs.
        while self.targets.pop().is_some() {}
    }
}

fn quiescent(state: u8) -> bool {
    matches!(state, b'T' | b't' | b'Z' | b'X')
}

fn thread_ids(pid: libc::pid_t, deadline: Instant) -> Result<Vec<libc::pid_t>, String> {
    let mut ids = Vec::new();
    for entry in fs::read_dir(format!("/proc/{pid}/task")).map_err(|error| error.to_string())? {
        check_deadline(deadline)?;
        let entry = entry.map_err(|error| error.to_string())?;
        let id = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
            .ok_or("Invalid terminal thread identity")?;
        ids.push(id);
        if ids.len() >= MAX_PROCESSES {
            return Err("Terminal thread census exceeded its safety bound".into());
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

fn threads_quiescent(pid: libc::pid_t, deadline: Instant) -> Result<bool, String> {
    let ids = thread_ids(pid, deadline)?;
    for &tid in &ids {
        check_deadline(deadline)?;
        let stat = match fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
        let process = parse_process(tid, &stat).ok_or("Invalid terminal thread identity")?;
        if !quiescent(process.state) {
            return Ok(false);
        }
    }
    // A stopped leader alone does not freeze its still-running worker threads.
    // Check the complete thread set again after every observed thread stopped.
    Ok(!ids.is_empty() && ids == thread_ids(pid, deadline)?)
}
fn check_deadline(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        Err("Close inspection expired; stopped processes were released".into())
    } else {
        Ok(())
    }
}
fn changed() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Terminal process identity changed",
    )
}
fn same_identity(a: &CloseProcess, b: &CloseProcess) -> bool {
    a.pid == b.pid
        && a.start == b.start
        && a.parent == b.parent
        && a.group == b.group
        && a.session == b.session
}

fn read_process(pid: libc::pid_t) -> io::Result<CloseProcess> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse_process(pid, &stat).ok_or_else(changed)
}
fn parse_process(pid: libc::pid_t, stat: &str) -> Option<CloseProcess> {
    let (_, fields) = stat.rsplit_once(") ")?;
    let mut fields = fields.split_whitespace();
    let state = fields.next()?.as_bytes().first().copied()?;
    let parent = fields.next()?.parse().ok()?;
    let group = fields.next()?.parse().ok()?;
    let session = fields.next()?.parse().ok()?;
    let start = fields.nth(15)?.parse().ok()?;
    Some(CloseProcess {
        pid,
        parent,
        group,
        session,
        start,
        state,
    })
}
fn census(deadline: Instant) -> Result<Vec<CloseProcess>, String> {
    let mut processes = Vec::new();
    for entry in fs::read_dir("/proc").map_err(|error| error.to_string())? {
        check_deadline(deadline)?;
        let entry = entry.map_err(|error| error.to_string())?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        match read_process(pid) {
            Ok(process) => processes.push(process),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Cannot inspect terminal jobs: {error}")),
        }
        if processes.len() >= MAX_PROCESSES {
            return Err("Process census exceeded its safety bound".into());
        }
    }
    Ok(processes)
}
fn process_family(
    root: &CloseProcess,
    processes: &[CloseProcess],
    frozen: &[FrozenProcess],
) -> Vec<CloseProcess> {
    use std::collections::{HashMap, HashSet};
    let mut children: HashMap<libc::pid_t, Vec<&CloseProcess>> = HashMap::new();
    for process in processes {
        children.entry(process.parent).or_default().push(process);
    }
    let mut family: Vec<_> = frozen
        .iter()
        .map(|target| target.identity.clone())
        .collect();
    if family.is_empty() {
        family.push(root.clone());
    }
    let mut seen: HashSet<_> = family.iter().map(|process| process.pid).collect();
    for process in processes {
        if process.session == root.session && seen.insert(process.pid) {
            family.push(process.clone());
        }
    }
    let mut index = 0;
    while index < family.len() {
        if let Some(descendants) = children.get(&family[index].pid) {
            for process in descendants {
                if seen.insert(process.pid) {
                    family.push((*process).clone());
                }
            }
        }
        index += 1;
    }
    family
}
fn open_target(process: &CloseProcess) -> io::Result<File> {
    // SAFETY: successful pidfd_open yields a fresh owned descriptor.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, process.pid, 0) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd as libc::c_int) };
    if !same_identity(process, &read_process(process.pid)?) {
        return Err(changed());
    }
    Ok(file)
}
pub(super) fn signal(pidfd: &File, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: the owned pidfd pins one process and null siginfo uses ordinary delivery.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_agent_authority_excludes_unrelated_background_jobs() {
        let target = |pid, parent| FrozenProcess {
            identity: CloseProcess {
                pid,
                parent,
                group: pid,
                session: 10,
                state: b'T',
                start: 1,
            },
            pidfd: File::open("/dev/null").unwrap(),
            resume: false,
        };
        let mut family = FrozenFamily {
            targets: vec![
                target(10, 1),
                target(11, 10),
                target(12, 11),
                target(14, 12),
            ],
            root_kill_receipt: Arc::new(Mutex::new(false)),
        };
        assert!(family.only_agent_tree(11));
        family.targets.push(target(13, 10));
        assert!(!family.only_agent_tree(11));
        assert!(!family.only_agent_tree(99));
    }

    #[test]
    fn finished_helpers_are_not_work_but_live_descendants_and_stopped_jobs_are() {
        let root = CloseProcess {
            pid: 10,
            parent: 1,
            group: 10,
            session: 10,
            state: b'T',
            start: 1,
        };
        let helper = CloseProcess {
            pid: 11,
            parent: 10,
            group: 11,
            session: 11,
            state: b'Z',
            ..root.clone()
        };
        let job = CloseProcess {
            pid: 12,
            parent: 11,
            group: 12,
            session: 12,
            state: b'T',
            ..root.clone()
        };
        let frozen = |processes: &[CloseProcess]| FrozenFamily {
            targets: process_family(&root, processes, &[])
                .into_iter()
                .map(|identity| FrozenProcess {
                    identity,
                    pidfd: File::open("/dev/null").unwrap(),
                    resume: false,
                })
                .collect(),
            root_kill_receipt: Arc::new(Mutex::new(false)),
        };
        assert!(!frozen(&[helper.clone()]).has_jobs());
        assert!(frozen(&[helper.clone(), job.clone()]).has_jobs());
        let background = CloseProcess {
            parent: root.pid,
            ..job
        };
        assert!(frozen(&[helper, background]).has_jobs());
    }
    #[test]
    fn census_keeps_other_sessions_out_but_includes_background_and_detached_descendants() {
        let root = CloseProcess {
            pid: 10,
            parent: 1,
            group: 10,
            session: 10,
            start: 100,
            state: b'S',
        };
        let background = CloseProcess {
            pid: 11,
            parent: 10,
            group: 11,
            ..root.clone()
        };
        let detached = CloseProcess {
            pid: 12,
            parent: 11,
            group: 12,
            session: 12,
            ..root.clone()
        };
        let grandchild = CloseProcess {
            pid: 13,
            parent: 12,
            group: 12,
            session: 12,
            ..root.clone()
        };
        let other = CloseProcess {
            pid: 20,
            parent: 1,
            group: 20,
            session: 20,
            ..root.clone()
        };
        let family = process_family(&root, &[grandchild, other, detached, background], &[]);
        let mut pids: Vec<_> = family.iter().map(|process| process.pid).collect();
        pids.sort_unstable();
        assert_eq!(pids, [10, 11, 12, 13]);
    }
    #[test]
    fn identity_rejects_pid_reuse_without_confusing_a_stop_with_replacement() {
        let root = CloseProcess {
            pid: 10,
            parent: 1,
            group: 10,
            session: 10,
            start: 100,
            state: b'S',
        };
        assert!(!same_identity(
            &root,
            &CloseProcess {
                start: 101,
                ..root.clone()
            }
        ));
        assert!(same_identity(
            &root,
            &CloseProcess {
                state: b'T',
                ..root.clone()
            }
        ));
        assert!(quiescent(b'T'));
        assert!(quiescent(b't'));
        assert!(!quiescent(b'S'));
    }
    #[test]
    fn proc_parser_handles_parentheses_inside_names_and_rejects_partial_identity() {
        let mut fields = vec!["0"; 20];
        fields[0] = "S";
        fields[1] = "1";
        fields[2] = "10";
        fields[3] = "10";
        fields[19] = "998877";
        let parsed =
            parse_process(10, &format!("10 (shell ) nested) {}", fields.join(" "))).unwrap();
        assert_eq!(
            (parsed.parent, parsed.group, parsed.session, parsed.start),
            (1, 10, 10, 998877)
        );
        assert!(parse_process(10, "10 (truncated) S 1").is_none());
    }

    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn isolated_child(script: &str) -> ChildGuard {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", script]);
        // SAFETY: the child performs only the async-signal-safe setsid syscall
        // between fork and exec. Every signal in this test targets its own child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        ChildGuard(command.spawn().unwrap())
    }

    fn wait_child_state(child: &ChildGuard, option: libc::c_int) {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: info is writable and this PID names the test's unreaped child.
        assert_eq!(
            unsafe { libc::waitid(libc::P_PID, child.0.id(), &mut info, option) },
            0
        );
        assert_eq!(unsafe { info.si_pid() }, child.0.id() as libc::pid_t);
    }

    #[test]
    fn launch_identity_is_not_the_transient_pre_exec_image() {
        let mut command = portable_pty::CommandBuilder::new("/bin/sleep");
        command.cwd("/");
        let expected = RootIdentity::launch_executable(&command).unwrap();
        let child = isolated_child("kill -STOP $$; exec /bin/sleep 60");
        wait_child_state(&child, libc::WSTOPPED);
        let root = RootIdentity::capture(child.0.id() as libc::pid_t, Some(expected)).unwrap();
        assert!(
            !root.original_executable(),
            "the intermediate shell is not the launch image"
        );
        signal(&root.pidfd, libc::SIGCONT).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !root.original_executable() {
            assert!(
                Instant::now() < deadline,
                "launch image never became visible"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn dropping_census_releases_its_stops_but_preserves_existing_stopped_jobs() {
        let running = isolated_child("exec sleep 60");
        let root = RootIdentity::capture(running.0.id() as libc::pid_t, None).unwrap();
        let family = FrozenFamily::capture(&root, Instant::now() + Duration::from_secs(2)).unwrap();
        assert_eq!(read_process(root.process.pid).unwrap().state, b'T');
        drop(family);
        wait_child_state(&running, libc::WCONTINUED | libc::WNOHANG);

        let stopped = isolated_child("kill -STOP $$; exec sleep 60");
        wait_child_state(&stopped, libc::WSTOPPED);
        let root = RootIdentity::capture(stopped.0.id() as libc::pid_t, None).unwrap();
        let family = FrozenFamily::capture(&root, Instant::now() + Duration::from_secs(2)).unwrap();
        drop(family);
        assert_eq!(read_process(root.process.pid).unwrap().state, b'T');
    }

    #[test]
    fn expired_census_does_not_leave_its_child_stopped() {
        let child = isolated_child("exec sleep 60");
        let root = RootIdentity::capture(child.0.id() as libc::pid_t, None).unwrap();
        assert!(FrozenFamily::capture(&root, Instant::now()).is_err());
        assert!(!quiescent(read_process(root.process.pid).unwrap().state));
    }

    #[test]
    fn kill_receipt_requires_a_live_root_and_a_successful_signal() {
        let child = isolated_child("kill -STOP $$; exec sleep 60");
        wait_child_state(&child, libc::WSTOPPED);
        let root = RootIdentity::capture(child.0.id() as libc::pid_t, None).unwrap();
        let mut family =
            FrozenFamily::capture(&root, Instant::now() + Duration::from_secs(2)).unwrap();
        assert!(family.kill(Instant::now()).is_err());
        assert!(!root.intentionally_killed());
        family
            .kill(Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert!(root.intentionally_killed());

        let exited = isolated_child("kill -STOP $$; exec sleep 60");
        wait_child_state(&exited, libc::WSTOPPED);
        let root = RootIdentity::capture(exited.0.id() as libc::pid_t, None).unwrap();
        // An unrelated killer wins before close inspection. Leave the child
        // unreaped, so pidfd SIGKILL may succeed without causing its exit.
        signal(&root.pidfd, libc::SIGKILL).unwrap();
        wait_child_state(&exited, libc::WEXITED | libc::WNOWAIT);
        let mut family =
            FrozenFamily::capture(&root, Instant::now() + Duration::from_secs(2)).unwrap();
        family
            .kill(Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert!(!root.intentionally_killed());
    }
}
