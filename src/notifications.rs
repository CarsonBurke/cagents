//! Agent attention is semantic lifecycle evidence, never a guess from output.
//!
//! The daemon owns delivery and the durable journal. Windows report focus and
//! acknowledge attention; they do not each send a desktop notification.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::{
    agent::Provider,
    model::{RunId, TerminalId},
};

/// Durable activation and lifecycle identity. A source process is only context:
/// retiring it must never retire an accepted run's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Target {
    Session(TerminalId),
    AgentRun(RunId),
}

impl Target {
    pub(crate) fn storage_key(self) -> String {
        match self {
            Self::Session(id) => id.to_string(),
            Self::AgentRun(id) => format!("run:{id}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Succeeded,
    Failed,
    Cancelled,
    Waiting,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Succeeded => "Agent finished",
            Self::Failed => "Agent failed",
            Self::Cancelled => "Agent cancelled",
            Self::Waiting => "Agent needs your attention",
        }
    }
}

/// A provider's stable turn/request boundary, not a transcript or task title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub key: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub target: Target,
    /// Focus and preferred-window metadata only, never lifecycle authority.
    pub source_session: Option<TerminalId>,
    pub provider: Option<Provider>,
    pub directory: PathBuf,
    pub evidence: Evidence,
}

impl Event {
    pub fn session(
        session: TerminalId,
        provider: Provider,
        directory: PathBuf,
        evidence: Evidence,
    ) -> Self {
        Self {
            target: Target::Session(session),
            source_session: Some(session),
            provider: Some(provider),
            directory,
            evidence,
        }
    }

    pub fn target(&self) -> Target {
        self.target
    }
    pub fn source_session(&self) -> Option<TerminalId> {
        self.source_session
    }

    /// Only the JSON journal accepts the old shape. Keeping wire decoding
    /// explicit avoids untagged/self-describing serde requirements in postcard.
    pub(crate) fn from_journal(value: &str) -> Result<Self, serde_json::Error> {
        #[derive(Deserialize)]
        struct Legacy {
            session: TerminalId,
            provider: Provider,
            directory: PathBuf,
            evidence: Evidence,
        }
        let json: serde_json::Value = serde_json::from_str(value)?;
        if json.get("target").is_some() {
            serde_json::from_value(json)
        } else {
            let old: Legacy = serde_json::from_value(json)?;
            Ok(Self::session(
                old.session,
                old.provider,
                old.directory,
                old.evidence,
            ))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub enabled: bool,
    pub success: bool,
    pub failure: bool,
    pub waiting: bool,
    pub while_focused: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            enabled: true,
            success: true,
            failure: true,
            waiting: true,
            while_focused: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub id: uuid::Uuid,
    pub event: Event,
    pub delivered: bool,
    pub acknowledged: bool,
    pub window: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub notices: Vec<Notice>,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    pub window: String,
    pub sessions: Vec<TerminalId>,
    pub focused: Option<TerminalId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Enabled,
    Success,
    Failure,
    Waiting,
    WhileFocused,
}

impl Preferences {
    pub fn get(self, field: Field) -> bool {
        match field {
            Field::Enabled => self.enabled,
            Field::Success => self.success,
            Field::Failure => self.failure,
            Field::Waiting => self.waiting,
            Field::WhileFocused => self.while_focused,
        }
    }
    pub fn set(&mut self, field: Field, value: bool) {
        *match field {
            Field::Enabled => &mut self.enabled,
            Field::Success => &mut self.success,
            Field::Failure => &mut self.failure,
            Field::Waiting => &mut self.waiting,
            Field::WhileFocused => &mut self.while_focused,
        } = value;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    Changed,
    Activated(Target),
}

impl Preferences {
    pub fn allows(self, kind: Kind, focused: bool) -> bool {
        self.enabled
            && (!focused || self.while_focused)
            && match kind {
                Kind::Succeeded => self.success,
                Kind::Failed | Kind::Cancelled => self.failure,
                Kind::Waiting => self.waiting,
            }
    }
}

/// Only bounded, non-content context may cross into desktop notifications.
pub fn context(event: &Event) -> String {
    let directory = event
        .directory
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let directory: String = directory
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    match (event.provider, directory.is_empty()) {
        (Some(provider), true) => provider.label().to_owned(),
        (Some(provider), false) => format!("{} · {directory}", provider.label()),
        (None, _) => directory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_session_journal_and_typed_wire_have_one_target() {
        let session = TerminalId::new();
        let legacy = serde_json::json!({"session":session,"provider":Provider::Claude,"directory":"/project","evidence":{"key":"turn","kind":"Succeeded"}});
        let event = Event::from_journal(&legacy.to_string()).unwrap();
        assert_eq!(event.target(), Target::Session(session));
        assert_eq!(event.source_session(), Some(session));
        let event = Event {
            target: Target::AgentRun(RunId::new()),
            source_session: None,
            provider: None,
            directory: "/project".into(),
            evidence: event.evidence,
        };
        let bytes = postcard::to_stdvec(&event).unwrap();
        assert_eq!(postcard::from_bytes::<Event>(&bytes).unwrap(), event);
        assert_eq!(
            Event::from_journal(&serde_json::to_string(&event).unwrap()).unwrap(),
            event
        );
        assert_eq!(context(&event), "project");
    }

    #[test]
    fn preferences_filter_each_outcome_and_focused_sessions() {
        let mut preferences = Preferences::default();
        for kind in [
            Kind::Succeeded,
            Kind::Failed,
            Kind::Cancelled,
            Kind::Waiting,
        ] {
            assert!(preferences.allows(kind, false));
            assert!(!preferences.allows(kind, true));
        }
        preferences.failure = false;
        assert!(!preferences.allows(Kind::Failed, false));
        assert!(!preferences.allows(Kind::Cancelled, false));
        assert!(preferences.allows(Kind::Succeeded, false));
        preferences.while_focused = true;
        assert!(preferences.allows(Kind::Succeeded, true));
        preferences.enabled = false;
        assert!(!preferences.allows(Kind::Succeeded, true));
    }

    #[test]
    fn context_is_bounded_and_does_not_include_parent_paths() {
        let event = Event {
            target: Target::Session(TerminalId::new()),
            source_session: None,
            provider: Some(Provider::Codex),
            directory: PathBuf::from(format!("/private/account/{}\n", "x".repeat(200))),
            evidence: Evidence {
                key: "turn".into(),
                kind: Kind::Succeeded,
            },
        };
        let body = context(&event);
        assert!(!body.contains("private"));
        assert!(!body.contains('\n'));
        assert!(body.chars().count() < 140);
    }
}
