//! Read-only update previews for the isolated native evidence environment.
use super::*;

pub(super) fn read(root: &Path) -> Result<Option<UpdateJob>, UpdateError> {
    let path = root.join("home/.config/fixture-update-review.json");
    let bytes = match read_private(&path, JOB_STATE_LIMIT) {
        Ok(bytes) => bytes,
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let job: UpdateJob = serde_json::from_slice(&bytes)?;
    // The non-UUID identity makes every decision fail before it can look up
    // an installation. A blocked preview disables the committing control.
    if job.id != "fixture-update-review"
        || job.phase != UpdatePhase::AwaitingConfirmation
        || job.active_confirmation().is_none_or(|confirmation| {
            !confirmation
                .preview
                .daemon
                .as_ref()
                .is_some_and(|daemon| daemon.blocked())
        })
    {
        return Err(invalid(
            "update review fixture must be blocked and read-only",
        ));
    }
    Ok(Some(job))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_native_review_fixture_cannot_submit_an_update_decision() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("home/.config");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("fixture-update-review.json");
        assert!(read(root.path()).unwrap().is_none());
        let mut job: UpdateJob =
            serde_json::from_str(include_str!("../../tests/fixtures/update-review.json")).unwrap();
        let publish = |job: &UpdateJob| {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            serde_json::to_writer(&mut file, job).unwrap();
        };
        publish(&job);
        let loaded = read(root.path()).unwrap().unwrap();
        let confirmation = loaded.active_confirmation().unwrap();
        for choice in [UpdateChoice::Confirm, UpdateChoice::Cancel] {
            assert!(matches!(
                decide(&loaded.id, &confirmation.nonce, choice),
                Err(UpdateError::Invalid(message)) if message == "invalid update confirmation identifier"
            ));
        }
        job.id = uuid::Uuid::new_v4().to_string();
        publish(&job);
        assert!(read(root.path()).is_err());
        job.id = loaded.id;
        job.phase = UpdatePhase::Installing;
        publish(&job);
        assert!(read(root.path()).is_err());
        job.phase = UpdatePhase::AwaitingConfirmation;
        job.confirmation.as_mut().unwrap().preview.daemon = None;
        publish(&job);
        assert!(read(root.path()).is_err());
    }
}
