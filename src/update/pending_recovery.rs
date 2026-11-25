//! Explicitly authorized recovery of an immutable incomplete preparation.
//! This coordinates the shared recovery owner; it never replays providers.
use super::*;
use crate::{daemon::lease_handoff::LeaseHandoff, upgrade::recovery::PendingRecovery};

/// A recovery the person can authorize from the update control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppRecovery {
    job: String,
    candidate: PathBuf,
    checkpoint: String,
    census: PathBuf,
    census_digest: String,
    include_original_census: bool,
    saved_windows: bool,
}

impl AppRecovery {
    pub fn latest() -> Result<Option<Self>, UpdateError> {
        if crate::device::is_remote()
            || std::env::var_os(crate::agent_execution::AUTHORITY_ENV).is_some()
        {
            return Ok(None);
        }
        match status()? {
            Some(job) if job.phase == UpdatePhase::Failed => Self::available(&job.id),
            _ => Ok(None),
        }
    }

    pub fn available(job: &str) -> Result<Option<Self>, UpdateError> {
        if crate::device::is_remote()
            || std::env::var_os(crate::agent_execution::AUTHORITY_ENV).is_some()
            || uuid::Uuid::parse_str(job)
                .ok()
                .is_none_or(|id| id.to_string() != job)
        {
            return Ok(None);
        }
        let Some(current) =
            status()?.filter(|current| current.id == job && current.phase == UpdatePhase::Failed)
        else {
            return Ok(None);
        };
        let installation = locate(&std::env::current_exe()?)?;
        let Some(pending) = PendingRecovery::inspect().map_err(invalid)? else {
            return Ok(None);
        };
        let original = pending.retained_job_id().unwrap_or(job);
        if uuid::Uuid::parse_str(original)
            .ok()
            .is_none_or(|id| id.to_string() != original)
        {
            return Ok(None);
        }
        let state = installation.prefix.join(".update");
        let receipt: recovery::Receipt =
            match read_private(&recovery::outcome_path(&state, original), JOB_STATE_LIMIT) {
                Ok(bytes) => serde_json::from_slice(&bytes)?,
                Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
        let Some(required) = receipt.target_required() else {
            return Ok(None);
        };
        if required.source != pending.source
            || required.checkpoint_digest != pending.checkpoint_hash
        {
            return Ok(None);
        }
        let retained =
            recovery::retained_install_job(&installation, &pending, original, &current.version)?;
        receipt.matches(&recovery::Census::load(&retained.census)?)?;
        Ok(Some(Self {
            job: job.to_owned(),
            candidate: retained.cli,
            checkpoint: required.checkpoint_digest,
            census: retained.census,
            census_digest: required.census_digest,
            include_original_census: !pending.original_census_complete,
            saved_windows: !pending.windows.is_empty(),
        }))
    }

    fn candidate_ui(&self) -> PathBuf {
        self.candidate.with_file_name("cagents-ui")
    }

    /// Recovery waits for open windows to release their leases. Keep its
    /// child waiter out of any window's executor so that executor can exit.
    #[cfg(feature = "ui")]
    pub fn start(
        self,
        open_window: bool,
    ) -> io::Result<tokio::sync::oneshot::Receiver<Result<String, UpdateError>>> {
        let (send, receive) = tokio::sync::oneshot::channel();
        thread::Builder::new()
            .name("update-recovery".into())
            .spawn(move || {
                let _ = send.send(self.recover(open_window));
            })?;
        Ok(receive)
    }

    /// Perform the exact previously offered recovery after explicit approval.
    pub fn recover(self, open_window: bool) -> Result<String, UpdateError> {
        if Self::available(&self.job)?.as_ref() != Some(&self) {
            return Err(invalid(
                "The saved recovery changed. Recheck the update before continuing.",
            ));
        }
        let mut command = Command::new(&self.candidate);
        command
            .args([
                "--no-start",
                "--json",
                "daemon",
                "recover-update",
                "--checkpoint",
            ])
            .arg(&self.checkpoint)
            .arg("--yes")
            .stdin(Stdio::null());
        if self.include_original_census {
            command.arg("--census").arg(&self.census);
        }
        crate::install_guard::sanitize_child_environment(&mut command);
        // The recovery coordinator closes the windows before it starts the
        // saved daemon and waits for the new windows to report ready.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                crate::daemon::seal_inherited_descriptors()
            });
        }
        let report = recovery_reply(&command.output()?)?;
        if open_window && !self.saved_windows {
            crate::instance::open_recovered_window(&self.candidate_ui())?;
        }
        Ok(report)
    }
}

