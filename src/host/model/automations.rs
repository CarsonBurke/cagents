//! Schedule definitions, claims and history share execution's SQLite authority.
//! No provider, filesystem or account I/O is performed inside a transaction.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
use uuid::Uuid;

use super::Registry;
use crate::automations::{
    Automation, Definition, Firing, FiringState, HistoryCursor, HistoryPage, MAX_HISTORY_PAGE,
    Occurrence, Summary,
};

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE IF NOT EXISTS automations (
             id TEXT PRIMARY KEY,
             repository TEXT NOT NULL,
             revision INTEGER NOT NULL,
             record TEXT NOT NULL,
             summary TEXT NOT NULL,
             enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
             next_due_ms INTEGER,
             active_firing TEXT,
             deleted_ms INTEGER,
             updated_ms INTEGER NOT NULL
         ) STRICT;
         CREATE INDEX IF NOT EXISTS automation_deadlines ON automations(enabled, next_due_ms)
             WHERE deleted_ms IS NULL;
         CREATE INDEX IF NOT EXISTS automation_owners ON automations(active_firing) WHERE active_firing IS NOT NULL;
         CREATE INDEX IF NOT EXISTS automations_by_project ON automations(repository) WHERE deleted_ms IS NULL;
         CREATE TABLE IF NOT EXISTS automation_revisions (
             automation TEXT NOT NULL REFERENCES automations(id),
             revision INTEGER NOT NULL,
             definition TEXT NOT NULL,
             target_identity TEXT NOT NULL,
             PRIMARY KEY(automation, revision)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS automation_firings (
             id TEXT PRIMARY KEY,
             automation TEXT NOT NULL REFERENCES automations(id),
             revision INTEGER NOT NULL,
             occurrence_key TEXT NOT NULL,
             record TEXT NOT NULL,
             created_ms INTEGER NOT NULL,
             state TEXT NOT NULL,
             run_id TEXT,
             UNIQUE(automation, occurrence_key),
             FOREIGN KEY(automation, revision) REFERENCES automation_revisions(automation, revision)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS automation_notifications (
            key TEXT PRIMARY KEY, event TEXT NOT NULL
         ) STRICT;
         CREATE INDEX IF NOT EXISTS automation_history ON automation_firings(automation, created_ms DESC, id DESC);
         CREATE INDEX IF NOT EXISTS automation_all_history ON automation_firings(created_ms DESC, id DESC);
         CREATE INDEX IF NOT EXISTS automation_active ON automation_firings(state);
         CREATE UNIQUE INDEX IF NOT EXISTS automation_run ON automation_firings(run_id) WHERE run_id IS NOT NULL;
         PRAGMA user_version = 2;
         COMMIT;"
    ).map_err(|error| error.to_string())
}

