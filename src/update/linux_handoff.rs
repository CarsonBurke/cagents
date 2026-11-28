//! Finish the update accepted by the replacement daemon in the user's launch context.
use super::*;
use crate::{
    daemon::holder_registry::Process,
    descriptor_frame::{FrameByteOrder, receive_json_with_fds_until, send_json_with_fds_until},
    instance::{UiUpdateGuard, WindowRestorePolicy},
    model::HostSessionProof,
    pinned_process::PinnedProcess,
    upgrade::handoff::Phase,
};
use std::os::{
    fd::{AsFd, BorrowedFd},
    unix::net::UnixStream,
};

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Handoff {
    job: String,
    worker: Process,
    finisher: Option<Process>,
    request_digest: String,
    census_digest: String,
    source: HostSessionProof,
    checkpoint_digest: String,
    checkpoint_phase: Phase,
    target: PathBuf,
    build: installed::Stamp,
    windows: Vec<String>,
    restore_policy: WindowRestorePolicy,
    accepted: Option<Acceptance>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Acceptance {
    proof: HostSessionProof,
    process: Process,
}

fn artifact(job: &str) -> String {
    format!("{job}.linux-handoff.json")
}

fn request_digest(state: &Path, job: &str) -> Result<String, UpdateError> {
    let request = read_private(&state.join(format!("{job}.request")), SMALL_ASSET_LIMIT)?;
    Ok(format!("{:x}", Sha256::digest(request)))
}

impl Handoff {
    fn read(state: &Path, job: &UpdateJob) -> Result<Option<Self>, UpdateError> {
        if job.phase != UpdatePhase::Installing {
            return Ok(None);
        }
        if uuid::Uuid::parse_str(&job.id).is_err() {
            return Err(invalid("Invalid update handoff job"));
        }
        let handoff: Self = match read_private(&state.join(artifact(&job.id)), JOB_STATE_LIMIT) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if handoff.job != job.id || !handoff.source.is_complete() {
            return Err(invalid(
                "The retained handoff belongs to a different update",
            ));
        }
        let slots: std::collections::BTreeSet<_> = handoff.windows.iter().collect();
        if slots.len() != handoff.windows.len() {
            return Err(invalid(
                "The approved handoff contains duplicate window slots",
            ));
        }
        for slot in slots {
            crate::instance::validate_window_slot(slot)?;
        }
        Ok(Some(handoff))
    }

    fn verify_target(&self, layout: &InstallLayout) -> Result<(), UpdateError> {
        if self.target != layout.version_root.join("bin/cagentsd")
            || installed::stamp(&layout.version_root)? != self.build
        {
            return Err(invalid("The approved replacement executable changed"));
        }
        Ok(())
    }

    fn census(&self, state: &Path) -> Result<recovery::Census, UpdateError> {
        let census = recovery::Census::load(&recovery::census_path(state, &self.job))?;
        if census.transaction != self.job
            || census.source.as_ref() != Some(&self.source)
            || census.digest()? != self.census_digest
            || request_digest(state, &self.job)? != self.request_digest
        {
            return Err(invalid(
                "The approved update request or original census changed",
            ));
        }
        Ok(census)
    }

    fn accepted_from(&self, mut retained: Handoff) -> Result<Option<Acceptance>, UpdateError> {
        let accepted = retained.accepted.take();
        if retained != *self {
            return Err(invalid("The accepted update handoff record changed"));
        }
        Ok(accepted)
    }
}

/// Persist the continuation before giving the target any startup authority.
pub(super) fn arm(
    state: &Path,
    job: &UpdateJob,
    target: &Path,
    recovery: &recovery::Context,
    guard: &UiUpdateGuard,
) -> Result<(), UpdateError> {
    let Some(census) = &recovery.census else {
        return Ok(());
    };
    let Some(checkpoint) = recovery::checkpoint_status()? else {
        return Ok(());
    };
    if census.source.as_ref() != Some(&checkpoint.proof)
        || !matches!(checkpoint.phase, Phase::Ready | Phase::Prepared)
    {
        return Err(invalid(
            "The retained checkpoint does not match this update handoff",
        ));
    }
    let layout = locate(target)?;
    let handoff = Handoff {
        job: job.id.clone(),
        worker: Process::read(std::process::id() as i32)?,
        finisher: None,
        request_digest: request_digest(state, &job.id)?,
        census_digest: census.digest()?,
        source: checkpoint.proof,
        checkpoint_digest: checkpoint.digest,
        checkpoint_phase: checkpoint.phase,
        target: target.to_owned(),
        build: installed::stamp(&layout.version_root)?,
        windows: guard.reopened_slots(),
        restore_policy: guard.restore_policy(),
        accepted: None,
    };
    match start_finisher(&layout.version_root.join("bin/cagents"), handoff) {
        Ok(armed) => write_state_file(state, &artifact(&job.id), &armed),
        Err(error) => {
            // An earlier target can still complete through the original worker.
            eprintln!("Could not arm the update finisher: {error}");
            Ok(())
        }
    }
}

const PLAN_TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);

