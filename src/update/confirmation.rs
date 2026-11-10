use super::*;

const CONFIRMATION_LIFETIME: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confirmation {
    pub nonce: String,
    #[serde(default)]
    pub stop: Option<floor_display::StopList>,
    pub preview: impact::Preview,
    pub expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Choice {
    Confirm,
    StopListed { items_digest: String },
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
    terminal: bool,
    observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
) -> Result<impact::Preview, UpdateError> {
    wait_with(state, job, terminal, None, observe)
}

pub(super) fn wait_approved(
    state: &Path,
    job: &mut UpdateJob,
    terminal: bool,
    reviewed: impact::Preview,
    observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
) -> Result<impact::Preview, UpdateError> {
    wait_with(state, job, terminal, Some(reviewed), observe)
}

fn wait_with(
    state: &Path,
    job: &mut UpdateJob,
    terminal: bool,
    reviewed: Option<impact::Preview>,
    mut observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
) -> Result<impact::Preview, UpdateError> {
    let deadline = Instant::now() + CONFIRMATION_LIFETIME;
    let expires = now_ms().saturating_add(CONFIRMATION_LIFETIME.as_millis() as u64);
    let mut policy = reviewed
        .as_ref()
        .and_then(|preview| preview.daemon.as_ref())
        .map_or(impact::Policy::WhenIdle, |daemon| daemon.policy);
    let mut approved = reviewed.is_some();
    let mut preview = match reviewed {
        Some(preview) => preview,
        None => observe(policy)?,
    };
    let mut nonce = uuid::Uuid::new_v4().to_string();
    loop {
        if !terminal
            && preview
                .daemon
                .as_ref()
                .is_some_and(impact::DaemonImpact::blocked)
        {
            return Err(invalid(floor::TERMINAL_CONFIRMATION));
        }
        if Instant::now() >= deadline {
            return Err(invalid(
                "Update preview expired without installation; review the update again",
            ));
        }
        job.confirmation = Some(Confirmation {
            nonce: nonce.clone(),
            stop: None,
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
                    Choice::StopListed { .. } => (),
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
            && preview.daemon.as_ref().is_none_or(|daemon| {
                daemon.blocked()
                    || !daemon
                        .sessions
                        .iter()
                        .any(|session| session.disposition == impact::Disposition::WaitsForIdle)
            })
        {
            job.confirmation = None;
            return Ok(preview);
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// Destructive consent is separate from the ordinary review, and bound to the
/// source proof, candidate and exact displayed items. A refreshed capability
/// alone is harmless; a different process or command needs another typed word.
pub(super) fn wait_stop(
    state: &Path,
    job: &mut UpdateJob,
    mut observe: impl FnMut() -> Result<(impact::Preview, floor_display::StopList), UpdateError>,
) -> Result<(impact::Preview, floor_display::StopList), UpdateError> {
    let deadline = Instant::now() + CONFIRMATION_LIFETIME;
    let expires = now_ms().saturating_add(CONFIRMATION_LIFETIME.as_millis() as u64);
    let (mut preview, mut list) = observe()?;
    let mut nonce = uuid::Uuid::new_v4().to_string();
    loop {
        if Instant::now() >= deadline {
            return Err(invalid(
                "Stop confirmation expired; nothing further was stopped",
            ));
        }
        job.confirmation = Some(Confirmation {
            nonce: nonce.clone(),
            preview: preview.clone(),
            stop: Some(list.clone()),
            expires_at_unix_ms: expires,
        });
        set_phase(
            state,
            job,
            UpdatePhase::AwaitingConfirmation,
            "The terminal service needs confirmation before stopping live work",
        )?;
        let path = state.join(format!("{}.{}.decision", job.id, nonce));
        let decision = match read_private(&path, SMALL_ASSET_LIMIT) {
            Ok(bytes) => {
                let decision: Decision = serde_json::from_slice(&bytes)?;
                fs::remove_file(&path)?;
                if decision.job != job.id || decision.nonce != nonce {
                    return Err(invalid("Stop decision does not match the current list"));
                }
                Some(decision.choice)
            }
            Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if matches!(decision, Some(Choice::Cancel)) {
            return Err(UpdateError::Cancelled);
        }
        let (current, current_list) = observe()?;
        let unchanged =
            preview.preserves_consent(&current) && list.digest() == current_list.digest();
        if unchanged
            && matches!(decision, Some(Choice::StopListed { items_digest }) if items_digest == current_list.digest())
        {
            job.confirmation = None;
            return Ok((current, current_list));
        }
        if !unchanged {
            nonce = uuid::Uuid::new_v4().to_string();
        }
        preview = current;
        list = current_list;
        thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

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
            from_version: String::new(),
            windows: 0,
            stopped: Vec::new(),
            stopped_windows: Vec::new(),
            labels: Default::default(),
            editors: Vec::new(),
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
            wait(&path, &mut job, true, |_| {
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
            wait(&path, &mut job, true, |_| Ok(preview(false)))
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

    fn stop_list() -> floor_display::StopList {
        floor_display::StopList {
            from_version: "0.1.3+local.source".into(),
            items: vec![floor_display::StopItem {
                id: crate::model::TerminalId::new(),
                kind: impact::SessionKind::Terminal,
                label: "build · General · alpha".into(),
                identity: "the reviewed foreground owner".into(),
                running: "shell running `cargo test` (since 13:40)".into(),
                recovery: "screen and directory kept; press Restart in the pane".into(),
            }],
        }
    }

    /// Wait through an entire observation after the decision was consumed.
    /// A worker that incorrectly returned success cannot satisfy this barrier.
    fn still_waiting<T>(
        directory: &Path,
        job: &UpdateJob,
        observations: &AtomicUsize,
        worker: &thread::JoinHandle<T>,
    ) {
        let nonce = &job.confirmation.as_ref().unwrap().nonce;
        let path = directory.join(format!("{}.{}.decision", job.id, nonce));
        let deadline = Instant::now() + Duration::from_secs(5);
        while path.exists() {
            assert!(!worker.is_finished(), "decision authorized a stop");
            assert!(Instant::now() < deadline, "decision was not consumed");
            thread::sleep(Duration::from_millis(10));
        }
        let consumed = observations.load(Ordering::Acquire);
        while observations.load(Ordering::Acquire) <= consumed {
            assert!(!worker.is_finished(), "decision authorized a stop");
            assert!(Instant::now() < deadline, "worker stopped observing");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!worker.is_finished(), "decision authorized a stop");
    }

    #[test]
    fn stop_wait_rejects_ordinary_confirmation_policy_and_wrong_digest_until_cancelled() {
        let directory = tempfile::tempdir().unwrap();
        let observations = Arc::new(AtomicUsize::new(0));
        let worker_observations = observations.clone();
        let path = directory.path().to_owned();
        let list = stop_list();
        let worker = thread::spawn(move || {
            let mut job = job(&path);
            wait_stop(&path, &mut job, || {
                worker_observations.fetch_add(1, Ordering::Release);
                Ok((preview(false), list.clone()))
            })
        });
        let current = published(directory.path(), None);
        assert!(current.confirmation.as_ref().unwrap().stop.is_some());
        for choice in [
            Choice::Confirm,
            Choice::Policy(impact::Policy::Interrupt),
            Choice::StopListed {
                items_digest: "another list".into(),
            },
        ] {
            publish_decision(
                directory.path(),
                &current.id,
                &current.confirmation.as_ref().unwrap().nonce,
                choice,
            )
            .unwrap();
            still_waiting(directory.path(), &current, &observations, &worker);
        }
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

    #[test]
    fn stop_wait_returns_only_the_exact_list_authorized_by_stop_listed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        let list = stop_list();
        let expected = list.clone();
        let worker = thread::spawn(move || {
            let mut job = job(&path);
            let result = wait_stop(&path, &mut job, || Ok((preview(false), list.clone())));
            assert!(job.confirmation.is_none());
            result
        });
        let current = published(directory.path(), None);
        let listed = current
            .confirmation
            .as_ref()
            .unwrap()
            .stop
            .as_ref()
            .unwrap();
        assert_eq!(listed, &expected);
        publish_decision(
            directory.path(),
            &current.id,
            &current.confirmation.as_ref().unwrap().nonce,
            Choice::StopListed {
                items_digest: listed.digest(),
            },
        )
        .unwrap();
        let (confirmed, returned) = worker.join().unwrap().unwrap();
        assert_eq!(returned, expected);
        assert_eq!(confirmed, preview(false));
    }

    #[test]
    fn changed_stop_list_reasks_and_cannot_reuse_a_stale_digest() {
        let directory = tempfile::tempdir().unwrap();
        let changed = Arc::new(AtomicBool::new(false));
        let worker_changed = changed.clone();
        let observations = Arc::new(AtomicUsize::new(0));
        let worker_observations = observations.clone();
        let path = directory.path().to_owned();
        let list = stop_list();
        let worker = thread::spawn(move || {
            let mut job = job(&path);
            wait_stop(&path, &mut job, || {
                worker_observations.fetch_add(1, Ordering::Release);
                let mut current = list.clone();
                if worker_changed.load(Ordering::Acquire) {
                    current.items[0].running = "shell running `cargo publish` (since 13:41)".into();
                    current.items[0].identity = "a replacement foreground owner".into();
                }
                Ok((preview(false), current))
            })
        });
        let first = published(directory.path(), None);
        let first_confirmation = first.confirmation.as_ref().unwrap();
        let stale_digest = first_confirmation.stop.as_ref().unwrap().digest();
        changed.store(true, Ordering::Release);
        publish_decision(
            directory.path(),
            &first.id,
            &first_confirmation.nonce,
            Choice::StopListed {
                items_digest: stale_digest.clone(),
            },
        )
        .unwrap();
        let second = published(directory.path(), Some(&first_confirmation.nonce));
        let second_confirmation = second.confirmation.as_ref().unwrap();
        let current_digest = second_confirmation.stop.as_ref().unwrap().digest();
        assert_ne!(current_digest, stale_digest);
        assert!(!worker.is_finished());
        publish_decision(
            directory.path(),
            &second.id,
            &second_confirmation.nonce,
            Choice::StopListed {
                items_digest: stale_digest,
            },
        )
        .unwrap();
        still_waiting(directory.path(), &second, &observations, &worker);
        publish_decision(
            directory.path(),
            &second.id,
            &second_confirmation.nonce,
            Choice::StopListed {
                items_digest: current_digest.clone(),
            },
        )
        .unwrap();
        let (_, returned) = worker.join().unwrap().unwrap();
        assert_eq!(returned.digest(), current_digest);
        assert_eq!(returned.items[0].identity, "a replacement foreground owner");
    }
}
