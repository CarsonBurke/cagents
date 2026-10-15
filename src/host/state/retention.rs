//! A retirement reservation fences new attachment/pinning until the exact
//! cleanup result arrives. Failure keeps the listed worker and explicit retry
//! path, without leaving an overdue automatic-retention timer armed.
use super::*;
use std::sync::mpsc::Receiver;

pub(in crate::host) struct Retirement {
    pub(in crate::host) handle: TerminalHandle,
    pub(in crate::host) ticket: uuid::Uuid,
    pub(in crate::host) result: Option<Receiver<crate::terminal::CompletedRelease>>,
    pub(in crate::host) failure: Option<String>,
}
impl Retirement {
    pub(in crate::host) fn submitting(handle: TerminalHandle) -> Self {
        Self { handle, ticket: uuid::Uuid::new_v4(), result: None, failure: None }
    }
    pub(in crate::host) fn pending(&self) -> bool { self.failure.is_none() }
}
impl HostState {
    pub(in crate::host) fn retention_blocked(&self, id: TerminalId, handle: &TerminalHandle) -> bool {
        self.terminal_retirements.get(&id)
            .is_some_and(|retirement| retirement.handle.same_worker(handle))
    }
    pub(in crate::host) fn ensure_retention_available(&self, ids: &[TerminalId]) -> Result<(), ProtocolError> {
        if self.terminal_retirements.is_empty() { return Ok(()); }
        if let Some(id) = ids.iter().find(|id| self.terminal_retirements.get(*id).is_some_and(|retirement| {
            retirement.pending() && self.terminals.get(*id)
                .is_some_and(|record| record.handle.same_worker(&retirement.handle))
        })) {
            return Err(ProtocolError::new(ProtocolErrorCode::TerminalUnavailable,
                format!("Terminal {id} is completing retirement; retry after cleanup settles"), true));
        }
        Ok(())
    }
    pub(in crate::host) fn ensure_view_retention_available(&self, edit: &ModelEdit) -> Result<(), ProtocolError> {
        if self.terminal_retirements.is_empty() { return Ok(()); }
        let panes: &[crate::workspace_model::views::Pane] = match edit {
            ModelEdit::SeedView { panes, .. } => panes,
            ModelEdit::ChangeView { change, .. } => match change {
                crate::workspace_model::views::Change::Insert { pane, .. }
                | crate::workspace_model::views::Change::InsertIfEmpty { pane }
                | crate::workspace_model::views::Change::Update { pane, .. } => std::slice::from_ref(pane),
                _ => &[],
            },
            _ => &[],
        };
        for pane in panes {
            match &pane.kind {
                crate::model::CardKind::Terminal(session) => self.ensure_retention_available(&[session.id])?,
                crate::model::CardKind::Chat(session) => {
                    self.ensure_retention_available(&[session.id])?;
                    if let Some(terminal) = &session.terminal { self.ensure_retention_available(&[terminal.id])?; }
                }
                _ => {},
            }
        }
        Ok(())
    }
}