fn recovery_reply(output: &std::process::Output) -> Result<String, UpdateError> {
    if output.status.success() {
        let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        return reply["data"]["report"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| invalid("The recovery finished without reporting its result"));
    }
    let message = output
        .stderr
        .rsplit(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .find_map(|reply| reply["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| {
            "Recovery could not finish. Your saved sessions are still available for recovery."
                .into()
        });
    Err(invalid(message))
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
#[test]
fn recovery_preserves_the_candidates_json_refusal() {
    use std::os::unix::process::ExitStatusExt;
    let output = std::process::Output {
        status: std::process::ExitStatus::from_raw(1 << 8),
        stdout: vec![],
        stderr: b"diagnostic\n{\"error\":{\"message\":\"The selected checkpoint changed\"}}\n"
            .to_vec(),
    };
    assert!(
        recovery_reply(&output)
            .unwrap_err()
            .to_string()
            .contains("The selected checkpoint changed")
    );
}

/// Run only through the exact retained candidate named in the recovery command.
/// `--yes` authorizes this hash, rather than pretending a disk artifact is ACK.
pub(crate) fn run(
    checkpoint: &str,
    yes: bool,
    original_census: Option<&Path>,
) -> Result<String, UpdateError> {
    if !yes
        || checkpoint.len() != 64
        || !checkpoint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(
            "Recovery requires --checkpoint SHA256 --yes; nothing was started",
        ));
    }
    let cli = fs::canonicalize(std::env::current_exe()?)?;
    let layout = locate(&cli)?;
    let daemon = layout.version_root.join("bin/cagentsd");
    let ui = layout.version_root.join("bin/cagents-ui");
    let pending = PendingRecovery::inspect()
        .map_err(invalid)?
        .ok_or_else(|| invalid("There is no retained Prepared checkpoint to recover"))?;
    let pending = match original_census {
        Some(path) => pending.with_original_census(path).map_err(invalid)?,
        None => pending,
    };
    if pending.checkpoint_hash != checkpoint {
        return Err(invalid(
            "The retained checkpoint differs from the explicitly authorized hash; nothing was started",
        ));
    }
    let state = layout.prefix.join(".update");
    crate::secure_file::prepare_private_directory(&state)?;
    // Global ordering matches ordinary updates: worker, UI, then daemon lease.
    // Convert to File so dropping a coordinator never LOCK_UNs shared custody.
    let worker = acquire_lock(&state)?.into_file();
    retention::before_start(&state);
    let _reserved_worker = reserve_worker_descriptor(&worker)?;
    let mut guard = crate::instance::UiUpdateGuard::acquire()?;
    // Arm Drop before any fallible close: reopening a saved recovery pane is
    // an attachment, never permission to resume or start its provider.
    guard.set_recovery_restore_only();
    guard.set_fallback_executable(ui.clone());
    let before = candidate_stamp(&layout.version_root)?;
    // Re-observe AFTER taking the real fences. A settled recovery can be
    // verified without restarting or stopping the recognized serving owner.
    let settled = match verify(&pending, &daemon) {
        Ok(_) => true,
        // A daemon is serving but would not answer the check. Nothing has
        // been closed or started yet, so this recovery can simply be repeated.
        Err(error @ UpdateError::SourceBusy { .. }) => {
            return Err(invalid(format!(
                "{error}. Nothing was closed or started; repeat this recovery once the daemon finishes"
            )));
        }
        Err(_) => false,
    };
    if settled {
        // A pre-existing UI may have ordinary restore policy. Close and reopen
        // it under the same guarded policy instead of merely activating it.
        let current_windows = guard.prepare_windows(Duration::from_secs(45))?;
        let windows = recovery_window_slots(&pending.windows, &current_windows);
        guard.close_prepared_windows(&current_windows, Duration::from_secs(45))?;
        guard.relaunch_windows(&windows, &ui)?;
        let outcome = verify(&pending, &daemon).and_then(|_| {
            recovery::retire_current_service(source::BusyPatience::for_one_run())?;
            verify(&pending, &daemon)
        });
        return match outcome {
            Ok(outcome) => Ok(report(&outcome)),
            Err(error @ UpdateError::SourceBusy { .. }) => Err(invalid(format!(
                "Recovery could not be verified: {error}. The serving daemon was kept and its windows were reopened; repeat this recovery once the daemon finishes"
            ))),
            Err(error) => Err(error),
        };
    }
    let paths = crate::daemon::RuntimePaths::discover().map_err(|e| invalid(e.to_string()))?;
    let handoff = LeaseHandoff::try_acquire(&paths).map_err(|e| invalid(e.to_string()))?
        .ok_or_else(|| invalid("A daemon is serving or starting. It was kept running; inspect `cagents --no-start daemon status` before repeating recovery"))?;
    if std::os::unix::net::UnixStream::connect(&paths.socket).is_ok() {
        return Err(invalid(
            "A daemon still answers on the socket; explicit recovery did not replace it",
        ));
    }
    let fresh = PendingRecovery::inspect()
        .map_err(invalid)?
        .ok_or_else(|| invalid("The retained checkpoint disappeared before recovery"))?;
    if fresh.checkpoint_hash != checkpoint || fresh.source != pending.source {
        return Err(invalid(
            "The selected checkpoint changed after acquiring recovery leases",
        ));
    }
    let current_windows = guard.prepare_windows(Duration::from_secs(45))?;
    let windows = recovery_window_slots(&pending.windows, &current_windows);
    let id = uuid::Uuid::new_v4().to_string();
    let census = recovery::Census::from_pending(id.clone(), &pending)?;
    census.save(&recovery::census_path(&state, &id))?;
    let log_path = state.join(format!("{id}.log"));
    let log = private_create(&log_path)?;
    let mut receipt = pending
        .authorize_manual(
            checkpoint, yes, &state, &id, &daemon, &worker, &guard, &handoff,
        )
        .map_err(invalid)?;
    let mut job = UpdateJob {
        id: id.clone(),
        version: crate::version().into(),
        phase: UpdatePhase::Installing,
        message: "Recovering retained owners without starting providers".into(),
        log_path,
        confirmation: None,
        refusal: None,
        in_flight: Vec::new(),
        recorded_by: Some(crate::version().into()),
    };
    write_job(&state, &job)?;
    let result = (|| {
        // The sealed receipt binds the target path/hash and exact checkpoint.
        // Verify the entire matched set again before routing current to it.
        if candidate_stamp(&layout.version_root)? != before {
            return Err(invalid(
                "The selected candidate changed before recovery; nothing was started",
            ));
        }
        select_target(&layout)?;
        guard.close_prepared_windows(&current_windows, Duration::from_secs(45))?;
        let timeout = crate::install_guard::daemon_start_timeout()?;
        let mut command = Command::new(&cli);
        command
            .args(["daemon", "start"])
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?);
        crate::install_guard::sanitize_child_environment(&mut command);
        unsafe {
            command.pre_exec(crate::daemon::seal_inherited_descriptors);
        }
        guard.inherit_for_install(&mut command)?;
        handoff.inherit(&mut command)?;
        // Receipt forwarding must remain last, after every descriptor remap.
        receipt.inherit(&mut command)?;
        crate::install_guard::wait_daemon_start(&mut command, timeout)
            .map_err(|failure| invalid(format!("Target recovery startup failed: {}. Its checkpoint and claim were preserved; inspect {} before another recovery attempt", failure.error, job.log_path.display())))?;
        // The host has settled and dropped its copies of the updater fences.
        // Keep our own continuous daemon reservation until verified service.
        let pending = PendingRecovery::inspect()
            .map_err(invalid)?
            .ok_or_else(|| invalid("The Prepared evidence disappeared after recovery"))?;
        let outcome = verify(&pending, &daemon)?;
        receipt.release_daemon_reservation();
        Ok::<_, UpdateError>(outcome)
    })();
    // Close the coordinator's reservation without unlocking the serving daemon.
    drop(handoff);
    let result = result.and_then(|_| {
        guard.relaunch_windows(&windows, &ui)?;
        // Opening saved panes may attach owners. Recheck actual serving
        // identity and full inventory after reopening before claiming Ready.
        let pending = PendingRecovery::inspect()
            .map_err(invalid)?
            .ok_or_else(|| invalid("The recovery checkpoint disappeared while reopening panes"))?;
        verify(&pending, &daemon)?;
        recovery::retire_current_service(source::BusyPatience::for_one_run())?;
        let outcome = verify_census(&census, &daemon)?;
        outcome
            .save(&recovery::outcome_path(&state, &job.id))
            .map_err(|error| {
                invalid(format!(
                    "Saved windows reopened, but the verified recovery result could not be saved: {error}"
                ))
            })?;
        Ok(outcome)
    });
    match result {
        Ok(outcome) => {
            job.phase = UpdatePhase::Complete;
            job.message = report(&outcome);
            write_job(&state, &job)?;
            retention::remember_success(&state, &job.id, &job.phase);
            Ok(job.message)
        }
        Err(error) => {
            job.phase = UpdatePhase::Failed;
            job.message = match &error {
                // Only the check was refused. Recovery may already serve the
                // target daemon and its windows; repeating it verifies them.
                UpdateError::SourceBusy { .. } => format!(
                    "Recovery could not be verified: {error}. Retained checkpoint {checkpoint} and its exact owner evidence were kept. No provider was replayed or started. Run '{} --no-start daemon status' to see what is serving, then repeat this recovery once the daemon finishes",
                    cli.display()
                ),
                _ => format!(
                    "Recovery failed: {error}. Retained checkpoint {checkpoint} and its exact owner evidence were kept. No provider was replayed or started. Inspect {} and run '{} --no-start daemon status' to see what is serving",
                    job.log_path.display(),
                    cli.display()
                ),
            };
            let _ = write_job(&state, &job);
            Err(invalid(job.message))
        }
    }
}