fn start_finisher(cli: &Path, mut approved: Handoff) -> Result<Handoff, UpdateError> {
    // Use the worker's login and display context for the same inventory and
    // window startup calls it makes normally. Daemon children are hosted peers.
    let (mut coordinator, receiver) = UnixStream::pair()?;
    coordinator.set_read_timeout(Some(PLAN_TRANSFER_TIMEOUT))?;
    if unsafe { libc::fcntl(WORKER_LOCK_FD, libc::F_GETFD) } == -1 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: run_worker owns this reserved description for the entire handoff.
    let worker = unsafe { BorrowedFd::borrow_raw(WORKER_LOCK_FD) };
    let mut command = Command::new(cli);
    command
        .arg("finish-update-handoff")
        .stdin(Stdio::from(OwnedFd::from(receiver)))
        .stdout(Stdio::null())
        .process_group(0);
    crate::install_guard::sanitize_child_environment(&mut command);
    // SAFETY: the new process can keep the launcher's output and private
    // channel. The worker and launch fences must settle independently of it.
    unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
    let mut child = command.spawn()?;
    drop(command);
    let finisher = Process::read(child.id() as i32);
    thread::spawn(move || {
        let _ = child.wait();
    });
    approved.finisher = Some(finisher?);
    send_json_with_fds_until(
        &coordinator,
        &approved,
        &[worker],
        Instant::now() + PLAN_TRANSFER_TIMEOUT,
        JOB_STATE_LIMIT as usize,
        1,
        FrameByteOrder::LittleEndian,
    )
    .map_err(invalid)?;
    let mut ready = [0];
    coordinator.read_exact(&mut ready)?;
    if ready != [1] {
        return Err(invalid(
            "The installed update finisher did not accept the approved handoff",
        ));
    }
    Ok(approved)
}

/// Called before restoration, with the authenticated inherited runtime and
/// launch leases held. An ordinary restart has no authority to accept this job.
#[cfg(feature = "host")]
pub fn accept(serving: &HostSessionProof) -> Result<Option<String>, UpdateError> {
    let layout = match locate(&crate::process::current_executable()?) {
        Ok(layout) => layout,
        Err(UpdateError::UnsupportedInstallation) => return Ok(None),
        Err(error) => return Err(error),
    };
    let state = layout.prefix.join(".update");
    let Some(job) = read_job(&state)? else {
        return Ok(None);
    };
    if layout.version != job.version {
        return Ok(None);
    }
    let Some(mut handoff) = Handoff::read(&state, &job)? else {
        return Ok(None);
    };
    if handoff.accepted.is_some() || handoff.source.boot_id != serving.boot_id {
        return Ok(None);
    }
    handoff.verify_target(&layout)?;
    if fs::metadata("/proc/self/exe").map(|m| (m.dev(), m.ino()))? != handoff.build.daemon_inode() {
        return Err(invalid("The successor is not the approved daemon image"));
    }
    handoff.census(&state)?;
    recovery::checkpoint_status()?
        .filter(|status| {
            status.proof == handoff.source
                && status.digest == handoff.checkpoint_digest
                && status.phase == handoff.checkpoint_phase
        })
        .ok_or_else(|| invalid("The successor did not receive the approved checkpoint"))?;
    handoff.accepted = Some(Acceptance {
        proof: serving.clone(),
        process: Process::read(std::process::id() as i32)?,
    });
    write_state_file(&state, &artifact(&job.id), &handoff)?;
    Ok((handoff.checkpoint_phase == Phase::Ready).then_some(handoff.checkpoint_digest))
}

