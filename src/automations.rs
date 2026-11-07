//! Durable recurring work. Scheduling records refer to the shared execution
//! service's agents and runs; they never imply that a prompt was sent twice.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    agent_execution,
    model::{AgentId, ChannelId, RunId},
};

#[cfg(any(feature = "host", feature = "ui"))]
pub mod schedule;

pub const MAX_NAME_BYTES: usize = 160;
pub const MAX_PRECHECK_BYTES: usize = 16 * 1024;
pub const MAX_HISTORY_PAGE: usize = 100;
pub const MAX_LIFECYCLE_EVENTS: usize = 256;
pub const MAX_CONCURRENT_PRECHECKS: usize = 4;
pub const DEFAULT_PRECHECK_SECONDS: u32 = 30;
pub const MAX_PRECHECK_SECONDS: u32 = 300;
pub const MISFIRE_GRACE_MS: u64 = 15 * 60 * 1000;

/// Active schedule management requires user authority. Hosted conversations
/// may inspect their own project and prepare immutable requests for approval.
/// Revisions fence stale editors and confirmations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    List {
        after: Option<Uuid>,
        limit: usize,
    },
    Show {
        id: Uuid,
    },
    Create {
        definition: Definition,
    },
    Edit {
        id: Uuid,
        revision: u64,
        definition: Definition,
    },
    SetEnabled {
        id: Uuid,
        revision: u64,
        enabled: bool,
        confirmed: bool,
    },
    Remove {
        id: Uuid,
        revision: u64,
    },
    RunNow {
        id: Uuid,
        execution_revision: u64,
        request: Uuid,
        confirmed: bool,
    },
    Cancel {
        id: Uuid,
        firing: Uuid,
    },
    History {
        /// None includes retained runs from removed definitions.
        id: Option<Uuid>,
        before: Option<HistoryCursor>,
        limit: usize,
    },
    ShowFiring {
        id: Uuid,
    },
    ShowRun {
        run: RunId,
    },
    /// Prepare an immutable request for user approval. This confers no
    /// authority to edit an existing schedule or execute unattended work.
    Propose {
        request: Uuid,
        definition: Definition,
    },
    Proposal {
        id: Uuid,
    },
    PendingProposals {
        repository: std::path::PathBuf,
        after: Option<Uuid>,
        limit: usize,
    },
    ResolveProposal {
        id: Uuid,
        revision: u64,
        approved: bool,
    },
    /// Read bounded lifecycle receipts, independently of provider outcomes.
    Audit {
        id: Uuid,
        before: Option<u64>,
        limit: usize,
    },
}

impl Request {
    pub fn allowed_from_hosted_conversation(&self) -> bool {
        matches!(
            self,
            Self::List { .. }
                | Self::Show { .. }
                | Self::History { .. }
                | Self::Audit { .. }
                | Self::ShowFiring { .. }
                | Self::ShowRun { .. }
                | Self::Propose { .. }
                | Self::Proposal { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Listed(Vec<Summary>),
    Automation(Automation),
    Firing(Firing),
    History(HistoryPage),
    Detail(RunDetail),
    Removed,
    CancellationRequested,
    Proposal(Box<Proposal>),
    Proposals(Vec<ProposalSummary>),
    Audit(LifecyclePage),
}

/// Actor classes describe the admitted operation, never a guessed person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleActor {
    UserManagement,
    System,
}

impl LifecycleActor {
    pub fn label(self) -> &'static str {
        match self {
            Self::UserManagement => "User management",
            Self::System => "System",
        }
    }
}

/// Fixed causes exclude prompt bodies, launch environment and provider diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleCause {
    Created,
    Edited,
    Enabled,
    Disabled,
    Removed,
    ProjectClosed,
    StartPaused,
    PrecheckInterrupted,
    PrecheckRecovered,
    PrecheckPaused,
    ProjectUnavailable,
    ReleaseBlocked,
    RunInterrupted,
    ReleaseRecovered,
}

