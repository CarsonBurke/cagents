//! One-shot terminal publication outcomes for the private native evidence UI.

use super::*;
use crate::workspace_model::views::{Change, Pane, Scope};

pub(super) const UNQUEUED: &str = ".cagents-fixture-terminal-insert-unqueued";
const REFUSED: &str = ".cagents-fixture-terminal-insert-refused";
const FENCED: &str = ".cagents-fixture-terminal-insert-fenced";

/// Only a new terminal's Insert consumes a fixture marker.
pub(super) fn terminal_insert(edit: &ModelEdit) -> Option<(&Scope, uuid::Uuid, &Pane)> {
    let ModelEdit::ChangeView {
        scope,
        operation,
        change: Change::Insert { pane, .. },
        ..
    } = edit
    else {
        return None;
    };
    (pane.pending_terminal_start && matches!(pane.kind, CardKind::Terminal(_)))
        .then_some((scope, *operation, pane))
}

pub(super) fn take(marker: &str) -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    let Ok(home) = std::fs::canonicalize(home) else {
        return false;
    };
    take_in(&home, marker)
}

fn take_in(home: &Path, marker: &str) -> bool {
    home.ancestors()
        .skip(1)
        .any(|path| path.join(".native-evidence").is_file())
        && std::fs::remove_file(home.join(marker)).is_ok()
}

/// Sends the same update a terminal publication would receive in this case.
/// The stand-in daemon and other windows keep their actual workspace model.
pub(super) fn update(
    request: &ModelRequest,
    client: &mut ControlClient,
) -> Result<Option<ModelUpdate>, String> {
    let Some((scope, operation, pane)) = terminal_insert(&request.edit) else {
        return Ok(None);
    };
    if request.fixture_unchanged_edge {
        let (model, _) = client
            .read_workspace_state()
            .map_err(|error| error.to_string())?;
        // An observer echo carries no changed rows. Processing it exercises
        // the same reconciliation as a duplicate receipt or an older edge.
        return Ok(Some(ModelUpdate::Changed {
            sequence: model.sequence,
            edit: request.edit.clone(),
            changes: EdgeChanges {
                identity: Some(model.identity),
                ..EdgeChanges::default()
            },
        }));
    }
    if take(REFUSED) {
        tracing::info!(target: "cagents::workspace_projection", "Native fixture refused a terminal Insert");
        return Ok(Some(ModelUpdate::ViewRefused {
            scope: scope.clone(),
            operation,
            error: "The workspace refused to add this terminal".into(),
        }));
    }
    if take(FENCED) {
        let (mut model, _) = client
            .read_workspace_state()
            .map_err(|error| error.to_string())?;
        if !install_fenced_terminal(&mut model, scope, pane) {
            return Err("native fixture needs an admitted workspace view".into());
        }
        tracing::info!(target: "cagents::workspace_projection", "Native fixture fenced a terminal Insert");
        return Ok(Some(ModelUpdate::ViewApplied {
            scope: scope.clone(),
            operation: Some(operation),
            model,
        }));
    }
    Ok(None)
}

fn install_fenced_terminal(model: &mut WorkspaceModel, scope: &Scope, pane: &Pane) -> bool {
    use crate::agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target};

    let (Some(view), CardKind::Terminal(saved)) = (
        model.views.iter_mut().find(|view| &view.scope == scope),
        &pane.kind,
    ) else {
        return false;
    };
    // An admitted agent with the pane's identity owns a different terminal
    // session. The window must keep this pane from starting a duplicate.
    let running = fresh_terminal_session(saved.launch.clone());
    model.agents.push(Agent {
        id: crate::model::AgentId::from_uuid(pane.id.as_uuid()),
        name: None,
        origin: Origin::User,
        spec: Spec {
            provider: crate::agent::Provider::Claude,
            account: None,
            target: Target {
                repository: saved.launch.working_directory.clone(),
                directory: saved.launch.working_directory.clone(),
                placement: Some(scope.clone()),
            },
            surface: Surface::Terminal,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        },
        placement: scope.clone(),
        session: Some(HostedSession::Terminal(running.clone())),
        console: Some(running),
        created_ms: 0,
        closed: false,
    });
    view.panes.push(pane.clone());
    view.revision += 1;
    model.sequence += 1;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_fixture_consumes_a_marker_only_inside_a_native_task() {
        let task = tempfile::tempdir().unwrap();
        let home = task.path().join("fixture/home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(UNQUEUED), "").unwrap();
        assert!(!take_in(&home, UNQUEUED));
        std::fs::write(task.path().join(".native-evidence"), "").unwrap();
        assert!(take_in(&home, UNQUEUED));
        assert!(!take_in(&home, UNQUEUED));
    }
}
