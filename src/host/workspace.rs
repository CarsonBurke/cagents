//! Workspace SQL owns its own serialized connection. HostState is borrowed only
//! for resident admission, subscription and publication, never for layout SQL.
use super::{
    model::{DeltaApplied, Registry},
    state::{ClientId, SharedHost},
};
use crate::{
    model::{CardId, CardKind, TerminalId, TerminalSession},
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
    workspace_model::{
        ModelEdit, WorkspaceModel,
        views::{Change, Pane, Scope},
    },
};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

#[cfg(all(test, target_os = "linux"))]
mod pending_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! {
    static BEFORE_PENDING_START: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn pending_start_test_hook() {
    let hook = BEFORE_PENDING_START.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

fn unavailable(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::Internal, message, false)
}

pub(super) fn writer(host: &SharedHost) -> Result<Arc<Mutex<Registry>>, ProtocolError> {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .workspace_registry
        .clone()
        .ok_or_else(|| unavailable("The workspace model writer is unavailable"))
}

/// Subscribe before reading. Concurrent execution edits are either in this
/// coherent snapshot or queued as edges; no commit falls between the two.
pub(super) fn snapshot(
    host: &SharedHost,
    watcher: Option<ClientId>,
) -> Result<WorkspaceModel, ProtocolError> {
    let writer = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let writer = state
            .workspace_registry
            .clone()
            .ok_or_else(|| unavailable("The workspace model writer is unavailable"))?;
        if let Some(watcher) = watcher {
            state.subscribe_model(watcher);
        }
        writer
    };
    let registry = writer.lock().unwrap_or_else(|p| p.into_inner());
    registry.model().map_err(unavailable)
}

/// Initial closed-identity inventory is read in the model's WAL snapshot.
pub(super) fn subscriber_snapshot(
    host: &SharedHost,
    watcher: Option<ClientId>,
) -> Result<(WorkspaceModel, Vec<CardId>), ProtocolError> {
    let writer = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let writer = state
            .workspace_registry
            .clone()
            .ok_or_else(|| unavailable("The workspace model writer is unavailable"))?;
        if let Some(watcher) = watcher {
            state.subscribe_model(watcher);
        }
        writer
    };
    writer
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .subscriber_snapshot()
        .map_err(unavailable)
}

pub(super) fn metadata(
    host: &SharedHost,
    query: crate::workspace_model::metadata::Query,
) -> Result<crate::workspace_model::metadata::Reply, ProtocolError> {
    let writer = writer(host)?;
    let registry = writer.lock().unwrap_or_else(|p| p.into_inner());
    registry.metadata(query).map_err(unavailable)
}

/// One scope and the model sequence it is consistent with.
pub(super) fn view(
    host: &SharedHost,
    scope: &Scope,
) -> Result<(Option<crate::workspace_model::views::View>, u64), ProtocolError> {
    let writer = writer(host)?;
    let registry = writer.lock().unwrap_or_else(|p| p.into_inner());
    registry.view_snapshot(scope).map_err(unavailable)
}

/// Called by the request handler while it retains shared control admission.
/// Explicit close and automatic retention need exclusive admission to reserve
/// retirement, so the resident check stays valid through commit/publication.
pub(super) fn edit_delta(
    host: &SharedHost,
    origin: ClientId,
    edit: ModelEdit,
) -> Result<crate::workspace_model::delta::EditReceipt, ProtocolError> {
    edit_then(host, origin, edit, |_| Ok(())).map(|(receipt, ())| receipt)
}