fn recovery_window_slots(pending: &[String], current: &[String]) -> Vec<String> {
    let mut windows = pending.to_vec();
    for slot in current {
        if !windows.contains(slot) {
            windows.push(slot.clone());
        }
    }
    windows
}

/// One recovery check. Its fences and census share one busy patience, so a
/// busy daemon delays the check, and the windows it may keep closed, at most
/// that long.
fn verify(pending: &PendingRecovery, daemon: &Path) -> Result<recovery::Receipt, UpdateError> {
    let census = recovery::Census::from_pending(uuid::Uuid::new_v4().to_string(), pending)?;
    verify_census(&census, daemon)
}

fn verify_census(
    census: &recovery::Census,
    daemon: &Path,
) -> Result<recovery::Receipt, UpdateError> {
    let busy = source::BusyPatience::for_one_run();
    let socket = crate::terminal_ipc::daemon_socket_path()?;
    let mut client = source::Client::connect(&socket)
        .map_err(|e| invalid(e.to_string()))?
        .with_busy_patience(busy);
    if client.serving_executable()? != daemon
        || client.protocol() != crate::terminal_ipc::PROTOCOL_VERSION
        || client.startup_state()? != crate::terminal_ipc::StartupState::Ready
    {
        return Err(invalid("The selected target daemon is not Ready"));
    }
    // The fence is asked again while the daemon finishes another operation,
    // which refuses it before issuing anything, rather than failing recovery.
    let generation = client.inventory_generation()?;
    let receipt = census.verify(busy)?;
    if let Some(error) = receipt.still_busy() {
        return Err(error);
    }
    if !receipt.usable() {
        return Err(invalid(format!(
            "The shared update census did not verify usable recovery: {:?}",
            receipt.outcome
        )));
    }
    if client.inventory_generation()? != generation {
        return Err(invalid(
            "Recovery inventory changed during verification; no success was reported",
        ));
    }
    if client.startup_state()? != crate::terminal_ipc::StartupState::Ready {
        return Err(invalid(
            "The recovery service ceased to be Ready during verification",
        ));
    }
    if client.serving_executable()? != daemon {
        return Err(invalid("The serving recovery executable changed"));
    }
    Ok(receipt)
}

