use super::*;

const CONFIRMATION_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// How long a waiting update pauses before it reads its decision and the
/// conversations it would stop again. Half a second is too short for a
/// person answering a prompt to notice; tests answer at once, so a pause
/// would only slow them.
const DECISION_POLL: Duration = if cfg!(test) {
    Duration::from_millis(10)
} else {
    Duration::from_millis(500)
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confirmation {
    pub nonce: String,
    #[serde(default)]
    pub stop: Option<floor_display::StopList>,
    /// The command that reviews stopping blocked work from a terminal, for a
    /// window's review: a window cannot confirm that stop itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_command: Option<String>,
    pub preview: impact::Preview,
    pub expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Choice {
    Confirm,
    StopListed {
        items_digest: String,
    },
    Policy(impact::Policy),
    Cancel,
    /// The terminal's `i` choice approves interruption of the displayed work.
    /// A policy selector alone still requires ordinary review.
    InterruptNow,
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
    publish_decision(state, job, nonce, choice)
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
    File::open(state)?.sync_all()?;
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

/// Within a review, its own Cancel closes it. Closing a review that named
/// something to resolve by hand first records that refusal; cancelling the
/// whole update, as Ctrl-C does, stays a cancellation.
fn check_review_closed(state: &Path, job: &UpdateJob) -> Result<(), UpdateError> {
    if let Some(nonce) = cancellation_nonce(state, &job.id)? {
        check_cancellation(state, &job.id, &nonce)?;
    }
    if let Some(confirmation) = &job.confirmation {
        match check_cancellation(state, &job.id, &confirmation.nonce) {
            // The preview it closed is the one the person was shown.
            Err(UpdateError::Cancelled) => return Err(closed(&confirmation.preview)),
            result => result?,
        }
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
///
/// `terminal` is whether the update runs from a terminal, the only place
/// that can confirm stopping the work a blocked review names. Started from a
/// window, a blocked review is never approved into closing windows: the
/// stop it would need is refused anyway, after every window was saved and
/// frozen for nothing.
pub(super) fn wait(
    state: &Path,
    job: &mut UpdateJob,
    terminal: bool,
    observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
) -> Result<impact::Preview, UpdateError> {
    wait_with(state, job, terminal, None, false, observe)
}

/// Applying an installed version starts under the idle policy. The legacy
/// terminal review still provides an explicit way to interrupt work.
pub(super) fn wait_with_policy(
    state: &Path,
    job: &mut UpdateJob,
    terminal: bool,
    mut observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
) -> Result<impact::Preview, UpdateError> {
    check_cancelled(state, job)?;
    let preview = observe(impact::Policy::WhenIdle)?;
    let waiting = preview.daemon.as_ref().is_some_and(waiting_for_idle);
    if (terminal && waiting) || preview.refusal().is_some() {
        return wait_reviewed(state, job, terminal, preview, observe);
    }
    if waiting {
        return wait_approved(state, job, terminal, preview, observe);
    }
    Ok(preview)
}

fn waiting_for_idle(daemon: &impact::DaemonImpact) -> bool {
    daemon
        .sessions
        .iter()
        .any(|session| session.disposition == impact::Disposition::WaitsForIdle)
}

pub(super) fn wait_approved(
    state: &Path,
    job: &mut UpdateJob,
    terminal: bool,
    reviewed: impact::Preview,
    observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
) -> Result<impact::Preview, UpdateError> {
    wait_with(state, job, terminal, Some(reviewed), true, observe)
}

pub(super) fn wait_reviewed(
    state: &Path,
    job: &mut UpdateJob,
    terminal: bool,
    preview: impact::Preview,
    observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
) -> Result<impact::Preview, UpdateError> {
    wait_with(state, job, terminal, Some(preview), false, observe)
}

pub(super) fn wait_with_ledger(
    state: &Path,
    job: &mut UpdateJob,
    terminal: bool,
    preview: impact::Preview,
    approved: bool,
    ledger: &mut ledger::Ledger,
    observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
) -> Result<impact::Preview, UpdateError> {
    wait_for(
        state,
        job,
        terminal,
        Some(preview),
        approved,
        Some(ledger),
        CONFIRMATION_LIFETIME,
        observe,
    )
}

fn accept_interruptions(preview: &impact::Preview, ledger: &mut ledger::Ledger) {
    for request in preview
        .daemon
        .iter()
        .flat_map(ledger::EndRequest::interruptions)
    {
        ledger.accept_end(request);
    }
}

fn wait_with(
    state: &Path,
    job: &mut UpdateJob,
    terminal: bool,
    reviewed: Option<impact::Preview>,
    approved: bool,
    observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
) -> Result<impact::Preview, UpdateError> {
    wait_for(
        state,
        job,
        terminal,
        reviewed,
        approved,
        None,
        CONFIRMATION_LIFETIME,
        observe,
    )
}

fn wait_for(
    state: &Path,
    job: &mut UpdateJob,
    terminal: bool,
    reviewed: Option<impact::Preview>,
    mut approved: bool,
    mut scoped: Option<&mut ledger::Ledger>,
    lifetime: Duration,
    mut observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
) -> Result<impact::Preview, UpdateError> {
    let mut deadline = Instant::now() + lifetime;
    let mut policy = reviewed
        .as_ref()
        .and_then(|preview| preview.daemon.as_ref())
        .map_or(impact::Policy::WhenIdle, |daemon| daemon.policy);
    let mut preview = match reviewed {
        Some(preview) => preview,
        None => observe(policy)?,
    };
    let mut nonce = uuid::Uuid::new_v4().to_string();
    let mut handoff = None;
    loop {
        if !approved && scoped.is_none() && Instant::now() >= deadline {
            return Err(refused(&preview, true).unwrap_or_else(|| {
                invalid("Update preview expired without installation; review the update again")
            }));
        }
        {
            // A cancellation acknowledged for the previous preview must win
            // before refreshing its nonce, including while observe was busy.
            let _decisions = DecisionLease::acquire(state, &job.id)?;
            check_review_closed(state, job)?;
            // A window's review of blocked work says where it can be stopped.
            // The job's request never changes, so it is read at most once.
            let terminal_command = if !terminal
                && preview
                    .daemon
                    .as_ref()
                    .is_some_and(impact::DaemonImpact::blocked)
            {
                if handoff.is_none() {
                    handoff = Some(floor::terminal_command(state, job)?);
                }
                handoff.clone()
            } else {
                None
            };
            let mut shown = preview.clone();
            if let Some(ledger) = &scoped
                && let Some(daemon) = &mut shown.daemon
            {
                daemon.sessions.retain(|item| {
                    ledger::EndRequest::interruption(&daemon.capability.proof, item)
                        .is_none_or(|request| !ledger.allows_end(&request))
                });
            }
            job.confirmation = Some(Confirmation {
                stop: None,
                terminal_command,
                nonce: nonce.clone(),
                preview: shown,
                expires_at_unix_ms: if approved || scoped.is_some() {
                    u64::MAX
                } else {
                    now_ms().saturating_add(
                        deadline
                            .saturating_duration_since(Instant::now())
                            .as_millis() as u64,
                    )
                },
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
        check_review_closed(state, job)?;
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
                    Choice::Cancel => return Err(closed(&preview)),
                    Choice::Confirm => {
                        // A window cannot confirm stopping blocked work:
                        // refuse now rather than wait on idle sessions first.
                        if scoped.is_none()
                            && !terminal
                            && let Some(refused) = refused(&preview, false)
                        {
                            return Err(refused);
                        }
                        approved = true;
                        if let Some(ledger) = scoped.as_deref_mut() {
                            accept_interruptions(&preview, ledger);
                        }
                    }
                    Choice::StopListed { .. } => (),
                    Choice::Policy(next) => {
                        policy = next;
                        let current = observe(policy)?;
                        approved = (approved || scoped.is_some())
                            && next == impact::Policy::WhenIdle
                            && preview.same_target(&current);
                        preview = current;
                        deadline = Instant::now() + lifetime;
                        nonce = uuid::Uuid::new_v4().to_string();
                        continue;
                    }
                    Choice::InterruptNow => {
                        policy = impact::Policy::Interrupt;
                        let current = observe(policy)?;
                        approved = interruption_preserves_review(&preview, &current);
                        if approved && let Some(ledger) = scoped.as_deref_mut() {
                            accept_interruptions(&current, ledger);
                        }
                        preview = current;
                        deadline = Instant::now() + lifetime;
                        nonce = uuid::Uuid::new_v4().to_string();
                        continue;
                    }
                }
            }
            Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        let current = observe(policy)?;
        if let Some(ledger) = scoped.as_deref_mut() {
            ledger.observe_preview(&current);
        }
        let authorized = scoped.as_deref().is_some_and(|ledger| {
            preview.same_target(&current)
                && current
                    .daemon
                    .iter()
                    .flat_map(ledger::EndRequest::interruptions)
                    .all(|request| ledger.allows_end(&request))
        });
        if !authorized && !covers_interruptions(&preview, &current) {
            approved = false;
            deadline = Instant::now() + lifetime;
            nonce = uuid::Uuid::new_v4().to_string();
        }
        preview = current;
        if approved && !preview.daemon.as_ref().is_some_and(waiting_for_idle) {
            check_review_closed(state, job)?;
            // Approved from a window, a blocked review would save and freeze
            // every window, and might interrupt turns, before refusing the
            // stop only a terminal can confirm. Refuse it before any of that.
            if scoped.is_none()
                && !terminal
                && let Some(refused) = refused(&preview, false)
            {
                return Err(refused);
            }
            // Preserve this exact nonce until the source accepts prepare, so
            // Ctrl-C still cancels while windows are saving and closing.
            return Ok(preview);
        }
        thread::sleep(DECISION_POLL);
    }
}

/// Closing a review is a cancellation unless something it named had to be
/// resolved by hand first; then that is why the update was not installed.
pub(super) fn closed(preview: &impact::Preview) -> UpdateError {
    refused(preview, false).unwrap_or(UpdateError::Cancelled)
}

fn refused(preview: &impact::Preview, expired: bool) -> Option<UpdateError> {
    let refusal = preview.refusal()?;
    Some(UpdateError::Refused(Box::new(impact::Refusal {
        expired,
        ..refusal
    })))
}

pub(super) fn covers_interruptions(accepted: &impact::Preview, observed: &impact::Preview) -> bool {
    accepted.same_target(observed)
        && observed.daemon.as_ref().is_none_or(|after| {
            accepted
                .daemon
                .as_ref()
                .is_some_and(|before| before.covers_interruptions(after))
        })
}

/// The terminal's typed choice covers losses already displayed in its review.
fn interruption_preserves_review(previous: &impact::Preview, current: &impact::Preview) -> bool {
    let (Some(before), Some(after)) = (&previous.daemon, &current.daemon) else {
        return false;
    };
    before.policy == impact::Policy::WhenIdle
        && after.policy == impact::Policy::Interrupt
        && covers_interruptions(&super::review::interruption_review(previous), current)
}

/// Destructive consent is separate from the ordinary review, and bound to the
/// source proof, candidate and exact displayed items. A refreshed capability
/// alone is harmless; a different process or command needs another typed word.
pub(super) struct StopConsent<'a> {
    pub ledger: &'a mut ledger::Ledger,
    pub background: &'a mut Option<(u64, String)>,
}

impl StopConsent<'_> {
    fn pending(
        &mut self,
        preview: &impact::Preview,
        observed: &floor_display::StopList,
    ) -> floor_display::StopList {
        let proof = &preview
            .daemon
            .as_ref()
            .expect("stop review has a source")
            .capability
            .proof;
        self.ledger.observe_preview(preview);
        let mut pending = observed.clone();
        pending.items.retain(|item| {
            !self
                .ledger
                .allows_end(&ledger::EndRequest::stop(proof, item))
        });
        if self.background.as_ref().is_some_and(|(count, identity)| {
            *count == pending.background_tasks && identity.as_str() == pending.background_identity
        }) {
            pending.background_tasks = 0;
            pending.background_identity.clear();
        }
        pending
    }

    fn accept(&mut self, preview: &impact::Preview, shown: &floor_display::StopList) {
        let proof = &preview
            .daemon
            .as_ref()
            .expect("stop review has a source")
            .capability
            .proof;
        for item in &shown.items {
            self.ledger
                .accept_end(ledger::EndRequest::stop(proof, item));
        }
        if shown.background_tasks != 0 {
            *self.background = Some((shown.background_tasks, shown.background_identity.clone()));
        }
    }
}

pub(super) fn wait_stop(
    state: &Path,
    job: &mut UpdateJob,
    terminal: bool,
    mut consent: Option<StopConsent<'_>>,
    mut observe: impl FnMut() -> Result<(impact::Preview, floor_display::StopList), UpdateError>,
) -> Result<(impact::Preview, floor_display::StopList), UpdateError> {
    let deadline = Instant::now() + CONFIRMATION_LIFETIME;
    let expires = now_ms().saturating_add(CONFIRMATION_LIFETIME.as_millis() as u64);
    check_review_closed(state, job)?;
    let (mut preview, observed) = observe()?;
    check_review_closed(state, job)?;
    let mut list = match &mut consent {
        Some(consent) => {
            let pending = consent.pending(&preview, &observed);
            if pending.items.is_empty()
                && pending.background_tasks == 0
                && (terminal
                    || preview
                        .daemon
                        .as_ref()
                        .is_some_and(impact::DaemonImpact::ready))
            {
                return Ok((preview, observed));
            }
            pending
        }
        None => observed,
    };
    if !terminal && consent.is_none() {
        return Err(invalid(format!(
            "{} Details: {}",
            floor::terminal_confirmation(state, job)?,
            list.render_preview()
        )));
    }
    let mut nonce = uuid::Uuid::new_v4().to_string();
    loop {
        if consent.is_none() && Instant::now() >= deadline {
            return Err(invalid(
                "Stop confirmation expired; nothing further was stopped",
            ));
        }
        {
            // A cancellation accepted for the old nonce must win before a
            // changed inventory can publish its replacement.
            let _decisions = DecisionLease::acquire(state, &job.id)?;
            check_review_closed(state, job)?;
            job.confirmation = Some(Confirmation {
                nonce: nonce.clone(),
                preview: preview.clone(),
                stop: terminal.then(|| list.clone()),
                terminal_command: None,
                expires_at_unix_ms: if consent.is_some() { u64::MAX } else { expires },
            });
            let message = if terminal {
                format!(
                    "{}\n{}",
                    list.render_preview(),
                    floor::terminal_confirmation(state, job)?
                )
            } else {
                format!(
                    "Waiting for work that would be stopped by this update to finish.\n{}",
                    list.render_preview()
                )
            };
            set_phase(
                state,
                job,
                if terminal {
                    UpdatePhase::AwaitingConfirmation
                } else {
                    UpdatePhase::WaitingForIdle
                },
                &message,
            )?;
        }
        check_review_closed(state, job)?;
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
            return Err(closed(&preview));
        }
        let (current, current_list) = observe()?;
        check_review_closed(state, job)?;
        let same_target = preview.same_target(&current);
        let accepted = terminal
            && same_target
            && matches!(decision, Some(Choice::StopListed { items_digest }) if items_digest == list.digest());
        if let Some(consent) = &mut consent {
            if accepted {
                consent.accept(&preview, &list);
            }
            let pending = consent.pending(&current, &current_list);
            if pending.items.is_empty()
                && pending.background_tasks == 0
                && (terminal
                    || current
                        .daemon
                        .as_ref()
                        .is_some_and(impact::DaemonImpact::ready))
            {
                return Ok((current, current_list));
            }
            if accepted || !same_target || !list.covers(&pending) {
                nonce = uuid::Uuid::new_v4().to_string();
                preview = current;
                list = pending;
            } else if pending.covers(&list) {
                preview = current;
                list = pending;
            }
        } else if same_target && list.covers(&current_list) {
            if accepted {
                return Ok((current, current_list));
            }
            if current_list.covers(&list) {
                preview = current;
                list = current_list;
            }
        } else {
            nonce = uuid::Uuid::new_v4().to_string();
            preview = current;
            list = current_list;
        }
        thread::sleep(DECISION_POLL);
    }
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
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
            refusal: None,
            in_flight: Vec::new(),
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

    fn working_preview() -> impact::Preview {
        let mut preview = preview(false);
        preview.daemon = Some(impact::DaemonImpact {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof: crate::model::HostSessionProof {
                    host_session_id: crate::model::HostSessionId::new("console-update"),
                    boot_id: crate::model::BootId::new("console-update"),
                    host_incarnation: crate::model::HostIncarnation::new("console-update"),
                },
                token: "review".into(),
                inventory_generation: 1,
                live_terminals: 1,
                background_subagents: 0,
                background_identity: String::new(),
                expires_at_unix_ms: i64::MAX,
            },
            policy: impact::Policy::WhenIdle,
            blockers: Vec::new(),
            sessions: vec![impact::SessionImpact {
                id: crate::model::TerminalId::new(),
                kind: impact::SessionKind::NativeChat,
                title: "Working agent".into(),
                identity: "exact accepted turn".into(),
                disposition: impact::Disposition::WaitsForIdle,
                reason: "Finishing its turn".into(),
                interrupted_subagents: Vec::new(),
            }],
        });
        preview
    }

    #[test]
    fn terminal_i_is_the_only_confirmation_for_an_unchanged_ordinary_update() {
        struct Terminal {
            state: PathBuf,
            output: Vec<u8>,
            reads: usize,
            deadline: Instant,
        }
        impl Write for Terminal {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.output.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl TerminalReview for Terminal {
            fn status(&mut self) -> Result<Option<UpdateJob>, UpdateError> {
                Ok(Some(serde_json::from_slice(&fs::read(
                    self.state.join("status.json"),
                )?)?))
            }
            fn decide(
                &mut self,
                job: &str,
                nonce: &str,
                choice: Choice,
            ) -> Result<(), UpdateError> {
                decide_at(&self.state, job, nonce, choice)
            }
            fn cancel(&mut self, job: &str) -> Result<bool, UpdateError> {
                cancel_at(&self.state, job)
            }
            fn read_line(&mut self, answer: &mut String) -> io::Result<usize> {
                self.reads += 1;
                let text = if self.reads == 1 { "i\n" } else { "n\n" };
                answer.push_str(text);
                Ok(text.len())
            }
            fn is_terminal(&self) -> bool {
                true
            }
            fn interrupted(&self) -> bool {
                false
            }
            fn pause(&mut self) {
                assert!(Instant::now() < self.deadline, "terminal update stalled");
                thread::sleep(Duration::from_millis(10));
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        let initial = working_preview();
        let worker = thread::spawn(move || {
            let mut job = job(&path);
            let outcome = wait(&path, &mut job, true, |policy| {
                Ok(if policy == impact::Policy::Interrupt {
                    super::super::review::interruption_review(&initial)
                } else {
                    initial.clone()
                })
            });
            job.phase = if outcome.is_ok() {
                UpdatePhase::Complete
            } else {
                UpdatePhase::Failed
            };
            job.confirmation = None;
            job.message = "fixture finished".into();
            write_job(&path, &job).unwrap();
            outcome
        });
        let initial = published(directory.path(), None);
        let mut terminal = Terminal {
            state: directory.path().to_owned(),
            output: Vec::new(),
            reads: 0,
            deadline: Instant::now() + Duration::from_secs(10),
        };
        let outcome = drive_terminal_review_with(initial, false, false, &mut terminal);
        let worker_outcome = worker.join().unwrap();
        outcome.unwrap();
        worker_outcome.unwrap();
        let output = String::from_utf8(terminal.output).unwrap();
        assert_eq!(terminal.reads, 1, "{output}");
        assert_eq!(output.matches("Update cagents ").count(), 1, "{output}");
        assert!(!output.contains("Update now?"), "{output}");
    }

    #[test]
    fn a_stale_confirmation_reuses_policy_for_routine_ownership_changes() {
        struct Terminal {
            job: UpdateJob,
            output: Vec<u8>,
            reads: usize,
            decisions: Vec<String>,
            changed_owner: bool,
        }
        impl Write for Terminal {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.output.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl TerminalReview for Terminal {
            fn status(&mut self) -> Result<Option<UpdateJob>, UpdateError> {
                Ok(Some(self.job.clone()))
            }
            fn decide(&mut self, _: &str, nonce: &str, choice: Choice) -> Result<(), UpdateError> {
                assert!(matches!(choice, Choice::Confirm));
                self.decisions.push(nonce.into());
                if self.decisions.len() == 1 {
                    let confirmation = self.job.confirmation.as_mut().unwrap();
                    confirmation.nonce = "renewed".into();
                    let daemon = confirmation.preview.daemon.as_mut().unwrap();
                    daemon.capability.token = "renewed capability".into();
                    if self.changed_owner {
                        daemon.sessions[0].identity = "a replacement owner".into();
                    }
                    return Err(invalid("the update preview changed"));
                }
                self.job.phase = UpdatePhase::Complete;
                self.job.confirmation = None;
                Ok(())
            }
            fn cancel(&mut self, _: &str) -> Result<bool, UpdateError> {
                panic!("a stale review must reload without cancellation")
            }
            fn read_line(&mut self, answer: &mut String) -> io::Result<usize> {
                self.reads += 1;
                answer.push('\n');
                Ok(1)
            }
            fn is_terminal(&self) -> bool {
                true
            }
            fn interrupted(&self) -> bool {
                false
            }
            fn pause(&mut self) {}
        }
        for changed_owner in [false, true] {
            let mut preview = working_preview();
            preview.daemon.as_mut().unwrap().sessions[0].disposition = impact::Disposition::Resumes;
            let job = UpdateJob {
                id: "stale-confirmation".into(),
                version: "9.8.7".into(),
                phase: UpdatePhase::AwaitingConfirmation,
                message: String::new(),
                log_path: "/fake/update.log".into(),
                confirmation: Some(Confirmation {
                    nonce: "first".into(),
                    stop: None,
                    terminal_command: None,
                    preview,
                    expires_at_unix_ms: u64::MAX,
                }),
                refusal: None,
                in_flight: Vec::new(),
            };
            let mut terminal = Terminal {
                job: job.clone(),
                output: Vec::new(),
                reads: 0,
                decisions: Vec::new(),
                changed_owner,
            };
            drive_terminal_review_with(job, false, false, &mut terminal).unwrap();
            let output = String::from_utf8(terminal.output).unwrap();
            assert_eq!(terminal.decisions, ["first", "renewed"], "{output}");
            assert_eq!(terminal.reads, 1, "{output}");
            assert!(!output.contains("Update review changed:"), "{output}");
        }
    }

    #[test]
    fn stale_terminal_i_renews_only_the_work_that_changed_before_submission() {
        struct Terminal {
            job: UpdateJob,
            output: Vec<u8>,
            reads: usize,
            decisions: usize,
        }
        impl Write for Terminal {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.output.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl TerminalReview for Terminal {
            fn status(&mut self) -> Result<Option<UpdateJob>, UpdateError> {
                Ok(Some(self.job.clone()))
            }
            fn decide(&mut self, _: &str, nonce: &str, choice: Choice) -> Result<(), UpdateError> {
                assert!(matches!(choice, Choice::InterruptNow));
                self.decisions += 1;
                if self.decisions == 1 {
                    assert_eq!(nonce, "first");
                    let confirmation = self.job.confirmation.as_mut().unwrap();
                    confirmation.nonce = "changed".into();
                    confirmation.preview.daemon.as_mut().unwrap().sessions[0].identity =
                        "another accepted turn".into();
                    return Err(invalid("the update preview changed"));
                }
                assert_eq!(self.decisions, 2);
                assert_eq!(nonce, "changed");
                self.job.phase = UpdatePhase::Complete;
                self.job.confirmation = None;
                Ok(())
            }
            fn cancel(&mut self, _: &str) -> Result<bool, UpdateError> {
                panic!("a stale review must reload without cancellation")
            }
            fn read_line(&mut self, answer: &mut String) -> io::Result<usize> {
                self.reads += 1;
                assert!(
                    self.reads <= 2,
                    "a changed list needs only one renewed choice"
                );
                answer.push_str("i\n");
                Ok(2)
            }
            fn is_terminal(&self) -> bool {
                true
            }
            fn interrupted(&self) -> bool {
                false
            }
            fn pause(&mut self) {}
        }
        let mut preview = working_preview();
        let daemon = preview.daemon.as_mut().unwrap();
        daemon.sessions[0].title = "Changed working agent".into();
        let mut unchanged = daemon.sessions[0].clone();
        unchanged.id = crate::model::TerminalId::new();
        unchanged.title = "Unchanged working agent".into();
        daemon.sessions.push(unchanged);
        let job = UpdateJob {
            id: "stale-interrupt".into(),
            version: "9.8.7".into(),
            phase: UpdatePhase::AwaitingConfirmation,
            message: String::new(),
            log_path: "/fake/update.log".into(),
            confirmation: Some(Confirmation {
                nonce: "first".into(),
                stop: None,
                terminal_command: None,
                preview,
                expires_at_unix_ms: u64::MAX,
            }),
            refusal: None,
            in_flight: Vec::new(),
        };
        let mut terminal = Terminal {
            job: job.clone(),
            output: Vec::new(),
            reads: 0,
            decisions: 0,
        };
        drive_terminal_review_with(job, false, false, &mut terminal).unwrap();
        let output = String::from_utf8(terminal.output).unwrap();
        assert_eq!(terminal.reads, 2, "{output}");
        assert_eq!(output.matches("Update cagents ").count(), 1, "{output}");
        assert_eq!(
            output.matches("Update review changed:").count(),
            1,
            "{output}"
        );
        assert_eq!(
            output.matches("Changed working agent").count(),
            2,
            "{output}"
        );
        assert_eq!(
            output.matches("Unchanged working agent").count(),
            1,
            "{output}"
        );
        assert!(!output.contains("Update now?"), "{output}");
    }

    #[test]
    fn interrupt_now_approves_the_same_work_without_a_second_decision() {
        let directory = tempfile::tempdir().unwrap();
        let initial = working_preview();
        let path = directory.path().to_owned();
        let (finished, result) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let mut job = job(&path);
            let outcome = wait(&path, &mut job, true, |policy| {
                let mut current = initial.clone();
                if policy == impact::Policy::Interrupt {
                    current = super::super::review::interruption_review(&current);
                }
                Ok(current)
            });
            finished.send(outcome).unwrap();
        });
        let current = published(directory.path(), None);
        publish_decision(
            directory.path(),
            &current.id,
            &current.confirmation.as_ref().unwrap().nonce,
            Choice::InterruptNow,
        )
        .unwrap();
        let completed = result.recv_timeout(Duration::from_secs(5));
        if completed.is_err() {
            let latest = published(directory.path(), None);
            publish_decision(
                directory.path(),
                &latest.id,
                &latest.confirmation.as_ref().unwrap().nonce,
                Choice::Cancel,
            )
            .unwrap();
        }
        worker.join().unwrap();
        let completed = completed
            .expect("i must finish without a second confirmation")
            .unwrap();
        assert_eq!(completed.daemon.unwrap().policy, impact::Policy::Interrupt);
    }

    #[test]
    fn i_approves_already_displayed_background_work_but_not_a_replacement_child() {
        let mut previous = working_preview();
        let daemon = previous.daemon.as_mut().unwrap();
        daemon.capability.background_subagents = 1;
        daemon.capability.background_identity = "displayed task census".into();
        daemon.sessions[0]
            .interrupted_subagents
            .push("displayed child".into());
        let mut current = super::super::review::interruption_review(&previous);
        assert!(interruption_preserves_review(&previous, &current));
        current.daemon.as_mut().unwrap().sessions[0].interrupted_subagents =
            vec!["different child".into()];
        assert!(!interruption_preserves_review(&previous, &current));
    }

    #[test]
    fn interrupt_now_requires_new_review_for_changed_work_but_policy_alone_is_not_consent() {
        let previous = working_preview();
        let current = super::super::review::interruption_review(&previous);
        assert!(interruption_preserves_review(&previous, &current));
        for change in ["candidate", "owner", "agent", "unseen child"] {
            let mut changed = current.clone();
            match change {
                "candidate" => changed.candidate.digest.push_str(" changed"),
                "owner" => changed.daemon.as_mut().unwrap().sessions[0]
                    .identity
                    .push_str(" changed"),
                "agent" => {
                    changed.daemon.as_mut().unwrap().sessions[0].id =
                        crate::model::TerminalId::new()
                }
                "unseen child" => changed.daemon.as_mut().unwrap().sessions[0]
                    .interrupted_subagents
                    .push("existing but undisclosed child".into()),
                _ => unreachable!(),
            }
            assert!(
                !interruption_preserves_review(&previous, &changed),
                "{change}"
            );
        }
        let mut idle = current;
        idle.daemon.as_mut().unwrap().sessions[0].disposition = impact::Disposition::Resumes;
        assert!(
            interruption_preserves_review(&previous, &idle),
            "finished work needs no interruption"
        );
        let mut queued = idle.clone();
        queued.daemon.as_mut().unwrap().sessions[0].disposition = impact::Disposition::WaitsForIdle;
        assert!(
            interruption_preserves_review(&previous, &queued),
            "a conversation still receiving queued messages keeps waiting"
        );
    }

    #[test]
    fn changed_candidate_requires_a_new_decision_even_after_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let changed = Arc::new(AtomicBool::new(false));
        let worker_changed = changed.clone();
        let path = directory.path().to_owned();
        let worker = thread::spawn(move || {
            let mut job = job(&path);
            wait(&path, &mut job, false, |_| {
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
            wait(&path, &mut job, false, |_| Ok(preview(false)))
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
    /// Closing a blocked review is the person declining because of what it
    /// named, so the job records those reasons rather than a cancellation.
    /// Cancelling the whole update, as Ctrl-C does, stays a cancellation.
    #[test]
    fn closing_a_blocked_review_records_what_refused_the_update() {
        for whole_update in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().to_owned();
            // One preview: each fixture mints new pane IDs, which would
            // renew the nonce on every poll and race the decision below.
            let blocked = impact::tests::blocked_preview();
            let worker = thread::spawn(move || {
                let mut job = job(&path);
                initialize(&path, &job.id).unwrap();
                wait(&path, &mut job, false, |_| Ok(blocked.clone()))
            });
            let current = published(directory.path(), None);
            if whole_update {
                assert!(cancel_at(directory.path(), &current.id).unwrap());
            } else {
                decide_at(
                    directory.path(),
                    &current.id,
                    &current.confirmation.as_ref().unwrap().nonce,
                    Choice::Cancel,
                )
                .unwrap();
            }
            match worker.join().unwrap() {
                Err(UpdateError::Refused(refusal)) if !whole_update => {
                    assert_eq!(Some(*refusal), impact::tests::blocked_preview().refusal());
                }
                Err(UpdateError::Cancelled) if whole_update => {}
                other => panic!("whole update {whole_update}: {other:?}"),
            }
        }
    }

    /// Declining the stop list that a blocked review leads to is the same
    /// refusal; one with nothing blocked is a plain cancellation.
    #[test]
    fn declining_a_blocked_stop_list_records_what_refused_the_update() {
        for blocked in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().to_owned();
            let worker = thread::spawn(move || {
                let mut job = job(&path);
                let reviewed = if blocked {
                    impact::tests::blocked_preview()
                } else {
                    preview(false)
                };
                // One list: each fixture mints new IDs, which would renew
                // the nonce on every poll and race the decision below.
                let list = stop_list();
                wait_stop(&path, &mut job, true, None, || {
                    Ok((reviewed.clone(), list.clone()))
                })
            });
            let current = published(directory.path(), None);
            decide_at(
                directory.path(),
                &current.id,
                &current.confirmation.as_ref().unwrap().nonce,
                Choice::Cancel,
            )
            .unwrap();
            match worker.join().unwrap() {
                Err(UpdateError::Refused(_)) if blocked => {}
                Err(UpdateError::Cancelled) if !blocked => {}
                other => panic!("blocked {blocked}: {other:?}"),
            }
        }
    }

    /// A review left blocked until it expires was refused by the same
    /// reasons, and says that it expired.
    #[test]
    fn a_blocked_review_that_expires_records_its_reasons() {
        let directory = tempfile::tempdir().unwrap();
        let mut job = job(directory.path());
        let result = wait_for(
            directory.path(),
            &mut job,
            false,
            None,
            false,
            None,
            Duration::ZERO,
            |_| Ok(impact::tests::blocked_preview()),
        );
        let Err(UpdateError::Refused(refusal)) = result else {
            panic!("a blocked review expired as something other than a refusal: {result:?}");
        };
        assert!(refusal.expired);
        assert!(
            refusal
                .message()
                .starts_with("Update not installed: the review expired while ")
        );
        let unblocked = wait_for(
            directory.path(),
            &mut job,
            false,
            None,
            false,
            None,
            Duration::ZERO,
            |_| Ok(preview(false)),
        );
        assert!(
            matches!(unblocked, Err(UpdateError::Invalid(message)) if message.contains("expired"))
        );
    }

    #[test]
    fn cancellation_overrides_pending_approval_and_remains_available_while_closing_windows() {
        let directory = tempfile::tempdir().unwrap();
        let mut job = job(directory.path());
        job.confirmation = Some(Confirmation {
            stop: None,
            terminal_command: None,
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
            terminal_command: None,
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
            wait(&path, &mut job, false, |_| Ok(preview(false))).unwrap();
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
            wait(&path, &mut job, false, |_| {
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
                interrupted_subagents: Vec::new(),
                running: "shell running `cargo test` (since 13:40)".into(),
                recovery: "screen and directory kept; press Restart in the pane".into(),
            }],
        }
    }

    #[test]
    fn a_stop_review_asks_only_for_new_loss_on_the_same_source() {
        let preview = working_preview();
        let proof = preview.daemon.as_ref().unwrap().capability.proof.clone();
        let mut ledger = ledger::Ledger::new(
            "job".into(),
            Some(proof),
            "target".into(),
            impact::Policy::WhenIdle,
        );
        ledger.freeze();
        let mut background = None;
        let mut consent = StopConsent {
            ledger: &mut ledger,
            background: &mut background,
        };
        let previously_shown = stop_list();
        consent.accept(&preview, &previously_shown);
        let mut observed = previously_shown.clone();
        let arriving = stop_list().items.pop().unwrap();
        observed.items.push(arriving.clone());
        assert_eq!(consent.pending(&preview, &observed).items, [arriving]);

        let mut new_source = preview;
        new_source
            .daemon
            .as_mut()
            .unwrap()
            .capability
            .proof
            .host_incarnation = crate::model::HostIncarnation::new("replacement");
        assert_eq!(
            consent.pending(&new_source, &observed).items,
            observed.items
        );
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
            wait_stop(&path, &mut job, true, None, || {
                worker_observations.fetch_add(1, Ordering::Release);
                Ok((preview(false), list.clone()))
            })
        });
        let current = published(directory.path(), None);
        assert!(current.confirmation.as_ref().unwrap().stop.is_some());
        for choice in [
            Choice::Confirm,
            Choice::Policy(impact::Policy::Interrupt),
            Choice::InterruptNow,
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
            let result = wait_stop(&path, &mut job, true, None, || {
                Ok((preview(false), list.clone()))
            });
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
            wait_stop(&path, &mut job, true, None, || {
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
            wait_stop(&path, &mut job, true, None, || {
                Ok((preview(false), list.clone()))
            })
            .unwrap();
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
            wait_stop(&path, &mut job, true, None, || {
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

    /// A review whose blocked terminal needs the whole service stopped, with
    /// a conversation that waits for idle until interruption is chosen.
    fn service_floor_preview() -> impact::Preview {
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
        previous
    }

    /// `preview`, a [`service_floor_preview`], as the worker observes it
    /// under `policy`: choosing interruption turns the waiting conversation's
    /// turn into one the update interrupts.
    fn service_floor_under(preview: &impact::Preview, policy: impact::Policy) -> impact::Preview {
        let mut preview = preview.clone();
        if policy == impact::Policy::Interrupt {
            let daemon = preview.daemon.as_mut().unwrap();
            daemon.policy = impact::Policy::Interrupt;
            daemon.sessions[1].disposition = impact::Disposition::Interrupted;
        }
        preview
    }

    /// Started from a window, confirming a blocked review refuses it at once,
    /// for what holds it back: going on would save and freeze every window
    /// only to reach the stop that a terminal alone can confirm. Started from
    /// a terminal, confirming goes on to that stop.
    #[test]
    fn confirming_a_blocked_review_from_a_window_refuses_it_before_closing_windows() {
        for terminal in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().to_owned();
            // One observation of the same panes throughout: a preview built
            // afresh names new panes, which is a different review.
            let mut blocked = impact::tests::blocked_preview();
            if !terminal {
                // A window refuses at once, without first waiting for a
                // conversation that is finishing its turn.
                blocked.daemon.as_mut().unwrap().sessions[1].disposition =
                    impact::Disposition::WaitsForIdle;
            }
            let expected = blocked.refusal();
            let worker = thread::spawn(move || {
                let mut job = job(&path);
                wait(&path, &mut job, terminal, |_| Ok(blocked.clone()))
            });
            let current = published(directory.path(), None);
            let review = current.confirmation.as_ref().unwrap();
            // Only a window's review needs to say where blocked work can be
            // stopped: a terminal's review goes on to its stop list.
            assert_eq!(
                review.terminal_command.as_deref(),
                (!terminal).then_some("cagents daemon upgrade")
            );
            decide_at(
                directory.path(),
                &current.id,
                &review.nonce,
                Choice::Confirm,
            )
            .unwrap();
            match worker.join().unwrap() {
                Err(UpdateError::Refused(refusal)) if !terminal => {
                    assert_eq!(Some(*refusal), expected);
                }
                Ok(_) if terminal => {}
                other => panic!("terminal {terminal}: {other:?}"),
            }
        }
    }

    #[test]
    fn setting_a_policy_leaves_destructive_review_open() {
        for terminal in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let observations = Arc::new(AtomicUsize::new(0));
            let worker_observations = observations.clone();
            let path = directory.path().to_owned();
            let floor = service_floor_preview();
            let worker = thread::spawn(move || {
                let mut job = job(&path);
                initialize(&path, &job.id).unwrap();
                wait(&path, &mut job, terminal, |policy| {
                    worker_observations.fetch_add(1, Ordering::Release);
                    Ok(service_floor_under(&floor, policy))
                })
            });
            let current = published(directory.path(), None);
            decide_at(
                directory.path(),
                &current.id,
                &current.confirmation.as_ref().unwrap().nonce,
                Choice::Policy(impact::Policy::Interrupt),
            )
            .unwrap();
            still_waiting(directory.path(), &current, &observations, &worker);
            let reopened = published(
                directory.path(),
                Some(&current.confirmation.as_ref().unwrap().nonce),
            );
            assert_eq!(reopened.phase, UpdatePhase::AwaitingConfirmation);
            assert!(cancel_at(directory.path(), &reopened.id).unwrap());
            assert!(matches!(
                worker.join().unwrap(),
                Err(UpdateError::Cancelled)
            ));
        }
    }
}
