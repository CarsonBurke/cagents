//! A locally installed matched set uses the same review and checkpoint flow.
use super::*;

const BINARIES: [&str; 3] = ["cagents", "cagents-ui", "cagentsd"];

#[derive(PartialEq, Eq)]
struct Stamp(Vec<(u64, u64, u64, i64, i64, i64, i64)>);

fn stamp(root: &Path) -> Result<Stamp, UpdateError> {
    let mut entries = Vec::new();
    for binary in BINARIES {
        let metadata = fs::symlink_metadata(root.join("bin").join(binary))?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::getuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err(invalid(
                "installed candidate binaries must be owned regular files without shared write permission",
            ));
        }
        entries.push((
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ));
    }
    Ok(Stamp(entries))
}

fn digest(root: &Path) -> Result<String, UpdateError> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    for binary in BINARIES {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join("bin").join(binary))?;
        let size = file.metadata()?.len();
        if size > ARCHIVE_LIMIT {
            return Err(invalid("installed binary exceeds update size limit"));
        }
        hash.update(binary.as_bytes());
        hash.update(size.to_le_bytes());
        let mut consumed = 0u64;
        let mut limited = Read::by_ref(&mut file).take(size + 1);
        loop {
            let count = limited.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            consumed += count as u64;
            hash.update(&buffer[..count]);
        }
        if consumed != size {
            return Err(invalid("installed binary changed during verification"));
        }
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(super) fn perform(
    installation: &Installation,
    state: &Path,
    job: &mut UpdateJob,
    root: &Path,
) -> Result<(), UpdateError> {
    let layout = locate(&std::env::current_exe()?)?;
    let (source_protocol, source_proof) = source::inspect_identity(None)?;
    // Retain the existing exact installation boundary even for a legacy peer.
    let _source_owner = installed_daemon_owner(&layout)?;
    let legacy_source = source_proof.is_some()
        && matches!(source_protocol, 77 | 119)
        && crate::terminal_ipc::PROTOCOL_VERSION > 77;
    if served_version_root(&layout)? != root || (root == layout.version_root && !legacy_source) {
        return Err(invalid(
            "the installed candidate changed; inspect the installation again",
        ));
    }
    set_phase(
        state,
        job,
        UpdatePhase::Verifying,
        "Verifying the installed application set",
    )?;
    let before = stamp(root)?;
    let identity = digest(root)?;
    let mut executable = anonymous_file()?;
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root.join("bin/cagents"))?;
    io::copy(
        &mut Read::by_ref(&mut source).take(512 * 1024 * 1024 + 1),
        &mut executable,
    )?;
    if executable.metadata()?.len() > 512 * 1024 * 1024 {
        return Err(invalid("installed CLI exceeds update size limit"));
    }
    seal_file(&executable)?;
    let cancellation = CancellationToken::new();
    let staged =
        candidate::Staged::from_file(executable, job.version.clone(), identity, &cancellation)?;
    let observe = |policy| {
        if served_version_root(&layout)? != root || stamp(root)? != before {
            return Err(invalid(
                "the installed application changed; review the new build before updating",
            ));
        }
        staged.preview(policy)
    };
    if staged.identity.protocol == source_protocol {
        staged.verify_compatible_handshake(&cancellation)?;
    }
    reconcile_saved_sources(staged.identity.protocol)?;
    let confirmed = confirmation::wait(state, job, observe)?;
    set_phase(
        state,
        job,
        UpdatePhase::ClosingWindows,
        "Saving and closing application windows",
    )?;
    let mut guard = crate::instance::UiUpdateGuard::acquire()?;
    crate::install_guard::preflight_editor_companions(Some(staged.identity.protocol))?;
    let windows = guard.prepare_and_close_windows_with_preflight(Duration::from_secs(45), |guard| {
        crate::install_guard::preflight_source_panes(guard,
            confirmed.daemon.as_ref().map(|daemon| daemon.capability.proof.clone()), confirmed.replaces_daemon)?;
        let latest = observe(confirmed.daemon.as_ref().map_or(impact::Policy::WhenIdle, |daemon| daemon.policy))
            .map_err(io::Error::other)?;
        if !confirmed.preserves_consent(&latest) || digest(root).map_err(io::Error::other)? != staged.identity.digest {
            return Err(io::Error::other("The installed build or session ownership changed during pane inspection; existing windows were kept open"));
        }
        Ok(())
    })?;
    let mut startup_attempted = false;
    let mut startup_owned = false;
    let result = (|| {
        let current = observe(
            confirmed
                .daemon
                .as_ref()
                .map_or(impact::Policy::WhenIdle, |daemon| daemon.policy),
        )?;
        if !confirmed.preserves_consent(&current) || digest(root)? != staged.identity.digest {
            return Err(invalid(
                "the installed build or session ownership changed; review the update again",
            ));
        }
        if let Some(daemon) = current.daemon {
            let socket = crate::terminal_ipc::daemon_socket_path()?;
            let mut client =
                source::Client::connect(&socket).map_err(|error| invalid(error.to_string()))?;
            client
                .prepare_confirmed_upgrade(daemon, false)
                .map_err(|error| invalid(error.to_string()))?;
            wait_daemon_stopped(&socket)?;
        }
        set_phase(
            state,
            job,
            UpdatePhase::Installing,
            "Starting the installed application",
        )?;
        let mut command = Command::new(
            root.join("bin")
                .join(crate::companion::Product::Cli.file_name()),
        );
        command
            .args(["--json", "daemon", "start"])
            .stderr(Stdio::null());
        guard.inherit_for_install(&mut command)?;
        let timeout = crate::install_guard::daemon_start_timeout()?;
        startup_attempted = true;
        let started = wait_for_daemon_start(&mut command, timeout, state, job);
        startup_owned = started.as_ref().err().is_none_or(|failure| failure.spawned);
        // A post-spawn failure is never converted to success by readiness.
        // Its exact original command has settled, but its result remains error.
        started.map_err(|failure| failure.error)?;
        if crate::daemon::running_executable().map_err(|error| invalid(error.to_string()))?
            != Some(root.join("bin/cagentsd"))
        {
            return Err(invalid(
                "The terminal service does not belong to the confirmed installation; its saved panes and recovery state were retained. Check cagents daemon status before retrying",
            ));
        }
        Ok(())
    })();
    // Before startup, retain the previous matched executable on failure. Once
    // the successor owns the runtime or an actual startup command was spawned,
    // reopen that candidate's recovery UI instead of an incompatible old UI.
    let successor_owns_runtime = startup_attempted
        && !startup_owned
        && result.is_err()
        && crate::daemon::running_executable()
            .map_err(|error| invalid(error.to_string()))?
            .is_some_and(|executable| executable == root.join("bin/cagentsd"));
    let reopen = if result.is_ok() || successor_owns_runtime || startup_owned {
        root.to_owned()
    } else {
        installation
            .prefix
            .join("versions")
            .join(&installation.version)
    };
    let reopened = guard
        .relaunch_windows(&windows, &reopen.join("bin/cagents-ui"))
        .map_err(UpdateError::from);
    result?;
    reopened
}

