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
    handoff: &mut Option<crate::daemon::lease_handoff::LeaseHandoff>,
    survivors: &mut floor::Survivors,
    terminal: bool,
) -> Result<(), UpdateError> {
    let layout = locate(&std::env::current_exe()?)?;
    let (source_protocol, _) = source::inspect_identity(None)?;
    // Retain the exact installation boundary even for an older source.
    let source_owner = installed_daemon_owner(&layout)?;
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
    let cancellation = CancellationToken::for_job(state, &job.id)?;
    let staged =
        candidate::Staged::from_file(executable, job.version.clone(), identity, &cancellation)?;
    let mut turns = source::TurnReview::default();
    let mut observe = |policy| {
        if served_version_root(&layout)? != root || stamp(root)? != before {
            return Err(invalid(
                "the installed application changed; review the new build before updating",
            ));
        }
        turns.apply(observe_installed_source(&layout, root, &staged, policy)?)
    };
    if staged.identity.protocol == source_protocol {
        staged.verify_compatible_handshake(&cancellation)?;
    }
    // A different installed daemon is replaced even at the same protocol.
    // Inspect with the same intent used by the reviewed impact and preflight.
    let replaces_compatible_daemon = source_owner
        .as_ref()
        .is_some_and(|owner| owner.version_root != root);
    let inspection = reconcile_saved_sources(staged.identity.protocol, replaces_compatible_daemon)?;
    let context = ReviewContext::capture(installation)?;
    let confirmed = confirmation::wait(state, job, |policy| {
        Ok(context.apply(observe(policy)?.with_inspection(&inspection)))
    })?;
    let mut recovery = recovery::Context::default();
    recovery.capture(state, job)?;
    set_phase(
        state,
        job,
        UpdatePhase::ClosingWindows,
        "Saving and closing application windows",
    )?;
    let mut guard = crate::instance::UiUpdateGuard::acquire()?;
    confirmation::check_cancelled(state, job)?;
    let windows = guard.prepare_windows(Duration::from_secs(45))?;
    {
        let inspected = crate::install_guard::preflight_source_panes(
            &guard,
            confirmed
                .daemon
                .as_ref()
                .map(|daemon| daemon.capability.proof.clone()),
            confirmed.replaces_daemon,
        )?;
        let latest = observe(
            confirmed
                .daemon
                .as_ref()
                .map_or(impact::Policy::WhenIdle, |daemon| daemon.policy),
        )
        .map_err(io::Error::other)?
        .with_inspection(&inspected);
        if !confirmed.preserves_consent(&latest)
            || digest(root).map_err(io::Error::other)? != staged.identity.digest
        {
            return Err(invalid(
                "The installed build or session ownership changed during pane inspection; existing windows were kept open",
            ));
        }
    }
    confirmation::check_cancelled(state, job)?;
    let output = StartupOutput::create(state)?;
    let mut startup_attempted = false;
    let mut startup_owned = false;
    let mut previous = None;
    let mut original_schema = None;
    let mut result = (|| {
        if let Some(daemon) = &confirmed.daemon {
            let paths = crate::daemon::RuntimePaths::discover()
                .map_err(|error| invalid(error.to_string()))?;
            let source =
                crate::daemon::lease_handoff::HandoffSource::pin(&paths, &daemon.capability.proof)
                    .map_err(|error| invalid(error.to_string()))?;
            let executable = fs::read_link(format!("/proc/{}/exe", source.pid))?;
            let owner = locate(&executable)?;
            if owner.prefix != installation.prefix
                || source.exited()?
                || recovery.census.as_ref().and_then(|c| c.source.as_ref())
                    != Some(&daemon.capability.proof)
            {
                return Err(invalid(
                    "The census belongs to a different source; shutdown was not requested",
                ));
            }
            previous = (owner.version_root != root).then_some(owner.version_root);
        }
        recovery.shutdown_requested = confirmed.replaces_daemon;
        floor::quiesce(
            state,
            job,
            &confirmed,
            terminal,
            |policy| Ok(context.apply(observe(policy)?.with_inspection(&inspection))),
            handoff,
            survivors,
            &guard,
        )?;
        if previous.is_some()
            && let Some(lease) = handoff.as_ref()
        {
            original_schema = Some(stopped_store_schema(root, &guard, lease)?);
        }
        survivors.authorize_successor(&root.join("bin/cagentsd"), None, handoff.as_ref())?;
        confirmation::prepared(state, job)?;
        guard.close_prepared_windows(&windows, Duration::from_secs(45))?;
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
        command.args(["--json", "daemon", "start"]);
        crate::install_guard::sanitize_child_environment(&mut command);
        unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
        output.attach(&mut command)?;
        guard.inherit_for_install(&mut command)?;
        if let Some(handoff) = handoff.as_ref() {
            handoff.inherit(&mut command)?;
        }
        survivors.inherit_recovery(&mut command)?;
        let timeout = crate::install_guard::daemon_start_timeout()?;
        startup_attempted = true;
        let started = wait_for_daemon_start(&mut command, timeout, state, job);
        startup_owned = started.as_ref().err().is_none_or(|failure| failure.spawned);
        // The worker's stderr is this job's log.
        let reason = output.record(&mut io::stderr().lock());
        // A post-spawn failure is never converted to success by readiness.
        // Its exact original command has settled, but its result remains error.
        if let Err(failure) = started {
            let successor_runs = crate::daemon::running_executable()
                .is_ok_and(|executable| executable == Some(root.join("bin/cagentsd")));
            return Err(startup_failure(
                &failure.error,
                reason.as_deref(),
                successor_runs,
            ));
        }
        // Wait through bounded restoration until the exact successor is
        // Ready, then prove this job's recovery inventory against it.
        confirm_daemon_ready(&root.join("bin/cagentsd"), timeout)?;
        survivors.verify_successor(&root.join("bin/cagentsd"))
    })();
    let mut restored_previous = false;
    if result.is_err()
        && recovery.shutdown_requested
        && let Some(previous) = previous.as_ref()
    {
        match rollback_stopped_candidate(
            &installation.prefix,
            root,
            previous,
            original_schema,
            startup_owned,
            &guard,
            handoff,
        ) {
            Ok(restored) => restored_previous = restored,
            Err(error) => {
                eprintln!("Installed candidate rollback failed: {error}");
                result = Err(invalid(format!(
                    "{} Recovery could not restore the previous installation: {error}",
                    result.unwrap_err()
                )));
            }
        }
    }
    // A failed command can have left a serving successor. Keep that writer's
    // matched UI; a proven-dead candidate instead restores the old matched set.
    let successor_owns_runtime = startup_attempted
        && !startup_owned
        && result.is_err()
        && crate::daemon::running_executable()
            .is_ok_and(|executable| executable == Some(root.join("bin/cagentsd")));
    let reopen = if restored_previous {
        previous.as_ref().unwrap().clone()
    } else if result.is_ok() || successor_owns_runtime || startup_owned {
        root.to_owned()
    } else {
        previous.unwrap_or_else(|| root.to_owned())
    };
    let recovery_result = finish_handoff(
        installation,
        state,
        job,
        &result,
        handoff,
        &recovery,
        survivors,
        &guard,
    );
    recovery_result?;
    let reopened = guard
        .relaunch_closed_windows(&reopen.join("bin/cagents-ui"))
        .map_err(UpdateError::from);
    verify_reopened_service(&recovery, state, job)?;
    if let Err(error) = reopened {
        return report_recovery_failure(
            state,
            job,
            invalid(format!(
                "The daemon is ready and its original inventory was verified, but some saved windows did not reopen: {error}. Run `cagents ui` to reopen the saved workspace."
            )),
        );
    }
    result
}

