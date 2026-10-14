//! Moves panes between the shared views of one project.
//!
//! A move is one `Transfer` in the daemon's view registry. Windows re-home the
//! pane by its card id, so the session it hosts keeps running.

use super::*;
use crate::{
    model::{CardId, CardKind},
    workspace_model::views::{Change, Pane, Scope, View},
};

/// How many view revisions a move rebases across before giving up, matching
/// the window publisher.
const CONFLICT_RETRIES: u32 = 8;

/// The id a pane hosts, which is the id `cagents sessions` reports.
fn hosted(kind: &CardKind) -> Option<uuid::Uuid> {
    match kind {
        CardKind::Chat(session) => Some(session.id.as_uuid()),
        CardKind::Terminal(session) => Some(session.id.as_uuid()),
        CardKind::Browser(session) => Some(session.id.as_uuid()),
        _ => None,
    }
}

/// The one shared pane `id` names, by its own id (an agent's id is its pane's)
/// or by the session it hosts.
fn locate(model: &WorkspaceModel, id: uuid::Uuid) -> Result<(&View, &Pane), AppError> {
    let mut found = model.views.iter().flat_map(|view| {
        view.panes
            .iter()
            .filter(move |pane| pane.id.as_uuid() == id || hosted(&pane.kind) == Some(id))
            .map(move |pane| (view, pane))
    });
    let first = found
        .next()
        .ok_or_else(|| AppError::new("target", format!("No shared pane, session or agent {id}")))?;
    if found.any(|(_, pane)| pane.id != first.1.id) {
        return Err(AppError::new(
            "target",
            format!("{id} names more than one pane; use the pane id"),
        ));
    }
    Ok(first)
}

/// The project a view belongs to, if the model still has it.
fn project(model: &WorkspaceModel, scope: &Scope) -> Option<PathBuf> {
    match scope {
        Scope::Channel(id) => model.channel(*id).map(|channel| channel.repository.clone()),
        Scope::Worktree { repository, .. } => Some(repository.clone()),
    }
}

fn describe(scope: &Scope) -> String {
    match scope {
        Scope::Channel(id) => id.to_string(),
        Scope::Worktree { path, .. } => path.display().to_string(),
    }
}

#[derive(Debug, PartialEq)]
struct Plan {
    pane: CardId,
    source: Scope,
    revision: u64,
    /// `None` when the pane already sits where it was asked to go.
    change: Option<Change>,
}

/// Decides one move against one model read, refusing a move across projects
/// before the daemon would.
fn plan(
    model: &WorkspaceModel,
    pane: uuid::Uuid,
    to: ChannelId,
    before: Option<uuid::Uuid>,
) -> Result<Plan, AppError> {
    let destination = model
        .channel(to)
        .ok_or_else(|| AppError::new("target", format!("No channel {to}")))?;
    let (view, moving) = locate(model, pane)?;
    let owner = project(model, &view.scope).ok_or_else(|| {
        AppError::new(
            "target",
            format!("{} belongs to a view the model no longer has", moving.id),
        )
    })?;
    if owner != destination.repository {
        return Err(AppError::new(
            "target",
            format!(
                "{} belongs to {}, but channel {to} belongs to {}; panes move only within one project",
                moving.id,
                owner.display(),
                destination.repository.display()
            ),
        ));
    }
    let target = Scope::Channel(to);
    let before = before
        .map(|before| {
            let (view, anchor) = locate(model, before)?;
            if view.scope != target {
                return Err(AppError::new(
                    "target",
                    format!("{before} is not in channel {to}"),
                ));
            }
            if anchor.id == moving.id {
                return Err(AppError::new(
                    "target",
                    "A pane cannot be placed before itself",
                ));
            }
            Ok(anchor.id)
        })
        .transpose()?;
    let change = if view.scope != target {
        Some(Change::Transfer {
            id: moving.id,
            destination: target,
            before,
        })
    } else {
        before.map(|before| Change::Move {
            id: moving.id,
            before: Some(before),
        })
    };
    Ok(Plan {
        pane: moving.id,
        source: view.scope.clone(),
        revision: view.revision,
        change,
    })
}

pub(super) fn move_pane(
    client: &mut ControlClient,
    pane: uuid::Uuid,
    to: ChannelId,
    before: Option<uuid::Uuid>,
) -> Result<CommandOutput, AppError> {
    // One operation across rebases: a conflict is refused before the registry
    // records it, and a replay of an applied one changes nothing.
    let operation = uuid::Uuid::new_v4();
    let mut conflicts = 0;
    loop {
        let model = client.read_workspace_model().map_err(protocol_error)?;
        let plan = plan(&model, pane, to, before)?;
        let Some(change) = plan.change.clone() else {
            return Ok(output(&plan, false, model.sequence));
        };
        match client.edit_workspace_model(ModelEdit::ChangeView {
            scope: plan.source.clone(),
            expected_revision: plan.revision,
            operation,
            change,
        }) {
            Ok(model) => {
                // The registry treats a destination deleted mid-move as a
                // no-op, so success is read from where the pane is now.
                let (view, _) = locate(&model, plan.pane.as_uuid())?;
                if view.scope != Scope::Channel(to) {
                    return Err(AppError::new(
                        "target",
                        format!(
                            "{} stayed in {}; channel {to} changed during the move",
                            plan.pane,
                            describe(&view.scope)
                        ),
                    ));
                }
                return Ok(output(&plan, true, model.sequence));
            }
            Err(ControlError::Remote {
                code: ProtocolErrorCode::ViewConflict,
                ..
            }) if conflicts < CONFLICT_RETRIES => {
                conflicts += 1;
                std::thread::sleep(Duration::from_millis(20 * u64::from(conflicts)));
            }
            Err(error) => return Err(protocol_error(error)),
        }
    }
}