/// Commit and publish an edit, then run `then` before the writer is released,
/// so it sees this edit and no later one.
fn edit_then<T>(
    host: &SharedHost,
    origin: ClientId,
    edit: ModelEdit,
    then: impl FnOnce(&Registry) -> Result<T, ProtocolError>,
) -> Result<(crate::workspace_model::delta::EditReceipt, T), ProtocolError> {
    let edit = edit
        .canonicalized()
        .map_err(|error| ProtocolError::new(ProtocolErrorCode::InvalidRequest, error, false))?;
    let writer = writer(host)?;
    // Always acquire this mutex outside HostState. Retain it through
    // publication so concurrent layout requests publish in commit order.
    let mut registry = writer.lock().unwrap_or_else(|p| p.into_inner());
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.ensure_view_retention_available(&edit)?;
        if matches!(&edit, ModelEdit::SuggestConversationTitle { .. }) {
            // Inference authorizes against current resident chat metadata.
            // Preserve its original identity/SQL guard; this is not a pane edit.
            let receipt = state.edit_model_delta(origin, edit)?;
            drop(state);
            return Ok((receipt, then(&registry)?));
        }
    }
    let replay = match &edit {
        ModelEdit::ChangeView { operation, .. } => registry
            .view_operation_seen(*operation)
            .map_err(unavailable)?,
        _ => false,
    };
    // A replayed operation returns its original result without re-checking later pane ownership.
    if !replay && super::chat::pane_start::requires_edit_check(&edit) {
        super::chat::pane_start::validate_edit(host, &registry, &edit)?;
    }
    let applied = if matches!(
        &edit,
        ModelEdit::ChangeView {
            change: Change::RemoveSource { .. } | Change::UpdateTerminalSource { .. },
            ..
        }
    ) {
        apply_delta_guarded(&mut registry, &edit, |seen, sql| {
            if seen {
                return Ok(None);
            }
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.validate_view_removal(&edit)?;
            if Registry::unbound_terminal_update(sql, &edit)? {
                let ModelEdit::ChangeView {
                    change: Change::UpdateTerminalSource { before, worker, .. },
                    ..
                } = &edit
                else {
                    unreachable!()
                };
                let CardKind::Terminal(source) = &before.kind else {
                    return Err(Registry::VIEW_REMOVAL_CONFLICT.into());
                };
                if !state.current_metadata_worker(source.id, worker) {
                    return Err(Registry::VIEW_REMOVAL_CONFLICT.into());
                }
            }
            Ok(Some(state))
        })?
    } else {
        apply_delta(&mut registry, &edit)?
    };
    if applied.changed {
        let waiting = Instant::now();
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let acquired = Instant::now();
        state.announce_model_edit(
            origin,
            applied.receipt.sequence,
            &edit,
            &applied.receipt.effects,
        );
        drop(state);
        tracing::debug!(
            operation = "workspace-model-edit",
            sequence = applied.receipt.sequence,
            host_lock_wait_us = acquired.duration_since(waiting).as_micros() as u64,
            host_lock_hold_us = acquired.elapsed().as_micros() as u64,
            "Published a workspace model edit"
        );
    }
    let then = then(&registry)?;
    Ok((applied.receipt, then))
}

/// Older CLI clients and windows that edit with full replies retain the
/// whole-model reply, read before another edit can follow this one. Edits
/// through `edit_delta` never construct it.
pub(super) fn edit(
    host: &SharedHost,
    origin: ClientId,
    edit: ModelEdit,
) -> Result<WorkspaceModel, ProtocolError> {
    edit_then(host, origin, edit, |registry| {
        registry.model().map_err(unavailable)
    })
    .map(|(_, model)| model)
}

pub(super) fn apply_delta(
    registry: &mut Registry,
    edit: &ModelEdit,
) -> Result<DeltaApplied, ProtocolError> {
    apply_delta_guarded(registry, edit, |_, _| Ok(()))
}

fn apply_delta_guarded<G>(
    registry: &mut Registry,
    edit: &ModelEdit,
    guard: impl FnOnce(bool, &rusqlite::Connection) -> Result<G, String>,
) -> Result<DeltaApplied, ProtocolError> {
    if let ModelEdit::ChangeView {
        scope,
        expected_revision,
        operation,
        change,
    } = edit
    {
        let seen = registry
            .view_operation_seen(*operation)
            .map_err(unavailable)?;
        if !seen {
            if let Some(reason) = registry
                .view_change_refusal(scope, change)
                .map_err(unavailable)?
            {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    reason,
                    false,
                ));
            }
            if registry.view_revision(scope).map_err(unavailable)? != *expected_revision {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::ViewConflict,
                    "The workspace view changed; rebase this pane mutation on the current revision",
                    true,
                ));
            }
        }
    }
    registry.apply_delta_guarded(edit, guard).map_err(|error| {
        if error == Registry::VIEW_REMOVAL_CONFLICT {
            return ProtocolError::new(ProtocolErrorCode::InvalidRequest, error, false);
        }
        // Stale-precondition refusals the client resolves by reading again,
        // the confirmed project close's channel set among them.
        if error == Registry::VIEW_SOURCE_CONFLICT
            || error == Registry::VIEW_REVISION_CONFLICT
            || error == Registry::PROJECT_CHANNELS_CONFLICT
        {
            return ProtocolError::new(ProtocolErrorCode::ViewConflict, error, true);
        }
        // Execution retirement may close the scope after the preflight read.
        if let ModelEdit::ChangeView { scope, change, .. } = edit {
            match registry.view_change_refusal(scope, change) {
                Ok(Some(reason)) => {
                    return ProtocolError::new(ProtocolErrorCode::InvalidRequest, reason, false);
                }
                Err(storage) => return unavailable(storage),
                Ok(None) => {}
            }
        }
        unavailable(error)
    })
}

