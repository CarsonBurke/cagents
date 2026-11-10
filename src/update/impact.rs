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
    /// Whether something has to be resolved by hand before the update can
    /// install. A conversation the update waits on is not that.
    pub fn blocked(&self) -> bool {
        !self.blockers.is_empty()
            || self
                .sessions
                .iter()
                .any(|session| session.disposition == Disposition::Blocked)
    }

    /// What holds this preview back, if anything has to be resolved by hand.
    pub fn refusal(&self) -> Option<Refusal> {
        self.blocked().then(|| Refusal {
            sessions: self
                .sessions
                .iter()
                .filter(|session| session.disposition == Disposition::Blocked)
                .map(|session| RefusedSession {
                    title: session.title.clone(),
                    reason: session.reason.clone(),
                })
                .collect(),
            blockers: self.blockers.clone(),
            expired: false,
        })
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
                            || (self.policy == Policy::WhenIdle
                                && previous.disposition == Disposition::WaitsForIdle
                                && current.disposition == Disposition::Resumes))
                })
            })
    }
}

/// Why a reviewed update was not installed: what its preview said had to be
/// resolved first. The job keeps it, so whoever reads the outcome later sees
/// the reasons the person reviewed rather than a bare cancellation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub sessions: Vec<RefusedSession>,
    /// Reasons that belong to no one session: work still settling, such as
    /// agent launches or accepted provider work.
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

/// Separates a stopped update's summary from its details, as every reader of
/// an update's message expects.
const DETAILS_LABEL: &str = " Details: ";

impl Refusal {
    /// "1 session needs attention", "2 sessions need attention and other
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

    /// Each reason, worded on its own line of a notice's details.
    pub fn details(&self) -> Vec<String> {
        self.sessions
            .iter()
            .map(|session| format!("{}: {}", session.title, session.reason))
            .chain(self.blockers.iter().cloned())
            .collect()
    }

    /// The job message: a summary, then every reason after the details label.
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

    #[test]
    fn a_refusal_names_only_what_holds_the_update_back() {
        assert_eq!(waiting().refusal(), None, "waiting for idle is not refused");
        let refusal = blocked().refusal().unwrap();
        assert_eq!(
            refusal.sessions,
            vec![RefusedSession {
                title: "Claude".into(),
                reason: STOPPED_WITH_UNSETTLED_WORK.into(),
            }]
        );
        assert_eq!(
            refusal.summary(),
            "1 session needs attention and other work must finish first"
        );
        assert_eq!(
            refusal.message(),
            "Update not installed: 1 session needs attention and other work must finish first. \
             Details: Claude: Its provider stopped but may still hold unfinished work. \
             Close this conversation to release it; \
             Additional agent launches must settle before updating."
        );
        let mut service = blocked();
        service.sessions.clear();
        assert_eq!(
            service.refusal().unwrap().summary(),
            "other work must finish first"
        );
        let mut sessions = blocked();
        sessions.blockers.clear();
        sessions.sessions[1].disposition = Disposition::Blocked;
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
    pub candidate: Candidate,
    /// Compatible updates leave the daemon and its entire inventory in place.
    pub replaces_daemon: bool,
    pub daemon: Option<DaemonImpact>,
}

impl Preview {
    pub fn preserves_consent(&self, observed: &Self) -> bool {
        self.candidate == observed.candidate
            && self.replaces_daemon == observed.replaces_daemon
            && match (&self.daemon, &observed.daemon) {
                (None, None) => true,
                (Some(previous), Some(current)) => previous.preserves_consent(current),
                _ => false,
            }
    }
}