fn report(receipt: &recovery::Receipt) -> String {
    match &receipt.outcome {
        recovery::Outcome::Usable {
            terminals,
            chats,
            paused_terminals,
            paused_chats,
            ..
        } => format!(
            "The target daemon is Ready with {terminals} live terminals and {chats} live conversations. Paused: {} terminals, {} conversations; these remain available for explicit continuation, and their providers were not started.",
            paused_terminals.len(),
            paused_chats.len()
        ),
        _ => receipt.failure().to_string(),
    }
}

#[derive(PartialEq, Eq)]
struct CandidateStamp(Vec<(u64, u64, u64, i64, i64, i64, i64)>);
fn candidate_stamp(root: &Path) -> Result<CandidateStamp, UpdateError> {
    let mut entries = Vec::new();
    for name in ["cagents", "cagentsd", "cagents-ui"] {
        let metadata = fs::symlink_metadata(root.join("bin").join(name))?;
        if !metadata.is_file()
            || metadata.uid() != crate::daemon::current_uid()
            || metadata.mode() & 0o022 != 0
            || metadata.len() > 512 * 1024 * 1024
        {
            return Err(invalid(
                "The selected candidate is not a privately owned matched executable set",
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
    Ok(CandidateStamp(entries))
}

fn select_target(layout: &InstallLayout) -> Result<(), UpdateError> {
    let current = layout.prefix.join("current");
    if let Ok(metadata) = fs::symlink_metadata(&current)
        && !metadata.file_type().is_symlink()
    {
        return Err(invalid(
            "The current installation pointer is not a symlink; it was preserved",
        ));
    }
    if let Ok(selected) = fs::canonicalize(&current)
        && selected == layout.version_root
    {
        return Ok(());
    }
    let relative = layout
        .version_root
        .strip_prefix(&layout.prefix)
        .map_err(|_| invalid("The recovery target belongs to another installation"))?;
    let temporary = layout
        .prefix
        .join(format!(".recovery-{}", uuid::Uuid::new_v4()));
    std::os::unix::fs::symlink(relative, &temporary)?;
    if let Err(error) = fs::rename(&temporary, &current) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    File::open(&layout.prefix)?.sync_all()?;
    Ok(())
}

fn reserve_worker_descriptor(worker: &File) -> Result<File, UpdateError> {
    if unsafe { libc::fcntl(WORKER_LOCK_FD, libc::F_GETFD) } != -1 {
        return Err(invalid(
            "The reserved recovery worker descriptor is already occupied",
        ));
    }
    Ok(File::from(crate::descriptor::duplicate_to(
        std::os::fd::AsFd::as_fd(worker),
        WORKER_LOCK_FD,
    )?))
}
