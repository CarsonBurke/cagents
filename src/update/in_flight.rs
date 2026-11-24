//! What each conversation had under way when an update began, and what became
//! of it.
//!
//! The census records it from the state the source daemon reports for each
//! conversation, which it keeps from its provider's own protocol: whether a
//! turn runs or awaits the person, the subagents that outlive it, the steps
//! that still run after it ended, such as a background command awaiting its
//! task notification, and messages the provider has yet to take. The update's
//! record then says, per conversation, whether that work lived through it: a
//! retained provider process keeps it, while a provider that was stopped and
//! started again loses it. Only a conversation that lost work has anything to
//! resume; one whose turn simply finished had nothing in flight.
//!
//! Both records are read by builds older and newer than the one that wrote
//! them, so every field defaults and every enum names what it doesn't know.
use crate::{
    agent::Provider,
    chat::{ItemContent, Snapshot, ToolStatus},
    model::TerminalId,
};
use serde::{Deserialize, Serialize};

/// Tasks kept for one conversation; the rest are only counted.
pub(crate) const MAX_TASKS: usize = 32;
/// Queued messages kept for one conversation; the rest are only counted.
pub(crate) const MAX_QUEUED: usize = 32;
/// Bytes of a provider-issued identity kept; a longer one is not an identity.
const MAX_ID_BYTES: usize = 256;

/// One conversation's work in flight when the update began.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlight {
    /// The conversation, as the census and its panes know it.
    pub id: TerminalId,
    /// Its turn was running or awaiting the person.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub turn: bool,
    /// Work running apart from the turn, with the identity its provider
    /// reported, at most [`MAX_TASKS`] of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<Task>,
    /// Messages its provider held without having taken them yet, by the
    /// provider's identity, at most [`MAX_QUEUED`] of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued: Vec<String>,
    /// Tasks and queued messages beyond those listed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unlisted: u32,
}

fn is_zero(count: &u32) -> bool {
    *count == 0
}

/// Work a conversation's provider runs apart from its turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// The provider's identity for it: the call that started it, or its
    /// child thread.
    pub id: String,
    #[serde(default)]
    pub kind: TaskKind,
    /// What a person knows it by, when the provider said.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// It finished its own turn and waits on background work it started.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waiting: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// A subagent or child thread.
    Subagent,
    /// A shell command, such as one moved to the background.
    Command,
    /// Any other step still running, such as a monitor.
    Step,
    #[default]
    #[serde(other)]
    Unknown,
}

impl InFlight {
    /// What the source reported `id` doing, from the snapshot it attached
    /// with. A provider that has exited has nothing in flight.
    ///
    /// While the turn runs, its own steps and subagents belong to it, so only
    /// the subagents that outlive it are listed. Once a Claude turn has
    /// ended, a step its adapter still reports running outlived it too: a
    /// command moved to the background, or a monitor, stays running until its
    /// task notification arrives. No other provider's adapter keeps a step
    /// running past its turn on purpose, so a step one left running is not
    /// read as work.
    pub fn observe(id: TerminalId, provider: Provider, snapshot: &Snapshot) -> Self {
        let mut work = Self {
            id,
            ..Self::default()
        };
        if snapshot.exited || snapshot.read_only_reason.is_some() {
            return work;
        }
        work.turn = snapshot.turn_accepted();
        for agent in snapshot
            .subagents
            .iter()
            .filter(|agent| agent.outlives_turn())
        {
            work.task(Task {
                id: agent.id.clone(),
                kind: TaskKind::Subagent,
                name: crate::chat::short_name([agent.kind.as_str(), agent.description.as_str()])
                    .unwrap_or_default(),
                waiting: agent.status == ToolStatus::Waiting,
            });
        }
        if !work.turn && provider == Provider::Claude {
            for item in &snapshot.items {
                let (kind, name) = match &item.content {
                    ItemContent::Command {
                        status: ToolStatus::Running,
                        label,
                        command,
                        ..
                    } => (TaskKind::Command, [label.as_str(), command.as_str()]),
                    ItemContent::Tool {
                        status: ToolStatus::Running,
                        name,
                        input,
                        ..
                    } => (TaskKind::Step, [name.as_str(), input.as_str()]),
                    _ => continue,
                };
                work.task(Task {
                    id: item.id.clone(),
                    kind,
                    name: crate::chat::short_name(name).unwrap_or_default(),
                    waiting: false,
                });
            }
        }
        for message in &snapshot.queued_messages {
            if message.id.len() > MAX_ID_BYTES {
                work.unlisted = work.unlisted.saturating_add(1);
            } else if work.queued.len() < MAX_QUEUED {
                work.queued.push(message.id.clone());
            } else {
                work.unlisted = work.unlisted.saturating_add(1);
            }
        }
        work
    }