/// Store maintenance receives the reservation intentionally; status probes
/// never do. This keeps conversion and the following startup in one handoff.
fn candidate_maintenance(
    root: &Path,
    arguments: &[&str],
    guard: &crate::instance::UiUpdateGuard,
    handoff: &crate::daemon::lease_handoff::LeaseHandoff,
) -> Result<File, UpdateError> {
    let mut command = Command::new(root.join("bin/cagents"));
    command.args(arguments);
    crate::install_guard::sanitize_child_environment(&mut command);
    unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
    guard.inherit_for_install(&mut command)?;
    handoff.inherit(&mut command)?;
    bounded_command(
        &mut command,
        "Preparing terminal service recovery",
        SMALL_ASSET_LIMIT,
        Duration::from_secs(30),
        &CancellationToken::new(),
    )
}

fn stopped_store_schema(
    root: &Path,
    guard: &crate::instance::UiUpdateGuard,
    handoff: &crate::daemon::lease_handoff::LeaseHandoff,
) -> Result<u32, UpdateError> {
    let mut output = candidate_maintenance(root, &["daemon", "store-schema"], guard, handoff)?;
    let mut schema = String::new();
    output.read_to_string(&mut schema)?;
    let schema = schema.trim();
    // Match installer rollback when the source had no shared-model store.
    if schema.is_empty() {
        Ok(22)
    } else {
        schema
            .parse()
            .map_err(|_| invalid("The stopped terminal store schema is unreadable"))
    }
}