fn output(plan: &Plan, moved: bool, sequence: u64) -> CommandOutput {
    let to = match &plan.change {
        Some(Change::Transfer { destination, .. }) => destination.clone(),
        _ => plan.source.clone(),
    };
    let human = match &plan.change {
        Some(Change::Transfer { .. }) => format!(
            "moved {} from {} to {}",
            plan.pane,
            describe(&plan.source),
            describe(&to)
        ),
        Some(_) => format!("reordered {} in {}", plan.pane, describe(&to)),
        None => format!("{} is already in {}", plan.pane, describe(&to)),
    };
    CommandOutput {
        command: "channel.move",
        data: json!({
            "pane": plan.pane,
            "from": plan.source,
            "to": to,
            "moved": moved,
            "sequence": sequence,
        }),
        human,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BrowserId, BrowserSession};

    fn pane() -> Pane {
        Pane {
            id: CardId::new(),
            title: "browser".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            scope: None,
            kind: CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: "https://example.com".parse().unwrap(),
                history: vec![],
                history_index: 0,
                scroll_x: 0.,
                scroll_y: 0.,
                fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
            }),
        }
    }

    fn channel(repository: &str) -> ModelChannel {
        ModelChannel {
            id: ChannelId::new(),
            repository: repository.into(),
            title: String::new(),
            title_source: Default::default(),
            ordinal: 0,
        }
    }

    fn view(channel: &ModelChannel, panes: Vec<Pane>) -> View {
        View {
            scope: Scope::Channel(channel.id),
            revision: 3,
            panes,
            retired: vec![],
        }
    }

    #[test]
    fn a_move_names_the_pane_by_its_id_or_its_session_and_transfers_it() {
        let (source, destination) = (channel("/repo"), channel("/repo"));
        let (moving, anchor) = (pane(), pane());
        let model = WorkspaceModel {
            views: vec![
                view(&source, vec![moving.clone()]),
                view(&destination, vec![anchor.clone()]),
            ],
            channels: vec![source.clone(), destination.clone()],
            ..Default::default()
        };
        let session = hosted(&moving.kind).unwrap();
        for name in [moving.id.as_uuid(), session] {
            assert_eq!(
                plan(&model, name, destination.id, Some(hosted(&anchor.kind).unwrap())).unwrap(),
                Plan {
                    pane: moving.id,
                    source: Scope::Channel(source.id),
                    revision: 3,
                    change: Some(Change::Transfer {
                        id: moving.id,
                        destination: Scope::Channel(destination.id),
                        before: Some(anchor.id),
                    }),
                }
            );
        }
    }

    #[test]
    fn a_pane_already_in_place_is_left_alone_unless_reordered() {
        let destination = channel("/repo");
        let (moving, anchor) = (pane(), pane());
        let model = WorkspaceModel {
            views: vec![view(&destination, vec![anchor.clone(), moving.clone()])],
            channels: vec![destination.clone()],
            ..Default::default()
        };
        let plan_for = |before| plan(&model, moving.id.as_uuid(), destination.id, before);
        assert_eq!(plan_for(None).unwrap().change, None);
        assert_eq!(
            plan_for(Some(anchor.id.as_uuid())).unwrap().change,
            Some(Change::Move {
                id: moving.id,
                before: Some(anchor.id),
            })
        );
        assert!(plan_for(Some(moving.id.as_uuid())).is_err());
    }

    #[test]
    fn a_move_is_refused_across_projects_or_to_unknown_targets() {
        let (source, foreign, sibling) = (channel("/repo"), channel("/other"), channel("/repo"));
        let (moving, stranger) = (pane(), pane());
        let model = WorkspaceModel {
            views: vec![
                view(&source, vec![moving.clone()]),
                view(&foreign, vec![stranger.clone()]),
            ],
            channels: vec![source.clone(), foreign.clone(), sibling.clone()],
            ..Default::default()
        };
        let id = moving.id.as_uuid();
        let refused = plan(&model, id, foreign.id, None).unwrap_err();
        assert!(refused.message.contains("only within one project"));
        assert!(plan(&model, id, ChannelId::new(), None).is_err());
        assert!(plan(&model, uuid::Uuid::new_v4(), sibling.id, None).is_err());
        assert!(
            plan(&model, id, sibling.id, Some(stranger.id.as_uuid())).is_err(),
            "the anchor has to be in the destination"
        );
    }
}