    /// The update interrupted this conversation's turn, and with it these
    /// background tasks. Its turn has already stopped when the census reads
    /// it, so the interruption is what says it was running.
    pub(crate) fn interrupted<'a>(&mut self, tasks: impl IntoIterator<Item = &'a String>) {
        self.turn = true;
        for id in tasks {
            if !self.tasks.iter().any(|task| task.id == *id) {
                self.task(Task {
                    id: id.clone(),
                    kind: TaskKind::Subagent,
                    name: String::new(),
                    waiting: false,
                });
            }
        }
    }

    fn task(&mut self, task: Task) {
        if self.tasks.iter().any(|known| known.id == task.id) {
            return;
        }
        if task.id.len() > MAX_ID_BYTES || self.tasks.len() >= MAX_TASKS {
            self.unlisted = self.unlisted.saturating_add(1);
        } else {
            self.tasks.push(task);
        }
    }

    /// Whether anything was in flight.
    pub fn is_empty(&self) -> bool {
        !self.turn && self.tasks.is_empty() && self.queued.is_empty() && self.unlisted == 0
    }
}

/// What became of one conversation's work in flight through the update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    #[serde(flatten)]
    pub work: InFlight,
    /// What the update review called the conversation.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub fate: Fate,
}

impl Outcome {
    /// Whether its work is gone and the person may want to resume it.
    pub fn lost(&self) -> bool {
        self.fate.lost()
    }
}

/// What became of a conversation's provider, and so of its work in flight.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fate {
    /// Its provider process was kept and reattached, work and all.
    Continued,
    /// Its provider process was stopped for the update and started again on
    /// its saved conversation, without the work it had under way.
    Restarted,
    /// The update interrupted its turn, with consent, before stopping it.
    Interrupted,
    /// The update ended the conversation; its saved history remains.
    Ended,
    /// Its provider was stopped for the update and could not be started
    /// again, so nothing of it runs.
    Stopped,
    /// Its provider was kept but could not be reattached; it was left as it
    /// was, so its work may still run outside cagents.
    Detached,
    /// The update's record cannot say.
    #[default]
    #[serde(other)]
    Unknown,
}

impl Fate {
    pub fn lost(self) -> bool {
        matches!(
            self,
            Self::Restarted | Self::Interrupted | Self::Ended | Self::Stopped
        )
    }
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;
    use crate::chat::{CommandAction, Item, QueuedMessage, Role, Subagent};
    use std::sync::Arc;

    fn agent(id: &str, status: ToolStatus, background: bool) -> Subagent {
        Subagent {
            id: id.into(),
            kind: "Reviewer".into(),
            description: format!("Review {id}\nwith details"),
            status,
            model: None,
            waiting: false,
            detail: String::new(),
            background,
            items: Vec::new(),
        }
    }

    fn command(id: &str, status: ToolStatus) -> Arc<Item> {
        Arc::new(Item {
            started_at: None,
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Command {
                label: String::new(),
                command: "cargo test --workspace".into(),
                working_directory: "/repo".into(),
                action: CommandAction::Run,
                status,
                exit_code: None,
                duration_ms: None,
                output: String::new(),
            },
            at: None,
        })
    }

