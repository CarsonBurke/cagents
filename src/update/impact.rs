//! The update confirmation describes one candidate and one observed inventory.
//! A count alone cannot authorize losing a different conversation or process.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::{model::TerminalId, terminal_ipc::ShutdownCapability};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    WhenIdle,
    Interrupt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Terminal,
    NativeChat,
}

/// A conversation has messages accepted for it but not yet delivered. They
/// cannot be carried through an update, so it waits under either policy.
pub const QUEUED_MESSAGES: &str = "Messages sent to this conversation are still waiting to be delivered. The update waits until it has received them, since queued messages can't be carried through an update.";
/// A source refuses to carry a conversation whose provider stopped without
/// confirming its work settled, and asks for it to be closed. Every source
/// line since the candidate line (128) reports it in exactly these words, so
/// the updater can recognize the one blocker that closing resolves.
pub const STOPPED_WITH_UNSETTLED_WORK: &str = "Its provider stopped but may still hold unfinished work. Close this conversation to release it.";
/// A conversation whose provider already stopped and left nothing to carry.
/// The source does not checkpoint it: its window restarts the saved
/// conversation, as after any daemon restart. The updater recognizes these
/// exact words so it does not require the checkpoint to name it. Every source
/// line reports it in exactly these words, and a settled recovery record
/// re-derives its required owners from them with the current build, so they
/// must never change.
pub const PROVIDER_ENDED: &str =
    "Its provider already stopped. The saved conversation will reopen with a new provider process.";

const BACKGROUND_WORK: &str = "Its turn finished, but background agents are still running. Review stopping them to continue the update.";
const RETURNING_TO_TERMINAL: &str =
    "Finish the pending return to terminal or close this conversation.";
const PROVIDER_STARTING: &str = "Its provider is not ready yet. This clears on its own once it finishes starting; close the conversation if it does not.";
const UNRECORDED_CONVERSATION: &str = "Its provider has not reported which conversation it is running, so it cannot be resumed after the update. Send it a message or close it.";
const TERMINAL_CANNOT_CARRY: &str = "This terminal cannot preserve its live process through this update; finish it or close it before updating.";
const PAIRED_TERMINAL_CANNOT_CARRY: &str =
    "The paired console cannot preserve its live process through this update.";

/// The reason for an update disposition. The existing wire and on-disk review
/// records store text, so their fixed messages are decoded at this boundary.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum SessionCause {
    PendingDelivery,
    ProviderEnded,
    UnsettledProvider,
    BackgroundWork,
    ReturningToTerminal,
    ProviderStarting,
    UnrecordedConversation,
    TerminalCannotCarry,
    PairedTerminalCannotCarry,
    Detail(String),
}

impl SessionCause {
    pub fn as_str(&self) -> &str {
        match self {
            Self::PendingDelivery => QUEUED_MESSAGES,
            Self::ProviderEnded => PROVIDER_ENDED,
            Self::UnsettledProvider => STOPPED_WITH_UNSETTLED_WORK,
            Self::BackgroundWork => BACKGROUND_WORK,
            Self::ReturningToTerminal => RETURNING_TO_TERMINAL,
            Self::ProviderStarting => PROVIDER_STARTING,
            Self::UnrecordedConversation => UNRECORDED_CONVERSATION,
            Self::TerminalCannotCarry => TERMINAL_CANNOT_CARRY,
            Self::PairedTerminalCannotCarry => PAIRED_TERMINAL_CANNOT_CARRY,
            Self::Detail(text) => text,
        }
    }
}

impl From<String> for SessionCause {
    fn from(text: String) -> Self {
        match text.as_str() {
            QUEUED_MESSAGES => Self::PendingDelivery,
            PROVIDER_ENDED => Self::ProviderEnded,
            STOPPED_WITH_UNSETTLED_WORK => Self::UnsettledProvider,
            BACKGROUND_WORK => Self::BackgroundWork,
            RETURNING_TO_TERMINAL => Self::ReturningToTerminal,
            PROVIDER_STARTING => Self::ProviderStarting,
            UNRECORDED_CONVERSATION => Self::UnrecordedConversation,
            TERMINAL_CANNOT_CARRY => Self::TerminalCannotCarry,
            PAIRED_TERMINAL_CANNOT_CARRY => Self::PairedTerminalCannotCarry,
            _ => Self::Detail(text),
        }
    }
}

