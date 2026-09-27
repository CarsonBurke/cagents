//! Durable recurring work. Scheduling records refer to the shared execution
//! service's agents and runs; they never imply that a prompt was sent twice.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    agent_execution,
    model::{AgentId, ChannelId, RunId},
};

pub const MAX_NAME_BYTES: usize = 160;
pub const MAX_PRECHECK_BYTES: usize = 16 * 1024;
pub const MAX_HISTORY_PAGE: usize = 100;
pub const DEFAULT_PRECHECK_SECONDS: u32 = 30;
pub const MAX_PRECHECK_SECONDS: u32 = 300;
pub const MISFIRE_GRACE_MS: u64 = 15 * 60 * 1000;

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
        if self.agent.target.channel.is_some() {
            return Err(
                "Automations create their own channel; they cannot take over an existing channel"
                    .into(),
            );
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
pub struct Automation {
    pub id: Uuid,
    /// Optimistic edit version, including presentation-only renames.
    pub revision: u64,
    /// The schedule/execution version named by immutable firing claims.
    pub execution_revision: u64,
    pub definition: Definition,
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
    DefinitionChanged,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Firing {
    pub id: Uuid,
    pub automation: Uuid,
    pub revision: u64,
    pub trigger: Trigger,
    pub state: FiringState,
    pub created_ms: u64,
    pub started_ms: Option<u64>,
    pub finished_ms: Option<u64>,
    pub agent: Option<AgentId>,
    pub run: Option<RunId>,
    pub skip: Option<SkipReason>,
    /// Bounded process/validation diagnostic, never a duplicate transcript.
    pub diagnostic: Option<String>,
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
