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
    /// A supervised provider server that both native and CLI views can join.
    OwnedUnixWebSocket {
        socket: PathBuf,
    },
    /// Join only the exact socket checked by the provider adapter.
    UnixWebSocket {
        socket: PathBuf,
        device: u64,
        inode: u64,
    },
}

#[derive(Clone)]
pub struct ProcessSpec {
    /// Private spawn evidence and an optional caller-validated directory. Never
    /// serialized into launch recipes or provider configuration.
    pub(crate) execution: Option<std::sync::Arc<ExecutionContext>>,
    pub transport: TransportSpec,
    pub program: String,
    pub arguments: Vec<String>,
    pub working_directory: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub unset_environment: std::collections::BTreeSet<String>,
}
impl std::fmt::Debug for ProcessSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessSpec")
            .field("transport", &self.transport)
            .field("program", &self.program)
            .field("arguments", &self.arguments)
            .field("working_directory", &self.working_directory)
            .field("environment", &"<redacted>")
            .finish()
    }
}
impl From<&crate::model::LaunchRecipe> for ProcessSpec {
    fn from(recipe: &crate::model::LaunchRecipe) -> Self {
        Self {
            execution: None,
            transport: TransportSpec::JsonLines,
            program: recipe.program.clone(),
            arguments: recipe.arguments.clone(),
            working_directory: recipe.working_directory.clone(),
            environment: recipe.environment.clone(),
            unset_environment: crate::account::launch_credential_overrides(recipe),
        }
    }
}

#[derive(Default)]
pub(crate) struct ExecutionContext {
    pub directory: Option<std::sync::Arc<std::fs::File>>,
    expected_directory: Option<crate::agent_execution::DirectoryIdentity>,
    spawned: std::sync::atomic::AtomicBool,
}

impl ExecutionContext {
    pub(crate) fn new(directory: Option<std::sync::Arc<std::fs::File>>) -> Self {
        Self {
            directory,
            ..Default::default()
        }
    }
    pub(crate) fn restored(identity: crate::agent_execution::DirectoryIdentity) -> Self {
        Self {
            expected_directory: Some(identity),
            ..Default::default()
        }
    }
    pub(crate) fn directory_identity(
        &self,
    ) -> Result<Option<crate::agent_execution::DirectoryIdentity>, String> {
        match &self.directory {
            Some(directory) => directory
                .metadata()
                .map(|metadata| {
                    Some(crate::agent_execution::DirectoryIdentity::from_metadata(
                        &metadata,
                    ))
                })
                .map_err(|error| error.to_string()),
            None => Ok(self.expected_directory),
        }
    }
    /// Call synchronously as soon as a child (including a launch holder) exists,
    /// before any asynchronous worker can create another provider process.
    pub(crate) fn spawned(&self) {
        self.spawned
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn did_spawn(&self) -> bool {
        self.spawned.load(std::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn verify_directory(&self, path: &std::path::Path) -> Result<(), String> {
        let Some(expected) = self.directory_identity()? else {
            return Ok(());
        };
        let current = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !current.is_dir()
            || expected != crate::agent_execution::DirectoryIdentity::from_metadata(&current)
        {
            return Err("The approved agent directory changed before provider dispatch".into());
        }
        Ok(())
    }
}
impl ProcessSpec {
    pub(crate) fn remove_unset_environment(&mut self) {
        self.environment
            .retain(|name, _| !self.unset_environment.contains(name));
    }
}
#[derive(Debug, Default)]
pub struct Step {
    /// Written in order before the next incoming frame is processed.
    pub writes: Vec<Value>,
    pub events: Vec<Event>,
}
/// Account identity verified during initialize on the actual server connection.
/// A holder can retain it when the account directory is subsequently removed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServerIdentity {
    pub user_agent: String,
    pub canonical_home: PathBuf,
    pub reported_home: String,
}
/// An adapter is confined to one daemon worker. No UI state, subprocesses,
/// unbounded transcript storage, credentials, or global mutable state live here.
pub trait Adapter: Send {
    fn initialize(&mut self) -> Result<Step, String>;
    fn command(&mut self, command: Command) -> Result<Step, String>;
    fn command_for_run(
        &mut self,
        _run: crate::model::RunId,
        _command: Command,
    ) -> Result<Step, String> {
        Err("This provider transport does not support correlated agent runs".into())
    }
    fn receive(&mut self, frame: Value) -> Result<Step, String>;
    /// Identity reported by the initialized server on this exact connection.
    /// Only a holder with a verified provider contract can use it for durable
    /// ownership; an executable's separate `--version` output is insufficient.
    fn server_identity(&self) -> Option<&ServerIdentity> {
        None
    }
}
pub fn create(launch: &Launch) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    launch.provider.create_chat(launch)
}

/// Record a provider-confirmed conversation rotation in the launch recipe.
///
/// Upgrade checkpointing stamps the live confirmed identity onto
/// `session_id`, but the recipe still names the originally launched resume
/// ID — a checkpoint that neither validates nor restarts the final
/// conversation. This rewrites the provider's resume arguments to the
/// confirmed identity. Only call with an identity the live provider
/// reported, never user input: strict `create()` forgery checks stay intact
/// everywhere else. OMP needs nothing: it derives `--resume` from the
/// session ID when creating the process spec.
pub fn align_resume_identity(launch: &mut Launch) -> Result<(), String> {
    let Some(id) = launch.session_id.clone() else {
        return Ok(());
    };
    match launch.provider {
        crate::agent::Provider::Claude => {
            claude::align_resume_identity(&mut launch.recipe.arguments, &id)
        }
        crate::agent::Provider::Codex => {
            codex::align_resume_identity(&mut launch.recipe.arguments, &id)
        }
        crate::agent::Provider::Omp => Ok(()),
    }
}
