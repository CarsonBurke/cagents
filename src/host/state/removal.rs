//! Source-checked shared removal; resident serialization spans the SQL commit.
use super::*;
use crate::workspace_model::views::{Change, TerminalRemovalBasis};

const CAPACITY: usize = 8192;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{BootId, CardId, CardKind, HostIncarnation, HostSessionId, LaunchRecipe},
        workspace_model::views::{Pane, Scope},
    };

    #[test]
    fn evicted_publication_cannot_restore_an_old_absence_stamp() {
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("removal-fixture"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        });
        let launch = LaunchRecipe {
            program: "/bin/true".into(),
            arguments: vec![],
            working_directory: "/fixture".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let source = TerminalSession {
            id: TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        };
        let basis = state.terminal_removal_basis(&source).unwrap();
        let removal = ModelEdit::ChangeView {
            scope: Scope::Worktree {
                repository: "/fixture".into(),
                path: "/fixture".into(),
            },
            expected_revision: 1,
            operation: uuid::Uuid::new_v4(),
            change: Change::RemoveSource {
                registry: uuid::Uuid::new_v4(),
                repository: "/fixture".into(),
                before: Pane {
                    id: CardId::new(),
                    title: "Fixture".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 0.5,
                    scope: None,
                    kind: CardKind::Terminal(source.clone()),
                },
                terminal: Some(basis),
            },
        };
        assert!(state.validate_view_removal(&removal).is_ok());
        state
            .published_terminal_workers
            .observe(source.id, uuid::Uuid::new_v4());
        for _ in 0..CAPACITY {
            state
                .published_terminal_workers
                .observe(TerminalId::new(), uuid::Uuid::new_v4());
        }
        assert!(
            state
                .published_terminal_workers
                .latest
                .get(&source.id)
                .is_none()
        );
        assert!(state.published_terminal_workers.latest.len() <= CAPACITY);
        assert!(
            state.validate_view_removal(&removal).is_err(),
            "eviction loses authority, even when both latest-worker reads are None"
        );
    }
}

#[derive(Default)]
pub(super) struct PublishedWorkers {
    latest: BTreeMap<TerminalId, uuid::Uuid>,
    order: VecDeque<(TerminalId, uuid::Uuid)>,
    epoch: u64,
}

impl PublishedWorkers {
    pub(super) fn observe(&mut self, id: TerminalId, worker: uuid::Uuid) {
        if self.latest.get(&id) == Some(&worker) {
            return;
        }
        self.latest.insert(id, worker);
        self.order.push_back((id, worker));
        while self.order.len() > CAPACITY {
            let (id, worker) = self.order.pop_front().unwrap();
            if self.latest.get(&id) == Some(&worker) {
                self.latest.remove(&id);
            }
            self.epoch = self
                .epoch
                .checked_add(1)
                .expect("worker history epoch exhausted");
        }
    }
}

fn same_source(before: &TerminalSession, current: &TerminalSession) -> bool {
    let mut comparable = current.clone();
    comparable.continuity = before.continuity.clone();
    comparable == *before
        && current
            .continuity
            .live_proof()
            .is_none_or(|proof| before.continuity.live_proof() == Some(proof))
}

fn conflict() -> String {
    super::super::model::Registry::VIEW_REMOVAL_CONFLICT.into()
}

impl HostState {
    pub(in crate::host) fn worker_marker(
        &self,
        id: TerminalId,
    ) -> Result<ServerMessage, ProtocolError> {
        let record = self.terminals.get(&id).ok_or_else(|| not_found(id))?;
        Ok(ServerMessage::TerminalWorker {
            terminal_id: id,
            instance: record.handle.execution_instance(),
        })
    }

    pub(super) fn publish_worker(&self, client: ClientId, id: TerminalId) {
        if let Some(delivery) = self.clients.get(&client)
            && let Ok(marker) = self.worker_marker(id)
        {
            let _ = delivery.push(Outbound::broadcast(&self.proof, marker));
        }
    }

