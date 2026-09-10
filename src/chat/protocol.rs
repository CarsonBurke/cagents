//! Provider JSON protocols used by daemon-owned pipe and Unix WebSocket transports.
use super::{Command, Event, Launch};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};

pub mod claude;
pub mod codex;
pub mod omp;

#[derive(Debug, Clone)]
pub enum TransportSpec {
    JsonLines,
    /// Join only the exact socket checked by the provider adapter.
    UnixWebSocket {
        socket: PathBuf,
        device: u64,
        inode: u64,
    },
}

#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub transport: TransportSpec,
    pub program: String,
    pub arguments: Vec<String>,
    pub working_directory: PathBuf,
    pub environment: BTreeMap<String, String>,
}
impl From<&crate::model::LaunchRecipe> for ProcessSpec {
    fn from(recipe: &crate::model::LaunchRecipe) -> Self {
        Self {
            transport: TransportSpec::JsonLines,
            program: recipe.program.clone(),
            arguments: recipe.arguments.clone(),
            working_directory: recipe.working_directory.clone(),
            environment: recipe.environment.clone(),
        }
    }
}
#[derive(Debug, Default)]
pub struct Step {
    /// Written in order before the next incoming frame is processed.
    pub writes: Vec<Value>,
    pub events: Vec<Event>,
}
/// An adapter is confined to one daemon worker. No UI state, subprocesses,
/// unbounded transcript storage, credentials, or global mutable state live here.
pub trait Adapter: Send {
    fn initialize(&mut self) -> Result<Step, String>;
    fn command(&mut self, command: Command) -> Result<Step, String>;
    fn receive(&mut self, frame: Value) -> Result<Step, String>;
}
pub fn create(launch: &Launch) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    launch.provider.create_chat(launch)
}
