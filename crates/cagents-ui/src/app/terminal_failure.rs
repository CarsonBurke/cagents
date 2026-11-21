//! Bounded terminal diagnostics and current, descriptor-authorized folder actions.
use super::*;
use crate::{control::ControlClient, model::TranscriptMetadata, transcript_location};
use std::borrow::Cow;
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
    consequence: Cow<'static, str>,
    /// Why the last Retry started nothing, in place of what Retry would do.
    retry_refusal: Option<String>,
}
/// The daemon answered and does not host this terminal, as after a restart.
/// Reattaching cannot work, so the failure must not promise it.
pub(super) const TERMINAL_NOT_HOSTED: &str = "the daemon no longer hosts this shared terminal";
/// The daemon that started this terminal is still running and has no record of
/// it. Recovery treats that as unsafe to replace while that daemon runs.
pub(super) const TERMINAL_RETIRED: &str = "the daemon that started this shared terminal retired it";
/// Another daemon replaced the one that started this terminal on the same boot,
/// so its process can have survived. Retry replaces it only if it has not.
pub(super) const TERMINAL_DAEMON_RESTARTED: &str =
    "the daemon that started this shared terminal restarted";
/// A proven lineage names a live terminal that carries on this work under
/// another identity. Retry attaches to it rather than starting anything.
pub(super) const TERMINAL_MOVED: &str = "this shared terminal continues under another identity";
/// A live terminal has a conflicting claim on this terminal's lineage or
/// conversation. Recovery preserves that terminal, so Retry cannot help.
pub(super) const TERMINAL_SUPERSEDED: &str =
    "another live terminal holds this shared terminal's work";
/// The daemon is still restoring the terminals it adopted after a restart, so
/// it refuses ordinary requests and its listing can't show this one is gone.
pub(super) const TERMINALS_RESTORING: &str =
    "the daemon is still restoring terminals after its restart";