/// An automatic terminal's pane, as the daemon accepted it.
#[derive(Clone)]
pub(super) struct PendingTerminal {
    pub(super) session: TerminalSession,
    pane: Pane,
    scope: Scope,
    #[cfg(test)]
    revision: u64,
    #[cfg(test)]
    operation: uuid::Uuid,
}

impl PendingTerminal {
    pub(super) fn validate(
        &self,
        admission: &super::model::terminal_sources::SourceAdmission<'_>,
    ) -> Result<(), ProtocolError> {
        admission
            .validate_pending(&self.scope, &self.pane)
            .map_err(|_| {
                ProtocolError::new(
                    ProtocolErrorCode::ViewConflict,
                    Registry::VIEW_SOURCE_CONFLICT,
                    true,
                )
            })
    }

    pub(super) fn pane_is_pending(&self) -> bool {
        self.pane.pending_terminal_start
    }

    pub(super) fn accepts(&self, session: &TerminalSession) -> bool {
        self.pane.pending_terminal_start && self.session == *session
    }

    /// Execution writes use another connection. Read the pane and its revision
    /// together; the next transaction fences any change after this snapshot.
    #[cfg(test)]
    pub(super) fn current_revision(&self, registry: &Registry) -> Result<u64, ProtocolError> {
        let (view, _) = registry.view_snapshot(&self.scope).map_err(unavailable)?;
        if let Some(view) = view
            && !view.retired.contains(&self.pane.id)
            && view.panes.iter().find(|pane| pane.id == self.pane.id) == Some(&self.pane)
        {
            return Ok(view.revision);
        }
        Err(ProtocolError::new(
            ProtocolErrorCode::ViewConflict,
            Registry::VIEW_SOURCE_CONFLICT,
            true,
        ))
    }
}

/// Reads only the daemon's accepted reservation, never a client's launch recipe.
pub(super) fn pending_terminal(
    registry: &Registry,
    pane: CardId,
    terminal_id: TerminalId,
) -> Result<PendingTerminal, ProtocolError> {
    if let Some(view) = registry.pane_view(pane).map_err(unavailable)? {
        for candidate in view.panes {
            if candidate.id == pane
                && let CardKind::Terminal(session) = &candidate.kind
                && session.id == terminal_id
            {
                return Ok(PendingTerminal {
                    session: session.clone(),
                    pane: candidate,
                    scope: view.scope,
                    #[cfg(test)]
                    revision: view.revision,
                    #[cfg(test)]
                    operation: uuid::Uuid::new_v4(),
                });
            }
        }
    }
    Err(ProtocolError::new(
        ProtocolErrorCode::TerminalNotFound,
        format!("terminal {terminal_id} was not found"),
        false,
    ))
}

/// Saves the published worker's source into its reserved pane. The caller
/// holds writer admission from the reservation check; HostState is free.
#[cfg(test)]
pub(super) fn complete_terminal(
    host: &SharedHost,
    registry: &mut Registry,
    reservation: PendingTerminal,
    session: TerminalSession,
) -> Result<(), ProtocolError> {
    let mut pane = reservation.pane.clone();
    pane.pending_terminal_start = false;
    pane.kind = CardKind::Terminal(session);
    let mut edit = ModelEdit::ChangeView {
        scope: reservation.scope.clone(),
        expected_revision: reservation.revision,
        operation: reservation.operation,
        change: Change::Update {
            before: reservation.pane.clone(),
            pane,
        },
    };
    let mut conflicts = 0;
    loop {
        match apply_delta(registry, &edit) {
            Err(error) if error.code == ProtocolErrorCode::ViewConflict && conflicts < 2 => {
                // Rebase only unrelated changes, never a new source, edited
                // presentation, transferred pane or withdrawn reservation.
                let revision = reservation.current_revision(registry)?;
                if let ModelEdit::ChangeView {
                    expected_revision, ..
                } = &mut edit
                {
                    *expected_revision = revision;
                }
                conflicts += 1;
            }
            result => {
                let applied = result?;
                if applied.changed {
                    host.lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .announce_model_edit(
                            0,
                            applied.receipt.sequence,
                            &edit,
                            &applied.receipt.effects,
                        );
                }
                return Ok(());
            }
        }
    }
}
