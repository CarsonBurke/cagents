//! Narrow authoritative reads used by window safety checks. None loads views.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ModelProject;
use crate::agent_execution::Agent;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Query {
    Identity,
    Projects,
    Agents,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Reply {
    Identity(Uuid),
    Projects {
        identity: Uuid,
        projects: Vec<ModelProject>,
    },
    Agents {
        identity: Uuid,
        agents: Vec<Agent>,
    },
}
