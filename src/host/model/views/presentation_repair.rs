//! New consent to remove a shown presentation never invents old execution proof.
use super::*;
use crate::{agent_execution::{Agent, Origin, PresentationRepairTarget}, model::{AgentId, CardId, CardKind}};

pub(super) fn registry_identity(db: &Connection) -> Result<Uuid, String> {
    let identity: String = db.query_row("SELECT identity FROM metadata WHERE key=0", [], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    identity.parse().map_err(|error| format!("Registry identity is invalid: {error}"))
}

fn closed_owner(db: &Connection, origin: &Origin, id: AgentId) -> Result<Agent, String> {
    let agent = super::super::execution::read_agent(db, id)?;
    if &agent.origin != origin { return Err("This caller does not own the agent".into()); }
    if !agent.closed { return Err("Presentation-only repair requires an already closed agent".into()); }
    let busy: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM agent_launches WHERE agent=?1)
        OR EXISTS(SELECT 1 FROM agent_runs WHERE agent=?1 AND state IN ('accepted','running','waiting'))",
        [id.to_string()], |row| row.get(0)).map_err(|error| error.to_string())?;
    if busy { return Err("Presentation-only repair cannot bypass a pending launch or unfinished run".into()); }
    Ok(agent)
}

pub(super) fn candidate(db: &Connection, agent: &Agent, pane: &crate::workspace_model::views::Pane) -> Result<(), String> {
    let CardKind::Terminal(ended) = &pane.kind else {
        return Err("Only an ended managed terminal presentation can require this consent".into());
    };
    let record = terminal_source(db, pane.id)?;
    let located_end = record.as_ref().is_some_and(|record| record.matches_end(ended));
    if pane.id != CardId::from_uuid(agent.id.as_uuid()) || pane.pending_terminal_start
        || !(ended_terminal(ended) || located_end)
    {
        return Err("The current pane is not an ended managed presentation".into());
    }
    // These public identifiers identify which presentation to show for consent.
    // They do not establish the unknown historical worker generation.
    let identified = agent.session.iter().filter_map(|source| match source {
        crate::agent_execution::HostedSession::Terminal(session) => Some(session), _ => None,
    }).chain(agent.console.iter()).any(|source| source.id == ended.id && source.launch == ended.launch);
    if !identified { return Err("The current pane is not identified by this agent".into()); }
    if record.as_ref().is_some_and(|record| record.original_source().is_some()) {
        return Err("Recorded terminal provenance exists; presentation-only repair cannot override it".into());
    }
    if record.is_some() && !located_end {
        return Err("The located presentation no longer matches its exact historical basis".into());
    }
    Ok(())
}

impl Registry {
    pub(crate) fn inspect_presentation_repair(&mut self, origin: &Origin, id: AgentId)
        -> Result<Option<PresentationRepairTarget>, String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        let agent = closed_owner(&tx, origin, id)?;
        let target = match owner(&tx, CardId::from_uuid(id.as_uuid()))? {
            None => None,
            Some(scope) => {
                let view = read(&tx, &scope)?;
                let pane = view.panes.iter().find(|pane| pane.id == CardId::from_uuid(id.as_uuid()))
                    .ok_or("The managed pane owner has no corresponding presentation")?;
                candidate(&tx, &agent, pane)?;
                Some(PresentationRepairTarget {registry: registry_identity(&tx)?, agent,
                    scope, view_revision: view.revision, pane: pane.clone()})
            }
        };
        tx.commit().map_err(|error| error.to_string())?;
        Ok(target)
    }

    /// Compare and remove only the exact observation the owner confirmed.
    /// Agent, run, report and provider state are deliberately never written.
    pub(crate) fn confirm_presentation_repair(&mut self, origin: &Origin, observed: &PresentationRepairTarget)
        -> Result<bool, String> {
        let tx = self.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let agent = closed_owner(&tx, origin, observed.agent.id)?;
        if registry_identity(&tx)? != observed.registry || agent != observed.agent {
            return Err("The registry or closed agent changed; inspect the current presentation again".into());
        }
        let id = CardId::from_uuid(agent.id.as_uuid());
        let Some(scope) = owner(&tx, id)? else {
            tx.commit().map_err(|error| error.to_string())?;
            return Ok(false);
        };
        let mut view = read(&tx, &scope)?;
        let pane = view.panes.iter().find(|pane| pane.id == id);
        if scope != observed.scope || view.revision != observed.view_revision || pane != Some(&observed.pane) {
            return Err("The displayed pane, owner or view revision changed; no presentation was removed".into());
        }
        candidate(&tx, &agent, &observed.pane)?;
        view.apply(&Change::Remove {id})?;
        view.revision = view.revision.checked_add(1).ok_or("view revision exhausted")?;
        tx.execute("DELETE FROM shared_view_panes WHERE id=?1", [id.to_string()]).map_err(|error| error.to_string())?;
        write(&tx, &view)?;
        tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", []).map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(true)
    }
}