impl From<&str> for SessionCause {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

impl Serialize for SessionCause {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl std::fmt::Display for SessionCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Continues,
    Resumes,
    WaitsForIdle,
    Interrupted,
    ResumePicker,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionImpact {
    pub id: TerminalId,
    pub kind: SessionKind,
    pub title: String,
    /// Hash of the exact source descriptor and its process ownership evidence.
    /// Recipes and credentials never belong in an impact preview.
    pub identity: String,
    pub disposition: Disposition,
    pub reason: SessionCause,
    /// Exact background tasks this interruption would stop. An idle wait or
    /// live process carry never authorizes terminating these tasks.
    pub interrupted_subagents: Vec<String>,
}

/// Safe presentation shared by the native dialog, copied details and terminal
/// review. Transport diagnostics remain in the source impact, not this model.
#[derive(Debug, PartialEq, Eq)]
pub struct SessionReview {
    pub id: TerminalId,
    pub kind: SessionKind,
    pub title: String,
    pub disposition: Disposition,
    pub reason: String,
}

impl SessionImpact {
    /// Whether the source's checkpoint must carry this owner. Only a
    /// conversation whose provider already ended is reviewed without one.
    pub fn checkpointed(&self) -> bool {
        !(self.kind == SessionKind::NativeChat
            && self.disposition == Disposition::Resumes
            && self.reason == SessionCause::ProviderEnded)
    }

