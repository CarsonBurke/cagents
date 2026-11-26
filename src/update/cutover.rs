//! Finish an installed Ready update after its coordinator releases custody.
//! This record can admit saved windows to the same successor. It cannot start
//! a daemon or authorize another checkpoint restore.
use super::*;
use crate::model::HostSessionProof;
#[cfg(feature = "host")]
use crate::terminal_ipc::StartupState;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    job: String,
    request_digest: String,
    checkpoint_digest: String,
    target: PathBuf,
    target_image: (u64, u64),
    window_image: (u64, u64),
    source: HostSessionProof,
    successor: HostSessionProof,
    process: crate::daemon::holder_registry::Process,
    windows: Vec<String>,
    stage: Stage,
}

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Pending,
    Deferred,
}

fn path(state: &Path, job: &str) -> PathBuf {
    state.join(format!("{job}.cutover.json"))
}

pub(super) fn read_job(state: &Path) -> Result<Option<UpdateJob>, UpdateError> {
    match read_private(&state.join("status.json"), JOB_STATE_LIMIT) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read(state: &Path, job: &str) -> Result<Option<Intent>, UpdateError> {
    if uuid::Uuid::parse_str(job).is_err() {
        return Err(invalid("Invalid cutover job"));
    }
    match read_private(&path(state, job), JOB_STATE_LIMIT) {
        Ok(bytes) => {
            let intent: Intent = serde_json::from_slice(&bytes)?;
            intent.validate()?;
            if intent.job != job {
                return Err(invalid("Installed cutover belongs to a different job"));
            }
            Ok(Some(intent))
        }
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

impl Intent {
    fn validate(&self) -> Result<(), UpdateError> {
        let hash = |text: &str| text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit());
        if uuid::Uuid::parse_str(&self.job).is_err()
            || !hash(&self.request_digest)
            || !hash(&self.checkpoint_digest)
            || !self.target.is_absolute()
            || !self.source.is_complete()
            || !self.successor.is_complete()
            || self.source == self.successor
            || self.source.boot_id != self.successor.boot_id
            || self.process.pid <= 1
            || self.process.start == 0
            || self.windows.len() > 1024
            || self
                .windows
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.windows.len()
        {
            return Err(invalid("Invalid installed cutover evidence"));
        }
        for slot in &self.windows {
            crate::instance::validate_window_slot(slot)?;
        }
        Ok(())
    }

    #[cfg(feature = "host")]
    fn verify_request(&self, state: &Path) -> Result<(), UpdateError> {
        let request = read_private(
            &state.join(format!("{}.request", self.job)),
            SMALL_ASSET_LIMIT,
        )?;
        let digest = Sha256::digest(request);
        if format!("{digest:x}") != self.request_digest {
            return Err(invalid("Installed cutover request changed"));
        }
        Ok(())
    }

    fn names_selected(&self, layout: &InstallLayout) -> Result<bool, UpdateError> {
        let daemon = layout.version_root.join("bin/cagentsd");
        if self.target != daemon || served_version_root(layout)? != layout.version_root {
            return Ok(false);
        }
        let named = fs::symlink_metadata(daemon)?;
        let ui = fs::symlink_metadata(layout.version_root.join("bin/cagents-ui"))?;
        Ok(named.is_file()
            && (named.dev(), named.ino()) == self.target_image
            && ui.is_file()
            && (ui.dev(), ui.ino()) == self.window_image)
    }

    #[cfg(feature = "host")]
    fn verify_serving(&self, layout: &InstallLayout) -> Result<(), UpdateError> {
        if !self.names_selected(layout)? {
            return Err(invalid("The restored build is no longer selected"));
        }
        let mut client =
            source::Client::connect_matched(&crate::terminal_ipc::daemon_socket_path()?)
                .map_err(|error| invalid(error.to_string()))?;
        if client.proof() != &self.successor
            || client.serving_executable()? != self.target
            || client.startup_state()? != StartupState::Ready
        {
            return Err(invalid(
                "The selected service no longer owns the restored work",
            ));
        }
        Ok(())
    }

    #[cfg(feature = "host")]
    fn persist(&self, state: &Path) -> Result<(), UpdateError> {
        let temporary = state.join(format!(".cutover-{}", uuid::Uuid::new_v4()));
        let saved = (|| {
            let mut file = private_create(&temporary)?;
            serde_json::to_writer(&mut file, self)?;
            file.sync_all()?;
            fs::rename(&temporary, path(state, &self.job))?;
            File::open(state)?.sync_all()?;
            Ok(())
        })();
        if saved.is_err() {
            let _ = fs::remove_file(temporary);
        }
        saved
    }
}

/// Written with the transferred worker lease and launch fence held, after
/// verifying all original owners. The selector remains the commit point.
#[cfg(feature = "host")]
pub(crate) fn record(
    job: &str,
    request_digest: &str,
    checkpoint_digest: &str,
    target: &Path,
    source: &HostSessionProof,
    successor: &HostSessionProof,
    windows: &[String],
) -> Result<(), UpdateError> {
    let layout = locate(target)?;
    let state = state_dir(&layout.prefix)?;
    let expected = read_job(&state)?.ok_or_else(|| invalid("The cutover job is missing"))?;
    if expected.id != job
        || expected.version != layout.version
        || expected.phase != UpdatePhase::Installing
    {
        return Err(invalid(
            "The installed cutover no longer belongs to this update",
        ));
    }
    let target_image = crate::process::mapped_region_identity(
        std::process::id(),
        crate::process::own_code_address(),
    )?;
    let ui = fs::symlink_metadata(layout.version_root.join("bin/cagents-ui"))?;
    let intent = Intent {
        job: job.into(),
        request_digest: request_digest.into(),
        checkpoint_digest: checkpoint_digest.into(),
        target: target.into(),
        target_image,
        window_image: (ui.dev(), ui.ino()),
        source: source.clone(),
        successor: successor.clone(),
        process: crate::daemon::holder_registry::Process::read(std::process::id() as i32)?,
        windows: windows.to_vec(),
        stage: Stage::Pending,
    };
    intent.validate()?;
    intent.verify_request(&state)?;
    intent.persist(&state)
}

/// An unrelated job must not replace the unfinished status between the
/// coordinator releasing its lease and this successor acquiring its own.
pub(super) fn pending_selected(
    layout: &InstallLayout,
    state: &Path,
    job: &UpdateJob,
) -> Result<bool, UpdateError> {
    if !matches!(job.phase, UpdatePhase::Installing | UpdatePhase::Failed)
        || layout.version != job.version
    {
        return Ok(false);
    }
    let Some(intent) = read(state, &job.id)? else {
        return Ok(false);
    };
    if intent.stage != Stage::Pending
        || !intent.names_selected(layout)?
        || crate::daemon::read_boot_id()? != intent.successor.boot_id
    {
        return Ok(false);
    }
    // Kernel identity is available during socket contention and before the
    // listener starts accepting. A transient query error cannot authorize an
    // unrelated job to replace this job's saved-window scope.
    match crate::daemon::holder_registry::Process::read(intent.process.pid) {
        Ok(current) => Ok(current == intent.process),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(feature = "host")]
pub(super) fn finish(
    layout: &InstallLayout,
    state: &Path,
    successor: &HostSessionProof,
    mut job: UpdateJob,
) -> Result<Option<String>, UpdateError> {
    if !matches!(job.phase, UpdatePhase::Installing | UpdatePhase::Failed)
        || layout.version != job.version
    {
        return Ok(None);
    }
    let Some(mut intent) = read(state, &job.id)? else {
        return Ok(None);
    };
    if intent.stage != Stage::Pending
        || &intent.successor != successor
        || intent.process.pid != std::process::id() as i32
        || !intent.names_selected(layout)?
    {
        return Ok(None);
    }
    let finished = finish_windows(&intent, layout, state);
    job.confirmation = None;
    job.refusal = None;
    match finished {
        Ok(()) => {
            job.phase = UpdatePhase::Complete;
            job.message = "Update installed; saved windows reopened".into();
        }
        Err(ref error) => {
            job.phase = UpdatePhase::Failed;
            job.message = format!(
                "The new build is installed, but reopening its saved windows could not be confirmed. {error}"
            );
        }
    }
    write_job(state, &job)?;
    if finished.is_err() {
        intent.stage = Stage::Deferred;
        intent.persist(state)?;
    }
    retention::remember_success(state, &job.id, &job.phase);
    finished.map(|()| Some(job.id))
}

#[cfg(feature = "host")]
fn finish_windows(
    intent: &Intent,
    layout: &InstallLayout,
    state: &Path,
) -> Result<(), UpdateError> {
    intent.verify_request(state)?;
    intent.verify_serving(layout)?;
    let ui = layout.version_root.join("bin/cagents-ui");
    if !intent.windows.is_empty() {
        let mut guard = crate::instance::UiUpdateGuard::acquire_for_cutover()?;
        guard.set_recovery_restore_only();
        let launched = guard.relaunch_missing_windows(&intent.windows, &ui);
        // A natural launch can legitimately win a slot after we release the
        // fence. An authenticated Ready owner settles that slot either way.
        let verified = crate::instance::UiUpdateGuard::acquire_for_cutover()
            .and_then(|guard| guard.verify_cutover_windows(&intent.windows, &ui));
        if verified.is_err() {
            launched?;
            verified?;
        }
    }
    intent.verify_serving(layout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> Intent {
        let source = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("original-ready"),
            boot_id: crate::model::BootId::new("same-boot"),
            host_incarnation: crate::model::HostIncarnation::new("original"),
        };
        Intent {
            job: uuid::Uuid::new_v4().to_string(),
            request_digest: "a".repeat(64),
            checkpoint_digest: "b".repeat(64),
            target: "/private/install/versions/0.2.4/bin/cagentsd".into(),
            target_image: (42, 12),
            window_image: (42, 14),
            successor: HostSessionProof {
                host_incarnation: crate::model::HostIncarnation::new("replacement"),
                ..source.clone()
            },
            source,
            process: crate::daemon::holder_registry::Process { pid: 42, start: 17 },
            windows: vec!["main".into()],
            stage: Stage::Pending,
        }
    }

    #[test]
    fn selector_intent_requires_the_exact_same_boot_scope_and_unique_windows() {
        let mut intent = intent();
        intent.validate().unwrap();
        intent.windows.push("main".into());
        assert!(intent.validate().is_err());
        *intent.windows.last_mut().unwrap() = "../main".into();
        assert!(intent.validate().is_err());
        intent.windows.pop();
        intent.successor = intent.source.clone();
        assert!(intent.validate().is_err());
        intent.successor.boot_id = crate::model::BootId::new("another-boot");
        assert!(intent.validate().is_err());
    }

    #[test]
    fn a_live_selected_intent_reserves_its_job_even_after_a_worker_failure() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().canonicalize().unwrap();
        let version = "0.2.4";
        let version_root = prefix.join("versions").join(version);
        fs::create_dir_all(version_root.join("bin")).unwrap();
        fs::write(
            prefix.join(".cagents-install-root"),
            "cagents install root\n",
        )
        .unwrap();
        for name in ["cagents", "cagentsd", "cagents-ui"] {
            File::create(version_root.join("bin").join(name)).unwrap();
        }
        std::os::unix::fs::symlink(Path::new("versions").join(version), prefix.join("current"))
            .unwrap();
        let layout = locate(&version_root.join("bin/cagentsd")).unwrap();
        let state = state_dir(&prefix).unwrap();
        let mut intent = intent();
        intent.target = version_root.join("bin/cagentsd");
        let daemon = fs::metadata(&intent.target).unwrap();
        intent.target_image = (daemon.dev(), daemon.ino());
        let window = fs::metadata(version_root.join("bin/cagents-ui")).unwrap();
        intent.window_image = (window.dev(), window.ino());
        intent.process =
            crate::daemon::holder_registry::Process::read(std::process::id() as i32).unwrap();
        intent.successor.boot_id = crate::daemon::read_boot_id().unwrap();
        intent.source.boot_id = intent.successor.boot_id.clone();
        let job = UpdateJob {
            id: intent.job.clone(),
            version: version.into(),
            phase: UpdatePhase::Failed,
            message: "Worker exited after selection".into(),
            log_path: state.join("update.log"),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        let record = path(&state, &job.id);
        private_create(&record).unwrap();
        let save = |intent: &Intent| {
            fs::write(&record, serde_json::to_vec(intent).unwrap()).unwrap();
        };
        save(&intent);
        assert!(pending_selected(&layout, &state, &job).unwrap());
        intent.stage = Stage::Deferred;
        save(&intent);
        assert!(!pending_selected(&layout, &state, &job).unwrap());
        intent.stage = Stage::Pending;
        intent.process.start += 1;
        save(&intent);
        assert!(!pending_selected(&layout, &state, &job).unwrap());
    }
}
