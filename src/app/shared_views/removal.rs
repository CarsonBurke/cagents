//! Close retains its original shared source through asynchronous cleanup.
use super::*;
use crate::workspace_model::views::TerminalRemovalBasis;

#[derive(Clone, Debug)]
pub(in crate::app) struct RemovalSource {
    scope: Scope,
    change: Change,
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
                repository,
                before: before.clone(),
                terminal,
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

    /// A cleanup receipt may fill missing worker evidence, but cannot replace
    /// an original known worker. A new daemon's positive absence is separate.
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
        let valid = match (terminal.as_ref(), &basis) {
            (
                Some(TerminalRemovalBasis::Worker {
                    owner: a,
                    instance: x,
                }),
                TerminalRemovalBasis::Worker {
                    owner: b,
                    instance: y,
                },
            ) => a == b && x == y,
            (
                Some(TerminalRemovalBasis::Worker { owner: a, instance }),
                TerminalRemovalBasis::Absent {
                    owner: b,
                    latest_worker,
                    ..
                },
            ) => a != b || latest_worker == &Some(*instance),
            (Some(existing), next) => existing == next,
            (None, _) => true,
        };
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