const WAITING_FOR_STARTUP: &str = "Restoring sessions is taking longer. Saved windows will reopen after terminal startup settles.";

fn wait_for_daemon_start(
    command: &mut Command,
    timeout: Duration,
    state: &Path,
    job: &mut UpdateJob,
) -> Result<(), crate::install_guard::DaemonStartError> {
    crate::install_guard::wait_daemon_start_with_progress(command, timeout, || {
        let persisted = set_phase(state, job, UpdatePhase::Installing, WAITING_FOR_STARTUP)
            .map_err(|error| io::Error::other(error.to_string()));
        // The CLI worker has no global tracing subscriber. Its ordinary
        // captured stderr remains useful even if the status file cannot update.
        let logged = writeln!(io::stderr().lock(), "{WAITING_FOR_STARTUP}");
        persisted.and(logged)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn overdue_startup_reports_once_without_releasing_original_child_or_fences() {
        use std::os::unix::fs::PermissionsExt;
        const ROOT: &str = "CAGENTS_STARTUP_PROGRESS_FIXTURE_ROOT";
        const MODE: &str = "CAGENTS_STARTUP_PROGRESS_FIXTURE_MODE";
        fn dummy(root: &Path, label: &str) -> Command {
            let mut command = Command::new("/usr/bin/python3");
            command.args(["-u", "-c", "import os,pathlib,sys,time\nroot=pathlib.Path(sys.argv[1]); label=sys.argv[2]\nready=root/(label+'-ready.tmp'); ready.write_text(str(os.getpid())); ready.replace(root/(label+'-ready'))\ndeadline=time.monotonic()+10\nwhile not (root/(label+'-release')).exists():\n if time.monotonic()>=deadline: sys.exit(7)\n time.sleep(.005)\n(root/(label+'-finished')).write_text('natural exit')"]);
            command.arg(root).arg(label).current_dir(root);
            command
        }
        if let Some(root) = std::env::var_os(ROOT) {
            let root = PathBuf::from(root);
            let mode = std::env::var(MODE).unwrap();
            let state = root.join("state");
            let _job_lock = acquire_lock(&state).unwrap();
            let guard = crate::instance::UiUpdateGuard::acquire().unwrap();
            let runtime = crate::instance::runtime_directory().unwrap();
            let fences: Vec<_> = fs::read_dir(&runtime)
                .unwrap()
                .map(|row| row.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .ends_with(".update")
                })
                .collect();
            assert_eq!(fences.len(), 1);
            fs::write(root.join("fence-path"), fences[0].to_str().unwrap()).unwrap();
            let mut job = UpdateJob {
                id: uuid::Uuid::new_v4().to_string(),
                version: "0.1.3+local.fixture".into(),
                phase: UpdatePhase::Installing,
                message: "Starting the installed application".into(),
                log_path: root.join("update.log"),
                confirmation: None,
            };
            write_job(&state, &job).unwrap();
            if mode == "write-error" {
                fs::remove_file(state.join("status.json")).unwrap();
                fs::create_dir(state.join("status.json")).unwrap();
            }
            let mut command = dummy(&root, "startup");
            guard.inherit_for_install(&mut command).unwrap();
            let result = if mode == "panic" {
                struct PanicOnDrop;
                impl Drop for PanicOnDrop {
                    fn drop(&mut self) {
                        panic!("panic payload must not be dropped");
                    }
                }
                crate::install_guard::wait_daemon_start_with_progress(
                    &mut command,
                    Duration::from_millis(10),
                    || {
                        fs::write(root.join("reporter-entered"), b"once").unwrap();
                        std::panic::panic_any(PanicOnDrop);
                    },
                )
            } else {
                wait_for_daemon_start(&mut command, Duration::from_millis(10), &state, &mut job)
            };
            let error = result.unwrap_err();
            assert!(error.spawned);
            assert_eq!(error.error.kind(), io::ErrorKind::TimedOut);
            let pid: i32 = fs::read_to_string(root.join("startup-ready"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(
                !Path::new(&format!("/proc/{pid}")).exists(),
                "returned before original reap"
            );
            if mode == "write-error" {
                assert!(
                    error
                        .error
                        .to_string()
                        .contains("Could not record the waiting status")
                );
            }
            if mode == "panic" {
                assert!(
                    error
                        .error
                        .to_string()
                        .contains("startup progress reporter panicked")
                );
            }
            fs::write(root.join("after-wait"), error.error.to_string()).unwrap();
            drop(guard);
            return;
        }
        struct Original {
            child: std::process::Child,
            release: PathBuf,
        }
        impl Drop for Original {
            fn drop(&mut self) {
                let _ = fs::write(&self.release, b"release");
                let _ = self.child.wait();
            }
        }
        let selector = thread::current().name().unwrap().to_owned();
        for mode in ["durable", "write-error", "panic"] {
            let root = tempfile::tempdir().unwrap();
            for name in ["state", "runtime"] {
                let path = root.path().join(name);
                fs::create_dir(&path).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let mut neighbor = Original {
                child: dummy(root.path(), "neighbor").spawn().unwrap(),
                release: root.path().join("neighbor-release"),
            };
            let stdout = root.path().join("caller.stdout");
            let stderr = root.path().join("caller.stderr");
            let mut caller = Original {
                child: Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
                    .env(ROOT, root.path())
                    .env(MODE, mode)
                    .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
                    .stdin(Stdio::null())
                    .stdout(File::create(&stdout).unwrap())
                    .stderr(File::create(&stderr).unwrap())
                    .spawn()
                    .unwrap(),
                release: root.path().join("startup-release"),
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let report_ready = match mode {
                    "durable" => fs::read(root.path().join("state/status.json"))
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<UpdateJob>(&bytes).ok())
                        .is_some_and(|job| {
                            job.phase == UpdatePhase::Installing
                                && job.message == WAITING_FOR_STARTUP
                        }),
                    "write-error" => fs::read_dir(root.path().join("state")).unwrap().any(|row| {
                        row.unwrap()
                            .file_name()
                            .to_string_lossy()
                            .starts_with("status-")
                    }),
                    "panic" => root.path().join("reporter-entered").exists(),
                    _ => unreachable!(),
                };
                if report_ready
                    && root.path().join("startup-ready").exists()
                    && root.path().join("neighbor-ready").exists()
                {
                    break;
                }
                assert!(
                    caller.child.try_wait().unwrap().is_none(),
                    "reporter abandoned original custody in {mode}"
                );
                assert!(
                    Instant::now() < deadline,
                    "no before-release progress in {mode}"
                );
                thread::sleep(Duration::from_millis(5));
            }
            let pid: i32 = fs::read_to_string(root.path().join("startup-ready"))
                .unwrap()
                .parse()
                .unwrap();
            let process = crate::daemon::holder_registry::Process::read(pid).unwrap();
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            assert!(fd >= 0);
            let pin = unsafe { File::from_raw_fd(fd) };
            let mut ready = libc::pollfd {
                fd: pin.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut ready, 1, 0) }, 0);
            assert!(process.is_current() && caller.child.try_wait().unwrap().is_none());
            assert!(neighbor.child.try_wait().unwrap().is_none());
            assert!(!root.path().join("after-wait").exists());
            let fence = PathBuf::from(fs::read_to_string(root.path().join("fence-path")).unwrap());
            let inherited = fs::metadata(format!("/proc/{pid}/fd/197")).unwrap();
            let original = fs::metadata(&fence).unwrap();
            assert_eq!(
                (inherited.dev(), inherited.ino()),
                (original.dev(), original.ino())
            );
            for path in [&fence, &root.path().join("state/lock")] {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .unwrap();
                assert_eq!(
                    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    -1,
                    "fence released before original wait in {mode}"
                );
                assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
            }
            fs::write(&caller.release, b"natural exit").unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let status = loop {
                if let Some(status) = caller.child.try_wait().unwrap() {
                    break status;
                }
                assert!(
                    Instant::now() < deadline,
                    "original caller did not settle in {mode}"
                );
                thread::sleep(Duration::from_millis(5));
            };
            let bytes = fs::read(&stdout).unwrap();
            let errors = fs::read(&stderr).unwrap();
            assert!(bytes.len() + errors.len() < 256 * 1024);
            assert!(
                status.success(),
                "{mode}: {}\n{}",
                String::from_utf8_lossy(&bytes),
                String::from_utf8_lossy(&errors)
            );
            assert!(String::from_utf8_lossy(&bytes).contains("1 passed; 0 failed"));
            if mode != "panic" {
                assert_eq!(
                    String::from_utf8_lossy(&errors)
                        .matches(WAITING_FOR_STARTUP)
                        .count(),
                    1
                );
            }
            assert!(root.path().join("after-wait").exists());
            assert_eq!(
                fs::read_to_string(root.path().join("startup-finished")).unwrap(),
                "natural exit"
            );
            assert!(!process.is_current());
            assert_eq!(unsafe { libc::poll(&mut ready, 1, 0) }, 1);
            assert!(
                neighbor.child.try_wait().unwrap().is_none(),
                "unrelated original was reaped or stopped"
            );
            for path in [&fence, &root.path().join("state/lock")] {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .unwrap();
                assert_eq!(
                    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0
                );
            }
            fs::write(&neighbor.release, b"natural exit").unwrap();
            assert!(neighbor.child.wait().unwrap().success());
            assert_eq!(
                fs::read_to_string(root.path().join("neighbor-finished")).unwrap(),
                "natural exit"
            );
        }
    }

    #[test]
    fn replacing_one_binary_invalidates_the_confirmed_matched_set() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("0.1.3+local.same-name");
        fs::create_dir_all(root.join("bin")).unwrap();
        for binary in BINARIES {
            fs::write(root.join("bin").join(binary), b"first build").unwrap();
        }
        let original_stamp = stamp(&root).unwrap();
        let original_identity = digest(&root).unwrap();
        let replacement = root.join("bin/replacement");
        fs::write(&replacement, b"other build").unwrap();
        fs::rename(replacement, root.join("bin/cagentsd")).unwrap();
        assert!(stamp(&root).unwrap() != original_stamp);
        assert_ne!(digest(&root).unwrap(), original_identity);
        // A different target behind an unchanged version name cannot borrow
        // the previous confirmation through a symlink either.
        fs::remove_file(root.join("bin/cagentsd")).unwrap();
        std::os::unix::fs::symlink("cagents", root.join("bin/cagentsd")).unwrap();
        assert!(stamp(&root).is_err());
    }
}
