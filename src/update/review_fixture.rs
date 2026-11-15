//! Read-only update previews for the isolated native evidence environment.
use super::*;

pub(super) fn read(root: &Path) -> Result<Option<UpdateJob>, UpdateError> {
    let Some(bytes) = read_fixture(&root.join("home/.config/fixture-update-review.json"))? else {
        return Ok(None);
    };
    let job: UpdateJob = serde_json::from_slice(&bytes)?;
    // The non-UUID identity makes every decision fail before it can look up
    // an installation. A blocked preview disables the committing control,
    // and a review that has ended has nothing left to decide.
    let blocked_review = job.phase == UpdatePhase::AwaitingConfirmation
        && job.active_confirmation().is_some_and(|confirmation| {
            confirmation
                .preview
                .daemon
                .as_ref()
                .is_some_and(|daemon| daemon.blocked())
        });
    let ended = job.phase.is_terminal() && job.confirmation.is_none();
    if job.id != "fixture-update-review" || !(blocked_review || ended) {
        return Err(invalid(
            "update review fixture must be blocked and read-only",
        ));
    }
    Ok(Some(job))
}

/// The release a development build's update check offers while
/// `home/.config/fixture-update-release.json` is present, so the header can
/// show what it says about an available update. `apply` refuses it.
pub(super) fn release(root: &Path) -> Result<Option<Release>, UpdateError> {
    read_fixture(&root.join(RELEASE))?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
        .transpose()
}

pub(super) fn offers_release(root: &Path) -> bool {
    root.join(RELEASE).exists()
}

/// A release the fixture offered is never handed to the installer.
pub(super) fn refuse_fixture_release(root: &Path, offered: &Release) -> Result<(), UpdateError> {
    if release(root)?.is_some_and(|fixture| &fixture == offered) {
        return Err(invalid("the update fixture's release cannot be installed"));
    }
    Ok(())
}

const RELEASE: &str = "home/.config/fixture-update-release.json";

fn read_fixture(path: &Path) -> Result<Option<Vec<u8>>, UpdateError> {
    match read_private(path, JOB_STATE_LIMIT) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
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
        // An ended review shows how it ended, with nothing left to decide.
        job.phase = UpdatePhase::Failed;
        publish(&job);
        assert!(read(root.path()).is_err());
        job.confirmation = None;
        publish(&job);
        assert_eq!(
            read(root.path()).unwrap().unwrap().phase,
            UpdatePhase::Failed
        );
    }

    #[test]
    fn the_release_fixture_is_offered_but_cannot_be_installed() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("home/.config");
        fs::create_dir_all(&directory).unwrap();
        assert!(!offers_release(root.path()));
        assert!(release(root.path()).unwrap().is_none());
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join("fixture-update-release.json"))
            .unwrap();
        file.write_all(include_bytes!("../../tests/fixtures/update-release.json"))
            .unwrap();
        assert!(offers_release(root.path()));
        let offered = release(root.path()).unwrap().unwrap();
        assert_eq!(offered.version, "9.8.7");
        assert!(matches!(
            refuse_fixture_release(root.path(), &offered),
            Err(UpdateError::Invalid(_))
        ));
    }
}
