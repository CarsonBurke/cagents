//! A read-only directory of the provider conversations the daemon hosts.
//!
//! Every human chat, provider CLI in a terminal, delegated agent and
//! automation appears here, whoever started it, so that sessions working on
//! related things can find each other. Listing grants no control: closing,
//! interrupting and answering remain restricted to an agent's owner.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::State;
use crate::{
    agent::{ActivityState, Provider},
    model::{AgentId, RunId, TerminalId},
};

/// Which sessions a directory read returns. The caller itself is always
/// reported separately, whatever the scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// Sessions in the project that contains this directory. A directory
    /// outside every open project widens to `All`, and the reply says so.
    Containing(PathBuf),
    /// Sessions in this open project, identified by its repository key, by
    /// any directory inside it, or, as a relative path of one component, by
    /// its name.
    Project(PathBuf),
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Surface {
    Chat,
    Terminal,
}

/// How a listed session relates to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Relation {
    Myself,
    /// The session that owns the caller's agent.
    Parent,
    /// An agent the caller owns and therefore may control.
    Child,
    Peer,
}

/// Who may control a delegated agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Owner {
    /// A CLI caller outside any cagents session.
    User,
    /// Every session of this lineage.
    Session(super::Lineage),
    Automation(uuid::Uuid),
}

