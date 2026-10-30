//! Bounded terminal diagnostics and current, descriptor-authorized folder actions.
use super::*;
use crate::{control::ControlClient, model::TranscriptMetadata, transcript_location};
use std::ops::Deref;

#[derive(Debug, Clone)]
pub(super) enum Failure {
    Generic(String),
    Terminal(Box<Details>),
}
impl Failure {
    pub(super) fn enrich_assigned_failure(&mut self, residency: Option<&Residency>) {
        let Self::Terminal(details) = self else {
            return;
        };
        // The first hydration failure has no typed record until lifecycle
        // bookkeeping assigns one. Fill only that fallback, preserving a
        // captured specific failure or the sign-in recovery classification.
        if details.code == "terminal_unavailable"
            && let Some(Residency::Failed { typed_error, .. }) = residency
        {
            (details.code, details.message) = known_failure(&typed_error.code);
        }
    }
    pub(super) fn message(&self) -> &str {
        match self {
            Self::Generic(message) => message,
            Self::Terminal(details) => details.message,
        }
    }
}
impl Deref for Failure {
    type Target = str;
    fn deref(&self) -> &str {
        self.message()
    }
}
impl From<String> for Failure {
    fn from(value: String) -> Self {
        Self::Generic(value)
    }
}
impl From<&str> for Failure {
    fn from(value: &str) -> Self {
        Self::Generic(value.into())
    }
}

#[derive(Debug, Clone)]
pub(super) struct Details {
    code: &'static str,
    message: &'static str,
    terminal: TerminalId,
    cwd: String,
    worktree: Option<crate::model::WorktreeId>,
    exit: Option<crate::model::TerminalExit>,
    consequence: &'static str,
}

/// The rendered diagnostic's source, never a card-only callback authority.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct DiagnosticIntent {
    card: CardId,
    session: TerminalSession,
    code: &'static str,
    exit: Option<crate::model::TerminalExit>,
}
impl std::fmt::Debug for DiagnosticIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The descriptor includes a launch environment and saved evidence.
        f.debug_struct("DiagnosticIntent")
            .field("card", &self.card)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct TranscriptIntent {
    diagnostic: DiagnosticIntent,
    operation: uuid::Uuid,
    saved_context: Option<(uuid::Uuid, PathBuf)>,
}
impl std::fmt::Debug for TranscriptIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptIntent")
            .field("card", &self.diagnostic.card)
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}
fn known_failure(code: &str) -> (&'static str, &'static str) {
    match code {
        "start_failed" => ("start_failed", "The terminal could not be started."),
        "runtime_hydration_failed" => (
            "runtime_hydration_failed",
            "The terminal could not be restored.",
        ),
        "runtime_failed" => ("runtime_failed", "The terminal connection is unavailable."),
        _ => (
            "terminal_unavailable",
            "The terminal is unavailable. Retry to reconnect or start it again.",
        ),
    }
}
/// Copy only structured fields. Never include backend error text or the launch
/// command, environment, resume token, input, output, or saved transcript path.
fn bounded_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = std::env::var_os("HOME")
        .and_then(|home| {
            path.strip_prefix(Path::new(&home))
                .ok()
                .map(|suffix| format!("~/{}", suffix.display()))
        })
        .unwrap_or_else(|| text.into_owned());
    text.chars().filter(|c| !c.is_control() && !matches!(*c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')).take(240).collect()
}
impl Details {
    fn capture(session: &TerminalSession, residency: Option<&Residency>) -> Self {
        Self::capture_context(session, residency, false)
    }
    fn capture_failure(session: &TerminalSession, residency: Option<&Residency>) -> Self {
        Self::capture_context(session, residency, true)
    }
    fn capture_context(
        session: &TerminalSession,
        residency: Option<&Residency>,
        failed: bool,
    ) -> Self {
        let typed = match &session.continuity {
            TerminalContinuity::Failed { typed_error, .. } => Some(typed_error.code.as_str()),
            _ => match residency {
                Some(Residency::Failed { typed_error, .. }) => Some(typed_error.code.as_str()),
                _ => None,
            },
        };
        let exit = match &session.continuity {
            TerminalContinuity::Exited { last_exit } => Some(last_exit.clone()),
            // RelaunchAvailable retains the previous process's exit for
            // continuity. A failed new launch must not attribute it to the
            // new terminal identity.
            TerminalContinuity::RelaunchAvailable { last_exit, .. } if !failed => last_exit.clone(),
            _ => None,
        };
        let (code, message) = if let Some(typed) = typed {
            known_failure(typed)
        } else if exit.is_some() {
            ("process_ended", "The terminal process has ended.")
        } else {
            known_failure(typed.unwrap_or(""))
        };
        let consequence = if session.continuity.live_proof().is_some() {
            "Reconnect to the existing terminal."
        } else if session.agent_resume_recipe().is_some() {
            "Resume the saved conversation in a new terminal."
        } else {
            "Run the saved command from the beginning in a new terminal."
        };
        Self {
            code,
            message,
            terminal: session.id,
            cwd: bounded_path(&session.launch.working_directory),
            worktree: None,
            exit,
            consequence,
        }
    }
    fn copy_text(&self) -> String {
        let mut text = format!(
            "Code: {}\nMessage: {}\nTerminal: {}\nDirectory: {}\nRetry: {}",
            self.code, self.message, self.terminal, self.cwd, self.consequence
        );
        if let Some(worktree) = self.worktree {
            text.push_str(&format!("\nWorktree: {worktree}"));
        }
        if let Some(exit) = &self.exit {
            if let Some(code) = exit.code {
                text.push_str(&format!("\nExit status: {code}"));
            }
            if let Some(signal) = exit.signal.as_deref() {
                let signal = match signal {
                    "SIGTERM"
                    | "SIGKILL"
                    | "SIGINT"
                    | "SIGHUP"
                    | "SIGQUIT"
                    | "SIGABRT"
                    | "SIGSEGV"
                    | "SIGPIPE"
                    | "Killed"
                    | "Terminated"
                    | "Hangup"
                    | "Interrupt"
                    | "Quit"
                    | "Aborted"
                    | "Segmentation fault"
                    | "Broken pipe"
                    | "Stopped"
                    | "Bus error"
                    | "Floating point exception" => signal,
                    _ => "Unspecified signal",
                };
                text.push_str(&format!("\nSignal: {signal}"));
            }
            if exit.occurred_at_unix_ms > 0 {
                text.push_str(&format!(
                    "\nEnded at (Unix milliseconds): {}",
                    exit.occurred_at_unix_ms
                ));
            }
        }
        text
    }
}

#[derive(Debug, Clone)]
struct Probe {
    operation: uuid::Uuid,
    session: TerminalSession,
    saved_context: Option<(uuid::Uuid, PathBuf)>,
    activation: Option<DiagnosticIntent>,
    available: bool,
    pending: bool,
    retry_at: Instant,
    /// Consecutive probes that found the transcript unavailable.
    failures: u32,
}
/// The wait before probing an unavailable transcript again. It doubles with
/// each consecutive failure, up to [`PROBE_RETRY_MAX`].
const PROBE_RETRY_BASE: Duration = Duration::from_secs(10);
const PROBE_RETRY_MAX: Duration = Duration::from_secs(5 * 60);
/// Consecutive failures that arm a timer of their own. Later retries wait for
/// another full update, so an idle app with a lost transcript goes quiet.
const PROBE_RETRY_TIMERS: u32 = 5;
impl Probe {
    /// Records that the transcript is unavailable and schedules the next
    /// probe. Answers when to wake for it, if it warrants its own timer.
    fn unavailable(&mut self, remote: bool, now: Instant) -> Option<Instant> {
        self.available = false;
        self.pending = false;
        if remote {
            // A remote device's transcript is never local, so probing again
            // cannot succeed; only another full update re-checks it.
            self.retry_at = now + PROBE_RETRY_BASE;
            return None;
        }
        self.failures = self.failures.saturating_add(1);
        let doublings = (self.failures - 1).min(16);
        self.retry_at = now
            + PROBE_RETRY_BASE
                .saturating_mul(1 << doublings)
                .min(PROBE_RETRY_MAX);
        (self.failures <= PROBE_RETRY_TIMERS).then_some(self.retry_at)
    }
}
#[derive(Debug, Clone)]
struct Attempt {
    operation: uuid::Uuid,
    details: Details,
}
#[derive(Default)]
pub(super) struct State {
    probes: HashMap<CardId, Probe>,
    attempts: HashMap<CardId, Attempt>,
    outstanding: HashMap<uuid::Uuid, CardId>,
}
#[derive(Clone)]
pub(super) struct Permit(Arc<Mutex<Option<transcript_location::ValidatedTranscript>>>);
impl std::fmt::Debug for Permit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ValidatedTranscriptPermit")
    }
}
#[derive(Debug, Clone)]
pub(super) enum Action {
    Copy(DiagnosticIntent),
    Reveal(TranscriptIntent),
    Probed {
        card: CardId,
        operation: uuid::Uuid,
        available: bool,
    },
    Prepared {
        card: CardId,
        operation: uuid::Uuid,
        owner: DiagnosticIntent,
        result: Result<Permit, transcript_location::Unavailable>,
    },
    Revealed {
        card: CardId,
        operation: uuid::Uuid,
        owner: DiagnosticIntent,
        result: Result<(), transcript_location::Unavailable>,
    },
    RetryFinished {
        card: CardId,
        operation: uuid::Uuid,
        terminal: TerminalId,
        result: Result<StartedTerminal, String>,
    },
    /// An unavailable probe's retry time has come. Handling it runs the full
    /// update tail, which probes again.
    ProbeRetryDue,
}
impl Action {
    pub(super) fn is_user_action(&self) -> bool {
        matches!(self, Self::Copy(_) | Self::Reveal(_))
    }
}
/// Probes are retried by the update tail, which runs only for messages that
/// may change what it reads; this message brings it at the retry time.
fn probe_retry_timer(at: Instant) -> Task<Message> {
    #[cfg(test)]
    tests::ARMED.with(|armed| armed.borrow_mut().push(at));
    cosmic::task::future(async move {
        tokio::time::sleep_until(at.into()).await;
        Message::TerminalFailure(Action::ProbeRetryDue)
    })
}
fn fresh_transcript(
    session: &TerminalSession,
    saved_socket: Option<&Path>,
) -> Result<transcript_location::ValidatedTranscript, transcript_location::Unavailable> {
    use transcript_location::Unavailable;
    // Connecting to the existing control socket never starts or repairs a host.
    let socket = match saved_socket {
        Some(socket) => socket.to_path_buf(),
        None => {
            crate::terminal::ipc::daemon_socket_path().map_err(|_| Unavailable::MetadataMissing)?
        }
    };
    let mut client = ControlClient::connect(&socket).map_err(|_| Unavailable::MetadataMissing)?;
    let descriptors = client.list().map_err(|_| Unavailable::MetadataMissing)?;
    let metadata: TranscriptMetadata = descriptors
        .into_iter()
        .find(|descriptor| {
            descriptor.session.id == session.id && descriptor.session.launch == session.launch
        })
        .and_then(|descriptor| descriptor.transcript)
        .ok_or(Unavailable::MetadataMissing)?;
    transcript_location::open(session.id, &metadata)
}
impl Cagents {
    fn terminal_diagnostic_intent(&self, card: CardId) -> Option<DiagnosticIntent> {
        let state = self
            .card_state(card)
            .filter(|state| state.existence.is_open())?;
        let CardKind::Terminal(session) = &state.kind else {
            return None;
        };
        let details = self.terminal_details(card)?;
        Some(DiagnosticIntent {
            card,
            session: session.clone(),
            code: details.code,
            exit: details.exit,
        })
    }

