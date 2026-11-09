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
            from_version: String::new(),
            windows: 0,
            stopped: Vec::new(),
            stopped_windows: Vec::new(),
            labels: Default::default(),
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
        assert!(preview.preserves_consent(&changed));
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
        assert!(preview.labels.is_empty());
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
    #[serde(default)]
    pub labels: BTreeMap<TerminalId, super::source_inspection::PaneLabel>,
}

impl Preview {
    pub(super) fn inspection(&self) -> super::source_inspection::SourceInspection {
        super::source_inspection::SourceInspection {
            stopped: self.stopped.clone(),
            stopped_windows: self.stopped_windows.clone(),
            labels: self.labels.clone(),
        }
    }

    pub(super) fn with_inspection(
        mut self,
        inspection: &super::source_inspection::SourceInspection,
    ) -> Self {
        self.stopped = inspection.stopped.clone();
        self.stopped_windows = inspection.stopped_windows.clone();
        self.labels = inspection.labels.clone();
        self
    }

    pub fn preserves_consent(&self, observed: &Self) -> bool {
        super::source_inspection::SourceInspection::same_outcomes(
            &self.stopped,
            &self.stopped_windows,
            &observed.stopped,
            &observed.stopped_windows,
        ) && self.candidate == observed.candidate
            && self.replaces_daemon == observed.replaces_daemon
            && match (&self.daemon, &observed.daemon) {
                (None, None) => true,
                (Some(previous), Some(current)) => previous.preserves_consent(current),
                _ => false,
            }
    }
}