fn rollback_stopped_candidate(
    prefix: &Path,
    candidate: &Path,
    previous: &Path,
    schema: Option<u32>,
    startup_spawned: bool,
    guard: &crate::instance::UiUpdateGuard,
    handoff: &mut Option<crate::daemon::lease_handoff::LeaseHandoff>,
) -> Result<bool, UpdateError> {
    selected_candidate(prefix, candidate)?;
    let paths =
        crate::daemon::RuntimePaths::discover().map_err(|error| invalid(error.to_string()))?;
    if crate::daemon::daemon_process_alive_at(&paths).map_err(|error| invalid(error.to_string()))? {
        return Ok(false);
    }
    if handoff.is_none() {
        *handoff = crate::daemon::lease_handoff::LeaseHandoff::try_acquire(&paths)
            .map_err(|error| invalid(error.to_string()))?;
    }
    let Some(lease) = handoff.as_ref() else {
        return Ok(false);
    };
    if crate::daemon::daemon_process_alive_at(&paths).map_err(|error| invalid(error.to_string()))? {
        return Ok(false);
    }
    if recovery::checkpoint_status()?
        .is_some_and(|s| s.phase != crate::upgrade::handoff::Phase::Ready)
    {
        return Err(invalid(
            "The checkpoint requires capable-target recovery; the candidate and current pointer were retained. No previous executable was restarted.",
        ));
    }
    if startup_spawned {
        let schema = schema.ok_or_else(|| invalid("The previous store schema was not captured"))?;
        candidate_maintenance(
            candidate,
            &["daemon", "downgrade-store", &schema.to_string()],
            guard,
            lease,
        )?;
    }
    restore_current(prefix, candidate, previous)?;
    Ok(true)
}

fn selected_candidate(prefix: &Path, candidate: &Path) -> Result<(), UpdateError> {
    let current = prefix.join("current");
    if current.canonicalize()? != candidate
        || !fs::symlink_metadata(&current)?.file_type().is_symlink()
    {
        return Err(invalid("The selected installation changed during recovery"));
    }
    Ok(())
}

fn restore_current(prefix: &Path, candidate: &Path, previous: &Path) -> Result<(), UpdateError> {
    selected_candidate(prefix, candidate)?;
    let current = prefix.join("current");
    // Both roots came from the authenticated installation layout. Keep a new
    // relative link beside current, then replace it atomically under the fence.
    let previous = previous
        .strip_prefix(prefix)
        .map_err(|_| invalid("The previous version belongs to another installation"))?;
    let temporary = prefix.join(format!(".rollback-{}", uuid::Uuid::new_v4()));
    std::os::unix::fs::symlink(previous, &temporary)?;
    let replaced = fs::rename(&temporary, &current);
    if replaced.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    replaced?;
    Ok(())
}

/// Listening is not proof that startup restored the reviewed panes. Recovery
/// deliberately serves inspection while ordinary conversation requests fail.
fn confirm_daemon_ready(expected: &Path, timeout: Duration) -> Result<(), UpdateError> {
    let socket = crate::daemon::RuntimePaths::discover()
        .map_err(|error| invalid(error.to_string()))?
        .socket;
    confirm_daemon_ready_at(&socket, expected, Instant::now() + timeout)
}