    fn monitor(id: &str) -> Arc<Item> {
        Arc::new(Item {
            started_at: None,
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Tool {
                name: "Monitor".into(),
                input: "CI on #12".into(),
                output: String::new(),
                status: ToolStatus::Running,
            },
            at: None,
        })
    }

    fn queued(id: &str) -> QueuedMessage {
        QueuedMessage {
            id: id.into(),
            text: "and then push".into(),
            editable: true,
            delivery: Default::default(),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn a_finished_turn_with_nothing_outstanding_has_nothing_in_flight() {
        let snapshot = Snapshot {
            items: vec![command("done", ToolStatus::Succeeded)],
            subagents: vec![agent("finished", ToolStatus::Succeeded, true)],
            ..Snapshot::default()
        };
        let work = InFlight::observe(TerminalId::new(), Provider::Claude, &snapshot);
        assert!(work.is_empty(), "{work:?}");
    }

    #[test]
    fn an_ended_turn_keeps_the_background_work_its_provider_still_reports() {
        let id = TerminalId::new();
        let snapshot = Snapshot {
            items: vec![
                command("toolu_build", ToolStatus::Running),
                command("toolu_done", ToolStatus::Succeeded),
                monitor("toolu_monitor"),
            ],
            subagents: vec![
                agent("toolu_worker", ToolStatus::Running, true),
                agent("toolu_waiting", ToolStatus::Waiting, false),
                agent("toolu_finished", ToolStatus::Succeeded, true),
            ],
            queued_messages: vec![queued("q1")],
            ..Snapshot::default()
        };
        let work = InFlight::observe(id, Provider::Claude, &snapshot);
        assert_eq!(work.id, id);
        assert!(!work.turn);
        let tasks: Vec<_> = work
            .tasks
            .iter()
            .map(|task| {
                (
                    task.id.as_str(),
                    task.kind,
                    task.name.as_str(),
                    task.waiting,
                )
            })
            .collect();
        assert_eq!(
            tasks,
            [
                ("toolu_worker", TaskKind::Subagent, "Reviewer", false),
                ("toolu_waiting", TaskKind::Subagent, "Reviewer", true),
                (
                    "toolu_build",
                    TaskKind::Command,
                    "cargo test --workspace",
                    false
                ),
                ("toolu_monitor", TaskKind::Step, "Monitor", false),
            ]
        );
        assert_eq!(work.queued, ["q1"]);
    }

    #[test]
    fn a_running_turn_lists_only_the_work_that_outlives_it() {
        let snapshot = Snapshot {
            busy: true,
            items: vec![command("toolu_foreground", ToolStatus::Running)],
            subagents: vec![
                agent("toolu_foreground_agent", ToolStatus::Running, false),
                agent("toolu_background_agent", ToolStatus::Running, true),
            ],
            ..Snapshot::default()
        };
        let work = InFlight::observe(TerminalId::new(), Provider::Claude, &snapshot);
        assert!(work.turn);
        let ids: Vec<_> = work.tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids, ["toolu_background_agent"]);

        let awaiting = Snapshot {
            waiting: true,
            ..Snapshot::default()
        };
        assert!(InFlight::observe(TerminalId::new(), Provider::Claude, &awaiting).turn);
    }

    #[test]
    fn only_claude_keeps_a_step_running_past_its_turn() {
        let snapshot = Snapshot {
            items: vec![command("item_left_running", ToolStatus::Running)],
            subagents: vec![agent("child-thread", ToolStatus::Running, true)],
            ..Snapshot::default()
        };
        let work = InFlight::observe(TerminalId::new(), Provider::Codex, &snapshot);
        let ids: Vec<_> = work.tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids, ["child-thread"]);
    }

    #[test]
    fn an_exited_or_read_only_provider_has_nothing_in_flight() {
        for snapshot in [
            Snapshot {
                exited: true,
                busy: true,
                items: vec![command("toolu_build", ToolStatus::Running)],
                ..Snapshot::default()
            },
            Snapshot {
                read_only_reason: Some("Saved history".into()),
                items: vec![command("toolu_build", ToolStatus::Running)],
                ..Snapshot::default()
            },
        ] {
            assert!(InFlight::observe(TerminalId::new(), Provider::Claude, &snapshot).is_empty());
        }
    }

