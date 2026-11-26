//! Durable inputs for the installed updater's exact Ready activation.
use super::*;
use crate::upgrade::recovery::{ReadyPlanEvidence, Receipt, STANDBY_RECEIPT_LIMIT};

const LIMIT: u64 = STANDBY_RECEIPT_LIMIT as u64 + JOB_STATE_LIMIT;

#[derive(Serialize)]
struct Plan<'a> {
    version: u16,
    target: &'a Path,
    selection: &'a installed::Selection,
    stamp: &'a installed::Stamp,
    binaries: &'a str,
    ready: ReadyPlanEvidence<'a>,
}

pub(super) struct Recorded {
    path: PathBuf,
    digest: [u8; 32],
}

impl Recorded {
    pub(super) fn unchanged(&self) -> Result<bool, UpdateError> {
        let (Some(directory), Some(name)) = (self.path.parent(), self.path.file_name()) else {
            return Err(invalid(
                "The Ready activation plan has no private file location",
            ));
        };
        let root = crate::fs_safety::Root::open(directory)?;
        let file = crate::fs_safety::open_beneath(&root, Path::new(name))
            .map_err(|error| invalid(error.to_string()))?;
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
        Ok(bytes.len() as u64 <= LIMIT && Sha256::digest(&bytes)[..] == self.digest)
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
        target,
        selection,
        stamp,
        binaries,
        ready: receipt.ready_plan_evidence().map_err(invalid)?,
    })?;
    if bytes.len() as u64 > LIMIT {
        return Err(invalid("The Ready activation plan exceeds its size limit"));
    }
    let path = state.join(format!("{}.ready-plan.json", job.id));
    cutover::persist(state, &path, &bytes, true)?;
    Ok(Recorded {
        path,
        digest: Sha256::digest(&bytes).into(),
    })
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
