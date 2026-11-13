//! Compiled only into the library harness, so it runs once.
use super::retained_fixture::*;
use std::{
    io,
    path::Path,
    process::Command,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

/// Both generations escape the launcher's group. Killing the first reparents
/// the second to our private subreaper.
fn detached_family(root: &Path, outcome: &str) -> Family {
    let provider = r#"
import os, pathlib, sys, time
root = pathlib.Path(sys.argv[1])
if os.fork() == 0:
    os.setsid()
    (root / 'grandchild').write_text(str(os.getpid()))
else:
    (root / 'provider').write_text(str(os.getpid()))
time.sleep(600)
"#;
    let launcher = r#"
import pathlib, subprocess, sys, time
root = pathlib.Path(sys.argv[1])
subprocess.Popen(['/usr/bin/python3', '-c', sys.argv[2], str(root)],
                 start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
while not (root / 'grandchild').exists() or not (root / 'provider').exists():
    time.sleep(0.01)
if sys.argv[3] in ('timeout', 'signal'):
    time.sleep(600)
sys.exit(7 if sys.argv[3] == 'failure' else 0)
"#;
    let mut command = Command::new("/usr/bin/python3");
    command.args(["-c", launcher, root.to_str().unwrap(), provider, outcome]);
    Family::spawn(command).unwrap()
}

fn wait_for_family(root: &Path) -> Vec<i32> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let processes = ["provider", "grandchild"]
            .map(|name| std::fs::read_to_string(root.join(name)).ok()?.parse().ok());
        if let [Some(provider), Some(grandchild)] = processes {
            return vec![provider, grandchild];
        }
        assert!(Instant::now() < deadline, "detached fixture never started");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_reaped(processes: &[i32], outcome: &str) {
    for pid in processes {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "{outcome} left detached process {pid} alive or unreaped"
        );
    }
}

#[test]
fn success_failure_panic_and_timeout_reap_detached_descendants() {
    if run_isolated() {
        return;
    }
    // A nested subreaper beneath the outer supervisor, so a regression in
    // Family itself cannot leak this test's processes.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    shutdown_on_signal().unwrap();
    for outcome in ["success", "failure", "panic", "timeout", "signal"] {
        let root = tempfile::tempdir().unwrap();
        let mut family = detached_family(root.path(), outcome);
        let processes = wait_for_family(root.path());
        match outcome {
            "panic" => {
                assert!(
                    std::panic::catch_unwind(move || {
                        let _family = family;
                        panic!("intentional fixture panic");
                    })
                    .is_err()
                );
            }
            "timeout" | "signal" => {
                let kind = if outcome == "signal" {
                    unsafe { libc::raise(libc::SIGTERM) };
                    io::ErrorKind::Interrupted
                } else {
                    io::ErrorKind::TimedOut
                };
                assert_eq!(
                    family.finish(Duration::from_millis(30)).unwrap_err().kind(),
                    kind
                );
                drop(family);
                INTERRUPTED.store(false, Ordering::Relaxed);
            }
            _ => {
                let (status, killed) = family.finish(Duration::from_secs(10)).unwrap();
                assert_eq!(
                    status.code(),
                    Some(if outcome == "failure" { 7 } else { 0 })
                );
                assert_eq!(killed, 2, "{outcome} must report both survivors");
                // Still holding Family: finish() itself completed cleanup.
                assert_reaped(&processes, outcome);
            }
        }
        assert_reaped(&processes, outcome);
    }
}
