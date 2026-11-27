//! Durable inputs for the installed updater's exact Ready activation.
use super::*;
use crate::upgrade::recovery::{ReadyPlanEvidence, Receipt, STANDBY_RECEIPT_LIMIT};

const LIMIT: u64 = STANDBY_RECEIPT_LIMIT as u64 + JOB_STATE_LIMIT;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan<'a> {
    version: u16,
    target: PathBuf,
    selection: installed::Selection,
    stamp: installed::Stamp,
    binaries: String,
    ready: ReadyPlanEvidence<'a>,
}

pub(super) struct Recorded {
    path: PathBuf,
    digest: [u8; 32],
}

impl Recorded {
    pub(super) fn unchanged(&self) -> Result<bool, UpdateError> {
        let bytes = read(&self.path)?
            .ok_or_else(|| invalid("The original Ready activation plan disappeared"))?;
        Ok(Sha256::digest(&bytes)[..] == self.digest)
    }
}

pub(super) fn record(
    state: &Path,
    job: &UpdateJob,
    receipt: &Receipt,
    target: &Path,
    selection: &installed::Selection,
    stamp: &installed::Stamp,
    binaries: &str,
) -> Result<Recorded, UpdateError> {
    let bytes = serde_json::to_vec(&Plan {
        version: 1,
        target: target.to_owned(),
        selection: selection.clone(),
        stamp: stamp.clone(),
        binaries: binaries.into(),
        ready: receipt.ready_plan_evidence().map_err(invalid)?,
    })?;
    if bytes.len() as u64 > LIMIT {
        return Err(invalid("The Ready activation plan exceeds its size limit"));
    }
    let path = path(state, &job.id)?;
    cutover::persist(state, &path, &bytes, true)?;
    Ok(Recorded {
        path,
        digest: Sha256::digest(&bytes).into(),
    })
}

fn path(state: &Path, job: &str) -> Result<PathBuf, UpdateError> {
    if uuid::Uuid::parse_str(job).is_ok_and(|id| id.to_string() == job) {
        Ok(state.join(format!("{job}.ready-plan.json")))
    } else {
        Err(invalid("Invalid Ready activation job"))
    }
}

fn read(path: &Path) -> Result<Option<Vec<u8>>, UpdateError> {
    let (Some(directory), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(invalid(
            "The Ready activation plan has no private file location",
        ));
    };
    let root = crate::fs_safety::Root::open(directory)?;
    let file = match crate::fs_safety::open_beneath(&root, Path::new(name)) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(invalid(error.to_string())),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() > LIMIT
    {
        return Err(invalid("The Ready activation plan is not a private file"));
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(invalid("The Ready activation plan exceeds its size limit"));
    }
    Ok(Some(bytes))
}

fn load(state: &Path, job: &UpdateJob) -> Result<Option<Plan<'static>>, UpdateError> {
    if !matches!(job.phase, UpdatePhase::Installing | UpdatePhase::Failed) {
        return Ok(None);
    }
    let Some(bytes) = read(&path(state, &job.id)?)? else {
        return Ok(None);
    };
    let plan: Plan<'static> = serde_json::from_slice(&bytes)?;
    if plan.version != 1 {
        return Err(invalid("Unsupported Ready activation plan"));
    }
    Ok(Some(plan))
}

impl Plan<'_> {
    fn target_layout(&self, prefix: &Path, job: &UpdateJob) -> Result<InstallLayout, UpdateError> {
        let layout = locate(&self.target.join("bin/cagents"))?;
        if layout.prefix != prefix
            || layout.version_root != self.target
            || layout.version != job.version
            || self.selection != installed::Selection::capture(&layout)?
            || self.stamp != installed::stamp(&self.target)?
        {
            return Err(invalid(
                "The approved installation changed while the update was interrupted",
            ));
        }
        Ok(layout)
    }

    fn verify_binaries(&self, state: &Path, job: &UpdateJob) -> Result<(), UpdateError> {
        let bytes = read_private(
            &state.join(format!("{}.request", job.id)),
            SMALL_ASSET_LIMIT,
        )?;
        let candidate = match serde_json::from_slice(&bytes)? {
            UpdateRequest::Installed { ref root }
            | UpdateRequest::InstalledFromTerminal { ref root }
                if *root == self.target =>
            {
                installed::Candidate::Local(&self.target).digest()?
            }
            UpdateRequest::Release(ref release)
            | UpdateRequest::ReleaseFromTerminal(ref release)
                if release.version == job.version =>
            {
                release_stage::verify_interrupted_candidate(
                    state,
                    job,
                    &self.target,
                    self.selection.previous_link(),
                    &Sha256::digest(&bytes).into(),
                )?
            }
            _ => {
                return Err(invalid(
                    "The interrupted update request names another candidate",
                ));
            }
        };
        if candidate != self.binaries || installed::stamp(&self.target)? != self.stamp {
            return Err(invalid(
                "The approved build changed while the update was interrupted",
            ));
        }
        Ok(())
    }

    fn selection_is_untouched(&self, layout: &InstallLayout) -> Result<bool, UpdateError> {
        Ok(installed::Selection::capture(layout)? == self.selection)
    }
}

