//! Agent attention is semantic lifecycle evidence, never a guess from output.
//!
//! The daemon owns delivery and the durable journal. Windows report focus and
//! acknowledge attention; they do not each send a desktop notification.

pub mod presentation;
pub mod review;
#[cfg(feature = "ui")]
pub mod ui;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::{
    agent::Provider,
    model::{RunId, TerminalId},
};

/// Durable activation and lifecycle identity. Optional source-process metadata
/// describes focus and window ownership, not the notice's retirement authority.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Target {
    Session(TerminalId),
    PullRequest(review::ReviewRequest),
    AgentRun(RunId),
    /// Pending terminal mailbox attention survives recipient process replacement.
    SessionInbox(crate::agent_execution::Lineage),
    /// An exact recorded lifecycle edge; the storage group remains the automation.
    Automation {
        id: uuid::Uuid,
        sequence: u64,
    },
    /// A historical creation, never current pending or consent authority.
    AutomationProposal {
        registry: uuid::Uuid,
        id: uuid::Uuid,
        revision: u64,
    },
}

impl Target {
    pub(crate) fn storage_key(&self) -> String {
        match self {
            Self::Session(id) => id.to_string(),
            Self::PullRequest(request) => request.storage_key(),
            Self::AgentRun(id) => format!("run:{id}"),
            Self::SessionInbox(lineage) => format!("inbox:{lineage}"),
            Self::Automation { id, .. } => format!("automation:{id}"),
            Self::AutomationProposal {
                registry,
                id,
                revision,
            } => format!("proposal:{registry}:{id}:revision:{revision}"),
        }
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum Application {
    #[default]
    Cagents,
    Ceditor,
}

/// A real shell window, distinct from the lifecycle identity being opened.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Destination {
    pub application: Application,
    pub window: String,
}

impl Destination {
    pub fn validate(&self) -> Result<(), String> {
        crate::instance::validate_window_slot(&self.window).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Succeeded,
    Failed,
    Cancelled,
    Waiting,
    ReviewRequested,
    Interrupted,
    MessageReceived,
    AutomationPaused,
    AutomationBlocked,
    ProposalCreated,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Succeeded => "Agent finished",
            Self::Failed => "Agent failed",
            Self::Cancelled => "Agent cancelled",
            Self::Waiting => "Agent needs your attention",
            Self::ReviewRequested => "Your review was requested",
            Self::Interrupted => "Agent outcome unknown",
            Self::MessageReceived => "Session messages waiting",
            Self::AutomationPaused => "Automation pause recorded",
            Self::AutomationBlocked => "Automation block recorded",
            Self::ProposalCreated => "Automation request created",
        }
    }
}

/// A provider's stable turn/request boundary, not a transcript or task title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub key: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestType {
    Approval,
    Question,
    Consent,
}

