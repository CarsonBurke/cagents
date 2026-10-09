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
    /// The channel's owning repository, independent of this session's cwd.
    #[serde(default)]
    pub channel_repository: Option<PathBuf>,
    pub state: ActivityState,
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
        line.push_str(relation);
        if self.lineage.0 != self.id {
            line.push_str(&format!("\n    lineage {}", self.lineage));
        }
        if let Some(delegation) = &self.delegation {
            line.push_str(&format!("\n    agent {}", delegation.agent));
            if let Some(state) = delegation.run_state {
                line.push_str(&format!(", latest run {state:?}"));
            }
        }
        line
    }
}
