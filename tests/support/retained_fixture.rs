//! Runs a retained-actor fixture beneath a private subreaper. The retained
//! helper calls setsid and moves into its own systemd scope, so process-group
//! and cgroup cleanup miss it; ancestry does not. Never make the parallel test
//! harness itself a subreaper.
use std::{
    collections::BTreeSet,
    io,
    os::unix::process::CommandExt,
    process::{Child, Command, ExitStatus},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// Same-test reexecs that clear their environment must preserve this, so they
/// run the body instead of nesting another supervisor.
pub const OWNER_ENV: &str = "CAGENTS_TEST_RETAINED_FIXTURE_OWNER";
const PARENT_ENV: &str = "CAGENTS_TEST_RETAINED_FIXTURE_PARENT";
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn interrupted(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

pub(crate) fn shutdown_on_signal() -> io::Result<()> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = interrupted as *const () as usize;
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// `OWNER_ENV` and its value, for `Command::envs` after `env_clear()`.
pub fn owner_env() -> Option<(&'static str, std::ffi::OsString)> {
    std::env::var_os(OWNER_ENV).map(|value| (OWNER_ENV, value))
}

/// Call first in the test. Returns true in the harness and the supervisor;
/// only the worker returns false and continues into the test body.
pub fn run_isolated() -> bool {
    run_isolated_with_timeout(Duration::from_secs(900))
}

/// Give a measured multi-hop fixture its complete bounded execution budget,
/// while retaining the same private subreaper and mandatory descendant cleanup.
pub fn run_isolated_with_timeout(timeout: Duration) -> bool {
    let thread = std::thread::current();
    let test = thread.name().expect("named libtest fixture");
    let worker = format!("worker:{test}");
    let supervisor = format!("supervisor:{test}");
    match std::env::var(OWNER_ENV).as_deref() {
        Ok(mode) if mode == worker => false,
        Ok(mode) if mode == supervisor => {
            // Only this separately executed test adopts orphaned descendants.
            assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
            shutdown_on_signal().expect("install fixture shutdown handlers");
            assert_eq!(
                unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) },
                0
            );
            let parent: i32 = std::env::var(PARENT_ENV).unwrap().parse().unwrap();
            if unsafe { libc::getppid() } != parent {
                INTERRUPTED.store(true, Ordering::Relaxed);
            }
            let mut command = test_command(test);
            command.env(OWNER_ENV, worker);
            let (status, killed) = Family::spawn(command)
                .and_then(|mut family| family.finish(timeout))
                .expect("fixture family must exit and be completely reaped");
            assert!(status.success(), "isolated fixture failed: {status}");
            if killed > 0 {
                eprintln!("retained fixture {test}: killed {killed} surviving descendant(s)");
            }
            true
        }
        _ => {
            // libtest succeeds when --exact matches nothing; refuse that.
            let listed = test_command(test)
                .arg("--list")
                .output()
                .expect("list retained fixture");
            let entry = format!("{test}: test");
            assert_eq!(
                String::from_utf8_lossy(&listed.stdout)
                    .lines()
                    .filter(|line| *line == entry)
                    .count(),
                1,
                "--exact {test} must select exactly this test"
            );
            let status = test_command(test)
                .env(OWNER_ENV, supervisor)
                .env(PARENT_ENV, std::process::id().to_string())
                .process_group(0)
                .status()
                .expect("start retained fixture supervisor");
            assert!(status.success(), "fixture supervisor failed: {status}");
            true
        }
    }
}

fn test_command(test: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        test,
        "--include-ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    command
}

pub(crate) struct Family {
    pub(crate) child: Child,
    settled: bool,
}

impl Family {
    pub(crate) fn spawn(mut command: Command) -> io::Result<Self> {
        command.process_group(0);
        Ok(Self {
            child: command.spawn()?,
            settled: false,
        })
    }

    /// Waits for the worker, then kills and reaps every remaining descendant.
    /// Returns the worker status and how many live descendants were killed.
    pub(crate) fn finish(&mut self, timeout: Duration) -> io::Result<(ExitStatus, usize)> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok((status, self.cleanup()?));
            }
            // Fixtures may wait for an adopted process to disappear.
            self.reap_exited_descendants()?;
            if INTERRUPTED.load(Ordering::Relaxed) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "fixture cancelled",
                ));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "fixture timed out"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn reap_exited_descendants(&self) -> io::Result<()> {
        for pid in direct_children()? {
            if pid != self.child.id() as i32 {
                reap(pid)?;
            }
        }
        Ok(())
    }

    fn cleanup(&mut self) -> io::Result<usize> {
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        let mut killed = BTreeSet::new();
        loop {
            if reap(-1)? {
                self.settled = true;
                return Ok(killed.len());
            }
            // This supervisor is the only waiter, so an unreaped child keeps
            // its PID reserved until the waitpid above.
            for pid in direct_children()? {
                if unsafe { libc::kill(pid, libc::SIGKILL) } == -1
                    && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                {
                    return Err(io::Error::last_os_error());
                }
                killed.insert(pid);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fixture descendants did not settle",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Family {
    fn drop(&mut self) {
        if !self.settled
            && let Err(error) = self.cleanup()
        {
            eprintln!("retained fixture family cleanup failed: {error}");
        }
    }
}

/// Reaps every exited match for `pid` without blocking. Returns true once no
/// such child remains.
fn reap(pid: i32) -> io::Result<bool> {
    loop {
        match unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } {
            0 => return Ok(false),
            reaped if reaped > 0 && pid == -1 => continue,
            reaped if reaped > 0 => return Ok(true),
            _ => match io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::ECHILD) => return Ok(true),
                _ => return Err(io::Error::last_os_error()),
            },
        }
    }
}

fn direct_children() -> io::Result<BTreeSet<i32>> {
    let mut children = BTreeSet::new();
    for task in std::fs::read_dir("/proc/self/task")? {
        let raw = match std::fs::read_to_string(task?.path().join("children")) {
            Ok(raw) => raw,
            Err(error)
                if matches!(error.raw_os_error(), Some(libc::ENOENT) | Some(libc::ESRCH)) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        children.extend(
            raw.split_whitespace()
                .filter_map(|id| id.parse::<i32>().ok()),
        );
    }
    Ok(children)
}
