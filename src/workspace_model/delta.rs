//! What an applied edit did beyond what the edit says, and the subscriber's
//! copy of the views that it keeps current from those edges.
//!
//! A watcher used to answer every view edge by reading the whole model. An
//! edge now carries, for each scope the edit changed, the revision before and
//! after it. Revisions come from one model-wide clock, so they are not
//! consecutive per scope: a subscriber may apply an edit to its copy of a view
//! only when the revision it holds is the edit's pre-edit revision. Otherwise
//! it reads that one scope.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{
    ModelChannel, ModelEdit,
    views::{self, Change, Scope, View},
};
use crate::model::CardId;

/// One scope's revision before and after an edit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewRevision {
    pub scope: Scope,
    /// `None` when the edit created the scope.
    pub before: Option<u64>,
    /// `None` when the edit deleted the scope, with a channel or a project.
    pub after: Option<u64>,
}

/// Everything an applied edit changed that the edit alone does not determine.
///
/// Empty for an edit that changed no view, no channel row beyond what it
/// names, and no agent. An `AgentsChanged` edge also carries nothing: the
/// execution service changes views as it adopts, rebinds and retires agents,
/// and the subscriber reads the whole model for it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EditEffects {
    /// Every scope whose revision the edit changed, created or deleted. A
    /// transfer names its source and its destination.
    pub views: Vec<ViewRevision>,
    /// Panes the edit closed: removed from their view rather than moved to
    /// another one, including every pane of a deleted channel or closed
    /// project. A transferred pane is not closed.
    pub closed: Vec<CardId>,
    /// Channel rows as the edit left them, when it changed one in a way the
    /// edit does not say: a new primary conversation resets an automatic
    /// title, and a conversation title suggestion is conditional on the
    /// stored title.
    pub channels: Vec<ModelChannel>,
    /// Agent records as the edit left them: transferring an agent's pane moves
    /// the agent's placement with it.
    pub agents: Vec<crate::agent_execution::Agent>,
}

impl EditEffects {
    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
            && self.closed.is_empty()
            && self.channels.is_empty()
            && self.agents.is_empty()
    }
}

/// The views of one edge, as complete views rather than as an edit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewChanges {
    /// Each changed scope's complete view after the edge. A view a subscriber
    /// already holds at a later revision is newer than this one.
    pub views: Vec<View>,
    /// Scopes the edge deleted, with the revision they had. A view held at a
    /// later revision is a later incarnation of the scope and stays.
    pub removed: Vec<(Scope, u64)>,
}

impl ViewChanges {
    pub fn is_empty(&self) -> bool {
        self.views.is_empty() && self.removed.is_empty()
    }
}

/// How one edge applied to a [`Mirror`].
#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    pub changes: ViewChanges,
    /// Scopes whose new content the edge does not determine for this copy;
    /// read each with `ReadWorkspaceView` and [`Mirror::install`] it.
    pub unresolved: Vec<Scope>,
}

/// A subscriber's copy of every view, kept current by applying edges.
///
/// It holds exactly what the registry held after the last edge it applied, so
/// a revision mismatch means an edge is missing or the daemon changed a view
/// without one, and the scope is read rather than guessed.
#[derive(Debug, Default)]
pub struct Mirror {
    views: HashMap<Scope, View>,
}

impl Mirror {
    pub fn new(views: &[View]) -> Self {
        Self {
            views: views
                .iter()
                .map(|view| (view.scope.clone(), view.clone()))
                .collect(),
        }
    }

    pub fn revision(&self, scope: &Scope) -> Option<u64> {
        self.views.get(scope).map(|view| view.revision)
    }

    /// Every view this copy holds, in no particular order.
    pub fn views(&self) -> impl Iterator<Item = &View> {
        self.views.values()
    }