    pub fn review(&self) -> SessionReview {
        let reason = match self.disposition {
            Disposition::Continues => match self.kind {
                SessionKind::Terminal => "The terminal and its running processes stay alive.".into(),
                SessionKind::NativeChat => "The conversation and its running work stay alive.".into(),
            },
            Disposition::Resumes => "Restarts on its recorded conversation.".into(),
            Disposition::WaitsForIdle => {
                let tasks = self.interrupted_subagents.len();
                if self.reason == SessionCause::PendingDelivery {
                    "Messages sent to it are still waiting to be delivered; the update waits for them.".into()
                } else if tasks == 0 {
                    "Finishing its turn; the update waits.".into()
                } else {
                    let noun = if tasks == 1 { "task" } else { "tasks" };
                    format!("Finishing its turn; interrupting now also stops {tasks} background {noun}; background work will not resume automatically.")
                }
            },
            Disposition::Interrupted => {
                let tasks = self.interrupted_subagents.len();
                if tasks == 0 {
                    "Its current turn is interrupted.".into()
                } else {
                    let noun = if tasks == 1 { "task" } else { "tasks" };
                    format!("Its current turn and {tasks} background {noun} stop; background work will not resume automatically.")
                }
            }
            Disposition::ResumePicker => "Its conversation cannot reopen automatically.".into(),
            Disposition::Blocked => match &self.reason {
                SessionCause::BackgroundWork => BACKGROUND_WORK.into(),
                SessionCause::UnsettledProvider => "Its provider stopped with unfinished work. Close this conversation before updating.".into(),
                SessionCause::ReturningToTerminal => "Finish returning to the terminal or close this conversation before updating.".into(),
                SessionCause::ProviderStarting => "Its provider is still starting. Wait for it to finish, then try the update again.".into(),
                SessionCause::UnrecordedConversation => "Its conversation has not been recorded yet. Send it a message or close it before updating.".into(),
                SessionCause::TerminalCannotCarry => "Its running processes cannot be preserved through this update. Finish their work or close this terminal before updating.".into(),
                SessionCause::PairedTerminalCannotCarry => "Its terminal cannot keep running through this update. Finish its work or close it before updating.".into(),
                _ => match self.kind {
                    SessionKind::Terminal => "Its terminal could not be prepared for this update. Let its work finish, then try again; close the terminal if the problem continues.".into(),
                    SessionKind::NativeChat => "Its conversation could not be prepared for this update. Let its work finish, then try again; close the conversation if the problem continues.".into(),
                },
            },
        };
        SessionReview {
            id: self.id,
            kind: self.kind,
            title: self.title.clone(),
            disposition: self.disposition,
            reason,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonImpact {
    pub capability: ShutdownCapability,
    pub policy: Policy,
    pub sessions: Vec<SessionImpact>,
    pub blockers: Vec<String>,
}

impl Disposition {
    /// The name of this outcome beside a session in an update preview.
    pub fn label(self) -> &'static str {
        match self {
            Self::Continues => "Continues",
            Self::Resumes => "Resumes",
            Self::WaitsForIdle => "Waits for idle",
            Self::Interrupted => "Interrupted",
            Self::ResumePicker => "Choose a conversation",
            Self::Blocked => "Needs attention",
        }
    }
}

impl DaemonImpact {
    /// Compatibility protocols carry free-form diagnostics. Only known fixed
    /// messages have specific recovery instructions; never echo unknown text.
    pub fn review_notices(&self) -> Vec<String> {
        let mut notices = Vec::new();
        for blocker in &self.blockers {
            let notice = match blocker.as_str() {
                "Wait for native provider cleanup to finish." => {
                    "A conversation is still closing. Wait for it to finish before updating."
                }
                "Wait for starting sessions to finish opening." => {
                    "Wait for starting sessions to finish opening."
                }
                "Finish saving the pending return to terminal." => {
                    "Finish saving the pending return to terminal."
                }
                "Additional agent launches must settle before updating" => {
                    "Agents are still starting. Wait for them to finish opening before updating."
                }
                "Additional automation prechecks must settle before updating" => {
                    "Scheduled work is still being checked. Wait for it to finish before updating."
                }
                _ => {
                    "The update could not confirm that running work can be preserved. Let pending work finish, check that conversations and terminals respond, then try the update again."
                }
            };
            if !notices.iter().any(|existing| existing == notice) {
                notices.push(notice.to_owned());
            }
        }
        notices
    }

    /// Whether something has to be resolved by hand before the update can
    /// install. A conversation the update waits on is not that.
    pub fn blocked(&self) -> bool {
        !self.blockers.is_empty()
            || self
                .sessions
                .iter()
                .any(|session| session.disposition == Disposition::Blocked)
    }

    pub fn ready(&self) -> bool {
        self.blockers.is_empty()
            && self.sessions.iter().all(|session| {
                !matches!(
                    session.disposition,
                    Disposition::Blocked | Disposition::WaitsForIdle
                )
            })
    }

    /// A renewed short-lived capability may retain consent only for the same
    /// daemon and exact session owners. Waiting authorizes the observed busy
    /// conversations to become idle; it never authorizes an extra interruption.
    pub fn preserves_consent(&self, observed: &Self) -> bool {
        fn keyed(
            sessions: &[SessionImpact],
        ) -> BTreeMap<(TerminalId, SessionKind), &SessionImpact> {
            sessions
                .iter()
                .map(|session| ((session.id, session.kind), session))
                .collect()
        }
        let previous = keyed(&self.sessions);
        let current = keyed(&observed.sessions);
        self.policy == observed.policy
            && self.capability.proof == observed.capability.proof
            && self.blockers == observed.blockers
            && self.sessions.len() == observed.sessions.len()
            && previous.len() == self.sessions.len()
            && current.len() == observed.sessions.len()
            && previous.iter().all(|(key, previous)| {
                current.get(key).is_some_and(|current| {
                    let permitted: BTreeSet<_> = previous
                        .interrupted_subagents
                        .iter()
                        .map(String::as_str)
                        .collect();
                    previous.identity == current.identity
                        && current
                            .interrupted_subagents
                            .iter()
                            .all(|id| permitted.contains(id.as_str()))
                        && (previous.disposition == current.disposition
                            // Waiting authorizes the wait's own end: resuming
                            // or, for a provider that outlives the update,
                            // continuing. Never an interruption.
                            || (previous.disposition == Disposition::WaitsForIdle
                                && matches!(
                                    current.disposition,
                                    Disposition::Resumes | Disposition::Continues
                                ))
                            || (self.policy == Policy::Interrupt
                                && previous.disposition == Disposition::Interrupted
                                && current.disposition == Disposition::Resumes))
                })
            })
    }
}

/// Why a reviewed update was not installed: what its preview said had to be
/// resolved by hand when the review was closed. The job keeps it, so whoever
/// reads the outcome later sees those reasons rather than a cancellation.
///
/// Titles and reasons are the review's own safe presentation, never a
/// source's free-form diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub sessions: Vec<RefusedSession>,
    /// Reasons that belong to no one session, such as agent launches that
    /// must settle first.
    pub blockers: Vec<String>,
    /// Nobody closed the review; it expired while these still held it back.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub expired: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedSession {
    pub title: String,
    pub reason: String,
}

/// Separates an update outcome's summary from its details, as every reader
/// of the job's message expects.
const DETAILS_LABEL: &str = " Details: ";

impl Refusal {
    /// "1 session needs attention", or "2 sessions need attention and other
    /// work must finish first".
    pub fn summary(&self) -> String {
        let sessions = match self.sessions.len() {
            0 => None,
            1 => Some("1 session needs attention".to_owned()),
            count => Some(format!("{count} sessions need attention")),
        };
        let work = (!self.blockers.is_empty()).then_some("other work must finish first");
        let held = match (sessions, work) {
            (Some(sessions), Some(work)) => format!("{sessions} and {work}"),
            (Some(sessions), None) => sessions,
            (None, work) => work.unwrap_or_default().to_owned(),
        };
        if self.expired {
            format!("the review expired while {held}")
        } else {
            held
        }
    }