pub(super) fn close_project(
    tx: &Transaction<'_>,
    repository: &str,
    now_ms: u64,
) -> Result<(), String> {
    let records = tx
        .prepare("SELECT record FROM automations WHERE repository=?1 AND deleted_ms IS NULL")
        .map_err(|e| e.to_string())?
        .query_map([repository], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    for record in records {
        let mut automation: Automation = decode(&record)?;
        automation.revision = automation
            .revision
            .checked_add(1)
            .ok_or("Automation revision overflow")?;
        automation.execution_revision = automation.revision;
        automation.enabled = false;
        automation.next = None;
        automation.channel = None;
        automation.updated_ms = now_ms;
        write_revision(tx, &automation)?;
        write_automation(tx, &automation)?;
    }
    Ok(())
}

impl Registry {
    pub(crate) fn archived_automation_agents_page(
        &self,
        scope: crate::agent_execution::Placement,
        identity: Uuid,
        sequence: u64,
        after: Option<crate::model::AgentId>,
        limit: usize,
        proof: crate::model::HostSessionProof,
    ) -> Result<crate::automations::ArchiveAgentPage, String> {
        let (current_identity,current_sequence):(String,u64)=self.db.query_row(
            "SELECT identity,sequence FROM metadata WHERE key=0",[],|row|Ok((row.get(0)?,row.get(1)?))).map_err(|error|error.to_string())?;
        if current_identity!=identity.to_string() || current_sequence!=sequence {
            return Err("The workspace changed during saved-result discovery; retry its current model".into());
        }
        let limit=limit.clamp(1,64);
        let mut query=self.db.prepare(
            "SELECT g.id,g.record FROM agents g JOIN automations a ON a.id=json_extract(g.record,'$.origin.Automation.id')
             LEFT JOIN shared_view_panes p ON p.id=g.id
             WHERE g.closed=1 AND EXISTS(SELECT 1 FROM automation_snapshots s WHERE s.agent=g.id)
               AND (json_extract(a.record,'$.definition.visibility') IS NOT 'headless' OR p.id IS NOT NULL)
               AND coalesce(p.scope,json_extract(g.record,'$.placement'))=?1 AND g.id>?2
             ORDER BY g.id LIMIT ?3").map_err(|error|error.to_string())?;
        let rows=query.query_map(params![encode(&scope)?,after.map(|id|id.to_string()).unwrap_or_default(),limit+1],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))
            .map_err(|error|error.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|error|error.to_string())?;
        let model=crate::workspace_model::WorkspaceModel {views:super::views::all(&self.db)?,..Default::default()};
        let index=crate::workspace_model::views::ManagedDisplayIndex::new(&model);
        let mut agents=Vec::new();let mut bytes=0usize;let mut last=None;let mut more=rows.len()>limit;
        for (id,record) in rows.into_iter().take(limit) {
            // Keep each IPC page below the ordinary archive payload limit.
            if bytes.saturating_add(record.len())>3*1024*1024 {
                if last.is_none() {return Err("A saved agent descriptor exceeds the discovery page limit".into());}
                more=true;break;
            }
            last=Some(id.parse::<crate::model::AgentId>().map_err(|error|error.to_string())?);
            bytes+=record.len();
            let agent:crate::agent_execution::Agent=decode(&record)?;
            if matches!(agent.origin,crate::agent_execution::Origin::Automation {..})
                && matches!(&agent.session,Some(crate::agent_execution::HostedSession::Chat(session)) if session.proof.as_ref().is_some_and(crate::model::HostSessionProof::is_complete))
                && index.resolve(&agent).is_some_and(|display|display.scope==scope) {
                agents.push(agent);
            }
        }
        Ok(crate::automations::ArchiveAgentPage {scope,identity,sequence,proof,agents,next:more.then_some(last).flatten()})
    }

    /// Saving final history reserves that provider identity before close and
    /// before any asynchronous window learns about the read-only result.
    pub(crate) fn is_archived_automation_session(&self,id:crate::model::TerminalId)->Result<bool,String> {
        let record:Option<String>=self.db.query_row(
            "SELECT g.record FROM agents g LEFT JOIN agent_policies p ON p.agent=g.id WHERE g.session_id=?1 AND (g.closed=1 OR (coalesce(p.interactive,0)=0 AND EXISTS(SELECT 1 FROM automation_snapshots s WHERE s.agent=g.id)))",
            [id.to_string()],|row|row.get(0)).optional().map_err(|error|error.to_string())?;
        let Some(record)=record else {return Ok(false)};
        let agent:crate::agent_execution::Agent=decode(&record)?;
        if !matches!(agent.origin,crate::agent_execution::Origin::Automation {..}) {return Ok(false);}
        if !agent.session.as_ref().is_some_and(|session|session.id()==id) {
            return Err("The saved automation session owner is invalid".into());
        }
        // Closing this owner remains authoritative after its bounded history
        // expires: deleting transcript data never grants restart permission.
        Ok(true)
    }

    pub(crate) fn archived_automation_agents(
        &self,
    ) -> Result<Vec<crate::agent_execution::Agent>, String> {
        let mut query = self
            .db
            .prepare(
                "SELECT g.record FROM agents g JOIN automation_snapshots s ON s.agent=g.id
            JOIN automation_firings f ON f.id=s.firing JOIN automations a ON a.id=f.automation
            LEFT JOIN shared_view_panes p ON p.id=g.id
            WHERE g.closed=1 AND (json_extract(a.record,'$.definition.visibility') IS NOT 'headless' OR p.id IS NOT NULL)
            ORDER BY f.created_ms DESC,f.id DESC LIMIT 1024",
            )
            .map_err(|error| error.to_string())?;
        query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .map(|record| decode(&record.map_err(|error| error.to_string())?))
            .collect()
    }

    pub(crate) fn archive_automation_conversation(
        &mut self,
        firing: Uuid,
        expected: &crate::agent_execution::Agent,
        mut snapshot: crate::chat::Snapshot,
    ) -> Result<(), String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        let firing = read_firing(&tx, firing)?;
        if firing.agent != Some(expected.id)
            || expected.origin
                != (crate::agent_execution::Origin::Automation {
                    id: firing.automation,
                })
        {
            return Err("The archived conversation does not belong to this firing".into());
        }
        let record:String=tx.query_row("SELECT record FROM agents WHERE id=?1",[expected.id.to_string()],|row|row.get(0)).map_err(|error|error.to_string())?;
        let mut current:crate::agent_execution::Agent=decode(&record)?;
        if current.origin!=expected.origin || current.session!=expected.session {
            return Err("The exact automation conversation changed before saving its result".into());
        }
        let confirmed=snapshot.session_id.as_deref().filter(|thread|!thread.trim().is_empty())
            .ok_or("The provider has not confirmed an exact conversation for its saved result")?;
        let Some(crate::agent_execution::HostedSession::Chat(session))=&mut current.session else {
            return Err("The automation has no original native conversation to save".into());
        };
        if session.launch.session_id.as_deref().is_some_and(|thread|thread!=confirmed) {
            return Err("The provider confirmed a different conversation; its saved result was refused".into());
        }
        if session.launch.session_id.is_none() {
            // The broker read this snapshot from the exact proved handle.
            // Learn only its confirmed thread; preserve current display moves
            // and every other authority field in the durable agent record.
            session.launch.session_id=Some(confirmed.to_owned());
            tx.execute("UPDATE agents SET record=?2 WHERE id=?1",params![current.id.to_string(),encode(&current)?]).map_err(|error|error.to_string())?;
        }
        if snapshot
            .title
            .as_ref()
            .is_none_or(|title| title.trim().is_empty())
        {
            snapshot.title = Some(firing.name.clone());
        }
        snapshot.busy = false;
        snapshot.waiting = false;
        snapshot.ready = false;
        snapshot.exited = true;
        snapshot.requests.clear();
        snapshot.queued_messages.clear();
        snapshot.models.clear();
        snapshot.commands.clear();
        snapshot.skills.clear();
        snapshot.capabilities = Default::default();
        snapshot.has_earlier = false;
        snapshot.has_later = false;
        snapshot.history_loading = false;
        snapshot.read_only_reason = Some("Completed scheduled conversation. The original provider was closed; this saved view starts no provider and resends no prompt.".into());
        // One archived presentation is capped at 4MiB. Keep newest immutable
        // items; full tool transcripts are not copied into the workspace model.
        let original_items = snapshot.items.len();
        let mut bytes = 0usize;
        let mut retained = Vec::new();
        for item in snapshot.items.iter().rev().take(256) {
            let size = serde_json::to_vec(item)
                .map_err(|error| error.to_string())?
                .len();
            if bytes.saturating_add(size) > 3 * 1024 * 1024 {
                break;
            }
            bytes += size;
            retained.push(item.clone());
        }
        retained.reverse();
        snapshot.items = retained;
        if snapshot.items.len() < original_items {
            snapshot.read_only_reason.as_mut().unwrap().push_str(" This bounded saved view contains only the most recent messages; earlier or oversized messages were omitted.");
        }
        snapshot.subagents.clear();
        snapshot.permissions = None;
        snapshot.permission_denials.clear();
        snapshot.composer = None;
        snapshot.prompt_suggestion = None;
        snapshot.open_url = None;
        snapshot.plan.clear();
        snapshot.queue_removals.clear();
        snapshot.branch_points.clear();
        snapshot.forked = None;
        snapshot.file_changes = Default::default();
        crate::agent_execution::truncate(&mut snapshot.detail, 1024);
        for value in [
            &mut snapshot.error,
            &mut snapshot.title,
            &mut snapshot.selected_model,
            &mut snapshot.reasoning,
            &mut snapshot.permission_mode,
        ]
        .into_iter()
        .flatten()
        {
            crate::agent_execution::truncate(value, 4096);
        }
        let record = encode(&snapshot)?;
        if record.len() > 4 * 1024 * 1024 {
            return Err("The saved conversation exceeds the archive metadata limit".into());
        }
        tx.execute(
            "INSERT INTO automation_snapshots(firing,agent,snapshot) VALUES(?1,?2,?3)
            ON CONFLICT(firing) DO UPDATE SET snapshot=excluded.snapshot",
            params![firing.id.to_string(), expected.id.to_string(), record],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())
    }

    /// Saving a completed view is part of release. If storage refuses it,
    /// preserve the exact owner and expose the recoverable blocker explicitly.
    pub(crate) fn preserve_automation_conversation(
        &mut self,
        firing: Uuid,
        agent: &crate::agent_execution::Agent,
        snapshot: crate::chat::Snapshot,
        now: u64,
    ) -> Result<bool, String> {
        match self.archive_automation_conversation(firing, agent, snapshot) {
            Ok(()) => Ok(true),
            Err(error) => {
                self.block_automation_release(firing,format!("The completed conversation could not be saved: {error}. Open the blocking run, then close it explicitly to unblock."),now)?;
                Ok(false)
            }
        }
    }

    pub(crate) fn archived_automation_conversation(
        &self,
        id: crate::model::AgentId,
    ) -> Result<crate::automations::ArchivedConversation, String> {
        let agent = self.agent(id)?;
        if !agent.closed {
            return Err("This conversation is still hosted; attach its original session".into());
        }
        let record: String = self
            .db
            .query_row(
                "SELECT snapshot FROM automation_snapshots WHERE agent=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .map_err(|_| "This conversation's saved history expired".to_owned())?;
        Ok(crate::automations::ArchivedConversation {
            agent,
            snapshot: decode(&record)?,
        })
    }

    pub(crate) fn automation_notifications(
        &self,
    ) -> Result<Vec<crate::notifications::Event>, String> {
        let mut query = self
            .db
            .prepare("SELECT event FROM automation_notifications ORDER BY rowid LIMIT 100")
            .map_err(|error| error.to_string())?;
        query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .map(|record| decode(&record.map_err(|error| error.to_string())?))
            .collect()
    }

    pub(crate) fn acknowledge_automation_notifications(
        &mut self,
        receipts: &std::collections::BTreeSet<String>,
    ) -> Result<(), String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        for key in receipts {
            tx.execute("DELETE FROM automation_notifications WHERE key=?1", [key])
                .map_err(|error| error.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())
    }

    pub(crate) fn prune_automation_history(&mut self, now_ms: u64) -> Result<(), String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        prune_history(&tx, now_ms, None)?;
        tx.commit().map_err(|error| error.to_string())
    }

    pub(crate) fn headless_automation_agents(&self) -> Result<Vec<crate::model::AgentId>, String> {
        // Match the current saved preference. Removed definitions retain their
        // execution metadata so their original sessions remain inspectable.
        let mut query = self
            .db
            .prepare(
                "SELECT g.id FROM agents g JOIN automations a
            ON json_extract(g.record, '$.origin.Automation.id')=a.id
            WHERE json_extract(a.record, '$.definition.visibility')='headless'",
            )
            .map_err(|error| error.to_string())?;
        query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .map(|row| {
                row.map_err(|error| error.to_string())?
                    .parse()
                    .map_err(|error| format!("Invalid agent id: {error}"))
            })
            .collect()
    }

    /// Cleanup can discard private precheck files only after the firing has
    /// settled and released its durable ownership slot. Interrupted work still
    /// owning the slot deliberately retains every proof file.
    pub(crate) fn retired_automation_precheck(&self, id: Uuid) -> Result<Option<u64>, String> {
        let record: Option<String> = self
            .db
            .query_row(
                "SELECT f.record FROM automation_firings f JOIN automations a ON a.id=f.automation
             WHERE f.id=?1 AND (a.active_firing IS NULL OR a.active_firing<>f.id)",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some(record) = record else {
            return Ok(None);
        };
        let firing: Firing = decode(&record)?;
        Ok((!firing.state.is_active() && !firing.release_pending).then_some(firing.revision))
    }

    pub(crate) fn pending_automation_precheck_retirements(&self,after:Option<Uuid>)->Result<Vec<(Uuid,u64)>,String> {
        let mut query=self.db.prepare("SELECT f.id,p.revision FROM automation_precheck_retirements p JOIN automation_firings f ON f.id=p.firing JOIN automations a ON a.id=f.automation WHERE p.retired=0 AND f.id>?1 AND (a.active_firing IS NULL OR a.active_firing<>f.id) AND json_extract(f.record,'$.state') NOT IN ('prechecking','accepted','running','waiting') AND coalesce(json_extract(f.record,'$.release_pending'),0)=0 ORDER BY f.id LIMIT 32").map_err(|error|error.to_string())?;
        query.query_map([after.map(|id|id.to_string()).unwrap_or_default()],|row|Ok((row.get::<_,String>(0)?,row.get::<_,u64>(1)?))).map_err(|error|error.to_string())?
            .map(|row|{let (id,revision)=row.map_err(|error|error.to_string())?;Ok((id.parse::<Uuid>().map_err(|error|error.to_string())?,revision))}).collect()
    }

    /// The broker calls this only after the exact helper's files are retired.
    /// Until then its original firing ID remains the cleanup authority.
    pub(crate) fn coalesce_retired_automation_precheck(&mut self,id:Uuid,revision:u64)->Result<(),String> {
        let tx=self.db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|error|error.to_string())?;
        let record:Option<String>=tx.query_row("SELECT record FROM automation_firings WHERE id=?1",[id.to_string()],|row|row.get(0)).optional().map_err(|error|error.to_string())?;
        let Some(record)=record else {return Ok(());};
        let mut firing:Firing=decode(&record)?;
        if firing.revision!=revision {return Err("The retired precheck revision changed".into());}
        let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM automations WHERE active_firing=?1)",[id.to_string()],|row|row.get(0)).map_err(|error|error.to_string())?;
        if active || firing.state.is_active() || firing.release_pending {return Err("The precheck still owns its automation slot".into());}
        tx.execute("UPDATE automation_precheck_retirements SET retired=1 WHERE firing=?1 AND revision=?2",params![id.to_string(),revision]).map_err(|error|error.to_string())?;
        if firing.state!=FiringState::Skipped || firing.agent.is_some() || firing.run.is_some() {
            tx.commit().map_err(|error|error.to_string())?;return Ok(());
        }
        coalesce_existing_skipped_firing(&tx,&mut firing)?;
        tx.execute("INSERT INTO automation_precheck_retirements(firing,revision,retired) VALUES(?1,?2,1) ON CONFLICT(firing) DO UPDATE SET retired=1",params![firing.id.to_string(),firing.revision]).map_err(|error|error.to_string())?;
        let automation=read_automation(&tx,firing.automation).ok();
        if let Some(automation)=automation {
            // The surviving group ID is the durable notification evidence key.
            // Reordered helper retirement can publish its first receipt late;
            // journal idempotency still delivers only one semantic skip notice.
            queue_automation_notice(&tx,&automation,crate::notifications::Kind::AutomationSkipped,skipped_reason(&firing),&format!("skip:{}",firing.id))?;
        }
        tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0",[]).map_err(|error|error.to_string())?;
        tx.commit().map_err(|error|error.to_string())
    }

    /// A confirmed retry reads its original receipt even after removal of the
    /// definition or target. Reusing the key for another revision is an error.
    pub(crate) fn previous_manual_automation_firing(
        &self,
        id: Uuid,
        revision: u64,
        request: Uuid,
    ) -> Result<Option<Firing>, String> {
        let previous = read_firing_key(
            &self.db,
            id,
            &occurrence_key(revision, &crate::automations::Trigger::Manual { request }),
        )?;
        if previous.is_none() {
            require_unretired_request(
                &self.db,
                id,
                &occurrence_key(revision, &crate::automations::Trigger::Manual { request }),
            )?;
        }
        if previous
            .as_ref()
            .is_some_and(|firing| firing.revision != revision)
        {
            return Err(
                "This manual request ID already belongs to another automation revision".into(),
            );
        }
        Ok(previous)
    }

    pub(crate) fn automation_firing(&self, id: Uuid) -> Result<Firing, String> {
        read_firing(&self.db, id)
    }

    pub(crate) fn cancel_automation_precheck(
        &mut self,
        automation_id: Uuid,
        firing_id: Uuid,
    ) -> Result<Firing, String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let automation = read_automation(&tx, automation_id)?;
        let mut firing = read_firing(&tx, firing_id)?;
        if automation.active_firing != Some(firing_id)
            || firing.automation != automation_id
            || !matches!(
                firing.state,
                FiringState::Prechecking | FiringState::Interrupted
            )
            || firing.run.is_some()
        {
            return Err("The precheck changed before cancellation; inspect its current run".into());
        }
        firing.cancel_requested = true;
        write_firing(&tx, &firing)?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(firing)
    }

    pub(crate) fn automation_run_detail(
        &self,
        id: Uuid,
    ) -> Result<crate::automations::RunDetail, String> {
        let mut firing = read_firing(&self.db, id)?;
        let mut definition = read_definition(&self.db, firing.automation, firing.revision)?;
        if firing.name.is_empty() {
            firing.name = definition.name.clone();
        }
        definition.name = firing.name.clone();
        let agent = firing.agent.map(|id| self.agent(id)).transpose()?;
        let run = firing.run.map(|id| self.agent_run(id)).transpose()?;
        if run
            .as_ref()
            .is_some_and(|run| Some(run.agent) != firing.agent)
            || agent.as_ref().is_some_and(|agent| {
                agent.origin
                    != (crate::agent_execution::Origin::Automation {
                        id: firing.automation,
                    })
            })
        {
            return Err("Automation history and execution identity disagree".into());
        }
        Ok(crate::automations::RunDetail {
            definition,
            firing,
            agent,
            run,
        })
    }

    pub(crate) fn automation_detail_for_run(
        &self,
        run: crate::model::RunId,
    ) -> Result<crate::automations::RunDetail, String> {
        let id: String = self
            .db
            .query_row(
                "SELECT id FROM automation_firings WHERE run_id=?1",
                [run.to_string()],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        self.automation_run_detail(
            id.parse()
                .map_err(|_| "Stored automation firing identity is invalid")?,
        )
    }

    /// The service discovered invalid authority before attempting dispatch.
    /// Refuse this shortcut once a launch claim exists: cleanup is then owned
    /// by the execution service and cannot be inferred from an error here.
    pub(crate) fn fail_automation_start(
        &mut self,
        firing_id: Uuid,
        mut diagnostic: String,
        now_ms: u64,
        pause: bool,
    ) -> Result<(), String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let mut firing = read_firing(&tx, firing_id)?;
        let mut automation = read_automation(&tx, firing.automation)?;
        if automation.active_firing != Some(firing_id)
            || !matches!(
                firing.state,
                FiringState::Accepted | FiringState::Prechecking
            )
        {
            return Err("Automation no longer owns this unstarted firing".into());
        }
        crate::agent_execution::truncate(
            &mut diagnostic,
            crate::agent_execution::MAX_DIAGNOSTIC_BYTES,
        );
        if let Some(run_id) = firing.run {
            let claimed: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM agent_dispatches WHERE run=?1)",
                    [run_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            if claimed {
                return Err("The execution service already owns this launch".into());
            }
            let mut run: crate::agent_execution::Run = decode(
                &tx.query_row(
                    "SELECT record FROM agent_runs WHERE id=?1",
                    [run_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|e| e.to_string())?,
            )?;
            if run.state != crate::agent_execution::State::Accepted {
                return Err("The accepted run changed before start failure was recorded".into());
            }
            run.state = crate::agent_execution::State::Failed;
            run.revision = run.revision.checked_add(1).ok_or("Run revision overflow")?;
            run.finished_ms = Some(now_ms);
            run.result.diagnostic = Some(diagnostic.clone());
            tx.execute(
                "UPDATE agent_runs SET state='failed',revision=?2,record=?3 WHERE id=?1",
                params![run_id.to_string(), run.revision, encode(&run)?],
            )
            .map_err(|e| e.to_string())?;
            let mut agent: crate::agent_execution::Agent = decode(
                &tx.query_row(
                    "SELECT record FROM agents WHERE id=?1",
                    [run.agent.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|e| e.to_string())?,
            )?;
            if agent.session.is_none() {
                agent.closed = true;
                tx.execute(
                    "UPDATE agents SET closed=1,record=?2 WHERE id=?1",
                    params![agent.id.to_string(), encode(&agent)?],
                )
                .map_err(|e| e.to_string())?;
                if automation.reuse_agent == Some(agent.id) {
                    automation.reuse_agent = None;
                }
            }
        }
        if firing.run.is_none() && firing.state==FiringState::Prechecking {
            tx.execute("UPDATE automation_precheck_retirements SET retired=1 WHERE firing=?1",[firing.id.to_string()]).map_err(|error|error.to_string())?;
        }
        firing.state = FiringState::Failed;
        firing.finished_ms = Some(now_ms);
        firing.diagnostic = Some(diagnostic);
        automation.active_firing = None;
        automation.updated_ms = now_ms;
        if pause && automation.execution_revision == firing.revision {
            queue_automation_notice(
                &tx,
                &automation,
                crate::notifications::Kind::AutomationPaused,
                "The run could not start; inspect its history, then confirm Enable to retry",
                &format!("paused:{}", automation.revision),
            )?;
            automation.enabled = false;
            automation.next = None;
        }
        write_firing(&tx, &firing)?;
        write_automation(&tx, &automation)?;
        tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
            .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    pub(crate) fn automation_project_kind(
        &self,
        repository: &std::path::Path,
    ) -> Result<crate::model::ProjectKind, String> {
        let kind: String = self
            .db
            .query_row(
                "SELECT kind FROM projects WHERE repository=?1",
                [super::repository_key(repository)?],
                |row| row.get(0),
            )
            .map_err(|error| format!("The automation project is unavailable: {error}"))?;
        match kind.as_str() {
            "git" => Ok(crate::model::ProjectKind::Git),
            "folder" => Ok(crate::model::ProjectKind::Folder),
            _ => Err("The automation project has an unsupported kind".into()),
        }
    }

    pub(crate) fn active_automation_firings(&self) -> Result<Vec<Firing>, String> {
        let mut query = self.db.prepare("SELECT f.record FROM automations a JOIN automation_firings f ON f.id=a.active_firing ORDER BY f.created_ms,f.id")
            .map_err(|error| error.to_string())?;
        let rows = query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        rows.into_iter().map(|record| decode(&record)).collect()
    }

    /// Precheck-only ownership has no hosted session in ordinary update
    /// inventory. Include uncertain cleanup until its exact receipt releases
    /// the active slot. Caller queries outside HostState under the update gate.
    pub(crate) fn automation_update_blockers(&self) -> Result<Vec<String>, String> {
        let limit = crate::automations::MAX_CONCURRENT_PRECHECKS;
        let mut query = self.db.prepare(
            "SELECT f.record FROM automations a JOIN automation_firings f ON f.id = a.active_firing
             WHERE f.run_id IS NULL ORDER BY f.created_ms, f.id LIMIT ?1",
        ).map_err(|error| error.to_string())?;
        let records = query
            .query_map([limit as u64 + 1], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        let mut blockers = records
            .iter()
            .take(limit)
            .map(|record| {
                let firing: Firing = decode(record)?;
                Ok(format!(
                    "Automation ‘{}’ ({}) has a precheck or cleanup still pending",
                    firing.name, firing.id
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        if records.len() > limit {
            blockers.push("Additional automation prechecks must settle before updating".into());
        }
        Ok(blockers)
    }

    /// A dead owner without a durable cleanup receipt is not a failed shell:
    /// descendants or command effects may remain. Preserve the capacity claim.
    pub(crate) fn interrupt_automation_precheck(
        &mut self,
        firing_id: Uuid,
        mut diagnostic: String,
        now_ms: u64,
    ) -> Result<(), String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let mut firing = read_firing(&tx, firing_id)?;
        let mut automation = read_automation(&tx, firing.automation)?;
        if automation.active_firing != Some(firing.id)
            || firing.run.is_some()
            || !matches!(
                firing.state,
                FiringState::Prechecking | FiringState::Interrupted
            )
        {
            return Err("Precheck no longer owns this automation".into());
        }
        crate::agent_execution::truncate(
            &mut diagnostic,
            crate::agent_execution::MAX_DIAGNOSTIC_BYTES,
        );
        firing.state = FiringState::Interrupted;
        firing.finished_ms = Some(now_ms);
        firing.diagnostic = Some(diagnostic.clone());
        automation.blocked = Some(diagnostic);
        automation.enabled = false;
        automation.next = None;
        automation.updated_ms = now_ms;
        write_firing(&tx, &firing)?;
        write_automation(&tx, &automation)?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// Called after the execution service proves close/idle at this exact
    /// descriptor. A newer accepted prompt cannot free an older firing's slot.
    pub(crate) fn finish_automation_release(
        &mut self,
        firing_id: Uuid,
        expected: &crate::agent_execution::Agent,
        now_ms: u64,
    ) -> Result<(), String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let mut firing = read_firing(&tx, firing_id)?;
        let mut automation = read_automation(&tx, firing.automation)?;
        let agent: crate::agent_execution::Agent = decode(
            &tx.query_row(
                "SELECT record FROM agents WHERE id=?1",
                [expected.id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map_err(|e| e.to_string())?,
        )?;
        let latest: String = tx
            .query_row(
                "SELECT id FROM agent_runs WHERE agent=?1 ORDER BY rowid DESC LIMIT 1",
                [agent.id.to_string()],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if &agent != expected
            || firing.agent != Some(agent.id)
            || firing.run.map(|id| id.to_string()).as_deref() != Some(latest.as_str())
            || automation.active_firing != Some(firing.id)
            || !firing.release_pending
            || firing.state.is_active()
            || agent.origin != (crate::agent_execution::Origin::Automation { id: automation.id })
        {
            return Err("Automation session changed before its ownership could be released".into());
        }
        let definition = read_definition(&tx, automation.id, firing.revision)?;
        if !agent.closed
            && (definition.session_mode == crate::automations::SessionMode::Fresh
                || firing.state == FiringState::Interrupted
                || agent.session.is_none())
        {
            return Err("The automation session still requires a proved close".into());
        }
        if !agent.closed
            && tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM agent_policies WHERE agent=?1 AND interactive=1)",
                    [agent.id.to_string()],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(|error| error.to_string())?
        {
            return Err(
                "Interactive work took ownership before the automation could release its slot"
                    .into(),
            );
        }
        firing.release_pending = false;
        automation.active_firing = None;
        automation.blocked = None;
        automation.updated_ms = now_ms;
        if agent.closed && automation.reuse_agent == Some(agent.id) {
            automation.reuse_agent = None;
        }
        write_firing(&tx, &firing)?;
        write_automation(&tx, &automation)?;
        tx.commit().map_err(|e| e.to_string())
    }

    pub(crate) fn block_automation_release(
        &mut self,
        firing_id: Uuid,
        diagnostic: String,
        now_ms: u64,
    ) -> Result<(), String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let firing = read_firing(&tx, firing_id)?;
        let mut automation = read_automation(&tx, firing.automation)?;
        if automation.active_firing != Some(firing.id) || !firing.release_pending {
            return Err("Automation no longer owns this cleanup".into());
        }
        let mut diagnostic = diagnostic;
        crate::agent_execution::truncate(
            &mut diagnostic,
            crate::agent_execution::MAX_DIAGNOSTIC_BYTES,
        );
        if automation.blocked.as_ref() == Some(&diagnostic) && !automation.enabled {
            return Ok(());
        }
        automation.blocked = Some(diagnostic);
        automation.enabled = false;
        automation.next = None;
        automation.updated_ms = now_ms;
        write_automation(&tx, &automation)?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// The execution service calls this inside its dispatch-claim transaction.
    /// Acceptance alone is not permission to launch after a schedule changed.
    pub(super) fn automation_dispatch_allowed_in(
        tx: &Transaction<'_>,
        accepted: &crate::agent_execution::Accepted,
        now_ms: u64,
    ) -> Result<bool, String> {
        let crate::agent_execution::Origin::Automation { id } = &accepted.agent.origin else {
            return Ok(true);
        };
        let record = tx
            .query_row(
                "SELECT record FROM automation_firings WHERE run_id=?1",
                [accepted.run.id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .ok_or("The scheduled run no longer has an owning firing")?;
        let mut firing: Firing = decode(&record)?;
        let mut automation = read_automation(tx, *id)?;
        if firing.automation != *id
            || firing.state != FiringState::Accepted
            || automation.active_firing != Some(firing.id)
            || firing.agent != Some(accepted.agent.id)
        {
            return Err("The accepted run and automation firing ownership disagree".into());
        }
        if automation.execution_revision != firing.revision
            || automation.blocked.is_some()
            || (!automation.enabled
                && matches!(
                    firing.trigger,
                    crate::automations::Trigger::Scheduled { .. }
                ))
        {
            // The caller proved no dispatch claim exists in this transaction.
            // This is a known cancellation, not an uncertain provider outcome.
            let diagnostic = "The automation changed or was disabled before this run started";
            let mut run = accepted.run.clone();
            run.state = crate::agent_execution::State::Cancelled;
            run.revision = run
                .revision
                .checked_add(1)
                .ok_or("Agent run revision overflow")?;
            run.finished_ms = Some(now_ms);
            run.result.diagnostic = Some(diagnostic.into());
            tx.execute(
                "UPDATE agent_runs SET state='cancelled',revision=?2,record=?3 WHERE id=?1",
                params![run.id.to_string(), run.revision, encode(&run)?],
            )
            .map_err(|error| error.to_string())?;
            if accepted.agent.session.is_none() {
                let mut agent = accepted.agent.clone();
                agent.closed = true;
                tx.execute(
                    "UPDATE agents SET closed=1,record=?2 WHERE id=?1",
                    params![agent.id.to_string(), encode(&agent)?],
                )
                .map_err(|error| error.to_string())?;
                if automation.reuse_agent == Some(agent.id) {
                    automation.reuse_agent = None;
                }
            }
            firing.state = FiringState::Cancelled;
            firing.finished_ms = Some(now_ms);
            firing.diagnostic = Some(diagnostic.into());
            automation.active_firing = None;
            automation.updated_ms = now_ms;
            write_firing(tx, &firing)?;
            write_automation(tx, &automation)?;
            tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
                .map_err(|error| error.to_string())?;
            return Ok(false);
        }
        Ok(true)
    }

    /// Read only definitions whose persisted deadline is due. The scheduler
    /// sleeps until the earliest deadline and uses bounded batches on wake.
    pub(crate) fn due_automations(
        &self,
        now_ms: u64,
        limit: usize,
    ) -> Result<Vec<Automation>, String> {
        let mut query = self.db.prepare("SELECT record FROM automations WHERE deleted_ms IS NULL AND enabled=1 AND next_due_ms<=?1 ORDER BY next_due_ms,id LIMIT ?2")
            .map_err(|error| error.to_string())?;
        let records = query
            .query_map(params![now_ms, limit.clamp(1, MAX_HISTORY_PAGE)], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| error.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        records.into_iter().map(|record| decode(&record)).collect()
    }

    pub(crate) fn next_automation_deadline(&self) -> Result<Option<u64>, String> {
        self.db
            .query_row(
                "SELECT min(next_due_ms) FROM automations WHERE deleted_ms IS NULL AND enabled=1",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }

    /// One record summarizes missed deadlines regardless of downtime length.
    /// `next` may be the latest due occurrence still eligible for catch-up;
    /// the following claim then fences that new exact persisted deadline.
    pub(crate) fn skip_automation_deadlines(
        &mut self,
        id: Uuid,
        revision: u64,
        expected: &Occurrence,
        next: Occurrence,
        now_ms: u64,
    ) -> Result<Firing, String> {
        use crate::automations::{SkipReason, Trigger};
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let mut automation = read_automation(&tx, id)?;
        if !automation.enabled
            || automation.execution_revision != revision
            || automation.next.as_ref() != Some(expected)
            || expected.due_ms > now_ms
            || next.due_ms <= expected.due_ms
        {
            return Err("Missed schedule snapshot is no longer current".into());
        }
        let trigger = Trigger::Scheduled {
            occurrence: expected.clone(),
        };
        let key = occurrence_key(revision, &trigger);
        let mut firing = Firing {
            id: Uuid::new_v4(),
            automation: id,
            name: automation.definition.name.clone(),
            revision,
            trigger,
            state: FiringState::Skipped,
            release_pending: false,
            cancel_requested: false,
            created_ms: now_ms,
            started_ms: None,
            finished_ms: Some(now_ms),
            agent: None,
            run: None,
            skip: Some(SkipReason::MissedDeadline),
            skipped_occurrences: None,
            missed_interval: Some(crate::automations::MissedInterval {
                first_due_ms: expected.due_ms,
                until_exclusive_ms: next.due_ms,
                intervals: 1,
            }),
            diagnostic: Some(format!(
                "Older scheduled occurrences were skipped. Next eligible occurrence: {} ({})",
                next.local, automation.definition.schedule.timezone
            )),
        };
        insert_or_coalesce_firing(&tx, &mut firing, &key)?;
        if firing
            .missed_interval
            .as_ref()
            .is_none_or(|summary| summary.intervals == 1)
        {
            queue_automation_notice(
                &tx,
                &automation,
                crate::notifications::Kind::AutomationSkipped,
                "Scheduled occurrences were missed while the scheduler was unavailable",
                &format!("skip:{}", firing.id),
            )?;
        }
        automation.next = Some(next);
        automation.updated_ms = now_ms;
        write_automation(&tx, &automation)?;
        prune_history(&tx, now_ms, Some(id))?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(firing)
    }

    /// Mirror the execution service's durable result, never a busy/idle guess
    /// or an outcome supplied by a scheduler callback. Reading both records in
    /// this transaction also prevents a delayed callback regressing history.
    pub(crate) fn reconcile_automation_run(
        &mut self,
        run_id: crate::model::RunId,
        now_ms: u64,
    ) -> Result<Option<Firing>, String> {
        use crate::agent_execution::{Run, State};
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let record = tx
            .query_row(
                "SELECT record FROM automation_firings WHERE run_id=?1",
                [run_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some(record) = record else {
            return Ok(None);
        };
        let mut firing: Firing = decode(&record)?;
        let run: Run = decode(
            &tx.query_row(
                "SELECT record FROM agent_runs WHERE id=?1",
                [run_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| error.to_string())?,
        )?;
        if firing.agent != Some(run.agent) {
            return Err("Stored automation and execution ownership disagree".into());
        }
        let state = match run.state {
            State::Accepted => FiringState::Accepted,
            State::Running => FiringState::Running,
            State::Waiting => FiringState::Waiting,
            State::Succeeded => FiringState::Succeeded,
            State::Failed => FiringState::Failed,
            State::Cancelled => FiringState::Cancelled,
            State::Interrupted => FiringState::Interrupted,
        };
        if firing.state == state
            && firing.started_ms == run.started_ms
            && firing.finished_ms == run.finished_ms
        {
            return Ok(Some(firing));
        }
        let mut automation = read_automation(&tx, firing.automation)?;
        if automation.active_firing != Some(firing.id) {
            return Err("Execution no longer owns the automation's active slot".into());
        }
        firing.state = state;
        firing.release_pending = !state.is_active();
        firing.started_ms = run.started_ms;
        firing.finished_ms = run.finished_ms;
        firing.diagnostic = run.result.diagnostic;
        if state == FiringState::Interrupted {
            automation.blocked = Some("The run's outcome or ownership is uncertain. Inspect and close it before scheduling more work.".into());
            automation.enabled = false;
            automation.next = None;
            // Keep the exact firing as the unresolved owner even though its
            // execution result is terminal. No later occurrence may overlap.
        }
        automation.updated_ms = now_ms;
        write_firing(&tx, &firing)?;
        write_automation(&tx, &automation)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(Some(firing))
    }

    pub(crate) fn create_automation(
        &mut self,
        definition: Definition,
        now_ms: u64,
        target_identity: crate::automations::DirectoryIdentity,
    ) -> Result<Automation, String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let automation = create_automation_in(&tx, definition, now_ms, target_identity, None)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(automation)
    }

    pub(crate) fn automation(&self, id: Uuid) -> Result<Automation, String> {
        read_automation(&self.db, id)
    }

    pub(crate) fn automations(
        &self,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<Summary>, String> {
        let mut query = self.db.prepare("SELECT summary FROM automations WHERE deleted_ms IS NULL AND (?1 IS NULL OR id > ?1) ORDER BY id LIMIT ?2")
            .map_err(|error| error.to_string())?;
        let records = query
            .query_map(
                params![
                    after.map(|id| id.to_string()),
                    limit.clamp(1, MAX_HISTORY_PAGE)
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| error.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        records.into_iter().map(|record| decode(&record)).collect()
    }

    pub(crate) fn project_automations(
        &self,
        repository: &std::path::Path,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<Summary>, String> {
        let mut query = self
            .db
            .prepare(
                "SELECT summary FROM automations WHERE deleted_ms IS NULL
            AND repository=?1 AND (?2 IS NULL OR id>?2) ORDER BY id LIMIT ?3",
            )
            .map_err(|error| error.to_string())?;
        let rows = query
            .query_map(
                params![
                    super::repository_key(repository)?,
                    after.map(|id| id.to_string()),
                    limit.clamp(1, MAX_HISTORY_PAGE)
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| error.to_string())?;
        rows.map(|row| decode(&row.map_err(|error| error.to_string())?))
            .collect()
    }

    /// Every material edit pauses execution until the user reviews it again.
    /// A name-only change preserves consent and the existing deadline.
    pub(crate) fn edit_automation(
        &mut self,
        id: Uuid,
        expected_revision: u64,
        definition: Definition,
        now_ms: u64,
        target_identity: crate::automations::DirectoryIdentity,
    ) -> Result<Automation, String> {
        definition.validate()?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let mut automation = read_automation(&tx, id)?;
        require_revision(&automation, expected_revision)?;
        require_project(&tx, &definition)?;
        let mut without_name = definition.clone();
        without_name.name = automation.definition.name.clone();
        without_name.visibility = automation.definition.visibility;
        // Saving the unchanged target after its directory was replaced is an
        // explicit refresh of that definition; it must pause for new consent.
        // A presentation-only rename never refreshes filesystem authority.
        let material = without_name != automation.definition
            || (definition == automation.definition
                && target_identity != automation.target_identity);
        let new_agent = definition.agent != automation.definition.agent
            || definition.session_mode != automation.definition.session_mode
            || (material && target_identity != automation.target_identity);
        let new_target = definition.agent.target != automation.definition.agent.target;
        if material && (automation.active_firing.is_some() || automation.blocked.is_some()) {
            return Err(
                "Resolve the active or interrupted run before changing its execution definition"
                    .into(),
            );
        }
        if new_agent {
            require_closed_reuse(&tx, &automation)?;
        }
        automation.revision = automation
            .revision
            .checked_add(1)
            .ok_or("Automation revision overflow")?;
        automation.definition = definition;
        automation.updated_ms = now_ms;
        if material {
            automation.target_identity = target_identity;
            automation.execution_revision = automation.revision;
            automation.enabled = false;
            automation.next = None;
            if new_agent {
                // A changed provider/account/target must not resume a
                // conversation accepted under a different session contract.
                automation.reuse_agent = None;
            }
            if new_target {
                automation.channel = automation.definition.agent.target.channel();
            }
        }
        write_revision(&tx, &automation)?;
        write_automation(&tx, &automation)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(automation)
    }

    /// The service computes the next occurrence from the validated schedule
    /// before opening this transaction. Enabling requires explicit consent.
    pub(crate) fn set_automation_enabled(
        &mut self,
        id: Uuid,
        expected_revision: u64,
        next: Option<Occurrence>,
        confirmed: bool,
        now_ms: u64,
    ) -> Result<Automation, String> {
        if next.is_some() && !confirmed {
            return Err("Confirm unattended execution before enabling this automation".into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let mut automation = read_automation(&tx, id)?;
        require_revision(&automation, expected_revision)?;
        if next.is_some() {
            require_project(&tx, &automation.definition)?;
            if automation.blocked.is_some() {
                return Err("Resolve the interrupted run before enabling this automation".into());
            }
        }
        automation.enabled = next.is_some();
        automation.next = next;
        automation.revision = automation
            .revision
            .checked_add(1)
            .ok_or("Automation revision overflow")?;
        automation.execution_revision = automation.revision;
        automation.updated_ms = now_ms;
        write_revision(&tx, &automation)?;
        write_automation(&tx, &automation)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(automation)
    }

    /// Tombstones disappear from the active list but cannot be reaccepted by
    /// an old queued request. Immutable firing history remains inspectable.
    pub(crate) fn remove_automation(
        &mut self,
        id: Uuid,
        expected_revision: u64,
        now_ms: u64,
    ) -> Result<(), String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let automation = read_automation(&tx, id)?;
        require_revision(&automation, expected_revision)?;
        if automation.active_firing.is_some() || automation.blocked.is_some() {
            return Err("Cancel and confirm the active or interrupted run has closed before removing this automation".into());
        }
        require_closed_reuse(&tx, &automation)?;
        tx.execute("UPDATE automations SET deleted_ms=?2,enabled=0,next_due_ms=NULL,revision=revision+1,updated_ms=?2 WHERE id=?1", params![id.to_string(), now_ms])
            .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())
    }

    pub(crate) fn automation_history(
        &self,
        id: Option<Uuid>,
        before: Option<HistoryCursor>,
        limit: usize,
    ) -> Result<HistoryPage, String> {
        let limit = limit.clamp(1, MAX_HISTORY_PAGE);
        let mut query = self
            .db
            .prepare(if id.is_some() {
                "SELECT f.record,json_extract(r.definition,'$.name') FROM automation_firings f JOIN automation_revisions r ON r.automation=f.automation AND r.revision=f.revision WHERE f.automation=?1
            AND (?2 IS NULL OR f.created_ms < ?2 OR (f.created_ms = ?2 AND f.id < ?3))
            ORDER BY f.created_ms DESC,f.id DESC LIMIT ?4"
            } else {
                "SELECT f.record,json_extract(r.definition,'$.name') FROM automation_firings f JOIN automation_revisions r ON r.automation=f.automation AND r.revision=f.revision WHERE ?1 IS NULL
            AND (?2 IS NULL OR f.created_ms < ?2 OR (f.created_ms = ?2 AND f.id < ?3))
            ORDER BY f.created_ms DESC,f.id DESC LIMIT ?4"
            })
            .map_err(|error| error.to_string())?;
        let records = query
            .query_map(
                params![
                    id.map(|id| id.to_string()),
                    before.as_ref().map(|cursor| cursor.created_ms),
                    before.map(|cursor| cursor.id.to_string()),
                    limit + 1
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|error| error.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        let mut firings = records
            .into_iter()
            .map(|(record, name)| {
                decode::<Firing>(&record).map(|mut firing| {
                    if firing.name.is_empty() {
                        firing.name = name;
                    }
                    firing
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let more = firings.len() > limit;
        firings.truncate(limit);
        let next = more
            .then(|| {
                firings.last().map(|last| HistoryCursor {
                    created_ms: last.created_ms,
                    id: last.id,
                })
            })
            .flatten();
        Ok(HistoryPage { firings, next })
    }
}

/// The proposal receipt and activation use the same transaction as creation.
pub(super) fn create_automation_in(
    tx: &Transaction<'_>,
    definition: Definition,
    now_ms: u64,
    target_identity: crate::automations::DirectoryIdentity,
    next: Option<Occurrence>,
) -> Result<Automation, String> {
    definition.validate()?;
    require_project(tx, &definition)?;
    let channel = definition.agent.target.channel();
    let automation = Automation {
        id: Uuid::new_v4(),
        revision: 1,
        execution_revision: 1,
        definition,
        target_identity,
        enabled: next.is_some(),
        created_ms: now_ms,
        updated_ms: now_ms,
        next,
        active_firing: None,
        reuse_agent: None,
        channel,
        blocked: None,
    };
    tx.execute(
        "INSERT INTO automations(id,revision,record,summary,enabled,next_due_ms,updated_ms,repository)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![automation.id.to_string(), automation.revision, encode(&automation)?,
            encode(&automation.summary(None))?, automation.enabled,
            automation.next.as_ref().map(|next| next.due_ms), now_ms,
            super::repository_key(&automation.definition.agent.target.repository)?],
    ).map_err(|error| error.to_string())?;
    write_revision(tx, &automation)?;
    Ok(automation)
}

fn read_automation(db: &Connection, id: Uuid) -> Result<Automation, String> {
    let record = db
        .query_row(
            "SELECT record FROM automations WHERE id=?1 AND deleted_ms IS NULL",
            [id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or("Automation was removed or does not exist")?;
    decode(&record)
}

fn write_automation(tx: &Transaction<'_>, automation: &Automation) -> Result<(), String> {
    let previous: Option<String> = tx
        .query_row(
            "SELECT record FROM automations WHERE id=?1",
            [automation.id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some(previous) = previous {
        let previous: Automation = decode(&previous)?;
        if previous.definition.visibility != automation.definition.visibility {
            tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
                .map_err(|error| error.to_string())?;
        }
        if previous.blocked != automation.blocked && automation.blocked.is_some() {
            queue_automation_notice(
                tx,
                automation,
                crate::notifications::Kind::AutomationBlocked,
                if automation.active_firing.and_then(|id| read_firing(tx,id).ok())
                    .is_some_and(|firing| firing.state==FiringState::Interrupted) {
                    "A run or precheck was interrupted; inspect and close the blocking run before resuming"
                } else {
                    "Run cleanup or its saved result needs attention; inspect and close the blocking run before resuming"
                },
                &format!("blocked:{}", Uuid::new_v4()),
            )?;
        } else if previous.enabled && !automation.enabled {
            let project_open: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM projects WHERE repository=?1)",
                    [super::repository_key(
                        &automation.definition.agent.target.repository,
                    )?],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            let reason = if !project_open {
                "The project was closed; reopen it and confirm Enable to resume".to_owned()
            } else {
                match automation.active_firing.and_then(|id| read_firing(tx,id).ok()).map(|firing| firing.state) {
                    Some(FiringState::Interrupted) => "A run or precheck was interrupted; inspect its history before enabling again".into(),
                    Some(FiringState::Failed) => "The run failed to start or finish; inspect its history before enabling again".into(),
                    _ => "The automation was paused; confirm Enable to resume".into(),
                }
            };
            queue_automation_notice(
                tx,
                automation,
                crate::notifications::Kind::AutomationPaused,
                &reason,
                &format!("paused:{}", automation.revision),
            )?;
        }
    }

    let last: Option<String> = tx.query_row("SELECT record FROM automation_firings WHERE automation=?1 ORDER BY created_ms DESC,id DESC LIMIT 1", [automation.id.to_string()], |row| row.get(0))
        .optional().map_err(|error| error.to_string())?;
    let last_state = last
        .map(|record| decode::<Firing>(&record).map(|firing| firing.state))
        .transpose()?;
    tx.execute("UPDATE automations SET revision=?2,record=?3,summary=?4,enabled=?5,next_due_ms=?6,updated_ms=?7,active_firing=?8,repository=?9 WHERE id=?1 AND deleted_ms IS NULL",
        params![automation.id.to_string(), automation.revision, encode(automation)?, encode(&automation.summary(last_state))?, automation.enabled, automation.next.as_ref().map(|next| next.due_ms), automation.updated_ms, automation.active_firing.map(|id| id.to_string()), super::repository_key(&automation.definition.agent.target.repository)?])
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn write_revision(tx: &Transaction<'_>, automation: &Automation) -> Result<(), String> {
    tx.execute(
        "INSERT INTO automation_revisions(automation,revision,definition,target_identity) VALUES(?1,?2,?3,?4)",
        params![
            automation.id.to_string(),
            automation.revision,
            encode(&automation.definition)?,
            encode(&automation.target_identity)?
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn require_closed_reuse(tx: &Transaction<'_>, automation: &Automation) -> Result<(), String> {
    let Some(agent) = automation.reuse_agent else {
        return Ok(());
    };
    let closed: bool = tx
        .query_row(
            "SELECT closed FROM agents WHERE id=?1",
            [agent.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if !closed {
        return Err("Close the automation's idle provider before changing its launch settings or removing it".into());
    }
    Ok(())
}

fn require_revision(automation: &Automation, expected: u64) -> Result<(), String> {
    if automation.revision != expected {
        return Err("Automation changed; reload it before applying this action".into());
    }
    Ok(())
}

pub(super) fn require_project(tx: &Transaction<'_>, definition: &Definition) -> Result<(), String> {
    let repository = super::repository_key(&definition.agent.target.repository)?;
    let exists: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE repository=?1)",
            [repository],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if !exists {
        return Err("The automation's project is no longer open".into());
    }
    if let Some(placement) = &definition.agent.target.placement {
        super::execution::require_placement(tx, placement, &definition.agent.target.repository)?;
    }
    if let Some(channel) = definition.agent.target.channel() {
        let owned: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM channels WHERE id=?1 AND repository=?2)",
                params![
                    channel.to_string(),
                    super::repository_key(&definition.agent.target.repository)?
                ],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if !owned {
            return Err(
                "The automation's selected channel no longer belongs to its project".into(),
            );
        }
    }
    Ok(())
}

fn encode(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}
fn decode<T: DeserializeOwned>(value: &str) -> Result<T, String> {
    serde_json::from_str(value)
        .map_err(|error| format!("Stored automation record is invalid: {error}"))
}

pub(crate) struct Claimed {
    pub firing: Firing,
    pub definition: Definition,
    pub target_identity: crate::automations::DirectoryIdentity,
    pub accepted: Option<crate::agent_execution::Accepted>,
    pub created: bool,
}

impl Registry {
    /// Claims one exact schedule snapshot. The service has already bounded
    /// catch-up and calculated `next`; a concurrent edit cannot adopt its work.
    pub(crate) fn claim_automation(
        &mut self,
        id: Uuid,
        expected_execution_revision: u64,
        expected_next: Option<&Occurrence>,
        trigger: crate::automations::Trigger,
        next: Option<Occurrence>,
        confirmed_manual: bool,
        now_ms: u64,
    ) -> Result<Claimed, String> {
        use crate::automations::{SkipReason, Trigger};
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let mut automation = read_automation(&tx, id)?;
        let key = occurrence_key(expected_execution_revision, &trigger);
        if let Some(firing) = read_firing_key(&tx, id, &key)? {
            if firing.revision != expected_execution_revision {
                return Err(
                    "This request ID already belongs to another automation revision".into(),
                );
            }
            let definition = read_definition(&tx, id, firing.revision)?;
            let target_identity = read_target_identity(&tx, id, firing.revision)?;
            return Ok(Claimed {
                firing,
                definition,
                target_identity,
                accepted: None,
                created: false,
            });
        }
        if matches!(trigger, Trigger::Manual { .. }) {
            require_unretired_request(&tx, id, &key)?;
        }
        if automation.execution_revision != expected_execution_revision {
            return Err(
                "Automation execution definition changed before its firing was claimed".into(),
            );
        }
        match &trigger {
            Trigger::Scheduled { occurrence } => {
                if !automation.enabled
                    || automation.next.as_ref() != expected_next
                    || expected_next.is_none_or(|expected| occurrence.due_ms < expected.due_ms)
                    || occurrence.due_ms > now_ms
                    || next.as_ref().is_none_or(|next| next.due_ms <= now_ms)
                {
                    return Err("Scheduled occurrence is no longer current".into());
                }
                automation.next = next;
            }
            Trigger::Manual { .. } if !confirmed_manual => {
                return Err("Confirm unattended execution before running this automation".into());
            }
            Trigger::Manual { .. } => {}
        }
        if automation.blocked.is_some() {
            return Err("Resolve the interrupted run before starting more work".into());
        }
        let project_open: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE repository=?1)",
                [super::repository_key(
                    &automation.definition.agent.target.repository,
                )?],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let mut capacity_full = false;
        if automation.active_firing.is_none() && automation.definition.precheck.is_some() {
            let count: usize = tx.query_row("SELECT count(*) FROM automations a JOIN automation_firings f ON f.id=a.active_firing WHERE f.run_id IS NULL", [], |row| row.get(0)).map_err(|error| error.to_string())?;
            if count >= crate::automations::MAX_CONCURRENT_PRECHECKS {
                if matches!(trigger, Trigger::Manual { .. }) {
                    return Err("All precheck slots are occupied; wait for running checks or resolve interrupted owners".into());
                }
                capacity_full = true;
            }
        }
        let mut firing = Firing {
            id: Uuid::new_v4(),
            automation: id,
            name: automation.definition.name.clone(),
            revision: automation.execution_revision,
            trigger,
            state: FiringState::Prechecking,
            release_pending: false,
            cancel_requested: false,
            created_ms: now_ms,
            started_ms: None,
            finished_ms: None,
            agent: None,
            run: None,
            skip: None,
            skipped_occurrences: None,
            missed_interval: None,
            diagnostic: None,
        };
        let definition = automation.definition.clone();
        if !project_open {
            firing.state = FiringState::Failed;
            firing.finished_ms = Some(now_ms);
            firing.diagnostic = Some("The automation project was closed; open it and confirm this automation before scheduling again".into());
            automation.enabled = false;
            automation.next = None;
        } else if capacity_full {
            firing.state = FiringState::Skipped;
            firing.skip = Some(SkipReason::Capacity);
            firing.finished_ms = Some(now_ms);
            firing.diagnostic = Some(
                "All precheck slots are occupied by running or cleanup-unconfirmed checks".into(),
            );
        } else if automation.active_firing.is_some() {
            firing.state = FiringState::Skipped;
            firing.skip = Some(SkipReason::Overlap);
            firing.finished_ms = Some(now_ms);
            firing.diagnostic = Some("An earlier run is still active".into());
        } else {
            automation.active_firing = Some(firing.id);
        }
        insert_or_coalesce_firing(&tx, &mut firing, &key)?;
        if definition.precheck.is_some() {
            tx.execute("INSERT OR IGNORE INTO automation_precheck_retirements(firing,revision,retired) VALUES(?1,?2,?3)",params![firing.id.to_string(),firing.revision,firing.state!=FiringState::Prechecking]).map_err(|error|error.to_string())?;
        }
        let accepted = if firing.state == FiringState::Prechecking && definition.precheck.is_none()
        {
            accept_firing(&tx, &mut automation, &mut firing, &definition, &key, now_ms)?
        } else {
            None
        };
        automation.updated_ms = now_ms;
        write_firing(&tx, &firing)?;
        if definition.precheck.is_none() && firing.state==FiringState::Skipped && firing.skipped_occurrences.is_none() {
            coalesce_existing_skipped_firing(&tx,&mut firing)?;
        }
        if firing.state == FiringState::Skipped
            && firing
                .skipped_occurrences
                .as_ref()
                .is_none_or(|summary| summary.count == 1)
        {
            queue_automation_notice(
                &tx,
                &automation,
                crate::notifications::Kind::AutomationSkipped,
                skipped_reason(&firing),
                &format!("skip:{}", firing.id),
            )?;
        }
        write_automation(&tx, &automation)?;
        prune_history(&tx, now_ms, Some(id))?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(Claimed {
            firing,
            definition,
            target_identity: automation.target_identity,
            accepted,
            created: true,
        })
    }

    pub(crate) fn finish_automation_precheck(
        &mut self,
        firing_id: Uuid,
        outcome: super::super::automations::precheck::Outcome,
        now_ms: u64,
    ) -> Result<Option<crate::agent_execution::Accepted>, String> {
        use crate::{automations::SkipReason, host::automations::precheck::Outcome};
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let mut firing = read_firing(&tx, firing_id)?;
        if !matches!(
            firing.state,
            FiringState::Prechecking | FiringState::Interrupted
        ) || firing.run.is_some()
        {
            return Ok(None);
        }
        let mut automation = read_automation(&tx, firing.automation)?;
        if automation.active_firing != Some(firing_id) {
            return Err("Precheck no longer owns this automation's active slot".into());
        }
        if firing.state == FiringState::Interrupted {
            // The caller has now read the exact durable cleanup receipt. It
            // releases ownership, but cannot resurrect a previously uncertain
            // firing into a new provider prompt, even if its shell succeeded.
            automation.active_firing = None;
            automation.blocked = None;
            automation.updated_ms = now_ms;
            write_automation(&tx, &automation)?;
            tx.commit().map_err(|error| error.to_string())?;
            return Ok(None);
        }
        let current = automation.execution_revision == firing.revision
            && (matches!(firing.trigger, crate::automations::Trigger::Manual { .. })
                || automation.enabled);
        let accepted = if let Outcome::Stopped {
            state,
            skip,
            diagnostic,
        } = outcome
        {
            firing.state = state;
            firing.skip = skip;
            firing.diagnostic = Some(diagnostic);
            None
        } else if firing.cancel_requested {
            firing.state = FiringState::Cancelled;
            firing.diagnostic = Some("Cancelled before launching the agent".into());
            None
        } else if !current {
            firing.state = FiringState::Skipped;
            firing.skip = Some(SkipReason::DefinitionChanged);
            firing.diagnostic =
                Some("Automation changed or was disabled before the precheck completed".into());
            None
        } else {
            let definition = read_definition(&tx, firing.automation, firing.revision)?;
            let key = occurrence_key(firing.revision, &firing.trigger);
            accept_firing(&tx, &mut automation, &mut firing, &definition, &key, now_ms)?
        };
        if !firing.state.is_active() {
            firing.finished_ms = Some(now_ms);
            automation.active_firing = None;
            if firing.state == FiringState::Interrupted {
                automation.enabled = false;
                automation.next = None;
            }
        }
        automation.updated_ms = now_ms;
        write_firing(&tx, &firing)?;
        write_automation(&tx, &automation)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(accepted)
    }
}

fn accept_firing(
    tx: &Transaction<'_>,
    automation: &mut Automation,
    firing: &mut Firing,
    definition: &Definition,
    key: &str,
    now_ms: u64,
) -> Result<Option<crate::agent_execution::Accepted>, String> {
    use crate::{
        agent_execution::{Destination, Origin, Submit},
        automations::SessionMode,
    };
    let destination = if definition.session_mode == SessionMode::Reuse
        && let Some(agent) = automation.reuse_agent
    {
        Destination::Existing(agent)
    } else {
        let mut spec = definition.agent.clone();
        if let Some(channel) = automation.channel {
            spec.target.placement = Some(crate::agent_execution::Placement::Channel(channel));
        }
        Destination::New(spec)
    };
    let request = Submit {
        name: None,
        destination,
        prompt: definition.prompt.clone(),
        idempotency_key: key.to_owned(),
    };
    // Failure must leave a useful firing record without leaving a half-created
    // agent/channel. Acceptance's normal caller rolls back its whole tx; here
    // a savepoint keeps that same guarantee inside the firing transaction.
    tx.execute_batch("SAVEPOINT automation_accept")
        .map_err(|error| error.to_string())?;
    let accepted = Registry::submit_agent_in(
        tx,
        &Origin::Automation { id: automation.id },
        &request,
        now_ms,
    );
    match accepted {
        Ok(accepted) => {
            if automation.channel.is_none()
                && let Some(channel) = accepted.agent.channel()
            {
                // Naming is a user choice, independent of the execution prompt.
                // Only initialize the owned channel: subsequent firings and
                // automation renames preserve any separately chosen title.
                tx.execute(
                    "UPDATE channels SET title = ?2, title_source = ?3 WHERE id = ?1",
                    params![
                        channel.to_string(),
                        automation.definition.name,
                        crate::model::ChannelTitleSource::Manual as i64,
                    ],
                )
                .map_err(|error| error.to_string())?;
            }
            tx.execute_batch("RELEASE automation_accept")
                .map_err(|error| error.to_string())?;
            firing.agent = Some(accepted.agent.id);
            firing.run = Some(accepted.run.id);
            firing.state = FiringState::Accepted;
            automation.channel = accepted.agent.channel();
            if definition.session_mode == SessionMode::Reuse {
                automation.reuse_agent = Some(accepted.agent.id);
            }
            Ok(Some(accepted))
        }
        Err(error) => {
            tx.execute_batch("ROLLBACK TO automation_accept; RELEASE automation_accept")
                .map_err(|error| error.to_string())?;
            firing.state = FiringState::Failed;
            firing.finished_ms = Some(now_ms);
            let mut diagnostic = error;
            crate::agent_execution::truncate(
                &mut diagnostic,
                crate::agent_execution::MAX_DIAGNOSTIC_BYTES,
            );
            firing.diagnostic = Some(diagnostic);
            automation.active_firing = None;
            Ok(None)
        }
    }
}

fn occurrence_key(revision: u64, trigger: &crate::automations::Trigger) -> String {
    match trigger {
        crate::automations::Trigger::Scheduled { occurrence } => {
            format!("scheduled:{revision}:{}", occurrence.local)
        }
        crate::automations::Trigger::Manual { request } => format!("manual:{request}"),
    }
}

fn read_target_identity(
    db: &Connection,
    id: Uuid,
    revision: u64,
) -> Result<crate::automations::DirectoryIdentity, String> {
    let record: String = db
        .query_row(
            "SELECT target_identity FROM automation_revisions WHERE automation=?1 AND revision=?2",
            params![id.to_string(), revision],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    decode(&record)
}

fn read_definition(db: &Connection, id: Uuid, revision: u64) -> Result<Definition, String> {
    let record = db
        .query_row(
            "SELECT definition FROM automation_revisions WHERE automation=?1 AND revision=?2",
            params![id.to_string(), revision],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| error.to_string())?;
    decode(&record)
}
fn read_firing(db: &Connection, id: Uuid) -> Result<Firing, String> {
    let record = db
        .query_row(
            "SELECT record FROM automation_firings WHERE id=?1",
            [id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| error.to_string())?;
    decode(&record)
}
fn read_firing_key(db: &Connection, id: Uuid, key: &str) -> Result<Option<Firing>, String> {
    db.query_row(
        "SELECT record FROM automation_firings WHERE automation=?1 AND occurrence_key=?2",
        params![id.to_string(), key],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|error| error.to_string())?
    .map(|record| decode(&record))
    .transpose()
}
pub(super) fn queue_automation_event(
    tx: &Transaction<'_>,
    event: crate::notifications::Event,
) -> Result<(), String> {
    event.validate()?;
    if let crate::notifications::Target::Automation { id, .. } = &event.target {
        tx.execute("DELETE FROM automation_notifications WHERE json_extract(event,'$.target.Automation.id')=?1 AND key<>?2",
            params![id.to_string(), event.evidence.key]).map_err(|error| error.to_string())?;
    }
    tx.execute(
        "INSERT OR IGNORE INTO automation_notifications(key,event) VALUES(?1,?2)",
        params![event.evidence.key, encode(&event)?],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}
fn queue_automation_notice(
    tx: &Transaction<'_>,
    automation: &Automation,
    kind: crate::notifications::Kind,
    reason: &str,
    edge: &str,
) -> Result<(), String> {
    let mut reason: String = reason.chars().filter(|c| !c.is_control()).collect();
    crate::agent_execution::truncate(&mut reason, 512);
    queue_automation_event(
        tx,
        crate::notifications::Event {
            target: crate::notifications::Target::Automation {
                id: automation.id,
                name: automation.definition.name.clone(),
                reason,
            },
            source_session: None,
            provider: None,
            directory: Some(automation.definition.agent.target.repository.clone()),
            destination: None,
            evidence: crate::notifications::Evidence {
                key: format!("automation:{}:{edge}", automation.id),
                kind,
            },
        },
    )
}

fn require_unretired_request(db: &Connection, automation: Uuid, key: &str) -> Result<(), String> {
    let retired: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM automation_retired_manual_requests WHERE automation=?1 AND request=?2)",
        params![automation.to_string(), key], |row| row.get(0)).map_err(|error| error.to_string())?;
    if retired {
        Err("This manual request's history expired; it will not be executed again. Use a new request ID to authorize new work.".into())
    } else {
        Ok(())
    }
}

const HISTORY_ROWS: usize = 500;
const HISTORY_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;

fn skipped_reason(firing:&Firing)->&'static str {
    use crate::automations::SkipReason;
    match firing.skip {
        Some(SkipReason::PrecheckExit)=>"The precheck declined this scheduled occurrence",
        Some(SkipReason::MissedDeadline)=>"Scheduled occurrences were missed while the scheduler was unavailable",
        Some(SkipReason::Overlap)=>"An earlier run is still active or waiting for input",
        Some(SkipReason::Capacity)=>"Execution or precheck capacity is occupied by another run",
        Some(SkipReason::DefinitionChanged)=>"The automation changed before this occurrence could start",
        Some(SkipReason::Disabled)=>"The automation was disabled before this occurrence could start",
        None=>"This scheduled occurrence could not run; inspect its history",
    }
}

fn coalesce_existing_skipped_firing(tx:&Transaction<'_>,firing:&mut Firing)->Result<(),String> {
    if firing.state!=FiringState::Skipped || firing.run.is_some() || firing.agent.is_some() || firing.release_pending
        || firing.skipped_occurrences.is_some() || !matches!(firing.trigger,crate::automations::Trigger::Scheduled {..}) {return Ok(());}
    let latest:Option<String>=tx.query_row("SELECT id FROM automation_firings WHERE automation=?1 ORDER BY rowid DESC LIMIT 1",[firing.automation.to_string()],|row|row.get(0)).optional().map_err(|error|error.to_string())?;
    // A precheck may finish after later overlap rows. Preserve their ordering
    // and reason boundary instead of reinserting an older occurrence last.
    if latest.as_deref()!=Some(firing.id.to_string().as_str()) {return Ok(());}
    let key:String=tx.query_row("SELECT occurrence_key FROM automation_firings WHERE id=?1",[firing.id.to_string()],|row|row.get(0)).map_err(|error|error.to_string())?;
    tx.execute("DELETE FROM automation_precheck_retirements WHERE firing=?1 AND retired=1",[firing.id.to_string()]).map_err(|error|error.to_string())?;
    tx.execute("DELETE FROM automation_firings WHERE id=?1",[firing.id.to_string()]).map_err(|error|error.to_string())?;
    insert_or_coalesce_firing(tx,firing,&key)
}

/// Consecutive scheduled skips keep the first row's stable identity and the
/// latest occurrence key. The persisted next deadline fences older retries.
fn insert_or_coalesce_firing(
    tx: &Transaction<'_>,
    firing: &mut Firing,
    key: &str,
) -> Result<(), String> {
    use crate::automations::{SkippedOccurrences, Trigger};
    if firing.state == FiringState::Skipped
        && let Trigger::Scheduled { occurrence } = &firing.trigger
    {
        let due = occurrence.due_ms;
        if firing.missed_interval.is_none() {
            firing.skipped_occurrences = Some(SkippedOccurrences {
                count: 1,
                first_occurrence_ms: due,
                last_occurrence_ms: due,
            });
        }
        let latest: Option<String> = tx.query_row(
            "SELECT record FROM automation_firings WHERE automation=?1 ORDER BY rowid DESC LIMIT 1",
            [firing.automation.to_string()], |row| row.get(0),
        ).optional().map_err(|error| error.to_string())?;
        if let Some(record) = latest {
            let mut previous: Firing = decode(&record)?;
            if previous.state == FiringState::Skipped
                && previous.agent.is_none() && previous.run.is_none() && !previous.release_pending
                && previous.skip == firing.skip
                && previous.revision == firing.revision
                && let Trigger::Scheduled { occurrence } = &previous.trigger
            {
                match (
                    previous.missed_interval.as_mut(),
                    firing.missed_interval.as_ref(),
                ) {
                    (Some(old), Some(new)) => {
                        old.first_due_ms = old.first_due_ms.min(new.first_due_ms);
                        old.until_exclusive_ms = old.until_exclusive_ms.max(new.until_exclusive_ms);
                        old.intervals = old.intervals.saturating_add(new.intervals);
                        previous.diagnostic = firing.diagnostic.clone();
                    }
                    (None, None) => {
                        let summary =
                            previous
                                .skipped_occurrences
                                .get_or_insert(SkippedOccurrences {
                                    count: 1,
                                    first_occurrence_ms: occurrence.due_ms,
                                    last_occurrence_ms: occurrence.due_ms,
                                });
                        summary.count = summary.count.saturating_add(1);
                        summary.last_occurrence_ms = due;
                    }
                    _ => return insert_firing(tx, firing, key),
                }
                previous.finished_ms = firing.finished_ms;
                tx.execute(
                    "UPDATE automation_firings SET occurrence_key=?2 WHERE id=?1",
                    params![previous.id.to_string(), key],
                )
                .map_err(|error| error.to_string())?;
                write_firing(tx, &previous)?;
                *firing = previous;
                return Ok(());
            }
        }
    }
    insert_firing(tx, firing, key)
}

/// Keep bounded terminal history while retaining every active or unresolved
/// owner. Revisions referenced by a firing or the current definition survive.
fn prune_history(
    tx: &Transaction<'_>,
    now_ms: u64,
    automation: Option<Uuid>,
) -> Result<(), String> {
    let cutoff = now_ms.saturating_sub(HISTORY_TTL_MS);
    tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS automation_history_prune (id TEXT PRIMARY KEY); DELETE FROM automation_history_prune;")
        .map_err(|error| error.to_string())?;
    let candidates = format!("INSERT INTO automation_history_prune(id)
        SELECT id FROM (
            SELECT f.id, f.created_ms, a.deleted_ms, a.active_firing,
                f.state, f.record,
                row_number() OVER (PARTITION BY f.automation ORDER BY
                    CASE WHEN a.active_firing=f.id OR json_extract(f.record,'$.state') IN ('prechecking','accepted','running','waiting')
                        OR coalesce(json_extract(f.record,'$.release_pending'),0)=1 THEN 1 ELSE 0 END DESC,
                    f.created_ms DESC,f.id DESC) AS rank
            FROM automation_firings f JOIN automations a ON a.id=f.automation
            {}
        ) WHERE (active_firing IS NULL OR active_firing<>id)
          AND json_extract(record,'$.state') NOT IN ('prechecking','accepted','running','waiting')
          AND coalesce(json_extract(record,'$.release_pending'),0)=0
          AND NOT EXISTS(SELECT 1 FROM automation_precheck_retirements p WHERE p.firing=id AND p.retired=0)
          AND ((deleted_ms IS NOT NULL AND deleted_ms<?1) OR coalesce(json_extract(record,'$.finished_ms'),created_ms)<?1 OR rank>?2)
    ", if automation.is_some() { "WHERE f.automation=?3" } else { "WHERE ?3 IS NULL" });
    tx.execute(
        &candidates,
        params![cutoff, HISTORY_ROWS, automation.map(|id| id.to_string())],
    )
    .map_err(|error| error.to_string())?;
    tx.execute(
        "INSERT OR IGNORE INTO automation_retired_manual_requests(automation,request,revision)
        SELECT automation,occurrence_key,revision FROM automation_firings
        WHERE occurrence_key LIKE 'manual:%' AND id IN (SELECT id FROM automation_history_prune)",
        [],
    )
    .map_err(|error| error.to_string())?;
    tx.execute("DELETE FROM automation_precheck_retirements WHERE retired=1 AND firing IN (SELECT id FROM automation_history_prune)",[]).map_err(|error|error.to_string())?;
    tx.execute(
        "DELETE FROM automation_firings WHERE id IN (SELECT id FROM automation_history_prune)",
        [],
    )
    .map_err(|error| error.to_string())?;
    tx.execute("DELETE FROM automation_retired_manual_requests WHERE automation IN (
        SELECT id FROM automations WHERE deleted_ms IS NOT NULL AND deleted_ms<?1 AND active_firing IS NULL
    )", [cutoff]).map_err(|error| error.to_string())?;
    tx.execute("DELETE FROM automation_revisions WHERE NOT EXISTS (
        SELECT 1 FROM automation_firings f WHERE f.automation=automation_revisions.automation AND f.revision=automation_revisions.revision
    ) AND NOT EXISTS (
        SELECT 1 FROM automations a WHERE a.id=automation_revisions.automation
        AND (a.deleted_ms IS NULL OR a.deleted_ms>=?1)
        AND json_extract(a.record,'$.execution_revision')=automation_revisions.revision
    )", [cutoff]).map_err(|error| error.to_string())?;
    Ok(())
}

fn insert_firing(tx: &Transaction<'_>, firing: &Firing, key: &str) -> Result<(), String> {
    tx.execute("INSERT INTO automation_firings(id,automation,revision,occurrence_key,record,created_ms,state,run_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![firing.id.to_string(), firing.automation.to_string(), firing.revision, key, encode(firing)?, firing.created_ms, encode(&firing.state)?, firing.run.map(|id| id.to_string())])
        .map_err(|error| error.to_string())?;
    Ok(())
}
fn write_firing(tx: &Transaction<'_>, firing: &Firing) -> Result<(), String> {
    tx.execute(
        "UPDATE automation_firings SET record=?2,state=?3,run_id=?4 WHERE id=?1",
        params![
            firing.id.to_string(),
            encode(firing)?,
            encode(&firing.state)?,
            firing.run.map(|id| id.to_string())
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Spec, Surface, Target},
        automations::{Schedule, SchedulePattern, SessionMode},
        model::ProjectKind,
        workspace_model::ModelEdit,
    };

    fn test_identity() -> crate::automations::DirectoryIdentity {
        crate::automations::DirectoryIdentity {
            device: 1,
            inode: 1,
        }
    }

    fn fixture() -> (Registry, tempfile::TempDir, Definition) {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        let repository = directory.path().to_owned();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: repository.clone(),
                name: "Automation fixture".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let definition = Definition {
            name: "Review daily".into(),
            prompt: "Review the fixture".into(),
            agent: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository: repository.clone(),
                    directory: repository,
                    placement: None,
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            schedule: Schedule {
                pattern: SchedulePattern::Daily { hour: 9, minute: 0 },
                timezone: "America/Vancouver".into(),
            },
            session_mode: SessionMode::Reuse,
            visibility: crate::automations::Visibility::default(),
            catch_up: crate::automations::CatchUp::default(),
            precheck: None,
        };
        (registry, directory, definition)
    }

    #[test]
    fn legacy_automation_target_keeps_its_definition_after_reopen() {
        let (mut registry, directory, definition) = fixture();
        let created = registry
            .create_automation(definition.clone(), 100, test_identity())
            .unwrap();
        let mut legacy = serde_json::to_value(&created).unwrap();
        let target = &mut legacy["definition"]["agent"]["target"];
        target.as_object_mut().unwrap().remove("placement");
        target["channel"] = serde_json::Value::Null;
        registry
            .db
            .execute(
                "UPDATE automations SET record=?2 WHERE id=?1",
                params![created.id.to_string(), legacy.to_string()],
            )
            .unwrap();
        drop(registry);
        let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        let restored = registry.automation(created.id).unwrap();
        assert_eq!(restored.definition, definition);
        assert_eq!(restored.channel, created.channel);
        assert_eq!(registry.automations(None, 100).unwrap().len(), 1);
    }

    #[test]
    fn definition_reopens_disabled_and_enable_requires_current_consent() {
        let (mut registry, directory, definition) = fixture();
        let created = registry
            .create_automation(definition.clone(), 100, test_identity())
            .unwrap();
        assert!(!created.enabled);
        assert!(created.next.is_none());
        drop(registry);
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(
            registry.automation(created.id).unwrap().definition,
            definition
        );
        let next = Some(Occurrence {
            due_ms: 1000,
            local: "2026-09-27T09:00".into(),
        });
        assert!(
            registry
                .set_automation_enabled(created.id, 1, next.clone(), false, 200)
                .is_err()
        );
        let enabled = registry
            .set_automation_enabled(created.id, 1, next, true, 200)
            .unwrap();
        assert!(enabled.enabled);
        assert_eq!(enabled.revision, 2);
        assert!(
            registry
                .set_automation_enabled(created.id, 1, None, false, 201)
                .is_err()
        );
    }

    #[test]
    fn cancellation_racing_successful_precheck_receipt_never_accepts_a_provider_run() {
        let (mut registry, _directory, mut definition) = fixture();
        definition.precheck = Some(crate::automations::Precheck {
            command: "true".into(),
            timeout_seconds: 5,
        });
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let claim = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        registry
            .cancel_automation_precheck(automation.id, claim.firing.id)
            .unwrap();
        // The child already finished, but its receipt had not yet authorized
        // provider acceptance. Cancellation wins that transaction boundary.
        assert!(
            registry
                .finish_automation_precheck(
                    claim.firing.id,
                    crate::host::automations::precheck::Outcome::Passed,
                    220
                )
                .unwrap()
                .is_none()
        );
        let firing = registry.automation_firing(claim.firing.id).unwrap();
        assert_eq!(firing.state, FiringState::Cancelled);
        assert!(firing.run.is_none());
        assert!(
            registry
                .automation(automation.id)
                .unwrap()
                .active_firing
                .is_none()
        );
        assert_eq!(
            registry
                .db
                .query_row("SELECT count(*) FROM agent_runs", [], |row| row
                    .get::<_, usize>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn rename_preserves_schedule_but_material_edit_pauses_and_fences_old_requests() {
        let (mut registry, _directory, definition) = fixture();
        let created = registry
            .create_automation(definition.clone(), 100, test_identity())
            .unwrap();
        let next = Some(Occurrence {
            due_ms: 1000,
            local: "2026-09-27T09:00".into(),
        });
        let enabled = registry
            .set_automation_enabled(created.id, 1, next.clone(), true, 200)
            .unwrap();
        let mut renamed = definition;
        renamed.name = "New name".into();
        let renamed = registry
            .edit_automation(
                created.id,
                enabled.revision,
                renamed,
                300,
                crate::automations::DirectoryIdentity {
                    device: 9,
                    inode: 9,
                },
            )
            .unwrap();
        assert!(renamed.enabled);
        assert_eq!(renamed.execution_revision, enabled.execution_revision);
        assert_eq!(renamed.next, next);
        assert_eq!(
            renamed.target_identity, created.target_identity,
            "a name-only edit cannot silently adopt a replaced directory"
        );
        let mut changed = renamed.definition.clone();
        changed.prompt = "Different authorized work".into();
        let changed = registry
            .edit_automation(created.id, renamed.revision, changed, 400, test_identity())
            .unwrap();
        assert!(!changed.enabled);
        assert!(changed.next.is_none());
        assert_ne!(changed.execution_revision, renamed.execution_revision);
        assert!(
            registry
                .set_automation_enabled(created.id, renamed.revision, next, true, 500)
                .is_err()
        );
    }

    #[test]
    fn removal_tombstones_identity_without_reopening_stale_enable() {
        let (mut registry, _directory, definition) = fixture();
        let created = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        registry
            .remove_automation(created.id, created.revision, 200)
            .unwrap();
        assert!(registry.automation(created.id).is_err());
        assert!(registry.automations(None, 100).unwrap().is_empty());
        assert!(
            registry
                .set_automation_enabled(
                    created.id,
                    created.revision,
                    Some(Occurrence {
                        due_ms: 1000,
                        local: "2026-09-27T09:00".into()
                    }),
                    true,
                    300
                )
                .is_err()
        );
        let retained: usize = registry
            .db
            .query_row(
                "SELECT count(*) FROM automations WHERE id=?1 AND deleted_ms IS NOT NULL",
                [created.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, 1);
    }

    #[test]
    fn manual_claim_and_acceptance_survive_reopen_without_a_second_run() {
        let (mut registry, directory, definition) = fixture();
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let trigger = crate::automations::Trigger::Manual {
            request: Uuid::new_v4(),
        };
        let claimed = registry
            .claim_automation(automation.id, 1, None, trigger.clone(), None, true, 200)
            .unwrap();
        assert!(claimed.created);
        let accepted = claimed.accepted.unwrap();
        assert_eq!(claimed.firing.run, Some(accepted.run.id));
        assert_eq!(
            registry.automation(automation.id).unwrap().channel,
            Some(accepted.agent.channel().unwrap())
        );
        drop(registry);

        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        let repeated = registry
            .claim_automation(automation.id, 1, None, trigger, None, true, 300)
            .unwrap();
        assert!(!repeated.created);
        assert!(
            repeated.accepted.is_none(),
            "a retried claim cannot dispatch again"
        );
        assert_eq!(repeated.firing.id, claimed.firing.id);
        let count: usize = registry
            .db
            .query_row("SELECT count(*) FROM agent_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            registry
                .automation_history(Some(automation.id), None, 100)
                .unwrap()
                .firings
                .len(),
            1
        );
    }

    #[test]
    fn automation_global_history_pages_removed_definitions_with_immutable_names() {
        use crate::{
            automations::{Precheck, SkipReason, Trigger},
            host::automations::precheck::Outcome,
        };
        let (mut registry, _directory, mut definition) = fixture();
        definition.precheck = Some(Precheck {
            command: "exit 1".into(),
            timeout_seconds: 1,
        });
        let mut expected = std::collections::BTreeMap::new();
        for index in 0..3 {
            definition.name = format!("Retained {index}");
            let automation = registry
                .create_automation(definition.clone(), 100, test_identity())
                .unwrap();
            definition.name.push_str(" renamed");
            let automation = registry
                .edit_automation(
                    automation.id,
                    automation.revision,
                    definition.clone(),
                    150,
                    test_identity(),
                )
                .unwrap();
            assert_eq!(
                automation.execution_revision, 1,
                "renaming preserves execution consent"
            );
            let claim = registry
                .claim_automation(
                    automation.id,
                    1,
                    None,
                    Trigger::Manual {
                        request: Uuid::new_v4(),
                    },
                    None,
                    true,
                    200,
                )
                .unwrap();
            registry
                .finish_automation_precheck(
                    claim.firing.id,
                    Outcome::Stopped {
                        state: FiringState::Skipped,
                        skip: Some(SkipReason::PrecheckExit),
                        diagnostic: "not ready".into(),
                    },
                    300,
                )
                .unwrap();
            let accepted_name = definition.name.clone();
            definition.name.push_str(" after run");
            let automation = registry
                .edit_automation(
                    automation.id,
                    automation.revision,
                    definition.clone(),
                    350,
                    test_identity(),
                )
                .unwrap();
            assert_eq!(
                registry
                    .automation_run_detail(claim.firing.id)
                    .unwrap()
                    .definition
                    .name,
                accepted_name
            );
            registry
                .remove_automation(automation.id, automation.revision, 400)
                .unwrap();
            expected.insert(claim.firing.id, accepted_name);
        }
        assert!(registry.automations(None, 100).unwrap().is_empty());
        let mut cursor = None;
        let mut observed = std::collections::BTreeMap::new();
        loop {
            let page = registry.automation_history(None, cursor, 1).unwrap();
            assert_eq!(page.firings.len(), 1);
            let firing = &page.firings[0];
            assert!(observed.insert(firing.id, firing.name.clone()).is_none());
            let Some(next) = page.next else { break };
            cursor = Some(next);
        }
        assert_eq!(observed, expected);
    }

    #[test]
    fn disabled_precheck_cannot_accept_even_after_reenable() {
        use crate::automations::{Precheck, SkipReason, Trigger};
        let (mut registry, _directory, mut definition) = fixture();
        definition.precheck = Some(Precheck {
            command: "exit 0".into(),
            timeout_seconds: 5,
        });
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let due = Occurrence {
            due_ms: 200,
            local: "2026-09-27T09:00".into(),
        };
        let next = Occurrence {
            due_ms: 400,
            local: "2026-09-28T09:00".into(),
        };
        let enabled = registry
            .set_automation_enabled(automation.id, 1, Some(due.clone()), true, 150)
            .unwrap();
        let claimed = registry
            .claim_automation(
                automation.id,
                enabled.execution_revision,
                Some(&due),
                Trigger::Scheduled {
                    occurrence: due.clone(),
                },
                Some(next.clone()),
                false,
                200,
            )
            .unwrap();
        assert_eq!(claimed.firing.state, FiringState::Prechecking);
        let disabled = registry
            .set_automation_enabled(automation.id, enabled.revision, None, false, 210)
            .unwrap();
        registry
            .set_automation_enabled(automation.id, disabled.revision, Some(next), true, 220)
            .unwrap();
        assert!(
            registry
                .finish_automation_precheck(
                    claimed.firing.id,
                    crate::host::automations::precheck::Outcome::Passed,
                    230
                )
                .unwrap()
                .is_none()
        );
        let firing = read_firing(&registry.db, claimed.firing.id).unwrap();
        assert_eq!(firing.state, FiringState::Skipped);
        assert_eq!(firing.skip, Some(SkipReason::DefinitionChanged));
        assert!(
            registry
                .automation(automation.id)
                .unwrap()
                .active_firing
                .is_none()
        );
        let count: usize = registry
            .db
            .query_row("SELECT count(*) FROM agent_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn failed_acceptance_retains_history_without_partial_agent_or_channel() {
        let (mut registry, _directory, definition) = fixture();
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        // The target may disappear after the definition was saved. Acceptance
        // must fail within the same transaction that records the firing.
        registry.db.execute("DELETE FROM projects", []).unwrap();
        let claimed = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        assert_eq!(claimed.firing.state, FiringState::Failed);
        assert!(claimed.accepted.is_none());
        assert!(
            registry
                .automation(automation.id)
                .unwrap()
                .active_firing
                .is_none()
        );
        for table in ["agents", "agent_runs", "channels"] {
            let count: usize = registry
                .db
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} must roll back");
        }
    }

    #[test]
    fn one_hundred_overlaps_coalesce_and_emit_one_attention_edge() {
        let (mut registry, _directory, definition) = fixture();
        let created = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let first_due = Occurrence {
            due_ms: 60_000,
            local: "minute-0".into(),
        };
        let enabled = registry
            .set_automation_enabled(created.id, 1, Some(first_due.clone()), true, 200)
            .unwrap();
        let first = registry
            .claim_automation(
                created.id,
                enabled.execution_revision,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                Some(first_due),
                true,
                300,
            )
            .unwrap();
        let accepted = first.accepted.as_ref().unwrap();
        let started = registry
            .update_agent_run(
                accepted.run.id,
                accepted.run.revision,
                crate::agent_execution::Update::Started {
                    provider_key: "waiting-overlap-fixture".into(),
                },
                400,
            )
            .unwrap();
        registry
            .update_agent_run(
                accepted.run.id,
                started.revision,
                crate::agent_execution::Update::Waiting,
                500,
            )
            .unwrap();
        assert_eq!(
            registry
                .reconcile_automation_run(accepted.run.id, 500)
                .unwrap()
                .unwrap()
                .state,
            FiringState::Waiting
        );
        for index in 0..100 {
            let automation = registry.automation(created.id).unwrap();
            let due = automation.next.clone().unwrap();
            let next = Occurrence {
                due_ms: due.due_ms + 60_000,
                local: format!("minute-{}", index + 1),
            };
            registry
                .claim_automation(
                    created.id,
                    enabled.execution_revision,
                    Some(&due),
                    crate::automations::Trigger::Scheduled {
                        occurrence: due.clone(),
                    },
                    Some(next),
                    false,
                    due.due_ms,
                )
                .unwrap();
        }
        let history = registry
            .automation_history(Some(created.id), None, 100)
            .unwrap();
        assert_eq!(history.firings.len(), 2);
        let skipped = history
            .firings
            .iter()
            .find(|firing| firing.state == FiringState::Skipped)
            .unwrap();
        let summary = skipped.skipped_occurrences.as_ref().unwrap();
        assert_eq!(summary.count, 100);
        assert_eq!(summary.first_occurrence_ms, 60_000);
        assert_eq!(summary.last_occurrence_ms, 6_000_000);
        assert_eq!(
            registry.automation(created.id).unwrap().active_firing,
            Some(first.firing.id)
        );
        let events = registry.automation_notifications().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].evidence.kind,
            crate::notifications::Kind::AutomationSkipped
        );
    }

    #[test]
    fn history_retention_protects_the_old_active_run_and_removed_history_expires() {
        let (mut registry, _directory, definition) = fixture();
        let created = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let active = registry
            .claim_automation(
                created.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        for now in 201..(201 + HISTORY_ROWS as u64 + 10) {
            registry
                .claim_automation(
                    created.id,
                    1,
                    None,
                    crate::automations::Trigger::Manual {
                        request: Uuid::new_v4(),
                    },
                    None,
                    true,
                    now,
                )
                .unwrap();
        }
        let count: usize = registry
            .db
            .query_row("SELECT count(*) FROM automation_firings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, HISTORY_ROWS);
        assert!(registry.automation_firing(active.firing.id).is_ok());
        let key: String = registry
            .db
            .query_row(
                "SELECT request FROM automation_retired_manual_requests LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let request: Uuid = key.strip_prefix("manual:").unwrap().parse().unwrap();
        assert!(
            registry
                .claim_automation(
                    created.id,
                    1,
                    None,
                    crate::automations::Trigger::Manual { request },
                    None,
                    true,
                    1000
                )
                .err()
                .unwrap()
                .contains("expired")
        );
        // A removed automation cannot own active work. Settle this fixture's
        // accepted claim before exercising its independent retention contract.
        registry
            .db
            .execute(
                "UPDATE automations SET active_firing=NULL,deleted_ms=1000",
                [],
            )
            .unwrap();
        registry.db.execute("UPDATE automation_firings SET state='\"cancelled\"',record=json_set(record,'$.state','cancelled')", []).unwrap();
        registry
            .prune_automation_history(1000 + HISTORY_TTL_MS + 1)
            .unwrap();
        let count: usize = registry
            .db
            .query_row("SELECT count(*) FROM automation_firings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        let revisions: usize = registry
            .db
            .query_row("SELECT count(*) FROM automation_revisions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(revisions, 0);
    }

    #[test]
    fn headless_visibility_is_persisted_and_legacy_definitions_are_headfull() {
        let (mut registry, _directory, mut definition) = fixture();
        let mut legacy = serde_json::to_value(&definition).unwrap();
        legacy.as_object_mut().unwrap().remove("visibility");
        legacy.as_object_mut().unwrap().remove("catch_up");
        let restored: Definition = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            restored.visibility,
            crate::automations::Visibility::Headfull
        );
        assert_eq!(
            restored.catch_up,
            crate::automations::CatchUp::WithinMinutes(15)
        );
        definition.visibility = crate::automations::Visibility::Headless;
        let created = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let claim = registry
            .claim_automation(
                created.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        assert_eq!(
            registry.headless_automation_agents().unwrap(),
            vec![claim.accepted.unwrap().agent.id]
        );
    }

    #[test]
    fn overlap_records_a_skip_without_taking_the_active_slot() {
        let (mut registry, _directory, definition) = fixture();
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let first = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        let second = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                201,
            )
            .unwrap();
        assert_eq!(second.firing.state, FiringState::Skipped);
        assert_eq!(
            second.firing.skip,
            Some(crate::automations::SkipReason::Overlap)
        );
        assert_eq!(
            registry.automation(automation.id).unwrap().active_firing,
            Some(first.firing.id)
        );
        assert!(second.accepted.is_none());
    }

    #[test]
    fn precheck_capacity_includes_interrupted_owners_until_cleanup_is_proved() {
        let (mut registry, _directory, mut definition) = fixture();
        definition.precheck = Some(crate::automations::Precheck {
            command: "exit 0".into(),
            timeout_seconds: 1,
        });
        let mut firings = Vec::new();
        for _ in 0..crate::automations::MAX_CONCURRENT_PRECHECKS {
            let automation = registry
                .create_automation(definition.clone(), 100, test_identity())
                .unwrap();
            let claim = registry
                .claim_automation(
                    automation.id,
                    1,
                    None,
                    crate::automations::Trigger::Manual {
                        request: Uuid::new_v4(),
                    },
                    None,
                    true,
                    200,
                )
                .unwrap();
            firings.push(claim.firing.id);
        }
        assert_eq!(registry.active_automation_firings().unwrap().len(), 4);
        assert_eq!(registry.automation_update_blockers().unwrap().len(), 4);
        let extra = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let trigger = crate::automations::Trigger::Manual {
            request: Uuid::new_v4(),
        };
        assert!(
            registry
                .claim_automation(extra.id, 1, None, trigger.clone(), None, true, 201)
                .is_err()
        );
        registry
            .interrupt_automation_precheck(
                firings[0],
                "Owner disappeared without a cleanup receipt".into(),
                202,
            )
            .unwrap();
        registry
            .interrupt_automation_precheck(
                firings[0],
                "Owner disappeared without a cleanup receipt".into(),
                203,
            )
            .unwrap();
        let notices = registry.automation_notifications().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(
            matches!(&notices[0].target, crate::notifications::Target::Automation {id,reason,..}
            if *id == registry.automation_firing(firings[0]).unwrap().automation && reason.contains("interrupted"))
        );
        assert_eq!(
            registry.automation_update_blockers().unwrap().len(),
            4,
            "uncertain cleanup remains visible in planned update impact"
        );
        assert_eq!(
            registry.retired_automation_precheck(firings[0]).unwrap(),
            None,
            "uncertain cleanup keeps every precheck proof even with a terminal history state"
        );
        assert!(
            registry
                .claim_automation(extra.id, 1, None, trigger.clone(), None, true, 203)
                .is_err()
        );
        registry
            .finish_automation_precheck(
                firings[1],
                crate::host::automations::precheck::Outcome::Stopped {
                    state: FiringState::Skipped,
                    skip: Some(crate::automations::SkipReason::PrecheckExit),
                    diagnostic: "not ready".into(),
                },
                204,
            )
            .unwrap();
        assert_eq!(
            registry.retired_automation_precheck(firings[1]).unwrap(),
            Some(1)
        );
        assert!(
            registry
                .claim_automation(extra.id, 1, None, trigger, None, true, 205)
                .is_ok()
        );
        assert_eq!(registry.active_automation_firings().unwrap().len(), 4);
        // A delayed receipt can prove cleanup after an earlier uncertain
        // observation. It must release the claim without resending work.
        assert!(
            registry
                .finish_automation_precheck(
                    firings[0],
                    crate::host::automations::precheck::Outcome::Passed,
                    206,
                )
                .unwrap()
                .is_none()
        );
        let interrupted = read_firing(&registry.db, firings[0]).unwrap();
        assert_eq!(interrupted.state, FiringState::Interrupted);
        assert!(interrupted.run.is_none());
        let owner = registry.automation(interrupted.automation).unwrap();
        assert!(owner.active_firing.is_none());
        assert!(owner.blocked.is_none());
        assert!(!owner.enabled);
        assert_eq!(registry.active_automation_firings().unwrap().len(), 3);
        assert_eq!(registry.automation_update_blockers().unwrap().len(), 3);
    }

    #[test]
    fn disabling_before_dispatch_atomically_cancels_and_releases_unused_agent() {
        let (mut registry, _directory, definition) = fixture();
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let claim = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        let accepted = claim.accepted.unwrap();
        registry
            .set_automation_enabled(automation.id, 1, None, false, 201)
            .unwrap();
        assert!(!registry.claim_agent_dispatch(&accepted, 202).unwrap());
        assert_eq!(
            registry.agent_run(accepted.run.id).unwrap().state,
            crate::agent_execution::State::Cancelled
        );
        assert!(registry.agent(accepted.agent.id).unwrap().closed);
        assert!(
            registry
                .automation(automation.id)
                .unwrap()
                .active_firing
                .is_none()
        );
        assert_eq!(
            read_firing(&registry.db, claim.firing.id).unwrap().state,
            FiringState::Cancelled
        );
        assert_eq!(
            registry
                .db
                .query_row("SELECT count(*) FROM agent_dispatches", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn replaced_target_requires_new_consent_even_when_its_path_is_unchanged() {
        let (mut registry, _directory, definition) = fixture();
        let created = registry
            .create_automation(definition.clone(), 100, test_identity())
            .unwrap();
        let enabled = registry
            .set_automation_enabled(
                created.id,
                1,
                Some(Occurrence {
                    due_ms: 1000,
                    local: "2026-09-27T09:00".into(),
                }),
                true,
                200,
            )
            .unwrap();
        let replacement = crate::automations::DirectoryIdentity {
            device: 1,
            inode: 99,
        };
        let edited = registry
            .edit_automation(
                created.id,
                enabled.revision,
                definition,
                300,
                replacement.clone(),
            )
            .unwrap();
        assert!(!edited.enabled);
        assert!(edited.next.is_none());
        assert_eq!(edited.target_identity, replacement);
        assert!(edited.execution_revision > enabled.execution_revision);
    }

    #[test]
    fn project_close_and_reopen_does_not_revive_a_queued_precheck() {
        let (mut registry, _directory, mut definition) = fixture();
        let repository = definition.agent.target.repository.clone();
        definition.precheck = Some(crate::automations::Precheck {
            command: "true".into(),
            timeout_seconds: 1,
        });
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let claim = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        registry
            .apply(&ModelEdit::CloseProject {
                repository: repository.clone(),
            })
            .unwrap();
        let paused = registry.automation(automation.id).unwrap();
        assert!(!paused.enabled);
        assert!(paused.execution_revision > claim.firing.revision);
        assert_eq!(paused.active_firing, Some(claim.firing.id));
        registry
            .apply(&ModelEdit::ReopenProject {
                repository,
                name: "Reopened".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        assert!(
            registry
                .finish_automation_precheck(
                    claim.firing.id,
                    crate::host::automations::precheck::Outcome::Passed,
                    300
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(
            registry.automation_firing(claim.firing.id).unwrap().skip,
            Some(crate::automations::SkipReason::DefinitionChanged)
        );
        assert!(!registry.automation(automation.id).unwrap().enabled);
    }

    #[test]
    fn authority_failure_closes_only_an_unclaimed_launch_and_pauses_its_schedule() {
        let (mut registry, _directory, definition) = fixture();
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let claim = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        let accepted = claim.accepted.unwrap();
        registry
            .fail_automation_start(claim.firing.id, "Target was replaced".into(), 201, true)
            .unwrap();
        assert_eq!(
            registry.agent_run(accepted.run.id).unwrap().state,
            crate::agent_execution::State::Failed
        );
        assert!(registry.agent(accepted.agent.id).unwrap().closed);
        assert!(
            registry
                .automation(automation.id)
                .unwrap()
                .active_firing
                .is_none()
        );
        assert!(!registry.automation(automation.id).unwrap().enabled);
        let notices = registry.automation_notifications().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(
            matches!(&notices[0].target, crate::notifications::Target::Automation {id,reason,..}
            if *id == automation.id && reason.contains("could not start"))
        );

        let second = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                202,
            )
            .unwrap();
        let accepted = second.accepted.unwrap();
        assert!(registry.claim_agent_dispatch(&accepted, 203).unwrap());
        assert!(
            registry
                .fail_automation_start(second.firing.id, "Late error".into(), 204, true)
                .is_err()
        );
        assert_eq!(
            registry.agent_run(accepted.run.id).unwrap().state,
            crate::agent_execution::State::Accepted
        );
        assert_eq!(
            registry.automation(automation.id).unwrap().active_firing,
            Some(second.firing.id)
        );
    }

    #[test]
    fn missed_interval_is_one_durable_record_before_the_latest_claim() {
        let (mut registry, _directory, definition) = fixture();
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let old = Occurrence {
            due_ms: 1000,
            local: "2020-01-01T00:00".into(),
        };
        let latest = Occurrence {
            due_ms: 9000,
            local: "2026-09-27T09:00".into(),
        };
        let enabled = registry
            .set_automation_enabled(automation.id, 1, Some(old.clone()), true, 200)
            .unwrap();
        registry
            .skip_automation_deadlines(
                automation.id,
                enabled.execution_revision,
                &old,
                latest.clone(),
                9999,
            )
            .unwrap();
        assert!(
            registry
                .skip_automation_deadlines(
                    automation.id,
                    enabled.execution_revision,
                    &old,
                    latest.clone(),
                    9999
                )
                .is_err()
        );
        let claim = registry
            .claim_automation(
                automation.id,
                enabled.execution_revision,
                Some(&latest),
                crate::automations::Trigger::Scheduled {
                    occurrence: latest.clone(),
                },
                Some(Occurrence {
                    due_ms: 10_000,
                    local: "2026-09-27T09:01".into(),
                }),
                false,
                9999,
            )
            .unwrap();
        assert!(claim.accepted.is_some());
        let history = registry
            .automation_history(Some(automation.id), None, 10)
            .unwrap();
        assert_eq!(history.firings.len(), 2);
        assert_eq!(history.firings.iter().filter(|firing| firing.skip == Some(crate::automations::SkipReason::MissedDeadline)).count(), 1);
        let missed = history
            .firings
            .iter()
            .find(|firing| firing.skip == Some(crate::automations::SkipReason::MissedDeadline))
            .unwrap();
        assert!(
            missed.skipped_occurrences.is_none(),
            "offline intervals must not claim an inferred occurrence count"
        );
        let interval = missed.missed_interval.as_ref().unwrap();
        assert_eq!(
            (
                interval.first_due_ms,
                interval.until_exclusive_ms,
                interval.intervals
            ),
            (old.due_ms, latest.due_ms, 1)
        );
    }

    #[test]
    fn disabling_after_dispatch_claim_cannot_cancel_committed_work() {
        let (mut registry, _directory, definition) = fixture();
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let claim = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        let accepted = claim.accepted.unwrap();
        // Manual confirmation authorizes this single run while disabled.
        assert!(registry.claim_agent_dispatch(&accepted, 201).unwrap());
        registry
            .set_automation_enabled(automation.id, 1, None, false, 202)
            .unwrap();
        assert!(!registry.claim_agent_dispatch(&accepted, 203).unwrap());
        assert_eq!(
            registry.agent_run(accepted.run.id).unwrap().state,
            crate::agent_execution::State::Accepted
        );
        assert!(!registry.agent(accepted.agent.id).unwrap().closed);
        assert_eq!(
            registry.automation(automation.id).unwrap().active_firing,
            Some(claim.firing.id)
        );
    }

    #[test]
    fn dispatch_storage_error_is_not_misreported_as_policy_cancellation() {
        let (mut registry, _directory, definition) = fixture();
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let claim = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        let accepted = claim.accepted.unwrap();
        registry
            .db
            .execute_batch("ALTER TABLE automation_firings RENAME TO unavailable_firings")
            .unwrap();
        assert!(registry.claim_agent_dispatch(&accepted, 201).is_err());
        assert_eq!(
            registry.agent_run(accepted.run.id).unwrap().state,
            crate::agent_execution::State::Accepted
        );
        assert!(!registry.agent(accepted.agent.id).unwrap().closed);
    }

    #[test]
    fn execution_reconciliation_keeps_waiting_active_and_uses_exact_durable_outcome() {
        use crate::agent_execution::{State, Update};
        let (mut registry, _directory, mut definition) = fixture();
        definition.session_mode = SessionMode::Fresh;
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let claimed = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        let run = claimed.accepted.unwrap().run;
        let started = registry
            .update_agent_run(
                run.id,
                run.revision,
                Update::Started {
                    provider_key: "exact-turn".into(),
                },
                210,
            )
            .unwrap();
        registry
            .update_agent_run(run.id, started.revision, Update::Waiting, 220)
            .unwrap();
        let waiting = registry
            .reconcile_automation_run(run.id, 230)
            .unwrap()
            .unwrap();
        assert_eq!(waiting.state, FiringState::Waiting);
        assert_eq!(
            registry.automation(automation.id).unwrap().active_firing,
            Some(claimed.firing.id)
        );

        let current = registry.agent_run(run.id).unwrap();
        registry
            .update_agent_run(
                run.id,
                current.revision,
                Update::Finished {
                    state: State::Succeeded,
                    result: Default::default(),
                },
                250,
            )
            .unwrap();
        let done = registry
            .reconcile_automation_run(run.id, 260)
            .unwrap()
            .unwrap();
        assert_eq!(done.state, FiringState::Succeeded);
        assert_eq!(done.started_ms, Some(210));
        assert_eq!(done.finished_ms, Some(250));
        assert!(done.release_pending);
        assert_eq!(
            registry.automation(automation.id).unwrap().active_firing,
            Some(done.id)
        );
        let agent = registry.agent(run.agent).unwrap();
        assert!(
            registry
                .finish_automation_release(done.id, &agent, 265)
                .is_err(),
            "fresh run must be closed before another occurrence owns the slot"
        );
        assert_eq!(
            registry.reconcile_automation_run(run.id, 270).unwrap(),
            Some(done.clone())
        );
        registry
            .block_automation_release(done.id, "Provider cleanup is still pending".into(), 275)
            .unwrap();
        registry
            .block_automation_release(done.id, "Provider cleanup is still pending".into(), 276)
            .unwrap();
        let notices = registry.automation_notifications().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].evidence.kind,
            crate::notifications::Kind::AutomationBlocked
        );
        assert!(
            matches!(&notices[0].target, crate::notifications::Target::Automation {id,reason,..}
            if *id == automation.id && reason.contains("cleanup"))
        );

        assert!(
            registry
                .automation(automation.id)
                .unwrap()
                .blocked
                .is_some()
        );
        let closed = registry.close_agent(&agent.origin, agent.id).unwrap();
        registry
            .finish_automation_release(done.id, &closed, 280)
            .unwrap();
        assert!(
            registry
                .automation(automation.id)
                .unwrap()
                .active_firing
                .is_none()
        );
        assert!(
            registry
                .automation(automation.id)
                .unwrap()
                .blocked
                .is_none()
        );
    }

    fn completed_reusable_run(
        registry: &mut Registry,
        definition: Definition,
    ) -> (Automation, Firing, crate::agent_execution::Agent) {
        use crate::agent_execution::{HostedSession, State, Update};
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let claim = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        let accepted = claim.accepted.unwrap();
        let mut session = crate::chat::fresh_session(
            Provider::Claude,
            crate::model::TerminalId::new(),
            automation.definition.agent.target.directory.clone(),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        session.proof=Some(crate::model::HostSessionProof {host_session_id:crate::model::HostSessionId::new("archive-test"),boot_id:crate::model::BootId::new("archive-boot"),host_incarnation:crate::model::HostIncarnation::new("archive-incarnation")});
        let agent = registry
            .bind_agent(accepted.agent.id, None, HostedSession::Chat(session))
            .unwrap();
        let started = registry
            .update_agent_run(
                accepted.run.id,
                accepted.run.revision,
                Update::Started {
                    provider_key: "fixture-turn".into(),
                },
                205,
            )
            .unwrap();
        registry
            .update_agent_run(
                accepted.run.id,
                started.revision,
                Update::Finished {
                    state: State::Succeeded,
                    result: Default::default(),
                },
                210,
            )
            .unwrap();
        let firing = registry
            .reconcile_automation_run(accepted.run.id, 220)
            .unwrap()
            .unwrap();
        (automation, firing, agent)
    }

    #[test]
    fn fresh_automation_preserves_worktree_placement_without_owning_a_channel() {
        use crate::agent_execution::Placement;
        let (mut registry, directory, mut definition) = fixture();
        let scope = Placement::Worktree {
            repository: directory.path().into(),
            path: directory.path().into(),
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![],
                closed: vec![],
            })
            .unwrap();
        definition.agent.target.placement = Some(scope.clone());
        definition.session_mode = SessionMode::Fresh;
        let (automation, firing, agent) = completed_reusable_run(&mut registry, definition);
        assert_eq!(agent.placement, scope);
        assert!(registry.model().unwrap().channels.is_empty());
        assert!(
            registry
                .automation(automation.id)
                .unwrap()
                .channel
                .is_none()
        );
        let closed = registry.close_agent(&agent.origin, agent.id).unwrap();
        registry
            .finish_automation_release(firing.id, &closed, 230)
            .unwrap();
        let current = registry.automation(automation.id).unwrap();
        assert!(current.active_firing.is_none());
        let next = registry
            .claim_automation(
                current.id,
                current.revision,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                300,
            )
            .unwrap()
            .accepted
            .unwrap();
        assert_eq!(next.agent.placement, scope);
        assert!(registry.model().unwrap().channels.is_empty());
    }

    #[test]
    fn first_channel_uses_current_name_after_a_rename_during_precheck() {
        let (mut registry, _directory, mut definition) = fixture();
        definition.precheck = Some(crate::automations::Precheck {
            command: "exit 0".into(),
            timeout_seconds: 5,
        });
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let claim = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        assert!(claim.accepted.is_none());
        let mut renamed = automation.definition;
        renamed.name = "Current standup name".into();
        registry
            .edit_automation(
                automation.id,
                automation.revision,
                renamed,
                210,
                test_identity(),
            )
            .unwrap();
        let accepted = registry
            .finish_automation_precheck(
                claim.firing.id,
                crate::host::automations::precheck::Outcome::Passed,
                220,
            )
            .unwrap()
            .unwrap();
        let model = registry.model().unwrap();
        let channel = model.channel(accepted.agent.channel().unwrap()).unwrap();
        assert_eq!(channel.title, "Current standup name");
        assert_eq!(
            channel.title_source,
            crate::model::ChannelTitleSource::Manual
        );
    }

    #[test]
    fn automation_channel_name_survives_renames_provider_titles_and_subsequent_runs() {
        use crate::model::ChannelTitleSource;
        for mode in [SessionMode::Fresh, SessionMode::Reuse] {
            for custom_title in [None, Some("My standup channel")] {
                let (mut registry, directory, mut definition) = fixture();
                definition.session_mode = mode;
                let (automation, firing, agent) = completed_reusable_run(&mut registry, definition);
                let channel = registry
                    .model()
                    .unwrap()
                    .channel(agent.channel().unwrap())
                    .unwrap()
                    .clone();
                assert_eq!(channel.title, "Review daily");
                assert_eq!(channel.title_source, ChannelTitleSource::Manual);
                let released = if mode == SessionMode::Fresh {
                    registry.close_agent(&agent.origin, agent.id).unwrap()
                } else {
                    agent.clone()
                };
                registry
                    .finish_automation_release(firing.id, &released, 230)
                    .unwrap();
                if let Some(title) = custom_title {
                    registry
                        .apply(&ModelEdit::RetitleChannel {
                            id: agent.channel().unwrap(),
                            title: title.into(),
                        })
                        .unwrap();
                }
                let mut renamed = automation.definition.clone();
                renamed.name = "Renamed automation".into();
                let renamed = registry
                    .edit_automation(
                        automation.id,
                        automation.revision,
                        renamed,
                        240,
                        test_identity(),
                    )
                    .unwrap();
                let expected_title = custom_title.unwrap_or("Review daily");
                assert_eq!(
                    registry
                        .model()
                        .unwrap()
                        .channel(agent.channel().unwrap())
                        .unwrap()
                        .title,
                    expected_title
                );
                registry
                    .apply(&ModelEdit::SuggestChannelTitle {
                        id: agent.channel().unwrap(),
                        title: "Provider-generated task title".into(),
                        source: ChannelTitleSource::Provider,
                    })
                    .unwrap();
                let due = Occurrence {
                    due_ms: 300,
                    local: "2026-09-28T09:00".into(),
                };
                let enabled = registry
                    .set_automation_enabled(
                        renamed.id,
                        renamed.revision,
                        Some(due.clone()),
                        true,
                        250,
                    )
                    .unwrap();
                drop(registry);
                let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
                let next = registry
                    .claim_automation(
                        enabled.id,
                        enabled.execution_revision,
                        Some(&due),
                        crate::automations::Trigger::Scheduled {
                            occurrence: due.clone(),
                        },
                        Some(Occurrence {
                            due_ms: 400,
                            local: "2026-09-29T09:00".into(),
                        }),
                        false,
                        300,
                    )
                    .unwrap()
                    .accepted
                    .unwrap();
                assert_eq!(next.agent.placement, agent.placement);
                assert_eq!(next.agent.id == agent.id, mode == SessionMode::Reuse);
                let model = registry.model().unwrap();
                assert_eq!(model.channels.len(), 1);
                let channel = model.channel(agent.channel().unwrap()).unwrap();
                assert_eq!(channel.title, expected_title);
                assert_eq!(channel.title_source, ChannelTitleSource::Manual);
            }
        }
    }

    #[test]
    fn interactive_takeover_between_idle_receipt_and_release_keeps_the_firing_owned() {
        let (mut registry, _directory, definition) = fixture();
        let (automation, firing, agent) = completed_reusable_run(&mut registry, definition);
        registry.claim_agent_interaction(agent.id, None).unwrap();
        assert!(
            registry
                .finish_automation_release(firing.id, &agent, 230)
                .unwrap_err()
                .contains("Interactive")
        );
        assert_eq!(
            registry.automation(automation.id).unwrap().active_firing,
            Some(firing.id)
        );
        let closed = registry.close_agent(&agent.origin, agent.id).unwrap();
        registry
            .finish_automation_release(firing.id, &closed, 240)
            .unwrap();
        assert!(
            registry
                .automation(automation.id)
                .unwrap()
                .active_firing
                .is_none()
        );
    }

    #[test]
    fn changing_launch_or_removing_definition_cannot_orphan_an_idle_reused_provider() {
        let (mut registry, _directory, definition) = fixture();
        let (automation, firing, agent) = completed_reusable_run(&mut registry, definition.clone());
        registry
            .finish_automation_release(firing.id, &agent, 230)
            .unwrap();
        let mut changed = definition;
        changed.agent.model = Some("another-model".into());
        assert!(
            registry
                .edit_automation(automation.id, 1, changed.clone(), 240, test_identity())
                .is_err()
        );
        assert!(registry.remove_automation(automation.id, 1, 240).is_err());
        registry.close_agent(&agent.origin, agent.id).unwrap();
        let updated = registry
            .edit_automation(automation.id, 1, changed, 250, test_identity())
            .unwrap();
        assert!(updated.reuse_agent.is_none());
        registry
            .remove_automation(automation.id, updated.revision, 260)
            .unwrap();
        assert!(registry.automation(automation.id).is_err());
        let historical = registry.automation_run_detail(firing.id).unwrap();
        assert_eq!(historical.firing.id, firing.id);
        assert_eq!(historical.firing.state, FiringState::Succeeded);
        assert_eq!(historical.definition, automation.definition);
    }

    #[test]
    fn interrupted_execution_pauses_and_cannot_be_cleared_by_definition_edit() {
        use crate::agent_execution::{State, Update};
        let (mut registry, _directory, definition) = fixture();
        let automation = registry
            .create_automation(definition.clone(), 100, test_identity())
            .unwrap();
        let claimed = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        let run = claimed.accepted.unwrap().run;
        registry
            .update_agent_run(
                run.id,
                run.revision,
                Update::Finished {
                    state: State::Interrupted,
                    result: Default::default(),
                },
                210,
            )
            .unwrap();
        registry.reconcile_automation_run(run.id, 220).unwrap();
        let paused = registry.automation(automation.id).unwrap();
        assert!(!paused.enabled);
        assert!(paused.next.is_none());
        assert!(paused.blocked.is_some());
        assert_eq!(paused.active_firing, Some(claimed.firing.id));
        let mut changed = definition;
        changed.prompt = "Another operation".into();
        assert!(
            registry
                .edit_automation(
                    automation.id,
                    paused.revision,
                    changed,
                    230,
                    test_identity()
                )
                .is_err()
        );
        assert!(
            registry
                .remove_automation(automation.id, paused.revision, 230)
                .is_err()
        );
        assert!(
            registry
                .claim_automation(
                    automation.id,
                    paused.execution_revision,
                    None,
                    crate::automations::Trigger::Manual {
                        request: Uuid::new_v4()
                    },
                    None,
                    true,
                    230
                )
                .is_err()
        );
    }
    #[test]
    fn queued_notifications_survive_reopen_until_the_journal_receipt_is_acknowledged() {
        let (mut registry, directory, definition) = fixture();
        let created = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let enabled = registry
            .set_automation_enabled(
                created.id,
                1,
                Some(Occurrence {
                    due_ms: 1000,
                    local: "next".into(),
                }),
                true,
                200,
            )
            .unwrap();
        registry
            .set_automation_enabled(created.id, enabled.revision, None, false, 300)
            .unwrap();
        let events = registry.automation_notifications().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            registry.automation_notifications().unwrap(),
            events,
            "a stopped/full broker queue has no durable receipt and must not consume the outbox"
        );
        drop(registry);
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(
            registry.automation_notifications().unwrap(),
            events,
            "a daemon interrupted after enqueue retries the exact same evidence"
        );
        registry
            .acknowledge_automation_notifications(&["another-event".into()].into())
            .unwrap();
        assert_eq!(registry.automation_notifications().unwrap(), events);
        registry
            .acknowledge_automation_notifications(&[events[0].evidence.key.clone()].into())
            .unwrap();
        assert!(registry.automation_notifications().unwrap().is_empty());
    }

    #[test]
    fn closing_an_enabled_project_emits_one_named_pause_notice_with_a_recovery_reason() {
        let (mut registry, _directory, definition) = fixture();
        let repository = definition.agent.target.repository.clone();
        let created = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        registry
            .set_automation_enabled(
                created.id,
                1,
                Some(Occurrence {
                    due_ms: 1000,
                    local: "next".into(),
                }),
                true,
                200,
            )
            .unwrap();
        registry
            .apply(&ModelEdit::CloseProject { repository })
            .unwrap();
        let events = registry.automation_notifications().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].evidence.kind,
            crate::notifications::Kind::AutomationPaused
        );
        assert!(
            matches!(&events[0].target,crate::notifications::Target::Automation {id,reason,..}
            if *id==created.id && reason.contains("closed") && reason.contains("reopen"))
        );
    }

    #[test]
    fn changed_skip_reasons_and_successful_runs_break_coalescing() {
        use crate::automations::{SkipReason, Trigger};
        let (mut registry, _directory, definition) = fixture();
        let automation = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let accepted = registry
            .claim_automation(
                automation.id,
                1,
                None,
                Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        let tx = registry.db.transaction().unwrap();
        let mut template = accepted.firing;
        template.run = None;
        template.agent = None;
        template.finished_ms = Some(300);
        template.state = FiringState::Skipped;
        template.skip = Some(SkipReason::Overlap);
        template.id = Uuid::from_u128(1);
        template.created_ms = 300;
        template.trigger = Trigger::Scheduled {
            occurrence: Occurrence {
                due_ms: 300,
                local: "300".into(),
            },
        };
        insert_or_coalesce_firing(&tx, &mut template, "scheduled:300").unwrap();
        let first_id = template.id;
        template.id = Uuid::from_u128(3);
        template.created_ms = 300;
        template.skip = Some(SkipReason::Capacity);
        template.trigger = Trigger::Scheduled {
            occurrence: Occurrence {
                due_ms: 400,
                local: "400".into(),
            },
        };
        insert_or_coalesce_firing(&tx, &mut template, "scheduled:400").unwrap();
        assert_ne!(template.id, first_id);
        assert_eq!(template.skipped_occurrences.as_ref().unwrap().count, 1);
        let mut success = template.clone();
        success.id = Uuid::from_u128(2);
        success.created_ms = 300;
        success.state = FiringState::Succeeded;
        success.skip = None;
        success.skipped_occurrences = None;
        success.trigger = Trigger::Scheduled {
            occurrence: Occurrence {
                due_ms: 500,
                local: "500".into(),
            },
        };
        insert_or_coalesce_firing(&tx, &mut success, "scheduled:500").unwrap();
        template.id = Uuid::from_u128(4);
        template.created_ms = 300;
        template.trigger = Trigger::Scheduled {
            occurrence: Occurrence {
                due_ms: 600,
                local: "600".into(),
            },
        };
        insert_or_coalesce_firing(&tx, &mut template, "scheduled:600").unwrap();
        assert_eq!(template.skipped_occurrences.as_ref().unwrap().count, 1);
        let count: usize = tx
            .query_row("SELECT count(*) FROM automation_firings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 5);
    }

    #[test]
    fn saved_conversations_are_bounded_read_only_and_headless_remains_manually_readable() {
        let (mut registry, _directory, mut definition) = fixture();
        definition.visibility = crate::automations::Visibility::Headless;
        let (_automation, firing, agent) = completed_reusable_run(&mut registry, definition);
        let agent = registry.close_agent(&agent.origin, agent.id).unwrap();
        registry
            .finish_automation_release(firing.id, &agent, 230)
            .unwrap();
        let item = |id: &str, text: String| {
            std::sync::Arc::new(crate::chat::Item {
                id: id.into(),
                role: crate::chat::Role::Assistant,
                content: crate::chat::ItemContent::Text {
                    text,
                    thinking: false,
                    phase: Default::default(),
                },
                started_at: None,
                at: None,
            })
        };
        registry
            .archive_automation_conversation(
                firing.id,
                &agent,
                crate::chat::Snapshot {
                    session_id:Some("bounded-archive-thread".into()),
                    items: vec![
                        item("oversized", "x".repeat(4 * 1024 * 1024)),
                        item("latest", "Latest result".into()),
                    ],
                    ready: true,
                    busy: true,
                    waiting: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let archive = registry.archived_automation_conversation(agent.id).unwrap();
        assert_eq!(archive.snapshot.items.len(), 1);
        assert_eq!(archive.snapshot.items[0].id, "latest");
        assert!(!archive.snapshot.ready && !archive.snapshot.busy && !archive.snapshot.waiting);
        assert!(archive.snapshot.exited);
        assert!(
            archive
                .snapshot
                .read_only_reason
                .as_deref()
                .unwrap()
                .contains("omitted")
        );
        assert!(serde_json::to_vec(&archive.snapshot).unwrap().len() < 4 * 1024 * 1024);
        assert!(
            registry.archived_automation_agents().unwrap().is_empty(),
            "headless archives have no automatic pane"
        );
        registry
            .db
            .execute(
                "DELETE FROM automation_firings WHERE id=?1",
                [firing.id.to_string()],
            )
            .unwrap();
        assert!(
            registry
                .archived_automation_conversation(agent.id)
                .err()
                .unwrap()
                .contains("expired"),
            "history retention must also retire its archived transcript"
        );
        assert!(registry.is_archived_automation_session(agent.session.as_ref().unwrap().id()).unwrap(),
            "expiring saved history never authorizes the closed automation provider to restart");
    }
    #[test]
    fn changing_visibility_preserves_active_run_and_execution_consent() {
        let (mut registry, _directory, definition) = fixture();
        let created = registry
            .create_automation(definition, 100, test_identity())
            .unwrap();
        let enabled = registry
            .set_automation_enabled(
                created.id,
                1,
                Some(Occurrence {
                    due_ms: 60_000,
                    local: "next".into(),
                }),
                true,
                200,
            )
            .unwrap();
        let firing = registry
            .claim_automation(
                created.id,
                enabled.execution_revision,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                enabled.next.clone(),
                true,
                300,
            )
            .unwrap();
        let mut changed = enabled.definition.clone();
        changed.visibility = crate::automations::Visibility::Headless;
        let sequence = registry.execution_sequence().unwrap();
        let updated = registry
            .edit_automation(created.id, enabled.revision, changed, 400, test_identity())
            .unwrap();
        assert!(updated.enabled);
        assert_eq!(updated.execution_revision, enabled.execution_revision);
        assert_eq!(updated.active_firing, Some(firing.firing.id));
        assert_eq!(registry.execution_sequence().unwrap(), sequence + 1);
        assert_eq!(
            registry.headless_automation_agents().unwrap(),
            vec![firing.accepted.unwrap().agent.id]
        );
    }

    #[test]
    fn archive_storage_failure_preserves_owner_and_emits_a_named_blocker() {
        let (mut registry, _directory, definition) = fixture();
        let (automation,done,agent)=completed_reusable_run(&mut registry,definition);
        registry.db.execute_batch("CREATE TRIGGER reject_saved_view BEFORE INSERT ON automation_snapshots BEGIN SELECT RAISE(ABORT,'fixture storage refusal'); END;").unwrap();
        assert!(
            !registry
                .preserve_automation_conversation(
                    done.id,
                    &agent,
                    crate::chat::Snapshot {session_id:Some("storage-fixture-thread".into()),..Default::default()},
                    240
                )
                .unwrap()
        );
        let current = registry.automation(automation.id).unwrap();
        assert_eq!(current.active_firing, Some(done.id));
        assert!(
            current
                .blocked
                .as_deref()
                .unwrap()
                .contains("fixture storage refusal")
        );
        assert!(!registry.agent(agent.id).unwrap().closed);
        let events = registry.automation_notifications().unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0].target,crate::notifications::Target::Automation {id,reason,..}
            if *id==automation.id && reason.contains("saved result"))
        );
    }

    #[test]
    fn retention_uses_completion_time_for_old_runs_with_recent_results() {
        let (mut registry, _directory, definition) = fixture();
        let (_automation, firing, agent) = completed_reusable_run(&mut registry, definition);
        let closed = registry.close_agent(&agent.origin, agent.id).unwrap();
        registry
            .finish_automation_release(firing.id, &closed, 230)
            .unwrap();
        registry.db.execute("UPDATE automation_firings SET created_ms=0,record=json_set(record,'$.created_ms',0) WHERE id=?1",[firing.id.to_string()]).unwrap();
        registry
            .prune_automation_history(HISTORY_TTL_MS + 50)
            .unwrap();
        assert!(
            registry.automation_firing(firing.id).is_ok(),
            "a recent result or coalesced skip survives even if its first occurrence is old"
        );
        registry
            .prune_automation_history(HISTORY_TTL_MS + 1000)
            .unwrap();
        assert!(registry.automation_firing(firing.id).is_err());
    }
    #[test]
    fn proved_closed_interrupted_history_expires_but_unresolved_owner_survives() {
        use crate::agent_execution::{State,Update};
        let (mut registry,_directory,definition)=fixture();
        let automation=registry.create_automation(definition,100,test_identity()).unwrap();
        let interrupt=|registry:&mut Registry,at:u64| {
            let claim=registry.claim_automation(automation.id,1,None,crate::automations::Trigger::Manual {request:Uuid::new_v4()},None,true,at).unwrap();
            let accepted=claim.accepted.unwrap();
            registry.update_agent_run(accepted.run.id,accepted.run.revision,Update::Finished {state:State::Interrupted,result:Default::default()},at+1).unwrap();
            let firing=registry.reconcile_automation_run(accepted.run.id,at+2).unwrap().unwrap();
            (firing,accepted.agent)
        };
        let (closed,agent)=interrupt(&mut registry,200);
        let agent=registry.close_agent(&agent.origin,agent.id).unwrap();
        registry.finish_automation_release(closed.id,&agent,210).unwrap();
        let (blocked,_agent)=interrupt(&mut registry,300);
        registry.prune_automation_history(HISTORY_TTL_MS+1000).unwrap();
        assert!(registry.automation_firing(closed.id).is_err(),"proved cleanup retires an interrupted row under normal history TTL");
        assert!(registry.automation_firing(blocked.id).is_ok(),"an unresolved interrupted owner remains protected");
    }

    #[test]
    fn scoped_discovery_keeps_old_offline_destinations_beyond_the_global_push_window() {
        use crate::agent_execution::Placement;
        let (mut registry,directory,mut definition)=fixture();definition.session_mode=SessionMode::Fresh;
        let worktree=Placement::Worktree {repository:directory.path().into(),path:directory.path().into()};
        let proof=crate::model::HostSessionProof {host_session_id:crate::model::HostSessionId::new("archive-test"),boot_id:crate::model::BootId::new("archive-boot"),host_incarnation:crate::model::HostIncarnation::new("archive-incarnation")};
        let mut first=None;
        for index in 0..1025 {
            let mut saved=definition.clone();
            if index>0 {saved.agent.target.placement=Some(worktree.clone());}
            let (_,firing,agent)=completed_reusable_run(&mut registry,saved);
            let agent=registry.close_agent(&agent.origin,agent.id).unwrap();
            registry.finish_automation_release(firing.id,&agent,230).unwrap();
            registry.archive_automation_conversation(firing.id,&agent,crate::chat::Snapshot {session_id:Some(format!("paged-thread-{index}")),..Default::default()}).unwrap();
            registry.db.execute("UPDATE automation_firings SET created_ms=?2,record=json_set(record,'$.created_ms',?2) WHERE id=?1",params![firing.id.to_string(),1000+index as u64]).unwrap();
            if first.is_none() {first=Some(registry.agent(agent.id).unwrap());}
        }
        let first=first.unwrap();let model=registry.model().unwrap();
        assert_eq!(model.archived_automation_agents.len(),1024);
        assert!(!model.archived_automation_agents.iter().any(|agent|agent.id==first.id));
        let oldest=registry.archived_automation_agents_page(first.placement.clone(),model.identity,model.sequence,None,64,proof.clone()).unwrap();
        assert_eq!(oldest.agents,vec![first.clone()]);assert!(oldest.next.is_none());
        let mut after=None;let mut seen=std::collections::HashSet::new();
        loop {
            let page=registry.archived_automation_agents_page(worktree.clone(),model.identity,model.sequence,after,64,proof.clone()).unwrap();
            assert!(page.agents.len()<=64);
            for agent in page.agents {assert!(seen.insert(agent.id));}
            match page.next {Some(next)=>{assert!(after.is_none_or(|previous|previous<next));after=Some(next);},None=>break,}
        }
        assert_eq!(seen.len(),1024);
        let archive=registry.archived_automation_conversation(first.id).unwrap();
        let crate::agent_execution::HostedSession::Chat(session)=first.session.as_ref().unwrap() else {unreachable!()};
        assert!(crate::automations::archive::ArchivedPaneReadiness::qualify(first.id,session,&archive).is_ok());
        registry.db.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0",[]).unwrap();
        assert!(registry.archived_automation_agents_page(first.placement,model.identity,model.sequence,None,64,proof).is_err(),"a changed view/model must restart discovery before using its old cursor");
    }

    #[test]
    fn headless_completion_preserves_an_explicitly_published_exact_pane() {
        let (mut registry,_directory,mut definition)=fixture();
        definition.visibility=crate::automations::Visibility::Headless;
        let (_,firing,agent)=completed_reusable_run(&mut registry,definition);
        let agent=registry.close_agent(&agent.origin,agent.id).unwrap();
        registry.finish_automation_release(firing.id,&agent,230).unwrap();
        registry.archive_automation_conversation(firing.id,&agent,crate::chat::Snapshot {session_id:Some("headless-manually-shown".into()),..Default::default()}).unwrap();
        assert!(registry.model().unwrap().archived_automation_agents.is_empty(),"headless completion creates no automatic pane");
        let agent=registry.agent(agent.id).unwrap();
        let Some(crate::agent_execution::HostedSession::Chat(session))=&agent.session else {unreachable!()};
        registry.apply(&ModelEdit::SeedView {scope:agent.placement.clone(),panes:vec![crate::workspace_model::views::Pane {id:crate::model::CardId::from_uuid(agent.id.as_uuid()),title:"Explicit saved view".into(),user_title:None,pending_terminal_start:false,width_fraction:1.0,kind:crate::model::CardKind::Chat(session.clone()),scope:None}],closed:vec![]}).unwrap();
        let model=registry.model().unwrap();
        assert_eq!(model.archived_automation_agents,vec![agent.clone()],"headless prevents automatic creation; a pane explicitly opened before completion still becomes its exact read-only result");
        let proof=session.proof.clone().unwrap();
        let page=registry.archived_automation_agents_page(agent.placement.clone(),model.identity,model.sequence,None,64,proof).unwrap();
        assert_eq!(page.agents,vec![agent]);
    }

    #[test]
    fn one_hundred_precheck_skips_merge_only_after_durable_outcome_and_retirement() {
        use crate::automations::{Precheck,SkipReason,Trigger};
        use crate::host::automations::precheck::Outcome;
        let (mut registry,_directory,mut definition)=fixture();
        definition.precheck=Some(Precheck {command:"exit 3".into(),timeout_seconds:1});
        let created=registry.create_automation(definition,100,test_identity()).unwrap();
        let enabled=registry.set_automation_enabled(created.id,1,Some(Occurrence {due_ms:60_000,local:"minute-0".into()}),true,200).unwrap();
        for index in 0..100 {
            let due=registry.automation(created.id).unwrap().next.unwrap();
            let next=Occurrence {due_ms:due.due_ms+60_000,local:format!("minute-{}",index+1)};
            let claim=registry.claim_automation(created.id,enabled.execution_revision,Some(&due),Trigger::Scheduled {occurrence:due.clone()},Some(next),false,due.due_ms).unwrap();
            assert!(registry.coalesce_retired_automation_precheck(claim.firing.id,claim.firing.revision).is_err());
            assert_eq!(registry.automation(created.id).unwrap().active_firing,Some(claim.firing.id),"an unresolved helper's original owner row cannot be merged");
            registry.finish_automation_precheck(claim.firing.id,Outcome::Stopped {state:FiringState::Skipped,skip:Some(SkipReason::PrecheckExit),diagnostic:"predicate declined".into()},due.due_ms+1).unwrap();
            assert!(registry.automation_firing(claim.firing.id).is_ok(),"durable completion alone does not erase the helper's cleanup key");
            registry.coalesce_retired_automation_precheck(claim.firing.id,claim.firing.revision).unwrap();
        }
        let history=registry.automation_history(Some(created.id),None,100).unwrap();
        assert_eq!(history.firings.len(),1);let summary=history.firings[0].skipped_occurrences.as_ref().unwrap();
        assert_eq!((summary.count,summary.first_occurrence_ms,summary.last_occurrence_ms),(100,60_000,6_000_000));
        assert_eq!(registry.automation_notifications().unwrap().len(),1);
        let due=registry.automation(created.id).unwrap().next.unwrap();
        let next=Occurrence {due_ms:due.due_ms+60_000,local:"after interruption".into()};
        let claim=registry.claim_automation(created.id,enabled.execution_revision,Some(&due),Trigger::Scheduled {occurrence:due.clone()},Some(next),false,due.due_ms).unwrap();
        registry.interrupt_automation_precheck(claim.firing.id,"cleanup receipt unavailable".into(),due.due_ms+1).unwrap();
        assert!(registry.coalesce_retired_automation_precheck(claim.firing.id,claim.firing.revision).is_err());
        assert_eq!(registry.automation(created.id).unwrap().active_firing,Some(claim.firing.id));
        assert_eq!(registry.automation_firing(claim.firing.id).unwrap().state,FiringState::Interrupted);
        assert!(registry.finish_automation_precheck(claim.firing.id,Outcome::Stopped {state:FiringState::Skipped,skip:Some(SkipReason::PrecheckExit),diagnostic:"late proved cleanup".into()},due.due_ms+2).unwrap().is_none());
        registry.coalesce_retired_automation_precheck(claim.firing.id,claim.firing.revision).unwrap();
        assert_eq!(registry.automation_firing(claim.firing.id).unwrap().state,FiringState::Interrupted,"late cleanup never turns uncertain work into a fresh skip or new provider dispatch");
        assert_eq!(registry.automation_history(Some(created.id),None,100).unwrap().firings.len(),2);
    }

    #[test]
    fn retention_keeps_settled_precheck_proof_until_helper_retirement_survives_restart() {
        use crate::automations::{Precheck,SkipReason,Trigger};
        use crate::host::automations::precheck::Outcome;
        let (mut registry,directory,mut definition)=fixture();
        definition.precheck=Some(Precheck {command:"exit 3".into(),timeout_seconds:1});
        let created=registry.create_automation(definition,100,test_identity()).unwrap();
        let claim=registry.claim_automation(created.id,1,None,Trigger::Manual {request:Uuid::new_v4()},None,true,200).unwrap();
        registry.finish_automation_precheck(claim.firing.id,Outcome::Stopped {state:FiringState::Skipped,skip:Some(SkipReason::PrecheckExit),diagnostic:"declined".into()},210).unwrap();
        let late=HISTORY_TTL_MS+1000;
        registry.prune_automation_history(late).unwrap();
        assert!(registry.automation_firing(claim.firing.id).is_ok(),"durable outcome alone does not prove the helper process and inherited lease retired");
        // Simulate an older store lacking the new cleanup authority table.
        registry.db.execute("DROP TABLE automation_precheck_retirements",[]).unwrap();
        drop(registry);
        let mut registry=Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        registry.prune_automation_history(late).unwrap();
        assert!(registry.automation_firing(claim.firing.id).is_ok());
        assert_eq!(registry.pending_automation_precheck_retirements(None).unwrap(),vec![(claim.firing.id,claim.firing.revision)]);
        registry.coalesce_retired_automation_precheck(claim.firing.id,claim.firing.revision).unwrap();
        assert!(registry.pending_automation_precheck_retirements(None).unwrap().is_empty());
        drop(registry);
        let mut registry=Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert!(registry.pending_automation_precheck_retirements(None).unwrap().is_empty(),"backfill never forgets a committed retirement receipt");
        registry.prune_automation_history(late).unwrap();
        assert!(registry.automation_firing(claim.firing.id).is_err());
        let markers:u64=registry.db.query_row("SELECT count(*) FROM automation_precheck_retirements",[],|row|row.get(0)).unwrap();
        assert_eq!(markers,0,"retired marker metadata expires atomically with its history");
    }

}
