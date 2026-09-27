//! Headless agent execution shared by the CLI, daemon, and windows.
//!
//! An agent is a conversation; a run is one accepted prompt. Provider turn
//! identifiers are evidence attached to a run, never a replacement for its
//! identity. A restarted daemon must not resend an accepted prompt merely
//! because it cannot determine whether the provider received it.

use std::path::PathBuf;

#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) mod bundled;

use serde::{Deserialize, Serialize};

use crate::{
    account::AccountId,
    agent::Provider,
    chat,
    model::{AgentId, ChannelId, RunId, TerminalId, TerminalSession},
};

pub const MAX_PROMPT_BYTES: usize = 256 * 1024;
pub const MAX_RESULT_BYTES: usize = 1024 * 1024;
pub const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;
pub const MAX_CHILDREN: usize = 64;
pub const MAX_PARENT_RUNNING: usize = 8;
pub const MAX_RUNNING: usize = 32;
pub const MAX_PARENT_DEPTH: usize = 8;
pub const AUTHORITY_ENV: &str = "CAGENTS_AGENT_AUTHORITY";

/// Presentation routing only. Daemon ownership additionally requires its
/// private exact console recipe and the issued paired session descriptor.
pub fn console_agent(recipe: &crate::model::LaunchRecipe) -> Option<AgentId> {
    let [command, surface, agent] = recipe.arguments.as_slice() else {
        return None;
    };
    (command == "agent" && surface == "console")
        .then(|| agent.parse().ok())
        .flatten()
}

/// A session capability injected by the daemon, never copied into a public
/// launch recipe or diagnostic. Absence is the user's CLI scope; that scope
/// does not grant authority over session-owned children.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Authority(Option<String>);

impl std::fmt::Debug for Authority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(if self.0.is_some() {
            "Authority::Session(<redacted>)"
        } else {
            "Authority::User"
        })
    }
}

