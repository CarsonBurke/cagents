//! Explicitly authorized recovery of an immutable incomplete preparation.
//! This coordinates the shared recovery owner; it never replays providers.
use super::*;
use crate::{
    daemon::lease_handoff::LeaseHandoff,
    model::HostSessionProof,
    upgrade::recovery::{
        PendingRecovery, RetirementAcquisition, RetirementGuard, pending_retirement,
    },
};

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
    let cli = fs::canonicalize(crate::process::current_executable()?)?;
    let layout = locate(&cli)?;
    layout.validated_programs()?;
    let daemon = layout.version_root.join("bin/cagentsd");
    let ui = layout.version_root.join("bin/cagents-ui");
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
    #[cfg(target_os = "linux")]
    guard.set_fallback_executable(ui.clone());
    let store = crate::upgrade::handoff::Store::existing()
        .map_err(invalid)?
        .ok_or_else(|| invalid("There is no retained update checkpoint"))?;
    // Protect the checkpoint-to-archive rename across the first verification
    // and the decision to close existing windows. The update lease already
    // excludes a new preparation for this installation.
    let RetirementAcquisition::Acquired(retirement) =
        RetirementGuard::acquire(&store.path).map_err(invalid)?
    else {
        return Err(invalid(
            "The serving daemon is finishing the previous update. Your saved work was kept; retry after it finishes",
        ));
    };
    let (pending, archived) = match PendingRecovery::inspect().map_err(invalid)? {
        Some(pending) => (pending, false),
        None => (
            PendingRecovery::inspect_archived(checkpoint, &daemon)
                .map_err(invalid)?
                .ok_or_else(|| {
                    invalid("There is no retained Prepared checkpoint matching this request")
                })?,
            true,
        ),
    };
    let pending = match original_census {
        Some(path) => pending.with_original_census(path).map_err(invalid)?,
        None => pending,
    };
    if pending.checkpoint_hash != checkpoint {
        return Err(invalid(
            "The retained checkpoint differs from the explicitly authorized hash; nothing was started",
        ));
    }
    // Re-observe AFTER taking the real fences. A settled recovery can be
    // verified without restarting or stopping the recognized serving owner.
    let settled_job = match verify_settled(&pending, &daemon, &store.path) {
        Ok(job) => Some(job),
        // A daemon is serving but would not answer the check. Nothing has
        // been closed or started yet, so this recovery can simply be repeated.
        Err(error @ UpdateError::SourceBusy { .. }) => {
            return Err(invalid(format!(
                "{error}. Nothing was closed or started; repeat this recovery once the daemon finishes"
            )));
        }
        Err(error) if archived => {
            return Err(invalid(format!(
                "The archived recovery's serving daemon could not be verified: {error}. Its saved work was kept"
            )));
        }
        Err(_) => None,
    };
    if let Some(settled) = settled_job {
        let Some(mut job) = incomplete_settled_job(&state, &settled.job)? else {
            // Another update or a completed job owns the current window state.
            return Ok(report(&settled.outcome));
        };
        if cfg!(target_os = "linux") && !archived {
            let switch = staged_selection(&layout)?
                .as_ref()
                .map(|selection| selection.prepare(&layout))
                .transpose()?
                .flatten();
            installed::complete_selection(switch, &layout)?;
            let current_windows = guard.prepare_windows(Duration::from_secs(45))?;
            let windows = recovery_window_slots(&pending.windows, &current_windows);
            guard.close_prepared_windows(&current_windows, Duration::from_secs(45))?;
            drop(retirement);
            guard.relaunch_windows(&windows, &ui)?;
        } else {
            // The verified Darwin successor already published its selection.
            // Both retries keep existing windows and open only recorded slots.
            installed::selected_candidate(&layout.prefix, &layout.version_root)?;
            drop(retirement);
            guard.relaunch_missing_windows(&settled.windows, &ui)?;
        }
        let outcome = verify(&pending, &daemon, None).and_then(|(serving, outcome)| {
            if archived {
                return Ok((serving, outcome));
            }
            recovery::retire_current_service(source::BusyPatience::for_one_run())?;
            verify(&pending, &daemon, None)
        });
        return match outcome {
            Ok((_, outcome)) => {
                let message = report(&outcome);
                record_completion(&state, &mut job, &message)?;
                Ok(message)
            }
            Err(error @ UpdateError::SourceBusy { .. }) => Err(invalid(format!(
                "Recovery could not be verified: {error}. The serving daemon was kept; repeat this recovery once the daemon finishes"
            ))),
            Err(error) => Err(error),
        };
    }
    drop(retirement);
    let before = installed::stamp_with_limit(&layout.version_root, RECOVERY_BINARY_LIMIT)?;
    #[cfg(target_os = "linux")]
    let selection = staged_selection(&layout)?;
    #[cfg(target_os = "linux")]
    let previous_link = selection
        .as_ref()
        .and_then(installed::Selection::previous_link);
    #[cfg(target_os = "macos")]
    let previous_link = (served_version_root(&layout)? != layout.version_root)
        .then(|| fs::read_link(layout.prefix.join("current")))
        .transpose()?;
    #[cfg(target_os = "macos")]
    let previous_link = previous_link.as_deref();
    let paths = crate::daemon::RuntimePaths::discover().map_err(|e| invalid(e.to_string()))?;
    let handoff = LeaseHandoff::try_acquire(&paths).map_err(|e| invalid(e.to_string()))?
        .ok_or_else(|| invalid("A daemon is serving or starting. It was kept running; inspect `cagents --no-start daemon status` before repeating recovery"))?;
    if crate::descriptor::connect_settled(&paths.socket).is_ok() {
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
    let id = uuid::Uuid::new_v4().to_string();
    let log_path = state.join(format!("{id}.log"));
    let log = private_create(&log_path)?;
    // Persist each slot before any request asks its window to close.
    let saved_windows = recovery_window_slots(&pending.windows, &guard.running_window_slots()?);
    let mut receipt = pending
        .authorize_manual(
            checkpoint,
            yes,
            &state,
            &id,
            &daemon,
            &worker,
            &guard,
            &handoff,
            &saved_windows,
        )
        .map_err(invalid)?;
    let saved_windows = receipt.saved_windows().to_vec();
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
        if installed::stamp_with_limit(&layout.version_root, RECOVERY_BINARY_LIMIT)? != before {
            return Err(invalid(
                "The selected candidate changed before recovery; nothing was started",
            ));
        }
        // The exact Prepared successor owns Darwin publication. A selected
        // Darwin candidate still needs standby to authenticate the receipt.
        let parks_successor = cfg!(target_os = "macos") || previous_link.is_some();
        let mut standby = parks_successor
            .then(|| {
                let binary = fs::symlink_metadata(&daemon)?;
                crate::daemon::standby::Parked::launch(
                    &daemon,
                    (binary.dev(), binary.ino()),
                    &guard,
                    previous_link,
                )
            })
            .transpose()?;
        let timeout = crate::install_guard::daemon_start_timeout()?;
        // Prepare the authority transport before touching windows or publishing
        // the direct-start target.
        let direct_start = if standby.is_some() {
            None
        } else {
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
            Some(command)
        };
        #[cfg(target_os = "macos")]
        let previous = served_version_root(&layout)?;
        #[cfg(target_os = "macos")]
        guard.set_fallback_executable(previous.join("bin/cagents-ui"));
        let current_windows = guard.prepare_windows(Duration::from_secs(45))?;
        if current_windows
            .iter()
            .any(|slot| !saved_windows.contains(slot))
        {
            return Err(invalid(
                "A window appeared outside the saved recovery scope",
            ));
        }
        if direct_start.is_some() {
            select_target(&layout)?;
        }
        guard.close_prepared_windows(&current_windows, Duration::from_secs(45))?;
        let pid = if let Some(standby) = standby.as_mut() {
            let started = standby
                .activate(&daemon, Some(&handoff), Some(&receipt), timeout, || {
                    let _ = installed::report_overdue_startup(&state, &mut job);
                })
                .map_err(UpdateError::from);
            // Authority may have arrived even when its acknowledgement did
            // not. Only the exact child can settle that uncertain transfer.
            let ready =
                if started.is_ok() || (cfg!(target_os = "macos") && standby.may_own_runtime()) {
                    installed::confirm_daemon_ready(&daemon, Some(standby.pid()), timeout)
                } else {
                    started
                };
            let ready = ready
                .and_then(|()| installed::selected_candidate(&layout.prefix, &layout.version_root));
            if ready.is_err() {
                let owns_runtime = standby.may_own_runtime();
                #[cfg(target_os = "macos")]
                if owns_runtime || installed::selected_candidate(&layout.prefix, &previous).is_err()
                {
                    guard.defer_reopening_to_recovery();
                }
                if owns_runtime {
                    guard.leave_launch_fence_with_successor();
                }
            }
            ready?;
            #[cfg(target_os = "macos")]
            guard.set_fallback_executable(ui.clone());
            Some(standby.pid())
        } else {
            None
        };
        if let Some(mut command) = direct_start {
            crate::install_guard::wait_daemon_start(&mut command, timeout)
            .map_err(|failure| invalid(format!("Target recovery startup failed: {}. Its checkpoint and claim were preserved; inspect {} before another recovery attempt", failure.error, job.log_path.display())))?;
        }
        // The host has settled and dropped its copies of the updater fences.
        // Keep our own continuous daemon reservation until verified service.
        let pending = PendingRecovery::inspect()
            .map_err(invalid)?
            .ok_or_else(|| invalid("The Prepared evidence disappeared after recovery"))?;
        let (serving, _) = verify(&pending, &daemon, pid)?;
        receipt.verify_successor(&serving).map_err(invalid)?;
        receipt.release_daemon_reservation();
        Ok::<_, UpdateError>(saved_windows)
    })();
    // Close the coordinator's reservation without unlocking the serving daemon.
    drop(handoff);
    let result = result.and_then(|windows| {
        guard.relaunch_windows(&windows, &ui)?;
        // Opening saved panes may attach owners. Recheck actual serving
        // identity and full inventory after reopening before claiming Ready.
        let pending = PendingRecovery::inspect()
            .map_err(invalid)?
            .ok_or_else(|| invalid("The recovery checkpoint disappeared while reopening panes"))?;
        verify(&pending, &daemon, None)?;
        recovery::retire_current_service(source::BusyPatience::for_one_run())?;
        verify(&pending, &daemon, None)
    });
    match result {
        Ok((_, outcome)) => {
            let message = report(&outcome);
            record_completion(&state, &mut job, &message)?;
            Ok(message)
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

fn incomplete_settled_job(state: &Path, id: &str) -> Result<Option<UpdateJob>, UpdateError> {
    // The caller retains the update lock through any window and status changes.
    let job: UpdateJob = match read_private(&state.join("status.json"), JOB_STATE_LIMIT) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok((job.id == id && job.phase != UpdatePhase::Complete).then_some(job))
}

fn staged_selection(layout: &InstallLayout) -> Result<Option<installed::Selection>, UpdateError> {
    match fs::symlink_metadata(layout.version_root.join(installed::LOCAL_STAGE_MARKER)) {
        Ok(_) => installed::Selection::capture(layout).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn record_completion(state: &Path, job: &mut UpdateJob, message: &str) -> Result<(), UpdateError> {
    job.phase = UpdatePhase::Complete;
    job.confirmation = None;
    job.refusal = None;
    job.message = message.to_owned();
    write_job(state, job)?;
    retention::remember_success(state, &job.id, &job.phase);
    Ok(())
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

struct Settled {
    job: String,
    windows: Vec<String>,
    outcome: recovery::Receipt,
}

fn verify_settled(
    pending: &PendingRecovery,
    daemon: &Path,
    checkpoint: &Path,
) -> Result<Settled, UpdateError> {
    let (_, outcome) = verify(pending, daemon, None)?;
    let settled = pending_retirement(checkpoint, daemon)
        .map_err(invalid)?
        .ok_or_else(|| invalid("The installed recovery has no settled Prepared record"))?;
    recovery::require_prepared_scope(pending, &settled, daemon)?;
    Ok(Settled {
        job: settled.job,
        windows: settled.windows,
        outcome,
    })
}

/// One recovery check. Its fences and census share one busy patience, so a
/// busy daemon delays the check, and the windows it may keep closed, at most
/// that long.
fn verify(
    pending: &PendingRecovery,
    daemon: &Path,
    pid: Option<u32>,
) -> Result<(HostSessionProof, recovery::Receipt), UpdateError> {
    let busy = source::BusyPatience::for_one_run();
    let socket = crate::terminal_ipc::daemon_socket_path()?;
    let mut client = source::Client::connect(&socket)
        .map_err(|e| invalid(e.to_string()))?
        .with_busy_patience(busy);
    if pid.is_some_and(|pid| client.peer_pid() != pid)
        || client.serving_executable()? != daemon
        || client.protocol() != crate::terminal_ipc::PROTOCOL_VERSION
        || client.startup_state()? != crate::terminal_ipc::StartupState::Ready
    {
        return Err(invalid("The selected target daemon is not Ready"));
    }
    // The fence is asked again while the daemon finishes another operation,
    // which refuses it before issuing anything, rather than failing recovery.
    let generation = client.inventory_generation()?;
    let original = recovery::Census::from_pending(uuid::Uuid::new_v4().to_string(), pending)?;
    let receipt = original.verify(busy)?;
    if let Some(error) = receipt.still_busy() {
        return Err(error);
    }
    if !receipt.usable() {
        return Err(invalid(format!(
            "The shared update census did not verify usable recovery: {:?}",
            receipt.outcome
        )));
    }
    if !receipt.verifies_replacement(&pending.source, client.proof()) {
        return Err(invalid(
            "The verified inventory belongs to a different recovery service",
        ));
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
    Ok((client.proof().clone(), receipt))
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

const RECOVERY_BINARY_LIMIT: u64 = 512 * 1024 * 1024;

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

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    #[test]
    fn an_archived_recovery_only_changes_its_own_incomplete_job() {
        use super::*;

        let state = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let failed = UpdateJob {
            id: id.clone(),
            version: crate::version().into(),
            phase: UpdatePhase::Failed,
            message: "Retirement did not finish".into(),
            log_path: state.path().join("retained.log"),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: Some(crate::version().into()),
        };
        write_job(state.path(), &failed).unwrap();
        let status = state.path().join("status.json");
        let before = fs::read(&status).unwrap();
        assert!(
            incomplete_settled_job(state.path(), &uuid::Uuid::new_v4().to_string())
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read(&status).unwrap(), before);

        let mut job = incomplete_settled_job(state.path(), &id).unwrap().unwrap();
        record_completion(state.path(), &mut job, "Ready").unwrap();
        assert!(incomplete_settled_job(state.path(), &id).unwrap().is_none());
        let completed: UpdateJob = serde_json::from_slice(&fs::read(&status).unwrap()).unwrap();
        assert_eq!(completed.phase, UpdatePhase::Complete);
        assert_eq!(completed.log_path, failed.log_path);
        assert_eq!(completed.message, "Ready");
        assert!(state.path().join(format!("{id}.complete")).is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn refused_start_transport_keeps_current() {
        use super::*;
        use crate::{
            daemon::RuntimePaths,
            model::{HostIncarnation, HostSessionId, HostSessionProof},
            upgrade::handoff::{Checkpoint, Store},
        };

        const TEST: &str = "update::pending_recovery::tests::refused_start_transport_keeps_current";
        const FIXTURE: &str = "CAGENTS_TEST_RECOVERY_START_PREFIX";
        if let Some(prefix) = std::env::var_os(FIXTURE) {
            let prefix = PathBuf::from(prefix);
            let paths = RuntimePaths::discover().unwrap();
            crate::secure_file::prepare_private_directory(&paths.directory).unwrap();
            private_create(&paths.lock).unwrap();
            let proof = HostSessionProof {
                host_session_id: HostSessionId::new("recovery-start-source"),
                boot_id: crate::daemon::read_boot_id().unwrap(),
                host_incarnation: HostIncarnation::new("recovery-start-source"),
            };
            let store = Store::discover().unwrap();
            store
                .save(&Checkpoint::new(proof.clone(), vec![], vec![], vec![]))
                .unwrap();
            let before = fs::read(&store.path).unwrap();
            let pending = PendingRecovery::inspect().unwrap().unwrap();
            let census = prefix.join("census");
            serde_json::to_writer(
                private_create(&census).unwrap(),
                &serde_json::json!({
                    "version": 1,
                    "transaction": uuid::Uuid::new_v4(),
                    "source": proof,
                    "terminals": [],
                    "chats": [],
                }),
            )
            .unwrap();
            // The test binary raises its soft limit on startup. Lower it in
            // this private child after initialization, with all ordinary
            // recovery descriptors still available below 200.
            let mut limits = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: the calls read and update this fixture child's limit.
            unsafe {
                assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits), 0);
                limits.rlim_cur = 200;
                assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limits), 0);
            }
            let error = run(&pending.checkpoint_hash, true, Some(&census)).unwrap_err();
            // The attempt reached startup after obtaining all its leases and
            // validating the checkpoint. Forwarding requires a descriptor
            // numbered 200 or above, which this child cannot allocate.
            let job: UpdateJob =
                serde_json::from_slice(&fs::read(prefix.join(".update/status.json")).unwrap())
                    .unwrap();
            assert_eq!(job.phase, UpdatePhase::Failed);
            assert!(
                error
                    .to_string()
                    .contains(&io::Error::from_raw_os_error(libc::EINVAL).to_string())
            );
            assert_eq!(
                fs::read_link(prefix.join("current")).unwrap(),
                Path::new("versions/old")
            );
            assert_eq!(fs::read(&store.path).unwrap(), before);
            return;
        }

        let fixture = tempfile::tempdir_in("/var/tmp").unwrap();
        let prefix = fixture.path().join("install");
        let candidate = prefix.join("versions/new/bin");
        fs::create_dir_all(&candidate).unwrap();
        fs::create_dir(prefix.join("versions/old")).unwrap();
        fs::write(
            prefix.join(".cagents-install-root"),
            "cagents install root\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("versions/old", prefix.join("current")).unwrap();
        let cli = candidate.join("cagents");
        fs::copy(crate::process::current_executable().unwrap(), &cli).unwrap();
        fs::write(candidate.join("cagentsd"), b"unused daemon").unwrap();
        fs::write(candidate.join("cagents-ui"), b"unused window").unwrap();
        let runtime = fixture.path().join("runtime");
        crate::secure_file::prepare_private_directory(&runtime).unwrap();
        let output = Command::new(cli)
            .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
            .env(FIXTURE, &prefix)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_DATA_HOME", fixture.path().join("data"))
            .env("XDG_CONFIG_HOME", fixture.path().join("config"))
            .env("XDG_STATE_HOME", fixture.path().join("state"))
            .env("XDG_CACHE_HOME", fixture.path().join("cache"))
            .env("HOME", fixture.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}
