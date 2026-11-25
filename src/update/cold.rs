//! Updates on macOS, where the daemon cannot yet hand its sessions to another
//! version: an update stops an idle daemon and starts the new build's.
//!
//! The review names every hosted session as one to finish or close first, so
//! nothing running ends with the old daemon. `install-macos.sh` switches
//! `current` and rewrites the login item, which starts the new daemon at each
//! login; after the windows reopen, they or this worker start it now. A build
//! installed by hand already serves, and usually its daemon runs: then only
//! the windows restart.
//!
//! Every platform compiles this module, so its types stay checked where the
//! update suites run; only a macOS worker takes it.
use super::*;

/// A job's verified release files, as the installer reads them by path.
const STAGING_SUFFIX: &str = ".release";

pub(super) fn perform_release(
    installation: &Installation,
    state: &Path,
    job: &mut UpdateJob,
    feed: &feed::Feed,
    release: &Release,
    terminal: bool,
    cancellation: &CancellationToken,
) -> Result<Completion, UpdateError> {
    set_phase(
        state,
        job,
        UpdatePhase::Downloading,
        "Downloading the release",
    )?;
    let (checksums, installer, archive) = download_release(feed, release, cancellation)?;
    set_phase(
        state,
        job,
        UpdatePhase::Verifying,
        "Verifying the downloaded release",
    )?;
    let staging = Staging::create(state, &job.id)?;
    let files = ReleaseFiles {
        installer: staging.write(&release.installer.name, installer)?,
        archive: staging.write(&release.archive.name, archive)?,
        checksums: staging.write(&release.checksums.name, checksums)?,
    };
    // Run the candidate before anything stops: it must start on this Mac and
    // name the protocol the review shows.
    let cli = staging.extract_cli(&files.archive, &release.version, cancellation)?;
    let candidate = impact::Candidate {
        version: release.version.clone(),
        protocol: protocol(&cli, cancellation)?,
        digest: release
            .archive
            .sha256
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    };
    let plan = Plan {
        candidate,
        keeps_daemon: false,
        unchanged: &|| Ok(()),
    };
    restart(installation, state, job, &plan, terminal, || {
        run_installer(installation, release, &files)
    })
}

pub(super) fn perform_installed(
    installation: &Installation,
    state: &Path,
    job: &mut UpdateJob,
    root: &Path,
    terminal: bool,
) -> Result<Completion, UpdateError> {
    let layout = locate(&std::env::current_exe()?)?;
    if served_version_root(&layout)? != root {
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
    let before = installed::stamp(root)?;
    let cancellation = CancellationToken::for_job(state, &job.id)?;
    let candidate = impact::Candidate {
        version: job.version.clone(),
        protocol: protocol(&root.join("bin/cagents"), &cancellation)?,
        digest: installed::digest(root)?,
    };
    // The installer refuses over a daemon of another version and starts its
    // own, so the daemon usually serves this build already: then only the
    // windows, still running the previous one, restart.
    let runs_root = || -> Result<bool, UpdateError> {
        Ok(installed_daemon_owner(&layout)?.is_some_and(|owner| owner.version_root == root))
    };
    let keeps_daemon = runs_root()?;
    let unchanged = || -> Result<(), UpdateError> {
        if served_version_root(&layout)? != root || installed::stamp(root)? != before {
            return Err(invalid(
                "the installed application changed; review the new build before updating",
            ));
        }
        if runs_root()? != keeps_daemon {
            return Err(invalid(
                "the daemon changed during the review; review the update again",
            ));
        }
        Ok(())
    };
    let plan = Plan {
        candidate,
        keeps_daemon,
        unchanged: &unchanged,
    };
    restart(installation, state, job, &plan, terminal, || Ok(()))
}

/// What an update restarts, as its review describes it.
struct Plan<'a> {
    candidate: impact::Candidate,
    /// The daemon already runs the candidate, so it keeps running.
    keeps_daemon: bool,
    /// Fails once the build the review describes has changed.
    unchanged: &'a dyn Fn() -> Result<(), UpdateError>,
}

