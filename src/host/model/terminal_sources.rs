//! Canonical source generations share the workspace SQL writer. Admission is
//! acquired before HostState, and held through the resident publication edge.
use super::*;
use crate::{
    model::{CardId, CardKind, TerminalSession},
    workspace_model::views::{CapturedTerminalSource, Change, Pane, Scope, TerminalRemovalBasis},
};

pub(in crate::host) struct SourceAdmission<'a>(rusqlite::Transaction<'a>);

impl Registry {
    pub(in crate::host) fn unbound_terminal_update(
        db: &Connection,
        edit: &ModelEdit,
    ) -> Result<bool, String> {
        let ModelEdit::ChangeView {
            change: Change::UpdateTerminalSource { before, .. },
            ..
        } = edit
        else {
            return Ok(false);
        };
        Ok(views::bindings::read(db, before.id)?.is_none_or(|b| b.basis.is_none()))
    }
    pub(in crate::host) fn terminal_publication_source(
        &self,
        terminal: crate::model::TerminalId,
    ) -> Result<Option<(CardId, u64)>, String> {
        let raw: Option<(String, i64)> = self.db.query_row(
            "SELECT p.id,b.birth_sequence FROM panes p JOIN terminal_pane_bindings b ON b.pane=p.id WHERE p.kind='terminal' AND p.resource=?1",
            [terminal.to_string()], |r| Ok((r.get(0)?, r.get(1)?))).optional().map_err(|e| e.to_string())?;
        raw.map(|(id, birth)| {
            Ok((
                id.parse::<CardId>().map_err(|e| e.to_string())?,
                birth as u64,
            ))
        })
        .transpose()
    }
    pub(in crate::host) fn source_admission(&mut self) -> Result<SourceAdmission<'_>, String> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        schema::require_current(&tx)?;
        Ok(SourceAdmission(tx))
    }
}

/// Only committed SQL source publication can construct this permit.
pub(in crate::host) struct PublishedTerminal(());

impl SourceAdmission<'_> {
    pub(in crate::host) fn retention_pin_generation(
        &self,
        terminal: crate::model::TerminalId,
    ) -> Result<Option<String>, String> {
        terminal_retention::read_generation(&self.0, terminal)
    }
    pub(in crate::host) fn validate(&self, capture: &CapturedTerminalSource) -> Result<(), String> {
        views::bindings::captured(&self.0, capture, None).map(drop)
    }

    pub(in crate::host) fn validate_pending(
        &self,
        scope: &Scope,
        pane: &Pane,
    ) -> Result<(), String> {
        let view = views::read(&self.0, scope)?;
        if view.retired.contains(&pane.id)
            || view.panes.iter().find(|candidate| candidate.id == pane.id) != Some(pane)
        {
            return Err(Registry::VIEW_SOURCE_CONFLICT.into());
        }
        Ok(())
    }

    pub(in crate::host) fn publish(
        self,
        session: &TerminalSession,
        worker: &TerminalRemovalBasis,
        pending: Option<CardId>,
        original: Option<(CardId, u64)>,
    ) -> Result<
        (
            PublishedTerminal,
            Vec<(ModelEdit, crate::workspace_model::delta::EditReceipt)>,
        ),
        String,
    > {
        if pending.is_some() {
            Registry::pending_terminal_save_fixture_error()?;
        }
        let tx = self.0;
        let mut edges = Vec::new();
        let pane: Option<String> = tx
            .query_row(
                "SELECT id FROM panes WHERE kind='terminal' AND resource=?1",
                [session.id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let mut scopes = pane
            .map(|id| -> Result<_, String> {
                let id = id.parse::<CardId>().map_err(|e| e.to_string())?;
                let scope = views::owner(&tx, id)?.ok_or(Registry::VIEW_REMOVAL_CONFLICT)?;
                views::read(&tx, &scope)
            })
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        for view in &mut scopes {
            let Some(index) = view.panes.iter().position(|p| {
                matches!(&p.kind,
                CardKind::Terminal(s) if s.id == session.id)
            }) else {
                continue;
            };
            let before = view.clone();
            let old = view.panes[index].clone();
            if let Some((pane, birth)) = original {
                if pane != old.id
                    || views::bindings::read(&tx, pane)?.is_none_or(|b| b.birth_sequence != birth)
                {
                    return Err(Registry::VIEW_REMOVAL_CONFLICT.into());
                }
            } else {
                let CardKind::Terminal(saved) = &old.kind else {
                    unreachable!()
                };
                let mut comparable = session.clone();
                comparable.continuity = saved.continuity.clone();
                if saved.resume_hint.is_none()
                    && comparable.resume_hint
                        == crate::model::agent_recovery::observed_hint_from_launch(&saved.launch)
                {
                    comparable.resume_hint = None;
                }
                if comparable != *saved {
                    return Err(Registry::VIEW_REMOVAL_CONFLICT.into());
                }
            }

            if let Some(pending) = pending {
                if old.id != pending
                    || !old.pending_terminal_start
                    || !matches!(&old.kind, CardKind::Terminal(s) if s.launch == session.launch)
                {
                    return Err(Registry::VIEW_REMOVAL_CONFLICT.into());
                }
            }
            view.panes[index].kind = CardKind::Terminal(session.clone());
            view.panes[index].pending_terminal_start = false;
            views::update_terminal_source(
                &tx,
                old.id,
                &old.kind,
                Some(&old.kind),
                Some(&view.panes[index].kind),
            )?;
            view.revision = views::store::next_revision(&tx)?;
            let mut recorder = views::Recorder::default();
            views::write_from(&tx, &before, view, &mut recorder)?;
            let sequence: i64 = tx
                .query_row(
                    "UPDATE metadata SET sequence=sequence+1 WHERE key=0 RETURNING sequence",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            views::bindings::write(
                &tx,
                &view.panes[index],
                sequence as u64,
                view.revision,
                Some(worker),
            )?;
            let edit = ModelEdit::ChangeView {
                scope: view.scope.clone(),
                expected_revision: before.revision,
                operation: Uuid::new_v4(),
                change: Change::Update {
                    before: old,
                    pane: view.panes[index].clone(),
                },
            };
            let mut effects = recorder.finish(&tx)?;
            effects
                .views
                .push(crate::workspace_model::delta::ViewRevision {
                    scope: view.scope.clone(),
                    before: Some(before.revision),
                    after: Some(view.revision),
                });
            edges.push((
                edit,
                crate::workspace_model::delta::EditReceipt {
                    identity: tx
                        .query_row("SELECT identity FROM metadata WHERE key=0", [], |r| {
                            r.get::<_, String>(0)
                        })
                        .map_err(|e| e.to_string())?
                        .parse::<Uuid>()
                        .map_err(|e| e.to_string())?,
                    sequence: sequence as u64,
                    effects,
                },
            ));
        }
        if (pending.is_some() || original.is_some()) && edges.is_empty() {
            return Err(Registry::VIEW_REMOVAL_CONFLICT.into());
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok((PublishedTerminal(()), edges))
    }
}

#[cfg(test)]
impl PublishedTerminal {
    pub(in crate::host) fn fixture() -> Self {
        Self(())
    }
}
