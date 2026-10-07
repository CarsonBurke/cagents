#![cfg(target_os = "linux")]

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
    for status in ["0", "7", "cancel"] {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let name = if cfg!(any(debug_assertions, feature = "development")) {
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
            .env("HOME", root.path())
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
fn staged_cli_routes_new_windows_and_control_to_kernel_verified_matched_owner() {
    let root = tempfile::tempdir().unwrap();
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
            .env("HOME", root.path())
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
}

#[test]
fn staged_native_ui_owner_fixture() {
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
