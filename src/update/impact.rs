//! The update confirmation describes one candidate and one observed inventory.
//! A count alone cannot authorize losing a different conversation or process.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonImpact {
    pub capability: ShutdownCapability,
    pub policy: Policy,
    pub sessions: Vec<SessionImpact>,
    pub blockers: Vec<String>,
}

impl DaemonImpact {
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
                    previous.identity == current.identity
                        && (previous.disposition == current.disposition
                            || (self.policy == Policy::WhenIdle
                                && previous.disposition == Disposition::WaitsForIdle
                                && current.disposition == Disposition::Resumes))
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId, HostSessionProof};

    fn waiting() -> DaemonImpact {
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