fn confirm_daemon_ready_at(
    socket: &Path,
    expected: &Path,
    deadline: Instant,
) -> Result<(), UpdateError> {
    use crate::{control::ControlClient, terminal_ipc::StartupState};
    let guidance =
        "Existing work and recovery records were kept. Check cagents daemon status before retrying";
    let communication = |error: &dyn std::fmt::Display| {
        invalid(format!(
            "Could not confirm restored panes: {error}. {guidance}"
        ))
    };
    let stream = crate::control::connect_control_socket(
        socket,
        deadline.saturating_duration_since(Instant::now()),
    )
    .map_err(|error| communication(&error))?;
    let peer = source::Peer::observe(&stream, socket).map_err(|error| communication(&error))?;
    let mut client = ControlClient::from_stream_with_deadline(stream, deadline)
        .map_err(|error| communication(&error))?;
    let ownership = |proof: &crate::model::HostSessionProof| {
        let unavailable = || invalid("the confirmed terminal service changed or is unavailable");
        peer.verify(proof).map_err(|_| unavailable())?;
        if peer.executable().map_err(|_| unavailable())? != expected {
            return Err(invalid("the confirmed terminal service changed"));
        }
        Ok(())
    };
    loop {
        if Instant::now() >= deadline {
            return Err(invalid(format!(
                "Timed out confirming restored panes. {guidance}"
            )));
        }
        ownership(client.proof()).map_err(|error: UpdateError| communication(&error))?;
        let state = client
            .startup_state_with_deadline(deadline)
            .map_err(|error| communication(&error))?;
        ownership(client.proof()).map_err(|error: UpdateError| communication(&error))?;
        match state {
            StartupState::Ready => return Ok(()),
            StartupState::Retained { detail } => {
                return Err(invalid(format!(
                    "The update could not restore all panes: {detail}. {guidance}"
                )));
            }
            StartupState::Restoring { .. } => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(invalid(format!(
                        "The terminal service is still restoring saved panes. {guidance}"
                    )));
                }
                thread::sleep(remaining.min(Duration::from_millis(100)));
            }
        }
    }
}

/// A compatible protocol is not proof that the daemon already runs the
/// selected executable. Replacing an older installed build still needs its
/// exact impact, consent and ordinary checkpoint; never use the candidate's
/// compatible-handshake fallback to turn a failed inspection into UI-only.
fn observe_installed_source(
    layout: &InstallLayout,
    root: &Path,
    staged: &candidate::Staged,
    policy: impact::Policy,
) -> Result<impact::Preview, UpdateError> {
    let source = source::inspect_installed_identity()?;
    let owner = installed_daemon_owner(layout)?;
    let executable = owner
        .as_ref()
        .map(|owner| owner.version_root.join("bin/cagentsd"));
    source.verify_owner(executable.as_deref())?;
    let older_compatible = source.proof.is_some()
        && source.protocol == staged.identity.protocol
        && owner
            .as_ref()
            .is_some_and(|owner| owner.version_root != root);
    if !older_compatible {
        return staged.preview(policy);
    }
    let socket = crate::terminal_ipc::daemon_socket_path()?;
    let mut client =
        source::Client::connect(&socket).map_err(|error| invalid(error.to_string()))?;
    if Some(client.proof()) != source.proof.as_ref() || client.protocol() != source.protocol {
        return Err(invalid(
            "The installed source changed before update inspection",
        ));
    }
    let daemon = client.upgrade_impact(policy)?;
    if Some(&daemon.capability.proof) != source.proof.as_ref() {
        return Err(invalid(
            "Update impact belongs to a different installed source",
        ));
    }
    // Inspection may take longer than entry routing's short observation TTL.
    // Reauthenticate a fresh observation instead of extending that old grant.
    let after = source::inspect_installed_identity()?;
    let after_owner = installed_daemon_owner(layout)?;
    let after_executable = after_owner
        .as_ref()
        .map(|owner| owner.version_root.join("bin/cagentsd"));
    after.verify_owner(after_executable.as_deref())?;
    source.verify_same_owner(&after)?;
    if after.protocol != source.protocol
        || after.proof != source.proof
        || after_executable != executable
    {
        return Err(invalid(
            "The installed source changed while inspecting update impact",
        ));
    }
    Ok(impact::Preview {
        from_version: String::new(),
        windows: 0,
        stopped: Vec::new(),
        stopped_windows: Vec::new(),
        labels: Default::default(),
        editors: crate::install_guard::editor_companions(),
        candidate: staged.identity.clone(),
        replaces_daemon: true,
        daemon: Some(daemon),
    })
}