    fn diagnostic_intent_is_current(&self, owner: &DiagnosticIntent) -> bool {
        self.terminal_diagnostic_intent(owner.card).as_ref() == Some(owner)
    }

    fn terminal_transcript_intent(&self, card: CardId) -> Option<TranscriptIntent> {
        let probe = self.terminal_failure.probes.get(&card)?;
        if !probe.available || probe.pending || !self.probe_current(card, probe.operation) {
            return None;
        }
        Some(TranscriptIntent {
            diagnostic: self.terminal_diagnostic_intent(card)?,
            operation: probe.operation,
            saved_context: probe.saved_context.clone(),
        })
    }

    pub(super) fn reveal_terminal_transcript(&mut self, card: CardId) -> Task<Message> {
        let Some(intent) = self.terminal_transcript_intent(card) else {
            return Task::none();
        };
        self.update_terminal_failure(Action::Reveal(intent))
    }

    pub(super) fn terminal_details(&self, card: CardId) -> Option<Details> {
        let state = self
            .card_state(card)
            .filter(|state| state.existence.is_open())?;
        let CardKind::Terminal(session) = &state.kind else {
            return None;
        };
        if let Some(CardRuntime::Failed(Failure::Terminal(details))) = self.cards.get(&card)
            && details.terminal == session.id
            && details.cwd == bounded_path(&session.launch.working_directory)
        {
            return Some((**details).clone());
        }
        self.capture_terminal_details(card, false)
    }
    fn capture_terminal_details(&self, card: CardId, failed: bool) -> Option<Details> {
        let state = self
            .card_state(card)
            .filter(|state| state.existence.is_open())?;
        let CardKind::Terminal(session) = &state.kind else {
            return None;
        };
        let mut details = if failed {
            Details::capture_failure(session, Some(&state.residency))
        } else {
            Details::capture(session, Some(&state.residency))
        };
        if self.completed_terminal_has_native_owner(card) {
            details.consequence = "Open the existing conversation in the chat view.";
        }
        // An explicitly filed card can name its known worktree. A launch
        // directory alone does not invent ownership or a diagnostic identity.
        if let Some(scope) = self.durable.card_scopes.get(&card) {
            details.worktree = self
                .sidebar_model
                .worktree_at(scope)
                .map(|key| key.worktree_id);
        }
        Some(details)
    }
    pub(super) fn terminal_failure_payload(&self, card: CardId, generic: String) -> Failure {
        // A new failure captures current state before the generic residency
        // transition. The view can subsequently reuse that bounded record.
        let Some(mut details) = self.capture_terminal_details(card, true) else {
            return generic.into();
        };
        if self.card_state(card).is_some_and(|state| matches!(&state.kind, CardKind::Terminal(session) if account::login::is_codex_browser(&session.launch)))
            && account::login::callback_failed(&generic)
        {
            // This fixed explanation is also the existing sign-in recovery
            // classifier's contract. Keep that behavior without keeping the
            // backend string that happened to report the known port failure.
            details.code = "login_callback_busy";
            details.message = account::login::PORT_BUSY;
        }
        Failure::Terminal(Box::new(details))
    }
    fn probe_current(&self, card: CardId, operation: uuid::Uuid) -> bool {
        !crate::device::is_remote()
            && self.terminal_failure.probes.get(&card).is_some_and(|probe| {
                probe.operation == operation
                    && probe.activation.as_ref().is_none_or(|owner| self.diagnostic_intent_is_current(owner))
                    && (self.saved_terminal_current(card).is_none() || probe.saved_context.is_some())
                    && probe.saved_context == self.saved_terminal_transcript_context(card)
                    && self.card_state(card).is_some_and(|state| {
                        state.existence.is_open()
                            && matches!(&state.kind, CardKind::Terminal(session) if session == &probe.session)
                    })
            })
    }
    pub(super) fn terminal_transcript_probes(&mut self) -> Task<Message> {
        let sessions: HashMap<_, _> = self
            .durable
            .cards
            .iter()
            .filter(|state| state.existence.is_open())
            .filter(|state| {
                matches!(
                    self.cards.get(&state.id),
                    Some(CardRuntime::Failed(Failure::Terminal(_)) | CardRuntime::Completed)
                ) || ended_terminal(Some(state)).is_some()
                    || self.saved_terminal_diagnostics_allowed(state.id)
            })
            .filter_map(|state| {
                if let CardKind::Terminal(session) = &state.kind {
                    Some((state.id, session))
                } else {
                    None
                }
            })
            .collect();
        self.terminal_failure.probes.retain(|card, probe| {
            sessions
                .get(card)
                .is_some_and(|session| **session == probe.session)
        });
        let mut tasks = Vec::new();
        for (card, session) in sessions {
            let saved_context = self.saved_terminal_transcript_context(card);
            let replaced_activation = self
                .terminal_failure
                .probes
                .get(&card)
                .and_then(|probe| probe.activation.as_ref())
                .is_some_and(|owner| !self.diagnostic_intent_is_current(owner));
            if self
                .terminal_failure
                .probes
                .get(&card)
                .is_some_and(|probe| {
                    replaced_activation
                        || probe.saved_context != saved_context
                        || (saved_context.is_some() && &probe.session != session)
                })
            {
                self.terminal_failure.probes.remove(&card);
            }
            if self.workspace.visibility(card) != Some(VisibilityClass::Visible) {
                continue;
            }
            if self
                .terminal_failure
                .probes
                .get(&card)
                .is_some_and(|probe| {
                    probe.pending || probe.available || Instant::now() < probe.retry_at
                })
            {
                continue;
            }
            let session = session.clone();
            let operation = uuid::Uuid::new_v4();
            // A retry keeps counting the failures before it.
            let failures = self
                .terminal_failure
                .probes
                .get(&card)
                .map_or(0, |probe| probe.failures);
            let remote = crate::device::is_remote();
            let mut probe = Probe {
                operation,
                session: session.clone(),
                saved_context: saved_context.clone(),
                activation: None,
                available: false,
                pending: true,
                retry_at: Instant::now(),
                failures,
            };
            if remote {
                let _ = probe.unavailable(true, Instant::now());
            }
            self.terminal_failure.probes.insert(card, probe);
            if remote {
                continue;
            }
            tasks.push(cosmic::task::future(async move {
                let available = tokio::task::spawn_blocking(move || {
                    fresh_transcript(
                        &session,
                        saved_context.as_ref().map(|(_, socket)| socket.as_path()),
                    )
                    .is_ok()
                })
                .await
                .unwrap_or(false);
                Message::TerminalFailure(Action::Probed {
                    card,
                    operation,
                    available,
                })
            }));
        }
        Task::batch(tasks)
    }
    pub(super) fn terminal_diagnostic_summary(&self, card: CardId) -> Element<'_, Message> {
        let Some(details) = self.terminal_details(card) else {
            return iced::widget::Space::new().into();
        };
        let mut rows = vec![
            design::caption(format!("Code: {}", details.code)).into(),
            design::caption(format!("Terminal: {}", details.terminal)).into(),
            design::caption(format!("Directory: {}", details.cwd)).into(),
        ];
        if let Some(worktree) = details.worktree {
            rows.push(design::caption(format!("Worktree: {worktree}")).into());
        }
        rows.push(design::caption(details.consequence).into());
        widget::column::with_children(rows)
            .spacing(space::XS)
            .into()
    }
    pub(super) fn terminal_diagnostic_controls(&self, card: CardId) -> Element<'static, Message> {
        let Some(intent) = self.terminal_diagnostic_intent(card) else {
            return iced::widget::Space::new().into();
        };
        crate::ui_controls::owned(
            intent.clone(),
            widget::flex_row(vec![
                crate::ui_controls::button::secondary("Copy details")
                    .id(card_control_id(card, "failure.copy"))
                    .on_press(Message::TerminalFailure(Action::Copy(intent)))
                    .into(),
                self.terminal_transcript_control(
                    card,
                    self.terminal_transcript_intent(card)
                        .map(|intent| Message::TerminalFailure(Action::Reveal(intent))),
                ),
            ])
            .spacing(space::S as u16),
        )
    }
    pub(super) fn terminal_transcript_control(
        &self,
        card: CardId,
        action: Option<Message>,
    ) -> Element<'static, Message> {
        let probe = self.terminal_failure.probes.get(&card);
        let remote = crate::device::is_remote();
        let owner = self.terminal_transcript_intent(card);
        let available = owner.is_some();
        let folder = crate::ui_controls::button::secondary(
            if !remote && probe.is_some_and(|probe| probe.pending) {
                "Checking transcript…"
            } else if available {
                "Open transcript folder"
            } else {
                "Transcript unavailable"
            },
        )
        .id(card_control_id(card, "failure.folder"));
        let folder = if let Some(action) = action.filter(|_| available) {
            folder.on_press(action)
        } else {
            folder
        };
        crate::ui_controls::owned(owner, folder)
    }

    pub(super) fn terminal_runtime_claims(&self, terminal: TerminalId) -> bool {
        self.durable.cards.iter().filter(|state| state.existence.is_open()).any(|state| match &state.kind {
            CardKind::Terminal(session) if session.id == terminal => {
                matches!(self.cards.get(&state.id), Some(CardRuntime::Terminal(_))) || self.terminal_retry_pending(state.id)
            }
            CardKind::Chat(session) if session.id == terminal => {
                matches!(self.cards.get(&state.id), Some(CardRuntime::Chat(runtime)) if runtime.session.id == terminal)
            }
            _ => false,
        })
    }
    pub(super) fn terminal_retry_pending(&self, card: CardId) -> bool {
        self.terminal_failure.attempts.contains_key(&card)
    }
    pub(super) fn terminal_retry_view(&self, card: CardId) -> Option<Element<'_, Message>> {
        let attempt = self.terminal_failure.attempts.get(&card)?;
        Some(
            widget::container(
                widget::column::with_children(vec![
                    crate::ui_controls::activity_spinner(24.0, design::accent_color).into(),
                    design::title("Launching terminal…").into(),
                    design::caption(attempt.details.consequence).into(),
                    widget::row::with_children(vec![
                        crate::ui_controls::button::primary("Retrying…").into(),
                        crate::ui_controls::button::secondary("Close")
                            .on_press(Message::CloseCard(card))
                            .into(),
                    ])
                    .spacing(space::S)
                    .into(),
                ])
                .spacing(space::M),
            )
            .padding(space::XL)
            .center(Length::Fill)
            .into(),
        )
    }
    pub(super) fn track_terminal_retry(
        &mut self,
        card: CardId,
        details: Details,
        task: Task<Message>,
    ) -> Task<Message> {
        if !self.terminal_start_in_flight(card) {
            return task;
        }
        let operation = uuid::Uuid::new_v4();
        self.terminal_failure
            .attempts
            .insert(card, Attempt { operation, details });
        self.terminal_failure.outstanding.insert(operation, card);
        self.terminal_failure.probes.remove(&card);
        task.map(move |action| match action {
            cosmic::Action::App(Message::TerminalStarted {
                card,
                terminal,
                result,
            }) => cosmic::Action::App(Message::TerminalFailure(Action::RetryFinished {
                card,
                operation,
                terminal,
                result,
            })),
            other => other,
        })
    }
    pub(super) fn update_terminal_failure(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Copy(owner) => self
                .diagnostic_intent_is_current(&owner)
                .then(|| self.terminal_details(owner.card))
                .flatten()
                .map(|details| {
                    chat_ui::invalidate_copy_intent();
                    iced::clipboard::write(details.copy_text()).map(cosmic::Action::App)
                })
                .unwrap_or_else(Task::none),
            Action::Probed {
                card,
                operation,
                available,
            } => {
                if self.probe_current(card, operation)
                    && let Some(probe) = self.terminal_failure.probes.get_mut(&card)
                {
                    if available {
                        probe.available = true;
                        probe.pending = false;
                        probe.failures = 0;
                    } else if let Some(at) = probe.unavailable(false, Instant::now()) {
                        return probe_retry_timer(at);
                    }
                }
                Task::none()
            }
            Action::ProbeRetryDue => Task::none(),
            Action::Reveal(intent) => {
                let card = intent.diagnostic.card;
                if self.terminal_transcript_intent(card).as_ref() != Some(&intent) {
                    return Task::none();
                }
                if self
                    .terminal_failure
                    .probes
                    .get(&card)
                    .is_none_or(|probe| !self.probe_current(card, probe.operation))
                {
                    return Task::none();
                }
                let Some(probe) = self
                    .terminal_failure
                    .probes
                    .get_mut(&card)
                    .filter(|probe| probe.available && !probe.pending)
                else {
                    return Task::none();
                };
                if crate::device::is_remote() {
                    return Task::none();
                }
                probe.pending = true;
                probe.operation = uuid::Uuid::new_v4();
                probe.activation = Some(intent.diagnostic.clone());
                let operation = probe.operation;
                let session = probe.session.clone();
                let saved_context = probe.saved_context.clone();
                let owner = intent.diagnostic;
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        fresh_transcript(
                            &session,
                            saved_context.as_ref().map(|(_, socket)| socket.as_path()),
                        )
                        .map(|transcript| Permit(Arc::new(Mutex::new(Some(transcript)))))
                    })
                    .await
                    .unwrap_or(Err(transcript_location::Unavailable::MetadataMissing));
                    Message::TerminalFailure(Action::Prepared {
                        card,
                        operation,
                        owner,
                        result,
                    })
                })
            }
            Action::Prepared {
                card,
                operation,
                owner,
                result,
            } => {
                if owner.card != card
                    || !self.diagnostic_intent_is_current(&owner)
                    || !self.probe_current(card, operation)
                {
                    return Task::none();
                }
                match result {
                    Ok(permit) => {
                        let Some(transcript) = permit
                            .0
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .take()
                        else {
                            return Task::none();
                        };
                        cosmic::task::future(async move {
                            Message::TerminalFailure(Action::Revealed {
                                card,
                                operation,
                                owner,
                                result: transcript.reveal().await,
                            })
                        })
                    }
                    Err(error) => self.update_terminal_failure(Action::Revealed {
                        card,
                        operation,
                        owner,
                        result: Err(error),
                    }),
                }
            }
            Action::Revealed {
                card,
                operation,
                owner,
                result,
            } => {
                let mut retry = Task::none();
                if owner.card == card
                    && self.diagnostic_intent_is_current(&owner)
                    && self.probe_current(card, operation)
                {
                    if let Some(probe) = self.terminal_failure.probes.get_mut(&card) {
                        if result.is_err() {
                            if let Some(at) = probe.unavailable(false, Instant::now()) {
                                retry = probe_retry_timer(at);
                            }
                        } else {
                            probe.pending = false;
                        }
                    }
                    if result.is_err() {
                        self.status = Some(TransientNotice::warning(
                            "The transcript folder is unavailable",
                        ));
                    }
                }
                retry
            }
            Action::RetryFinished {
                card,
                operation,
                terminal,
                result,
            } => {
                if self.terminal_failure.outstanding.get(&operation) != Some(&card) {
                    // Duplicate delivery has no outstanding start to settle. A
                    // cloned completion must never terminate the winning PTY.
                    return Task::none();
                }
                self.terminal_failure.outstanding.remove(&operation);
                let current_operation = self
                    .terminal_failure
                    .attempts
                    .get(&card)
                    .is_some_and(|attempt| attempt.operation == operation);
                if current_operation {
                    self.terminal_failure.attempts.remove(&card);
                }
                // Existing identity/close checks retire stale spawned handles
                // and decrement the actual start count even after presentation
                // was replaced or its card was closed.
                self.terminal_started_for_operation(card, terminal, result, current_operation)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app_and_session() -> (Cagents, CardId, TerminalSession) {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.durable.cards[0].id;
        let CardKind::Terminal(session) = &app.durable.cards[0].kind else {
            unreachable!()
        };
        let session = session.clone();
        app.lifecycle.register(
            card,
            Exposure::Visible,
            Residency::Hydrating { epoch: 1 },
            Instant::now(),
        );
        (app, card, session)
    }

    fn ready_diagnostic_probe(app: &mut Cagents, card: CardId) {
        let CardKind::Terminal(session) = &app.card_state(card).unwrap().kind else {
            unreachable!()
        };
        let session = session.clone();
        app.terminal_failure.probes.insert(
            card,
            Probe {
                operation: uuid::Uuid::new_v4(),
                session,
                saved_context: None,
                activation: None,
                available: true,
                pending: false,
                retry_at: Instant::now(),
                failures: 0,
            },
        );
    }

    fn replace_diagnostic_source(app: &mut Cagents, card: CardId, scenario: u8) {
        if scenario == 0 {
            return;
        }
        if matches!(scenario, 4 | 5) {
            let mut details = app.terminal_details(card).unwrap();
            if scenario == 4 {
                details.code = "runtime_failed";
                details.message = "The terminal connection is unavailable.";
            } else {
                details.worktree = Some(crate::model::WorktreeId::new());
            }
            app.cards.insert(
                card,
                CardRuntime::Failed(Failure::Terminal(Box::new(details))),
            );
            if scenario == 4 {
                ready_diagnostic_probe(app, card);
            }
            return;
        }
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        match scenario {
            1 => session.id = TerminalId::new(),
            2 => session.launch.arguments.push("different recipe".into()),
            3 => {
                session.continuity = TerminalContinuity::Live {
                    host_session_id: "replacement-session".into(),
                    boot_id: "replacement-boot".into(),
                    host_incarnation: "replacement-incarnation".into(),
                }
            }
            _ => unreachable!(),
        }
        ready_diagnostic_probe(app, card);
    }

    fn diagnostic_clipboard_text(task: Task<Message>) -> String {
        use cosmic::iced::futures::StreamExt;
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let mut effects = iced::runtime::task::into_stream(task).unwrap();
                tokio::time::timeout(Duration::from_secs(1), async {
                    while let Some(effect) = effects.next().await {
                        if let iced::runtime::Action::Clipboard(
                            iced::runtime::clipboard::Action::Write { contents, .. },
                        ) = effect
                        {
                            return contents;
                        }
                    }
                    panic!("Copy must publish current bounded diagnostics");
                })
                .await
                .unwrap()
            })
    }

    #[test]
    fn diagnostic_native_press_and_fresh_click_keep_their_original_source() {
        use iced::advanced::clipboard;
        use iced::runtime::user_interface::{Cache, UserInterface};

        fn bounds(
            tree: &iced_accessibility::A11yTree,
            name: &str,
            size: iced::Size,
        ) -> iced::Rectangle {
            let nodes: Vec<_> = tree
                .root()
                .iter()
                .chain(tree.children())
                .filter(|node| {
                    node.node().role() == iced_accessibility::accesskit::Role::Button
                        && node.node().label() == Some(name)
                        && !node.node().is_hidden()
                        && !node.node().is_disabled()
                })
                .filter_map(|node| node.node().bounds())
                .collect();
            assert_eq!(nodes.len(), 1, "one actual diagnostic button: {name}");
            let rect = nodes[0];
            assert!(
                rect.x0 >= 0.0
                    && rect.y0 >= 0.0
                    && rect.x1 > rect.x0
                    && rect.y1 > rect.y0
                    && rect.x1 <= f64::from(size.width)
                    && rect.y1 <= f64::from(size.height),
                "diagnostic button must remain inside the viewport: {name}"
            );
            iced::Rectangle {
                x: rect.x0 as f32,
                y: rect.y0 as f32,
                width: (rect.x1 - rect.x0) as f32,
                height: (rect.y1 - rect.y0) as f32,
            }
        }

        for (copy, width) in [(false, 320.0), (false, 600.0), (true, 320.0), (true, 600.0)] {
            for scenario in 0..6 {
                let (mut app, card, _) = app_and_session();
                ready_diagnostic_probe(&mut app, card);
                let mut renderer = super::super::tests::headless_renderer();
                let size = iced::Size::new(width, 100.0);
                let name = if copy {
                    "Copy details"
                } else {
                    "Open transcript folder"
                };
                let mut ui = UserInterface::build(
                    app.terminal_diagnostic_controls(card),
                    size,
                    Cache::new(),
                    &mut renderer,
                );
                let pointer =
                    bounds(&ui.a11y_nodes(iced::mouse::Cursor::Unavailable), name, size).center();
                let mut messages = Vec::new();
                ui.update(
                    &[iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                        iced::mouse::Button::Left,
                    ))],
                    iced::mouse::Cursor::Available(pointer),
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                assert!(messages.is_empty());
                let cache = ui.into_cache();
                replace_diagnostic_source(&mut app, card, scenario);
                let mut ui = UserInterface::build(
                    app.terminal_diagnostic_controls(card),
                    size,
                    cache,
                    &mut renderer,
                );
                let pointer =
                    bounds(&ui.a11y_nodes(iced::mouse::Cursor::Unavailable), name, size).center();
                ui.update(
                    &[iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                        iced::mouse::Button::Left,
                    ))],
                    iced::mouse::Cursor::Available(pointer),
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                if matches!(scenario, 0 | 5) {
                    assert_eq!(messages.len(), 1, "unchanged owner keeps its native press");
                } else {
                    assert!(
                        messages.is_empty(),
                        "held diagnostic press retargeted, copy={copy}, scenario={scenario}"
                    );
                    let released = ui.into_cache();
                    let mut ui = UserInterface::build(
                        app.terminal_diagnostic_controls(card),
                        size,
                        released,
                        &mut renderer,
                    );
                    for event in [
                        iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
                        iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
                    ] {
                        ui.update(
                            &[iced::Event::Mouse(event)],
                            iced::mouse::Cursor::Available(pointer),
                            &mut renderer,
                            &mut clipboard::Null,
                            &mut messages,
                        );
                    }
                    assert_eq!(messages.len(), 1, "replacement accepts a fresh click");
                }
                assert!(match messages.pop().unwrap() {
                    Message::TerminalFailure(Action::Copy(owner)) if copy => {
                        assert!(app.diagnostic_intent_is_current(&owner));
                        let expected = app.terminal_details(card).unwrap().copy_text();
                        let copied = diagnostic_clipboard_text(
                            app.update_terminal_failure(Action::Copy(owner)),
                        );
                        assert_eq!(
                            copied, expected,
                            "same-incident enrichment copies current fields"
                        );
                        true
                    }
                    Message::TerminalFailure(Action::Reveal(owner)) if !copy =>
                        app.terminal_transcript_intent(card).as_ref() == Some(&owner),
                    _ => false,
                });
            }
        }
    }

    #[test]
    fn queued_diagnostics_and_late_reveal_results_cannot_follow_replaced_evidence() {
        for scenario in 1..5 {
            let (mut app, card, _) = app_and_session();
            ready_diagnostic_probe(&mut app, card);
            let old_copy = app.terminal_diagnostic_intent(card).unwrap();
            let old_reveal = app.terminal_transcript_intent(card).unwrap();
            let mut redacted = old_copy.clone();
            redacted
                .session
                .launch
                .environment
                .insert("TOKEN".into(), "PRIVATE_INTENT_EVIDENCE".into());
            assert!(!format!("{:?}", Action::Copy(redacted)).contains("PRIVATE_INTENT_EVIDENCE"));
            replace_diagnostic_source(&mut app, card, scenario);
            let operation = app.terminal_failure.probes[&card].operation;
            assert!(
                iced::runtime::task::into_stream(
                    app.update_terminal_failure(Action::Copy(old_copy.clone()))
                )
                .is_none()
            );
            assert!(
                iced::runtime::task::into_stream(
                    app.update_terminal_failure(Action::Reveal(old_reveal))
                )
                .is_none()
            );
            assert_eq!(app.terminal_failure.probes[&card].operation, operation);
            assert!(!app.terminal_failure.probes[&card].pending);
            let expected = app.terminal_details(card).unwrap().copy_text();
            let owner = app.terminal_diagnostic_intent(card).unwrap();
            let copy = app.update_terminal_failure(Action::Copy(owner.clone()));
            let contents = diagnostic_clipboard_text(copy);
            assert_eq!(contents, expected);
            // Construct, then drop the genuine preparation task: no host or
            // file manager is contacted by this ownership regression.
            drop(app.reveal_terminal_transcript(card));
            let operation = app.terminal_failure.probes[&card].operation;
            assert!(app.terminal_failure.probes[&card].pending);
            for prepared in [false, true] {
                let action = if prepared {
                    Action::Prepared {
                        card,
                        operation,
                        owner: old_copy.clone(),
                        result: Err(transcript_location::Unavailable::Changed),
                    }
                } else {
                    Action::Revealed {
                        card,
                        operation,
                        owner: old_copy.clone(),
                        result: Err(transcript_location::Unavailable::Changed),
                    }
                };
                assert!(
                    iced::runtime::task::into_stream(app.update_terminal_failure(action)).is_none()
                );
                assert!(app.terminal_failure.probes[&card].pending);
                assert!(app.status.is_none());
            }
            drop(app.update_terminal_failure(Action::Prepared {
                card,
                operation,
                owner,
                result: Err(transcript_location::Unavailable::Changed),
            }));
            assert!(!app.terminal_failure.probes[&card].pending);
            assert!(
                app.status.is_some(),
                "current preparation failure still settles"
            );
        }
        let (mut app, card, _) = app_and_session();
        app.workspace.resize_viewport(1200.0);
        ready_diagnostic_probe(&mut app, card);
        drop(app.reveal_terminal_transcript(card));
        let operation = app.terminal_failure.probes[&card].operation;
        app.terminal_failure.probes.get_mut(&card).unwrap().failures = 4;
        // A changed incident must retire an old in-flight activation even
        // when the terminal descriptor itself stays the same. Otherwise the
        // rejected completion would leave this probe pending forever.
        let mut details = app.terminal_details(card).unwrap();
        details.code = "runtime_failed";
        app.cards.insert(
            card,
            CardRuntime::Failed(Failure::Terminal(Box::new(details))),
        );
        drop(app.terminal_transcript_probes());
        let probe = &app.terminal_failure.probes[&card];
        assert_ne!(probe.operation, operation);
        assert!(probe.activation.is_none());
        assert_eq!(probe.failures, 0, "a new incident starts a fresh backoff");
    }

    #[test]
    fn stale_diagnostic_errors_cannot_change_current_retry_deadline_or_arm_a_timer() {
        for scenario in 1..5 {
            let (mut app, card, _) = app_and_session();
            ready_diagnostic_probe(&mut app, card);
            let old_owner = app.terminal_diagnostic_intent(card).unwrap();
            // Admit the original action, without polling any host/file-manager work.
            drop(app.reveal_terminal_transcript(card));
            replace_diagnostic_source(&mut app, card, scenario);
            let current_owner = app.terminal_diagnostic_intent(card).unwrap();
            drop(app.reveal_terminal_transcript(card));
            let probe = app.terminal_failure.probes.get_mut(&card).unwrap();
            probe.failures = 2;
            probe.retry_at = Instant::now() + Duration::from_secs(80);
            let expected = (
                probe.operation,
                probe.available,
                probe.pending,
                probe.retry_at,
                probe.failures,
            );
            armed();
            for prepared in [false, true] {
                // Deliberately use the *current* operation. Only the original
                // diagnostic source can reject this stale completion.
                let action = if prepared {
                    Action::Prepared {
                        card,
                        operation: expected.0,
                        owner: old_owner.clone(),
                        result: Err(transcript_location::Unavailable::Changed),
                    }
                } else {
                    Action::Revealed {
                        card,
                        operation: expected.0,
                        owner: old_owner.clone(),
                        result: Err(transcript_location::Unavailable::Changed),
                    }
                };
                assert_eq!(app.update_terminal_failure(action).units(), 0);
                let probe = &app.terminal_failure.probes[&card];
                assert_eq!(
                    (
                        probe.operation,
                        probe.available,
                        probe.pending,
                        probe.retry_at,
                        probe.failures,
                    ),
                    expected,
                    "an old incident must not consume the current retry budget"
                );
                assert_eq!(probe.activation.as_ref(), Some(&current_owner));
                assert!(app.status.is_none());
                assert!(armed().is_empty());
            }
            let before = Instant::now();
            assert_eq!(
                app.update_terminal_failure(Action::Prepared {
                    card,
                    operation: expected.0,
                    owner: current_owner,
                    result: Err(transcript_location::Unavailable::Changed),
                })
                .units(),
                1,
                "a current error still returns its retry task"
            );
            let probe = &app.terminal_failure.probes[&card];
            assert_eq!(probe.failures, 3);
            assert!(!probe.pending && !probe.available);
            assert!(probe.retry_at >= before + Duration::from_secs(40));
            assert_eq!(armed(), vec![probe.retry_at]);
            assert!(app.status.is_some());
        }
    }

    #[test]
    fn a_retired_incidents_retry_wake_respects_the_current_deadline_and_pending_probe() {
        let (mut app, card, original) = app_and_session();
        app.cards.insert(card, CardRuntime::Completed);
        let _ = app.workspace.resize_viewport(1200.0);
        let operation = probing(&mut app, card, &original);
        armed();
        drop(app.update_terminal_failure(Action::Probed {
            card,
            operation,
            available: false,
        }));
        assert_eq!(armed().len(), 1, "the retired owner really armed a wake");
        replace_diagnostic_source(&mut app, card, 1);
        let CardKind::Terminal(current) = &app.card_state(card).unwrap().kind else {
            unreachable!()
        };
        let current = current.clone();
        // Put the replacement at the published policy's last backoff step
        // through actual reducer replies, without polling transcript I/O.
        for _ in 0..6 {
            let operation = probing(&mut app, card, &current);
            drop(app.update_terminal_failure(Action::Probed {
                card,
                operation,
                available: false,
            }));
        }
        armed();
        let probe = &app.terminal_failure.probes[&card];
        let expected = (probe.operation, probe.retry_at, probe.failures);
        assert_eq!(probe.failures, 6);
        assert!(Instant::now() < probe.retry_at);
        // Deliver the real App action an old timer would publish. It has no
        // authority to advance the replacement's still-future deadline.
        drop(app.update(Message::TerminalFailure(Action::ProbeRetryDue)));
        let probe = &app.terminal_failure.probes[&card];
        assert_eq!((probe.operation, probe.retry_at, probe.failures), expected);
        assert!(!probe.pending && !probe.available);
        assert_eq!(probe.session, current);
        assert!(armed().is_empty());
        app.terminal_failure.probes.get_mut(&card).unwrap().retry_at = Instant::now();
        drop(app.update(Message::TerminalFailure(Action::ProbeRetryDue)));
        let probe = &app.terminal_failure.probes[&card];
        let current_operation = probe.operation;
        assert_ne!(current_operation, expected.0, "a due current probe starts");
        assert!(probe.pending);
        assert_eq!(probe.failures, 6);
        assert_eq!(probe.session, current);
        assert!(probe.activation.is_none());
        // A coincident retired wake cannot start a second pending probe.
        drop(app.update(Message::TerminalFailure(Action::ProbeRetryDue)));
        let probe = &app.terminal_failure.probes[&card];
        assert_eq!(probe.operation, current_operation);
        assert!(probe.pending);
        assert_eq!(probe.failures, 6);
        assert!(armed().is_empty());
    }
    #[test]
    fn diagnostics_copy_uses_known_fields_and_never_raw_errors_or_launch_secrets() {
        let (_, _, mut session) = app_and_session();
        session.launch.arguments = vec!["--api-key=argument-secret".into()];
        session
            .launch
            .environment
            .insert("TOKEN".into(), "environment-secret".into());
        session.resume_hint = Some("resume-secret".into());
        session.transcript_path = Some("/arbitrary/transcript-secret".into());
        session.launch.working_directory = PathBuf::from(format!(
            "/workspace/\u{202e}{}\u{200b}\n\u{1b}",
            "x".repeat(1000)
        ));
        session.continuity = TerminalContinuity::Failed {
            typed_error: RuntimeError {
                code: "unknown-code-secret".into(),
                message: "backend-secret\nAuthorization: Bearer token".into(),
            },
            retryable: false,
        };
        let details = Details::capture(&session, None);
        let copied = details.copy_text();
        assert_eq!(details.code, "terminal_unavailable");
        assert!(details.cwd.chars().count() <= 240);
        assert!(!details.cwd.chars().any(char::is_control));
        assert!(!details.cwd.contains('\u{202e}'));
        for secret in [
            "argument-secret",
            "environment-secret",
            "resume-secret",
            "transcript-secret",
            "unknown-code-secret",
            "backend-secret",
            "Bearer",
        ] {
            assert!(!copied.contains(secret), "copied {secret}");
        }
        assert!(
            !copied.contains("Ended at"),
            "there is no recorded launch/failure timestamp"
        );
    }
    #[test]
    fn failure_capture_preserves_typed_code_before_generic_residency_is_written() {
        let (mut app, card, _) = app_and_session();
        let state = app.card_state_mut(card).unwrap();
        state.residency = Residency::Failed {
            typed_error: RuntimeError {
                code: "start_failed".into(),
                message: "secret raw error".into(),
            },
            retryable: true,
        };
        app.fail_runtime(card, "different secret".into());
        let Some(CardRuntime::Failed(Failure::Terminal(details))) = app.cards.get(&card) else {
            panic!("typed terminal failure missing")
        };
        assert_eq!(details.code, "start_failed");
        assert!(!details.copy_text().contains("secret"));
    }
    #[test]
    fn first_hydrating_failure_uses_the_assigned_typed_code() {
        let (mut app, card, _) = app_and_session();
        app.fail_runtime(card, "first backend secret".into());
        let details = app.terminal_details(card).unwrap();
        assert_eq!(details.code, "runtime_hydration_failed");
        assert_eq!(details.message, "The terminal could not be restored.");
        assert!(!details.copy_text().contains("secret"));
        assert!(details.exit.is_none());
    }
    #[test]
    fn new_failure_on_same_identity_captures_current_typed_state() {
        let (mut app, card, session) = app_and_session();
        app.card_state_mut(card).unwrap().residency = Residency::Failed {
            typed_error: RuntimeError {
                code: "start_failed".into(),
                message: "old secret".into(),
            },
            retryable: true,
        };
        app.fail_runtime(card, "old secret".into());
        assert_eq!(app.terminal_details(card).unwrap().code, "start_failed");
        app.card_state_mut(card).unwrap().residency = Residency::Failed {
            typed_error: RuntimeError {
                code: "runtime_hydration_failed".into(),
                message: "new secret".into(),
            },
            retryable: true,
        };
        app.fail_runtime(card, "new secret".into());
        let details = app.terminal_details(card).unwrap();
        assert_eq!(details.terminal, session.id);
        assert_eq!(details.code, "runtime_hydration_failed");
        assert!(!details.copy_text().contains("secret"));
    }
    #[test]
    fn failed_relaunch_reports_new_failure_without_previous_identity_exit() {
        let (mut app, card, mut previous) = app_and_session();
        previous.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(17),
                signal: None,
                occurred_at_unix_ms: 42,
            },
        };
        let ended = Details::capture(&previous, None);
        assert_eq!(ended.code, "process_ended");
        assert!(ended.copy_text().contains("Exit status: 17"));
        assert!(ended.copy_text().contains("Unix milliseconds): 42"));

        let relaunched = fresh_terminal_relaunch(&previous);
        assert_ne!(relaunched.id, previous.id);
        let state = app.card_state_mut(card).unwrap();
        state.kind = CardKind::Terminal(relaunched.clone());
        state.residency = Residency::Failed {
            typed_error: RuntimeError {
                code: "start_failed".into(),
                message: "new failure secret".into(),
            },
            retryable: true,
        };
        app.fail_runtime(card, "backend secret".into());
        let Some(CardRuntime::Failed(Failure::Terminal(details))) = app.cards.get(&card) else {
            panic!("terminal failure missing")
        };
        assert_eq!(details.terminal, relaunched.id);
        assert_eq!(details.code, "start_failed");
        assert!(details.exit.is_none());
        let copied = details.copy_text();
        assert!(!copied.contains("Exit status"));
        assert!(!copied.contains("Ended at"));
        assert!(!copied.contains("secret"));
    }
    #[test]
    fn known_codex_callback_failure_preserves_recovery_without_raw_backend_text() {
        let (mut app, card, _) = app_and_session();
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.launch.program = "codex".into();
        session.launch.arguments = vec!["login".into()];
        app.fail_runtime(
            card,
            "Address already in use; backend-secret=private".into(),
        );
        let Some(CardRuntime::Failed(failure @ Failure::Terminal(details))) = app.cards.get(&card)
        else {
            panic!("terminal failure missing")
        };
        assert!(account::login::callback_failed(failure));
        assert_eq!(details.code, "login_callback_busy");
        assert!(!details.copy_text().contains("backend-secret"));
    }
    #[test]
    fn exit_copy_keeps_real_status_time_and_redacts_unknown_signal_text() {
        let (_, _, mut session) = app_and_session();
        session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(17),
                signal: Some("signal secret".into()),
                occurred_at_unix_ms: 42,
            },
        };
        let text = Details::capture(&session, None).copy_text();
        assert!(text.contains("Exit status: 17"));
        assert!(text.contains("Unix milliseconds): 42"));
        assert!(text.contains("Signal: Unspecified signal"));
        assert!(!text.contains("signal secret"));
        let TerminalContinuity::Exited { last_exit } = &mut session.continuity else {
            unreachable!()
        };
        last_exit.signal = Some("Terminated".into());
        assert!(
            Details::capture(&session, None)
                .copy_text()
                .contains("Signal: Terminated")
        );
    }
    #[test]
    fn retry_launching_rejects_repeat_and_failure_settles_actual_start_once() {
        let (mut app, card, _) = app_and_session();
        drop(app.retry_card(card));
        let attempt = app.terminal_failure.attempts[&card].clone();
        let CardKind::Terminal(session) = &app.card_state(card).unwrap().kind else {
            unreachable!()
        };
        let terminal = session.id;
        assert!(app.terminal_start_in_flight(card));
        assert!(app.terminal_retry_view(card).is_some());
        drop(app.retry_card(card));
        assert_eq!(
            app.terminal_failure.attempts[&card].operation,
            attempt.operation
        );
        let action = Action::RetryFinished {
            card,
            operation: attempt.operation,
            terminal,
            result: Err("secret failed start".into()),
        };
        drop(app.update_terminal_failure(action.clone()));
        assert!(!app.terminal_start_in_flight(card));
        assert!(!app.terminal_failure.attempts.contains_key(&card));
        assert!(matches!(
            app.cards.get(&card),
            Some(CardRuntime::Failed(Failure::Terminal(_)))
        ));
        app.begin_terminal_start(card);
        drop(app.update_terminal_failure(action));
        assert!(
            app.terminal_start_in_flight(card),
            "duplicate delivery cannot settle a new start"
        );
        drop(app.finish_terminal_start(card));
    }
    #[test]
    fn stale_retry_operation_with_same_identity_cannot_replace_newer_loading_surface() {
        let (mut app, card, session) = app_and_session();
        app.begin_terminal_start(card);
        app.begin_terminal_start(card);
        let stale = uuid::Uuid::new_v4();
        let current = uuid::Uuid::new_v4();
        app.terminal_failure.outstanding.insert(stale, card);
        app.terminal_failure.outstanding.insert(current, card);
        app.terminal_failure.attempts.insert(
            card,
            Attempt {
                operation: current,
                details: Details::capture(&session, None),
            },
        );
        drop(app.update_terminal_failure(Action::RetryFinished {
            card,
            operation: stale,
            terminal: session.id,
            result: Err("old failure".into()),
        }));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert_eq!(app.terminal_failure.attempts[&card].operation, current);
        assert!(app.terminal_start_in_flight(card));
        drop(app.update_terminal_failure(Action::RetryFinished {
            card,
            operation: current,
            terminal: session.id,
            result: Err("new failure".into()),
        }));
        assert!(!app.terminal_start_in_flight(card));
        assert!(matches!(
            app.cards.get(&card),
            Some(CardRuntime::Failed(Failure::Terminal(_)))
        ));
    }
    #[test]
    fn native_owner_retry_consequence_matches_open_conversation_admission() {
        let (mut app, card, _) = app_and_session();
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.launch.program = "codex".into();
        let socket = PathBuf::from("/private-daemon-root")
            .join(format!("cagents-codex-{}", session.id))
            .join("control.sock");
        session.launch.arguments = vec!["--remote".into(), format!("unix://{}", socket.display())];
        session.owned_codex_listener = Some(crate::chat::OwnedCodexListener {
            socket,
            allocator: HostSessionProof {
                host_session_id: "allocator".into(),
                boot_id: "boot".into(),
                host_incarnation: "host".into(),
            },
        });
        transition_terminal_to_relaunchable(
            session,
            crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 42,
            },
        );
        app.card_state_mut(card).unwrap().residency = Residency::Hot;
        app.cards.insert(card, CardRuntime::Completed);
        assert_eq!(
            app.terminal_details(card).unwrap().consequence,
            "Open the existing conversation in the chat view."
        );
        drop(app.retry_card(card));
        assert!(app.chat_start_pending(card));
        assert!(!app.terminal_start_in_flight(card));
    }
    #[test]
    fn old_reveal_reply_cannot_release_a_new_activation() {
        let (mut app, card, session) = app_and_session();
        let old = uuid::Uuid::new_v4();
        app.terminal_failure.probes.insert(
            card,
            Probe {
                operation: old,
                session,
                saved_context: None,
                activation: None,
                available: true,
                pending: false,
                retry_at: Instant::now(),
                failures: 0,
            },
        );
        let owner = app.terminal_diagnostic_intent(card).unwrap();
        drop(app.reveal_terminal_transcript(card));
        let current = app.terminal_failure.probes[&card].operation;
        assert_ne!(old, current);
        assert!(app.terminal_failure.probes[&card].pending);
        drop(app.update_terminal_failure(Action::Revealed {
            card,
            operation: old,
            owner,
            result: Err(transcript_location::Unavailable::Changed),
        }));
        assert!(app.terminal_failure.probes[&card].pending);
        assert!(app.terminal_failure.probes[&card].available);
    }
    #[test]
    fn transcript_probe_is_fenced_by_operation_and_exact_current_launch() {
        let (mut app, card, session) = app_and_session();
        let operation = uuid::Uuid::new_v4();
        app.terminal_failure.probes.insert(
            card,
            Probe {
                operation,
                session,
                saved_context: None,
                activation: None,
                available: false,
                pending: true,
                retry_at: Instant::now(),
                failures: 0,
            },
        );
        drop(app.update_terminal_failure(Action::Probed {
            card,
            operation: uuid::Uuid::new_v4(),
            available: true,
        }));
        assert!(!app.terminal_failure.probes[&card].available);
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.launch.arguments.push("new launch".into());
        drop(app.update_terminal_failure(Action::Probed {
            card,
            operation,
            available: true,
        }));
        assert!(!app.terminal_failure.probes[&card].available);
    }
    thread_local! {
        /// Deadlines of the probe retry timers armed on this test thread.
        pub(super) static ARMED: std::cell::RefCell<Vec<Instant>> = const {
            std::cell::RefCell::new(Vec::new())
        };
    }
    fn armed() -> Vec<Instant> {
        ARMED.with(|armed| std::mem::take(&mut *armed.borrow_mut()))
    }
    fn probing(app: &mut Cagents, card: CardId, session: &TerminalSession) -> uuid::Uuid {
        let operation = uuid::Uuid::new_v4();
        let failures = app
            .terminal_failure
            .probes
            .get(&card)
            .map_or(0, |probe| probe.failures);
        app.terminal_failure.probes.insert(
            card,
            Probe {
                operation,
                session: session.clone(),
                saved_context: None,
                activation: None,
                available: false,
                pending: true,
                retry_at: Instant::now(),
                failures,
            },
        );
        operation
    }
    /// The update tail retries an unavailable probe, but nothing else may run
    /// that tail by then, so the reply arms its own retry at the probe's
    /// retry time. A found transcript arms nothing and resets the backoff.
    #[test]
    fn an_unavailable_probe_arms_its_retry_and_an_available_one_does_not() {
        let (mut app, card, session) = app_and_session();
        armed();
        let operation = probing(&mut app, card, &session);
        let before = Instant::now();
        assert_eq!(
            app.update_terminal_failure(Action::Probed {
                card,
                operation,
                available: false,
            })
            .units(),
            1
        );
        let probe = &app.terminal_failure.probes[&card];
        assert_eq!(armed(), vec![probe.retry_at]);
        assert!(probe.retry_at >= before + PROBE_RETRY_BASE);
        assert!(!probe.pending && !probe.available);

        let operation = probing(&mut app, card, &session);
        drop(app.update_terminal_failure(Action::Probed {
            card,
            operation,
            available: true,
        }));
        assert!(armed().is_empty());
        assert_eq!(app.terminal_failure.probes[&card].failures, 0);
    }
    /// Delivering the timer reprobes a visible ended terminal whose retry time
    /// has come, carrying its failure count forward.
    #[test]
    fn a_due_retry_probes_again() {
        let (mut app, card, session) = app_and_session();
        app.cards.insert(card, CardRuntime::Completed);
        let _ = app.workspace.resize_viewport(1200.0);
        assert_eq!(
            app.workspace.visibility(card),
            Some(VisibilityClass::Visible)
        );
        let operation = probing(&mut app, card, &session);
        drop(app.update_terminal_failure(Action::Probed {
            card,
            operation,
            available: false,
        }));
        app.terminal_failure.probes.get_mut(&card).unwrap().retry_at = Instant::now();
        drop(app.update(Message::TerminalFailure(Action::ProbeRetryDue)));
        let probe = &app.terminal_failure.probes[&card];
        assert_ne!(probe.operation, operation, "a new probe started");
        assert!(probe.pending);
        assert_eq!(probe.failures, 1);
    }
    /// Retries back off and stop arming timers, so an idle app with a lost
    /// transcript goes quiet. A remote transcript is never armed for.
    #[test]
    fn retries_back_off_then_wait_for_other_updates_and_never_arm_remotely() {
        let (_, _, session) = app_and_session();
        let now = Instant::now();
        let mut probe = Probe {
            operation: uuid::Uuid::new_v4(),
            session,
            saved_context: None,
            activation: None,
            available: false,
            pending: true,
            retry_at: now,
            failures: 0,
        };
        let delays = (0..8)
            .map(|_| probe.unavailable(false, now).map(|at| at - now))
            .collect::<Vec<_>>();
        let secs = |s| Some(Duration::from_secs(s));
        assert_eq!(
            delays,
            [
                secs(10),
                secs(20),
                secs(40),
                secs(80),
                secs(160),
                None,
                None,
                None
            ]
        );
        assert_eq!(probe.retry_at - now, PROBE_RETRY_MAX);

        probe.failures = 0;
        assert_eq!(probe.unavailable(true, now), None);
        assert_eq!(probe.failures, 0);
        assert_eq!(probe.retry_at - now, PROBE_RETRY_BASE);
    }
    /// A reveal that finds the transcript gone arms the same retry.
    #[test]
    fn a_failed_reveal_arms_a_retry() {
        let (mut app, card, session) = app_and_session();
        armed();
        let operation = probing(&mut app, card, &session);
        let owner = app.terminal_diagnostic_intent(card).unwrap();
        assert_eq!(
            app.update_terminal_failure(Action::Revealed {
                card,
                operation,
                owner,
                result: Err(transcript_location::Unavailable::Changed),
            })
            .units(),
            1
        );
        let probe = &app.terminal_failure.probes[&card];
        assert_eq!(armed(), vec![probe.retry_at]);
        assert!(!probe.available && !probe.pending);
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;
    #[test]
    fn closed_retry_failure_settles_start_without_reinstalling_a_failed_card() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.durable.cards[0].id;
        drop(app.retry_card(card));
        let operation = app.terminal_failure.attempts[&card].operation;
        let CardKind::Terminal(session) = &app.card_state(card).unwrap().kind else {
            unreachable!()
        };
        let terminal = session.id;
        app.card_state_mut(card).unwrap().existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: None,
            },
        };
        app.cards.remove(&card);
        drop(app.update_terminal_failure(Action::RetryFinished {
            card,
            operation,
            terminal,
            result: Err("late raw error".into()),
        }));
        assert!(!app.terminal_start_in_flight(card));
        assert!(!app.cards.contains_key(&card));
        assert!(!app.terminal_failure.attempts.contains_key(&card));
    }
    #[test]
    fn retry_success_adopts_surface_and_retires_launching_phase() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.durable.cards[0].id;
        app.lifecycle.register(
            card,
            Exposure::Visible,
            Residency::Hydrating { epoch: 1 },
            Instant::now(),
        );
        drop(app.retry_card(card));
        let operation = app.terminal_failure.attempts[&card].operation;
        let CardKind::Terminal(session) = &app.card_state(card).unwrap().kind else {
            unreachable!()
        };
        let mut session = session.clone();
        let terminal = session.id;
        // A successfully attached terminal may already have exited. The
        // accepted surface must preserve that final screen and real exit.
        session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 42,
            },
        };
        let directory = tempfile::tempdir().unwrap();
        let handle = super::super::tests::unconnected_surface(directory.path(), terminal);
        drop(app.update_terminal_failure(Action::RetryFinished {
            card,
            operation,
            terminal,
            result: Ok(StartedTerminal {
                handle,
                session,
                spawned: false,
            }),
        }));
        assert!(!app.terminal_start_in_flight(card));
        assert!(!app.terminal_failure.attempts.contains_key(&card));
        assert!(matches!(
            app.cards.get(&card),
            Some(CardRuntime::Terminal(_))
        ));
        assert!(ended_terminal(app.card_state(card)).is_some());
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use crate::terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
        ServerResponse, TerminalDescriptor, read_frame, write_frame,
    };
    #[test]
    fn stale_success_same_terminal_never_sends_shutdown_to_the_winning_client() {
        assert_rejected_success_preserves_owner(false, false);
    }
    #[test]
    fn stale_success_after_native_handoff_never_shuts_down_the_chat_owner() {
        assert_rejected_success_preserves_owner(true, false);
    }
    #[test]
    fn current_retry_success_rejected_after_handoff_preserves_the_native_owner() {
        assert_rejected_success_preserves_owner(true, true);
    }
    fn assert_rejected_success_preserves_owner(native_owner: bool, current_operation: bool) {
        let (mut app, card, _source_directory) = super::super::tests::live_agent_app("codex");
        let CardKind::Terminal(session) = &app.card_state(card).unwrap().kind else {
            unreachable!()
        };
        let session = session.clone();
        let terminal = session.id;
        if native_owner {
            drop(app.update(Message::ChatUi(chat_ui::Action::Switch(card))));
            let ready = chat_ui::ready_message_for_pending_switch(
                &app,
                card,
                crate::chat::Snapshot::default(),
            );
            drop(app.update(ready));
            assert!(
                matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(session) if session.id == terminal)
            );
        }
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let proof = HostSessionProof {
            host_session_id: "test-host".into(),
            boot_id: "test-boot".into(),
            host_incarnation: "test-incarnation".into(),
        };
        let descriptor = TerminalDescriptor {
            screen_lost_after_crash: false,
            session: session.clone(),
            generation: 0,
            event_sequence: 0,
            oldest_event_sequence: 1,
            attached: true,
            transcript: None,
            activity: None,
            agent_activity: None,
            upgrade_sources: Vec::new(),
            upgrade_relaunches: Vec::new(),
            title: None,
        };
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            write_frame(
                &mut stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: vec![descriptor],
                        },
                    },
                },
            )
            .unwrap();
            let visibility: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(
                visibility.request,
                ClientRequest::SetVisibility { .. }
            ));
            write_frame(
                &mut stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(visibility.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Accepted,
                    },
                },
            )
            .unwrap();
            ready_tx.send(()).unwrap();
            loop {
                match read_frame::<ClientEnvelope>(&mut stream) {
                    Ok(Some(request)) => {
                        if matches!(request.request, ClientRequest::ShutdownTerminal { .. }) {
                            return true;
                        }
                        write_frame(
                            &mut stream,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id: Some(request.request_id),
                                proof: proof.clone(),
                                message: ServerMessage::Response {
                                    response: ServerResponse::Accepted,
                                },
                            },
                        )
                        .unwrap();
                    }
                    Ok(None) | Err(_) => return false,
                }
            }
        });
        let handle = TerminalSurface::from(
            HostedTerminalHandle::connect_at(socket, terminal, None, |_| {}).unwrap(),
        );
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if !native_owner {
            app.cards
                .insert(card, CardRuntime::Terminal(handle.clone()));
        }
        app.begin_terminal_start(card);
        let stale = uuid::Uuid::new_v4();
        app.terminal_failure.outstanding.insert(stale, card);
        if current_operation {
            app.terminal_failure.attempts.insert(
                card,
                Attempt {
                    operation: stale,
                    details: Details::capture(&session, None),
                },
            );
        }
        drop(app.update_terminal_failure(Action::RetryFinished {
            card,
            operation: stale,
            terminal,
            result: Ok(StartedTerminal {
                handle,
                session,
                spawned: true,
            }),
        }));
        assert!(app.terminal_runtime_claims(terminal));
        if native_owner {
            assert!(matches!(app.cards.get(&card), Some(CardRuntime::Chat(_))));
        } else {
            assert!(matches!(
                app.cards.get(&card),
                Some(CardRuntime::Terminal(_))
            ));
        }
        assert!(!app.terminal_start_in_flight(card));
        assert!(
            !server.join().unwrap(),
            "stale success shut down the winning hosted PTY"
        );
    }
}