/// Opt-in semantic context; the broker removes this before notification-journal
/// persistence when previews are disabled. Producers bound and sanitize it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Content {
    pub conversation: String,
    pub preview: String,
    pub request: Option<RequestType>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub target: Target,
    /// Focus and preferred-window metadata only, never lifecycle authority.
    pub source_session: Option<TerminalId>,
    pub provider: Option<Provider>,
    pub directory: Option<PathBuf>,
    #[serde(default)]
    pub destination: Option<Destination>,
    pub evidence: Evidence,
    #[serde(default)]
    pub content: Option<Content>,
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
            directory: Some(directory),
            destination: None,
            evidence,
            content: None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(content) = &self.content {
            if content.conversation.chars().count() > presentation::TITLE_LIMIT
                || content.preview.chars().count() > presentation::PREVIEW_LIMIT
                || content
                    .conversation
                    .chars()
                    .chain(content.preview.chars())
                    .any(char::is_control)
                || (content.request.is_some() && self.evidence.kind != Kind::Waiting)
            {
                return Err("Notification preview exceeds its text boundary".into());
            }
        }
        if self.evidence.key.len() > 1024
            || self
                .directory
                .as_ref()
                .is_some_and(|directory| directory.as_os_str().len() > 4096)
        {
            return Err("Notification metadata exceeds its size limit".into());
        }
        if let Some(destination) = &self.destination {
            destination.validate()?;
        }
        if matches!(self.target, Target::SessionInbox(_)) {
            if self.content.is_some()
                || self.evidence.kind != Kind::MessageReceived
                || self.source_session.is_none()
                || self.provider.is_some()
                || self.directory.is_some()
                || self.destination.is_some()
                || self
                    .evidence
                    .key
                    .parse::<crate::model::MessageId>()
                    .is_err()
            {
                return Err("Inbox alerts require a live terminal and a durable arrival identity without content metadata".into());
            }
        } else if self.evidence.kind == Kind::MessageReceived {
            return Err("Message alerts require a session inbox target".into());
        }
        if let Target::PullRequest(request) = &self.target {
            request.validate()?;
            if self.content.is_some()
                || self.evidence.kind != Kind::ReviewRequested
                || self.evidence.key != request.request
                || self.source_session.is_some()
                || self.provider.is_some()
                || self.directory.is_some()
                || self.destination.is_none()
            {
                return Err("Review alerts require exact request and application authority, without provider metadata".into());
            }
        } else if self.evidence.kind == Kind::ReviewRequested {
            return Err("Review alerts require a pull-request target".into());
        }
        if let Target::Automation { id, sequence } = &self.target {
            if id.is_nil()
                || *sequence == 0
                || *sequence > i64::MAX as u64
                || !matches!(
                    self.evidence.kind,
                    Kind::AutomationPaused | Kind::AutomationBlocked
                )
                || self.evidence.key != format!("automation:{id}:lifecycle:{sequence}")
                || self.source_session.is_some()
                || self.provider.is_some()
                || self.directory.is_some()
                || self.destination.is_some()
            {
                return Err(
                    "Automation alerts require exact lifecycle identity without actor metadata"
                        .into(),
                );
            }
        } else if matches!(
            self.evidence.kind,
            Kind::AutomationPaused | Kind::AutomationBlocked
        ) {
            return Err("Automation attention requires an automation target".into());
        }
        if let Target::AutomationProposal {
            registry,
            id,
            revision,
        } = &self.target
        {
            if registry.is_nil()
                || id.is_nil()
                || *revision == 0
                || i64::try_from(*revision).is_err()
                || self.evidence.kind != Kind::ProposalCreated
                || self.evidence.key
                    != format!("proposal:{registry}:{id}:revision:{revision}:created")
                || self.source_session.is_some()
                || self.provider.is_some()
                || self.directory.is_some()
                || self.destination.is_some()
            {
                return Err("Proposal alerts require exact registry/request/revision without actor metadata".into());
            }
        } else if self.evidence.kind == Kind::ProposalCreated {
            return Err("Proposal creation attention requires an exact proposal target".into());
        }
        Ok(())
    }

    pub fn target(&self) -> Target {
        self.target.clone()
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
    pub review_requests: bool,
    /// Explicit opt-in; existing configurations retain metadata-only alerts.
    pub message_previews: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            enabled: true,
            success: true,
            failure: true,
            waiting: true,
            while_focused: false,
            review_requests: true,
            message_previews: false,
        }
    }
}

/// How long the daemon waits for a test notification's outcome. It covers
/// a portal connection and the notification itself, each bounded by the
/// daemon's own portal timeout, behind at most one bounded delivery batch.
pub const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// What became of one requested test notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TestOutcome {
    /// The desktop accepted it; how it is presented, and whether it makes a
    /// sound, is the desktop's own choice.
    Sent,
    /// The desktop notification service could not be reached or refused it.
    Unavailable(String),
    /// The desktop notification service did not answer in time.
    TimedOut,
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

/// Reading a target clears its existing alert on a focus edge. An opted-in
/// fresh alert for the target already being watched remains visible.
pub fn focused_notice_ids(
    snapshot: &Snapshot,
    target: Option<&Target>,
    source_session: Option<TerminalId>,
    focus_changed: bool,
    while_focused: bool,
) -> Vec<uuid::Uuid> {
    if !focus_changed && while_focused {
        return Vec::new();
    }
    snapshot
        .notices
        .iter()
        .filter(|notice| {
            !notice.acknowledged
                && (target.is_some_and(|target| &notice.event.target == target)
                    || source_session
                        .is_some_and(|session| notice.event.source_session() == Some(session)))
        })
        .map(|notice| notice.id)
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    pub window: String,
    #[serde(default)]
    pub application: Application,
    #[serde(default)]
    pub focused_target: Option<Target>,
    pub sessions: Vec<TerminalId>,
    pub focused: Option<TerminalId>,
}

