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

/// A stable, private cancellation nonce exists before the worker is spawned,
/// independently of the previews it will later publish for approval.
pub(super) fn initialize(state: &Path, job: &str) -> Result<(), UpdateError> {
    let file = private_create(&state.join(format!("{job}.cancel-nonce")))?;
    serde_json::to_writer(&file, &uuid::Uuid::new_v4().to_string())?;
    file.sync_all()?;
    File::open(state)?.sync_all()?;
    Ok(())
}

fn cancellation_nonce(state: &Path, job: &str) -> Result<Option<String>, UpdateError> {
    let bytes = match read_private(
        &state.join(format!("{job}.cancel-nonce")),
        SMALL_ASSET_LIMIT,
    ) {
        Ok(bytes) => bytes,
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let nonce: String = serde_json::from_slice(&bytes)?;
    if uuid::Uuid::parse_str(&nonce)
        .ok()
        .is_none_or(|id| id.to_string() != nonce)
    {
        return Err(invalid("invalid update cancellation identifier"));
    }
    Ok(Some(nonce))
}

pub(super) fn cancellation_path(state: &Path, job: &str) -> Result<Option<PathBuf>, UpdateError> {
    Ok(cancellation_nonce(state, job)?.map(|nonce| state.join(format!("{job}.{nonce}.cancel"))))
}

/// True means cancellation was recorded before the preparation boundary.
/// False means the worker must finish restoring service instead.
pub(super) fn cancel(job: &str) -> Result<bool, UpdateError> {
    let state = running_installation()?.state_dir()?;
    cancel_at(&state, job)
}

pub(super) fn cancel_at(state: &Path, job: &str) -> Result<bool, UpdateError> {
    let _decisions = DecisionLease::acquire(state, job)?;
    let current: UpdateJob =
        serde_json::from_slice(&read_private(&state.join("status.json"), JOB_STATE_LIMIT)?)?;
    if current.id != job || current.phase.is_terminal() {
        return Ok(false);
    }
    let Some(nonce) = cancellation_nonce(state, job)? else {
        return Ok(false);
    };
    publish_decision(state, job, &nonce, Choice::Cancel)?;
    Ok(true)
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
    decide_at(&state, job, nonce, choice)
}

pub(super) fn decide_at(
    state: &Path,
    job: &str,
    nonce: &str,
    choice: Choice,
) -> Result<(), UpdateError> {
    let _decisions = DecisionLease::acquire(state, job)?;
    let current: UpdateJob =
        serde_json::from_slice(&read_private(&state.join("status.json"), JOB_STATE_LIMIT)?)?;
    let accepts_choice = matches!(
        current.phase,
        UpdatePhase::AwaitingConfirmation | UpdatePhase::WaitingForIdle
    ) || matches!(choice, Choice::Cancel)
        && current.phase == UpdatePhase::ClosingWindows;
    if !accepts_choice
        || current.id != job
        || current.confirmation.as_ref().is_none_or(|confirmation| {
            confirmation.nonce != nonce
                || !matches!(choice, Choice::Cancel) && now_ms() >= confirmation.expires_at_unix_ms
        })
    {
        return Err(invalid(
            "update preview changed or expired; review the current preview",
        ));
    }
    publish_decision(&state, job, nonce, choice)
}

/// Serialize cancellation publication with closing its admission. The worker
/// never promises cancellation after it has begun preparing the source.
struct DecisionLease(File);

impl DecisionLease {
    fn acquire(state: &Path, job: &str) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(state.join(format!("{job}.decisions.lock")))?;
        loop {
            // SAFETY: a live, private file descriptor; the guard owns the lock.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(Self(file));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Drop for DecisionLease {
    fn drop(&mut self) {
        // SAFETY: the guard retains the locked descriptor for this call.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn publish_decision(
    state: &Path,
    job: &str,
    nonce: &str,
    choice: Choice,
) -> Result<(), UpdateError> {
    let suffix = if matches!(choice, Choice::Cancel) {
        "cancel"
    } else {
        "decision"
    };
    let path = state.join(format!("{job}.{nonce}.{suffix}"));
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
    match published {
        Ok(()) => (),
        Err(error) if suffix == "cancel" && error.kind() == io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    File::open(&state)?.sync_all()?;
    Ok(())
}

/// Cancellation is independent of approval: an already published Confirm must
/// never prevent a later Ctrl-C from reaching the detached worker.
pub(super) fn check_cancelled(state: &Path, job: &UpdateJob) -> Result<(), UpdateError> {
    if let Some(nonce) = cancellation_nonce(state, &job.id)? {
        check_cancellation(state, &job.id, &nonce)?;
    }
    if let Some(confirmation) = &job.confirmation {
        check_cancellation(state, &job.id, &confirmation.nonce)?;
    }
    Ok(())
}

fn check_cancellation(state: &Path, job: &str, nonce: &str) -> Result<(), UpdateError> {
    let path = state.join(format!("{job}.{nonce}.cancel"));
    match read_private(&path, SMALL_ASSET_LIMIT) {
        Ok(bytes) => {
            let decision: Decision = serde_json::from_slice(&bytes)?;
            if decision.job != job
                || decision.nonce != nonce
                || !matches!(decision.choice, Choice::Cancel)
            {
                return Err(invalid(
                    "update cancellation does not match the current review",
                ));
            }
            Err(UpdateError::Cancelled)
        }
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Close cancellation admission immediately before requesting preparation.
/// Once this boundary is crossed the worker must restore service even if its
/// observing CLI goes away. A cancellation already acknowledged always wins.
pub(super) fn prepared(state: &Path, job: &mut UpdateJob) -> Result<(), UpdateError> {
    let _decisions = DecisionLease::acquire(state, &job.id)?;
    check_cancelled(state, job)?;
    job.confirmation = None;
    write_job(state, job)?;
    match fs::remove_file(state.join(format!("{}.cancel-nonce", job.id))) {
        Ok(()) => File::open(state)?.sync_all()?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// A refused request has not ended anything. Restore cancellation before any
/// renewed human review, while keeping its publication serialized with Ctrl-C.
pub(super) fn resume_cancellation(state: &Path, job: &UpdateJob) -> Result<(), UpdateError> {
    let _decisions = DecisionLease::acquire(state, &job.id)?;
    if cancellation_nonce(state, &job.id)?.is_none() {
        initialize(state, &job.id)?;
    }
    Ok(())
}

/// Keep the verified assets and worker lease alive while any window may resume
/// this preview. Neither helper death nor an expired choice permits installation.
pub(super) fn wait(
    state: &Path,
    job: &mut UpdateJob,
    observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
) -> Result<impact::Preview, UpdateError> {
    wait_with(state, job, None, observe)
}

pub(super) fn wait_approved(
    state: &Path,
    job: &mut UpdateJob,
    reviewed: impact::Preview,
    observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
) -> Result<impact::Preview, UpdateError> {
    wait_with(state, job, Some(reviewed), observe)
}

fn wait_with(
    state: &Path,
    job: &mut UpdateJob,
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
        if Instant::now() >= deadline {
            return Err(invalid(
                "Update preview expired without installation; review the update again",
            ));
        }
        {
            // A cancellation acknowledged for the previous preview must win
            // before refreshing its nonce, including while observe was busy.
            let _decisions = DecisionLease::acquire(state, &job.id)?;
            check_cancelled(state, job)?;
            job.confirmation = Some(Confirmation {
                stop: None,
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
        }
        check_cancelled(state, job)?;
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
                        let current = observe(policy)?;
                        approved = policy_choice_approves_service_floor(&preview, &current, next);
                        preview = current;
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
                !daemon
                    .sessions
                    .iter()
                    .any(|session| session.disposition == impact::Disposition::WaitsForIdle)
            })
        {
            // Preserve this exact nonce until the source accepts prepare, so
            // Ctrl-C still cancels while windows are saving and closing.
            check_cancelled(state, job)?;
            return Ok(preview);
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// Choosing interruption advances a service-wide floor to its exact typed
/// stop list. That next confirmation covers every affected owner. Other
/// updates retain the ordinary renewed review of interruption scope.
fn policy_choice_approves_service_floor(
    previous: &impact::Preview,
    current: &impact::Preview,
    next: impact::Policy,
) -> bool {
    let (Some(before), Some(after)) = (&previous.daemon, &current.daemon) else {
        return false;
    };
    if next != impact::Policy::Interrupt
        || before.policy != impact::Policy::WhenIdle
        || !floor::needs_service_stop(before)
        || !floor::needs_service_stop(after)
    {
        return false;
    }
    let mut approved = previous.clone();
    let daemon = approved.daemon.as_mut().unwrap();
    daemon.policy = next;
    for session in &mut daemon.sessions {
        if session.disposition == impact::Disposition::WaitsForIdle {
            let Some(interrupted) = after.sessions.iter().find(|item| {
                item.kind == session.kind
                    && item.id == session.id
                    && item.identity == session.identity
                    && item.disposition == impact::Disposition::Interrupted
            }) else {
                return false;
            };
            session.disposition = impact::Disposition::Interrupted;
            session.interrupted_subagents = interrupted.interrupted_subagents.clone();
        }
    }
    approved.preserves_consent(current)
}

/// Destructive consent is separate from the ordinary review, and bound to the
/// source proof, candidate and exact displayed items. A refreshed capability
/// alone is harmless; a different process or command needs another typed word.
pub(super) fn wait_stop(
    state: &Path,
    job: &mut UpdateJob,
    terminal: bool,
    mut observe: impl FnMut() -> Result<(impact::Preview, floor_display::StopList), UpdateError>,
) -> Result<(impact::Preview, floor_display::StopList), UpdateError> {
    let deadline = Instant::now() + CONFIRMATION_LIFETIME;
    let expires = now_ms().saturating_add(CONFIRMATION_LIFETIME.as_millis() as u64);
    check_cancelled(state, job)?;
    let (mut preview, mut list) = observe()?;
    check_cancelled(state, job)?;
    if !terminal {
        return Err(invalid(format!(
            "{} Details: {}",
            floor::terminal_confirmation(state, job)?,
            list.render_preview()
        )));
    }
    let mut nonce = uuid::Uuid::new_v4().to_string();
    loop {
        if Instant::now() >= deadline {
            return Err(invalid(
                "Stop confirmation expired; nothing further was stopped",
            ));
        }
        {
            // A cancellation accepted for the old nonce must win before a
            // changed inventory can publish its replacement.
            let _decisions = DecisionLease::acquire(state, &job.id)?;
            check_cancelled(state, job)?;
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
                &format!(
                    "{}\n{}",
                    list.render_preview(),
                    floor::terminal_confirmation(state, job)?
                ),
            )?;
        }
        check_cancelled(state, job)?;
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
        check_cancelled(state, job)?;
        let unchanged =
            preview.preserves_stop_consent(&current) && list.digest() == current_list.digest();
        if unchanged
            && matches!(decision, Some(Choice::StopListed { items_digest }) if items_digest == current_list.digest())
        {
            // Keep this nonce cancellable through snapshot capture and the
            // final inventory checks. Only prepared() closes admission.
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
        let job = UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "9.8.7".into(),
            phase: UpdatePhase::Verifying,
            message: String::new(),
            log_path: directory.join("worker.log"),
            confirmation: None,
        };
        let request = private_create(&directory.join(format!("{}.request", job.id))).unwrap();
        serde_json::to_writer(
            request,
            &UpdateRequest::InstalledFromTerminal {
                root: "/fake".into(),
            },
        )
        .unwrap();
        job
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
    #[test]
    fn cancellation_overrides_pending_approval_and_remains_available_while_closing_windows() {
        let directory = tempfile::tempdir().unwrap();
        let mut job = job(directory.path());
        job.confirmation = Some(Confirmation {
            stop: None,
            nonce: uuid::Uuid::new_v4().to_string(),
            preview: preview(false),
            expires_at_unix_ms: now_ms() + 60_000,
        });
        job.phase = UpdatePhase::AwaitingConfirmation;
        write_job(directory.path(), &job).unwrap();
        let nonce = job.confirmation.as_ref().unwrap().nonce.clone();
        decide_at(directory.path(), &job.id, &nonce, Choice::Confirm).unwrap();
        decide_at(directory.path(), &job.id, &nonce, Choice::Cancel).unwrap();
        assert!(matches!(
            check_cancelled(directory.path(), &job),
            Err(UpdateError::Cancelled)
        ));

        // Approval and a long idle wait must not expire cancellation.
        job.phase = UpdatePhase::WaitingForIdle;
        job.confirmation.as_mut().unwrap().expires_at_unix_ms = 0;
        write_job(directory.path(), &job).unwrap();
        decide_at(directory.path(), &job.id, &nonce, Choice::Cancel).unwrap();

        // Saving windows does not revoke the already reviewed nonce.
        job.phase = UpdatePhase::ClosingWindows;
        write_job(directory.path(), &job).unwrap();
        decide_at(directory.path(), &job.id, &nonce, Choice::Cancel).unwrap();
        assert!(matches!(
            prepared(directory.path(), &mut job),
            Err(UpdateError::Cancelled)
        ));
        assert!(
            job.confirmation.is_some(),
            "a cancelled job cannot reach prepare"
        );
    }

    #[test]
    fn preparation_closes_cancellation_admission_before_source_request() {
        let directory = tempfile::tempdir().unwrap();
        let mut job = job(directory.path());
        job.phase = UpdatePhase::ClosingWindows;
        let nonce = uuid::Uuid::new_v4().to_string();
        job.confirmation = Some(Confirmation {
            stop: None,
            nonce: nonce.clone(),
            preview: preview(false),
            expires_at_unix_ms: now_ms() + 60_000,
        });
        write_job(directory.path(), &job).unwrap();
        prepared(directory.path(), &mut job).unwrap();
        assert!(job.confirmation.is_none());
        assert!(decide_at(directory.path(), &job.id, &nonce, Choice::Cancel).is_err());
    }

    #[test]
    fn approval_keeps_the_nonce_for_cancellation_after_review() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        let worker = thread::spawn(move || {
            let mut job = job(&path);
            wait(&path, &mut job, |_| Ok(preview(false))).unwrap();
            job
        });
        let current = published(directory.path(), None);
        let nonce = current.confirmation.as_ref().unwrap().nonce.clone();
        decide_at(directory.path(), &current.id, &nonce, Choice::Confirm).unwrap();
        let mut approved = worker.join().unwrap();
        assert_eq!(approved.confirmation.as_ref().unwrap().nonce, nonce);
        approved.phase = UpdatePhase::ClosingWindows;
        write_job(directory.path(), &approved).unwrap();
        decide_at(directory.path(), &approved.id, &nonce, Choice::Cancel).unwrap();
        assert!(matches!(
            prepared(directory.path(), &mut approved),
            Err(UpdateError::Cancelled)
        ));
    }
    #[test]
    fn cancellation_during_observation_survives_a_changed_preview() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        let (observing, entered) = std::sync::mpsc::sync_channel(0);
        let (release, resume) = std::sync::mpsc::sync_channel(0);
        let worker = thread::spawn(move || {
            let mut job = job(&path);
            let mut first = true;
            wait(&path, &mut job, |_| {
                if first {
                    first = false;
                    Ok(preview(false))
                } else {
                    observing.send(()).unwrap();
                    resume.recv().unwrap();
                    Ok(preview(true))
                }
            })
        });
        let current = published(directory.path(), None);
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        decide_at(
            directory.path(),
            &current.id,
            &current.confirmation.as_ref().unwrap().nonce,
            Choice::Cancel,
        )
        .unwrap();
        release.send(()).unwrap();
        assert!(matches!(
            worker.join().unwrap(),
            Err(UpdateError::Cancelled)
        ));
    }
    #[test]
    fn cancellation_during_verification_stops_probe_without_waiting_for_review() {
        let directory = tempfile::tempdir().unwrap();
        let mut job = job(directory.path());
        initialize(directory.path(), &job.id).unwrap();
        job.phase = UpdatePhase::Verifying;
        write_job(directory.path(), &job).unwrap();
        let cancellation = CancellationToken::for_job(directory.path(), &job.id).unwrap();
        let ready = directory.path().join("probe-started");
        let probe_ready = ready.clone();
        let worker = thread::spawn(move || {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "touch \"$1\"; exec sleep 30", "probe"])
                .arg(probe_ready);
            bounded_command(
                &mut command,
                "fixture verification",
                100,
                Duration::from_secs(30),
                &cancellation,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "verification probe did not start"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(job.confirmation.is_none());
        assert!(cancel_at(directory.path(), &job.id).unwrap());
        assert!(matches!(
            worker.join().unwrap(),
            Err(UpdateError::Cancelled)
        ));
        assert!(
            Instant::now() < deadline,
            "cancellation waited for the probe timeout"
        );
        assert!(matches!(
            check_cancelled(directory.path(), &job),
            Err(UpdateError::Cancelled)
        ));
        assert!(matches!(
            prepared(directory.path(), &mut job),
            Err(UpdateError::Cancelled)
        ));
    }

    #[test]
    fn preparation_atomically_closes_initial_cancellation_nonce() {
        let directory = tempfile::tempdir().unwrap();
        let mut job = job(directory.path());
        initialize(directory.path(), &job.id).unwrap();
        job.phase = UpdatePhase::ClosingWindows;
        write_job(directory.path(), &job).unwrap();
        let token = CancellationToken::for_job(directory.path(), &job.id).unwrap();
        prepared(directory.path(), &mut job).unwrap();
        assert!(!cancel_at(directory.path(), &job.id).unwrap());
        assert!(!token.cancelled());
        assert!(
            !directory
                .path()
                .join(format!("{}.cancel-nonce", job.id))
                .exists()
        );
    }
    fn stop_list() -> floor_display::StopList {
        floor_display::StopList {
            background_tasks: 0,
            background_identity: String::new(),
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
            wait_stop(&path, &mut job, true, || {
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
            let result = wait_stop(&path, &mut job, true, || Ok((preview(false), list.clone())));
            assert!(
                job.confirmation
                    .as_ref()
                    .is_some_and(|confirmation| confirmation.stop.is_some())
            );
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
            wait_stop(&path, &mut job, true, || {
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
    #[test]
    fn typed_stop_keeps_cancellation_available_until_the_first_destructive_request() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        let worker = thread::spawn(move || {
            let mut job = job(&path);
            initialize(&path, &job.id).unwrap();
            let list = stop_list();
            wait_stop(&path, &mut job, true, || Ok((preview(false), list.clone()))).unwrap();
            job
        });
        let current = published(directory.path(), None);
        let confirmation = current.confirmation.as_ref().unwrap();
        decide_at(
            directory.path(),
            &current.id,
            &confirmation.nonce,
            Choice::StopListed {
                items_digest: confirmation.stop.as_ref().unwrap().digest(),
            },
        )
        .unwrap();
        let mut approved = worker.join().unwrap();
        assert!(approved.confirmation.as_ref().unwrap().stop.is_some());
        assert!(cancel_at(directory.path(), &approved.id).unwrap());
        assert!(matches!(
            prepared(directory.path(), &mut approved),
            Err(UpdateError::Cancelled)
        ));
    }

    #[test]
    fn ctrl_c_during_stop_observation_survives_a_changed_preview() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        let (observing, entered) = std::sync::mpsc::sync_channel(0);
        let (release, resume) = std::sync::mpsc::sync_channel(0);
        let worker = thread::spawn(move || {
            let mut job = job(&path);
            initialize(&path, &job.id).unwrap();
            let list = stop_list();
            let mut first = true;
            wait_stop(&path, &mut job, true, || {
                if first {
                    first = false;
                    Ok((preview(false), list.clone()))
                } else {
                    observing.send(()).unwrap();
                    resume.recv().unwrap();
                    let mut changed = list.clone();
                    changed.items[0]
                        .identity
                        .push_str(" changed foreground owner");
                    Ok((preview(true), changed))
                }
            })
        });
        let current = published(directory.path(), None);
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(cancel_at(directory.path(), &current.id).unwrap());
        release.send(()).unwrap();
        assert!(matches!(
            worker.join().unwrap(),
            Err(UpdateError::Cancelled)
        ));
    }

    #[test]
    fn interruption_choice_advances_only_the_same_service_floor_owners() {
        use crate::model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, TerminalId};
        let mut previous = preview(false);
        previous.daemon = Some(impact::DaemonImpact {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof: HostSessionProof {
                    host_session_id: HostSessionId::new("floor"),
                    boot_id: BootId::new("floor"),
                    host_incarnation: HostIncarnation::new("floor"),
                },
                token: "floor".into(),
                inventory_generation: 1,
                live_terminals: 1,
                background_subagents: 0,
                background_identity: String::new(),
                expires_at_unix_ms: i64::MAX,
            },
            policy: impact::Policy::WhenIdle,
            blockers: Vec::new(),
            sessions: [
                (impact::SessionKind::Terminal, impact::Disposition::Blocked),
                (
                    impact::SessionKind::NativeChat,
                    impact::Disposition::WaitsForIdle,
                ),
            ]
            .into_iter()
            .map(|(kind, disposition)| impact::SessionImpact {
                id: TerminalId::new(),
                kind,
                title: "reviewed pane".into(),
                identity: "reviewed exact owner".into(),
                disposition,
                reason: String::new(),
                interrupted_subagents: Vec::new(),
            })
            .collect(),
        });
        let mut current = previous.clone();
        let daemon = current.daemon.as_mut().unwrap();
        daemon.policy = impact::Policy::Interrupt;
        daemon.sessions[1].disposition = impact::Disposition::Interrupted;
        daemon.sessions[1]
            .interrupted_subagents
            .push("background task".into());
        assert!(policy_choice_approves_service_floor(
            &previous,
            &current,
            impact::Policy::Interrupt
        ));
        for change in [
            "candidate",
            "owner",
            "extra pane",
            "new work",
            "no service floor",
        ] {
            let mut changed = current.clone();
            match change {
                "candidate" => changed.candidate.digest.push_str(" changed"),
                "owner" => changed.daemon.as_mut().unwrap().sessions[1]
                    .identity
                    .push_str(" changed"),
                "extra pane" => {
                    let daemon = changed.daemon.as_mut().unwrap();
                    let mut extra = daemon.sessions[1].clone();
                    extra.id = TerminalId::new();
                    daemon.sessions.push(extra);
                }
                "new work" => {
                    changed.daemon.as_mut().unwrap().sessions[1].disposition =
                        impact::Disposition::WaitsForIdle
                }
                "no service floor" => {
                    changed.daemon.as_mut().unwrap().sessions[0].disposition =
                        impact::Disposition::Continues
                }
                _ => unreachable!(),
            }
            assert!(
                !policy_choice_approves_service_floor(
                    &previous,
                    &changed,
                    impact::Policy::Interrupt
                ),
                "{change}"
            );
        }
    }
}