/// What applies to a terminal the daemon does not list. This asks the questions
/// Retry's reconciliation asks, so what the pane says is what Retry will do.
pub(super) fn missing_terminal_message(
    session: &TerminalSession,
    daemon: &crate::model::HostSessionProof,
    terminals: &[crate::terminal::ipc::TerminalDescriptor],
) -> &'static str {
    // Without a live proof Retry only checks that no live terminal holds the
    // saved conversation, then launches again.
    let Some(saved) = session.continuity.live_proof() else {
        return if daemon_recovery::conversation_hosted_elsewhere(session, daemon, terminals) {
            TERMINAL_SUPERSEDED
        } else {
            TERMINAL_NOT_HOSTED
        };
    };
    match daemon_recovery::adoptable_lineage(session, &saved, daemon, terminals) {
        Ok(Some(_)) => TERMINAL_MOVED,
        Err(_) => TERMINAL_SUPERSEDED,
        Ok(None) if saved == *daemon => TERMINAL_RETIRED,
        Ok(None) if daemon_recovery::conversation_hosted_elsewhere(session, daemon, terminals) => {
            TERMINAL_SUPERSEDED
        }
        Ok(None) if saved.boot_id == daemon.boot_id => TERMINAL_DAEMON_RESTARTED,
        Ok(None) => TERMINAL_NOT_HOSTED,
    }
}
fn missing_terminal_code(message: &str) -> Option<&'static str> {
    match message {
        TERMINALS_RESTORING => Some("terminals_restoring"),
        TERMINAL_NOT_HOSTED => Some("terminal_not_hosted"),
        TERMINAL_RETIRED => Some("terminal_retired"),
        TERMINAL_DAEMON_RESTARTED => Some("terminal_daemon_restarted"),
        TERMINAL_MOVED => Some("terminal_moved"),
        TERMINAL_SUPERSEDED => Some("terminal_superseded"),
        _ => None,
    }
}
pub(super) fn runtime_error_code(message: &str, default: &'static str) -> &'static str {
    missing_terminal_code(message).unwrap_or(default)
}
const RECONNECT: &str = "Reconnect to the existing terminal.";
const RETRY_AFTER_RESTORING: &str = "Once restoring has finished, Retry reconnects this terminal, or starts it again if it wasn't restored.";
const RETRY_AFTER_RESTORING_RESUME: &str = "Once restoring has finished, Retry resumes the saved conversation in this pane, unless a restored terminal already holds it.";
const RETRY_AFTER_RESTORING_SHELL: &str =
    "Once restoring has finished, Retry starts a new shell in this pane.";
const RETRY_AFTER_RESTORING_FRESH: &str = "Once restoring has finished, Retry runs the saved command again from the beginning in this pane.";
const RETRY_REFUSED_RESTORING: &str =
    "Retry didn't start anything because restoring hasn't finished. Press Retry again once it has.";
const RETRY_REFUSED: &str = "Retry didn't start anything. Open a new terminal to keep working.";
// A failure isn't re-evaluated, and either obstacle can go away, so these keep
// the Retry button and say when it can work.
const RETRY_BLOCKED_RETIRED: &str =
    "Retry can't restore it while this daemon is running. Open a new terminal to keep working.";
const RETRY_BLOCKED_RESUME: &str = "That terminal keeps its work. Once it has stopped, Retry resumes the saved conversation in this pane.";
const RETRY_BLOCKED_PICKER: &str = "That terminal keeps its work. Once it has stopped, Retry opens the agent's conversation picker in this pane.";
const RETRY_BLOCKED_SHELL: &str =
    "That terminal keeps its work. Once it has stopped, Retry starts a new shell in this pane.";
const RETRY_BLOCKED_FRESH: &str = "That terminal keeps its work. Once it has stopped, Retry runs the saved command again from the beginning in this pane.";
const RETRY_REFUSED_AFTER_CHECK: &str = "Retry didn't start anything. If its process is still running, stop it and press Retry again, or open a new terminal.";
const RETRY_PENDING: &str = "Retry to reconnect or start it again.";
pub(super) const NATIVE_OWNER: &str = "Open the existing conversation in the chat view.";
const OUTPUT_GOES_WITH_RETRY: &str = "Retry replaces this terminal. Its saved output is deleted the next time Cagents starts, so copy it from the transcript folder first if you need it.";
const OUTPUT_MAY_GO_WITH_RETRY: &str = "If Retry replaces this terminal, its saved output is deleted the next time Cagents starts, so copy it from the transcript folder first if you need it.";
const RETRY_FRESH: &str = "Retry runs the saved command again from the beginning in this pane.";
const RETRY_RESUME: &str = "Retry resumes the saved conversation in this pane.";
/// Retry opens the provider's picker, as recovery does for an agent whose
/// exact conversation cagents couldn't confirm.
fn retry_picker(provider: &str) -> String {
    format!(
        "Retry opens {provider}'s conversation picker. cagents couldn't confirm which conversation this terminal was last in, so it can't resume it directly."
    )
}
fn retry_after_restoring_picker(provider: &str) -> String {
    format!(
        "Once restoring has finished, Retry opens {provider}'s conversation picker in this pane. cagents couldn't confirm which conversation this terminal was last in, so it can't resume it directly."
    )
}
const RETRY_SHELL: &str = "Retry starts a new shell in this pane.";
/// Retry would refuse to start anything, so the pane doesn't offer it. The
/// disabled Retry's tooltip and accessible description repeat this.
pub(super) const RETRY_UNAVAILABLE: &str = "cagents can't restart this agent. Its saved command may reopen a conversation the agent has since left, and cagents can't open the agent's conversation picker with the other options it was launched with. Open a new terminal to keep working.";
/// What Retry launches in place of a terminal.
enum NewTerminal {
    Resume,
    /// The provider's picker, named by its label.
    Picker(&'static str),
    Shell,
    Fresh,
    /// Retry follows a recovery plan that refuses to replay this launch, so it
    /// would start nothing.
    Refused,
}
/// What recovery launches once a terminal's daemon is gone.
fn recovered_terminal(session: &TerminalSession) -> NewTerminal {
    match crate::upgrade::recovery_plan(session) {
        Ok((_, crate::upgrade::RecoveryKind::Exact)) => NewTerminal::Resume,
        Ok((recipe, crate::upgrade::RecoveryKind::Picker)) => NewTerminal::Picker(
            crate::agent::Provider::recognize_launch(&recipe).map_or("the agent", |p| p.label()),
        ),
        Ok((_, crate::upgrade::RecoveryKind::Shell)) => NewTerminal::Shell,
        // Retry reruns the saved command, unless the conversation it selects
        // is one observation found stale.
        Err(_) if super::terminal_relaunch_refused(session) => NewTerminal::Refused,
        Err(_) => NewTerminal::Fresh,
    }
}
/// What Retry launches in place of a terminal with no live process, which
/// follows the recovery plan only when the agent's current launch was observed.
fn new_terminal(session: &TerminalSession) -> NewTerminal {
    if super::observed_agent_launch(session).is_some() {
        recovered_terminal(session)
    } else if session.agent_resume_recipe().is_some() {
        NewTerminal::Resume
    } else if matches!(recovered_terminal(session), NewTerminal::Shell) {
        NewTerminal::Shell
    } else {
        NewTerminal::Fresh
    }
}
fn new_terminal_consequence(session: &TerminalSession) -> Cow<'static, str> {
    match new_terminal(session) {
        NewTerminal::Resume => RETRY_RESUME.into(),
        NewTerminal::Picker(provider) => retry_picker(provider).into(),
        NewTerminal::Shell => RETRY_SHELL.into(),
        NewTerminal::Fresh => RETRY_FRESH.into(),
        NewTerminal::Refused => RETRY_UNAVAILABLE.into(),
    }
}
/// What Retry will do once the daemon has restored its terminals. A pane with a
/// live proof can find its terminal among them; one without starts a new one.
fn restoring_consequence(session: &TerminalSession) -> Cow<'static, str> {
    if session.continuity.live_proof().is_some() {
        return RETRY_AFTER_RESTORING.into();
    }
    match new_terminal(session) {
        NewTerminal::Resume => RETRY_AFTER_RESTORING_RESUME.into(),
        NewTerminal::Picker(provider) => retry_after_restoring_picker(provider).into(),
        NewTerminal::Shell => RETRY_AFTER_RESTORING_SHELL.into(),
        NewTerminal::Fresh => RETRY_AFTER_RESTORING_FRESH.into(),
        NewTerminal::Refused => RETRY_UNAVAILABLE.into(),
    }
}
/// What Retry does for a terminal whose daemon is gone: the plan recovery will
/// actually follow, which is not always the saved command.
fn replacement_consequence(session: &TerminalSession) -> Cow<'static, str> {
    match recovered_terminal(session) {
        NewTerminal::Resume => RETRY_RESUME.into(),
        NewTerminal::Picker(provider) => retry_picker(provider).into(),
        NewTerminal::Shell => RETRY_SHELL.into(),
        NewTerminal::Fresh => RETRY_FRESH.into(),
        NewTerminal::Refused => RETRY_UNAVAILABLE.into(),
    }
}
/// The same plan, once Retry has established that the process is gone.
fn after_check_consequence(session: &TerminalSession) -> &'static str {
    match recovered_terminal(session) {
        NewTerminal::Resume => {
            "Retry checks that it has stopped, then resumes the saved conversation in this pane."
        }
        NewTerminal::Picker(_) => {
            "Retry checks that it has stopped, then opens the agent's conversation picker in this pane."
        }
        NewTerminal::Shell => {
            "Retry checks that it has stopped, then starts a new shell in this pane."
        }
        NewTerminal::Fresh => {
            "Retry checks that it has stopped, then runs the saved command again from the beginning in this pane."
        }
        NewTerminal::Refused => RETRY_UNAVAILABLE,
    }
}
/// What Retry will do once the terminal holding this work has stopped.
fn blocked_consequence(session: &TerminalSession) -> &'static str {
    match recovered_terminal(session) {
        NewTerminal::Resume => RETRY_BLOCKED_RESUME,
        NewTerminal::Picker(_) => RETRY_BLOCKED_PICKER,
        NewTerminal::Shell => RETRY_BLOCKED_SHELL,
        NewTerminal::Fresh => RETRY_BLOCKED_FRESH,
        NewTerminal::Refused => RETRY_UNAVAILABLE,
    }
}
/// What a pane says once Retry was refused for this missing terminal, when
/// recovery gave no sentence of its own. The state it explained still applies.
fn refused_text(code: &str, consequence: &str) -> String {
    match code {
        "terminal_retired" | "terminal_superseded" => {
            format!("Retry didn't start anything. {consequence}")
        }
        "terminal_daemon_restarted" => RETRY_REFUSED_AFTER_CHECK.to_owned(),
        "terminals_restoring" => RETRY_REFUSED_RESTORING.to_owned(),
        _ => RETRY_REFUSED.to_owned(),
    }
}
/// The warning a pane in this state carries, before any press.
#[cfg(test)]
pub(super) fn missing_terminal_warning(
    message: &str,
    session: &TerminalSession,
) -> Option<&'static str> {
    let code = missing_terminal_code(message)?;
    Details {
        code,
        message: known_failure(code).1,
        terminal: session.id,
        cwd: String::new(),
        worktree: None,
        exit: None,
        consequence: missing_terminal_consequence(code, session),
        retry_refusal: None,
    }
    .output_warning()
}
fn terminal_notice(pane: Option<&str>, text: &str) -> String {
    match pane {
        Some(pane) => format!("{pane}: {text}"),
        None => text.to_owned(),
    }
}
/// Whether the pane's own sentence explains this failure, so a notice about it
/// can say the same rather than relay the daemon's wording.
fn pane_explains(code: &str) -> bool {
    is_missing_terminal(code) || code == "terminals_restoring"
}
fn is_missing_terminal(code: &str) -> bool {
    matches!(
        code,
        "terminal_not_hosted"
            | "terminal_retired"
            | "terminal_daemon_restarted"
            | "terminal_moved"
            | "terminal_superseded"
    )
}
fn missing_terminal_consequence(code: &str, session: &TerminalSession) -> Cow<'static, str> {
    match code {
        "terminals_restoring" => restoring_consequence(session),
        "terminal_moved" => RECONNECT.into(),
        "terminal_retired" => RETRY_BLOCKED_RETIRED.into(),
        "terminal_superseded" => blocked_consequence(session).into(),
        "terminal_daemon_restarted" => after_check_consequence(session).into(),
        _ if session.continuity.live_proof().is_none() => new_terminal_consequence(session),
        _ => replacement_consequence(session),
    }
}
fn known_failure(code: &str) -> (&'static str, &'static str) {
    match code {
        "terminal_not_hosted" => ("terminal_not_hosted", "This terminal is no longer running."),
        "terminal_retired" => ("terminal_retired", "This terminal is no longer running."),
        "terminal_daemon_restarted" => (
            "terminal_daemon_restarted",
            "This terminal's daemon restarted. Its process may still be running.",
        ),
        "terminal_moved" => ("terminal_moved", "This terminal needs to reconnect."),
        "terminals_restoring" => (
            "terminals_restoring",
            "Terminals are still being restored after Cagents restarted.",
        ),
        "terminal_superseded" => (
            "terminal_superseded",
            "Another running terminal already holds this terminal's work.",
        ),
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
    /// Whether what the pane said rules out a launch from this press, or only
    /// explained the last attempt.
    fn expects_no_launch(&self) -> bool {
        self.retry_refusal.is_some()
            || matches!(
                self.consequence.as_ref(),
                RETRY_REFUSED
                    | RETRY_UNAVAILABLE
                    | RETRY_BLOCKED_RETIRED
                    | RETRY_BLOCKED_RESUME
                    | RETRY_BLOCKED_PICKER
                    | RETRY_BLOCKED_SHELL
                    | RETRY_BLOCKED_FRESH
            )
    }
    fn attempt_title(&self) -> &'static str {
        if self.expects_no_launch() || self.code == "terminals_restoring" {
            "Checking terminal…"
        } else {
            "Launching terminal…"
        }
    }
    fn attempt_caption(&self) -> &str {
        if self.code == "terminals_restoring" {
            // What it will do once restoring has finished isn't known yet.
            "Checking whether restoring has finished."
        } else if self.expects_no_launch() {
            "Trying again."
        } else {
            &self.consequence
        }
    }
    /// A press that replaces the terminal leaves only its next launch's prune
    /// between that and the output it saved. See #2142. A refusal or a blocked
    /// state doesn't remove the risk, because a later press can still replace it.
    fn output_warning(&self) -> Option<&'static str> {
        if self.consequence == NATIVE_OWNER {
            return None;
        }
        match self.code {
            "terminal_not_hosted" if !self.expects_no_launch() => Some(OUTPUT_GOES_WITH_RETRY),
            "terminal_not_hosted"
            | "terminal_daemon_restarted"
            | "terminal_retired"
            | "terminal_superseded"
            | "terminals_restoring" => Some(OUTPUT_MAY_GO_WITH_RETRY),
            _ => None,
        }
    }
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
        let consequence = if pane_explains(code) {
            missing_terminal_consequence(code, session)
        } else if session.continuity.live_proof().is_some() {
            RECONNECT.into()
        } else {
            new_terminal_consequence(session)
        };
        Self {
            code,
            message,
            terminal: session.id,
            cwd: bounded_path(&session.launch.working_directory),
            worktree: None,
            exit,
            consequence,
            retry_refusal: None,
        }
    }
    fn copy_text(&self) -> String {
        let mut text = format!(
            "Code: {}\nMessage: {}\nTerminal: {}\nDirectory: {}\nNext: {}",
            self.code,
            self.message,
            self.terminal,
            self.cwd,
            self.retry_refusal.as_deref().unwrap_or(&self.consequence)
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

impl State {
    /// The terminals' transcript probes, for the projection check.
    #[cfg(any(test, feature = "development"))]
    pub(super) fn projection_view(&self) -> String {
        super::projection_oracle::sorted_entries(&self.probes)
    }
}
#[derive(Clone)]
pub struct Permit(Arc<Mutex<Option<transcript_location::ValidatedTranscript>>>);
impl std::fmt::Debug for Permit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ValidatedTranscriptPermit")
    }
}
#[derive(Debug, Clone)]
pub enum Action {
    Copy(CardId),
    /// A new shell beside a pane that can't restart.
    NewTerminal(CardId),
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
        result: Result<Box<StartedTerminal>, String>,
    },
    /// An unavailable probe's retry time has come. Handling it runs the
    /// update's transcript probe stage, which probes again.
    ProbeRetryDue,
}
impl Action {
    /// Whether handling the action changes nothing but a transcript probe,
    /// so the update projects only the probe stage
    /// (`Projection::TranscriptProbes`).
    pub(super) fn only_touches_probes(&self) -> bool {
        matches!(self, Self::Probed { .. } | Self::ProbeRetryDue)
    }