impl Presence {
    pub fn destination(&self) -> Destination {
        Destination {
            application: self.application,
            window: self.window.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Enabled,
    Success,
    Failure,
    Waiting,
    WhileFocused,
    ReviewRequests,
    MessagePreviews,
}

impl Preferences {
    pub fn get(self, field: Field) -> bool {
        match field {
            Field::Enabled => self.enabled,
            Field::Success => self.success,
            Field::Failure => self.failure,
            Field::Waiting => self.waiting,
            Field::WhileFocused => self.while_focused,
            Field::ReviewRequests => self.review_requests,
            Field::MessagePreviews => self.message_previews,
        }
    }
    pub fn set(&mut self, field: Field, value: bool) {
        *match field {
            Field::Enabled => &mut self.enabled,
            Field::Success => &mut self.success,
            Field::Failure => &mut self.failure,
            Field::Waiting => &mut self.waiting,
            Field::WhileFocused => &mut self.while_focused,
            Field::ReviewRequests => &mut self.review_requests,
            Field::MessagePreviews => &mut self.message_previews,
        } = value;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
                Kind::Waiting
                | Kind::Interrupted
                | Kind::MessageReceived
                | Kind::AutomationPaused
                | Kind::AutomationBlocked
                | Kind::ProposalCreated => self.waiting,
                Kind::ReviewRequested => self.review_requests,
            }
    }
}

/// Private metadata-only context. Rich excerpts are a separate explicit opt-in.
pub fn context(event: &Event) -> String {
    if matches!(
        event.target,
        Target::Automation { .. } | Target::AutomationProposal { .. }
    ) {
        return String::new(); // User-authored automation context belongs only in opt-in Content.
    }
    if matches!(event.target, Target::SessionInbox(_)) {
        return "Open the terminal and run cagents inbox to read messages.".into();
    }
    if let Target::PullRequest(request) = &event.target {
        return format!(
            "{}/{} #{}",
            request.owner, request.repository, request.number
        );
    }
    let directory = event
        .directory
        .as_ref()
        .and_then(|directory| directory.file_name())
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
    fn optional_context_keeps_legacy_journal_records_readable() {
        let mut event = Event::session(
            TerminalId::new(),
            Provider::Claude,
            "/fixture".into(),
            Evidence {
                key: "accepted-turn".into(),
                kind: Kind::Waiting,
            },
        );
        let mut legacy = serde_json::to_value(&event).unwrap();
        legacy.as_object_mut().unwrap().remove("content");
        assert_eq!(Event::from_journal(&legacy.to_string()).unwrap(), event);
        event.content = Some(Content {
            conversation: "Fixture conversation".into(),
            preview: "Question for the operator".into(),
            request: Some(RequestType::Question),
        });
        let bytes = postcard::to_stdvec(&event).unwrap();
        assert_eq!(postcard::from_bytes::<Event>(&bytes).unwrap(), event);
    }

    #[test]
    fn inbox_attention_is_distinct_fixed_and_compatible_with_waiting_preferences() {
        let session = TerminalId::new();
        let event = Event {
            content: None,
            target: Target::SessionInbox(crate::agent_execution::Lineage(session)),
            source_session: Some(session),
            provider: None,
            directory: None,
            destination: None,
            evidence: Evidence {
                key: crate::model::MessageId::new().to_string(),
                kind: Kind::MessageReceived,
            },
        };
        event.validate().unwrap();
        assert_ne!(
            event.target.storage_key(),
            Target::Session(session).storage_key()
        );
        assert_eq!(
            context(&event),
            "Open the terminal and run cagents inbox to read messages."
        );
        assert_eq!(
            serde_json::from_str::<Event>(&serde_json::to_string(&event).unwrap()).unwrap(),
            event
        );
        let mut unsafe_event = event.clone();
        unsafe_event.directory = Some("/private/message-content".into());
        assert!(unsafe_event.validate().is_err());
        let mut preferences = Preferences::default();
        preferences.enabled = true;
        preferences.waiting = false;
        assert!(!preferences.allows(Kind::MessageReceived, false));
    }

    #[test]
    fn legacy_session_journal_and_typed_wire_have_one_target() {
        let session = TerminalId::new();
        let legacy = serde_json::json!({"session":session,"provider":Provider::Claude,"directory":"/project","evidence":{"key":"turn","kind":"Succeeded"}});
        let event = Event::from_journal(&legacy.to_string()).unwrap();
        assert_eq!(event.target(), Target::Session(session));
        assert_eq!(event.source_session(), Some(session));
        let event = Event {
            content: None,
            target: Target::AgentRun(RunId::new()),
            source_session: None,
            provider: None,
            directory: Some("/project".into()),
            destination: None,
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
    fn typed_focus_acknowledges_only_read_requests_and_preserves_opted_in_fresh_alerts() {
        let target = Target::PullRequest(review::ReviewRequest {
            host: "github.com".into(),
            port: None,
            owner: "o".into(),
            repository: "r".into(),
            number: 51,
            viewer: "reader".into(),
            request: "current".into(),
        });
        let id = uuid::Uuid::new_v4();
        let snapshot = Snapshot {
            notices: vec![Notice {
                id,
                event: Event {
                    content: None,
                    target: target.clone(),
                    source_session: None,
                    provider: None,
                    directory: None,
                    destination: Some(Destination {
                        application: Application::Ceditor,
                        window: "editor".into(),
                    }),
                    evidence: Evidence {
                        key: "current".into(),
                        kind: Kind::ReviewRequested,
                    },
                },
                delivered: true,
                acknowledged: false,
                window: None,
            }],
            diagnostic: None,
        };
        assert_eq!(
            focused_notice_ids(&snapshot, Some(&target), None, true, true),
            [id]
        );
        assert!(focused_notice_ids(&snapshot, Some(&target), None, false, true).is_empty());
        assert_eq!(
            focused_notice_ids(&snapshot, Some(&target), None, false, false),
            [id]
        );
        let mut different = target.clone();
        let Target::PullRequest(request) = &mut different else {
            unreachable!()
        };
        request.request = "new-request".into();
        assert!(focused_notice_ids(&snapshot, Some(&different), None, true, false).is_empty());
        assert!(focused_notice_ids(&snapshot, None, None, true, false).is_empty());
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
            content: None,
            target: Target::Session(TerminalId::new()),
            source_session: None,
            provider: Some(Provider::Codex),
            directory: Some(PathBuf::from(format!(
                "/private/account/{}\n",
                "x".repeat(200)
            ))),
            destination: None,
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

    fn automation_event(id: uuid::Uuid, sequence: u64, kind: Kind) -> Event {
        Event {
            target: Target::Automation { id, sequence },
            source_session: None,
            provider: None,
            directory: None,
            destination: None,
            evidence: Evidence {
                key: format!("automation:{id}:lifecycle:{sequence}"),
                kind,
            },
            content: Some(Content {
                conversation: "PRIVATE_AUTOMATION".into(),
                preview: "Project closed".into(),
                request: None,
            }),
        }
    }

    #[test]
    fn automation_attention_requires_exact_lifecycle_identity_and_no_actor_authority() {
        let id = uuid::Uuid::new_v4();
        for kind in [Kind::AutomationPaused, Kind::AutomationBlocked] {
            let valid = automation_event(id, 17, kind);
            valid.validate().unwrap();
            for sequence in [0, i64::MAX as u64 + 1, u64::MAX] {
                assert!(automation_event(id, sequence, kind).validate().is_err());
            }
            assert!(
                automation_event(uuid::Uuid::nil(), 17, kind)
                    .validate()
                    .is_err()
            );
            automation_event(id, i64::MAX as u64, kind)
                .validate()
                .unwrap();
            for evidence in [
                "automation:another:lifecycle:17".to_owned(),
                format!("automation:{id}:lifecycle:18"),
                format!("automation:{id}:lifecycle:017"),
            ] {
                let mut invalid = valid.clone();
                invalid.evidence.key = evidence;
                assert!(invalid.validate().is_err());
            }
            let mut invalids = Vec::new();
            let mut event = valid.clone();
            event.source_session = Some(TerminalId::new());
            invalids.push(event);
            let mut event = valid.clone();
            event.provider = Some(Provider::Codex);
            invalids.push(event);
            let mut event = valid.clone();
            event.directory = Some("/private/project".into());
            invalids.push(event);
            let mut event = valid.clone();
            event.destination = Some(Destination {
                application: Application::Cagents,
                window: crate::instance::PRIMARY_WINDOW.into(),
            });
            invalids.push(event);
            let mut event = valid.clone();
            event.content.as_mut().unwrap().request = Some(RequestType::Question);
            invalids.push(event);
            let mut event = valid.clone();
            event.content.as_mut().unwrap().conversation =
                "x".repeat(presentation::TITLE_LIMIT + 1);
            invalids.push(event);
            let mut event = valid.clone();
            event.content.as_mut().unwrap().preview = "private\ncontrol".into();
            invalids.push(event);
            let mut event = valid.clone();
            event.target = Target::AgentRun(RunId::new());
            invalids.push(event);
            for invalid in invalids {
                assert!(invalid.validate().is_err(), "{invalid:?}");
            }
            for other in [
                Kind::Succeeded,
                Kind::Failed,
                Kind::Waiting,
                Kind::ReviewRequested,
                Kind::MessageReceived,
            ] {
                let mut invalid = valid.clone();
                invalid.evidence.kind = other;
                assert!(invalid.validate().is_err());
            }
            // Uses the ordinary shared codec: no internally-tagged JSON-only wire type.
            assert_eq!(
                postcard::from_bytes::<Event>(&postcard::to_stdvec(&valid).unwrap()).unwrap(),
                valid
            );
            assert_eq!(
                Event::from_journal(&serde_json::to_string(&valid).unwrap()).unwrap(),
                valid
            );
        }
    }

    #[test]
    fn fresh_automation_sequence_keeps_its_own_focus_ack_and_waiting_preference() {
        let id = uuid::Uuid::new_v4();
        let old = automation_event(id, 17, Kind::AutomationPaused);
        let current = automation_event(id, 18, Kind::AutomationBlocked);
        assert_ne!(old.target, current.target);
        assert_eq!(old.target.storage_key(), current.target.storage_key());
        assert_eq!(
            [old.target.clone(), current.target.clone()]
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            2
        );
        let make = |event| Notice {
            id: uuid::Uuid::new_v4(),
            event,
            delivered: false,
            acknowledged: false,
            window: None,
        };
        let old_notice = make(old.clone());
        let current_notice = make(current.clone());
        let snapshot = Snapshot {
            notices: vec![old_notice.clone(), current_notice.clone()],
            diagnostic: None,
        };
        assert_eq!(
            focused_notice_ids(&snapshot, Some(&old.target), None, true, false),
            vec![old_notice.id]
        );
        assert_eq!(
            focused_notice_ids(&snapshot, Some(&current.target), None, true, false),
            vec![current_notice.id]
        );
        // GUI Update is not a wire type. Exercise the actual unsolicited
        // activation carrier shared by the matched CLI, daemon and UI.
        let activation = crate::terminal_ipc::ServerEnvelope {
            version: crate::terminal_ipc::PROTOCOL_VERSION,
            request_id: None,
            proof: crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("automation-codec-fixture"),
                boot_id: crate::model::BootId::new("automation-codec-fixture"),
                host_incarnation: crate::model::HostIncarnation::new("automation-codec-fixture"),
            },
            message: crate::terminal_ipc::ServerMessage::NotificationActivated {
                target: current.target.clone(),
            },
        };
        assert!(activation.proof.is_complete());
        assert_eq!(
            postcard::from_bytes::<crate::terminal_ipc::ServerEnvelope>(
                &postcard::to_stdvec(&activation).unwrap()
            )
            .unwrap(),
            activation
        );
        assert_eq!(
            postcard::from_bytes::<Snapshot>(&postcard::to_stdvec(&snapshot).unwrap()).unwrap(),
            snapshot
        );
        let mut policy = Preferences {
            success: false,
            failure: false,
            ..Default::default()
        };
        for kind in [Kind::AutomationPaused, Kind::AutomationBlocked] {
            assert!(policy.allows(kind, false));
            assert!(!policy.allows(kind, true));
        }
        policy.waiting = false;
        assert!(!policy.allows(Kind::AutomationPaused, false));
        assert!(!policy.allows(Kind::AutomationBlocked, false));
        assert!(
            context(&current).is_empty(),
            "private context never falls outside Content"
        );
    }
}