    /// Each reason on its own line of a notice's details.
    pub fn details(&self) -> Vec<String> {
        self.sessions
            .iter()
            .map(|session| format!("{}: {}", session.title, session.reason))
            .chain(self.blockers.iter().cloned())
            .collect()
    }

    /// What the CLI prints: the summary, then each reason on its own line.
    pub fn report(&self) -> String {
        let mut report = format!("Update not installed: {}.", self.summary());
        for detail in self.details() {
            report.push_str("\n  ");
            report.push_str(&detail);
        }
        report
    }

    /// The job's message: the summary, then every reason after the details
    /// label, so a reader that predates `refusal` still shows them.
    pub fn message(&self) -> String {
        let details = self
            .details()
            .iter()
            .map(|detail| detail.trim_end_matches('.').to_owned())
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "Update not installed: {}.{DETAILS_LABEL}{details}.",
            self.summary()
        )
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId, HostSessionProof};

    #[test]
    fn session_causes_read_and_write_the_legacy_string_in_reviews_and_on_the_wire() {
        for (text, cause) in [
            (QUEUED_MESSAGES, SessionCause::PendingDelivery),
            (PROVIDER_ENDED, SessionCause::ProviderEnded),
            (STOPPED_WITH_UNSETTLED_WORK, SessionCause::UnsettledProvider),
            (
                "an old diagnostic",
                SessionCause::Detail("an old diagnostic".into()),
            ),
        ] {
            let legacy_wire = postcard::to_allocvec(text).unwrap();
            assert_eq!(
                postcard::from_bytes::<SessionCause>(&legacy_wire).unwrap(),
                cause
            );
            assert_eq!(postcard::to_allocvec(&cause).unwrap(), legacy_wire);

            let legacy_json = serde_json::to_string(text).unwrap();
            assert_eq!(
                serde_json::from_str::<SessionCause>(&legacy_json).unwrap(),
                cause
            );
            assert_eq!(serde_json::to_string(&cause).unwrap(), legacy_json);
        }
    }

    pub(in crate::update) fn waiting() -> DaemonImpact {
        DaemonImpact {
            capability: ShutdownCapability {
                proof: HostSessionProof {
                    host_session_id: HostSessionId::new("host"),
                    boot_id: BootId::new("boot"),
                    host_incarnation: HostIncarnation::new("daemon"),
                },
                token: "one".into(),
                inventory_generation: 1,
                live_terminals: 1,
                background_subagents: 0,
                background_identity: String::new(),
                expires_at_unix_ms: 100,
            },
            policy: Policy::WhenIdle,
            sessions: vec![SessionImpact {
                id: TerminalId::new(),
                kind: SessionKind::NativeChat,
                title: "Claude".into(),
                identity: "exact-owned-conversation".into(),
                disposition: Disposition::WaitsForIdle,
                reason: "Finishing its turn".into(),
                interrupted_subagents: Vec::new(),
            }],
            blockers: vec![],
        }
    }

    #[test]
    fn waiting_consent_survives_idle_and_capability_renewal_but_not_changed_owners() {
        let confirmed = waiting();
        let mut current = confirmed.clone();
        current.capability.token = "renewed".into();
        current.capability.expires_at_unix_ms = 200;
        current.sessions[0].disposition = Disposition::Resumes;
        assert!(confirmed.preserves_consent(&current));
        assert!(current.ready());
        current.sessions[0].identity = "another-conversation".into();
        assert!(!confirmed.preserves_consent(&current));
        current = confirmed.clone();
        current.sessions[0].disposition = Disposition::Interrupted;
        assert!(!confirmed.preserves_consent(&current));
        current = confirmed.clone();
        current.capability.proof.host_incarnation = HostIncarnation::new("replacement");
        assert!(!confirmed.preserves_consent(&current));
    }