    pub(super) fn is_user_action(&self) -> bool {
        matches!(self, Self::Copy(_) | Self::Reveal(_) | Self::NewTerminal(_))
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
/// The receipt to open a terminal's transcript with. `None` means the daemon
/// answered and lists no such identity, whether it restarted or the work moved
/// to a new one, so the only transcript left is that identity's own file. A
/// listed identity keeps its receipt requirement.
fn listed_transcript(
    session: &TerminalSession,
    descriptors: Vec<crate::terminal::ipc::TerminalDescriptor>,
) -> Result<Option<TranscriptMetadata>, transcript_location::Unavailable> {
    if !descriptors
        .iter()
        .any(|descriptor| descriptor.session.id == session.id)
    {
        return Ok(None);
    }
    descriptors
        .into_iter()
        .find(|descriptor| {
            descriptor.session.id == session.id && descriptor.session.launch == session.launch
        })
        .and_then(|descriptor| descriptor.transcript)
        .map(Some)
        .ok_or(transcript_location::Unavailable::MetadataMissing)
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
    // With no daemon answering, nothing can vouch for another location, and a
    // file named for this identity is what its terminal left behind.
    let descriptors = ControlClient::connect(&socket).and_then(|mut client| client.list());
    let Ok(descriptors) = descriptors else {
        return transcript_location::open_without_host(session.id);
    };
    match listed_transcript(session, descriptors)? {
        Some(metadata) => transcript_location::open(session.id, &metadata),
        None => transcript_location::open_without_host(session.id),
    }
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
            Details::capture_failure(session, Some(self.residency(card)))
        } else {
            Details::capture(session, Some(self.residency(card)))
        };
        if self.pending_terminal_reservation(card).is_some() {
            // Retry completes the accepted start, reusing its worker when it
            // exists. A pending recipe is not a fresh relaunch instruction.
            details.consequence = RETRY_PENDING.into();
        } else if self.completed_terminal_has_native_owner(card) {
            details.consequence = NATIVE_OWNER.into();
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
        // Captured before the residency records this failure, so the capture
        // above cannot have seen the code and promises a reconnect.
        if let Some(code) = missing_terminal_code(&generic) {
            (details.code, details.message) = known_failure(code);
            if let Some(CardKind::Terminal(session)) =
                self.card_state(card).map(|state| &state.kind)
                && !self.completed_terminal_has_native_owner(card)
            {
                details.consequence = missing_terminal_consequence(code, session);
            }
        }
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
                    && (self.saved_terminal_current(card).is_none() || probe.saved_context.is_some())
                    && probe.saved_context == self.saved_terminal_transcript_context(card)
                    && self.card_state(card).is_some_and(|state| {
                        state.existence.is_open()
                            && matches!(&state.kind, CardKind::Terminal(session)
                                if if probe.saved_context.is_some() {
                                    session == &probe.session
                                } else {
                                    session.id == probe.session.id && session.launch == probe.session.launch
                                })
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
                ) || self.ended_terminal(state.id).is_some()
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
            sessions.get(card).is_some_and(|session| {
                session.id == probe.session.id && session.launch == probe.session.launch
            })
        });
        let mut tasks = Vec::new();
        for (card, session) in sessions {
            let saved_context = self.saved_terminal_transcript_context(card);
            if self
                .terminal_failure
                .probes
                .get(&card)
                .is_some_and(|probe| {
                    probe.saved_context != saved_context
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
        let warning = details.output_warning().filter(|_| {
            self.terminal_failure
                .probes
                .get(&card)
                .is_some_and(|probe| probe.available && !probe.pending)
        });
        let next = details
            .retry_refusal
            .clone()
            .unwrap_or_else(|| details.consequence.to_string());
        let mut rows: Vec<Element<'_, Message>> = vec![design::body(next).into()];
        if let Some(warning) = warning {
            rows.push(design::body(warning).into());
        }
        rows.push(design::caption(format!("Code: {}", details.code)).into());
        rows.push(design::caption(format!("Terminal: {}", details.terminal)).into());
        rows.push(design::caption(format!("Directory: {}", details.cwd)).into());
        if let Some(worktree) = details.worktree {
            rows.push(design::caption(format!("Worktree: {worktree}")).into());
        }
        widget::column::with_children(rows)
            .spacing(space::XS)
            .into()
    }
    pub(super) fn terminal_diagnostic_controls(&self, card: CardId) -> Element<'static, Message> {
        widget::row::with_children(vec![
            crate::ui_controls::button::secondary("Copy details")
                .id(card_control_id(card, "failure.copy"))
                .on_press(Message::TerminalFailure(Action::Copy(card)))
                .into(),
            self.terminal_transcript_control(
                card,
                Some(Message::TerminalFailure(Action::Reveal(card))),
            ),
        ])
        .spacing(space::S)
        .wrap()
        .into()
    }
    pub(super) fn terminal_transcript_control(
        &self,
        card: CardId,
        action: Option<Message>,
    ) -> Element<'static, Message> {
        let probe = self.terminal_failure.probes.get(&card);
        let remote = crate::device::is_remote();
        let available = !remote && probe.is_some_and(|probe| probe.available && !probe.pending);
        let folder = crate::ui_controls::button::secondary("Open transcript folder")
            .instead(
                "Checking transcript…",
                !remote && probe.is_some_and(|probe| probe.pending),
            )
            .instead("Transcript unavailable", !available)
            .id(card_control_id(card, "failure.folder"));
        let folder = if let Some(action) = action.filter(|_| available) {
            folder.on_press(action)
        } else {
            folder
        };
        folder.into()
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
    /// A notice about a failure its pane explains reads as the pane does, not
    /// as the reason recovery recorded, and names the pane, because the notice
    /// log is shared by every pane.
    pub(super) fn missing_terminal_notice(&self, card: CardId) -> Option<String> {
        match self.cards.get(&card) {
            Some(CardRuntime::Failed(Failure::Terminal(details)))
                if pane_explains(details.code) =>
            {
                Some(terminal_notice(
                    self.notice_pane_name(card).as_deref(),
                    details.message,
                ))
            }
            _ => None,
        }
    }
    /// Retry can't help until something else changes, so it isn't the main action.
    pub(super) fn terminal_retry_is_blocked(&self, card: CardId) -> bool {
        matches!(
            self.cards.get(&card),
            Some(CardRuntime::Failed(Failure::Terminal(details)))
                if matches!(details.code, "terminal_retired" | "terminal_superseded")
        )
    }
    /// Retry would start nothing whatever else changes, so the pane disables it.
    pub(super) fn terminal_retry_unavailable(&self, card: CardId) -> bool {
        matches!(
            self.cards.get(&card),
            Some(CardRuntime::Failed(Failure::Terminal(details)))
                if details.consequence == RETRY_UNAVAILABLE
        )
    }
    pub(super) fn terminal_retry_pending(&self, card: CardId) -> bool {
        self.terminal_failure.attempts.contains_key(&card)
    }
    pub(super) fn terminal_retry_view(&self, card: CardId) -> Option<Element<'_, Message>> {
        let attempt = self.terminal_failure.attempts.get(&card)?;
        Some(
            widget::container(
                widget::column::with_children(vec![
                    crate::ui_controls::activity_spinner(24.0, design::accent_color),
                    design::title(attempt.details.attempt_title()).into(),
                    design::caption(attempt.details.attempt_caption()).into(),
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
                result: result.map(Box::new),
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
            Action::NewTerminal(card) => {
                // The pane's own directory, except on a remote device, where
                // a new terminal opens where any other would.
                let directory = (!crate::device::is_remote())
                    .then(|| self.card_state(card))
                    .flatten()
                    .and_then(super::worktree_ui::card_directory)
                    .map(Path::to_path_buf)
                    .or_else(|| self.terminal_directory());
                directory.map_or_else(Task::none, |directory| self.add_terminal(directory))
            }
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
            Action::Reveal(card) => {
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
                let operation = probe.operation;
                let session = probe.session.clone();
                let saved_context = probe.saved_context.clone();
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
                let mut retry = Task::none();
                if self.probe_current(card, operation) {
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
                let prior = current_operation
                    .then(|| self.terminal_failure.attempts.remove(&card))
                    .flatten()
                    .map(|attempt| attempt.details);
                // Existing identity/close checks retire stale spawned handles
                // and decrement the actual start count even after presentation
                // was replaced or its card was closed.
                let refusal = result.as_ref().err().cloned();
                let task = self.terminal_started_for_operation(
                    card,
                    terminal,
                    result.map(|started| *started),
                    current_operation,
                );
                let (Some(prior), Some(refusal)) = (prior, refusal) else {
                    return task;
                };
                let pane = self.notice_pane_name(card);
                let Some(CardRuntime::Failed(Failure::Terminal(details))) =
                    self.cards.get_mut(&card)
                else {
                    return task;
                };
                // Recovery's own sentence is written for the reader and can name
                // the process in the way. Any other error stays out of the pane.
                let readable =
                    super::daemon_recovery::is_retry_refusal(&refusal).then(|| refusal.clone());
                // When the pane explains what this press found, such as a
                // daemon still restoring, that is what it now says, whatever it
                // said before. Otherwise a missing terminal it explained before
                // still applies. An earlier explanation of restoring never
                // outlives the press: by then the daemon answered something else.
                let explained = pane_explains(details.code);
                if explained || is_missing_terminal(prior.code) {
                    if !explained {
                        // The refusal doesn't change what the pane knew about
                        // this terminal, so it keeps saying so.
                        details.code = prior.code;
                        details.message = prior.message;
                        details.consequence = prior.consequence.clone();
                    }
                    let (code, message) = (details.code, details.message);
                    let consequence = details.consequence.clone();
                    details.retry_refusal =
                        Some(readable.unwrap_or_else(|| refused_text(code, &consequence)));
                    self.status = Some(
                        TransientNotice::error(terminal_notice(
                            pane.as_deref(),
                            &format!("Retry didn't start anything. {message}"),
                        ))
                        .with_details(refusal)
                        .with_key(NoticeKey::TerminalStart(card)),
                    );
                } else {
                    if details.consequence == RECONNECT {
                        // Having just failed to reattach, it must not suggest doing so.
                        details.consequence = RETRY_REFUSED.into();
                    }
                    details.retry_refusal = readable;
                }
                task
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
    fn pending_start_failure_does_not_promise_a_fresh_terminal() {
        let (mut app, card, _) = app_and_session();
        let mut pane =
            crate::workspace_model::views::Pane::from_card(app.card_state(card).unwrap(), None)
                .unwrap();
        pane.pending_terminal_start = true;
        let mut view = crate::workspace_model::views::View::empty(crate::model::ChannelId::new());
        view.panes.push(pane);
        app.workspace_model.views = vec![view];
        app.fail_runtime(card, "completion save refused".into());
        let details = app.terminal_details(card).unwrap();
        assert_eq!(details.consequence, "Retry to reconnect or start it again.");
        assert!(!details.copy_text().contains("new terminal"));
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
        app.set_residency(
            card,
            Residency::Failed {
                typed_error: RuntimeError {
                    code: "start_failed".into(),
                    message: "secret raw error".into(),
                },
                retryable: true,
            },
        );
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
    fn a_terminal_the_daemon_lost_does_not_promise_reconnecting() {
        let (mut app, card, _) = app_and_session();
        // The saved proof from before the restart is what made this misleading.
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("old-host"),
            boot_id: crate::model::BootId::new("old-boot"),
            host_incarnation: crate::model::HostIncarnation::new("old"),
        };
        app.fail_runtime(card, TERMINAL_NOT_HOSTED.into());
        let details = app.terminal_details(card).unwrap();
        assert_eq!(details.code, "terminal_not_hosted");
        assert_eq!(details.message, "This terminal is no longer running.");
        assert_eq!(details.consequence, RETRY_FRESH);
    }
    #[test]
    fn a_lost_terminal_names_what_retry_will_actually_do() {
        let (_, _, session) = app_and_session();
        let live = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("old-host"),
            boot_id: crate::model::BootId::new("old-boot"),
            host_incarnation: crate::model::HostIncarnation::new("old"),
        };
        let lost = |launch: LaunchRecipe| TerminalSession {
            continuity: live.clone(),
            launch,
            ..session.clone()
        };
        let cwd = session.launch.working_directory.clone();
        let agent = |arguments: &[&str]| {
            let mut launch = LaunchRecipe::shell(cwd.clone()).unwrap();
            launch.program = "claude".into();
            launch.arguments = arguments.iter().map(|&argument| argument.into()).collect();
            launch
        };
        for (launch, expected) in [
            (
                agent(&["--resume", "12345678-1234-4234-8234-123456789abc"]),
                RETRY_RESUME,
            ),
            (agent(&[]), retry_picker("Claude Code").as_str()),
            (
                LaunchRecipe::shell(cwd.clone()).unwrap(),
                "Retry starts a new shell in this pane.",
            ),
            (
                LaunchRecipe {
                    program: "fixture-build".into(),
                    ..LaunchRecipe::shell(cwd.clone()).unwrap()
                },
                RETRY_FRESH,
            ),
        ] {
            assert_eq!(replacement_consequence(&lost(launch)), expected);
        }
    }
    #[test]
    fn a_terminal_its_own_daemon_retired_does_not_promise_a_replacement() {
        let (mut app, card, _) = app_and_session();
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("host"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("same"),
        };
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.continuity = TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        let mut other = proof.clone();
        other.host_incarnation = crate::model::HostIncarnation::new("later");
        assert_eq!(
            missing_terminal_message(session, &other, &[]),
            TERMINAL_DAEMON_RESTARTED
        );
        other.boot_id = crate::model::BootId::new("later-boot");
        assert_eq!(
            missing_terminal_message(session, &other, &[]),
            TERMINAL_NOT_HOSTED
        );
        let message = missing_terminal_message(session, &proof, &[]);
        assert_eq!(message, TERMINAL_RETIRED);
        app.fail_runtime(card, message.into());
        let details = app.terminal_details(card).unwrap();
        assert_eq!(details.code, "terminal_retired");
        assert_eq!(details.message, "This terminal is no longer running.");
        assert_eq!(details.consequence, RETRY_BLOCKED_RETIRED);
    }
    #[test]
    fn a_terminal_with_no_live_proof_promises_the_launch_it_will_get() {
        let (mut app, card, session) = app_and_session();
        let mut launch = session.launch.clone();
        launch.program = "claude".into();
        launch.arguments.clear();
        // The recovery plan would open a picker, but with no live proof Retry
        // takes the fresh launch and never reaches that plan.
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: launch.clone(),
            last_exit: None,
        };
        session.launch = launch;
        app.fail_runtime(card, TERMINAL_NOT_HOSTED.into());
        let details = app.terminal_details(card).unwrap();
        assert_eq!(details.code, "terminal_not_hosted");
        assert_eq!(details.consequence, RETRY_FRESH);
        // That launch is a fresh relaunch under a new ID, so the saved
        // output is pruned at the next start as for any replacement.
        assert_eq!(details.output_warning(), Some(OUTPUT_GOES_WITH_RETRY));
    }
    #[test]
    fn each_missing_terminal_says_what_applies_to_it() {
        let (_, _, restarted) = app_and_session();
        for (message, code, consequence) in [
            (TERMINAL_MOVED, "terminal_moved", RECONNECT),
            (
                TERMINAL_DAEMON_RESTARTED,
                "terminal_daemon_restarted",
                after_check_consequence(&restarted),
            ),
            (
                TERMINAL_SUPERSEDED,
                "terminal_superseded",
                blocked_consequence(&restarted),
            ),
        ] {
            let (mut app, card, _) = app_and_session();
            app.fail_runtime(card, message.into());
            let details = app.terminal_details(card).unwrap();
            assert_eq!(details.code, code);
            assert_eq!(details.consequence, consequence);
            // The shared notice log names the pane, then says what it says.
            let pane = app.notice_pane_name(card).unwrap();
            assert_eq!(
                app.missing_terminal_notice(card),
                Some(format!("{pane}: {}", details.message))
            );
        }
    }
    #[test]
    fn an_unlisted_identity_opens_its_own_transcript_and_a_listed_one_needs_its_receipt() {
        let (_, _, session) = app_and_session();
        let listed = |launch: LaunchRecipe| {
            let mut descriptor =
                crate::terminal::ipc::TerminalDescriptor::relaunchable(session.id, launch, None);
            descriptor.session.id = session.id;
            descriptor
        };
        assert!(matches!(listed_transcript(&session, Vec::new()), Ok(None)));
        let other = crate::terminal::ipc::TerminalDescriptor::relaunchable(
            TerminalId::new(),
            session.launch.clone(),
            None,
        );
        assert!(matches!(listed_transcript(&session, vec![other]), Ok(None)));
        // Listed with no receipt, or under another command: never by path alone.
        let mut changed = session.launch.clone();
        changed.program = "fixture-other".into();
        for descriptor in [listed(session.launch.clone()), listed(changed)] {
            assert!(matches!(
                listed_transcript(&session, vec![descriptor]),
                Err(transcript_location::Unavailable::MetadataMissing)
            ));
        }
    }
    #[test]
    fn a_repeated_retry_does_not_show_the_last_refusal_as_its_progress() {
        let (_, _, session) = app_and_session();
        let mut details = Details::capture(&session, None);
        details.consequence = RETRY_REFUSED.into();
        assert_eq!(details.attempt_caption(), "Trying again.");
        details.consequence = RETRY_RESUME.into();
        assert_eq!(details.attempt_caption(), RETRY_RESUME);
    }
    #[test]
    fn a_refused_retry_keeps_what_the_pane_knew_and_says_this_press_did_nothing() {
        for (message, code) in [
            (TERMINAL_NOT_HOSTED, "terminal_not_hosted"),
            (TERMINAL_DAEMON_RESTARTED, "terminal_daemon_restarted"),
            (TERMINAL_RETIRED, "terminal_retired"),
            (TERMINAL_SUPERSEDED, "terminal_superseded"),
        ] {
            let (mut app, card, _) = app_and_session();
            let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
                unreachable!()
            };
            session.continuity = TerminalContinuity::Live {
                host_session_id: crate::model::HostSessionId::new("old-host"),
                boot_id: crate::model::BootId::new("old-boot"),
                host_incarnation: crate::model::HostIncarnation::new("old"),
            };
            let terminal = session.id;
            app.fail_runtime(card, message.into());
            let before = app.terminal_details(card).unwrap();
            assert_eq!(before.code, code);
            drop(app.retry_card(card));
            let attempt = &app.terminal_failure.attempts[&card];
            let operation = attempt.operation;
            assert_eq!(
                attempt.details.attempt_title() == "Checking terminal…",
                matches!(code, "terminal_retired" | "terminal_superseded"),
                "{code} shows no launch it can rule out"
            );
            drop(app.update_terminal_failure(Action::RetryFinished {
                card,
                operation,
                terminal,
                result: Err("terminal fixture still has a live process (42); preserving it".into()),
            }));
            let after = app.terminal_details(card).unwrap();
            assert_eq!(after.code, code);
            assert_eq!(after.message, before.message);
            // Every missing terminal says the same thing about this press.
            assert_eq!(after.consequence, before.consequence);
            let said = after.retry_refusal.as_deref().unwrap();
            assert!(said.starts_with("Retry didn't start anything."), "{said}");
            assert!(after.copy_text().contains(said));
            let notice = app.status.as_ref().unwrap();
            let pane = app.notice_pane_name(card).unwrap();
            assert_eq!(
                notice.message,
                format!("{pane}: Retry didn't start anything. {}", before.message)
            );
            assert_eq!(
                notice.details.as_deref(),
                Some("terminal fixture still has a live process (42); preserving it"),
            );
            assert_eq!(notice.key, Some(NoticeKey::TerminalStart(card)));
            // Pressing again doesn't present the refusal as progress.
            assert_eq!(after.attempt_caption(), "Trying again.");
            assert_eq!(after.attempt_title(), "Checking terminal…");
            // A later press can still replace it, so the warning stays.
            assert_eq!(after.output_warning(), Some(OUTPUT_MAY_GO_WITH_RETRY));
        }
    }
    #[test]
    fn a_retry_that_starts_after_a_refusal_clears_the_refusal_notice() {
        let (mut app, card, _) = app_and_session();
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        // A live proof keeps the identity across presses, as after a restart.
        session.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("old-host"),
            boot_id: crate::model::BootId::new("old-boot"),
            host_incarnation: crate::model::HostIncarnation::new("old"),
        };
        let terminal = session.id;
        app.fail_runtime(card, TERMINAL_NOT_HOSTED.into());
        drop(app.retry_card(card));
        let operation = app.terminal_failure.attempts[&card].operation;
        drop(app.update_terminal_failure(Action::RetryFinished {
            card,
            operation,
            terminal,
            result: Err("terminal fixture still has a live process (42); preserving it".into()),
        }));
        let refused = app.status.clone().unwrap();
        assert_eq!(refused.key, Some(NoticeKey::TerminalStart(card)));
        app.notices.record(refused);

        drop(app.retry_card(card));
        let operation = app.terminal_failure.attempts[&card].operation;
        let session = match &app.card_state(card).unwrap().kind {
            CardKind::Terminal(session) => session.clone(),
            _ => unreachable!(),
        };
        let directory = tempfile::tempdir().unwrap();
        let handle = super::super::tests::unconnected_surface(directory.path(), terminal);
        drop(app.update_terminal_failure(Action::RetryFinished {
            card,
            operation,
            terminal,
            result: Ok(Box::new(StartedTerminal {
                handle,
                session,
                spawned: true,
                save_failure: None,
            })),
        }));
        assert!(matches!(
            app.cards.get(&card),
            Some(CardRuntime::Terminal(_))
        ));
        let key = NoticeKey::TerminalStart(card);
        assert!(
            !app.status
                .as_ref()
                .is_some_and(|notice| notice.key.as_ref() == Some(&key))
        );
        assert!(!app.notices.has(key));
    }
    #[test]
    fn a_pane_refused_while_its_daemon_restores_says_so_and_when_retry_helps() {
        let (mut app, card, session) = app_and_session();
        app.fail_runtime(card, TERMINALS_RESTORING.into());
        let details = app.terminal_details(card).unwrap();
        assert_eq!(details.code, "terminals_restoring");
        assert_eq!(
            details.message,
            "Terminals are still being restored after Cagents restarted."
        );
        // Without a live proof Retry starts a new terminal; it has nothing
        // to reconnect. This one runs a saved command.
        assert_eq!(details.consequence, RETRY_AFTER_RESTORING_FRESH);
        // A press once restoring has finished can replace it.
        assert_eq!(details.output_warning(), Some(OUTPUT_MAY_GO_WITH_RETRY));
        // Until then a press can't say what it will start.
        assert_eq!(details.attempt_title(), "Checking terminal…");
        assert_eq!(
            details.attempt_caption(),
            "Checking whether restoring has finished."
        );
        let pane = app.notice_pane_name(card).unwrap();
        assert_eq!(
            app.missing_terminal_notice(card).as_deref(),
            Some(format!("{pane}: {}", details.message).as_str())
        );
        let mut live = session.clone();
        live.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("old-host"),
            boot_id: crate::model::BootId::new("old-boot"),
            host_incarnation: crate::model::HostIncarnation::new("old"),
        };
        assert_eq!(restoring_consequence(&live), RETRY_AFTER_RESTORING);
        let mut agent = session;
        agent.launch.program = "/usr/bin/codex".into();
        agent.launch.arguments = vec![
            "resume".into(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        ];
        assert_eq!(restoring_consequence(&agent), RETRY_AFTER_RESTORING_RESUME);
    }
    #[test]
    fn a_retry_refused_because_terminals_are_still_restoring_says_so() {
        let (mut app, card, _) = app_and_session();
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("old-host"),
            boot_id: crate::model::BootId::new("old-boot"),
            host_incarnation: crate::model::HostIncarnation::new("old"),
        };
        let terminal = session.id;
        // Whatever the pane said before, and pressed again while restoring.
        for prior in [
            TERMINAL_NOT_HOSTED,
            TERMINALS_RESTORING,
            TERMINALS_RESTORING,
        ] {
            if prior == TERMINAL_NOT_HOSTED {
                app.fail_runtime(card, prior.into());
            }
            drop(app.retry_card(card));
            let operation = app.terminal_failure.attempts[&card].operation;
            drop(app.update_terminal_failure(Action::RetryFinished {
                card,
                operation,
                terminal,
                result: Err(TERMINALS_RESTORING.into()),
            }));
            let details = app.terminal_details(card).unwrap();
            assert_eq!(details.code, "terminals_restoring", "after {prior}");
            assert_eq!(details.consequence, RETRY_AFTER_RESTORING);
            assert_eq!(
                details.retry_refusal.as_deref(),
                Some(RETRY_REFUSED_RESTORING)
            );
            assert!(!details.copy_text().contains("new terminal"));
            let pane = app.notice_pane_name(card).unwrap();
            assert_eq!(
                app.status.as_ref().unwrap().message,
                format!("{pane}: Retry didn't start anything. {}", details.message)
            );
        }
    }
    /// Once restoring has finished, a press can be refused for another reason.
    /// The pane then says that, not that terminals are still being restored.
    #[test]
    fn a_refusal_after_restoring_finished_replaces_the_restoring_explanation() {
        let sentence = "sleep (PID 42) is still running, so nothing was started.";
        for (refusal, readable) in [
            (sentence, Some(sentence)),
            ("daemon ownership changed during recovery", None),
        ] {
            let (mut app, card, _) = app_and_session();
            let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
                unreachable!()
            };
            session.continuity = TerminalContinuity::Live {
                host_session_id: crate::model::HostSessionId::new("old-host"),
                boot_id: crate::model::BootId::new("old-boot"),
                host_incarnation: crate::model::HostIncarnation::new("old"),
            };
            let terminal = session.id;
            app.fail_runtime(card, TERMINALS_RESTORING.into());
            drop(app.retry_card(card));
            let operation = app.terminal_failure.attempts[&card].operation;
            drop(app.update_terminal_failure(Action::RetryFinished {
                card,
                operation,
                terminal,
                result: Err(refusal.into()),
            }));
            let details = app.terminal_details(card).unwrap();
            assert_ne!(details.code, "terminals_restoring", "after {refusal}");
            assert!(
                !details.copy_text().contains("still being restored")
                    && !details.copy_text().contains("restoring"),
                "after {refusal}: {}",
                details.copy_text()
            );
            assert_eq!(details.retry_refusal.as_deref(), readable);
        }
    }
    #[test]
    fn a_refusal_recovery_wrote_for_the_reader_is_what_the_pane_shows() {
        let (mut app, card, _) = app_and_session();
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("old-host"),
            boot_id: crate::model::BootId::new("old-boot"),
            host_incarnation: crate::model::HostIncarnation::new("old"),
        };
        let terminal = session.id;
        app.fail_runtime(card, TERMINAL_DAEMON_RESTARTED.into());
        drop(app.retry_card(card));
        let operation = app.terminal_failure.attempts[&card].operation;
        let sentence = "sleep (PID 42) is still running, so nothing was started.";
        drop(app.update_terminal_failure(Action::RetryFinished {
            card,
            operation,
            terminal,
            result: Err(sentence.into()),
        }));
        let after = app.terminal_details(card).unwrap();
        assert_eq!(after.code, "terminal_daemon_restarted");
        assert_eq!(after.retry_refusal.as_deref(), Some(sentence));
    }
    #[test]
    fn the_output_warning_follows_what_a_press_could_do() {
        let (_, _, session) = app_and_session();
        for (message, warning) in [
            (TERMINAL_NOT_HOSTED, Some(OUTPUT_GOES_WITH_RETRY)),
            (TERMINAL_DAEMON_RESTARTED, Some(OUTPUT_MAY_GO_WITH_RETRY)),
            (TERMINAL_RETIRED, Some(OUTPUT_MAY_GO_WITH_RETRY)),
            (TERMINAL_SUPERSEDED, Some(OUTPUT_MAY_GO_WITH_RETRY)),
            (TERMINAL_MOVED, None),
        ] {
            assert_eq!(
                missing_terminal_warning(message, &session),
                warning,
                "{message}"
            );
        }
        // Retry only switches to the chat view here, so nothing is replaced.
        let (mut app, card, _) = app_and_session();
        app.fail_runtime(card, TERMINAL_NOT_HOSTED.into());
        let mut details = app.terminal_details(card).unwrap();
        details.consequence = NATIVE_OWNER.into();
        assert_eq!(details.output_warning(), None);
    }
    #[test]
    fn a_pane_that_retry_cant_help_does_not_make_it_the_main_action() {
        for (message, blocked) in [
            (TERMINAL_RETIRED, true),
            (TERMINAL_SUPERSEDED, true),
            (TERMINAL_NOT_HOSTED, false),
            (TERMINAL_DAEMON_RESTARTED, false),
        ] {
            let (mut app, card, _) = app_and_session();
            app.fail_runtime(card, message.into());
            assert_eq!(app.terminal_retry_is_blocked(card), blocked, "{message}");
        }
    }
    #[test]
    fn a_conversation_a_live_terminal_holds_reads_as_held_without_a_proof() {
        let (_, _, mut session) = app_and_session();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        session.launch.program = "claude".into();
        session.launch.arguments = vec!["--resume".into(), id.into()];
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: session.launch.clone(),
            last_exit: None,
        };
        let daemon = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("host"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("current"),
        };
        assert_eq!(
            missing_terminal_message(&session, &daemon, &[]),
            TERMINAL_NOT_HOSTED
        );
        let mut live = session.clone();
        live.id = crate::model::TerminalId::new();
        live.continuity = TerminalContinuity::Live {
            host_session_id: daemon.host_session_id.clone(),
            boot_id: daemon.boot_id.clone(),
            host_incarnation: daemon.host_incarnation.clone(),
        };
        let mut descriptor = crate::terminal::ipc::TerminalDescriptor::relaunchable(
            live.id,
            live.launch.clone(),
            None,
        );
        descriptor.session = live;
        assert_eq!(
            missing_terminal_message(&session, &daemon, &[descriptor]),
            TERMINAL_SUPERSEDED
        );
    }
    #[test]
    fn with_no_daemon_answering_a_terminal_still_reaches_its_own_saved_output() {
        let (_, _, session) = app_and_session();
        let directory = tempfile::tempdir().unwrap();
        let unanswered = directory.path().join("no-daemon.sock");
        assert!(!matches!(
            fresh_transcript(&session, Some(&unanswered)),
            Err(transcript_location::Unavailable::MetadataMissing)
        ));
    }
    #[test]
    fn what_a_retry_will_do_follows_the_recovery_plan_after_its_check() {
        let (_, _, mut session) = app_and_session();
        session.launch = LaunchRecipe::shell(session.launch.working_directory.clone()).unwrap();
        assert!(after_check_consequence(&session).contains("starts a new shell"));
        assert_eq!(new_terminal_consequence(&session), RETRY_SHELL);
    }
    #[test]
    fn retry_names_the_picker_it_opens_for_a_conversation_it_cannot_confirm() {
        let (mut app, card, _) = app_and_session();
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.launch.program = "claude".into();
        session.launch.arguments = vec![
            "--resume".into(),
            "a1b2c3d4-1111-4111-8111-123456789abc".into(),
        ];
        let exact = crate::model::agent_recovery::resume_recipe(&session.launch).unwrap();
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: session.launch.clone(),
            last_exit: None,
        };
        // Recognition-only evidence: the agent is still Claude, but which
        // conversation it was last in is withheld.
        session.resume_hint =
            Some(serde_json::json!({"version": 2, "recipe": session.launch}).to_string());
        let picker = crate::model::agent_recovery::picker_recipe(&session.launch).unwrap();
        assert!(matches!(
            super::super::terminal_start_mode(session),
            Ok(super::super::TerminalStartMode::Spawn(recipe)) if recipe == picker
        ));
        app.fail_runtime(card, TERMINAL_NOT_HOSTED.into());
        assert_eq!(
            app.terminal_details(card).unwrap().consequence,
            retry_picker("Claude Code")
        );

