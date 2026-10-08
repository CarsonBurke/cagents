//! Narrow maintenance helper. Request JSON contains a reviewed authoritative
//! shared Pane, database path, boot_id, workspace id and expected generations.
//! It never connects to the daemon, starts actors or reconciles store startup.
use cagents::{
    model::{BootId, CardId, CardState, Existence, Exposure, Residency, WorkspaceId},
    persistence::{SnapshotBase, StateStore},
    workspace_model::views::Pane,
};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    database: PathBuf,
    boot_id: BootId,
    workspace: WorkspaceId,
    pane: Pane,
    workspace_generation: u64,
    card_generations: BTreeMap<CardId, u64>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .ok_or("usage: repair-closed-terminal-owner REQUEST.json")?;
    if args.next().is_some() {
        return Err("usage: repair-closed-terminal-owner REQUEST.json".into());
    }
    let request: Request = serde_json::from_slice(&std::fs::read(path)?)?;
    request.pane.validate()?;
    if request.pane.pending_terminal_start {
        return Err("repair cannot authorize a pending terminal start".into());
    }
    let canonical = CardState {
        id: request.pane.id,
        generation: 0,
        title: request.pane.title,
        width_fraction: request.pane.width_fraction,
        existence: Existence::Open,
        exposure: Exposure::Offscreen,
        residency: Residency::Cold,
        kind: request.pane.kind,
    };
    match StateStore::repair_closed_terminal_pane_owner(
        request.database,
        &request.boot_id,
        request.workspace,
        &canonical,
        SnapshotBase {
            workspace: request.workspace_generation,
            cards: &request.card_generations,
        },
    )? {
        Some(commit) => println!(
            "repaired canonical pane {}; workspace generation {}",
            canonical.id, commit.generation
        ),
        None => println!("canonical pane {} already owns its terminal", canonical.id),
    }
    Ok(())
}
