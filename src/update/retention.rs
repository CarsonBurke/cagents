//! Retains recent verified update jobs and the exact inputs of unfinished jobs.
use super::*;
use crate::fs_safety::{self, Root};
use std::{collections::BTreeSet, ffi::OsStr, time::SystemTime};

const KEEP: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const COMPLETE: &str = ".complete";
#[cfg(any(target_os = "macos", test))]
pub(super) const RELEASE_STAGE_PREFIX: &str = ".release-stage-";
#[cfg(any(target_os = "macos", test))]
pub(super) const RELEASE_STAGE_RANDOM_BYTES: usize = 6;

#[cfg(any(target_os = "macos", test))]
pub(super) fn extraction_lease_path(stage: &Path) -> PathBuf {
    stage.with_extension("lock")
}

/// The installer receives an empty directory. Keep its lease beside the
/// stage and bind it to that exact directory before any data is extracted.
#[cfg(any(target_os = "macos", test))]
pub(super) fn lease_extraction(stage: &Path) -> Result<File, UpdateError> {
    let identity = directory_identity(&fs::symlink_metadata(stage)?);
    let mut lease = lock_file(private_create(&extraction_lease_path(stage))?)?.into_file();
    lease.write_all(&identity)?;
    lease.sync_all()?;
    let state = stage
        .parent()
        .ok_or_else(|| invalid("release stage has no parent"))?;
    File::open(state)?.sync_all()?;
    Ok(lease)
}

#[cfg(any(target_os = "macos", test))]
fn directory_identity(metadata: &fs::Metadata) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&metadata.dev().to_le_bytes());
    bytes[8..].copy_from_slice(&metadata.ino().to_le_bytes());
    bytes
}

#[cfg(any(target_os = "macos", test))]
fn extraction_stage_name(lease: &str) -> Option<&str> {
    let stage = lease.strip_suffix(".lock")?;
    let suffix = stage.strip_prefix(RELEASE_STAGE_PREFIX)?;
    (suffix.len() == RELEASE_STAGE_RANDOM_BYTES
        && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    .then_some(stage)
}

#[cfg(any(target_os = "macos", test))]
fn collect_extraction(root: &Root, name: &str, lease: &str) -> Result<(), UpdateError> {
    let mut file = match fs_safety::open_beneath_writable(root, Path::new(lease)) {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(()),
        Err(error) if error.is_not_found() => return Ok(()),
        Err(error) => return Err(io::Error::other(error).into()),
    };
    let metadata = file.metadata()?;
    if !private_file(&metadata) || metadata.len() != 16 {
        return Ok(());
    }
    file = match lock_file(file) {
        Ok(lock) => lock.into_file(),
        Err(UpdateError::Busy) => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut identity = [0; 16];
    file.read_exact(&mut identity)?;

    let path = Path::new(name);
    match fs_safety::metadata_beneath(root, path) {
        Ok(metadata) => {
            let device = root.metadata()?.dev();
            if !metadata.is_dir()
                || metadata.uid() != crate::daemon::current_uid()
                || metadata.mode() & 0o077 != 0
                || metadata.dev() != device
                || directory_identity(&metadata) != identity
            {
                return Ok(());
            }
            let stage = fs_safety::open_directory_beneath(root, path).map_err(io::Error::other)?;
            if directory_identity(&stage.metadata()?) != identity {
                return Ok(());
            }
            clear_extraction(&stage, device)?;
            fs_safety::remove_directory_within(root, OsStr::new(name)).map_err(io::Error::other)?;
            fs_safety::sync_directory(root)?;
        }
        Err(error) if error.is_not_found() => {}
        Err(error) => return Err(io::Error::other(error).into()),
    }
    // Leave the identity and lock available until the tree's removal is durable.
    fs_safety::unlink_within(root, OsStr::new(lease)).map_err(io::Error::other)?;
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn clear_extraction(directory: &Root, device: u64) -> Result<(), UpdateError> {
    let names = fs_safety::directory_entries_beneath(directory, Path::new(""), usize::MAX)
        .map_err(io::Error::other)?;
    for name in names {
        let path = Path::new(&name);
        let metadata = fs_safety::metadata_beneath(directory, path).map_err(io::Error::other)?;
        if metadata.is_dir() {
            let child =
                fs_safety::open_directory_beneath(directory, path).map_err(io::Error::other)?;
            let held = child.metadata()?;
            if held.dev() != device || directory_identity(&held) != directory_identity(&metadata) {
                return Err(invalid("release staging directory changed during cleanup"));
            }
            clear_extraction(&child, device)?;
            fs_safety::remove_directory_within(directory, &name).map_err(io::Error::other)?;
        } else {
            fs_safety::unlink_within(directory, &name).map_err(io::Error::other)?;
        }
    }
    Ok(())
}

fn canonical_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == id)
}

fn private_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
        && metadata.uid() == crate::daemon::current_uid()
        && metadata.mode() & 0o077 == 0
        && metadata.nlink() == 1
}