pub(super) fn recover_worker(
    installation: &Installation,
    state: &Path,
    job: &mut UpdateJob,
    worker: &File,
) -> Result<(), UpdateError> {
    let plan =
        load(state, job)?.ok_or_else(|| invalid("This update has no Ready activation plan"))?;
    let target = plan.target_layout(&installation.prefix, job)?;
    if target.version != installation.version {
        return Err(invalid(
            "The interrupted update needs its original staged build",
        ));
    }
    let executable = fs::symlink_metadata(target.version_root.join("bin/cagents"))?;
    if crate::process::mapped_region_identity(
        std::process::id(),
        recover_worker as *const () as usize as u64,
    )? != (executable.dev(), executable.ino())
    {
        return Err(invalid(
            "The interrupted update worker no longer matches the approved CLI",
        ));
    }
    let mut guard = crate::instance::UiUpdateGuard::acquire()?;
    if guard.has_running_windows()? {
        return Err(invalid(
            "A window reopened before the approved update could resume",
        ));
    }
    let daemon = target.version_root.join("bin/cagentsd");
    let publication = crate::install_guard::reserve_darwin_publication()?;
    plan.verify_binaries(state, job)?;
    if !plan.selection_is_untouched(&target)? {
        return Err(invalid(
            "The installed selector changed before the update could resume",
        ));
    }
    let paths =
        crate::daemon::RuntimePaths::discover().map_err(|error| invalid(error.to_string()))?;
    let handoff = crate::daemon::lease_handoff::LeaseHandoff::try_acquire(&paths)
        .map_err(|error| invalid(error.to_string()))?
        .ok_or_else(|| invalid("The terminal service already has an owner"))?;
    match crate::descriptor::connect_settled(&paths.socket) {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => return Err(error.into()),
        Ok(_) => return Err(invalid("The terminal service is already serving")),
    }
    let receipt = plan
        .ready
        .reopen(&daemon, worker, &guard, &handoff)
        .map_err(invalid)?;
    let mut standby = crate::daemon::standby::Parked::launch_with_custody(
        &daemon,
        plan.stamp.daemon_inode(),
        &guard,
        plan.selection.previous_link(),
        publication,
    )?;
    set_phase(
        state,
        job,
        UpdatePhase::Installing,
        "Finishing the approved update",
    )?;
    let timeout = crate::install_guard::daemon_start_timeout()?;
    let started = standby.activate(&daemon, Some(&handoff), Some(&receipt), timeout, || {
        let _ = installed::report_overdue_startup(state, job);
    });
    if standby.may_own_runtime() {
        guard.leave_launch_fence_with_successor();
    } else {
        started?;
    }
    // A lost acknowledgement can race the exact successor becoming Ready.
    // Its verified listener settles the transfer and its own launcher
    // completes the original job after our final fence copies close.
    installed::confirm_daemon_ready(&daemon, Some(standby.pid()), timeout)?;
    installed::selected_candidate(&installation.prefix, &target.version_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_requires_the_original_private_plan_contents() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("ready-plan.json");
        let bytes = b"exact Ready plan";
        cutover::persist(state.path(), &path, bytes, true).unwrap();
        let recorded = Recorded {
            path: path.clone(),
            digest: Sha256::digest(bytes).into(),
        };
        assert!(recorded.unchanged().unwrap());
        fs::write(&path, b"changed Ready plan").unwrap();
        assert!(!recorded.unchanged().unwrap());

        let alias = state.path().join("alias");
        fs::hard_link(&path, &alias).unwrap();
        assert!(recorded.unchanged().is_err());
        fs::remove_file(path).unwrap();
        let name = std::ffi::CString::new(recorded.path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the pathname is NUL-terminated and refers to a private fixture directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(recorded.unchanged().is_err());
    }
}