    #[test]
    fn a_reviewed_wait_may_end_by_continuing_under_either_policy_but_never_by_interrupting() {
        for policy in [Policy::WhenIdle, Policy::Interrupt] {
            let mut confirmed = waiting();
            confirmed.policy = policy;
            for settled in [Disposition::Resumes, Disposition::Continues] {
                let mut current = confirmed.clone();
                current.sessions[0].disposition = settled;
                assert!(
                    confirmed.preserves_consent(&current),
                    "{policy:?} {settled:?}"
                );
                assert!(
                    !current.preserves_consent(&confirmed),
                    "new queued work needs another review"
                );
            }
            let mut current = confirmed.clone();
            current.sessions[0].disposition = Disposition::Interrupted;
            assert!(!confirmed.preserves_consent(&current));
        }
    }

    #[test]
    fn interruption_consent_cannot_expand_to_new_background_tasks() {
        let mut confirmed = waiting();
        confirmed.policy = Policy::Interrupt;
        confirmed.sessions[0].disposition = Disposition::Interrupted;
        confirmed.sessions[0].interrupted_subagents = vec!["first".into(), "second".into()];
        let mut current = confirmed.clone();
        current.sessions[0].interrupted_subagents.remove(0);
        assert!(
            confirmed.preserves_consent(&current),
            "a completed task reduces impact"
        );
        current.sessions[0]
            .interrupted_subagents
            .push("replacement".into());
        assert!(
            !confirmed.preserves_consent(&current),
            "same count is not the same work"
        );
        current = confirmed.clone();
        current.sessions[0]
            .interrupted_subagents
            .push("third".into());
        assert!(!confirmed.preserves_consent(&current));
    }

    #[test]
    fn interruption_consent_survives_settlement_only_for_the_same_conversation() {
        let mut confirmed = waiting();
        confirmed.policy = Policy::Interrupt;
        confirmed.sessions[0].disposition = Disposition::Interrupted;
        let mut settled = confirmed.clone();
        settled.sessions[0].disposition = Disposition::Resumes;
        assert!(confirmed.preserves_consent(&settled));
        assert!(
            !settled.preserves_consent(&confirmed),
            "a new turn needs consent"
        );
        settled.sessions[0].identity.push_str(" replacement owner");
        assert!(!confirmed.preserves_consent(&settled));
    }

    fn preview_with_daemon(daemon: DaemonImpact) -> Preview {
        Preview {
            candidate: Candidate {
                version: "2".into(),
                protocol: 166,
                digest: "verified".into(),
            },
            replaces_daemon: true,
            daemon: Some(daemon),
            from_version: "1".into(),
            windows: 1,
            stopped: Vec::new(),
            stopped_windows: Vec::new(),
            labels: Default::default(),
            editors: Vec::new(),
        }
    }

    #[test]
    fn preview_consent_survives_approved_activity_and_background_task_settlement() {
        for policy in [Policy::WhenIdle, Policy::Interrupt] {
            let mut daemon = waiting();
            daemon.policy = policy;
            if policy == Policy::Interrupt {
                daemon.sessions[0].disposition = Disposition::Interrupted;
            }
            daemon.sessions[0].interrupted_subagents = vec!["first".into(), "second".into()];
            let approved = preview_with_daemon(daemon);
            let mut reduced = approved.clone();
            reduced.daemon.as_mut().unwrap().sessions[0]
                .interrupted_subagents
                .remove(0);
            assert!(approved.preserves_consent(&reduced), "{policy:?}");
            assert!(!reduced.preserves_consent(&approved), "{policy:?}");
            let mut settled = reduced;
            let daemon = settled.daemon.as_mut().unwrap();
            daemon.capability.token = "renewed".into();
            daemon.capability.expires_at_unix_ms += 100;
            daemon.sessions[0].disposition = Disposition::Resumes;
            daemon.sessions[0].interrupted_subagents.clear();
            assert!(approved.preserves_consent(&settled), "{policy:?}");
            assert!(!settled.preserves_consent(&approved), "{policy:?}");
        }
    }