/// Review, close the windows, stop the idle daemon unless it already runs
/// the candidate, switch the installation with `switch`, reopen the windows,
/// and start the daemon that was serving when no window started it.
fn restart(
    installation: &Installation,
    state: &Path,
    job: &mut UpdateJob,
    plan: &Plan,
    terminal: bool,
    switch: impl FnOnce() -> Result<(), UpdateError>,
) -> Result<Completion, UpdateError> {
    let context = Context::capture(installation)?;
    let confirmed = confirmation::wait(state, job, terminal, |policy| {
        (plan.unchanged)()?;
        observe(&context, plan, policy)
    })?;
    // A terminal may confirm a review that lists sessions; this update never
    // stops them, so it refuses them here.
    if let Some(refused) = confirmation::refused(&confirmed, false) {
        return Err(refused);
    }
    let serving = plan.keeps_daemon || confirmed.daemon.is_some();
    set_phase(
        state,
        job,
        UpdatePhase::ClosingWindows,
        "Saving and closing all application windows",
    )?;
    let mut guard = crate::instance::UiUpdateGuard::acquire()?;
    let windows = guard.prepare_windows(Duration::from_secs(45))?;
    // From here the update no longer accepts a cancellation.
    confirmation::prepared(state, job)?;
    guard.close_prepared_windows(&windows, Duration::from_secs(45))?;
    set_phase(
        state,
        job,
        UpdatePhase::Installing,
        "Stopping the daemon and switching the installation",
    )?;
    let mut stopped = false;
    let mut switched = (|| {
        if confirmed.daemon.is_some() && !plan.keeps_daemon {
            stop_idle_daemon()?;
            stopped = true;
        }
        (plan.unchanged)()?;
        switch()
    })();
    if let Err(error) = &switched
        && stopped
    {
        switched = Err(invalid(format!(
            "{error} The daemon was stopped and starts again when cagents opens."
        )));
    }
    // The windows reopen from the build of the daemon still running, which a
    // failed stop leaves; otherwise from the build `current` serves, the new
    // one when the switch finished, whose daemon they start. A window started
    // through `current` would look for its companions there rather than in
    // its own version.
    let current = installation.prefix.join("current");
    let served = current.canonicalize().unwrap_or_else(|error| {
        eprintln!("Could not resolve the served build: {error}");
        current
    });
    let reopen = match locate(&std::env::current_exe()?)
        .and_then(|layout| installed_daemon_owner(&layout))
    {
        Ok(Some(owner)) => owner.version_root,
        Ok(None) => served.clone(),
        Err(error) => {
            eprintln!("Could not identify the running daemon: {error}");
            served.clone()
        }
    };
    // Releases the launch fence, which refuses every other daemon start
    // until then, the installer's through launchd included.
    let reopened = guard
        .relaunch_closed_windows(&reopen.join("bin/cagents-ui"))
        .map_err(UpdateError::from);
    // With no window to start it, start the daemon that was serving.
    if switched.is_ok() && serving && !daemon_running() {
        start_daemon(&served);
    }
    let handoff = if serving {
        Handoff::Served
    } else {
        Handoff::NoDaemon
    };
    finish_reopening_windows(state, job, switched, handoff, reopened)
}

/// What the review reports beside the daemon's impact, read once.
struct Context {
    from_version: String,
    windows: usize,
}

impl Context {
    fn capture(installation: &Installation) -> Result<Self, UpdateError> {
        let windows = crate::instance::UiUpdateGuard::acquire()?.running_window_count()?;
        let layout = locate(&std::env::current_exe()?)?;
        let from_version = installed_daemon_owner(&layout)?
            .map_or_else(|| installation.version.clone(), |owner| owner.version);
        Ok(Self {
            from_version,
            windows,
        })
    }
}

