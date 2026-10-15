//! Explicit current-presentation consent, conditional on normal exact cleanup.
use super::*;
use super::presentation_repair::{candidate, registry_identity};
use crate::{agent_execution::{Agent, Origin}, model::{AgentId, CardId}};

/// An open observation never substitutes for provider cleanup authority. It is
/// checked once before teardown and again in the final retirement transaction.
fn open_owner(db: &Connection, origin: &Origin, id: AgentId) -> Result<Agent, String> {
    let agent = super::super::execution::read_agent(db, id)?;
    if &agent.origin != origin { return Err("This caller does not own the agent".into()); }
    if agent.closed { return Err("Combined close consent requires an open agent; use presentation-only repair for closed agents".into()); }
    let pending: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM agent_launches WHERE agent=?1)",
        [id.to_string()], |row| row.get(0)).map_err(|error| error.to_string())?;
    if pending { return Err("A pending provider launch must settle before combined close consent".into()); }
    if !matches!(agent.session.as_ref(), Some(crate::agent_execution::HostedSession::Chat(_))) {
        return Err("Combined close consent requires the exact retained native provider source".into());
    }
    Ok(agent)
}

pub(in crate::host::model) fn validate_open_close(db: &Connection, origin: &Origin,
    observed: &crate::agent_execution::PresentationCloseObservation,
    source: &crate::agent_execution::HostedSession) -> Result<View, String> {
    let agent = open_owner(db, origin, observed.agent.id)?;
    if registry_identity(db)? != observed.registry || agent != observed.agent
        || agent.session.as_ref() != Some(&observed.source) || source != &observed.source {
        return Err("The registry, agent or exact provider source changed; inspect again before closing".into());
    }
    let id = CardId::from_uuid(agent.id.as_uuid());
    let scope = owner(db, id)?.ok_or("The observed presentation disappeared; no close consent was applied")?;
    let view = read(db, &scope)?;
    if scope != observed.scope || view.revision != observed.view_revision
        || view.panes.iter().find(|pane| pane.id == id) != Some(&observed.pane) {
        return Err("The displayed pane, owner or view revision changed; no combined close consent was applied".into());
    }
    candidate(db, &agent, &observed.pane)?;
    Ok(view)
}

pub(in crate::host::model) fn remove_open_close(db: &Connection, origin: &Origin,
    observed: &crate::agent_execution::PresentationCloseObservation,
    source: &crate::agent_execution::HostedSession) -> Result<(), String> {
    let mut view = validate_open_close(db, origin, observed, source)?;
    view.apply(&Change::Remove {id: observed.pane.id})?;
    view.revision = view.revision.checked_add(1).ok_or("view revision exhausted")?;
    db.execute("DELETE FROM shared_view_panes WHERE id=?1", [observed.pane.id.to_string()])
        .map_err(|error| error.to_string())?;
    write(db, &view)
}

impl Registry {
    pub(crate) fn inspect_presentation_close(&mut self, origin: &Origin, id: AgentId)
        -> Result<Option<crate::agent_execution::PresentationCloseObservation>, String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        let agent = open_owner(&tx, origin, id)?;
        let target = match owner(&tx, CardId::from_uuid(id.as_uuid()))? {
            None => None,
            Some(scope) => {
                let view = read(&tx, &scope)?;
                let pane = view.panes.iter().find(|pane| pane.id == CardId::from_uuid(id.as_uuid()))
                    .ok_or("The managed pane owner has no corresponding presentation")?;
                candidate(&tx, &agent, pane)?;
                Some(crate::agent_execution::PresentationCloseObservation {
                    registry: registry_identity(&tx)?, source: agent.session.clone().unwrap(), agent,
                    scope, view_revision: view.revision, pane: pane.clone()})
            }
        };
        tx.commit().map_err(|error| error.to_string())?;
        Ok(target)
    }

    pub(crate) fn preflight_presentation_close(&mut self, origin: &Origin,
        observed: &crate::agent_execution::PresentationCloseObservation) -> Result<(), String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        validate_open_close(&tx, origin, observed, &observed.source)?;
        tx.commit().map_err(|error| error.to_string())
    }
}
