//! Discovery evidence for terminal holders. Only process identities go to disk;
//! recipes and transient execution settings stay in the holder's memory.

use serde::{Deserialize, Serialize};

pub(crate) mod cleanup;

pub(crate) use crate::daemon::holder_registry::{Process, Record};
use crate::model::{BootId, TerminalId, TerminalSession};

pub(crate) fn directory() -> Result<std::path::PathBuf, String> {
    crate::daemon::holder_registry::directory(crate::daemon::holder_registry::Kind::Terminal)
}

pub(crate) use crate::daemon::holder_registry::socket;

pub(crate) fn evidence(socket: &std::path::Path) -> Result<Option<Record>, String> {
    crate::daemon::holder_registry::evidence(socket, crate::daemon::holder_registry::Kind::Terminal)
}

pub(crate) fn discover(directory: &std::path::Path, boot: &BootId) -> Result<Vec<Record>, String> {
    crate::daemon::holder_registry::discover(
        directory,
        boot,
        crate::daemon::holder_registry::Kind::Terminal,
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recovery {
    pub source: TerminalSession,
    pub previous_sources: Vec<TerminalSession>,
    pub relaunches: Vec<super::ipc::UpgradeRelaunch>,
    pub options: super::TerminalSpawnOptions,
    pub execution_environment: Option<super::ipc::ChatExecutionEnvironment>,
}

impl Recovery {
    pub(crate) fn validate(&self, boot: &BootId, terminal: TerminalId) -> Result<(), String> {
        let proof = self
            .source
            .continuity
            .live_proof()
            .filter(|proof| proof.is_complete() && &proof.boot_id == boot)
            .ok_or("holder has no complete ownership proof for this boot")?;
        if self.source.id != terminal
            || self.previous_sources.len() > 1024
            || self.previous_sources.iter().any(|source| {
                source.id != terminal
                    || source
                        .continuity
                        .live_proof()
                        .is_none_or(|old| !old.is_complete() || old.boot_id != proof.boot_id)
            })
        {
            return Err("holder has inconsistent terminal lineage".into());
        }
        Ok(())
    }
}