impl From<&super::Origin> for Owner {
    fn from(origin: &super::Origin) -> Self {
        match origin {
            super::Origin::User => Self::User,
            super::Origin::Child { parent } => Self::Session(*parent),
            super::Origin::Automation { id } => Self::Automation(*id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegation {
    pub agent: AgentId,
    pub owner: Owner,
    pub run: Option<RunId>,
    pub run_state: Option<State>,
}

/// One coherent, resident-only provider publication. An absent publication is
/// unknown, including when an older retained helper cannot supply this field.
/// A missing active run never means the provider has no interactive/child work.
/// Tracker ownership records an outbound request, not provider execution ACK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionStatus {
    pub activity: ActivityState,
    pub generation: u64,
    pub turn_generation: u64,
    pub parent_busy: bool,
    pub active_run: Option<RunId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStatus {
    pub id: RunId,
    pub state: State,
    pub request_recorded: bool,
    /// Owner-only, bounded diagnostic. Never a result body or provider prompt.
    #[serde(default)]
    pub diagnostic: Option<String>,
}

/// Independent evidence, rather than a single badge synthesized from history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub observed: Option<ExecutionStatus>,
    pub latest_run: Option<RunStatus>,
    /// Only the caller/owner may see its actual mailbox queue. False proves no
    /// queued message, not that an instruction was consumed or executed.
    pub pending_message: Option<bool>,
}

impl Status {
    pub fn explanation(&self) -> Vec<String> {
        let mut lines = Vec::new();
        match &self.observed {
            Some(observed) => {
                let activity = match observed.activity {
                    ActivityState::Working => "working",
                    ActivityState::Waiting => "waiting",
                    ActivityState::Idle => "idle",
                    ActivityState::Unknown => "unknown",
                };
                lines.push(match observed.active_run {
                    Some(run)
                        if !observed.parent_busy && observed.activity == ActivityState::Working =>
                    {
                        format!("parent not busy; background work; tracked request {run}")
                    }
                    Some(run) => format!("tracked request {run} ({activity})"),
                    None if !observed.parent_busy
                        && observed.activity == ActivityState::Working =>
                    {
                        "parent not busy; background work; no tracked parent request recorded"
                            .into()
                    }
                    None => {
                        format!("provider {activity}; current work not linked to a tracked request")
                    }
                });
            }
            None => lines.push("current request identity unavailable".into()),
        }
        if let Some(run) = &self.latest_run {
            let state = match run.state {
                State::Accepted => "accepted",
                State::Running => "running",
                State::Waiting => "waiting for input",
                State::Succeeded => "succeeded",
                State::Failed => "failed",
                State::Cancelled => "cancelled",
                State::Interrupted => "outcome uncertain",
            };
            let confirmation = if run.request_recorded {
                ""
            } else {
                "; provider request not recorded"
            };
            lines.push(format!(
                "latest tracked run {} {state}{confirmation}",
                run.id
            ));
            if let Some(diagnostic) = &run.diagnostic {
                let mut summary = diagnostic
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect::<String>();
                if super::truncate(&mut summary, 160) {
                    summary.push('…');
                }
                lines.push(format!("last run error: {summary}"));
            }
        }
        if self.pending_message == Some(true) {
            lines.push("instruction queued; not delivered".into());
        }
        lines
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The live session ID. It addresses this session until it exits; a
    /// relaunched or recovered session is listed under a new ID and the same
    /// lineage.
    pub id: TerminalId,
    pub surface: Surface,
    /// Absent only for the caller's own plain shell.
    pub provider: Option<Provider>,
    pub title: Option<String>,
    /// Daemon-owned channel membership, when this session belongs to one.
    #[serde(default)]
    pub channel: Option<crate::model::ChannelId>,
    /// Exact daemon-owned view, absent when membership is missing or ambiguous.
    #[serde(default)]
    pub view_scope: Option<super::Placement>,
    /// The channel's owning repository, independent of this session's cwd.
    #[serde(default)]
    pub channel_repository: Option<PathBuf>,
    pub state: ActivityState,
    #[serde(default)]
    pub status: Option<Status>,
    pub directory: Option<PathBuf>,
    /// The checkout that contains `directory`: a repository or linked worktree.
    pub worktree: Option<PathBuf>,
    /// The open project's repository key and display name.
    pub project: Option<PathBuf>,
    pub project_name: Option<String>,
    pub delegation: Option<Delegation>,
    pub relation: Relation,
    /// Stable across this session's relaunches and recoveries.
    pub lineage: super::Lineage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directory {
    /// The calling session; absent for a caller outside cagents.
    pub caller: Option<Entry>,
    /// The scope actually served, after a directory outside every open
    /// project widened to `All`.
    pub scope: Scope,
    /// Every other matching session, the caller's relatives first.
    pub sessions: Vec<Entry>,
}

impl Entry {
    /// One line for people and agents scanning a list. IDs are printed whole
    /// because they are what every other command takes.
    pub fn line(&self) -> String {
        let provider = self
            .provider
            .map_or("shell", |provider| provider.executable());
        let state = match self.state {
            ActivityState::Unknown => "unknown",
            ActivityState::Idle => "idle",
            ActivityState::Working => "working",
            ActivityState::Waiting => "waiting",
        };
        let relation = match (self.relation, self.delegation.as_ref().map(|d| &d.owner)) {
            (Relation::Myself, _) => " (me)",
            (Relation::Parent, _) => " (my parent)",
            // A caller outside cagents controls every agent another such
            // caller started; they are shared, not personal.
            (Relation::Child, Some(Owner::User)) => " (user agent)",
            (Relation::Child, _) => " (my agent)",
            (Relation::Peer, _) => "",
        };
        let mut line = format!("{}  {provider:<6} {state:<8}", self.id);
        if let Some(name) = &self.project_name {
            line.push_str(&format!(" [{name}]"));
        }
        if let Some(directory) = &self.directory {
            line.push_str(&format!(" {}", directory.display()));
        }
        if let Some(title) = self.title.as_deref().filter(|title| !title.is_empty()) {
            line.push_str(&format!("  \"{title}\""));
        }
        if let Some(super::Placement::Worktree { path, .. }) = &self.view_scope {
            line.push_str(&format!(" [worktree view: {}]", path.display()));
        }
        line.push_str(relation);
        if self.lineage.0 != self.id {
            line.push_str(&format!("\n    lineage {}", self.lineage));
        }
        if let Some(delegation) = &self.delegation {
            line.push_str(&format!("\n    agent {}", delegation.agent));
            if self.status.is_none()
                && let Some(state) = delegation.run_state
            {
                line.push_str(&format!(", latest run {state:?}"));
            }
        }
        if let Some(status) = &self.status {
            for explanation in status.explanation() {
                line.push_str("\n    ");
                line.push_str(&explanation);
            }
        }
        line
    }
}