        // With the exact conversation saved, Retry resumes it as before.
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.resume_hint = None;
        assert!(matches!(
            super::super::terminal_start_mode(session),
            Ok(super::super::TerminalStartMode::Spawn(recipe)) if recipe == exact
        ));
        app.fail_runtime(card, TERMINAL_NOT_HOSTED.into());
        assert_eq!(
            app.terminal_details(card).unwrap().consequence,
            RETRY_RESUME
        );
    }
    /// The (enabled, description) of the failure pane's button whose name
    /// starts with `name`, as assistive technology reads it.
    fn rendered_button(app: &Cagents, card: CardId, name: &str) -> Option<(bool, Option<String>)> {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
        let renderer = crate::app::tests::headless_renderer();
        let mut element = app.card_view(card, true, false);
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(900.0, 900.0)),
        );
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let retries: Vec<_> = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter(|entry| {
                entry.node().role() == iced_accessibility::accesskit::Role::Button
                    && entry
                        .node()
                        .label()
                        .is_some_and(|label| label.starts_with(name))
            })
            .map(|entry| {
                (
                    !entry.node().is_disabled(),
                    entry.node().description().map(str::to_owned),
                )
            })
            .collect();
        assert!(retries.len() <= 1, "one {name} button");
        retries.into_iter().next()
    }
    #[test]
    fn retry_is_disabled_when_recovery_refuses_to_replay_the_observed_launch() {
        let (mut app, card, _) = app_and_session();
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.launch.program = "claude".into();
        session.launch.arguments = vec![
            "--resume".into(),
            "a1b2c3d4-1111-4111-8111-123456789abc".into(),
            "--unknown-option".into(),
        ];
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: session.launch.clone(),
            last_exit: None,
        };
        // Recognised as Claude without its current conversation, and the
        // saved launch selects one beside an option the picker won't replay.
        session.resume_hint =
            Some(serde_json::json!({"version": 2, "recipe": session.launch}).to_string());
        assert!(super::super::terminal_start_mode(session).is_err());
        assert!(super::super::fresh_terminal_relaunch(session).is_err());
        let saved = session.clone();
        app.fail_runtime(card, TERMINAL_NOT_HOSTED.into());
        let details = app.terminal_details(card).unwrap();
        assert_eq!(details.consequence, RETRY_UNAVAILABLE);
        assert!(details.expects_no_launch());
        assert!(app.terminal_retry_unavailable(card));
        assert_eq!(
            rendered_button(&app, card, "Retry"),
            Some((false, Some(RETRY_UNAVAILABLE.to_owned())))
        );
        assert_eq!(
            rendered_button(&app, card, "Open new terminal"),
            Some((true, None))
        );

        // A Retry message that arrives anyway starts nothing and leaves the
        // pane as it was.
        drop(app.retry_card(card));
        assert!(!app.terminal_retry_pending(card));
        assert!(!app.terminal_start_in_flight(card));
        assert!(matches!(
            &app.card_state(card).unwrap().kind,
            CardKind::Terminal(session) if *session == saved
        ));
        assert_eq!(
            app.terminal_details(card).unwrap().consequence,
            RETRY_UNAVAILABLE
        );

        // A launch that selects no conversation has nothing stale to reopen,
        // so Retry reruns it from the beginning, and says so.
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.launch.arguments = vec!["--unknown-option".into()];
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: session.launch.clone(),
            last_exit: None,
        };
        session.resume_hint =
            Some(serde_json::json!({"version": 2, "recipe": session.launch}).to_string());
        assert!(matches!(
            super::super::terminal_start_mode(session),
            Ok(super::super::TerminalStartMode::Spawn(recipe)) if recipe == session.launch
        ));
        app.fail_runtime(card, TERMINAL_NOT_HOSTED.into());
        assert_eq!(app.terminal_details(card).unwrap().consequence, RETRY_FRESH);
        assert!(!app.terminal_retry_unavailable(card));
        assert_eq!(rendered_button(&app, card, "Retry"), Some((true, None)));
        assert_eq!(rendered_button(&app, card, "Open new terminal"), None);

        // Nor without an observed launch, as before.
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.resume_hint = None;
        app.fail_runtime(card, TERMINAL_NOT_HOSTED.into());
        assert_eq!(app.terminal_details(card).unwrap().consequence, RETRY_FRESH);
        assert!(!app.terminal_retry_unavailable(card));
        assert_eq!(rendered_button(&app, card, "Retry"), Some((true, None)));
        assert_eq!(rendered_button(&app, card, "Open new terminal"), None);
    }
    #[test]
    fn open_new_terminal_starts_a_shell_in_the_failed_panes_directory() {
        let (mut app, card, session) = app_and_session();
        let before: Vec<CardId> = app.durable.cards.iter().map(|state| state.id).collect();
        drop(app.update_terminal_failure(Action::NewTerminal(card)));
        let added: Vec<_> = app
            .durable
            .cards
            .iter()
            .filter(|state| !before.contains(&state.id))
            .collect();
        let [added] = added.as_slice() else {
            panic!("exactly one new card, got {}", added.len())
        };
        let CardKind::Terminal(shell) = &added.kind else {
            unreachable!()
        };
        assert_eq!(
            shell.launch.working_directory,
            session.launch.working_directory
        );
        assert!(Action::NewTerminal(card).is_user_action());
    }
    #[test]
    fn a_refused_retry_does_not_suggest_reconnecting() {
        let (mut app, card, _) = app_and_session();
        let live = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("old-host"),
            boot_id: crate::model::BootId::new("old-boot"),
            host_incarnation: crate::model::HostIncarnation::new("old"),
        };
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.continuity = live;
        let terminal = session.id;
        drop(app.retry_card(card));
        let operation = app.terminal_failure.attempts[&card].operation;
        drop(app.update_terminal_failure(Action::RetryFinished {
            card,
            operation,
            terminal,
            result: Err("cannot serialize terminal recovery".into()),
        }));
        assert_eq!(
            app.terminal_details(card).unwrap().consequence,
            RETRY_REFUSED
        );
    }
    #[test]
    fn new_failure_on_same_identity_captures_current_typed_state() {
        let (mut app, card, session) = app_and_session();
        app.set_residency(
            card,
            Residency::Failed {
                typed_error: RuntimeError {
                    code: "start_failed".into(),
                    message: "old secret".into(),
                },
                retryable: true,
            },
        );
        app.fail_runtime(card, "old secret".into());
        assert_eq!(app.terminal_details(card).unwrap().code, "start_failed");
        app.set_residency(
            card,
            Residency::Failed {
                typed_error: RuntimeError {
                    code: "runtime_hydration_failed".into(),
                    message: "new secret".into(),
                },
                retryable: true,
            },
        );
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

        let relaunched = fresh_terminal_relaunch(&previous).unwrap();
        assert_ne!(relaunched.id, previous.id);
        app.set_residency(
            card,
            Residency::Failed {
                typed_error: RuntimeError {
                    code: "start_failed".into(),
                    message: "new failure secret".into(),
                },
                retryable: true,
            },
        );
        let state = app.card_state_mut(card).unwrap();
        state.kind = CardKind::Terminal(relaunched.clone());
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
    /// Retry that refuses has to say why where it was pressed, but a raw start
    /// error stays out of the pane.
    #[test]
    fn a_refused_retry_says_why_on_the_pane_and_a_raw_error_does_not() {
        for (error, shown) in [
            ("Process 42 is running, so nothing was started.", true),
            ("secret failed start", false),
        ] {
            let (mut app, card, _) = app_and_session();
            drop(app.retry_card(card));
            let attempt = app.terminal_failure.attempts[&card].clone();
            let CardKind::Terminal(session) = &app.card_state(card).unwrap().kind else {
                unreachable!()
            };
            let terminal = session.id;
            drop(app.update_terminal_failure(Action::RetryFinished {
                card,
                operation: attempt.operation,
                terminal,
                result: Err(error.into()),
            }));
            let copied = app.terminal_details(card).unwrap().copy_text();
            assert_eq!(copied.contains(error), shown, "{copied}");
            assert_eq!(
                app.terminal_details(card).unwrap().retry_refusal.is_some(),
                shown
            );
        }
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
        app.set_residency(card, Residency::Hot);
        app.cards.insert(card, CardRuntime::Completed);
        assert_eq!(
            app.terminal_details(card).unwrap().consequence,
            "Open the existing conversation in the chat view."
        );
        let Failure::Terminal(failed) =
            app.terminal_failure_payload(card, TERMINAL_NOT_HOSTED.to_string())
        else {
            unreachable!()
        };
        assert_eq!(
            failed.consequence, "Open the existing conversation in the chat view.",
            "Retry switches to the chat, whatever the daemon no longer lists"
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
                available: true,
                pending: false,
                retry_at: Instant::now(),
                failures: 0,
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
                saved_context: None,
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
    /// has come, carrying its failure count forward. A probe's answer and its
    /// retry project only the probe stage; the projection oracle checks that
    /// against the full projection, unsettled, so the retry's own stage must
    /// start the probe.
    #[test]
    fn a_due_retry_probes_again() {
        use super::super::terminal_output_tests::projections;
        let (mut app, card, session) = app_and_session();
        app.cards.insert(card, CardRuntime::Completed);
        let _ = app.workspace.resize_viewport(1200.0);
        assert_eq!(
            app.workspace.visibility(card),
            Some(VisibilityClass::Visible)
        );
        let operation = probing(&mut app, card, &session);
        // Project the arrangement, then judge each update as it stands.
        drop(app.project_workspace(false));
        super::super::projection_oracle::disable_settling();
        let projected = projections();
        drop(app.update(Message::TerminalFailure(Action::Probed {
            card,
            operation,
            available: false,
        })));
        assert!(!app.terminal_failure.probes[&card].pending);
        // The retry time is due by the next update, as when its timer fires.
        app.terminal_failure.probes.get_mut(&card).unwrap().retry_at = Instant::now();
        drop(app.update(Message::TerminalFailure(Action::ProbeRetryDue)));
        let probe = &app.terminal_failure.probes[&card];
        assert_ne!(probe.operation, operation, "a new probe started");
        assert!(probe.pending);
        assert_eq!(probe.failures, 1);
        assert_eq!(projections(), projected, "neither projected everything");
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
        assert_eq!(
            app.update_terminal_failure(Action::Revealed {
                card,
                operation,
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
            result: Ok(Box::new(StartedTerminal {
                handle,
                session,
                spawned: false,
                save_failure: None,
            })),
        }));
        assert!(!app.terminal_start_in_flight(card));
        assert!(!app.terminal_failure.attempts.contains_key(&card));
        assert!(matches!(
            app.cards.get(&card),
            Some(CardRuntime::Terminal(_))
        ));
        assert!(app.ended_terminal(card).is_some());
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
            result: Ok(Box::new(StartedTerminal {
                handle,
                session,
                spawned: true,
                save_failure: None,
            })),
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
