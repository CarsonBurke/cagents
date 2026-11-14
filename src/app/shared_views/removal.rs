//! Close retains its original shared source through asynchronous cleanup.
use super::*;
use crate::workspace_model::views::{CapturedTerminalSource, TerminalRemovalBasis};

#[derive(Clone, Debug)]
pub(in crate::app) struct RemovalSource {
    scope: Scope,
    change: Change,
}

/// A basis read for this immutable gesture, never an unscoped inventory reply.
#[derive(Clone, Debug)]
pub(in crate::app) struct TerminalRemovalReceipt {
    pub basis: TerminalRemovalBasis,
    pub capture: Option<CapturedTerminalSource>,
}

impl From<TerminalRemovalBasis> for TerminalRemovalReceipt {
    fn from(basis: TerminalRemovalBasis) -> Self {
        Self {
            basis,
            capture: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(in crate::app) enum TerminalCleanupAuthority {
    Local,
    Shared {
        basis: Option<TerminalRemovalBasis>,
        capture: Option<CapturedTerminalSource>,
    },
}

impl TerminalCleanupAuthority {
    pub(in crate::app) fn receipt(&self, basis: TerminalRemovalBasis) -> TerminalRemovalReceipt {
        TerminalRemovalReceipt {
            basis,
            capture: self.capture().cloned(),
        }
    }
    pub(in crate::app) fn capture(&self) -> Option<&CapturedTerminalSource> {
        match self {
            Self::Shared { capture, .. } => capture.as_ref(),
            Self::Local => None,
        }
    }
    pub(in crate::app) fn read_basis(
        &self,
        client: &mut crate::control::ControlClient,
        source: &TerminalSession,
    ) -> Result<TerminalRemovalBasis, crate::control::ControlError> {
        if let Some(capture) = self.capture() {
            client.captured_terminal_removal_basis(capture.clone())
        } else {
            client.terminal_removal_basis(source.clone())
        }
    }
    pub(in crate::app) fn worker(&self) -> Option<&TerminalRemovalBasis> {
        match self {
            Self::Shared {
                basis: Some(basis @ TerminalRemovalBasis::Worker { .. }),
                ..
            } => Some(basis),
            _ => None,
        }
    }
    pub(in crate::app) fn requires_absence(&self) -> bool {
        matches!(
            self,
            Self::Shared {
                basis: None | Some(TerminalRemovalBasis::Absent { .. }),
                ..
            }
        )
    }

    /// Called only after read_basis validated the original canonical context.
    pub(in crate::app) fn permits_resolved(&self, observed: &TerminalRemovalBasis) -> bool {
        matches!(
            self,
            Self::Shared {
                basis: None,
                capture: Some(_)
            }
        ) || self.permits(observed)
    }

    pub(in crate::app) fn permits(&self, observed: &TerminalRemovalBasis) -> bool {
        match self {
            Self::Local => true,
            Self::Shared {
                basis: Some(expected),
                ..
            } => observed.follows_captured(Some(expected)),
            Self::Shared { basis: None, .. } => false,
        }
    }
}

impl Cagents {
    pub(in crate::app) fn capture_shared_removal(&mut self, card: CardId) -> bool {
        let captured = (|| {
            let mut owners = self.workspace_model.views.iter().filter_map(|view| {
                view.panes
                    .iter()
                    .find(|pane| pane.id == card)
                    .map(|pane| (&view.scope, pane))
            });
            let Some((scope, before)) = owners.next() else {
                // An unadmitted saved draft is local text, despite cached
                // shared membership. Closing it grants no shared authority.
                return if self.chat_awaiting_admission(card) {
                    Ok(None)
                } else if self.shared_views.attached.contains(&card)
                    || self
                        .shared_views
                        .baseline
                        .values()
                        .flatten()
                        .any(|pane| pane.id == card)
                {
                    Err(())
                } else {
                    Ok(None)
                };
            };
            if owners.next().is_some() || before.pending_terminal_start {
                return Err(());
            }
            let local = self
                .card_state(card)
                .and_then(|state| {
                    Pane::from_card(state, self.durable.card_scopes.get(&card).cloned())
                })
                .ok_or(())?;
            if !views::same_removal_source(before, &local) {
                return Err(());
            }
            let repository = match scope {
                Scope::Channel(channel) => self
                    .workspace_model
                    .channel(*channel)
                    .map(|channel| channel.repository.clone())
                    .ok_or(())?,
                Scope::Worktree { repository, .. } => repository.clone(),
            };
            let terminal = match self.cards.get(&card) {
                Some(CardRuntime::Terminal(handle)) => handle
                    .execution_instance()
                    .zip(handle.host_proof())
                    .map(|(instance, owner)| TerminalRemovalBasis::Worker { owner, instance }),
                _ => None,
            };
            let change = Change::RemoveSource {
                registry: self.workspace_model.identity,
                repository: repository.clone(),
                before: before.clone(),
                terminal,
                capture: matches!(before.kind, CardKind::Terminal(_)).then(|| {
                    CapturedTerminalSource {
                        registry: self.workspace_model.identity,
                        repository,
                        scope: scope.clone(),
                        view_revision: self
                            .workspace_model
                            .views
                            .iter()
                            .find(|v| &v.scope == scope)
                            .unwrap()
                            .revision,
                        model_sequence: self.shared_views.snapshot_sequence.max(
                            self.shared_views
                                .view_sequences
                                .get(scope)
                                .copied()
                                .unwrap_or(0),
                        ),
                        before: before.clone(),
                    }
                }),
            };
            if !views::removal_source_current(&self.workspace_model, scope, &change) {
                return Err(());
            }
            Ok(Some(RemovalSource {
                scope: scope.clone(),
                change,
            }))
        })();
        match captured {
            Ok(source) => {
                self.shared_views.removals.insert(card, source);
                true
            }
            Err(()) => {
                self.shared_removal_refused();
                false
            }
        }
    }

    fn shared_removal_refused(&mut self) {
        self.status = Some(TransientNotice::warning(
            "This pane couldn't be removed right now, so it was kept. Close it again.",
        ));
    }

    pub(in crate::app) fn prepare_shared_removal(&mut self, card: CardId) -> bool {
        if !self.shared_views.removals.contains_key(&card) && !self.capture_shared_removal(card) {
            return false;
        }
        let valid = self.shared_views.removals.get(&card).is_some_and(|source| {
            let Some(source) = source else {
                return !self
                    .workspace_model
                    .views
                    .iter()
                    .any(|view| view.panes.iter().any(|pane| pane.id == card));
            };
            let Change::RemoveSource { before, .. } = &source.change else {
                unreachable!()
            };
            (views::removal_source_current(&self.workspace_model, &source.scope, &source.change)
                || views::removal_source_retired(
                    &self.workspace_model,
                    &source.scope,
                    &source.change,
                ))
                && self
                    .card_state(card)
                    .and_then(|state| {
                        Pane::from_card(state, self.durable.card_scopes.get(&card).cloned())
                    })
                    .is_some_and(|current| views::same_removal_source(before, &current))
        });
        if !valid {
            self.shared_removal_refused();
        }
        valid
    }

    pub(in crate::app) fn shared_removal_terminal_receipt(
        &mut self,
        card: CardId,
        receipt: TerminalRemovalReceipt,
    ) -> bool {
        let Some(Some(source)) = self.shared_views.removals.get_mut(&card) else {
            return true;
        };
        let Change::RemoveSource {
            terminal, capture, ..
        } = &mut source.change
        else {
            unreachable!()
        };
        let valid = capture.as_ref() == receipt.capture.as_ref()
            && ((terminal.is_none() && capture.is_some())
                || receipt.basis.follows_captured(terminal.as_ref()));
        if valid {
            *terminal = Some(receipt.basis);
        } else {
            self.shared_removal_refused();
        }
        valid
    }

    /// Positive absence may fill missing authority. A current inventory read
    /// never grants an unknown worker to the original Close.
    pub(in crate::app) fn shared_removal_terminal_basis(
        &mut self,
        card: CardId,
        basis: TerminalRemovalBasis,
    ) -> bool {
        let Some(Some(source)) = self.shared_views.removals.get_mut(&card) else {
            return true;
        };
        let Change::RemoveSource { terminal, .. } = &mut source.change else {
            unreachable!()
        };
        let valid = basis.follows_captured(terminal.as_ref());
        if valid {
            *terminal = Some(basis);
        } else {
            self.shared_removal_refused();
        }
        valid
    }

    pub(super) fn shared_removal_change(&mut self, card: CardId, scope: &Scope) -> Option<Change> {
        self.shared_views
            .removals
            .remove(&card)
            .flatten()
            .filter(|source| &source.scope == scope)
            .filter(|source| {
                !views::removal_source_retired(&self.workspace_model, scope, &source.change)
            })
            .map(|source| source.change)
    }

    pub(in crate::app) fn shared_terminal_removal_needs_receipt(&self, card: CardId) -> bool {
        matches!(
            self.shared_views.removals.get(&card),
            Some(Some(RemovalSource {
                change: Change::RemoveSource {
                    before: Pane {
                        kind: CardKind::Terminal(_),
                        ..
                    },
                    terminal: None,
                    ..
                },
                ..
            }))
        )
    }

    pub(in crate::app) fn terminal_cleanup_authority(
        &self,
        card: CardId,
    ) -> TerminalCleanupAuthority {
        match self.shared_views.removals.get(&card) {
            Some(None) => TerminalCleanupAuthority::Local,
            Some(Some(RemovalSource {
                change:
                    Change::RemoveSource {
                        terminal, capture, ..
                    },
                ..
            })) => TerminalCleanupAuthority::Shared {
                basis: terminal.clone(),
                capture: capture.clone(),
            },
            _ => TerminalCleanupAuthority::Shared {
                basis: None,
                capture: None,
            },
        }
    }

    /// Reattach an obsolete surface without granting its original Close any
    /// authority over the successor. A newer surface is never replaced here.
    pub(in crate::app) fn readmit_shared_terminal_after_source_refusal(
        &mut self,
        card: CardId,
    ) -> Task<Message> {
        let Some(Some(RemovalSource {
            scope,
            change:
                Change::RemoveSource {
                    registry,
                    repository,
                    before,
                    terminal: Some(basis @ TerminalRemovalBasis::Worker { owner, instance }),
                    ..
                },
        })) = self.shared_views.removals.get(&card)
        else {
            return Task::none();
        };
        let CardKind::Terminal(original) = &before.kind else {
            return Task::none();
        };
        if self.pane_transfer_pending(card) || self.terminal_start_in_flight(card) {
            return Task::none();
        }
        match self.cards.get(&card) {
            Some(CardRuntime::Terminal(surface))
                if surface.execution_instance() == Some(*instance)
                    && surface.host_proof().as_ref() == Some(owner) => {}
            Some(CardRuntime::Completed | CardRuntime::Failed(_)) => {}
            _ => return Task::none(),
        }
        let mut owners = self.workspace_model.views.iter().filter_map(|view| {
            view.panes
                .iter()
                .find(|pane| pane.id == card)
                .map(|pane| (&view.scope, pane))
        });
        let Some((current_scope, canonical)) = owners.next() else {
            return Task::none();
        };
        if owners.next().is_some() || current_scope != scope || canonical.pending_terminal_start {
            return Task::none();
        }
        let CardKind::Terminal(current) = &canonical.kind else {
            return Task::none();
        };
        if current.id != original.id
            || !self.card_state(card).is_some_and(|card| {
                card.existence.is_open()
                    && matches!(&card.kind, CardKind::Terminal(local) if local.id == current.id)
            })
        {
            return Task::none();
        }
        let current_basis = Change::RemoveSource {
            registry: *registry,
            repository: repository.clone(),
            before: canonical.clone(),
            terminal: Some(basis.clone()),
            capture: None,
        };
        if !views::removal_source_current(&self.workspace_model, current_scope, &current_basis) {
            return Task::none();
        }
        let terminal = original.id;
        let kind = canonical.kind.clone();
        self.card_state_mut(card).unwrap().kind = kind;
        self.saved_terminals.entries.remove(&card);
        self.retained_outputs.remove(&terminal);
        self.shared_views.ended_terminal_workers.remove(&card);
        self.shared_views.attached.insert(card);
        self.terminal_recovery_attempts.remove(&terminal);
        if matches!(self.card_state(card).map(|s| &s.kind), Some(CardKind::Terminal(s)) if terminal_ended(s))
        {
            self.cards.insert(card, CardRuntime::Completed);
            self.lifecycle.remove(card);
            self.set_residency(card, Residency::Hibernated);
            return self.attach_retained_outputs([card]);
        }
        let exposure = self.exposure(card);
        self.readmit_held_terminal(card, exposure);
        self.lifecycle_sweep()
    }

    pub(in crate::app) fn shared_removal_ready(&mut self, card: CardId) -> bool {
        let ready = !self.shared_terminal_removal_needs_receipt(card)
            || matches!(self.shared_views.removals.get(&card), Some(Some(source))
                if views::removal_source_retired(&self.workspace_model, &source.scope, &source.change));
        if !ready {
            self.shared_removal_refused();
        }
        ready
    }
}
