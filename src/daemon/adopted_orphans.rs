//! Child ownership inside a long-lived, multi-threaded subreaper.
//!
//! The retained actor adopts every orphaned tool descendant of its provider.
//! Those exited orphans must be reaped while the actor lives, but the actor
//! also hosts code that waits on specific children (the provider's
//! `ChildGuard`, scope enrollment). Such owners spawn through [`spawn_owned`],
//! which registers the pid before releasing its share of the spawn gate. The
//! sweep holds the gate exclusively, so it never observes an owned child
//! without its registration and never consumes a status that belongs to an
//! owner. Spawns share the gate, so processes that never sweep (the daemon)
//! do not serialize their spawns on it.
use std::{
    collections::BTreeMap,
    io,
    process::{Child, Command},
    sync::{
        Mutex, MutexGuard, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

static GATE: RwLock<()> = RwLock::new(());
/// Live registrations per pid. A stale registration whose child was already
/// reaped can coexist with a new owner of the recycled pid.
static OWNED: Mutex<BTreeMap<u32, usize>> = Mutex::new(BTreeMap::new());
static SWEEPING: AtomicBool = AtomicBool::new(false);

fn owned() -> MutexGuard<'static, BTreeMap<u32, usize>> {
    OWNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Reserves a child's wait status for its owner. Drop it only after that
/// owner's final wait, or once the owner abandons the child unreaped.
#[must_use = "dropping the registration lets the orphan sweep reap this child"]
pub(crate) struct Registration(u32);

impl Drop for Registration {
    fn drop(&mut self) {
        let mut owned = owned();
        if let Some(count) = owned.get_mut(&self.0) {
            *count -= 1;
            if *count == 0 {
                owned.remove(&self.0);
            }
        }
    }
}

/// Spawns a child whose status belongs to the caller. Fork, exec and
/// registration complete while the caller shares the gate the sweep takes
/// exclusively, so even a child that exits immediately is never reaped by
/// [`reap_exited`].
pub(crate) fn spawn_owned(command: &mut Command) -> io::Result<(Child, Registration)> {
    let _gate = GATE.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    let child = command.spawn()?;
    let pid = child.id();
    #[cfg(test)]
    tests::after_spawn(pid);
    *owned().entry(pid).or_default() += 1;
    Ok((child, Registration(pid)))
}

/// Whether this process reaps unregistered children. A process that does must
/// spawn every child it waits for through [`spawn_owned`].
pub(crate) fn sweeping() -> bool {
    SWEEPING.load(Ordering::Acquire)
}

/// Declares, before any child is spawned, that this process will sweep.
pub(crate) fn become_sweeper() {
    SWEEPING.store(true, Ordering::Release);
}

/// Outcome of one bounded sweep.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sweep {
    pub(crate) reaped: usize,
    /// More exited children may remain because the pass reached its limit.
    pub(crate) truncated: bool,
}

/// Reaps at most `limit` children that have already exited and are not
/// registered by an in-process owner. Live children are never signalled or
/// otherwise touched. Only a subreaper whose every waited child is spawned
/// through [`spawn_owned`] may call this.
pub(crate) fn reap_exited(limit: usize) -> io::Result<Sweep> {
    become_sweeper();
    let children = super::owned_children::direct_children()?;
    let _gate = GATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let owned = owned();
    let mut sweep = Sweep::default();
    for pid in children {
        if owned.contains_key(&(pid as u32)) {
            continue;
        }
        if sweep.reaped == limit {
            sweep.truncated = true;
            break;
        }
        let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: the siginfo buffer is live and writable. An owner may
            // have reaped a pid from the census before the gate was taken; if
            // it was recycled, the new process is either registered (its spawn
            // finished under the gate) or another adopted orphan, and any other
            // pid yields ECHILD. WNOHANG leaves a live child untouched.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut information,
                    libc::WEXITED | libc::WNOHANG,
                )
            };
            if result == 0 {
                // SAFETY: successful waitid initialized the child fields.
                if unsafe { information.si_pid() } != 0 {
                    sweep.reaped += 1;
                }
                break;
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                // The census is a snapshot: an owner may already have reaped it.
                Some(libc::ECHILD) => break,
                _ => return Err(error),
            }
        }
    }
    Ok(sweep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::process::ExitStatusExt,
        time::{Duration, Instant},
    };

    fn child_state(pid: i32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat[stat.rfind(')')? + 2..].chars().next()
    }

    fn zombie_children() -> Vec<i32> {
        super::super::owned_children::direct_children()
            .unwrap()
            .into_iter()
            .filter(|pid| child_state(*pid) == Some('Z'))
            .collect()
    }

    /// Every case runs in a re-executed, single-test process that is its own
    /// subreaper, so neither the harness nor other tests share its children.
    fn isolated(stage: &str, body: impl FnOnce()) {
        if std::env::var_os(stage).is_none() {
            let test = std::thread::current().name().unwrap().to_owned();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test, "--nocapture", "--test-threads=1"])
                .env(stage, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        body();
    }

    #[test]
    fn sweep_reaps_burst_orphans_and_preserves_owned_and_live_children() {
        isolated("CAGENTS_ADOPTED_ORPHANS_BURST_TEST", || {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let live = root.path().join("live");
            // The owned provider orphans 400 grandchildren in four bursts that
            // exit together after their parents, so their SIGCHLDs coalesce.
            // It also orphans one daemon that, like the provider, lives until
            // this test closes their shared stdin, even if the test panics.
            let (mut provider, registration) = spawn_owned(
                Command::new("/bin/sh")
                    .arg("-c")
                    .arg(
                        r#"exec 3<&0
for burst in 1 2 3 4; do
  i=0
  while [ $i -lt 100 ]; do (/bin/sleep 0.2 &); i=$((i + 1)); done
done
(/bin/cat <&3 >/dev/null & echo $! > "$1")
while IFS= read -r line; do :; done
exit 7"#,
                    )
                    .args(["provider", live.to_str().unwrap()])
                    .stdin(std::process::Stdio::piped()),
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            let live: i32 = loop {
                if let Some(pid) = std::fs::read_to_string(&live)
                    .ok()
                    .and_then(|pid| pid.trim().parse().ok())
                {
                    break pid;
                }
                assert!(Instant::now() < deadline, "fixture did not daemonize");
                std::thread::sleep(Duration::from_millis(10));
            };
            let mut reaped = 0;
            while reaped < 400 {
                let sweep = reap_exited(64).unwrap();
                assert!(sweep.reaped <= 64);
                reaped += sweep.reaped;
                assert!(
                    Instant::now() < deadline,
                    "only {reaped} of 400 adopted orphans were reaped"
                );
            }
            assert!(zombie_children().is_empty());
            // The provider and the adopted daemon are live, unsignalled and
            // still this process's children.
            assert_eq!(child_state(provider.id() as i32), Some('S'));
            assert_eq!(child_state(live), Some('S'));
            assert!(
                super::super::owned_children::direct_children()
                    .unwrap()
                    .contains(&live)
            );
            provider.stdin.take();
            while child_state(provider.id() as i32) != Some('Z') {
                assert!(Instant::now() < deadline, "provider did not exit");
                std::thread::sleep(Duration::from_millis(5));
            }
            // An exited owned child stays reserved for its owner, while the
            // daemon that exited with it is reaped as an adopted orphan.
            let mut reaped = 0;
            while child_state(live).is_some() {
                reaped += reap_exited(usize::MAX).unwrap().reaped;
                assert!(Instant::now() < deadline, "exited daemon was not reaped");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(reaped, 1);
            assert_eq!(child_state(provider.id() as i32), Some('Z'));
            assert_eq!(provider.wait().unwrap().code(), Some(7));
            drop(registration);
        });
    }

    thread_local! {
        static AFTER_SPAWN: std::cell::RefCell<Option<Box<dyn Fn(u32)>>> =
            const { std::cell::RefCell::new(None) };
    }
    pub(super) fn after_spawn(pid: u32) {
        AFTER_SPAWN.with(|hook| {
            if let Some(hook) = hook.borrow().as_ref() {
                hook(pid);
            }
        });
    }

    #[test]
    fn a_sweep_cannot_reap_an_owned_child_that_exits_before_registration() {
        isolated("CAGENTS_ADOPTED_ORPHANS_RACE_TEST", || {
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let sweeper = {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        reap_exited(usize::MAX).unwrap();
                    }
                })
            };
            // Hold each child exited, between spawn and registration, while
            // the sweeper keeps running. Only the shared gate keeps it owned.
            AFTER_SPAWN.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(|pid| {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while child_state(pid as i32) != Some('Z') {
                        assert!(
                            Instant::now() < deadline,
                            "owned child {pid} was reaped or did not exit: {:?}",
                            child_state(pid as i32)
                        );
                        std::thread::yield_now();
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }));
            });
            for _ in 0..50 {
                let (mut child, registration) =
                    spawn_owned(Command::new("/bin/sh").args(["-c", "exit 3"])).unwrap();
                let status = child.wait().unwrap();
                assert_eq!(status.code(), Some(3), "{:?}", status.signal());
                drop(registration);
            }
            AFTER_SPAWN.with(|hook| hook.borrow_mut().take());
            stop.store(true, Ordering::Relaxed);
            sweeper.join().unwrap();
        });
    }

    #[test]
    fn a_stale_registration_never_releases_a_recycled_pid() {
        let (mut first, stale) = spawn_owned(&mut Command::new("/bin/true")).unwrap();
        first.wait().unwrap();
        // Model the pid's reuse by a new owner before the stale drop.
        let pid = first.id();
        *owned().entry(pid).or_default() += 1;
        let current = Registration(pid);
        drop(stale);
        assert_eq!(owned().get(&pid), Some(&1));
        drop(current);
        assert_eq!(owned().get(&pid), None);
    }
}
