//! Legacy wire types for reporting update impact and requesting preparation.

use serde::{Deserialize, Serialize};

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
    pub reason: String,
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
    pub fn review(&self) -> SessionReview {
        let reason = match self.disposition {
            Disposition::Continues => match self.kind {
                SessionKind::Terminal => "The terminal and its running processes stay alive.".into(),
                SessionKind::NativeChat => "The conversation and its running work stay alive.".into(),
            },
            Disposition::Resumes => "Restarts on its recorded conversation.".into(),
            Disposition::WaitsForIdle => {
                let tasks = self.interrupted_subagents.len();
                if self.reason == QUEUED_MESSAGES {
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
            Disposition::Blocked => match self.reason.as_str() {
                "Its turn finished, but background agents are still running. Review stopping them to continue the update." => "Its turn finished, but background agents are still running. Review stopping them to continue the update.".into(),
                STOPPED_WITH_UNSETTLED_WORK => "Its provider stopped with unfinished work. Close this conversation before updating.".into(),
                "Finish the pending return to terminal or close this conversation." => "Finish returning to the terminal or close this conversation before updating.".into(),
                "Its provider is not ready yet. This clears on its own once it finishes starting; close the conversation if it does not." => "Its provider is still starting. Wait for it to finish, then try the update again.".into(),
                "Its provider has not reported which conversation it is running, so it cannot be resumed after the update. Send it a message or close it." => "Its conversation has not been recorded yet. Send it a message or close it before updating.".into(),
                "This terminal cannot preserve its live process through this update; finish it or close it before updating." => "Its running processes cannot be preserved through this update. Finish their work or close this terminal before updating.".into(),
                "The paired console cannot preserve its live process through this update." => "Its terminal cannot keep running through this update. Finish its work or close it before updating.".into(),
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

    /// Check destructive scope independently of routine work and presentation.
    pub fn covers_interruptions(&self, observed: &Self) -> bool {
        use super::ledger::EndRequest;

        self.capability.proof == observed.capability.proof
            && EndRequest::interruptions(observed)
                .all(|request| EndRequest::interruptions(self).any(|end| end.covers(&request)))
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
    fn policy_consent_ignores_routine_ownership_and_activity_changes() {
        let approved = waiting();
        let mut observed = approved.clone();
        observed.capability.token = "renewed".into();
        observed.sessions[0].identity = "another incarnation".into();
        observed.sessions[0].disposition = Disposition::Resumes;
        let mut arriving = observed.sessions[0].clone();
        arriving.id = TerminalId::new();
        arriving.disposition = Disposition::WaitsForIdle;
        observed.sessions.push(arriving);
        assert!(approved.covers_interruptions(&observed));
        observed.sessions[0].disposition = Disposition::Interrupted;
        assert!(!approved.covers_interruptions(&observed));
        observed.sessions.clear();
        observed.capability.proof.host_incarnation = HostIncarnation::new("replacement");
        assert!(!approved.covers_interruptions(&observed));
    }

    #[test]
    fn interruption_consent_only_covers_the_owner_and_tasks_shown() {
        let mut approved = waiting();
        approved.policy = Policy::Interrupt;
        approved.sessions[0].disposition = Disposition::Interrupted;
        approved.sessions[0].interrupted_subagents = vec!["first".into(), "second".into()];
        let mut observed = approved.clone();
        observed.sessions[0].interrupted_subagents.remove(0);
        assert!(approved.covers_interruptions(&observed));
        observed.sessions[0]
            .interrupted_subagents
            .push("replacement".into());
        assert!(!approved.covers_interruptions(&observed));
        observed.sessions[0].interrupted_subagents.clear();
        observed.sessions[0].identity = "replacement owner".into();
        assert!(!approved.covers_interruptions(&observed));
        observed.sessions[0].disposition = Disposition::Resumes;
        assert!(approved.covers_interruptions(&observed));
    }

    #[test]
    fn editor_census_is_backwards_compatible() {
        let mut preview: Preview = serde_json::from_value(serde_json::json!({
            "candidate": { "version": "1.0", "protocol": 160, "digest": "verified" },
            "replaces_daemon": true,
            "daemon": null
        }))
        .unwrap();
        assert!(preview.editors.is_empty());
        preview.editors.push(crate::install_guard::EditorCompanion {
            name: "Ceditor".into(),
            version: Some("07e7efc5".into()),
            protocol: Some(127),
            pid: 42,
        });
        let roundtrip: Preview =
            serde_json::from_slice(&serde_json::to_vec(&preview).unwrap()).unwrap();
        assert_eq!(preview, roundtrip);
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

    /// The signed target and authenticated source delimit the policy decision.
    pub(super) fn same_target(&self, observed: &Self) -> bool {
        self.candidate == observed.candidate
            && self.replaces_daemon == observed.replaces_daemon
            && self.daemon.as_ref().map(|daemon| &daemon.capability.proof)
                == observed
                    .daemon
                    .as_ref()
                    .map(|daemon| &daemon.capability.proof)
    }
}
