//! Finish an approved Linux handoff from the daemon that accepted it.
use super::*;
use crate::{
    daemon::holder_registry::Process,
    instance::{UiUpdateGuard, WindowRestorePolicy},
    model::HostSessionProof,
    pinned_process::PinnedProcess,
    upgrade::handoff::Phase,
};

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Handoff {
    job: String,
    worker: Process,
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

/// The exact approved record this process saw before restoring work.
#[cfg(feature = "host")]
pub struct AcceptedHandoff(Handoff);

#[cfg(feature = "host")]
impl AcceptedHandoff {
    pub fn ready_checkpoint(&self) -> Option<&str> {
        (self.0.checkpoint_phase == Phase::Ready).then_some(&self.0.checkpoint_digest)
    }
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

    #[cfg(feature = "host")]
    fn verify_target(&self, layout: &InstallLayout) -> Result<(), UpdateError> {
        if self.target != layout.version_root.join("bin/cagentsd")
            || installed::stamp(&layout.version_root)? != self.build
            || fs::metadata("/proc/self/exe").map(|m| (m.dev(), m.ino()))?
                != self.build.daemon_inode()
        {
            return Err(invalid("The approved replacement executable changed"));
        }
        Ok(())
    }

    #[cfg(feature = "host")]
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

    #[cfg(feature = "host")]
    fn can_finish(&self, serving: &HostSessionProof) -> Result<bool, UpdateError> {
        let exact_successor = serving.boot_id == self.source.boot_id
            && self.accepted.as_ref().is_some_and(|accepted| {
                &accepted.proof == serving
                    && accepted.process.pid == std::process::id() as i32
                    && accepted.process.is_current()
            });
        Ok(exact_successor && process_exited(&self.worker, Duration::from_secs(1))?)
    }
}

/// Persist the continuation before giving the target any startup authority.
pub(super) fn record(
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
    write_state_file(state, &artifact(&job.id), &handoff)
}

/// Called before restoration, with the authenticated inherited runtime and
/// launch leases held. An ordinary restart has no authority to accept this job.
#[cfg(feature = "host")]
pub fn accept(serving: &HostSessionProof) -> Result<Option<AcceptedHandoff>, UpdateError> {
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
    Ok(Some(AcceptedHandoff(handoff)))
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
    if handoff.source.boot_id != crate::daemon::read_boot_id()? {
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

#[cfg(feature = "host")]
impl AcceptedHandoff {
    /// Runs under the worker lease. Only this exact daemon may reopen slots.
    pub(super) fn finish(
        &self,
        layout: &InstallLayout,
        state: &Path,
        serving: &HostSessionProof,
        job: &UpdateJob,
    ) -> Result<Option<String>, UpdateError> {
        if job.phase != UpdatePhase::Installing
            || job.id != self.0.job
            || layout.version != job.version
            || !self.0.can_finish(serving)?
        {
            return Ok(None);
        }
        let result = (|| {
            if Handoff::read(state, job)?.as_ref() != Some(&self.0) {
                return Err(invalid("The accepted update handoff record changed"));
            }
            finish_windows(layout, state, serving, &self.0)
        })();
        let mut job = job.clone();
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
        result.map(|_| Some(job.id))
    }
}

#[cfg(feature = "host")]
fn finish_windows(
    layout: &InstallLayout,
    state: &Path,
    serving: &HostSessionProof,
    handoff: &Handoff,
) -> Result<Completion, UpdateError> {
    // A release installer may still be running after its parent was killed.
    // Wait until it drops the inherited launch fence before reading current
    // or reopening a window, even for a job that saved zero windows.
    let mut guard = UiUpdateGuard::acquire_after_updater()?;
    let verify_target = || {
        handoff.verify_target(layout)?;
        if served_version_root(layout)? != layout.version_root {
            return Err(invalid("The approved replacement is no longer selected"));
        }
        source::Peer::confirm_serving(
            &crate::terminal_ipc::daemon_socket_path()?,
            &handoff.target,
            serving,
        )
    };
    let census = handoff.census(state)?;
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
        (Err(error), Err(_)) | (Ok(_), Err(error)) => return Err(error.into()),
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
    fn only_the_exact_successor_can_continue_after_worker_exit() {
        let state = tempfile::tempdir().unwrap();
        let mut worker = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let epoch = Process::read(worker.id() as i32).unwrap();
        let alive = !process_exited(&epoch, Duration::ZERO).unwrap();
        // The pidfd proves exit even before the parent reaps the original PID.
        worker.kill().unwrap();
        let exited = process_exited(&epoch, Duration::from_secs(2)).unwrap();
        worker.wait().unwrap();
        assert!(alive && exited);

        let (_, mut handoff) = saved_handoff(state.path(), epoch);
        let successor = proof("successor");
        assert!(handoff.can_finish(&successor).unwrap());
        assert!(!handoff.can_finish(&proof("another-successor")).unwrap());
        handoff.accepted.as_mut().unwrap().process.start += 1;
        assert!(!handoff.can_finish(&successor).unwrap());
        handoff.accepted = None;
        assert!(!handoff.can_finish(&successor).unwrap());
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
        let mut worker = Command::new("/bin/true").spawn().unwrap();
        let epoch = Process::read(worker.id() as i32).unwrap();
        worker.wait().unwrap();
        let (job, handoff) = saved_handoff(state.path(), epoch);
        let target = handoff.target.parent().unwrap().parent().unwrap();
        let layout = InstallLayout {
            prefix: state.path().to_owned(),
            version_root: target.to_owned(),
            version: job.version.clone(),
        };
        let successor = proof("successor");
        let mut changed: Handoff =
            serde_json::from_value(serde_json::to_value(&handoff).unwrap()).unwrap();
        changed.windows.push("unapproved".into());
        write_state_file(state.path(), &artifact(&job.id), &changed).unwrap();
        let _lease = acquire_lock(state.path()).unwrap();
        let error = AcceptedHandoff(handoff)
            .finish(&layout, state.path(), &successor, &job)
            .unwrap_err();
        assert!(error.to_string().contains("handoff record changed"));
        assert_eq!(
            read_job(state.path()).unwrap().unwrap().phase,
            UpdatePhase::Failed
        );
    }
}
