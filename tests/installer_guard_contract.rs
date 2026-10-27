#![cfg(target_os = "linux")]

#[path = "support/user_fixture.rs"]
mod user_fixture;

use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            net::UnixListener,
            process::CommandExt,
        },
    },
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

// These fixtures copy executables and fork with pre_exec hooks. A sibling fork
// can briefly inherit a copy's writable FD before CLOEXEC, causing ETXTBSY in
// another fixture. Serialize the complete OS fixture, not product execution.
static OS_FIXTURE: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct OwnedProcess(std::process::Child);
impl OwnedProcess {
    fn window(executable: &Path, runtime: &Path, ready: &Path) -> Self {
        fs::copy(std::env::current_exe().unwrap(), executable).unwrap();
        let child = Command::new(executable)
            .args(["--exact", "staged_window_lease_fixture", "--nocapture"])
            .env("CAGENTS_TEST_UI_LEASE", ready)
            .env("XDG_RUNTIME_DIR", runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let fixture = Self(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ready.exists(),
            "installed fixture UI must acquire its actual lease"
        );
        fixture
    }
}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn staged_window_lease_fixture() {
    use std::io::Read;
    let Some(ready) = std::env::var_os("CAGENTS_TEST_UI_LEASE") else {
        return;
    };
    let _lease = cagents::instance::WindowLease::acquire(false, None).unwrap();
    fs::write(ready, "ready").unwrap();
    let _ = std::io::stdin().read(&mut [0]);
}

fn verify(runtime: &Path, lease: Option<&File>) -> Output {
    verify_mode(runtime, lease, false)
}

fn verify_mode(runtime: &Path, lease: Option<&File>, stage_only: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cagents"));
    command
        .arg("verify-install-guard")
        .env("XDG_RUNTIME_DIR", runtime)
        .env("CAGENTS_UI_UPDATE_GUARD_FD", "197");
    if stage_only {
        command.arg("--stage-only");
    }
    if let Some(lease) = lease {
        let fd = lease.as_raw_fd();
        // SAFETY: the parent retains this descriptor until output returns.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 197) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(197, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.output().unwrap()
}

#[test]
fn external_installer_saves_closes_and_reopens_windows_even_on_failure() {
    let _fixture = OS_FIXTURE.lock().unwrap();
    for status in ["0", "7", "cancel", "deferred", "complete"] {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let development = cfg!(any(debug_assertions, feature = "development"));
        let name = if development {
            "cagents-development-ui"
        } else {
            "cagents-ui"
        };
        let window = file(&runtime.join(format!("{name}.lock.primary")));
        assert_eq!(
            unsafe { libc::flock(window.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let listener = UnixListener::bind(runtime.join(format!("{name}.activate"))).unwrap();
        listener.set_nonblocking(true).unwrap();
        let events = root.path().join("events");
        let server_events = events.clone();
        let server = thread::spawn(move || {
            for expected in ["update-prepare", "update-commit"] {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Err(error) => panic!("window request unavailable: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = String::new();
                BufReader::new(&stream).read_line(&mut request).unwrap();
                assert_eq!(request.trim(), expected);
                writeln!(
                    OpenOptions::new()
                        .append(true)
                        .create(true)
                        .open(&server_events)
                        .unwrap(),
                    "{expected}"
                )
                .unwrap();
                if expected == "update-commit" {
                    assert_eq!(unsafe { libc::flock(window.as_raw_fd(), libc::LOCK_UN) }, 0);
                }
                writeln!(stream, "ok").unwrap();
            }
        });
        let prefix = root.path().join("installation");
        let bin = prefix.join("versions/old/bin");
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink("versions/old", prefix.join("current")).unwrap();
        let ui = bin.join("cagents-ui");
        fs::write(
            &ui,
            "#!/bin/sh\ntest -z \"${CAGENTS_UI_UPDATE_GUARD_FD+x}${CAGENTS_INSTALL_RESULT_FD+x}${CAGENTS_ARCHIVE+x}${CAGENTS_CHECKSUMS+x}${CAGENTS_REPOSITORY+x}\" || exit 92\nprintf 'old %s\\n' \"$*\" > \"$HOME/reopened\"\nprintf 'ready\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&ui, fs::Permissions::from_mode(0o700)).unwrap();
        let new_bin = prefix.join("versions/new/bin");
        fs::create_dir_all(&new_bin).unwrap();
        let new_ui = new_bin.join("cagents-ui");
        fs::write(
            &new_ui,
            fs::read_to_string(&ui)
                .unwrap()
                .replace("'old %s", "'new %s"),
        )
        .unwrap();
        fs::set_permissions(&new_ui, fs::Permissions::from_mode(0o700)).unwrap();
        let installer = root.path().join("installer.sh");
        fs::write(&installer, "#!/bin/sh\n\"$FIXTURE_REAL_CLI\" verify-install-guard >/dev/null || exit 91\ntrap 'printf \"rolled-back\\n\" >> \"$HOME/events\"; exit 7' TERM\nprintf 'installed\\n' >> \"$HOME/events\"\nif [ \"$1\" = cancel ]; then while :; do sleep 0.05; done; fi\nif [ \"$1\" = deferred ]; then \"$FIXTURE_REAL_CLI\" record-deferred-install >/dev/null || exit 93; fi\ncase \"$1\" in deferred|complete) ln -sfn versions/new \"$FIXTURE_PREFIX/current\"; exit 0 ;; esac\nexit \"$1\"\n").unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_cagents"));
        command
            .arg("coordinate-install")
            .arg("--prefix")
            .arg(&prefix)
            .arg("--installer")
            .arg(&installer)
            .arg("--")
            .arg(status)
            .env("HOME", root.path())
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env_remove("CAGENTS_UI_UPDATE_GUARD_FD")
            .env("CAGENTS_ARCHIVE", "/stale/installer/archive")
            .env("CAGENTS_CHECKSUMS", "/stale/installer/checksums")
            .env("CAGENTS_REPOSITORY", "fixture/source")
            .env("FIXTURE_REAL_CLI", env!("CARGO_BIN_EXE_cagents"))
            .env("FIXTURE_PREFIX", &prefix)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Ignored SIGCHLD survives exec; the coordinator must reset it before
        // relying on its unreaped child as process-group cleanup authority.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGCHLD, libc::SIG_IGN);
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        if status == "cancel" {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !fs::read_to_string(&events).is_ok_and(|text| text.contains("installed")) {
                assert!(Instant::now() < deadline, "installer did not start");
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("coordinator did not finish");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        server.join().unwrap();
        assert_eq!(
            output.status.success(),
            matches!(status, "0" | "deferred" | "complete"),
            "{output:?}"
        );
        assert_eq!(
            fs::read_to_string(events).unwrap(),
            if status == "cancel" {
                "update-prepare\nupdate-commit\ninstalled\nrolled-back\n"
            } else {
                "update-prepare\nupdate-commit\ninstalled\n"
            }
        );
        assert!(
            fs::read_to_string(root.path().join("reopened"))
                .unwrap()
                .contains("--window primary")
        );
        assert!(
            fs::read_to_string(root.path().join("reopened"))
                .unwrap()
                .starts_with(if status == "complete" { "new " } else { "old " }),
            "deferred installations must reopen the exact prior UI version"
        );
    }
}

fn file(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap()
}

#[test]
fn inherited_fence_requires_exact_locked_description_and_closed_windows() {
    let _fixture = OS_FIXTURE.lock().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let name = if cfg!(any(debug_assertions, feature = "development")) {
        "cagents-development-ui.lock"
    } else {
        "cagents-ui.lock"
    };
    let lease = file(&runtime.path().join(format!("{name}.update")));
    assert!(
        !verify(runtime.path(), None).status.success(),
        "an environment flag is not a lease"
    );
    assert!(
        !verify(runtime.path(), Some(&lease)).status.success(),
        "an unlocked file is not authority"
    );
    assert!(
        !verify_mode(runtime.path(), Some(&lease), true)
            .status
            .success(),
        "an unlocked staging fence is not authority"
    );
    assert_eq!(
        unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let different = file(&runtime.path().join("different"));
    assert!(!verify(runtime.path(), Some(&different)).status.success());
    for stage_only in [false, true] {
        assert!(
            !verify_mode(runtime.path(), None, stage_only)
                .status
                .success()
        );
        assert!(
            !verify_mode(runtime.path(), Some(&different), stage_only)
                .status
                .success()
        );
    }
    let valid = verify(runtime.path(), Some(&lease));
    assert!(valid.status.success(), "{valid:?}");
    let independent = File::open(runtime.path().join(format!("{name}.update"))).unwrap();
    assert_eq!(
        unsafe { libc::flock(independent.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        -1,
        "validation must not unlock its parent"
    );
    let window = file(&runtime.path().join(format!("{name}.primary")));
    assert_eq!(
        unsafe { libc::flock(window.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(
        !verify(runtime.path(), Some(&lease)).status.success(),
        "an unclosed window cannot be bypassed"
    );
    let staged = verify_mode(runtime.path(), Some(&lease), true);
    assert!(staged.status.success(), "{staged:?}");
    assert_eq!(
        unsafe { libc::flock(independent.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        -1
    );
}

#[test]
fn staging_never_contacts_or_reopens_live_windows_even_on_failure_or_cancel() {
    let _fixture = OS_FIXTURE.lock().unwrap();
    for status in ["0", "7", "cancel"] {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let runtime = root.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let name = if cfg!(any(debug_assertions, feature = "development")) {
            "cagents-development-ui"
        } else {
            "cagents-ui"
        };
        let listener = UnixListener::bind(runtime.join(format!("{name}.activate"))).unwrap();
        listener.set_nonblocking(true).unwrap();
        let prefix = root.path().join("install");
        for version in ["old", "new"] {
            let bin = prefix.join("versions").join(version).join("bin");
            fs::create_dir_all(&bin).unwrap();
            for name in ["cagents", "cagentsd", "cagents-ui"] {
                fs::write(
                    bin.join(name),
                    "#!/bin/sh\nprintf 'unexpected launch\\n' >> \"$TEST_ACTIONS\"\nexit 90\n",
                )
                .unwrap();
                fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        fs::write(
            prefix.join(".cagents-install-root"),
            "cagents install root\n",
        )
        .unwrap();
        let _window = OwnedProcess::window(
            &prefix.join("versions/old/bin/cagents-ui"),
            &runtime,
            &root.path().join("window-ready"),
        );
        std::os::unix::fs::symlink("versions/old", prefix.join("current")).unwrap();
        let script = root.path().join("installer");
        fs::write(&script, r#"#!/bin/sh
set -eu
"$TEST_CLI" verify-install-guard --stage-only
if "$TEST_CLI" verify-install-guard; then exit 91; fi
"$TEST_CLI" record-deferred-install --stage-only --prefix "$TEST_PREFIX" --version-root "$TEST_PREFIX/versions/new"
printf ready > "$TEST_READY"
case "$TEST_STATUS" in
  0) ln -s versions/new "$TEST_PREFIX/next"; mv -Tf "$TEST_PREFIX/next" "$TEST_PREFIX/current" ;;
  7) exit 7 ;;
  cancel) sleep 30 ;;
esac
"#).unwrap();
        let ready = root.path().join("ready");
        let actions = root.path().join("actions");
        let mut child = Command::new(env!("CARGO_BIN_EXE_cagents"))
            .args(["coordinate-install", "--stage-only", "--prefix"])
            .arg(&prefix)
            .arg("--installer")
            .arg(&script)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("TEST_CLI", env!("CARGO_BIN_EXE_cagents"))
            .env("TEST_PREFIX", &prefix)
            .env("TEST_READY", &ready)
            .env("TEST_ACTIONS", &actions)
            .env("TEST_STATUS", status)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ready.exists(),
            "staging did not reach its held-fence transaction"
        );
        if status == "cancel" {
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        }
        assert_eq!(child.wait().unwrap().success(), status == "0");
        assert!(!actions.exists(), "no UI or daemon may be launched");
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "no prepare, commit or cancel request may be sent"
        );
        let independent = File::open(runtime.join(format!("{name}.lock.primary"))).unwrap();
        assert_eq!(
            unsafe { libc::flock(independent.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1,
            "window lease must survive"
        );
        let fence = File::open(runtime.join(format!("{name}.lock.update"))).unwrap();
        assert_eq!(
            unsafe { libc::flock(fence.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "staging must release its launch fence"
        );
        assert_eq!(
            fs::read_link(prefix.join("current")).unwrap(),
            Path::new(if status == "0" {
                "versions/new"
            } else {
                "versions/old"
            })
        );
    }
}

#[test]
fn staged_candidate_startup_authenticates_the_inherited_coordinator_fence() {
    let _fixture = OS_FIXTURE.lock().unwrap();
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let prefix = root.path().join("install");
    for version in ["old", "new"] {
        let bin = prefix.join("versions").join(version).join("bin");
        fs::create_dir_all(&bin).unwrap();
        for name in ["cagents", "cagentsd", "cagents-ui"] {
            fs::write(bin.join(name), "complete matched fixture").unwrap();
        }
    }
    let candidate = prefix.join("versions/new/bin/cagents");
    fs::copy(env!("CARGO_BIN_EXE_cagents"), &candidate).unwrap();
    fs::write(
        prefix.join(".cagents-install-root"),
        "cagents install root\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("versions/new", prefix.join("current")).unwrap();
    let marker = prefix.join("versions/new/.deferred-owner.json");
    fs::write(&marker, r#"{"version":1,"owner_version":"old"}"#).unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    let runtime = root.path().join("runtime");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let name = if cfg!(any(debug_assertions, feature = "development")) {
        "cagents-development-ui.lock"
    } else {
        "cagents-ui.lock"
    };
    // After cooperative UI shutdown the coordinator keeps both fences alive.
    // There is deliberately no daemon or window to authenticate instead.
    let legacy = file(&runtime.join(name));
    assert_eq!(
        unsafe { libc::flock(legacy.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        0
    );
    let update_path = runtime.join(format!("{name}.update"));
    let update = file(&update_path);
    let run = |inherited: Option<&File>, marker: Option<&str>| {
        let mut command = Command::new(&candidate);
        command
            .args(["--json", "daemon", "status"])
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env_remove("CAGENTS_DAEMON_LAUNCH_FENCE_FD")
            .env_remove("CAGENTS_UI_UPDATE_GUARD_FD");
        if let Some(marker) = marker {
            command.env("CAGENTS_UI_UPDATE_GUARD_FD", marker);
        }
        let descriptor = inherited.map(AsRawFd::as_raw_fd);
        // SAFETY: only descriptor operations run after fork, and the parent
        // retains each input description until the child exits.
        unsafe {
            command.pre_exec(move || {
                if let Some(descriptor) = descriptor {
                    if libc::dup2(descriptor, 197) == -1 || libc::fcntl(197, libc::F_SETFD, 0) == -1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                } else {
                    libc::close(197);
                }
                Ok(())
            });
        }
        command.output().unwrap()
    };
    let ordinary = run(None, None);
    assert!(
        !ordinary.status.success(),
        "legacy contention must remain a refusal: {ordinary:?}"
    );
    for (descriptor, marker) in [
        (None, "197"),
        (Some(&update), "197"),
        (Some(&update), "999"),
    ] {
        let output = run(descriptor, Some(marker));
        assert!(
            !output.status.success(),
            "missing, unheld or malformed authority: {output:?}"
        );
    }
    assert_eq!(
        unsafe { libc::flock(update.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let different = file(&runtime.join("unrelated-update"));
    let independent = File::open(&update_path).unwrap();
    for descriptor in [&different, &independent] {
        let output = run(Some(descriptor), Some("197"));
        assert!(
            !output.status.success(),
            "a different description must not borrow authority: {output:?}"
        );
    }
    let valid = run(Some(&update), Some("197"));
    assert!(valid.status.success(), "{valid:?}");
    let result: serde_json::Value = serde_json::from_slice(&valid.stdout).unwrap();
    assert_eq!(result["data"]["running"], false);
    assert_eq!(
        unsafe { libc::flock(independent.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        -1,
        "validation must not release the coordinator's exclusive fence"
    );
    assert_eq!(unsafe { libc::flock(update.as_raw_fd(), libc::LOCK_UN) }, 0);
    // An older coordinator releases its launch fence before native reopen, but
    // keeps the shared legacy fence until the new window handshakes. Pin an
    // actual candidate executable with the daemon's kernel lease, not a stale
    // path/marker claim. No provider or production daemon is contacted.
    let daemon_dir = runtime.join(cagents::instance::RUNTIME_SUBDIRECTORY);
    fs::create_dir_all(&daemon_dir).unwrap();
    fs::set_permissions(&daemon_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let candidate_daemon = prefix.join("versions/new/bin/cagentsd");
    fs::copy("/bin/sleep", &candidate_daemon).unwrap();
    let daemon_lock_path = daemon_dir.join("cagentsd.lock");
    let daemon_lock = file(&daemon_lock_path);
    assert_eq!(
        unsafe { libc::flock(daemon_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let mut command = Command::new(&candidate_daemon);
    command.arg("30");
    let descriptor = daemon_lock.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(descriptor, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let daemon = OwnedProcess(command.spawn().unwrap());
    fs::write(&daemon_lock_path, format!(r#"{{"pid":{}}}"#, daemon.0.id())).unwrap();
    let native = prefix.join("versions/new/bin/cagents-ui");
    fs::copy(std::env::current_exe().unwrap(), &native).unwrap();
    let reopened = Command::new(&native)
        .args(["--exact", "staged_native_ui_owner_fixture", "--nocapture"])
        .env("CAGENTS_TEST_STAGED_NATIVE_OWNER", "candidate")
        .env("XDG_RUNTIME_DIR", &runtime)
        .env_remove("CAGENTS_DAEMON_LAUNCH_FENCE_FD")
        .env_remove("CAGENTS_UI_UPDATE_GUARD_FD")
        .output()
        .unwrap();
    assert!(reopened.status.success(), "{reopened:?}");
    let candidate_status = run(None, None);
    assert!(candidate_status.status.success(), "{candidate_status:?}");
    let result: serde_json::Value = serde_json::from_slice(&candidate_status.stdout).unwrap();
    assert_eq!(result["data"]["running"], true);
    assert_eq!(
        unsafe { libc::flock(legacy.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let legacy_owner = run(None, None);
    assert!(
        !legacy_owner.status.success(),
        "an exclusive legacy UI cannot be bypassed by a candidate daemon: {legacy_owner:?}"
    );
    assert_eq!(
        unsafe { libc::flock(legacy.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        0
    );
    drop(daemon);
    drop(daemon_lock);
    let stopped_candidate = run(None, None);
    assert!(
        !stopped_candidate.status.success(),
        "a stopped candidate cannot relax unknown shared legacy ownership: {stopped_candidate:?}"
    );
}

#[test]
fn staged_cli_routes_new_windows_and_control_to_kernel_verified_matched_owner() {
    if !user_fixture::enter() {
        return;
    }
    let _fixture = OS_FIXTURE.lock().unwrap();
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let prefix = root.path().join("install");
    let old = prefix.join("versions/old/bin");
    let new = prefix.join("versions/new/bin");
    for bin in [&old, &new] {
        fs::create_dir_all(bin).unwrap();
    }
    fs::write(
        prefix.join(".cagents-install-root"),
        "cagents install root\n",
    )
    .unwrap();
    fs::copy(env!("CARGO_BIN_EXE_cagents"), new.join("cagents")).unwrap();
    for bin in [&old, &new] {
        fs::copy("/bin/sleep", bin.join("cagentsd")).unwrap();
        fs::write(bin.join("cagents-ui"), "complete matched fixture").unwrap();
    }
    fs::write(old.join("cagents"), "#!/bin/sh\nprintf '%s\\n' \"$*\"\n").unwrap();
    fs::set_permissions(old.join("cagents"), fs::Permissions::from_mode(0o755)).unwrap();
    let marker = prefix.join("versions/new/.deferred-owner.json");
    fs::write(&marker, r#"{"version":1,"owner_version":"old"}"#).unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    let runtime = root.path().join("runtime");
    let daemon_dir = runtime.join(if cfg!(any(debug_assertions, feature = "development")) {
        "cagents-development"
    } else {
        "cagents"
    });
    fs::create_dir_all(&daemon_dir).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&daemon_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let lock = file(&daemon_dir.join("cagentsd.lock"));
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let fd = lock.as_raw_fd();
    let mut command = Command::new(old.join("cagentsd"));
    command.arg("30");
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut owner = command.spawn().unwrap();
    fs::write(
        daemon_dir.join("cagentsd.lock"),
        format!("{{\"pid\":{}}}", owner.id()),
    )
    .unwrap();
    for args in [
        vec!["ui", "--new-window"],
        vec!["--json", "daemon", "status"],
        vec!["terminal", "list"],
    ] {
        let output = Command::new(new.join("cagents"))
            .args(&args)
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            args.join(" ")
        );
    }
    // Exercise the real native launch resolver from a matched candidate path.
    // An older updater reopens that exact UI path without retiring our marker.
    fs::copy(std::env::current_exe().unwrap(), new.join("cagents-ui")).unwrap();
    let native_route = |expected: &str| {
        let output = Command::new(new.join("cagents-ui"))
            .args(["--exact", "staged_native_ui_owner_fixture", "--nocapture"])
            .env("CAGENTS_TEST_STAGED_NATIVE_OWNER", expected)
            .env("CAGENTS_TEST_STAGED_OWNER_PATH", old.join("cagents-ui"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    native_route("old");
    fs::write(
        &marker,
        serde_json::to_vec(&serde_json::json!({
            "version": 1, "owner_version": "old", "daemon_compatible": true,
            "candidate_protocol": cagents::terminal_ipc::PROTOCOL_VERSION,
        }))
        .unwrap(),
    )
    .unwrap();
    native_route("candidate");
    let output = Command::new(new.join("cagents"))
        .args(["--json", "daemon", "status"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert_ne!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "--json daemon status",
        "compatible control must use the candidate"
    );
    // Untrusted marker shape and filesystem authority fail before any routing.
    for body in [
        "not JSON",
        r#"{"version":2,"owner_version":"old"}"#,
        r#"{"version":1,"owner_version":"../old"}"#,
    ] {
        fs::write(&marker, body).unwrap();
        let output = Command::new(new.join("cagents"))
            .args(["terminal", "list"])
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "invalid marker cannot select old control"
        );
    }
    fs::write(&marker, r#"{"version":1,"owner_version":"old"}"#).unwrap();
    let alias = prefix.join("alias");
    fs::hard_link(&marker, &alias).unwrap();
    let output = Command::new(new.join("cagents"))
        .args(["terminal", "list"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(!output.status.success(), "shared marker inode cannot route");
    fs::remove_file(&alias).unwrap();
    fs::rename(&marker, &alias).unwrap();
    std::os::unix::fs::symlink(&alias, &marker).unwrap();
    let output = Command::new(new.join("cagents"))
        .args(["terminal", "list"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(!output.status.success(), "marker symlink cannot route");
    fs::remove_file(&marker).unwrap();
    fs::rename(&alias, &marker).unwrap();
    // Local capability discovery must report the candidate, without forwarding.
    let output = Command::new(new.join("cagents"))
        .args(["daemon", "protocol-version"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_ne!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "daemon protocol-version"
    );
    owner.kill().unwrap();
    owner.wait().unwrap();
    drop(lock);
    // A real installed old UI survives its daemon. Read-only public commands
    // select its CLI without invoking daemon start; autostart attempts exactly
    // the old CLI, never silently falls through to the incompatible candidate.
    let actions = root.path().join("recovery-actions");
    fs::write(
        old.join("cagents"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$CAGENTS_TEST_RECOVERY_ACTIONS"
printf '%s\n' "$*"
"#,
    )
    .unwrap();
    let window = OwnedProcess::window(
        &old.join("cagents-ui"),
        &runtime,
        &root.path().join("surviving-ui-ready"),
    );
    for (args, expected) in [
        (vec!["--json", "daemon", "status"], "--json daemon status"),
        (vec!["daemon", "preflight"], "daemon preflight"),
        // Stop carries its implicit no-start policy to the matched owner.
        (vec!["daemon", "stop"], "--no-start daemon stop"),
    ] {
        let output = Command::new(new.join("cagents"))
            .args(&args)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("CAGENTS_TEST_RECOVERY_ACTIONS", &actions)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);
    }
    assert_eq!(fs::read_to_string(&actions).unwrap().lines().count(), 3);
    let handoff = OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            runtime.join(if cfg!(any(debug_assertions, feature = "development")) {
                "cagents-development-ui.lock.update"
            } else {
                "cagents-ui.lock.update"
            }),
        )
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(handoff.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let output = Command::new(new.join("cagents"))
        .args(["daemon", "start"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("CAGENTS_TEST_RECOVERY_ACTIONS", &actions)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "an update handoff vetoes recovery"
    );
    assert_eq!(fs::read_to_string(&actions).unwrap().lines().count(), 3);
    drop(handoff);
    let output = Command::new(new.join("cagents"))
        .args(["daemon", "start"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("CAGENTS_TEST_RECOVERY_ACTIONS", &actions)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "a success exit alone cannot authenticate the restored daemon"
    );
    assert_eq!(
        fs::read_to_string(&actions).unwrap(),
        "--json daemon status\ndaemon preflight\n--no-start daemon stop\ndaemon start\n"
    );
    drop(window);
    // A stopped owner does not authorize stale routing: the candidate reports
    // normal daemon status instead of invoking the old fixture CLI.
    let output = Command::new(new.join("cagents"))
        .args(["--json", "daemon", "status"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("\"running\":false")
    );
    // A candidate that started with no previous owner must remain usable. Its
    // own verified kernel daemon identity makes the old unknown marker stale.
    fs::write(&marker, r#"{"version":1,"owner_version":null}"#).unwrap();
    let candidate_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(daemon_dir.join("cagentsd.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(candidate_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let fd = candidate_lock.as_raw_fd();
    let mut command = Command::new(new.join("cagentsd"));
    command.arg("30");
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let candidate = OwnedProcess(command.spawn().unwrap());
    fs::write(
        daemon_dir.join("cagentsd.lock"),
        format!("{{\"pid\":{}}}", candidate.0.id()),
    )
    .unwrap();
    let output = Command::new(new.join("cagents"))
        .args(["--json", "daemon", "status"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("\"running\":true")
    );
    drop(candidate);
    drop(candidate_lock);
}

#[test]
fn staged_native_ui_owner_fixture() {
    let _fixture = OS_FIXTURE.lock().unwrap();
    let Some(expected) = std::env::var_os("CAGENTS_TEST_STAGED_NATIVE_OWNER") else {
        return;
    };
    let selected = cagents::control::staged_ui_executable().unwrap();
    if expected == "old" {
        assert_eq!(
            selected,
            Some(std::path::PathBuf::from(
                std::env::var_os("CAGENTS_TEST_STAGED_OWNER_PATH").unwrap()
            ))
        );
    } else {
        assert_eq!(
            selected, None,
            "reviewed compatible reopening must not bounce to the old UI"
        );
    }
}

#[test]
fn sealed_anonymous_update_probe_does_not_require_installed_routing() {
    let _fixture = OS_FIXTURE.lock().unwrap();
    use std::os::fd::FromRawFd;
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("runtime");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    for sealed in [false, true] {
        let anonymous = if sealed {
            let fd = unsafe {
                libc::memfd_create(
                    c"cagents-update-probe".as_ptr(),
                    libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
                )
            };
            assert_ne!(fd, -1);
            unsafe { File::from_raw_fd(fd) }
        } else {
            let path = root.path().join("unlinked-update-probe");
            let file = file(&path);
            fs::remove_file(path).unwrap();
            file
        };
        let mut source = File::open(env!("CARGO_BIN_EXE_cagents")).unwrap();
        std::io::copy(&mut source, &mut &anonymous).unwrap();
        fs::set_permissions(
            format!("/proc/self/fd/{}", anonymous.as_raw_fd()),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        if sealed {
            assert_eq!(
                unsafe {
                    libc::fcntl(
                        anonymous.as_raw_fd(),
                        libc::F_ADD_SEALS,
                        libc::F_SEAL_WRITE
                            | libc::F_SEAL_SHRINK
                            | libc::F_SEAL_GROW
                            | libc::F_SEAL_SEAL,
                    )
                },
                0
            );
        }
        // A regular executable cannot remain open for writing during exec.
        // Preserve its unlinked inode through a read-only descriptor instead.
        let anonymous = if sealed {
            anonymous
        } else {
            let readable = File::open(format!("/proc/self/fd/{}", anonymous.as_raw_fd())).unwrap();
            drop(anonymous);
            readable
        };
        let fd = anonymous.as_raw_fd();
        let mut command = Command::new(format!("/proc/self/fd/{fd}"));
        command
            .args(["--json", "daemon", "status"])
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("XDG_RUNTIME_DIR", &runtime);
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "anonymous probe sealed={sealed}: {output:?}"
        );
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(response["command"], "daemon.status");
        assert_eq!(response["data"]["running"], false);
    }
}
