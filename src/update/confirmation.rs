use super::*;

const CONFIRMATION_LIFETIME: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confirmation {
    pub nonce: String,
    pub preview: impact::Preview,
    pub expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Choice {
    Confirm,
    Policy(impact::Policy),
    Cancel,
}

#[derive(Serialize, Deserialize)]
struct Decision {
    job: String,
    nonce: String,
    choice: Choice,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// A choice only applies to the exact preview still held by the leased worker.
/// Unique files avoid replacing another window's decision mid-read.
pub fn decide(job: &str, nonce: &str, choice: Choice) -> Result<(), UpdateError> {
    for value in [job, nonce] {
        if uuid::Uuid::parse_str(value)
            .ok()
            .is_none_or(|id| id.to_string() != value)
        {
            return Err(invalid("invalid update confirmation identifier"));
        }
    }
    let state = running_installation()?.state_dir()?;
    let current: UpdateJob =
        serde_json::from_slice(&read_private(&state.join("status.json"), JOB_STATE_LIMIT)?)?;
    if !matches!(
        current.phase,
        UpdatePhase::AwaitingConfirmation | UpdatePhase::WaitingForIdle
    ) || current.id != job
        || current.confirmation.as_ref().is_none_or(|confirmation| {
            confirmation.nonce != nonce || now_ms() >= confirmation.expires_at_unix_ms
        })
    {
        return Err(invalid(
            "update preview changed or expired; review the current preview",
        ));
    }
    publish_decision(&state, job, nonce, choice)
}

fn publish_decision(
    state: &Path,
    job: &str,
    nonce: &str,
    choice: Choice,
) -> Result<(), UpdateError> {
    let path = state.join(format!("{job}.{nonce}.decision"));
    let temporary = state.join(format!("{}.decision-writing", uuid::Uuid::new_v4()));
    let file = private_create(&temporary)?;
    serde_json::to_writer(
        &file,
        &Decision {
            job: job.into(),
            nonce: nonce.into(),
            choice,
        },
    )?;
    file.sync_all()?;
    let published = fs::hard_link(&temporary, &path);
    let _ = fs::remove_file(&temporary);
    published?;
    File::open(&state)?.sync_all()?;
    Ok(())
}

/// Keep the verified assets and worker lease alive while any window may resume
/// this preview. Neither helper death nor an expired choice permits installation.
pub(super) fn wait(
    state: &Path,
    job: &mut UpdateJob,
    mut observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
) -> Result<impact::Preview, UpdateError> {
    let deadline = Instant::now() + CONFIRMATION_LIFETIME;
    let expires = now_ms().saturating_add(CONFIRMATION_LIFETIME.as_millis() as u64);
    let mut policy = impact::Policy::WhenIdle;
    let mut approved = false;
    let mut preview = observe(policy)?;
    let mut nonce = uuid::Uuid::new_v4().to_string();
    loop {
        if Instant::now() >= deadline {
            return Err(invalid(
                "Update preview expired without installation; review the update again",
            ));
        }
        job.confirmation = Some(Confirmation {
            nonce: nonce.clone(),
            preview: preview.clone(),
            expires_at_unix_ms: expires,
        });
        set_phase(
            state,
            job,
            if approved {
                UpdatePhase::WaitingForIdle
            } else {
                UpdatePhase::AwaitingConfirmation
            },
            if approved {
                "Waiting for the confirmed conversations to become idle"
            } else {
                "Verified update is ready for review"
            },
        )?;
        let path = state.join(format!("{}.{}.decision", job.id, nonce));
        match read_private(&path, SMALL_ASSET_LIMIT) {
            Ok(bytes) => {
                let decision: Decision = serde_json::from_slice(&bytes)?;
                if decision.job != job.id || decision.nonce != nonce {
                    return Err(invalid(
                        "update decision does not match the current preview",
                    ));
                }
                fs::remove_file(&path)?;
                match decision.choice {
                    Choice::Cancel => return Err(UpdateError::Cancelled),
                    Choice::Confirm => approved = true,
                    Choice::Policy(next) => {
                        policy = next;
                        approved = false;
                        preview = observe(policy)?;
                        nonce = uuid::Uuid::new_v4().to_string();
                        continue;
                    }
                }
            }
            Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        let current = observe(policy)?;
        if !preview.preserves_consent(&current) {
            approved = false;
            nonce = uuid::Uuid::new_v4().to_string();
        }
        preview = current;
        if approved
            && preview
                .daemon
                .as_ref()
                .is_none_or(impact::DaemonImpact::ready)
        {
            job.confirmation = None;
            return Ok(preview);
        }
        thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(directory: &Path) -> UpdateJob {
        UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "9.8.7".into(),
            phase: UpdatePhase::Verifying,
            message: String::new(),
            log_path: directory.join("worker.log"),
            confirmation: None,
        }
    }

    fn preview(changed: bool) -> impact::Preview {
        impact::Preview {
            candidate: impact::Candidate {
                version: "9.8.7".into(),
                protocol: 32,
                digest: if changed { "new" } else { "original" }.into(),
            },
            replaces_daemon: false,
            daemon: None,
        }
    }

    fn published(directory: &Path, previous: Option<&str>) -> UpdateJob {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(bytes) = fs::read(directory.join("status.json"))
                && let Ok(job) = serde_json::from_slice::<UpdateJob>(&bytes)
                && job
                    .confirmation
                    .as_ref()
                    .is_some_and(|confirmation| Some(confirmation.nonce.as_str()) != previous)
            {
                return job;
            }
            assert!(
                Instant::now() < deadline,
                "worker did not publish confirmation"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn changed_candidate_requires_a_new_decision_even_after_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let changed = Arc::new(AtomicBool::new(false));
        let worker_changed = changed.clone();
        let path = directory.path().to_owned();
        let worker = thread::spawn(move || {
            let mut job = job(&path);
            wait(&path, &mut job, |_| {
                Ok(preview(worker_changed.load(Ordering::Acquire)))
            })
        });
        let first = published(directory.path(), None);
        let first_nonce = &first.confirmation.as_ref().unwrap().nonce;
        changed.store(true, Ordering::Release);
        publish_decision(directory.path(), &first.id, first_nonce, Choice::Confirm).unwrap();
        let second = published(directory.path(), Some(first_nonce));
        assert!(!worker.is_finished());
        assert_eq!(
            second
                .confirmation
                .as_ref()
                .unwrap()
                .preview
                .candidate
                .digest,
            "new"
        );
        publish_decision(
            directory.path(),
            &second.id,
            &second.confirmation.as_ref().unwrap().nonce,
            Choice::Confirm,
        )
        .unwrap();
        assert_eq!(worker.join().unwrap().unwrap().candidate.digest, "new");
    }

    #[test]
    fn cancellation_does_not_return_an_installable_preview() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        let worker = thread::spawn(move || {
            let mut job = job(&path);
            wait(&path, &mut job, |_| Ok(preview(false)))
        });
        let current = published(directory.path(), None);
        publish_decision(
            directory.path(),
            &current.id,
            &current.confirmation.as_ref().unwrap().nonce,
            Choice::Cancel,
        )
        .unwrap();
        assert!(matches!(
            worker.join().unwrap(),
            Err(UpdateError::Cancelled)
        ));
    }
}