    fn removal_in_transition(&self, id: TerminalId) -> bool {
        self.spawning.contains(&id)
            || super::super::chat::terminal_claim_pending(self, id)
            || self.chat_handoffs.contains_key(&id)
            || self.chat_bindings.contains_key(&id)
    }

    pub(in crate::host) fn terminal_removal_basis(
        &self,
        source: &TerminalSession,
    ) -> Result<TerminalRemovalBasis, ProtocolError> {
        let refused = || ProtocolError::new(ProtocolErrorCode::InvalidRequest, conflict(), false);
        if self.removal_in_transition(source.id) {
            return Err(refused());
        }
        if let Some(record) = self.terminals.get(&source.id) {
            let current = self.descriptor(0, source.id)?.session;
            let mut same_fields = current.clone();
            same_fields.continuity = source.continuity.clone();
            let ended_source = matches!(
                source.continuity,
                TerminalContinuity::Exited { .. }
                    | TerminalContinuity::RelaunchAvailable {
                        last_exit: Some(_),
                        ..
                    }
            );
            if !(same_source(source, &current) || (ended_source && same_fields == *source)) {
                return Err(refused());
            }
            return Ok(TerminalRemovalBasis::Worker {
                owner: self.proof().clone(),
                instance: record.handle.execution_instance(),
            });
        }
        if self.terminal_retirements.contains_key(&source.id) || self.removal_chat_owner(source.id)
        {
            return Err(refused());
        }
        Ok(TerminalRemovalBasis::Absent {
            owner: self.proof().clone(),
            latest_worker: self
                .published_terminal_workers
                .latest
                .get(&source.id)
                .copied(),
            history_epoch: self.published_terminal_workers.epoch,
        })
    }

    pub(in crate::host) fn validate_view_removal(&self, edit: &ModelEdit) -> Result<(), String> {
        let ModelEdit::ChangeView {
            change: Change::RemoveSource {
                before, terminal, ..
            },
            ..
        } = edit
        else {
            return Ok(());
        };
        let crate::model::CardKind::Terminal(source) = &before.kind else {
            if let crate::model::CardKind::Chat(source) = &before.kind {
                if self.spawning.contains(&source.id)
                    || super::super::chat::shared_removal_in_transition(self, before)
                {
                    return Err(conflict());
                }
                if let Some(record) = self.chats.get(&source.id) {
                    let current = crate::workspace_model::views::Pane {
                        kind: crate::model::CardKind::Chat(record.session.clone()),
                        ..before.clone()
                    };
                    if !crate::workspace_model::views::same_removal_source(before, &current) {
                        return Err(conflict());
                    }
                }
            }
            return if terminal.is_none() {
                Ok(())
            } else {
                Err(conflict())
            };
        };
        if self.removal_in_transition(source.id)
            || self.terminal_retirements.contains_key(&source.id)
            || self.removal_chat_owner(source.id)
        {
            return Err(conflict());
        }
        let latest = self
            .published_terminal_workers
            .latest
            .get(&source.id)
            .copied();
        match terminal {
            Some(TerminalRemovalBasis::Worker { owner, instance })
                if owner == self.proof()
                    && !instance.is_nil()
                    && latest == Some(*instance)
                    && self.terminals.get(&source.id).is_none_or(|record| {
                        record.handle.execution_instance() == *instance
                            && self
                                .descriptor(0, source.id)
                                .is_ok_and(|current| same_source(source, &current.session))
                    }) =>
            {
                Ok(())
            }
            Some(TerminalRemovalBasis::Absent {
                owner,
                latest_worker,
                history_epoch,
            }) if owner == self.proof()
                && latest_worker == &latest
                && *history_epoch == self.published_terminal_workers.epoch
                && !self.terminals.contains_key(&source.id) =>
            {
                Ok(())
            }
            _ => Err(conflict()),
        }
    }

    fn removal_chat_owner(&self, id: TerminalId) -> bool {
        self.chats.values().any(|record| {
            record.session.id == id
                || record
                    .session
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| terminal.id == id)
        })
    }
}
