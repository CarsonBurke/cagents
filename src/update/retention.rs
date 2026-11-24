//! Retains recent verified update jobs and the exact inputs of unfinished jobs.
use super::*;
use crate::fs_safety::{self, Root};
use std::{collections::BTreeSet, ffi::OsStr, time::SystemTime};

const KEEP: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const COMPLETE: &str = ".complete";

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
        "log" | "request" | "census.json" | "recovery.json" | "cancel-nonce" | "decisions.lock"
    ) || suffix
        .split_once('.')
        .is_some_and(|(nonce, kind)| matches!(kind, "decision" | "cancel") && canonical_id(nonce))
    {
        Some(job)
    } else {
        None
    }
}

fn collect(root: &Root, current: Option<&str>, now: SystemTime) -> Result<(), UpdateError> {
    let names = fs::read_dir(root.process_path())?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    let names: Vec<_> = names.iter().filter_map(|name| name.to_str()).collect();
    let mut expired = BTreeSet::new();
    for name in &names {
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
        assert!(linked.is_symlink());
        for artifact in artifacts {
            assert!(!state.join(format!("{old}.{artifact}")).exists());
            for job in [&current, &failed, &unknown] {
                assert!(state.join(format!("{job}.{artifact}")).exists());
            }
        }
    }
}