pub(super) fn remember_success(state: &Path, id: &str, phase: &UpdatePhase) {
    if *phase == UpdatePhase::Complete
        && canonical_id(id)
        && let Err(error) = mark_complete(state, id)
    {
        tracing::warn!(%error, "could not record completed update for retention");
    }
}

fn mark_complete(state: &Path, id: &str) -> io::Result<()> {
    // The private empty marker distinguishes a verified completion from a
    // receipt which only proved the old daemon was usable after a failure.
    match private_create(&state.join(format!("{id}{COMPLETE}"))) {
        Ok(file) => file.sync_all().and_then(|()| File::open(state)?.sync_all()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Both update entry points call this while holding the updater lease and
/// before publishing a different job as current. Maintenance errors only
/// leave more evidence available.
pub(super) fn before_start(state: &Path) {
    if let Err(error) = maintain(state) {
        tracing::warn!(%error, "completed update files were not collected");
    }
}

fn maintain(state: &Path) -> Result<(), UpdateError> {
    let root = Root::open(state)?;
    let current: Option<UpdateJob> = match fs_safety::open_beneath(&root, Path::new("status.json"))
    {
        Ok(mut file) => {
            let metadata = file.metadata()?;
            if !private_file(&metadata) || metadata.len() > JOB_STATE_LIMIT {
                return Err(invalid("unsafe updater status"));
            }
            let bytes = fs_safety::read_bounded(&mut file, JOB_STATE_LIMIT)?;
            if bytes.len() as u64 > JOB_STATE_LIMIT {
                return Err(invalid("updater status exceeds limit"));
            }
            Some(serde_json::from_slice(&bytes)?)
        }
        Err(error) if error.is_not_found() => None,
        Err(error) => return Err(invalid(error.to_string())),
    };
    // Also enroll the newest job from an earlier updater that never wrote a
    // marker. Older jobs without final status stay available for recovery.
    if let Some(job) = &current {
        remember_success(state, &job.id, &job.phase);
    }
    if recovery::checkpoint_status()?.is_none() {
        let current = current.as_ref().map(|job| job.id.as_str());
        collect(&root, current, SystemTime::now())?;
    }
    Ok(())
}

fn job_artifact(name: &str) -> Option<&str> {
    let (job, suffix) = name.split_once('.')?;
    if matches!(
        suffix,
        "log"
            | "request"
            | "census.json"
            | "recovery.json"
            | "settled.json"
            | "linux-handoff.json"
            | "saved-windows.json"
            | "cutover.json"
            | "cutover-start-failed.json"
            | "cutover-restored.json"
            | "cutover-inventory.json"
            | "ready-claim.json"
            | "ready-effects.json"
            | "ready-retry-effects.json"
            | "ready-init-start.json"
            | "ready-init-complete.json"
            | "ready-retry-init-start.json"
            | "ready-retry-init-complete.json"
            | "ready-restore.json"
            | "ready-retry-claim.json"
            | "ready-runtime-owner.json"
            | "ready-plan.json"
            | "ready-source.json"
            | "ready-source-process.json"
            | "ready-return-intent.json"
            | "ready-return-claim.json"
            | "ready-return-ready.json"
            | "ready-return-effects.json"
            | "ready-return-init-start.json"
            | "ready-return-init-complete.json"
            | "ready-return-restore.json"
            | "ready-parked-support.json"
            | "ready-return.json"
            | "release-stage.json"
            | "cancel-nonce"
            | "decisions.lock"
    ) || suffix
        .strip_prefix("cutover-inventory.")
        .or_else(|| suffix.strip_prefix("cutover-parked."))
        .and_then(|digest| digest.strip_suffix(".json"))
        .is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        || suffix.split_once('.').is_some_and(|(nonce, kind)| {
            matches!(kind, "decision" | "cancel") && canonical_id(nonce)
        })
    {
        Some(job)
    } else {
        None
    }
}

fn collect(root: &Root, current: Option<&str>, now: SystemTime) -> Result<(), UpdateError> {
    let names = fs_safety::directory_entries_beneath(root, Path::new(""), usize::MAX)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let names: Vec<_> = names.iter().filter_map(|name| name.to_str()).collect();
    let mut expired = BTreeSet::new();
    for name in &names {
        #[cfg(any(target_os = "macos", test))]
        if let Some(stage) = extraction_stage_name(name)
            && let Err(error) = collect_extraction(root, stage, name)
        {
            tracing::warn!(name = *name, %error, "could not collect abandoned release extraction");
        }
        if let Some(job) = name.strip_suffix(COMPLETE)
            && Some(job) != current
            && canonical_id(job)
            && let Some(metadata) = private_metadata(root, name)?
            && metadata.len() == 0
            && metadata
                .modified()
                .ok()
                .and_then(|time| now.duration_since(time).ok())
                .is_some_and(|age| age >= KEEP)
        {
            expired.insert(job);
        }
    }
    if expired.is_empty() {
        return Ok(());
    }
    for name in names {
        if job_artifact(name).is_some_and(|job| expired.contains(job))
            && private_metadata(root, name)?.is_some()
        {
            fs_safety::unlink_within(root, OsStr::new(name))
                .map_err(|error| invalid(error.to_string()))?;
        }
    }
    // Make artifact removal durable before removing the proof needed to
    // retry it after a crash.
    fs_safety::sync_directory(root)?;
    for job in &expired {
        fs_safety::unlink_within(root, OsStr::new(&format!("{job}{COMPLETE}")))
            .map_err(|error| invalid(error.to_string()))?;
    }
    fs_safety::sync_directory(root)?;
    Ok(())
}

fn private_metadata(root: &Root, name: &str) -> Result<Option<fs::Metadata>, UpdateError> {
    match fs_safety::metadata_beneath(root, Path::new(name)) {
        Ok(metadata) => Ok(private_file(&metadata).then_some(metadata)),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(invalid(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn collects_abandoned_release_extractions_and_keeps_other_entries() {
        let directory = crate::test_fixture::tempdir().unwrap();
        let state = directory.path();
        let neighboring = crate::test_fixture::tempdir().unwrap();
        let kept_file = neighboring.path().join("note");
        fs::write(&kept_file, b"support data").unwrap();
        let link = state.join(".release-stage-A1b2C3");
        std::os::unix::fs::symlink(neighboring.path(), &link).unwrap();
        let diagnostic = state.join(".release-stage-saved-diagnostics");
        fs::create_dir(&diagnostic).unwrap();

        let staging = || {
            tempfile::Builder::new()
                .prefix(RELEASE_STAGE_PREFIX)
                .rand_bytes(RELEASE_STAGE_RANDOM_BYTES)
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir_in(state)
                .unwrap()
                .keep()
        };
        let legacy = staging();
        let active = staging();
        let active_lease = lease_extraction(&active).unwrap();
        let extractor = active_lease.try_clone().unwrap();
        drop(active_lease);
        let abandoned = staging();
        drop(lease_extraction(&abandoned).unwrap());
        let abandoned_lease = extraction_lease_path(&abandoned);
        let unpacking = abandoned.join(".unpacking-A");
        fs::create_dir(&unpacking).unwrap();
        fs::write(unpacking.join("download"), b"partial extraction").unwrap();
        std::os::unix::fs::symlink(neighboring.path(), unpacking.join("neighbor")).unwrap();
        let replaced = staging();
        drop(lease_extraction(&replaced).unwrap());
        fs::rename(&replaced, state.join("saved-extraction")).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&replaced).unwrap();

        let root = Root::open(state).unwrap();
        collect(&root, None, SystemTime::now()).unwrap();
        assert!(!abandoned.exists());
        assert!(!abandoned_lease.exists());
        assert!(legacy.is_dir());
        assert!(active.is_dir());
        assert!(replaced.is_dir());
        assert!(diagnostic.is_dir());
        assert!(fs::symlink_metadata(link).unwrap().is_symlink());
        assert_eq!(fs::read(kept_file).unwrap(), b"support data");

        drop(extractor);
        collect(&root, None, SystemTime::now()).unwrap();
        assert!(!active.exists());
    }

    #[test]
    fn collects_only_expired_verified_jobs_that_are_no_longer_current() {
        let directory = crate::test_fixture::tempdir().unwrap();
        let state = directory.path();
        let root = Root::open(state).unwrap();
        let old = uuid::Uuid::new_v4().to_string();
        let current = uuid::Uuid::new_v4().to_string();
        let failed = uuid::Uuid::new_v4().to_string();
        let unknown = uuid::Uuid::new_v4().to_string();
        let nonce = uuid::Uuid::new_v4().to_string();
        let artifacts = [
            "log",
            "request",
            "census.json",
            "recovery.json",
            "cutover-inventory.json",
            "ready-parked-support.json",
            "cutover-start-failed.json",
            "cutover-restored.json",
            "cutover-inventory.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json",
            "ready-claim.json",
            "ready-effects.json",
            "ready-retry-effects.json",
            "ready-init-start.json",
            "ready-init-complete.json",
            "ready-restore.json",
            "ready-retry-claim.json",
            "ready-runtime-owner.json",
            "ready-plan.json",
            "ready-source.json",
            "ready-return.json",
            "cancel-nonce",
            "decisions.lock",
        ];
        for job in [&old, &current, &failed, &unknown] {
            for artifact in artifacts {
                private_create(&state.join(format!("{job}.{artifact}"))).unwrap();
            }
        }
        for job in [&old, &current] {
            remember_success(state, job, &UpdatePhase::Complete);
            assert!(state.join(format!("{job}{COMPLETE}")).is_file());
        }
        remember_success(state, &failed, &UpdatePhase::Failed);
        let decision = state.join(format!("{old}.{nonce}.decision"));
        private_create(&decision).unwrap();
        let unknown_file = state.join(format!("{old}.notes"));
        private_create(&unknown_file).unwrap();
        let similarly_named = state.join(format!("{old}.cutover-inventory.note.json"));
        private_create(&similarly_named).unwrap();
        let linked = state.join(format!("{old}.{nonce}.cancel"));
        std::os::unix::fs::symlink(&unknown_file, &linked).unwrap();

        let now = SystemTime::now();
        collect(&root, Some(&current), now).unwrap();
        assert!(decision.exists());
        let expired = now + KEEP + Duration::from_secs(1);
        for _ in 0..2 {
            collect(&root, Some(&current), expired).unwrap();
        }
        assert!(!state.join(format!("{old}{COMPLETE}")).exists());
        assert!(!decision.exists());
        assert!(unknown_file.exists());
        assert!(similarly_named.exists());
        assert!(linked.is_symlink());
        for artifact in artifacts {
            assert!(!state.join(format!("{old}.{artifact}")).exists());
            for job in [&current, &failed, &unknown] {
                assert!(state.join(format!("{job}.{artifact}")).exists());
            }
        }
    }
}
