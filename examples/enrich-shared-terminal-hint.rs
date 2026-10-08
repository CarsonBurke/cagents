//! Publish a reviewed historical resume hint through the daemon's normal view
//! edit API. Other pane fields, actor starts and per-window state are untouched.
use cagents::{
    control::ControlClient,
    model::{CardKind, agent_recovery},
    workspace_model::{
        ModelEdit,
        views::{Change, Pane, Scope},
    },
};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    socket: PathBuf,
    scope: Scope,
    revision: u64,
    before: Pane,
    pane: Pane,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .ok_or("usage: enrich-shared-terminal-hint REQUEST.json")?;
    if args.next().is_some() {
        return Err("usage: enrich-shared-terminal-hint REQUEST.json".into());
    }
    let request: Request = serde_json::from_slice(&std::fs::read(path)?)?;
    request.pane.validate()?;
    let (CardKind::Terminal(before), CardKind::Terminal(after)) =
        (&request.before.kind, &request.pane.kind)
    else {
        return Err("hint enrichment requires terminal panes".into());
    };
    let hint = after
        .resume_hint
        .as_deref()
        .ok_or("enrichment must retain a historical hint")?;
    let recipe =
        agent_recovery::observed_launch_from_hint(hint).ok_or("invalid historical resume hint")?;
    if let Some(existing) = before.resume_hint.as_deref() {
        if agent_recovery::observed_launch_from_hint(existing).as_ref() != Some(&recipe) {
            return Err("hint enrichment cannot replace an exact provider thread".into());
        }
    }
    let mut expected = request.before.clone();
    let CardKind::Terminal(terminal) = &mut expected.kind else {
        unreachable!()
    };
    terminal.resume_hint.clone_from(&after.resume_hint);
    if expected != request.pane {
        return Err("hint enrichment cannot change any other pane field".into());
    }
    let mut client = ControlClient::connect(&request.socket)?;
    let model = client.read_workspace_model()?;
    let view = model
        .views
        .iter()
        .find(|view| view.scope == request.scope)
        .ok_or("missing shared scope")?;
    if view.revision != request.revision || !view.panes.contains(&request.before) {
        return Err("shared view changed since this request was reviewed".into());
    }
    client.edit_workspace_model(ModelEdit::ChangeView {
        scope: request.scope,
        expected_revision: request.revision,
        operation: uuid::Uuid::new_v4(),
        change: Change::Update {
            before: request.before,
            pane: request.pane.clone(),
        },
    })?;
    println!(
        "enriched shared pane {} without actor changes",
        request.pane.id
    );
    Ok(())
}