fn process_exited(expected: &Process, timeout: Duration) -> io::Result<bool> {
    let pin = match PinnedProcess::open_unreaped(expected.pid as u32) {
        Ok(pin) => pin,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    match Process::read(expected.pid) {
        Ok(current) if &current == expected => pin.wait_for_exit(timeout),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

/// Status and competing updates can obtain the worker lease ahead of the
/// successor. The startup fence covers the gap before it records acceptance.
pub(super) fn pending(state: &Path, job: &UpdateJob) -> Result<bool, UpdateError> {
    let Some(handoff) = Handoff::read(state, job)? else {
        return Ok(false);
    };
    let Some(finisher) = &handoff.finisher else {
        return Ok(false);
    };
    if handoff.source.boot_id != crate::daemon::read_boot_id()?
        || process_exited(finisher, Duration::ZERO)?
    {
        return Ok(false);
    }
    if handoff.accepted.is_none()
        && crate::instance::private_lock_is_held(&crate::instance::update_fence_path()?)?
    {
        return Ok(true);
    }
    // Acceptance is written before startup releases its fence. Read again
    // if the fence went away between the first observation and the probe.
    let accepted = match handoff.accepted {
        Some(accepted) => Some(accepted),
        None => Handoff::read(state, job)?.and_then(|handoff| handoff.accepted),
    };
    accepted.map_or(Ok(false), |accepted| {
        Ok(!process_exited(&accepted.process, Duration::ZERO)?)
    })
}

/// Hidden entry point. The worker must send its exact pre-transfer approval
/// over a private channel it created, while it still owns the worker lease.
pub(crate) fn run_finisher() -> Result<(), UpdateError> {
    let mut channel = UnixStream::from(io::stdin().as_fd().try_clone_to_owned()?);
    let (approved, descriptors): (Handoff, _) = receive_json_with_fds_until(
        &channel,
        Instant::now() + PLAN_TRANSFER_TIMEOUT,
        JOB_STATE_LIMIT as usize,
        1,
        FrameByteOrder::LittleEndian,
    )
    .map_err(invalid)?;
    let peer = crate::pinned_process::peer_credentials(&channel)?;
    let layout = locate(&crate::process::current_executable()?)?;
    let caller = PinnedProcess::open(peer.pid)?;
    let worker_executable = caller.executable()?;
    if peer.uid != crate::daemon::current_uid()
        || Process::read(peer.pid as i32)? != approved.worker
        || approved.finisher.as_ref() != Some(&Process::read(std::process::id() as i32)?)
        || approved.accepted.is_some()
        || worker_executable.file_name().and_then(|name| name.to_str()) != Some("cagents")
        || locate(&worker_executable)?.prefix != layout.prefix
    {
        return Err(invalid("The update finisher requires its original worker"));
    }
    let state = state_dir(&layout.prefix)?;
    let [worker]: [OwnedFd; 1] = descriptors
        .try_into()
        .map_err(|_| invalid("The update finisher requires the original worker lease"))?;
    let worker = File::from(worker);
    let actual = worker.metadata()?;
    let expected = fs::symlink_metadata(state.join("lock"))?;
    validate_lock(&worker)?;
    if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino())
        || !crate::flock::description_holds_exclusive_lock(&worker)?
    {
        return Err(invalid(
            "The update finisher received a different or unheld worker lease",
        ));
    }
    approved.verify_target(&layout)?;
    approved.census(&state)?;
    // Close the proof copy before acknowledging. The finisher waits on a
    // fresh description and must never keep the worker's flock alive itself.
    drop(worker);
    channel.write_all(&[1])?;
    drop(channel);
    let _worker = crate::instance::owned_file_lock(open_worker_lock(&state)?, libc::LOCK_EX)?;
    finish_under_lease(&layout, &state, &approved)
}

fn finish_under_lease(
    layout: &InstallLayout,
    state: &Path,
    approved: &Handoff,
) -> Result<(), UpdateError> {
    let Some(mut job) = read_job(state)? else {
        return Ok(());
    };
    if job.phase != UpdatePhase::Installing
        || job.id != approved.job
        || layout.version != job.version
        || approved.source.boot_id != crate::daemon::read_boot_id()?
        || !process_exited(&approved.worker, Duration::from_secs(1))?
    {
        return Ok(());
    }
    // The signed installer and parked daemon can outlive the worker while
    // holding the same launch description. Neither may race a reopen.
    let guard = UiUpdateGuard::acquire_after_updater()?;
    let Some(retained) = Handoff::read(state, &job)? else {
        return Ok(());
    };
    let result = match approved.accepted_from(retained) {
        Ok(Some(accepted)) => finish_windows(layout, state, &job, approved, &accepted, guard),
        Ok(None) => return Ok(()),
        Err(error) => Err(error),
    };
    job.confirmation = None;
    job.refusal = result.as_ref().err().and_then(UpdateError::refusal);
    job.message = completion_message(&result, &job, false);
    job.phase = if result.is_ok() {
        UpdatePhase::Complete
    } else {
        UpdatePhase::Failed
    };
    write_job(state, &job)?;
    retention::remember_success(state, &job.id, &job.phase);
    result.map(drop)
}

fn finish_windows(
    layout: &InstallLayout,
    state: &Path,
    job: &UpdateJob,
    handoff: &Handoff,
    accepted: &Acceptance,
    mut guard: UiUpdateGuard,
) -> Result<Completion, UpdateError> {
    let serving = &accepted.proof;
    let verify_target = || {
        handoff.verify_target(layout)?;
        if served_version_root(layout)? != layout.version_root
            || serving.boot_id != handoff.source.boot_id
        {
            return Err(invalid("The approved replacement is no longer selected"));
        }
        let client = source::Client::connect_matched(&crate::terminal_ipc::daemon_socket_path()?)
            .map_err(|error| invalid(error.to_string()))?;
        if client.proof() != serving
            || client.peer_pid() as i32 != accepted.process.pid
            || client.serving_executable()? != handoff.target
            || fs::metadata(format!("/proc/{}/exe", accepted.process.pid))
                .map(|m| (m.dev(), m.ino()))?
                != handoff.build.daemon_inode()
            || !accepted.process.is_current()
        {
            return Err(invalid(
                "The serving process is not the successor that accepted the handoff",
            ));
        }
        Ok(())
    };
    let census = handoff.census(state)?;
    installed::confirm_daemon_ready(
        &handoff.target,
        Some(accepted.process.pid as u32),
        crate::install_guard::daemon_start_timeout()?,
    )?;
    verify_target()?;
    if handoff.checkpoint_phase == Phase::Prepared {
        let store = crate::upgrade::handoff::Store::existing()
            .map_err(invalid)?
            .ok_or_else(|| invalid("The approved Prepared recovery is missing"))?;
        let recovered = crate::upgrade::recovery::pending_retirement_for_source(
            &store.path,
            &handoff.target,
            &handoff.source,
        )
        .map_err(invalid)?;
        if !recovered.is_some_and(|record| {
            record.job == handoff.job
                && record.witness.checkpoint_hash == handoff.checkpoint_digest
                && record.windows == handoff.windows
        }) {
            return Err(invalid(
                "The successor did not settle the approved Prepared recovery",
            ));
        }
    }
    let verify_inventory = || {
        let receipt = census.verify(source::BusyPatience::for_one_run())?;
        if !receipt.verifies_replacement(&handoff.source, serving) {
            return Err(receipt.failure());
        }
        Ok(receipt)
    };
    verify_inventory()?;

    guard.set_restore_policy(handoff.restore_policy);
    let ui = layout.version_root.join("bin/cagents-ui");
    let launched = guard.relaunch_missing_windows_with_notes(&handoff.windows, &ui);
    let guard = UiUpdateGuard::acquire_for_cutover()?;
    // The previous worker or an ordinary launch may already have taken
    // a slot. The matched window must confirm its workspace finished opening.
    let verified = guard.verify_cutover_windows(&handoff.windows, &ui);
    let pane_notes = match (launched, verified) {
        (Ok(notes), Ok(())) => notes,
        (Err(_), Ok(())) => None,
        (Err(error), Err(_)) | (Ok(_), Err(error)) => {
            return Err(windows::reopen_failure(state, job, error));
        }
    };

    recovery::retire_current_service(source::BusyPatience::for_one_run())?;
    let receipt = verify_inventory()?;
    verify_target()?;
    receipt.save(&recovery::outcome_path(state, &handoff.job))?;
    Ok(Completion {
        handoff: super::Handoff::Served,
        pane_notes,
    })
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;
    use crate::model::{HostIncarnation, HostSessionId};

    fn proof(id: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new(id),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new(id),
        }
    }

    fn saved_handoff(state: &Path, worker: Process) -> (UpdateJob, Handoff) {
        let id = uuid::Uuid::new_v4().to_string();
        let candidate = state.join("versions/0.2.4");
        fs::create_dir_all(candidate.join("bin")).unwrap();
        for executable in ["cagentsd", "cagents", "cagents-ui"] {
            fs::write(candidate.join("bin").join(executable), b"fixture").unwrap();
        }
        let source = proof("original");
        let census = serde_json::json!({
            "version": 1, "transaction": id, "source": source, "terminals": [], "chats": [],
        });
        write_state_file(state, &format!("{id}.census.json"), &census).unwrap();
        write_state_file(state, &format!("{id}.request"), &"reviewed request").unwrap();
        let handoff = Handoff {
            job: id.clone(),
            worker,
            finisher: Some(Process::read(std::process::id() as i32).unwrap()),
            request_digest: request_digest(state, &id).unwrap(),
            census_digest: recovery::Census::load(&recovery::census_path(state, &id))
                .unwrap()
                .digest()
                .unwrap(),
            source,
            checkpoint_digest: "a".repeat(64),
            checkpoint_phase: Phase::Ready,
            target: candidate.join("bin/cagentsd"),
            build: installed::stamp(&candidate).unwrap(),
            windows: vec!["primary".into(), "second".into()],
            restore_policy: WindowRestorePolicy::Ordinary,
            accepted: Some(Acceptance {
                proof: proof("successor"),
                process: Process::read(std::process::id() as i32).unwrap(),
            }),
        };
        let job = UpdateJob {
            id,
            version: "0.2.4".into(),
            phase: UpdatePhase::Installing,
            message: "Starting the installed application".into(),
            log_path: state.join("update.log"),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        (job, handoff)
    }

    #[test]
    fn the_original_worker_is_proven_exited_before_it_is_reaped() {
        let mut worker = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let epoch = Process::read(worker.id() as i32).unwrap();
        let alive = !process_exited(&epoch, Duration::ZERO).unwrap();
        // The pidfd proves exit even before the parent reaps the original PID.
        worker.kill().unwrap();
        let exited = process_exited(&epoch, Duration::from_secs(2)).unwrap();
        worker.wait().unwrap();
        assert!(alive && exited);

        assert!(process_exited(&epoch, Duration::ZERO).unwrap());
    }

    #[test]
    fn a_worker_completed_job_never_reopens_its_slots_again() {
        let state = tempfile::tempdir().unwrap();
        let worker = Process::read(std::process::id() as i32).unwrap();
        let (mut job, handoff) = saved_handoff(state.path(), worker);
        write_state_file(state.path(), &artifact(&job.id), &handoff).unwrap();
        assert!(pending(state.path(), &job).unwrap());
        job.phase = UpdatePhase::Complete;
        assert!(!pending(state.path(), &job).unwrap());
        assert!(Handoff::read(state.path(), &job).unwrap().is_none());
    }

    #[test]
    fn continuation_rejects_changed_original_evidence() {
        let state = tempfile::tempdir().unwrap();
        let worker = Process::read(std::process::id() as i32).unwrap();
        let (job, mut handoff) = saved_handoff(state.path(), worker);
        assert!(handoff.census(state.path()).is_ok());
        let original = std::mem::replace(&mut handoff.census_digest, "b".repeat(64));
        assert!(handoff.census(state.path()).is_err());
        handoff.census_digest = original;
        write_state_file(state.path(), &format!("{}.request", job.id), &"changed").unwrap();
        assert!(handoff.census(state.path()).is_err());
    }

    #[test]
    fn the_accepted_window_roster_cannot_be_replaced() {
        let state = tempfile::tempdir().unwrap();
        let worker = Process::read(std::process::id() as i32).unwrap();
        let (job, mut approved) = saved_handoff(state.path(), worker);
        write_state_file(state.path(), &artifact(&job.id), &approved).unwrap();
        approved.accepted = None;
        let accepted = Handoff::read(state.path(), &job).unwrap().unwrap();
        assert!(approved.accepted_from(accepted).unwrap().is_some());
        let mut changed = Handoff::read(state.path(), &job).unwrap().unwrap();
        changed.windows.push("unapproved".into());
        assert!(approved.accepted_from(changed).is_err());
    }

    #[test]
    fn a_dead_finisher_does_not_keep_an_accepted_job_pending() {
        let state = tempfile::tempdir().unwrap();
        let worker = Process::read(std::process::id() as i32).unwrap();
        let (job, mut handoff) = saved_handoff(state.path(), worker);
        handoff.finisher.as_mut().unwrap().start += 1;
        write_state_file(state.path(), &artifact(&job.id), &handoff).unwrap();
        assert!(!pending(state.path(), &job).unwrap());
    }
}
