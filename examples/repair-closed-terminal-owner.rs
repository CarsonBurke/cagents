//! Narrow maintenance helper. Request JSON contains a reviewed authoritative
//! shared Pane, database path, boot_id, workspace id and expected generations.
//! It never connects to the daemon, starts actors or reconciles store startup.
use cagents::{
    control::ControlClient,
    model::{
        BootId, CardId, CardKind, CardState, Existence, Exposure, LaunchRecipe, Residency,
        TerminalContinuity, WorkspaceId,
    },
    persistence::{SnapshotBase, StateStore},
    workspace_model::views::{Pane, Scope},
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
    #[serde(default)]
    offline_authority: Option<OfflineAuthority>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OfflineAuthority {
    socket: PathBuf,
    scope: Scope,
    revision: u64,
    expected_historical_launch: LaunchRecipe,
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
    if let Some(authority) = &request.offline_authority {
        let CardKind::Terminal(terminal) = &request.pane.kind else {
            return Err("offline recipe authority requires a terminal".into());
        };
        if !matches!(&terminal.continuity,TerminalContinuity::RelaunchAvailable {recipe,..} if recipe==&terminal.launch)
        {
            return Err("offline authority requires an exact relaunch-available descriptor".into());
        }
        let mut client = ControlClient::connect(&authority.socket)?;
        let model = client.read_workspace_model()?;
        let view = model
            .views
            .iter()
            .find(|view| view.scope == authority.scope)
            .ok_or("missing shared scope")?;
        if view.revision != authority.revision || !view.panes.contains(&request.pane) {
            return Err("authoritative view changed since this repair was reviewed".into());
        }
        if client.list()?.iter().any(|actor| {
            actor.session.id == terminal.id
                || actor.session.recovery_origin == Some(terminal.id)
                || actor.session.predecessor == Some(terminal.id)
        }) || client.list_chats()?.iter().any(|actor| {
            actor.id == terminal.id
                || actor.terminal.as_ref().is_some_and(|view| {
                    view.id == terminal.id
                        || view.recovery_origin == Some(terminal.id)
                        || view.predecessor == Some(terminal.id)
                })
        }) {
            return Err("offline recipe repair cannot change a current daemon actor".into());
        }
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
    let base = SnapshotBase {
        workspace: request.workspace_generation,
        cards: &request.card_generations,
    };
    let commit = match request.offline_authority {
        Some(authority) => StateStore::repair_closed_terminal_pane_owner_with_offline_recipe(
            request.database,
            &request.boot_id,
            request.workspace,
            &canonical,
            base,
            &authority.expected_historical_launch,
        )?,
        None => StateStore::repair_closed_terminal_pane_owner(
            request.database,
            &request.boot_id,
            request.workspace,
            &canonical,
            base,
        )?,
    };
    match commit {
        Some(commit) => println!(
            "repaired canonical pane {}; workspace generation {}",
            canonical.id, commit.generation
        ),
        None => println!("canonical pane {} already owns its terminal", canonical.id),
    }
    Ok(())
}