/// The review of an update that restarts the daemon: every session it hosts
/// must be finished or closed first.
fn observe(
    context: &Context,
    plan: &Plan,
    policy: impact::Policy,
) -> Result<impact::Preview, UpdateError> {
    let daemon = if plan.keeps_daemon {
        None
    } else {
        let socket = crate::terminal_ipc::daemon_socket_path()?;
        match source::Client::connect(&socket) {
            Ok(mut client) => Some(finish_first(client.upgrade_impact(policy)?)),
            Err(crate::control::ControlError::Connect(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                let paths = crate::daemon::RuntimePaths::discover()
                    .map_err(|error| invalid(error.to_string()))?;
                if crate::daemon::HostLease::is_held(&paths)
                    .map_err(|error| invalid(error.to_string()))?
                {
                    return Err(invalid(
                        "The terminal service is starting or stopping; retry the update preview shortly",
                    ));
                }
                None
            }
            Err(error) => return Err(invalid(error.to_string())),
        }
    };
    Ok(impact::Preview {
        from_version: context.from_version.clone(),
        windows: context.windows,
        stopped: Vec::new(),
        stopped_windows: Vec::new(),
        labels: Default::default(),
        editors: crate::install_guard::editor_companions(),
        candidate: plan.candidate.clone(),
        replaces_daemon: daemon.is_some(),
        daemon,
    })
}

/// Marks every session the daemon reports as one the update waits on.
fn finish_first(mut impact: impact::DaemonImpact) -> impact::DaemonImpact {
    for session in &mut impact.sessions {
        session.disposition = impact::Disposition::Blocked;
        session.reason = impact::SessionCause::DaemonRestarts;
        session.interrupted_subagents.clear();
    }
    impact
}

/// Whether a daemon runs now. One that cannot be read counts as running, so
/// nothing starts a second.
fn daemon_running() -> bool {
    crate::daemon::running_executable().map_or_else(
        |error| {
            eprintln!("Could not read the daemon's state: {error}");
            true
        },
        |executable| executable.is_some(),
    )
}

/// Stops the daemon through its own CLI, which refuses while it hosts any
/// session, and waits for it to release its lease.
fn stop_idle_daemon() -> Result<(), UpdateError> {
    let Some(owner) = installed_daemon_owner(&locate(&std::env::current_exe()?)?)? else {
        return Ok(());
    };
    let mut command = Command::new(owner.version_root.join("bin/cagents"));
    command.args(["--json", "daemon", "stop", "--require-empty"]);
    crate::install_guard::sanitize_child_environment(&mut command);
    unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
    // It exits unsuccessfully when it refuses, as when a session started
    // after the review.
    let busy = || {
        invalid(
            "A session started in the daemon during the update, so the daemon kept running and the installation was not changed. Finish or close it, then update again.",
        )
    };
    let output = bounded_command(
        &mut command,
        "Stopping the idle daemon",
        SMALL_ASSET_LIMIT,
        Duration::from_secs(30),
        &CancellationToken::new(),
    )
    .map_err(|error| match error {
        UpdateError::Exited { .. } => {
            eprintln!("The idle daemon refused to stop: {error}");
            busy()
        }
        error => {
            eprintln!("The idle daemon did not stop: {error}");
            invalid(handoff::SERVICE_NOT_STOPPED)
        }
    })?;
    let reply: serde_json::Value = serde_json::from_reader(output)?;
    if !handoff::confirms_empty_stop(&reply) {
        eprintln!("The daemon did not confirm an empty shutdown: {reply}");
        return Err(busy());
    }
    let paths =
        crate::daemon::RuntimePaths::discover().map_err(|error| invalid(error.to_string()))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while crate::daemon::HostLease::is_held(&paths).map_err(|error| invalid(error.to_string()))? {
        if Instant::now() >= deadline {
            return Err(invalid(handoff::SERVICE_NOT_STOPPED));
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// Starts the daemon of the build at `root`, as its login item does at the
/// next login.
fn start_daemon(root: &Path) {
    let mut command = Command::new(root.join("bin/cagents"));
    command.args(["daemon", "start"]);
    crate::install_guard::sanitize_child_environment(&mut command);
    unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
    if let Err(error) = bounded_command(
        &mut command,
        "Starting the daemon",
        SMALL_ASSET_LIMIT,
        Duration::from_secs(30),
        &CancellationToken::new(),
    ) {
        eprintln!("Could not start the daemon: {error}");
    }
}

struct ReleaseFiles {
    installer: PathBuf,
    archive: PathBuf,
    checksums: PathBuf,
}

/// `install-macos.sh` from the verified files. It checks the archive against
/// the checksum file again, refuses over a running daemon of another version,
/// and rewrites the login item for the next login. It does not load the item:
/// the update's launch fence would refuse the daemon start it runs.
fn run_installer(
    installation: &Installation,
    release: &Release,
    files: &ReleaseFiles,
) -> Result<(), UpdateError> {
    let mut command = Command::new("/bin/sh");
    command
        .arg(&files.installer)
        .args(["--version", &release.version, "--prefix"])
        .arg(&installation.prefix)
        .env("CAGENTS_ARCHIVE", &files.archive)
        .env("CAGENTS_CHECKSUMS", &files.checksums)
        .env("CAGENTS_REPOSITORY", REPOSITORY)
        .env_remove("ENV")
        .env_remove("BASH_ENV")
        .stdin(Stdio::null());
    command.arg("--no-start");
    unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
    // Its output goes to the job's log. Do not kill it mid-transaction: it
    // bounds its own waits and switches `current` in one rename.
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(invalid(format!(
            "installer failed ({status}); inspect the update log before retrying"
        )))
    }
}

/// The daemon protocol the CLI at `executable` speaks.
fn protocol(executable: &Path, cancellation: &CancellationToken) -> Result<u16, UpdateError> {
    let mut command = Command::new(executable);
    command.args(["--json", "daemon", "protocol-version"]);
    crate::install_guard::sanitize_child_environment(&mut command);
    unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
    let output = bounded_command(
        &mut command,
        "candidate protocol probe",
        SMALL_ASSET_LIMIT,
        Duration::from_secs(10),
        cancellation,
    )?;
    let envelope: serde_json::Value = serde_json::from_reader(output)?;
    envelope
        .get("data")
        .and_then(|data| data.get("protocol_version"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid("candidate did not report a valid daemon protocol"))
}

/// A private directory of one job's verified release files, removed with it.
struct Staging {
    path: PathBuf,
}

impl Staging {
    /// Removes what earlier jobs left, which only a worker that did not
    /// finish can: this worker holds the update lease.
    fn create(state: &Path, job: &str) -> Result<Self, UpdateError> {
        for entry in fs::read_dir(state)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .ends_with(STAGING_SUFFIX)
                && entry.file_type()?.is_dir()
            {
                fs::remove_dir_all(entry.path())?;
            }
        }
        let path = state.join(format!("{job}{STAGING_SUFFIX}"));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self { path })
    }

    /// Copies a verified download to `name`, which the signed metadata
    /// named after its fixed pattern.
    fn write(&self, name: &str, mut file: File) -> Result<PathBuf, UpdateError> {
        if Path::new(name).file_name() != Some(name.as_ref()) {
            return Err(invalid("release metadata names an unexpected file"));
        }
        let path = self.path.join(name);
        let mut copy = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        file.seek(SeekFrom::Start(0))?;
        io::copy(&mut file, &mut copy)?;
        copy.sync_all()?;
        Ok(path)
    }

    /// Unpacks only the archive's CLI, to probe it before the installer runs.
    fn extract_cli(
        &self,
        archive: &Path,
        version: &str,
        cancellation: &CancellationToken,
    ) -> Result<PathBuf, UpdateError> {
        let target = self.path.join("candidate");
        fs::DirBuilder::new().mode(0o700).create(&target)?;
        let member = format!("cagents-{version}/bin/cagents");
        let mut extract = Command::new("tar");
        extract
            .args(["-xzf"])
            .arg(archive)
            .arg("-C")
            .arg(&target)
            .arg("--")
            .arg(&member);
        bounded_command(
            &mut extract,
            "candidate extraction",
            SMALL_ASSET_LIMIT,
            Duration::from_secs(120),
            cancellation,
        )?;
        let cli = target.join(member);
        let metadata = fs::symlink_metadata(&cli)?;
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            return Err(invalid("the release archive holds no runnable cagents"));
        }
        Ok(cli)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            eprintln!("Could not remove the update's release files: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TerminalId;

    fn session(disposition: impact::Disposition) -> impact::SessionImpact {
        impact::SessionImpact {
            id: TerminalId::new(),
            kind: impact::SessionKind::Terminal,
            title: "build".into(),
            identity: "identity".into(),
            disposition,
            reason: impact::SessionCause::ProviderEnded,
            interrupted_subagents: vec!["task".into()],
        }
    }

    #[test]
    fn a_restarting_daemon_waits_on_every_session_it_hosts() {
        let impact = finish_first(impact::DaemonImpact {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof: crate::model::HostSessionProof {
                    host_session_id: crate::model::HostSessionId::new("cold-update"),
                    boot_id: crate::model::BootId::new("cold-update"),
                    host_incarnation: crate::model::HostIncarnation::new("cold-update"),
                },
                token: "review".into(),
                inventory_generation: 1,
                live_terminals: 2,
                background_subagents: 0,
                background_identity: String::new(),
                expires_at_unix_ms: i64::MAX,
            },
            policy: impact::Policy::WhenIdle,
            sessions: vec![
                session(impact::Disposition::Continues),
                session(impact::Disposition::Resumes),
            ],
            blockers: Vec::new(),
        });
        assert!(impact.blocked());
        for session in &impact.sessions {
            assert_eq!(session.disposition, impact::Disposition::Blocked);
            assert_eq!(session.reason, impact::SessionCause::DaemonRestarts);
            assert!(session.interrupted_subagents.is_empty());
            assert!(session.review().reason.contains("restart the daemon"));
        }
    }

    #[test]
    fn the_restart_cause_survives_the_review_record() {
        let cause = impact::SessionCause::DaemonRestarts;
        let text = serde_json::to_string(&cause).unwrap();
        assert_eq!(
            serde_json::from_str::<impact::SessionCause>(&text).unwrap(),
            cause
        );
    }

    #[test]
    fn only_an_empty_or_finished_stop_lets_the_update_continue() {
        use serde_json::json;
        let reply = |data| json!({ "ok": true, "command": "daemon.stop", "data": data });
        assert!(handoff::confirms_empty_stop(&reply(
            json!({ "outcome": "accepted", "live_terminals_before": 0 })
        )));
        assert!(handoff::confirms_empty_stop(&reply(
            json!({ "state": "already_stopped" })
        )));
        assert!(!handoff::confirms_empty_stop(&reply(
            json!({ "outcome": "accepted", "live_terminals_before": 1 })
        )));
        assert!(!handoff::confirms_empty_stop(&json!({
            "ok": false, "command": "daemon.stop", "data": { "state": "already_stopped" }
        })));
    }

    #[test]
    fn staging_holds_only_named_files_and_leaves_nothing_behind() {
        let state = tempfile::tempdir().unwrap();
        fs::create_dir(state.path().join("stale.release")).unwrap();
        fs::write(state.path().join("status.json"), b"{}").unwrap();
        let path = {
            let staging = Staging::create(state.path(), "job").unwrap();
            assert!(!state.path().join("stale.release").exists());
            assert!(state.path().join("status.json").exists());
            let mut source = tempfile::tempfile().unwrap();
            source.write_all(b"bytes").unwrap();
            let written = staging.write("install-macos.sh", source).unwrap();
            assert_eq!(fs::read(&written).unwrap(), b"bytes");
            assert_eq!(fs::metadata(&written).unwrap().mode() & 0o777, 0o600);
            assert!(
                staging
                    .write("../escape", tempfile::tempfile().unwrap())
                    .is_err()
            );
            staging.path.clone()
        };
        assert!(!path.exists());
    }
}