    #[test]
    fn preview_activity_settlement_cannot_hide_changed_review_content_or_owners() {
        let approved = preview_with_daemon(waiting());
        let mut settled = approved.clone();
        settled.daemon.as_mut().unwrap().sessions[0].disposition = Disposition::Resumes;
        assert!(approved.preserves_consent(&settled));
        for change in [
            "title",
            "owner",
            "kind",
            "notice",
            "window",
            "saved outcome",
            "stop",
        ] {
            let mut changed = settled.clone();
            match change {
                "title" => changed.daemon.as_mut().unwrap().sessions[0]
                    .title
                    .push_str(" renamed"),
                "owner" => changed.daemon.as_mut().unwrap().sessions[0]
                    .identity
                    .push_str(" replaced"),
                "kind" => changed.daemon.as_mut().unwrap().sessions[0].kind = SessionKind::Terminal,
                "notice" => changed
                    .labels
                    .details
                    .notices
                    .push("A pending return to terminal needs attention.".into()),
                "window" => changed.windows += 1,
                "saved outcome" => {
                    let pane = changed.daemon.as_ref().unwrap().sessions[0].id;
                    changed.labels.details.panes.insert(
                        pane,
                        super::super::source_inspection::InspectedPane {
                            kind: SessionKind::NativeChat,
                            outcome: super::super::source_inspection::SavedOutcome::Resumes,
                            detail: "Its saved pane requires a new recovery step.".into(),
                        },
                    );
                }
                "stop" => {
                    changed
                        .stopped_windows
                        .push(super::super::source_inspection::StoppedWindow {
                            window: 1,
                            reason: "A saved window could not be read".into(),
                            recovery: "Its saved file stays as it is".into(),
                        })
                }
                _ => unreachable!(),
            }
            assert!(!approved.preserves_consent(&changed), "{change}");
        }
    }

    #[test]
    fn editor_census_is_backwards_compatible_and_does_not_expand_consent() {
        let before: Preview = serde_json::from_value(serde_json::json!({
            "candidate": { "version": "1.0", "protocol": 160, "digest": "verified" },
            "replaces_daemon": true,
            "daemon": null
        }))
        .unwrap();
        assert!(before.editors.is_empty());
        let mut after = before.clone();
        after.editors.push(crate::install_guard::EditorCompanion {
            name: "Ceditor".into(),
            version: Some("07e7efc5".into()),
            protocol: Some(127),
            pid: 42,
        });
        assert!(before.preserves_consent(&after));
        assert!(after.preserves_consent(&before));
        let roundtrip: Preview =
            serde_json::from_slice(&serde_json::to_vec(&after).unwrap()).unwrap();
        assert_eq!(after, roundtrip);
    }

    #[test]
    fn duplicate_inventory_and_changed_candidate_cannot_reuse_confirmation() {
        let mut previous = waiting();
        let mut additional = previous.sessions[0].clone();
        additional.id = TerminalId::new();
        previous.sessions.push(additional);
        let mut duplicated = previous.clone();
        duplicated.sessions[1] = duplicated.sessions[0].clone();
        assert!(!previous.preserves_consent(&duplicated));
        assert!(!duplicated.preserves_consent(&previous));
        let preview = Preview {
            from_version: String::new(),
            windows: 0,
            stopped: Vec::new(),
            stopped_windows: Vec::new(),
            labels: Default::default(),
            editors: Vec::new(),
            candidate: Candidate {
                version: "1.0.0".into(),
                protocol: 31,
                digest: "verified-bytes".into(),
            },
            replaces_daemon: true,
            daemon: Some(previous),
        };
        let mut changed = preview.clone();
        changed.candidate.digest = "different-local-build".into();
        assert!(!preview.preserves_consent(&changed));
    }
    #[test]
    fn stopped_outcomes_require_the_same_panes_windows_and_reasons() {
        use super::super::source_inspection::{StoppedKind, StoppedPane, StoppedWindow};
        let preview = Preview {
            editors: Vec::new(),
            candidate: Candidate {
                version: "2".into(),
                protocol: 160,
                digest: "same".into(),
            },
            replaces_daemon: false,
            daemon: None,
            from_version: "1".into(),
            windows: 2,
            stopped: vec![StoppedPane {
                pane: TerminalId::new(),
                kind: StoppedKind::Chat,
                label: "Review".into(),
                reason: "its conversation could not be confirmed".into(),
                recovery: "Press Resume in the pane to reopen it".into(),
            }],
            stopped_windows: vec![StoppedWindow {
                window: 2,
                reason: "Window 2's saved panes could not be read".into(),
                recovery: "The saved file is kept unchanged".into(),
            }],
            labels: Default::default(),
        };
        assert!(preview.preserves_consent(&preview));
        let mut changed = preview.clone();
        changed.stopped[0].pane = TerminalId::new();
        assert!(!preview.preserves_consent(&changed));
        changed = preview.clone();
        changed.stopped[0].reason = "ended before it recorded its conversation".into();
        assert!(!preview.preserves_consent(&changed));
        changed = preview.clone();
        changed.stopped.clear();
        assert!(!preview.preserves_consent(&changed));
        changed = preview.clone();
        changed.stopped_windows[0].window = 1;
        assert!(!preview.preserves_consent(&changed));
        changed = preview.clone();
        changed.stopped_windows[0].reason = "another reason".into();
        assert!(!preview.preserves_consent(&changed));
        changed = preview.clone();
        changed.stopped.push(changed.stopped[0].clone());
        assert!(!preview.preserves_consent(&changed));
        changed = preview.clone();
        changed.stopped[0].label = "Renamed".into();
        assert!(!preview.preserves_consent(&changed));
    }

