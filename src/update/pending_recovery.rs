//! Explicitly authorized recovery of an immutable incomplete preparation.
//! This coordinates the shared recovery owner; it never replays providers.
use super::*;
use crate::{daemon::lease_handoff::LeaseHandoff, upgrade::recovery::PendingRecovery};

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
    let _reserved_worker = reserve_worker_descriptor(&worker)?;
    let mut guard = crate::instance::UiUpdateGuard::acquire()?;
    let before = candidate_stamp(&layout.version_root)?;
    // Re-observe AFTER taking the real fences. A settled recovery can be
    // verified without restarting or stopping the recognized serving owner.
    if verify(&pending, &daemon).is_ok() {
        guard.relaunch_windows(&pending.windows, &ui)?;
        verify(&pending, &daemon)?;
        recovery::retire_current_service()?;
        let outcome = verify(&pending, &daemon)?;
        return Ok(report(&outcome));
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
    let mut windows = pending.windows.clone();
    let current_windows = guard.prepare_windows(Duration::from_secs(45))?;
    for slot in &current_windows {
        if !windows.contains(slot) {
            windows.push(slot.clone());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
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
        guard.set_fallback_executable(ui.clone());
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
        recovery::retire_current_service()?;
        verify(&pending, &daemon)
    });
    match result {
        Ok(outcome) => {
            job.phase = UpdatePhase::Complete;
            job.message = report(&outcome);
            write_job(&state, &job)?;
            Ok(job.message)
        }
        Err(error) => {
            job.phase = UpdatePhase::Failed;
            job.message = format!(
                "Recovery failed: {error}. Retained checkpoint {checkpoint} and its exact owner evidence were kept. No provider was replayed or started. Inspect {} and run '{} --no-start daemon status' to see what is serving",
                job.log_path.display(),
                cli.display()
            );
            let _ = write_job(&state, &job);
            Err(invalid(job.message))
        }
    }
}

fn verify(pending: &PendingRecovery, daemon: &Path) -> Result<recovery::Receipt, UpdateError> {
    let socket = crate::terminal_ipc::daemon_socket_path()?;
    let source::Client::Current { mut client, peer } =
        source::Client::connect(&socket).map_err(|e| invalid(e.to_string()))?
    else {
        return Err(invalid(
            "The serving daemon cannot verify this candidate's recovery outcome",
        ));
    };
    peer.verify(client.proof())?;
    if peer.executable()? != daemon
        || client.protocol() != crate::terminal_ipc::PROTOCOL_VERSION
        || client.startup_state().map_err(|e| invalid(e.to_string()))?
            != crate::terminal_ipc::StartupState::Ready
    {
        return Err(invalid("The selected target daemon is not Ready"));
    }
    let generation = client
        .shutdown_preflight()
        .map_err(|e| invalid(e.to_string()))?
        .capability
        .inventory_generation;
    let original = recovery::Census::from_pending(uuid::Uuid::new_v4().to_string(), pending)?;
    let receipt = original.verify()?;
    if !receipt.usable() {
        return Err(invalid(format!(
            "The shared update census did not verify usable recovery: {:?}",
            receipt.outcome
        )));
    }
    if client
        .shutdown_preflight()
        .map_err(|e| invalid(e.to_string()))?
        .capability
        .inventory_generation
        != generation
    {
        return Err(invalid(
            "Recovery inventory changed during verification; no success was reported",
        ));
    }
    if client.startup_state().map_err(|e| invalid(e.to_string()))?
        != crate::terminal_ipc::StartupState::Ready
    {
        return Err(invalid(
            "The recovery service ceased to be Ready during verification",
        ));
    }
    peer.verify(client.proof())?;
    if peer.executable()? != daemon {
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
    if unsafe { libc::dup3(worker.as_raw_fd(), WORKER_LOCK_FD, libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(WORKER_LOCK_FD) })
}
