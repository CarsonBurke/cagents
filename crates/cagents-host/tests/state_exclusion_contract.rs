#![cfg(target_os = "linux")]
//! One instance's saved data has one daemon, whatever runtime directory each
//! daemon is started with.
//!
//! Every daemon lock lives in the runtime directory that `XDG_RUNTIME_DIR`
//! names, and a nested compositor session gives itself a private one while
//! keeping the person's data directory. A window started there used to start
//! a second daemon over the same data, which restored the same chats, stamped
//! its own proof on their panes and replaced the serving daemon's Ready
//! evidence. These cases start the real daemon twice over one data directory.

use std::{
    collections::BTreeMap,
    fs,
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const STARTUP: Duration = Duration::from_secs(30);

/// Private roots shared by both daemons, except for their runtime
/// directories.
struct Instance {
    root: tempfile::TempDir,
}

impl Instance {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for directory in [
            "home", "data", "config", "state", "cache", "tmp", "run-a", "run-b",
        ] {
            fs::create_dir(root.path().join(directory)).unwrap();
            fs::set_permissions(
                root.path().join(directory),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn daemon(&self, runtime: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cagentsd"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path("home"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CACHE_HOME", self.path("cache"))
            .env("TMPDIR", self.path("tmp"))
            .env("XDG_RUNTIME_DIR", self.path(runtime))
            .current_dir(self.root.path())
            .stdin(Stdio::null());
        command
    }

    fn runtime_directory(&self, runtime: &str) -> PathBuf {
        self.path(runtime)
            .join(cagents::instance::runtime_subdirectory())
    }

    fn start(&self, runtime: &str) -> Child {
        let child = self
            .daemon(runtime)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        self.wait_until_ready(runtime);
        child
    }

    /// Ready once it accepts a client and has published its Ready evidence.
    fn wait_until_ready(&self, runtime: &str) {
        let socket = self
            .runtime_directory(runtime)
            .join(cagents::terminal_ipc::SOCKET_FILE_NAME);
        let deadline = Instant::now() + STARTUP;
        while UnixStream::connect(&socket).is_err() || self.ready_outcome().is_none() {
            assert!(
                Instant::now() < deadline,
                "the daemon in {runtime} did not start"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn ready_outcome(&self) -> Option<PathBuf> {
        files(&self.path("data"))
            .into_keys()
            .find(|path| path.ends_with("daemon-upgrade.ready-outcome.json"))
    }

    /// Every saved file but the claims themselves, whose lock state is what
    /// the second daemon is allowed to look at.
    fn saved(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut saved = files(&self.path("data"));
        saved.retain(|path, _| !path.components().any(|part| part.as_os_str() == "claims"));
        saved
    }

    /// The saved data once a starting daemon has finished writing it.
    fn settled(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let deadline = Instant::now() + STARTUP;
        let mut last = self.saved();
        loop {
            thread::sleep(Duration::from_millis(250));
            let current = self.saved();
            if current == last {
                return current;
            }
            assert!(Instant::now() < deadline, "the saved data never settled");
            last = current;
        }
    }
}

fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(path),
                Ok(kind) if kind.is_file() => {
                    if let Ok(bytes) = fs::read(&path) {
                        found.insert(path, bytes);
                    }
                }
                _ => {}
            }
        }
    }
    found
}

fn stop(mut daemon: Child) {
    // SAFETY: kill takes a pid this test owns and a signal number.
    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    daemon.wait().unwrap();
}

/// Starts a daemon in `runtime` and returns what it said as it refused. One
/// that serves instead is stopped, and fails the test rather than hanging it.
fn refused(instance: &Instance, runtime: &str) -> String {
    let mut daemon = instance
        .daemon(runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + STARTUP;
    let status = loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            daemon.kill().unwrap();
            let output = daemon.wait_with_output().unwrap();
            panic!(
                "the daemon in {runtime} served the same data: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut daemon.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(
        !status.success(),
        "the daemon in {runtime} exited cleanly: {stderr}"
    );
    stderr
}

#[test]
fn a_daemon_in_another_runtime_directory_refuses_before_touching_the_saved_data() {
    let instance = Instance::new();
    let first = instance.start("run-a");
    let ready = fs::read(instance.ready_outcome().unwrap()).unwrap();
    let before = instance.settled();
    assert!(
        before
            .keys()
            .any(|path| path.ends_with("workspace-model.sqlite3")),
        "the first daemon saved no workspace model: {:?}",
        before.keys().collect::<Vec<_>>()
    );

    let stderr = refused(&instance, "run-b");
    assert!(
        stderr.contains(&format!("process {}", first.id())),
        "the refusal names the serving daemon: {stderr}"
    );
    let serving = fs::canonicalize(instance.runtime_directory("run-a")).unwrap();
    assert!(
        stderr.contains(&serving.display().to_string()),
        "the refusal names the serving runtime directory: {stderr}"
    );

    assert_eq!(
        instance.saved(),
        before,
        "the refused daemon changed the saved data"
    );
    assert_eq!(
        fs::read(instance.ready_outcome().unwrap()).unwrap(),
        ready,
        "the refused daemon replaced the serving daemon's Ready evidence"
    );
    assert!(
        !instance
            .runtime_directory("run-b")
            .join(cagents::terminal_ipc::SOCKET_FILE_NAME)
            .exists(),
        "the refused daemon left a socket behind"
    );
    stop(first);
}

#[test]
fn an_update_keeps_the_saved_data_for_its_own_successor() {
    let instance = Instance::new();
    let first = instance.start("run-a");
    let lock_path = instance.runtime_directory("run-a").join("cagentsd.lock");
    stop(first);

    // The updater takes the stopped daemon's lease and keeps it across the
    // installation, as `LeaseHandoff` does.
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    // SAFETY: flock takes a descriptor this test owns and scalar flags.
    assert_eq!(
        unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    // Nobody holds the claim now, but the lease its record names is held.
    let before = instance.settled();
    refused(&instance, "run-b");
    assert_eq!(instance.saved(), before);

    // The successor inherits the lease and takes the claim through it.
    let descriptor = lease.as_raw_fd();
    let mut successor = instance.daemon("run-a");
    successor
        .env("CAGENTS_DAEMON_LEASE_FD", "199")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe descriptor calls between fork and exec.
    unsafe {
        successor.pre_exec(move || {
            if libc::dup2(descriptor, 199) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let successor = successor.spawn().unwrap();
    drop(lease);
    instance.wait_until_ready("run-a");

    // And keeps excluding the other runtime directory once it serves.
    let stderr = refused(&instance, "run-b");
    assert!(
        stderr.contains(&format!("process {}", successor.id())),
        "{stderr}"
    );
    stop(successor);
}