    /// A waiting conversation, one that needs attention, one the update
    /// carries, and agent launches still settling.
    pub(in crate::update) fn blocked() -> DaemonImpact {
        let mut daemon = waiting();
        daemon.sessions[0].disposition = Disposition::Blocked;
        daemon.sessions[0].reason = STOPPED_WITH_UNSETTLED_WORK.into();
        let mut carried = daemon.sessions[0].clone();
        carried.id = TerminalId::new();
        carried.title = "Shell".into();
        carried.disposition = Disposition::Continues;
        carried.reason = "Keeps running".into();
        daemon.sessions.push(carried);
        daemon
            .blockers
            .push("Additional agent launches must settle before updating".into());
        daemon
    }

    pub(in crate::update) fn blocked_preview() -> Preview {
        Preview {
            editors: Vec::new(),
            candidate: Candidate {
                version: "9.8.7".into(),
                protocol: 32,
                digest: "original".into(),
            },
            replaces_daemon: true,
            daemon: Some(blocked()),
            from_version: String::new(),
            windows: 0,
            stopped: Vec::new(),
            stopped_windows: Vec::new(),
            labels: Default::default(),
        }
    }

    #[test]
    fn a_refusal_names_only_what_holds_the_update_back_in_review_wording() {
        let mut preview = Preview {
            daemon: Some(waiting()),
            ..blocked_preview()
        };
        assert_eq!(preview.refusal(), None, "waiting for idle is not refused");
        preview = blocked_preview();
        let refusal = preview.refusal().unwrap();
        assert_eq!(
            refusal.sessions,
            vec![RefusedSession {
                title: "Claude".into(),
                reason: "Its provider stopped with unfinished work. Close this conversation before updating.".into(),
            }]
        );
        assert_eq!(
            refusal.blockers,
            ["Agents are still starting. Wait for them to finish opening before updating."]
        );
        assert_eq!(
            refusal.summary(),
            "1 session needs attention and other work must finish first"
        );
        assert_eq!(
            refusal.message(),
            "Update not installed: 1 session needs attention and other work must finish first. \
             Details: Claude: Its provider stopped with unfinished work. \
             Close this conversation before updating; \
             Agents are still starting. Wait for them to finish opening before updating."
        );

        // The sidebar's name tells untitled conversations apart, as the
        // terminal review does.
        let id = preview.daemon.as_ref().unwrap().sessions[0].id;
        preview.labels.names.insert(id, "Release review".into());
        assert_eq!(
            preview.refusal().unwrap().sessions[0].title,
            "Release review"
        );

        // Free-form source diagnostics never reach the record.
        let daemon = preview.daemon.as_mut().unwrap();
        daemon.sessions[0].reason = "socket /run/private/token refused".into();
        daemon.blockers = vec!["internal: lease 42 busy".into()];
        let refusal = preview.refusal().unwrap();
        assert!(!refusal.message().contains("socket"));
        assert!(!refusal.message().contains("lease 42"));

        let mut service = blocked_preview();
        service.daemon.as_mut().unwrap().sessions.clear();
        assert_eq!(
            service.refusal().unwrap().summary(),
            "other work must finish first"
        );
        let mut sessions = blocked_preview();
        let daemon = sessions.daemon.as_mut().unwrap();
        daemon.blockers.clear();
        daemon.sessions[1].disposition = Disposition::Blocked;
        assert_eq!(
            sessions.refusal().unwrap().summary(),
            "2 sessions need attention"
        );
        let expired = Refusal {
            expired: true,
            ..sessions.refusal().unwrap()
        };
        assert_eq!(
            expired.summary(),
            "the review expired while 2 sessions need attention"
        );
    }

