#![cfg(feature = "host")]

use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Worker {
    _root: tempfile::TempDir,
    directory: PathBuf,
    child: Child,
}

impl Worker {
    fn start(command: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let firing = uuid::Uuid::new_v4();
        let directory = root.path().join(firing.to_string());
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            directory.join("request"),
            serde_json::to_vec(&serde_json::json!({
                "firing": firing, "revision": 1, "directory": directory,
                "precheck": { "command": command, "timeout_seconds": 1 }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(directory.join("request"), fs::Permissions::from_mode(0o600)).unwrap();
        let lease = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(directory.join("lease"))
            .unwrap();
        let fd = lease.as_raw_fd();
        assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);
        let mut command = Command::new(env!("CARGO_BIN_EXE_cagentsd"));
        command
            .arg("--automation-precheck-supervisor")
            .arg(&directory)
            .arg(fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        // Simulates the daemon disappearing after launch: only the helper now
        // owns the inherited lease and deadline, not this controller.
        drop(lease);
        Self {
            _root: root,
            directory,
            child,
        }
    }

    fn wait_file(&mut self, name: &str) -> PathBuf {
        let path = self.directory.join(name);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            if let Some(status) = self.child.try_wait().unwrap() {
                let mut stderr = String::new();
                use std::io::Read;
                self.child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                panic!("worker exited before {name}: {status}: {stderr}");
            }
            assert!(Instant::now() < deadline, "worker did not publish {name}");
            std::thread::sleep(Duration::from_millis(10));
        }
        path
    }

    fn receipt(&mut self) -> serde_json::Value {
        let path = self.wait_file("receipt");
        let receipt = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(self.child.wait().unwrap().success());
        receipt
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn assert_descendant_gone(path: &Path) {
    let pid = fs::read_to_string(path).unwrap();
    assert!(
        !Path::new(&format!("/proc/{}", pid.trim())).exists(),
        "receipt preceded descendant reaping"
    );
}

#[test]
fn successful_shell_still_reaps_detached_descendants_before_receipt() {
    let mut worker = Worker::start(
        "setsid /bin/sh -c 'echo $$ > escaped.pid; exec sleep 5' >/dev/null 2>&1 & while ! test -s escaped.pid; do sleep 0.01; done; exit 0",
    );
    let receipt = worker.receipt();
    assert_eq!(receipt["outcome"], "Passed");
    assert_descendant_gone(&worker.directory.join("escaped.pid"));
}

#[test]
fn worker_deadline_survives_controller_disconnect_and_reaps_detached_process() {
    let mut worker = Worker::start(
        "setsid /bin/sh -c 'echo $$ > escaped.pid; exec sleep 5' >/dev/null 2>&1 & while ! test -s escaped.pid; do sleep 0.01; done; sleep 5",
    );
    let receipt = worker.receipt();
    assert_eq!(receipt["outcome"]["Stopped"]["state"], "failed");
    assert_descendant_gone(&worker.directory.join("escaped.pid"));
}

#[test]
fn cancellation_receipt_follows_descendant_cleanup() {
    let mut worker = Worker::start(
        "setsid /bin/sh -c 'echo $$ > escaped.pid; exec sleep 5' >/dev/null 2>&1 & while ! test -s escaped.pid; do sleep 0.01; done; sleep 5",
    );
    worker.wait_file("escaped.pid");
    File::create(worker.directory.join("cancel")).unwrap();
    let receipt = worker.receipt();
    assert_eq!(receipt["outcome"]["Stopped"]["state"], "cancelled");
    assert_descendant_gone(&worker.directory.join("escaped.pid"));
}
