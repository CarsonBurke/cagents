//! Narrow authoritative metadata reads. None loads views or conversations.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ModelProject;
use crate::agent_execution::Agent;
use crate::model::AgentId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Query {
    Identity,
    Projects,
    Agents,
    AgentLabels,
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
    AgentLabels {
        identity: Uuid,
        labels: Vec<(AgentId, String)>,
    },
}