    #[test]
    fn older_preview_json_defaults_the_outcomes_and_context() {
        let preview: Preview = serde_json::from_value(serde_json::json!({
            "candidate": {"version":"2", "protocol":160, "digest":"same"},
            "replaces_daemon":false, "daemon":null
        }))
        .unwrap();
        assert!(preview.stopped.is_empty());
        assert!(preview.stopped_windows.is_empty());
        assert!(preview.labels.names.is_empty());
        assert!(preview.from_version.is_empty());
        assert_eq!(preview.windows, 0);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub version: String,
    pub protocol: u16,
    /// Content hash of the verified release archive or installed matched set.
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    #[serde(default)]
    pub editors: Vec<crate::install_guard::EditorCompanion>,
    pub candidate: Candidate,
    /// Compatible updates leave the daemon and its entire inventory in place.
    pub replaces_daemon: bool,
    pub daemon: Option<DaemonImpact>,
    #[serde(default)]
    pub from_version: String,
    #[serde(default)]
    pub windows: usize,
    #[serde(default)]
    pub stopped: Vec<super::source_inspection::StoppedPane>,
    #[serde(default)]
    pub stopped_windows: Vec<super::source_inspection::StoppedWindow>,
    #[serde(flatten)]
    pub labels: super::source_inspection::ReviewInventory,
}

impl Preview {
    /// What holds this preview back, if anything has to be resolved by hand
    /// before it can install. Each session is named as the review names it.
    pub fn refusal(&self) -> Option<Refusal> {
        let daemon = self.daemon.as_ref().filter(|daemon| daemon.blocked())?;
        Some(Refusal {
            sessions: daemon
                .sessions
                .iter()
                .filter(|session| session.disposition == Disposition::Blocked)
                .map(|session| {
                    let review = session.review();
                    RefusedSession {
                        title: self
                            .labels
                            .names
                            .get(&session.id)
                            .cloned()
                            .unwrap_or(review.title),
                        reason: review.reason,
                    }
                })
                .collect(),
            blockers: daemon.review_notices(),
            expired: false,
        })
    }

    pub(super) fn with_inspection(
        mut self,
        inspection: &super::source_inspection::SourceInspection,
    ) -> Self {
        self.labels.details = inspection.details.clone();
        self.stopped = inspection.stopped.clone();
        self.stopped_windows = inspection.stopped_windows.clone();
        self.labels.names = inspection.labels.clone();
        self
    }

    /// Stop consent binds the candidate and exact listed owners. Activity is
    /// display-only here; the ordinary review has already settled the wait.
    pub(super) fn preserves_stop_consent(&self, observed: &Self) -> bool {
        self.candidate == observed.candidate
            && self.replaces_daemon == observed.replaces_daemon
            && self.daemon.as_ref().map(|daemon| &daemon.capability.proof)
                == observed
                    .daemon
                    .as_ref()
                    .map(|daemon| &daemon.capability.proof)
    }

    pub fn preserves_consent(&self, observed: &Self) -> bool {
        if !super::source_inspection::SourceInspection::same_outcomes(
            &self.stopped,
            &self.stopped_windows,
            &observed.stopped,
            &observed.stopped_windows,
        ) || self.labels.details.console_capabilities
            != observed.labels.details.console_capabilities
            || self.candidate != observed.candidate
            || self.replaces_daemon != observed.replaces_daemon
            || !match (&self.daemon, &observed.daemon) {
                (None, None) => true,
                (Some(previous), Some(current)) => previous.preserves_consent(current),
                _ => false,
            }
        {
            return false;
        }

        let mut settled = self.clone();
        // Editor census entries describe independent companions; their protocol
        // preflight remains separate from consent to this update's consequences.
        settled.editors.clone_from(&observed.editors);
        if let (Some(previous), Some(current)) = (&mut settled.daemon, &observed.daemon) {
            for session in &mut previous.sessions {
                let Some(now) = current
                    .sessions
                    .iter()
                    .find(|now| now.id == session.id && now.kind == session.kind)
                else {
                    return false;
                };
                // The daemon comparison above proved these are the same owners
                // and only permitted activity settlement or shrinking task sets.
                // Keep all titles, source reasons, saved outcomes and notices so
                // settling work cannot conceal any other change to the review.
                session.disposition = now.disposition;
                session
                    .interrupted_subagents
                    .clone_from(&now.interrupted_subagents);
            }
        }
        super::review::Review::from(&settled) == super::review::Review::from(observed)
    }
}