    #[test]
    fn an_interruption_says_the_turn_ran_and_names_the_tasks_it_stopped() {
        let snapshot = Snapshot {
            subagents: vec![agent("toolu_worker", ToolStatus::Stopped, true)],
            ..Snapshot::default()
        };
        let mut work = InFlight::observe(TerminalId::new(), Provider::Claude, &snapshot);
        assert!(work.is_empty());
        work.interrupted(&["toolu_worker".to_owned()]);
        assert!(work.turn);
        assert_eq!(work.tasks.len(), 1);
        assert_eq!(work.tasks[0].id, "toolu_worker");
        assert_eq!(work.tasks[0].kind, TaskKind::Subagent);
    }

    #[test]
    fn listed_tasks_and_messages_are_bounded_and_the_rest_counted() {
        let snapshot = Snapshot {
            items: (0..MAX_TASKS + 3)
                .map(|index| command(&format!("toolu_{index}"), ToolStatus::Running))
                .chain([command(&"x".repeat(MAX_ID_BYTES + 1), ToolStatus::Running)])
                .collect(),
            queued_messages: (0..MAX_QUEUED + 2)
                .map(|index| queued(&format!("q{index}")))
                .collect(),
            ..Snapshot::default()
        };
        let work = InFlight::observe(TerminalId::new(), Provider::Claude, &snapshot);
        assert_eq!(work.tasks.len(), MAX_TASKS);
        assert_eq!(work.queued.len(), MAX_QUEUED);
        assert_eq!(work.unlisted, 3 + 1 + 2);
    }

    #[test]
    fn records_read_across_versions() {
        // A record from before these fields, or naming kinds and fates this
        // build does not know, still reads.
        let id = TerminalId::new();
        let older: Outcome = serde_json::from_value(serde_json::json!({ "id": id })).unwrap();
        assert_eq!(older.work.id, id);
        assert!(older.work.is_empty());
        assert_eq!(older.fate, Fate::Unknown);
        assert!(!older.lost());
        let newer: Outcome = serde_json::from_value(serde_json::json!({
            "id": id,
            "turn": true,
            "tasks": [{"id": "t", "kind": "future_kind", "later": 1}],
            "fate": "future_fate",
            "label": "Fix the build",
            "later": {"field": true},
        }))
        .unwrap();
        assert_eq!(newer.work.tasks[0].kind, TaskKind::Unknown);
        assert_eq!(newer.fate, Fate::Unknown);
        assert_eq!(newer.label, "Fix the build");

        // Nothing absent is written, so an empty record is just its identity.
        let empty = InFlight {
            id,
            ..InFlight::default()
        };
        assert_eq!(
            serde_json::to_value(&empty).unwrap(),
            serde_json::json!({ "id": id })
        );
        let round_trip = Outcome {
            work: InFlight {
                id,
                turn: true,
                tasks: vec![Task {
                    id: "toolu".into(),
                    kind: TaskKind::Command,
                    name: "cargo test".into(),
                    waiting: false,
                }],
                queued: vec!["q".into()],
                unlisted: 2,
            },
            label: "Fix the build".into(),
            fate: Fate::Restarted,
        };
        let bytes = serde_json::to_vec(&round_trip).unwrap();
        assert_eq!(
            serde_json::from_slice::<Outcome>(&bytes).unwrap(),
            round_trip
        );
    }

    #[test]
    fn only_work_that_was_stopped_is_lost() {
        assert!(
            [
                Fate::Restarted,
                Fate::Interrupted,
                Fate::Ended,
                Fate::Stopped
            ]
            .into_iter()
            .all(Fate::lost)
        );
        assert!(
            [Fate::Continued, Fate::Detached, Fate::Unknown]
                .into_iter()
                .all(|fate| !fate.lost())
        );
    }
}
