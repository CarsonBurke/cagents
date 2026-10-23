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
    available: bool,
    pending: bool,
    retry_at: Instant,
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
    Copy(CardId),
    Reveal(CardId),
    Probed {
        card: CardId,
        operation: uuid::Uuid,
        available: bool,
    },
    Prepared {
        card: CardId,
        operation: uuid::Uuid,
        result: Result<Permit, transcript_location::Unavailable>,
    },
    Revealed {
        card: CardId,
        operation: uuid::Uuid,
        result: Result<(), transcript_location::Unavailable>,
    },
    RetryFinished {
        card: CardId,
        operation: uuid::Uuid,
        terminal: TerminalId,
        result: Result<StartedTerminal, String>,
    },
}
impl Action {
    pub(super) fn is_user_action(&self) -> bool {
        matches!(self, Self::Copy(_) | Self::Reveal(_))
    }
}
fn fresh_transcript(
    session: &TerminalSession,
) -> Result<transcript_location::ValidatedTranscript, transcript_location::Unavailable> {
    use transcript_location::Unavailable;
    // Connecting to the existing control socket never starts or repairs a host.
    let socket =
        crate::terminal::ipc::daemon_socket_path().map_err(|_| Unavailable::MetadataMissing)?;
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
        !crate::device::is_remote() && self.terminal_failure.probes.get(&card).is_some_and(|probe| probe.operation == operation && self.card_state(card).is_some_and(|state| state.existence.is_open() && matches!(&state.kind, CardKind::Terminal(session) if session.id == probe.session.id && session.launch == probe.session.launch)))
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
            sessions.get(card).is_some_and(|session| {
                session.id == probe.session.id && session.launch == probe.session.launch
            })
        });
        let mut tasks = Vec::new();
        for (card, session) in sessions {
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
            self.terminal_failure.probes.insert(
                card,
                Probe {
                    operation,
                    session: session.clone(),
                    available: false,
                    pending: !crate::device::is_remote(),
                    retry_at: Instant::now() + Duration::from_secs(10),
                },
            );
            if crate::device::is_remote() {
                continue;
            }
            tasks.push(cosmic::task::future(async move {
                let available =
                    tokio::task::spawn_blocking(move || fresh_transcript(&session).is_ok())
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
        let probe = self.terminal_failure.probes.get(&card);
        let remote = crate::device::is_remote();
        let available = !remote && probe.is_some_and(|probe| probe.available && !probe.pending);
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
        let folder = if available {
            folder.on_press(Message::TerminalFailure(Action::Reveal(card)))
        } else {
            folder
        };
        widget::row::with_children(vec![
            crate::ui_controls::button::secondary("Copy details")
                .id(card_control_id(card, "failure.copy"))
                .on_press(Message::TerminalFailure(Action::Copy(card)))
                .into(),
            folder.into(),
        ])
        .spacing(space::S)
        .wrap()
        .into()
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
            Action::Copy(card) => self
                .terminal_details(card)
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
                    probe.available = available;
                    probe.pending = false;
                }
                Task::none()
            }
            Action::Reveal(card) => {
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
                let operation = probe.operation;
                let session = probe.session.clone();
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        fresh_transcript(&session)
                            .map(|transcript| Permit(Arc::new(Mutex::new(Some(transcript)))))
                    })
                    .await
                    .unwrap_or(Err(transcript_location::Unavailable::MetadataMissing));
                    Message::TerminalFailure(Action::Prepared {
                        card,
                        operation,
                        result,
                    })
                })
            }
            Action::Prepared {
                card,
                operation,
                result,
            } => {
                if !self.probe_current(card, operation) {
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
                                result: transcript.reveal().await,
                            })
                        })
                    }
                    Err(error) => self.update_terminal_failure(Action::Revealed {
                        card,
                        operation,
                        result: Err(error),
                    }),
                }
            }
            Action::Revealed {
                card,
                operation,
                result,
            } => {
                if self.probe_current(card, operation) {
                    if let Some(probe) = self.terminal_failure.probes.get_mut(&card) {
                        probe.pending = false;
                        if result.is_err() {
                            probe.available = false;
                        }
                    }
                    if result.is_err() {
                        self.status = Some(TransientNotice::warning(
                            "The transcript folder is unavailable",
                        ));
                    }
                }
                Task::none()
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
                available: true,
                pending: false,
                retry_at: Instant::now(),
            },
        );
        drop(app.update_terminal_failure(Action::Reveal(card)));
        let current = app.terminal_failure.probes[&card].operation;
        assert_ne!(old, current);
        assert!(app.terminal_failure.probes[&card].pending);
        drop(app.update_terminal_failure(Action::Revealed {
            card,
            operation: old,
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
                available: false,
                pending: true,
                retry_at: Instant::now(),
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