// Keep the production observer private while its owning parent tests exercise
// actual installed executable routing and transport failure behavior.
#[cfg(test)]
pub(super) fn observe_installed_source_for_test(
    layout: &InstallLayout,
    root: &Path,
    staged: &candidate::Staged,
    policy: impact::Policy,
) -> Result<impact::Preview, UpdateError> {
    observe_installed_source(layout, root, staged, policy)
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

/// Bound on each captured stream: a `daemon start` envelope is well under a
/// kilobyte, and a listing of unattached conversations stays far below this.
const STARTUP_OUTPUT_LIMIT: u64 = 16 * 1024;
/// Bound on the reason quoted in the job's one-line status message.
const STARTUP_REASON_LIMIT: usize = 400;

/// The startup command's JSON envelope and diagnostics. Unlinked files rather
/// than pipes: a descendant that inherited a pipe could hold it open after the
/// command exits, and this coordinator must return through its fences once the
/// command itself has been waited.
struct StartupOutput {
    stdout: File,
    stderr: File,
}

impl StartupOutput {
    fn create(state: &Path) -> io::Result<Self> {
        Ok(Self {
            stdout: tempfile::tempfile_in(state)?,
            stderr: tempfile::tempfile_in(state)?,
        })
    }

    fn attach(&self, command: &mut Command) -> io::Result<()> {
        command
            .stdout(self.stdout.try_clone()?)
            .stderr(self.stderr.try_clone()?);
        Ok(())
    }

    /// Copies both streams, bounded, into `log` and returns the command's own
    /// account of a failure: its JSON error message, else its last stderr line.
    fn record(self, log: &mut impl Write) -> Option<String> {
        let stdout = bounded_text(self.stdout);
        let stderr = bounded_text(self.stderr);
        for (name, text) in [("stdout", &stdout), ("stderr", &stderr)] {
            if !text.trim().is_empty() {
                // The log is diagnostic; failing to write it must not mask the result.
                let _ = writeln!(log, "Startup command {name}:\n{}", text.trim_end());
            }
        }
        failure_reason(&stderr)
    }
}

fn bounded_text(mut file: File) -> String {
    let mut bytes = Vec::new();
    let read = file.seek(SeekFrom::Start(0)).and_then(|_| {
        (&mut file)
            .take(STARTUP_OUTPUT_LIMIT + 1)
            .read_to_end(&mut bytes)
    });
    if let Err(error) = read {
        return format!("[could not read captured output: {error}]");
    }
    let truncated = bytes.len() as u64 > STARTUP_OUTPUT_LIMIT;
    bytes.truncate(STARTUP_OUTPUT_LIMIT as usize);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str(&format!("\n[truncated after {STARTUP_OUTPUT_LIMIT} bytes]"));
    }
    text
}

fn failure_reason(stderr: &str) -> Option<String> {
    let envelope = stderr.lines().rev().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        (value["ok"] == false)
            .then(|| value["error"]["message"].as_str().map(str::to_owned))
            .flatten()
    });
    let reason = envelope.or_else(|| {
        stderr
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("[truncated after"))
            .map(|line| line.strip_prefix("cagents: ").unwrap_or(line).to_owned())
    })?;
    let mut reason: String = reason
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    if let Some((index, _)) = reason.char_indices().nth(STARTUP_REASON_LIMIT) {
        reason.truncate(index);
        reason.push('…');
    }
    Some(reason)
}