impl LifecycleCause {
    pub fn actor(self) -> LifecycleActor {
        match self {
            Self::Created | Self::Edited | Self::Enabled | Self::Disabled | Self::Removed => {
                LifecycleActor::UserManagement
            }
            Self::ProjectClosed
            | Self::StartPaused
            | Self::PrecheckInterrupted
            | Self::PrecheckRecovered
            | Self::PrecheckPaused
            | Self::ProjectUnavailable
            | Self::ReleaseBlocked
            | Self::RunInterrupted
            | Self::ReleaseRecovered => LifecycleActor::System,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "Created",
            Self::Edited => "Definition edited",
            Self::Enabled => "Enabled",
            Self::Disabled => "Disabled",
            Self::Removed => "Removed",
            Self::ProjectClosed => "Project closed",
            Self::StartPaused => "Paused before launch",
            Self::PrecheckInterrupted => "Precheck interrupted",
            Self::PrecheckRecovered => "Precheck cleanup recovered",
            Self::PrecheckPaused => "Paused after precheck",
            Self::ProjectUnavailable => "Paused because project is unavailable",
            Self::ReleaseBlocked => "Session cleanup blocked",
            Self::RunInterrupted => "Run interrupted",
            Self::ReleaseRecovered => "Session cleanup recovered",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleEvent {
    pub sequence: u64,
    pub automation: Uuid,
    pub revision: u64,
    pub occurred_ms: u64,
    pub cause: LifecycleCause,
    pub actor: LifecycleActor,
    pub enabled: bool,
    pub blocked: bool,
    pub firing: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecyclePage {
    pub events: Vec<LifecycleEvent>,
    /// Continue before this monotonic receipt sequence, independent of wall time.
    pub next: Option<u64>,
    /// Earlier events were never recorded or have left the bounded retention.
    /// Their actor and cause cannot be reconstructed from current state.
    pub earlier_events_unavailable: bool,
}

/// A saved execution configuration, separate from active scheduler records.
/// Its request ID is immutable: changed settings require another proposal,
/// never reuse the approval of the previous configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub id: Uuid,
    pub revision: u64,
    pub owner: agent_execution::Origin,
    pub requested: Definition,
    pub definition: Definition,
    pub target_identity: DirectoryIdentity,
    pub channel_title: Option<String>,
    pub created_ms: u64,
    pub state: ProposalState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalState {
    Pending,
    Approved {
        automation: Uuid,
        execution_revision: u64,
    },
    Declined,
}

/// Pending lists do not load or transfer every saved prompt and precheck.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalSummary {
    pub id: Uuid,
    pub revision: u64,
    pub repository: std::path::PathBuf,
    pub name: String,
}

impl From<&Proposal> for ProposalSummary {
    fn from(proposal: &Proposal) -> Self {
        Self {
            id: proposal.id,
            revision: proposal.revision,
            repository: proposal.definition.agent.target.repository.clone(),
            name: proposal.definition.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDetail {
    pub definition: Definition,
    pub firing: Firing,
    pub agent: Option<agent_execution::Agent>,
    pub run: Option<agent_execution::Run>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    #[default]
    Fresh,
    Reuse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulePattern {
    Hourly {
        minute: u8,
    },
    Daily {
        hour: u8,
        minute: u8,
    },
    Weekdays {
        hour: u8,
        minute: u8,
    },
    /// Monday is 1 and Sunday is 7, matching the editor's weekday order.
    Weekly {
        weekday: u8,
        hour: u8,
        minute: u8,
    },
    Cron {
        expression: String,
    },
}

impl SchedulePattern {
    pub fn expression(&self) -> Result<String, String> {
        let (hour, minute) = match self {
            Self::Hourly { minute } => (0, *minute),
            Self::Daily { hour, minute }
            | Self::Weekdays { hour, minute }
            | Self::Weekly { hour, minute, .. } => (*hour, *minute),
            Self::Cron { expression } => {
                if expression.len() > 256
                    || expression.split_whitespace().count() != 5
                    || expression.chars().any(|c| c.is_control())
                {
                    return Err("A cron schedule must contain five fields (minute, hour, day, month, weekday)".into());
                }
                return Ok(expression.split_whitespace().collect::<Vec<_>>().join(" "));
            }
        };
        if hour > 23 || minute > 59 {
            return Err("Schedule time must use hours 0–23 and minutes 0–59".into());
        }
        Ok(match self {
            Self::Hourly { .. } => format!("{minute} * * * *"),
            Self::Daily { .. } => format!("{minute} {hour} * * *"),
            Self::Weekdays { .. } => format!("{minute} {hour} * * 1-5"),
            Self::Weekly { weekday, .. } => {
                if !(1..=7).contains(weekday) {
                    return Err(
                        "Weekly schedules require a weekday from Monday (1) through Sunday (7)"
                            .into(),
                    );
                }
                format!("{minute} {hour} * * {}", weekday % 7)
            }
            Self::Cron { .. } => unreachable!(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub pattern: SchedulePattern,
    /// An explicit IANA name; never inherited from the daemon's environment.
    pub timezone: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Precheck {
    /// Runs once via /bin/sh in the explicit target directory. This is an
    /// authorized command, not a read-only predicate that may be retried.
    pub command: String,
    pub timeout_seconds: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Definition {
    pub name: String,
    pub prompt: String,
    pub agent: agent_execution::Spec,
    pub schedule: Schedule,
    pub session_mode: SessionMode,
    pub precheck: Option<Precheck>,
}

impl Definition {
    /// Cheap structural checks shared by clients. The daemon separately checks
    /// cron/timezone semantics, account authority and canonical target identity.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty()
            || self.name.len() > MAX_NAME_BYTES
            || self.name.chars().any(char::is_control)
        {
            return Err(format!(
                "An automation name must contain 1–{MAX_NAME_BYTES} bytes without control characters"
            ));
        }
        agent_execution::Submit {
            name: None,
            destination: agent_execution::Destination::New(self.agent.clone()),
            prompt: self.prompt.clone(),
            idempotency_key: "validate-automation".into(),
        }
        .validate()?;
        self.schedule.pattern.expression()?;
        if self.schedule.timezone.is_empty()
            || self.schedule.timezone.len() > 128
            || self.schedule.timezone.chars().any(char::is_whitespace)
            || self.schedule.timezone.chars().any(char::is_control)
        {
            return Err("Choose an explicit IANA timezone, such as America/Vancouver".into());
        }
        if let Some(precheck) = &self.precheck {
            if precheck.command.trim().is_empty()
                || precheck.command.len() > MAX_PRECHECK_BYTES
                || precheck.command.contains('\0')
            {
                return Err(format!(
                    "A precheck must contain 1–{MAX_PRECHECK_BYTES} bytes without NUL characters"
                ));
            }
            if !(1..=MAX_PRECHECK_SECONDS).contains(&precheck.timeout_seconds) {
                return Err(format!(
                    "Precheck timeout must be between 1 and {MAX_PRECHECK_SECONDS} seconds"
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    pub due_ms: u64,
    /// Civil minute without a UTC offset. A repeated DST minute therefore has
    /// one key even though it can correspond to two physical instants.
    pub local: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Automation {
    pub id: Uuid,
    /// Optimistic edit version, including presentation-only renames.
    pub revision: u64,
    /// The schedule/execution version named by immutable firing claims.
    pub execution_revision: u64,
    pub definition: Definition,
    /// Filesystem authority pinned by the daemon when saving the definition.
    pub target_identity: DirectoryIdentity,
    pub enabled: bool,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub next: Option<Occurrence>,
    pub active_firing: Option<Uuid>,
    pub reuse_agent: Option<AgentId>,
    pub channel: Option<ChannelId>,
    pub blocked: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Scheduled {
        occurrence: Occurrence,
    },
    /// Caller supplies a stable key so a CLI transport retry returns the same
    /// manual firing rather than creating another provider charge.
    Manual {
        request: Uuid,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FiringState {
    Prechecking,
    Accepted,
    Running,
    Waiting,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
    Interrupted,
}

impl FiringState {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Prechecking | Self::Accepted | Self::Running | Self::Waiting
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    PrecheckExit,
    MissedDeadline,
    Overlap,
    Capacity,
    DefinitionChanged,
    Disabled,
}

/// Actual consecutive scheduled skips, not an estimate of missed downtime slots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedOccurrences {
    pub count: u64,
    pub first_occurrence_ms: u64,
    pub last_occurrence_ms: u64,
}

/// Detailed settled history is bounded independently of unresolved ownership
/// and compact lifetime manual-request replay proofs.
pub const HISTORY_RETAINED_ROWS: usize = 500;
pub const REMOVED_HISTORY_RETENTION_DAYS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Firing {
    pub id: Uuid,
    pub automation: Uuid,
    /// Presentation at acceptance, independent of the execution revision.
    #[serde(default)]
    pub name: String,
    pub revision: u64,
    pub trigger: Trigger,
    pub state: FiringState,
    /// A terminal provider result still owns the automation until its session
    /// is proved closed (fresh) or idle at the exact source (reuse).
    pub release_pending: bool,
    /// An explicit precheck cancellation fences later provider acceptance even
    /// if the shell's successful receipt raced with the cancellation request.
    pub cancel_requested: bool,
    pub created_ms: u64,
    pub started_ms: Option<u64>,
    pub finished_ms: Option<u64>,
    pub agent: Option<AgentId>,
    pub run: Option<RunId>,
    pub skip: Option<SkipReason>,
    /// Bounded process/validation diagnostic, never a duplicate transcript.
    pub diagnostic: Option<String>,
    /// First identity/name remain stable; trigger and finish time describe the
    /// most recent exact occurrence. Legacy and unenumerated downtime rows use None.
    #[serde(default)]
    pub skipped_occurrences: Option<SkippedOccurrences>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_and_firing_identity_roundtrip_on_the_shared_wire() {
        let schedule = Schedule {
            pattern: SchedulePattern::Weekly {
                weekday: 2,
                hour: 9,
                minute: 30,
            },
            timezone: "America/Vancouver".into(),
        };
        let trigger = Trigger::Scheduled {
            occurrence: Occurrence {
                due_ms: 1_790_000_000_000,
                local: "2026-09-29T09:30".into(),
            },
        };
        let bytes = postcard::to_allocvec(&(schedule.clone(), trigger.clone())).unwrap();
        assert_eq!(
            postcard::from_bytes::<(Schedule, Trigger)>(&bytes).unwrap(),
            (schedule, trigger)
        );
    }

    #[test]
    fn lifecycle_audit_roundtrips_nonempty_receipts_and_cursor_on_shared_wire() {
        let id = Uuid::new_v4();
        let request = Request::Audit {
            id,
            before: Some(42),
            limit: 37,
        };
        assert_eq!(
            postcard::from_bytes::<Request>(&postcard::to_allocvec(&request).unwrap()).unwrap(),
            request
        );
        let response = Response::Audit(LifecyclePage {
            events: vec![LifecycleEvent {
                sequence: 41,
                automation: id,
                revision: 7,
                occurred_ms: 100,
                cause: LifecycleCause::ReleaseBlocked,
                actor: LifecycleActor::System,
                enabled: false,
                blocked: true,
                firing: Some(Uuid::new_v4()),
            }],
            next: Some(41),
            earlier_events_unavailable: true,
        });
        assert_eq!(
            postcard::from_bytes::<Response>(&postcard::to_allocvec(&response).unwrap()).unwrap(),
            response
        );
        assert_eq!(
            serde_json::from_value::<Response>(serde_json::to_value(&response).unwrap()).unwrap(),
            response
        );
    }

    #[test]
    fn presets_expand_to_minute_cron_with_explicit_weekday_order() {
        assert_eq!(
            SchedulePattern::Weekly {
                weekday: 7,
                hour: 8,
                minute: 30
            }
            .expression()
            .unwrap(),
            "30 8 * * 0"
        );
        assert_eq!(
            SchedulePattern::Weekdays { hour: 9, minute: 0 }
                .expression()
                .unwrap(),
            "0 9 * * 1-5"
        );
        assert!(SchedulePattern::Hourly { minute: 60 }.expression().is_err());
        assert!(
            SchedulePattern::Weekly {
                weekday: 0,
                hour: 0,
                minute: 0
            }
            .expression()
            .is_err()
        );
        assert!(
            SchedulePattern::Cron {
                expression: "0 * * * * *".into()
            }
            .expression()
            .is_err()
        );
    }
}

/// List rows intentionally omit prompts and shell commands. Listing hundreds
/// of automations must not deserialize every full execution definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub id: Uuid,
    pub revision: u64,
    pub name: String,
    pub provider: crate::agent::Provider,
    pub directory: std::path::PathBuf,
    pub enabled: bool,
    pub next: Option<Occurrence>,
    pub active_firing: Option<Uuid>,
    pub last_state: Option<FiringState>,
    pub blocked: Option<String>,
}

impl Automation {
    pub fn summary(&self, last_state: Option<FiringState>) -> Summary {
        Summary {
            id: self.id,
            revision: self.revision,
            name: self.definition.name.clone(),
            provider: self.definition.agent.provider,
            directory: self.definition.agent.target.directory.clone(),
            enabled: self.enabled,
            next: self.next.clone(),
            active_firing: self.active_firing,
            last_state,
            blocked: self.blocked.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCursor {
    pub created_ms: u64,
    pub id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage {
    pub firings: Vec<Firing>,
    pub next: Option<HistoryCursor>,
}

#[cfg(test)]
#[path = "automations/history_wire_tests.rs"]
mod history_wire_tests;
