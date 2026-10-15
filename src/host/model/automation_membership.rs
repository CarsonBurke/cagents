//! Closed archive ownership authorizes membership removal, never execution.
use super::*;
use crate::workspace_model::views::Pane;

impl Registry {
    pub(crate) fn archived_automation_pane_owner(&self, before: &Pane) -> Result<Option<crate::agent_execution::Agent>, String> {
        archived_owner_in(&self.db, before)
    }
}

/// The caller must repeat this inside the original source/revision transaction.
/// SQL bounds precede allocation. No provider, process, or service is contacted.
pub(super) fn archived_owner_in(db: &Connection, before: &Pane) -> Result<Option<crate::agent_execution::Agent>, String> {
    if before.pending_terminal_start { return Ok(None); }
    if let crate::model::CardKind::Chat(session)=&before.kind {
        if let Some(reference)=&session.archive_report {
            return super::automation_reports::report_in(db,reference,session)
                .map(|report|Some(report.archive.agent));
        }
    }
    let id = crate::model::AgentId::from_uuid(before.id.as_uuid());
    let row: Option<(Option<String>, Option<String>)> = db.query_row(
        "SELECT CASE WHEN length(CAST(g.record AS BLOB))<=3145728 THEN g.record END, CASE WHEN length(CAST(s.snapshot AS BLOB))<=4194304 THEN s.snapshot END FROM agents g JOIN automation_snapshots s ON s.agent=g.id WHERE g.id=?1 AND g.closed=1",
        [id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(|error| error.to_string())?;
    let Some((agent, snapshot)) = row else { return Ok(None); };
    let agent: crate::agent_execution::Agent = decode(&agent.ok_or("The archived owner exceeds its descriptor limit")?)?;
    let snapshot: crate::chat::Snapshot = decode(&snapshot.ok_or("The archived result exceeds its presentation limit")?)?;
    let archive = crate::automations::ArchivedConversation { agent, snapshot };
    crate::automations::archive::ArchivedPaneReadiness::qualify_kind(id, &before.kind, &archive)?;
    Ok(Some(archive.agent))
}
