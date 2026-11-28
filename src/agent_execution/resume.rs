//! Resuming a session whose provider stopped, without a window.
//!
//! The daemon reopens the conversation the way its pane would when a window
//! opens or the person presses Resume: the exact saved conversation, in its
//! original directory and on its original account, only once the daemon has
//! proved nothing else still writes it. Nothing the stopped provider held is
//! resent; the conversation starts idle and waits for its next message.

use serde::{Deserialize, Serialize};

use crate::{
    agent::Provider,
    model::{AgentId, ChannelId, TerminalId},
};

/// Which stopped sessions a [`super::Operation::Stopped`] read lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// Sessions in a directory scope, as `cagents sessions` reads one.
    Sessions(super::directory::Scope),
    /// The sessions of this channel, or of the caller's own when absent.
    Channel(Option<ChannelId>),
}

/// What a [`super::Operation::Stopped`] read finds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedSessions {
    /// The conversations [`super::Operation::Resume`] may reopen, one for
    /// each lineage, the most recently active first.
    pub sessions: Vec<Stopped>,
    /// Stopped chats in scope that only their pane in the app can reopen,
    /// such as one whose provider never recorded a conversation.
    pub in_app_only: usize,
}

/// A session whose provider stopped, which [`super::Operation::Resume`] may
/// reopen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stopped {
    pub id: TerminalId,
    pub provider: Provider,
    pub title: Option<String>,
    /// The open agent this conversation belongs to, if any.
    pub agent: Option<AgentId>,
    /// The provider's last reported failure, such as a sign-in that needs
    /// attention, in one line.
    pub failure: Option<String>,
}

/// What became of one resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resumed {
    /// The session that was named, as it was before resuming.
    pub stopped: TerminalId,
    /// The session that now runs the conversation: the same ID when the
    /// conversation reopened in place, a new one in its lineage when the
    /// daemon had to replace the stopped process.
    pub session: TerminalId,
    pub provider: Provider,
    pub title: Option<String>,
    /// The open agent whose session this now is.
    pub agent: Option<AgentId>,
    /// Nothing was started: the conversation was already running.
    pub already_running: bool,
    /// The provider had not confirmed it was ready when the reply was sent.
    pub starting: bool,
    /// Something the person should know although the conversation runs, such
    /// as its agent not taking it over.
    pub note: Option<String>,
}
