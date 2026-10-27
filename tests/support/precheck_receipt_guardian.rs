//! Custody for this integration target's finite, private precheck families.
//! Only a selected same-ELF child is a subreaper. The shared harness never
//! changes signal policy or reaps children belonging to another test.

use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const STAGE: &str = "CAGENTS_PRECHECK_CUSTODY_STAGE";
const ROOT: &str = "CAGENTS_PRECHECK_CUSTODY_ROOT";
const TEST: &str = "CAGENTS_PRECHECK_CUSTODY_TEST";
const POLL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub pid: i32,
    pub start: u64,
}

#[derive(Clone, Copy, Debug)]
struct ReceiptObservedFailure {
    owner: Identity,
    descendant: Identity,
}

pub fn fail_after_receipt_observation(owner: Identity, descendant: Identity) -> ! {
    std::panic::panic_any(ReceiptObservedFailure { owner, descendant })
}

impl Identity {
    pub fn read(pid: i32) -> io::Result<Self> {
        let raw = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let fields: Vec<_> = raw
            .rsplit_once(')')
            .ok_or_else(invalid)?
            .1
            .split_whitespace()
            .collect();
        Ok(Self {
            pid,
            start: fields
                .get(19)
                .ok_or_else(invalid)?
                .parse()
                .map_err(|_| invalid())?,
        })
    }

    pub fn from_file(path: &Path) -> io::Result<Self> {
        let raw = fs::read_to_string(path)?;
        let mut fields = raw.split_whitespace();
        let pid = fields
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        let start = fields
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        if fields.next().is_some() || pid <= 0 {
            return Err(invalid());
        }
        Ok(Self { pid, start })
    }

    /// A zombie is still present. pidfd readiness alone is not reaping proof.
    pub fn present(self) -> io::Result<bool> {
        match Self::read(self.pid) {
            Ok(current) => Ok(current == self),
            Err(error) if gone(&error) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn invalid() -> io::Error {
    io::Error::other("Invalid private fixture process identity")
}

fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

struct Pinned {
    identity: Identity,
    fd: OwnedFd,
}

impl Pinned {
    fn open(pid: i32) -> io::Result<Self> {
        let identity = Identity::read(pid)?;
        // SAFETY: pidfd_open returns a fresh descriptor; the identity is checked
        // again before it can become signalling authority.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        if Identity::read(pid)? != identity {
            return Err(invalid());
        }
        Ok(Self { identity, fd })
    }

    fn signal(&self, signal: i32) -> io::Result<()> {
        // SAFETY: this owned pidfd names the original process, never a reused
        // marker PID. A null siginfo requests the ordinary signal operation.
        if unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.fd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        } == 0
        {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn freeze(&self, deadline: Instant) -> io::Result<()> {
        self.signal(libc::SIGSTOP)?;
        loop {
            if !self.identity.present()? {
                return Ok(());
            }
            // Same-ELF actors have a libtest main and test thread. Leader T
            // alone is not group-stop completion: a sibling could still fork.
            let before = stopped_tasks(self.identity.pid)?;
            let after = stopped_tasks(self.identity.pid)?;
            if before.is_some() && before == after && self.identity.present()? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "Private fixture did not stop for custody census",
                ));
            }
            std::thread::sleep(POLL);
        }
    }
}

