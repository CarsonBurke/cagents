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
    /// Exact daemon-owned view, absent when membership is missing or ambiguous.
    #[serde(default)]
    pub view_scope: Option<super::Placement>,
    /// The channel's owning repository, independent of this session's cwd.
    #[serde(default)]
    pub channel_repository: Option<PathBuf>,
    /// Reported for the caller only: the channel its lineage last belonged
    /// to, which an agent it starts joins when its own view is missing or
    /// ambiguous.
    #[serde(skip)]
    pub lineage_channel: Option<ChannelHome>,
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

/// A channel and the project that owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelHome {
    pub id: crate::model::ChannelId,
    pub repository: PathBuf,
}

/// Placement context uses a new reply so the directory entry remains readable
/// by CLIs retained across an update. Postcard cannot default an added field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Context {
    pub sessions: Sessions,
    pub lineage_channel: Option<ChannelHome>,
}

impl Context {
    pub fn into_sessions(mut self) -> Sessions {
        if let Some(caller) = &mut self.sessions.caller {
            caller.session.lineage_channel = self.lineage_channel;
        }
        self.sessions
    }
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

/// A session as a listing, a channel or a transcript reports it: its
/// directory entry, and what someone picking up its work or messaging it
/// needs first.
///
/// Kept apart from [`Entry`], whose layout older `cagents` CLIs carried across
/// an update still decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub session: Entry,
    /// The provider has exited. Its conversation stays readable with
    /// `cagents transcript` until the session is closed.
    pub exited: bool,
    /// The provider's last reported failure, such as a usage limit, in one line.
    pub failure: Option<String>,
    /// Unix milliseconds of the conversation's last saved provider history.
    pub active_at: Option<u64>,
    /// The usage limit that stopped the conversation, until a later turn
    /// starts or the limit resets. Whatever its activity says, a limited
    /// session cannot act on a prompt or a message before then.
    pub limit: Option<UsageLimit>,
    /// The account the provider runs on, when the daemon can tell.
    pub account: Option<SessionAccount>,
}

impl Member {
    /// What the session is doing, as a listing names it: a usage limit or an
    /// exit outranks the activity the provider last reported.
    pub fn state_label(&self) -> &'static str {
        if self.exited {
            "exited"
        } else if self.limit.is_some() {
            "limited"
        } else {
            match self.session.state {
                ActivityState::Unknown => "unknown",
                ActivityState::Idle => "idle",
                ActivityState::Working => "working",
                ActivityState::Waiting => "waiting",
            }
        }
    }
}

/// A provider usage limit a session reached: its provider refused the latest
/// turn because the account it runs on is out of allowance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLimit {
    /// The limit in a line, such as "Claude weekly limit reached".
    pub summary: String,
    /// When the provider said it resets, in its own words.
    pub reset: Option<String>,
    /// Unix seconds at which the account's exhausted usage window resets,
    /// when the daemon's latest usage reading for the account knows it.
    pub resets_at: Option<i64>,
}

impl UsageLimit {
    /// Whether a known reset has passed, so the limit no longer holds.
    pub fn has_reset(&self, now_ms: u64) -> bool {
        self.resets_at
            .is_some_and(|at| at >= 0 && (at as u64).saturating_mul(1000) <= now_ms)
    }
}

/// The account a session's provider runs on, and its allowance as the
/// daemon last observed it. It is the account summary `cagents account info`
/// reports, narrowed to what choosing a session to message needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionAccount {
    pub id: crate::account::AccountId,
    pub label: Option<String>,
    /// How the account stands against its limits; absent when the daemon
    /// has no reading of it.
    pub status: Option<crate::account::UsageStatus>,
    pub remaining: Vec<crate::account::info::RemainingWindow>,
    /// The reading is too old, or one of its windows has reset since, so its
    /// figures are no basis for a plan.
    pub stale: bool,
    /// Unix milliseconds at which the reading was taken; absent with it.
    pub observed_at: Option<u64>,
    /// The provider's own word, in that reading, on whether the account may
    /// be used as usual, where it gives one (Codex does). It outranks the
    /// windows: Codex can permit usage with a window used up.
    pub ordinary_usage_allowed: Option<bool>,
}

impl SessionAccount {
    /// The window that limits the account most: the one the provider named
    /// binding, or else the one with the least left.
    pub fn tightest(&self) -> Option<&crate::account::info::RemainingWindow> {
        self.remaining
            .iter()
            .find(|window| window.window.binding && !window.reset_elapsed)
            .or_else(|| {
                self.remaining
                    .iter()
                    .filter(|window| !window.reset_elapsed)
                    .min_by(|a, b| a.remaining_fraction.total_cmp(&b.remaining_fraction))
            })
    }
}

/// The caller and the sessions in a scope, as members: what a directory
/// read reports, with each session's usage limit and account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sessions {
    pub caller: Option<Member>,
    /// The scope actually served, after a directory outside every open
    /// project widened to `All`.
    pub scope: Scope,
    /// Every other matching session, the caller's relatives first.
    pub sessions: Vec<Member>,
}

/// The sessions placed in one channel, including chats whose provider exited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Members {
    pub channel: crate::model::ChannelId,
    /// The calling session, when it is in this channel.
    pub caller: Option<Member>,
    /// Every other member, most recently active first.
    pub sessions: Vec<Member>,
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
            if let Some(state) = delegation.run_state {
                line.push_str(&format!(", latest run {state:?}"));
            }
        }
        line
    }
}