/// Readiness never turns a failed startup command into success, but when the
/// installed daemon owns the runtime anyway the user must not be told that the
/// update did not take effect.
fn startup_failure(error: &io::Error, reason: Option<&str>, successor_runs: bool) -> UpdateError {
    fn sentence(text: &str) -> String {
        let text = text.trim_end();
        if text.ends_with(['.', '!', '?', '…']) {
            text.to_owned()
        } else {
            format!("{text}.")
        }
    }
    let mut parts = Vec::new();
    if successor_runs {
        parts.push(
            "The installed version's terminal service is running, but its startup command did not confirm it."
                .to_owned(),
        );
    }
    parts.push(sentence(&error.to_string()));
    if let Some(reason) = reason {
        parts.push(sentence(&format!("It reported: {reason}")));
    }
    if !parts
        .iter()
        .any(|part| part.contains("cagents daemon status"))
    {
        parts.push("Check cagents daemon status before retrying the update.".to_owned());
    }
    invalid(parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_installed_candidate_rolls_back_only_without_a_live_writer() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        const ROOT: &str = "CAGENTS_INSTALLED_ROLLBACK_FIXTURE_ROOT";
        let Some(root) = std::env::var_os(ROOT) else {
            let root = tempfile::tempdir().unwrap();
            for member in ["runtime", "home"] {
                fs::create_dir(root.path().join(member)).unwrap();
                fs::set_permissions(root.path().join(member), fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            let selector = thread::current().name().unwrap().to_owned();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
                .env(ROOT, root.path())
                .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
                .env("HOME", root.path().join("home"))
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        };
        let root = PathBuf::from(root);
        let old = root.join("versions/old");
        let candidate = root.join("versions/candidate");
        for version in [&old, &candidate] {
            fs::create_dir_all(version.join("bin")).unwrap();
        }
        symlink("versions/candidate", root.join("current")).unwrap();
        let cli = candidate.join("bin/cagents");
        fs::write(
            &cli,
            r#"#!/usr/bin/python3
import fcntl, os, pathlib, sys
assert os.environ['CAGENTS_DAEMON_LEASE_FD'] == '199'
assert 'FLOCK' in pathlib.Path('/proc/self/fdinfo/199').read_text()
fcntl.flock(199, fcntl.LOCK_EX | fcntl.LOCK_NB)
root=pathlib.Path(__file__).resolve().parents[3]
if sys.argv[1:] == ['daemon', 'store-schema']:
 print(23)
elif sys.argv[1:] == ['daemon', 'downgrade-store', '23']:
 if (root/'fail-conversion').exists(): sys.exit(1)
 (root/'converted').write_text('23')
else: raise AssertionError(sys.argv)
"#,
        )
        .unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        fs::create_dir_all(&paths.directory).unwrap();
        fs::set_permissions(&paths.directory, fs::Permissions::from_mode(0o700)).unwrap();
        let mut stopped = Command::new("/bin/true").spawn().unwrap();
        let stopped_pid = stopped.id();
        stopped.wait().unwrap();
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("installed-rollback"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: crate::model::HostIncarnation::new("installed-rollback"),
        };
        let metadata =
            |pid| serde_json::to_vec(&serde_json::json!({"pid":pid,"proof":proof})).unwrap();
        let mut lock = OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .mode(0o600)
            .open(&paths.lock)
            .unwrap();
        lock.write_all(&metadata(stopped_pid)).unwrap();
        let mut lease = crate::daemon::lease_handoff::LeaseHandoff::try_acquire(&paths).unwrap();
        let guard = crate::instance::UiUpdateGuard::acquire().unwrap();
        let schema = stopped_store_schema(&candidate, &guard, lease.as_ref().unwrap()).unwrap();
        assert_eq!(schema, 23);
        // Conversion failure must leave the only readable installation selected.
        fs::write(root.join("fail-conversion"), "fail").unwrap();
        assert!(
            rollback_stopped_candidate(
                &root,
                &candidate,
                &old,
                Some(schema),
                true,
                &guard,
                &mut lease
            )
            .is_err()
        );
        assert_eq!(root.join("current").canonicalize().unwrap(), candidate);
        // A command that never spawned cannot have converted the store; old
        // recovery must not depend on invoking that unavailable candidate.
        assert!(
            rollback_stopped_candidate(&root, &candidate, &old, None, false, &guard, &mut lease,)
                .unwrap()
        );
        assert_eq!(root.join("current").canonicalize().unwrap(), old);
        assert!(!root.join("converted").exists());
        fs::remove_file(root.join("current")).unwrap();
        symlink("versions/candidate", root.join("current")).unwrap();
        fs::remove_file(root.join("fail-conversion")).unwrap();
        // A live metadata owner keeps its matched installation and store.
        lock.seek(SeekFrom::Start(0)).unwrap();
        lock.set_len(0).unwrap();
        lock.write_all(&metadata(std::process::id())).unwrap();
        assert!(
            !rollback_stopped_candidate(
                &root,
                &candidate,
                &old,
                Some(schema),
                true,
                &guard,
                &mut lease
            )
            .unwrap()
        );
        assert!(!root.join("converted").exists());
        // Once the new daemon is dead, retain the reservation through conversion
        // and atomically restore the authenticated old source's matched set.
        lock.seek(SeekFrom::Start(0)).unwrap();
        lock.set_len(0).unwrap();
        lock.write_all(&metadata(stopped_pid)).unwrap();
        assert!(
            rollback_stopped_candidate(
                &root,
                &candidate,
                &old,
                Some(schema),
                true,
                &guard,
                &mut lease
            )
            .unwrap()
        );
        assert_eq!(root.join("current").canonicalize().unwrap(), old);
        assert_eq!(fs::read_to_string(root.join("converted")).unwrap(), "23");
        fs::remove_file(root.join("converted")).unwrap();
        // An independently changed current is rejected before store conversion.
        assert!(
            rollback_stopped_candidate(
                &root,
                &candidate,
                &old,
                Some(schema),
                true,
                &guard,
                &mut lease
            )
            .is_err()
        );
        assert!(!root.join("converted").exists());
    }

    #[derive(Clone, Copy)]
    enum StartupPeer {
        Matched,
        OtherExecutable,
        OtherProof,
        ChangedLease,
    }

    fn startup_fixture(
        states: Vec<crate::terminal_ipc::StartupState>,
        peer: StartupPeer,
        timeout: Duration,
    ) -> (Result<(), UpdateError>, usize) {
        use crate::{
            model::{HostIncarnation, HostSessionId, HostSessionProof},
            terminal_ipc::{
                ClientEnvelope, ClientRequest, ServerEnvelope, ServerMessage, ServerResponse,
                StartupState, read_frame, write_frame,
            },
        };
        use std::{
            collections::VecDeque,
            os::unix::net::{UnixListener, UnixStream},
        };
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("cagentsd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut proof = HostSessionProof {
            host_session_id: HostSessionId::new("startup-probe"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new("startup-probe"),
        };
        let mut lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.path().join("cagentsd.lock"))
            .unwrap();
        assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX) }, 0);
        serde_json::to_writer(
            &mut lease,
            &serde_json::json!({"pid":std::process::id(),"proof":proof}),
        )
        .unwrap();
        if matches!(peer, StartupPeer::OtherProof) {
            proof.host_incarnation = HostIncarnation::new("another-startup");
        }
        let lease_path = root.path().join("cagentsd.lock");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let reply = |stream: &mut UnixStream, id, response| {
                write_frame(
                    stream,
                    &ServerEnvelope {
                        version: crate::terminal_ipc::PROTOCOL_VERSION,
                        request_id: Some(id),
                        proof: proof.clone(),
                        message: ServerMessage::Response { response },
                    },
                )
                .unwrap();
            };
            let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(connect.request, ClientRequest::Connect { .. }));
            reply(
                &mut stream,
                connect.request_id,
                ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: Vec::new(),
                },
            );
            let mut states = VecDeque::from(states);
            let mut requests = 0;
            while let Some(request) = read_frame::<ClientEnvelope>(&mut stream).unwrap() {
                assert_eq!(request.request, ClientRequest::StartupState);
                let state = states
                    .pop_front()
                    .unwrap_or(StartupState::Restoring { awaiting: 1 });
                let finished = !matches!(state, StartupState::Restoring { .. });
                if matches!(peer, StartupPeer::ChangedLease) {
                    let mut changed = proof.clone();
                    changed.host_incarnation = HostIncarnation::new("replacement-owner");
                    fs::write(
                        &lease_path,
                        serde_json::to_vec(&serde_json::json!({
                            "pid": std::process::id(), "proof": changed
                        }))
                        .unwrap(),
                    )
                    .unwrap();
                }
                reply(
                    &mut stream,
                    request.request_id,
                    ServerResponse::StartupState(state),
                );
                requests += 1;
                if finished {
                    break;
                }
            }
            requests
        });
        let executable = if matches!(peer, StartupPeer::OtherExecutable) {
            PathBuf::from("/another-installation/bin/cagentsd")
        } else {
            std::env::current_exe().unwrap()
        };
        let result = confirm_daemon_ready_at(&socket, &executable, Instant::now() + timeout);
        (result, server.join().unwrap())
    }

    #[test]
    fn matched_service_must_finish_restoring_before_update_success() {
        use crate::terminal_ipc::StartupState;
        let (result, requests) = startup_fixture(
            vec![StartupState::Restoring { awaiting: 1 }, StartupState::Ready],
            StartupPeer::Matched,
            Duration::from_secs(3),
        );
        result.unwrap();
        assert_eq!(requests, 2);
    }

    #[test]
    fn listening_service_with_retained_ownership_is_an_update_failure() {
        use crate::terminal_ipc::StartupState;
        let (result, requests) = startup_fixture(
            vec![StartupState::Retained {
                detail: "The console could not reconnect".into(),
            }],
            StartupPeer::Matched,
            Duration::from_secs(3),
        );
        let error = result.unwrap_err().to_string();
        assert!(error.contains("could not restore all panes"), "{error}");
        assert!(error.contains("The console could not reconnect"), "{error}");
        assert!(
            error.contains("Existing work and recovery records were kept"),
            "{error}"
        );
        assert_eq!(requests, 1);
    }

    #[test]
    fn another_executable_or_daemon_proof_cannot_satisfy_restoration() {
        for peer in [
            StartupPeer::OtherExecutable,
            StartupPeer::OtherProof,
            StartupPeer::ChangedLease,
        ] {
            let (result, requests) = startup_fixture(
                vec![crate::terminal_ipc::StartupState::Ready],
                peer,
                Duration::from_secs(3),
            );
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("terminal service changed")
            );
            assert_eq!(
                requests,
                usize::from(matches!(peer, StartupPeer::ChangedLease))
            );
        }
    }

    #[test]
    fn a_service_that_remains_restoring_has_a_bounded_failure() {
        let (result, requests) =
            startup_fixture(vec![], StartupPeer::Matched, Duration::from_millis(500));
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("Timed out confirming restored panes"),
            "{error}"
        );
        assert!(requests > 0);
    }

    fn run_captured(
        script: &str,
    ) -> (
        Result<(), crate::install_guard::DaemonStartError>,
        String,
        Option<String>,
    ) {
        let state = tempfile::tempdir().unwrap();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        let output = StartupOutput::create(state.path()).unwrap();
        output.attach(&mut command).unwrap();
        let started =
            crate::install_guard::wait_daemon_start(&mut command, Duration::from_secs(10));
        let mut log = Vec::new();
        let reason = output.record(&mut log);
        // Captures are unlinked: nothing is left in the job's state directory.
        assert_eq!(fs::read_dir(state.path()).unwrap().count(), 0);
        (started, String::from_utf8(log).unwrap(), reason)
    }

    #[test]
    fn failed_startup_logs_its_envelope_and_names_the_reason() {
        let (started, log, reason) = run_captured(
            r#"echo 'partial stdout'
echo 'cagents: 1 retained native conversation could not be reattached' >&2
echo '{"ok":false,"error":{"kind":"daemon_start","message":"could not verify the running cagents daemon: daemon response deadline expired","details":null}}' >&2
exit 1"#,
        );
        let failure = started.unwrap_err();
        assert!(failure.spawned);
        assert!(
            log.contains("Startup command stdout:\npartial stdout"),
            "{log}"
        );
        assert!(
            log.contains("Startup command stderr:\ncagents: 1 retained"),
            "{log}"
        );
        assert!(log.contains(r#""kind":"daemon_start""#), "{log}");
        let reason = reason.unwrap();
        assert_eq!(
            reason,
            "could not verify the running cagents daemon: daemon response deadline expired"
        );
        assert_eq!(
            startup_failure(&failure.error, Some(&reason), false).to_string(),
            "The matched daemon startup command failed (exit status: 1). It reported: could not \
             verify the running cagents daemon: daemon response deadline expired. Check cagents \
             daemon status before retrying the update."
        );
        assert_eq!(
            startup_failure(&failure.error, Some(&reason), true).to_string(),
            "The installed version's terminal service is running, but its startup command did \
             not confirm it. The matched daemon startup command failed (exit status: 1). It \
             reported: could not verify the running cagents daemon: daemon response deadline \
             expired. Check cagents daemon status before retrying the update."
        );
    }

    #[test]
    fn startup_success_keeps_its_envelope_in_the_log() {
        let (started, log, reason) =
            run_captured(r#"echo '{"ok":true,"command":"daemon.start","data":{"running":true}}'"#);
        started.unwrap();
        assert!(
            log.contains(
                r#"Startup command stdout:
{"ok":true,"command":"daemon.start""#
            ),
            "{log}"
        );
        assert!(!log.contains("stderr"), "{log}");
        assert_eq!(reason, None);
    }

    #[test]
    fn startup_capture_is_bounded_and_falls_back_to_the_last_stderr_line() {
        let (started, log, reason) = run_captured(
            "head -c 100000 /dev/zero | tr '\\0' x; printf 'first\\ncagents: last \\033[31mline\\n' >&2; exit 2",
        );
        assert!(started.is_err());
        assert!(log.contains(&format!("[truncated after {STARTUP_OUTPUT_LIMIT} bytes]")));
        assert!(
            log.len() < 2 * STARTUP_OUTPUT_LIMIT as usize,
            "{}",
            log.len()
        );
        assert_eq!(reason.as_deref(), Some("last  [31mline"));
        let long = "y".repeat(2 * STARTUP_REASON_LIMIT);
        let bounded = failure_reason(&long).unwrap();
        assert_eq!(bounded.chars().count(), STARTUP_REASON_LIMIT + 1);
        assert!(bounded.ends_with('…'));
    }

    #[test]
    fn budget_failure_keeps_its_own_guidance() {
        let error = io::Error::new(
            io::ErrorKind::TimedOut,
            "The matched terminal service exceeded its startup budget; its original command was retained until its actual wait completed. Check cagents daemon status before retrying",
        );
        assert_eq!(
            startup_failure(&error, None, true).to_string(),
            "The installed version's terminal service is running, but its startup command did \
             not confirm it. The matched terminal service exceeded its startup budget; its \
             original command was retained until its actual wait completed. Check cagents \
             daemon status before retrying."
        );
    }

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