    /// Applies one edge's view effects.
    ///
    /// Every scope the edge names is reported with the newest view this copy
    /// holds, even one a read already brought past the edge: the subscriber
    /// may have installed an older snapshot since, and the edge is its only
    /// prompt to take the newer view.
    pub fn apply(&mut self, edit: &ModelEdit, effects: &EditEffects) -> Applied {
        let mut applied = Applied::default();
        let mut changed = Vec::new();
        for revision in &effects.views {
            let held = self.revision(&revision.scope);
            match (revision.before, revision.after) {
                (Some(before), None) => {
                    applied
                        .changes
                        .removed
                        .push((revision.scope.clone(), before));
                    // A later incarnation of a recreated worktree scope stays.
                    match held {
                        Some(held) if held <= before => {
                            self.views.remove(&revision.scope);
                        }
                        Some(_) => applied.changes.views.push(self.views[&revision.scope].clone()),
                        None => {}
                    }
                }
                (_, Some(after)) if held.is_some_and(|held| held >= after) => {
                    applied.changes.views.push(self.views[&revision.scope].clone());
                }
                (_, Some(after)) => changed.push((revision, after)),
                (None, None) => {}
            }
        }
        if changed.is_empty() {
            return applied;
        }
        let derived = match edit {
            ModelEdit::ChangeView { scope, change, .. } => {
                self.derive(scope, change, &changed, &effects.closed)
            }
            _ => None,
        };
        match derived {
            Some(views) => {
                for view in views {
                    self.views.insert(view.scope.clone(), view.clone());
                    applied.changes.views.push(view);
                }
            }
            None => {
                applied.unresolved = changed
                    .into_iter()
                    .map(|(revision, _)| revision.scope.clone())
                    .collect();
            }
        }
        applied
    }

    /// The views `change` leaves, if this copy holds exactly what the registry
    /// changed: every scope at its pre-edit revision. A scope the edit created
    /// is not derived, except as a transfer's destination: an insert into it
    /// may have been refused for a resource another view holds.
    fn derive(
        &self,
        scope: &Scope,
        change: &Change,
        changed: &[(&ViewRevision, u64)],
        closed: &[CardId],
    ) -> Option<Vec<View>> {
        let held = |revision: &ViewRevision| -> Option<View> {
            match (revision.before, self.views.get(&revision.scope)) {
                (Some(before), Some(view)) if view.revision == before => Some(view.clone()),
                _ => None,
            }
        };
        let views = match change {
            Change::Transfer {
                id,
                destination,
                before,
            } => {
                let [first, second] = changed else {
                    return None;
                };
                let (source, target) = if &first.0.scope == scope {
                    (first, second)
                } else {
                    (second, first)
                };
                if &source.0.scope != scope || &target.0.scope != destination {
                    return None;
                }
                let target_view = match target.0.before {
                    None if !self.views.contains_key(destination) => {
                        View::empty(destination.clone())
                    }
                    _ => held(target.0)?,
                };
                let mut views = vec![held(source.0)?, target_view];
                if !views[0].panes.iter().any(|pane| pane.id == *id) {
                    return None;
                }
                views::transfer(&mut views, scope, *id, destination, *before).ok()?;
                views[0].revision = source.1;
                views[1].revision = target.1;
                views
            }
            _ => {
                let [(revision, after)] = changed else {
                    return None;
                };
                if &revision.scope != scope {
                    return None;
                }
                let mut view = held(revision)?;
                view.apply(change).ok()?;
                view.revision = *after;
                vec![view]
            }
        };
        // The registry closed exactly these; a copy that disagrees is read.
        let retired = |id: &CardId| {
            views
                .iter()
                .all(|view| view.panes.iter().all(|pane| pane.id != *id))
                && views.iter().any(|view| view.retired.contains(id))
        };
        if !closed.iter().all(retired) {
            return None;
        }
        Some(views)
    }

    /// Records a scope as read, unless this copy already holds it at a later
    /// revision. Returns what the subscriber should install: the newest view
    /// this copy holds, or the removal of the one it held.
    pub fn install(&mut self, scope: Scope, view: Option<View>) -> ViewChanges {
        match view {
            Some(view) => {
                if self
                    .revision(&scope)
                    .is_none_or(|held| held < view.revision)
                {
                    self.views.insert(scope.clone(), view);
                }
                ViewChanges {
                    views: vec![self.views[&scope].clone()],
                    removed: Vec::new(),
                }
            }
            None => ViewChanges {
                views: Vec::new(),
                removed: self
                    .views
                    .remove(&scope)
                    .map(|view| (scope, view.revision))
                    .into_iter()
                    .collect(),
            },
        }
    }
}

#[cfg(test)]
mod tests;