fn stopped_tasks(pid: i32) -> io::Result<Option<BTreeSet<i32>>> {
    let tasks = match fs::read_dir(format!("/proc/{pid}/task")) {
        Ok(tasks) => tasks,
        Err(error) if gone(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut stopped = BTreeSet::new();
    for task in tasks {
        let path = task?.path();
        let raw = match fs::read_to_string(path.join("stat")) {
            Ok(raw) => raw,
            Err(error) if gone(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let state = raw
            .rsplit_once(')')
            .ok_or_else(invalid)?
            .1
            .split_whitespace()
            .next();
        if !matches!(state, Some("T" | "t" | "Z" | "X")) {
            return Ok(None);
        }
        let tid = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(invalid)?;
        stopped.insert(tid.parse().map_err(|_| invalid())?);
    }
    Ok((!stopped.is_empty()).then_some(stopped))
}

pub struct Custody {
    root: PathBuf,
    owners: RefCell<Vec<Pinned>>,
    cancels: RefCell<Vec<PathBuf>>,
}

impl Custody {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn register(&self, child: &Child, directory: &Path) -> io::Result<()> {
        self.cancels.borrow_mut().push(directory.to_owned());
        // Child has not been waited, so its number cannot have been recycled.
        let pinned = Pinned::open(child.id() as i32)?;
        self.owners.borrow_mut().push(pinned);
        Ok(())
    }

    fn settle(&self) -> io::Result<bool> {
        // Body/Worker unwinding has finished. There are no remaining Child
        // waiters in this process; only this guardian may reap its family.
        for directory in self.cancels.borrow().iter() {
            fs::write(directory.join("cancel"), [])?;
        }
        let cancellation_grace = Instant::now() + Duration::from_secs(2);
        while !reap()? {
            if Instant::now() >= cancellation_grace {
                break;
            }
            std::thread::sleep(POLL);
        }
        if reap()? {
            return Ok(false);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if reap()? {
                return Ok(true);
            }
            // Freeze launchers before inspecting their children. Every node
            // comes from the kernel genealogy of this guardian, not a PID file.
            // The finite scripts/actors stop creating children at readiness.
            let mut family = Vec::new();
            let mut seen = BTreeSet::new();
            freeze_children(std::process::id() as i32, deadline, &mut seen, &mut family)?;
            for process in family.iter().rev() {
                if !process.identity.present()? {
                    continue;
                }
                if children(process.identity.pid)?.is_empty() {
                    process.signal(libc::SIGKILL)?;
                } else {
                    // Let an owner reap its stopped/killed children. Never
                    // kill their owner while those child identities remain.
                    process.signal(libc::SIGCONT)?;
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "Private precheck family settlement is unconfirmed",
                ));
            }
            std::thread::sleep(POLL);
        }
    }
}

fn children(pid: i32) -> io::Result<BTreeSet<i32>> {
    let tasks = match fs::read_dir(format!("/proc/{pid}/task")) {
        Ok(tasks) => tasks,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error),
    };
    let mut result = BTreeSet::new();
    for task in tasks {
        let raw = match fs::read_to_string(task?.path().join("children")) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for value in raw.split_whitespace() {
            result.insert(value.parse().map_err(|_| invalid())?);
        }
    }
    Ok(result)
}

fn freeze_children(
    parent: i32,
    deadline: Instant,
    seen: &mut BTreeSet<i32>,
    family: &mut Vec<Pinned>,
) -> io::Result<()> {
    for pid in children(parent)? {
        if !seen.insert(pid) {
            continue;
        }
        if seen.len() > 32 {
            return Err(io::Error::other(
                "Private precheck family exceeded its fixed fixture bound",
            ));
        }
        let process = match Pinned::open(pid) {
            Ok(process) => process,
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => {
                continue;
            }
            Err(error) => return Err(error),
        };
        process.freeze(deadline)?;
        family.push(process);
        freeze_children(pid, deadline, seen, family)?;
    }
    Ok(())
}

fn reap() -> io::Result<bool> {
    loop {
        // SAFETY: only the dedicated selected guardian reaches this function,
        // after all test-body Child waiters have been dropped.
        match unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } {
            result if result > 0 => continue,
            0 => return Ok(false),
            _ => match io::Error::last_os_error().raw_os_error() {
                Some(libc::ECHILD) => return Ok(true),
                Some(libc::EINTR) => continue,
                _ => return Err(io::Error::last_os_error()),
            },
        }
    }
}

pub fn actor_command(kind: &str, root: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &std::env::var(TEST).unwrap(), "--test-threads=1"])
        .env(STAGE, kind)
        .env(ROOT, root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

pub fn diagnostic(path: &Path) -> String {
    let mut text = String::new();
    // This file is created by the fixture, not a pipe held by descendants.
    if let Ok(file) = File::open(path) {
        let _ = file.take(64 * 1024).read_to_string(&mut text);
    }
    text
}

pub fn isolated(name: &str, actor: fn(&str, &Path), body: impl FnOnce(&Custody)) {
    isolated_with_rescue_control(name, actor, false, body);
}

pub fn isolated_rescue_control(name: &str, actor: fn(&str, &Path), body: impl FnOnce(&Custody)) {
    isolated_with_rescue_control(name, actor, true, body);
}

fn isolated_with_rescue_control(
    name: &str,
    actor: fn(&str, &Path),
    expect_rescue: bool,
    body: impl FnOnce(&Custody),
) {
    match std::env::var(STAGE).as_deref() {
        Ok("guardian") => {
            let mut subreaper = 0_i32;
            assert_eq!(
                unsafe {
                    libc::prctl(
                        libc::PR_GET_CHILD_SUBREAPER,
                        &mut subreaper as *mut i32,
                        0,
                        0,
                        0,
                    )
                },
                0
            );
            assert_eq!(
                subreaper, 1,
                "Only the separately admitted guardian may reap this family"
            );
            assert_eq!(std::env::var(TEST).unwrap(), name);
            let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
            let custody = Custody {
                root: root.clone(),
                owners: RefCell::new(Vec::new()),
                cancels: RefCell::new(Vec::new()),
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&custody)));
            let observed_failure = result
                .as_ref()
                .err()
                .and_then(|payload| payload.downcast_ref::<ReceiptObservedFailure>())
                .copied();
            let forced = match custody.settle() {
                Ok(forced) => forced,
                Err(error) => {
                    // Keep the guardian, its exact pidfds and its evidence alive.
                    // The bounded outer test fails/quarantines this uncertainty;
                    // exiting here would abandon an unproved detached family.
                    fs::write(root.join("QUARANTINE"), error.to_string()).ok();
                    loop {
                        std::thread::park_timeout(Duration::from_secs(60));
                    }
                }
            };
            fs::write(root.join("SETTLED"), "ECHILD\n").unwrap();
            if expect_rescue {
                let failure = observed_failure
                    .expect("Rescue control must fail with the exact receipt-observation payload");
                assert!(forced, "Cooperative release is not forced-rescue coverage");
                assert!(!failure.owner.present().unwrap());
                assert!(!failure.descendant.present().unwrap());
                assert_eq!(
                    fs::read_to_string(root.join("rescue/leaf-exit-observed")).unwrap(),
                    format!(
                        "{} {} SIGKILL\n",
                        failure.descendant.pid, failure.descendant.start
                    )
                );
                fs::write(
                    root.join("RESCUED"),
                    format!(
                        "receipt-before-reap {} {} {} {}\n",
                        failure.owner.pid,
                        failure.owner.start,
                        failure.descendant.pid,
                        failure.descendant.start
                    ),
                )
                .unwrap();
                return;
            }
            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
        }
        Ok(stage) => actor(stage, &PathBuf::from(std::env::var_os(ROOT).unwrap())),
        Err(_) => {
            let mut root = tempfile::Builder::new()
                .prefix("precheck-custody-")
                .tempdir()
                .unwrap();
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let stdout = File::create(root.path().join("guardian.stdout")).unwrap();
            let stderr = File::create(root.path().join("guardian.stderr")).unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", name, "--nocapture", "--test-threads=1"])
                .env(STAGE, "guardian")
                .env(ROOT, root.path())
                .env(TEST, name)
                .stdin(Stdio::null())
                .stdout(stdout)
                .stderr(stderr);
            unsafe {
                command.pre_exec(|| {
                    if libc::signal(libc::SIGCHLD, libc::SIG_DFL) == libc::SIG_ERR
                        || libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().unwrap();
            // Admission transfers custody before any later fallible operation.
            // Only positive exit + ECHILD proof permits namespace removal.
            root.disable_cleanup(true);
            let deadline = Instant::now() + Duration::from_secs(45);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break Some(status);
                }
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(POLL);
            };
            let settled = fs::read_to_string(root.path().join("SETTLED"))
                .ok()
                .as_deref()
                == Some("ECHILD\n");
            if !status.is_some_and(|status| status.success()) || !settled {
                panic!(
                    "Private precheck guardian failed: {status:?}; settled={settled}; retained={}\n{}\n{}",
                    root.path().display(),
                    diagnostic(&root.path().join("guardian.stdout")),
                    diagnostic(&root.path().join("guardian.stderr"))
                );
            }
            if expect_rescue {
                let receipt = fs::read_to_string(root.path().join("RESCUED")).unwrap();
                let fields: Vec<_> = receipt.split_whitespace().collect();
                assert_eq!(fields.len(), 5);
                assert_eq!(fields[0], "receipt-before-reap");
                for pair in fields[1..].chunks_exact(2) {
                    let original = Identity {
                        pid: pair[0].parse().unwrap(),
                        start: pair[1].parse().unwrap(),
                    };
                    assert!(
                        !original.present().unwrap(),
                        "Rescue left original fixture work present"
                    );
                }
            }
            root.disable_cleanup(false);
        }
    }
}