impl Authority {
    pub fn from_environment() -> Self {
        Self(std::env::var(AUTHORITY_ENV).ok())
    }
    pub(crate) fn session(token: String) -> Self {
        Self(Some(token))
    }
    pub(crate) fn token(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

/// Assigned after caller authentication. A raw parent ID in a request grants
/// no authority to address another session's children.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    User,
    Child { parent: TerminalId },
    Automation { id: uuid::Uuid },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Surface {
    #[default]
    Chat,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub repository: PathBuf,
    pub directory: PathBuf,
    /// An existing shared channel, or a fresh channel created atomically with
    /// acceptance. Neither a window's active channel nor its cwd is implicit.
    pub channel: Option<ChannelId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub provider: Provider,
    pub account: Option<AccountId>,
    pub target: Target,
    pub surface: Surface,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Destination {
    New(Spec),
    Existing(AgentId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submit {
    pub destination: Destination,
    pub prompt: String,
    /// Stable within the authenticated origin. Retrying an identical request
    /// returns its original acceptance; reusing the key for other work fails.
    pub idempotency_key: String,
}

impl Submit {
    pub fn validate(&self) -> Result<(), String> {
        if self.prompt.trim().is_empty() || self.prompt.len() > MAX_PROMPT_BYTES {
            return Err(format!(
                "A prompt must contain text and be at most {MAX_PROMPT_BYTES} bytes"
            ));
        }
        if self.prompt.contains('\0') {
            return Err("A prompt cannot contain a NUL byte".into());
        }
        if self.idempotency_key.is_empty()
            || self.idempotency_key.len() > 256
            || self.idempotency_key.chars().any(char::is_control)
        {
            return Err("An idempotency key must be 1–256 bytes without control characters".into());
        }
        if let Destination::New(spec) = &self.destination {
            if !spec.target.repository.is_absolute() || !spec.target.directory.is_absolute() {
                return Err("Agent repository and working directory must be absolute".into());
            }
            for (name, value) in [
                ("model", spec.model.as_deref()),
                ("permission mode", spec.permission_mode.as_deref()),
            ] {
                if value.is_some_and(|value| {
                    value.is_empty() || value.len() > 256 || value.chars().any(char::is_control)
                }) {
                    return Err(format!("Invalid {name}"));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostedSession {
    Chat(chat::Session),
    Terminal(TerminalSession),
}

impl HostedSession {
    /// Runtime ownership excludes mutable conversation/presentation metadata.
    /// A successor proof or different launch never identifies this process.
    pub(crate) fn same_process(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Chat(a), Self::Chat(b)) => {
                a.id == b.id
                    && a.proof == b.proof
                    && a.launch.provider == b.launch.provider
                    && a.launch.recipe == b.launch.recipe
            }
            (Self::Terminal(a), Self::Terminal(b)) => {
                a.id == b.id
                    && a.launch == b.launch
                    && a.continuity.live_proof().is_some()
                    && a.continuity.live_proof() == b.continuity.live_proof()
            }
            _ => false,
        }
    }

    pub fn id(&self) -> TerminalId {
        match self {
            Self::Chat(session) => session.id,
            Self::Terminal(session) => session.id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub origin: Origin,
    pub spec: Spec,
    pub channel: ChannelId,
    pub session: Option<HostedSession>,
    #[serde(default)]
    pub console: Option<TerminalSession>,
    pub created_ms: u64,
    pub closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Accepted,
    Running,
    Waiting,
    Succeeded,
    Failed,
    Cancelled,
    /// Ownership or delivery evidence was lost. This is not permission to
    /// retry a prompt: its effects may already have occurred.
    Interrupted,
}

impl State {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }

    pub fn wait_ready(self) -> bool {
        self.is_terminal() || self == Self::Waiting
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultSummary {
    pub text: String,
    pub truncated: bool,
    pub usage: chat::Usage,
    pub diagnostic: Option<String>,
}

impl ResultSummary {
    pub fn bound(&mut self) {
        self.truncated |= truncate(&mut self.text, MAX_RESULT_BYTES);
        if let Some(diagnostic) = &mut self.diagnostic {
            truncate(diagnostic, MAX_DIAGNOSTIC_BYTES);
        }
    }
}

pub(crate) fn truncate(text: &mut String, limit: usize) -> bool {
    if text.len() <= limit {
        return false;
    }
    let mut boundary = limit;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub agent: AgentId,
    pub state: State,
    pub revision: u64,
    pub accepted_ms: u64,
    pub started_ms: Option<u64>,
    pub finished_ms: Option<u64>,
    /// Opaque correlation key for the submitted provider request. This is not
    /// a provider session ID and need not be a provider-generated turn ID.
    pub provider_key: Option<String>,
    pub result: ResultSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Accepted {
    pub agent: Agent,
    pub run: Run,
    /// False when an identical earlier request is being acknowledged again.
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Operation {
    Submit(Submit),
    Status(AgentId),
    Run(RunId),
    List,
    Close(AgentId),
    Console {
        agent: AgentId,
        command: ConsoleCommand,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConsoleCommand {
    Read {
        after_generation: Option<u64>,
        after_run: Option<(RunId, u64)>,
    },
    Submit {
        prompt: String,
        idempotency_key: String,
    },
    Control(chat::Command),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsoleView {
    pub generation: u64,
    pub text: String,
    pub truncated: bool,
    pub requests: Vec<chat::Request>,
    pub ready: bool,
    pub run: Run,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub agent: Agent,
    pub run: RunId,
    pub state: State,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Reply {
    Accepted(Accepted),
    Status { agent: Agent, run: Run },
    Listed(Vec<Summary>),
    Closed(Agent),
    Console(Option<ConsoleView>),
}

/// Already correlated to one accepted RunId by the provider adapter. A late
/// outcome must never be assigned to the agent's currently active run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    Started { provider_key: String },
    Waiting,
    Resumed,
    Finished { state: State, result: ResultSummary },
}

/// Adapter-local evidence. Settlement freezes the result before a deferred
/// cancellation decision can arrive during a later prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderEvent {
    Started {
        key: String,
    },
    /// Only this frame's output belongs to the named run. Shared providers
    /// may interleave other clients' turns, history, or background work.
    Scope,
    Settled,
    Finished {
        outcome: crate::notifications::Kind,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_is_observable_without_settling_the_run() {
        assert!(State::Waiting.wait_ready());
        assert!(!State::Waiting.is_terminal());
        assert!(!State::Accepted.wait_ready());
        assert!(!State::Running.wait_ready());
        assert!(State::Interrupted.is_terminal());
    }

    #[test]
    fn bounded_results_preserve_utf8_and_report_truncation() {
        let mut result = ResultSummary {
            text: "€".repeat(MAX_RESULT_BYTES / 3 + 1),
            diagnostic: Some("é".repeat(MAX_DIAGNOSTIC_BYTES)),
            ..Default::default()
        };
        result.bound();
        assert!(result.truncated);
        assert!(result.text.len() <= MAX_RESULT_BYTES);
        assert!(result.text.ends_with('€'));
        assert_eq!(result.diagnostic.unwrap().len(), MAX_DIAGNOSTIC_BYTES);
    }
}
