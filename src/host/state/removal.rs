//! Source-checked shared removal; resident serialization spans the SQL commit.
use super::*;
use crate::workspace_model::views::{Change, TerminalRemovalBasis};

const CAPACITY: usize = 8192;

/// Whether `before` names the chat `record` holds: as it was started, or
/// with the conversation its provider has named since. A new conversation
/// learns its provider's ID only after it starts; the window records that ID
/// on its pane, and a Close sent from that pane names it.
fn removes_resident_chat(
    before: &crate::workspace_model::views::Pane,
    record: &super::super::chat::Record,
) -> bool {
    let holds = |session: crate::chat::Session| {
        crate::workspace_model::views::same_removal_source(
            before,
            &crate::workspace_model::views::Pane {
                kind: crate::model::CardKind::Chat(session),
                ..before.clone()
            },
        )
    };
    holds(record.session.clone()) || holds(record.current_session())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{BootId, CardId, CardKind, HostIncarnation, HostSessionId, LaunchRecipe},
        workspace_model::views::{Pane, Scope},
    };

    /// A new conversation learns its provider's ID after it starts, and the
    /// window's Close names it; that is still the chat the daemon holds.
    #[test]
    fn a_chat_closes_after_its_provider_names_its_conversation() {
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("removal-fixture"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        });
        let started = crate::chat::Session {
            id: TerminalId::new(),
            owned_codex_listener: None,
            native_continuity: Default::default(),
            proof: None,
            terminal: None,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Muse,
                session_id: None,
                recipe: LaunchRecipe {
                    program: "/bin/true".into(),
                    arguments: vec![],
                    working_directory: "/fixture".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
        };
        let (handle, _) = crate::chat::runtime::Handle::fixture_remote_attachment_pair();
        state.chats.insert(
            started.id,
            crate::host::chat::Record {
                execution_instance: uuid::Uuid::nil(),
                console_recipe: None,
                paired_terminal: None,
                session: started.clone(),
                handle: handle.clone(),
                source_process: None,
                subscribers: Default::default(),
                observations: Default::default(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        let close = |session: &crate::chat::Session| ModelEdit::ChangeView {
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
                    title: "quiet-heron".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 0.5,
                    scope: None,
                    kind: CardKind::Chat(session.clone()),
                },
                terminal: None,
                capture: None,
            },
        };
        let mut named = started.clone();
        named.launch.session_id = Some("conversation".into());
        // Before the provider names it, only the chat as started is held.
        assert!(state.validate_view_removal(&close(&started)).is_ok());
        assert!(state.validate_view_removal(&close(&named)).is_err());

        handle.fixture_publish_session("conversation".into());
        assert!(state.validate_view_removal(&close(&named)).is_ok());
        // A Close the window sent before it learned the name still holds.
        assert!(state.validate_view_removal(&close(&started)).is_ok());
        // Another conversation is not this chat.
        let mut other = started.clone();
        other.launch.session_id = Some("another".into());
        assert!(state.validate_view_removal(&close(&other)).is_err());
    }

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
                capture: None,
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
            !state
                .published_terminal_workers
                .latest
                .contains_key(&source.id)
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

    pub(in crate::host) fn removal_in_transition(&self, id: TerminalId) -> bool {
        self.spawning.contains(&id)
            || self.startup_custody.contains(&id)
            || self.publication_pending.contains_key(&id)
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

    pub(in crate::host) fn current_metadata_worker(
        &self,
        id: TerminalId,
        basis: &TerminalRemovalBasis,
    ) -> bool {
        matches!(basis, TerminalRemovalBasis::Worker { owner, instance }
            if owner == self.proof() && owner.is_complete() && !instance.is_nil()
                && self.terminals.get(&id).is_some_and(|record| record.handle.execution_instance() == *instance))
    }

    pub(in crate::host) fn validate_view_removal(&self, edit: &ModelEdit) -> Result<(), String> {
        if let ModelEdit::ChangeView {
            change:
                Change::UpdateTerminalSource {
                    before,
                    pane,
                    worker,
                },
            ..
        } = edit
        {
            let (
                crate::model::CardKind::Terminal(source),
                crate::model::CardKind::Terminal(observed),
            ) = (&before.kind, &pane.kind)
            else {
                return Err(conflict());
            };
            if source.id != observed.id
                || source.launch != observed.launch
                || self.removal_in_transition(source.id)
                || (self.terminals.contains_key(&source.id)
                    && !self.current_metadata_worker(source.id, worker))
            {
                return Err(conflict());
            }
            return Ok(());
        }
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
                if let Some(record) = self.chats.get(&source.id)
                    && !removes_resident_chat(before, record)
                {
                    return Err(conflict());
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
