//! The channel-work wire surface carries intent and captured placement, never
//! an actor identity or a checkout-derived ownership assertion.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    channel_work::{
        Home, HomeLifecycle, Mutation, Page, Receipt, Reply, Snapshot, Target, TargetSnapshot,
    },
    model::{CardId, ChannelId, RunId, TerminalId},
};

/// Public comparison values returned by the daemon. These are not authority:
/// the authenticated source and canonical pane are rechecked at commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caller {
    pub session: TerminalId,
    pub card: CardId,
    /// Durable causal revision of this exact pane's placement and source.
    /// Presentation and unrelated pane edits cannot change this epoch.
    pub placement_epoch: u64,
    pub run: Option<RunId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    pub home: Home,
    pub title: String,
    pub caller: Option<Caller>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// A session defaults to its actual canonical channel. User callers must
    /// choose a channel explicitly; cwd and the active window are not owners.
    Context {
        channel: Option<ChannelId>,
    },
    Read {
        home: Home,
        page: Page,
    },
    Mutate {
        mutation: Mutation,
        caller: Option<Caller>,
    },
    Receipt {
        home: Home,
        operation: Uuid,
    },
    /// Refresh only already tracked PRs. Forge facts never create claims.
    Refresh {
        home: Home,
    },
    ReadTarget {
        home: Home,
        target: Target,
    },
    /// Reconcile an original durable home without loading any work projection.
    ReadHome {
        home: Home,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Failure {
    /// Validation or a rolled-back transaction: this attempt made no change.
    Refused(String),
    /// The original operation may have committed. Reconcile its UUID; a
    /// missing receipt does not authorize a replacement operation.
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Context(Context),
    Snapshot(Snapshot),
    Applied(Reply),
    Receipt(Option<Receipt>),
    Failed(Failure),
    Target(TargetSnapshot),
    Home {
        home: Home,
        lifecycle: HomeLifecycle,
        revision: u64,
    },
}
