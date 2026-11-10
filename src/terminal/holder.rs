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
    #[serde(serialize_with = "serialize_recovery_options")]
    pub options: super::TerminalSpawnOptions,
    pub execution_environment: Option<super::ipc::ChatExecutionEnvironment>,
    /// The actual restart recipe retained by this original holder. It must
    /// survive adoption even when the published source is a fresh/shell tuple.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) execution_recipe: Option<crate::model::LaunchRecipe>,
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
        if let Some(recipe) = &self.execution_recipe
            && crate::upgrade::exact_conversation(recipe).is_none()
        {
            return Err("holder has inconsistent exact restart execution".into());
        }
        Ok(())
    }
}

// Recovery travels as private holder JSON. The Rust field was renamed to make
// its byte units explicit, but old holders require the original JSON key on
// Attach. Current readers accept that same key through their alias.
fn serialize_recovery_options<S: serde::Serializer>(
    options: &super::TerminalSpawnOptions,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Options<'a> {
        columns: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        max_scrollback: usize,
        initially_visible: bool,
        theme: &'a super::TerminalTheme,
    }
    Options {
        columns: options.columns,
        rows: options.rows,
        cell_width_px: options.cell_width_px,
        cell_height_px: options.cell_height_px,
        max_scrollback: options.max_scrollback_bytes,
        initially_visible: options.initially_visible,
        theme: &options.theme,
    }
    .serialize(serializer)
}
