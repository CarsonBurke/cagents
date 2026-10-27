//! Exact-worker reservations fence attachment while automatic cleanup runs.
use super::*;

pub(in crate::host) struct Retirement {
    pub(in crate::host) handle: TerminalHandle,
    pub(in crate::host) ticket: uuid::Uuid,
    pub(in crate::host) failure: Option<String>,
    // An explicit close owns its response and final compaction. Automatic
    // maintenance must not consume that request's exact ticket underneath it.
    pub(in crate::host) explicit_close: bool,
}

impl Retirement {
    pub(in crate::host) fn submitting(handle: TerminalHandle) -> Self {
        Self {
            handle,
            ticket: uuid::Uuid::new_v4(),
            failure: None,
            explicit_close: false,
        }
    }

    pub(in crate::host) fn explicit_close(handle: TerminalHandle) -> Self {
        Self {
            explicit_close: true,
            ..Self::submitting(handle)
        }
    }
}

impl HostState {
    pub(in crate::host) fn ensure_retention_available(
        &self,
        ids: &[TerminalId],
    ) -> Result<(), ProtocolError> {
        for id in ids {
            if self.terminal_retirements.get(id).is_some_and(|entry| {
                entry.failure.is_none()
                    && self
                        .terminals
                        .get(id)
                        .is_some_and(|record| record.handle.same_worker(&entry.handle))
            }) {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::TerminalUnavailable,
                    format!("Terminal {id} is completing retirement; retry when cleanup settles"),
                    true,
                ));
            }
        }
        Ok(())
    }

    pub(in crate::host) fn ensure_view_retention_available(
        &self,
        edit: &ModelEdit,
    ) -> Result<(), ProtocolError> {
        if self.terminal_retirements.is_empty() {
            return Ok(());
        }
        let panes: &[crate::workspace_model::views::Pane] = match edit {
            ModelEdit::SeedView { panes, .. } => panes,
            ModelEdit::ChangeView { change, .. } => match change {
                crate::workspace_model::views::Change::Insert { pane, .. }
                | crate::workspace_model::views::Change::InsertIfEmpty { pane }
                | crate::workspace_model::views::Change::Update { pane, .. } => {
                    std::slice::from_ref(pane)
                }
                _ => &[],
            },
            _ => &[],
        };
        for pane in panes {
            match &pane.kind {
                crate::model::CardKind::Terminal(session) => {
                    self.ensure_retention_available(&[session.id])?
                }
                crate::model::CardKind::Chat(session) => {
                    self.ensure_retention_available(&[session.id])?;
                    if let Some(terminal) = &session.terminal {
                        self.ensure_retention_available(&[terminal.id])?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(in crate::host) fn retire_terminal_if_current(
        &mut self,
        id: TerminalId,
        expected: &TerminalHandle,
    ) -> Option<(TerminalRecord, Option<Retirement>)> {
        if self
            .terminals
            .get(&id)
            .is_some_and(|record| record.handle.same_worker(expected))
        {
            let record = self.retire_terminal(id)?;
            let reservation = if self
                .terminal_retirements
                .get(&id)
                .is_some_and(|entry| entry.handle.same_worker(expected))
            {
                self.terminal_retirements.remove(&id)
            } else {
                None
            };
            Some((record, reservation))
        } else {
            None
        }
    }

    pub(in crate::host) fn observe_completed_terminals(&mut self, now: Instant) {
        for record in self.terminals.values_mut() {
            if record.handle.is_completed() {
                record.completed_at.get_or_insert(now);
            }
        }
    }

    fn retention_candidate(&self, id: TerminalId, record: &TerminalRecord) -> bool {
        !self.shutting_down
            && !self.shutdown_requested
            && self.upgrade.is_none()
            && record.handle.is_completed()
            // A failed startup still owns its original recovery slot. Keep the
            // record reachable until exact checked Close settles that custody.
            && !self.startup_custody.contains(&id)
            && !self.terminal_retirements.contains_key(&id)
            && self.subscriptions.clients(id).is_empty()
            && !self.chats.contains_key(&id)
            && !self.chat_handoffs.contains_key(&id)
            // Return-to-terminal receipts are keyed by the original chat,
            // while their replacement has its own terminal identity.
            && !self
                .chat_handoffs
                .values()
                .any(|receipt| receipt.replacement().id == id)
            && !self.chat_spawning.contains_key(&id)
            && !self.chat_bindings.contains_key(&id)
            && !self.native_closing.contains_key(&id)
            && !self.native_actor_startups.contains_key(&id)
            && self.native_adoption_failures.is_empty()
    }

    pub(in crate::host) fn terminal_retention_deadline(
        &self,
        retention: Duration,
        pinned: &BTreeSet<TerminalId>,
    ) -> Option<Instant> {
        self.terminals
            .iter()
            .filter(|(id, record)| !pinned.contains(id) && self.retention_candidate(**id, record))
            .filter_map(|(_, record)| record.completed_at?.checked_add(retention))
            .min()
    }

    pub(in crate::host) fn expired_terminal_handles(
        &self,
        now: Instant,
        retention: Duration,
        pinned: &BTreeSet<TerminalId>,
    ) -> Vec<(TerminalId, TerminalHandle)> {
        self.terminals
            .iter()
            .filter(|(id, record)| {
                !pinned.contains(id)
                    && self.retention_candidate(**id, record)
                    && record
                        .completed_at
                        .and_then(|at| at.checked_add(retention))
                        .is_some_and(|at| at <= now)
            })
            .map(|(id, record)| (*id, record.handle.clone()))
            .collect()
    }

    pub(in crate::host) fn development_idle(&self) -> bool {
        self.clients.is_empty()
            && self.terminals.is_empty()
            && self.terminal_retirements.is_empty()
            && self.spawning.is_empty()
            && self.browser_logins_spawning.is_empty()
            && self.chat_spawning.is_empty()
            && self.chat_bindings.is_empty()
            && self.execution_startups.is_empty()
            && self.native_closing.is_empty()
            && self.chat_handoffs.is_empty()
            && self.native_actor_startups.is_empty()
            && self.native_adoption_failures.is_empty()
            && self.upgrade.is_none()
            && !self.shutting_down
            && !self.shutdown_requested
            && !self
                .chats
                .values()
                .any(|record| record.handle.owns_work() || record.returning_terminal)
            && self
                .account_lifecycle
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty()
            && self
                .worktree_removals
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty()
            && !crate::daemon::child_reaper::has_outstanding_ownership()
    }
}
