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
         CREATE INDEX IF NOT EXISTS automation_history ON automation_firings(automation, created_ms DESC, id DESC);
         CREATE INDEX IF NOT EXISTS automation_active ON automation_firings(state);
         CREATE UNIQUE INDEX IF NOT EXISTS automation_run ON automation_firings(run_id) WHERE run_id IS NOT NULL;
         PRAGMA user_version = 2;
         COMMIT;"
    ).map_err(|error| error.to_string())
}

impl Registry {
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
        let firing = Firing {
            id: Uuid::new_v4(),
            automation: id,
            revision,
            trigger,
            state: FiringState::Skipped,
            release_pending: false,
            created_ms: now_ms,
            started_ms: None,
            finished_ms: Some(now_ms),
            agent: None,
            run: None,
            skip: Some(SkipReason::MissedDeadline),
            diagnostic: Some(format!(
                "Older scheduled occurrences were skipped. Next eligible occurrence: {} ({})",
                next.local, automation.definition.schedule.timezone
            )),
        };
        insert_firing(&tx, &firing, &key)?;
        automation.next = Some(next);
        automation.updated_ms = now_ms;
        write_automation(&tx, &automation)?;
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
        definition.validate()?;
        let automation = Automation {
            id: Uuid::new_v4(),
            revision: 1,
            execution_revision: 1,
            definition,
            target_identity,
            enabled: false,
            created_ms: now_ms,
            updated_ms: now_ms,
            next: None,
            active_firing: None,
            reuse_agent: None,
            channel: None,
            blocked: None,
        };
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        require_project(&tx, &automation.definition)?;
        tx.execute(
            "INSERT INTO automations(id,revision,record,summary,enabled,next_due_ms,updated_ms)
                    VALUES(?1,?2,?3,?4,0,NULL,?5)",
            params![
                automation.id.to_string(),
                automation.revision,
                encode(&automation)?,
                encode(&automation.summary(None))?,
                now_ms
            ],
        )
        .map_err(|error| error.to_string())?;
        write_revision(&tx, &automation)?;
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
        let material = without_name != automation.definition;
        let new_agent = definition.agent != automation.definition.agent
            || definition.session_mode != automation.definition.session_mode;
        let new_target = definition.agent.target != automation.definition.agent.target;
        if material && (automation.active_firing.is_some() || automation.blocked.is_some()) {
            return Err(
                "Resolve the active or interrupted run before changing its execution definition"
                    .into(),
            );
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
                automation.channel = None;
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
        tx.execute("UPDATE automations SET deleted_ms=?2,enabled=0,next_due_ms=NULL,revision=revision+1,updated_ms=?2 WHERE id=?1", params![id.to_string(), now_ms])
            .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())
    }

    pub(crate) fn automation_history(
        &self,
        id: Uuid,
        before: Option<HistoryCursor>,
        limit: usize,
    ) -> Result<HistoryPage, String> {
        let limit = limit.clamp(1, MAX_HISTORY_PAGE);
        let mut query = self
            .db
            .prepare(
                "SELECT record FROM automation_firings WHERE automation=?1
            AND (?2 IS NULL OR created_ms < ?2 OR (created_ms = ?2 AND id < ?3))
            ORDER BY created_ms DESC,id DESC LIMIT ?4",
            )
            .map_err(|error| error.to_string())?;
        let records = query
            .query_map(
                params![
                    id.to_string(),
                    before.as_ref().map(|cursor| cursor.created_ms),
                    before.map(|cursor| cursor.id.to_string()),
                    limit + 1
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| error.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        let mut firings = records
            .into_iter()
            .map(|record| decode::<Firing>(&record))
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
    let last: Option<String> = tx.query_row("SELECT record FROM automation_firings WHERE automation=?1 ORDER BY created_ms DESC,id DESC LIMIT 1", [automation.id.to_string()], |row| row.get(0))
        .optional().map_err(|error| error.to_string())?;
    let last_state = last
        .map(|record| decode::<Firing>(&record).map(|firing| firing.state))
        .transpose()?;
    tx.execute("UPDATE automations SET revision=?2,record=?3,summary=?4,enabled=?5,next_due_ms=?6,updated_ms=?7,active_firing=?8 WHERE id=?1 AND deleted_ms IS NULL",
        params![automation.id.to_string(), automation.revision, encode(automation)?, encode(&automation.summary(last_state))?, automation.enabled, automation.next.as_ref().map(|next| next.due_ms), automation.updated_ms, automation.active_firing.map(|id| id.to_string())])
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

fn require_revision(automation: &Automation, expected: u64) -> Result<(), String> {
    if automation.revision != expected {
        return Err("Automation changed; reload it before applying this action".into());
    }
    Ok(())
}

fn require_project(tx: &Transaction<'_>, definition: &Definition) -> Result<(), String> {
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
        if automation.active_firing.is_none() && automation.definition.precheck.is_some() {
            let count: usize = tx.query_row("SELECT count(*) FROM automations a JOIN automation_firings f ON f.id=a.active_firing WHERE f.run_id IS NULL", [], |row| row.get(0)).map_err(|error| error.to_string())?;
            if count >= crate::automations::MAX_CONCURRENT_PRECHECKS {
                return Err("All precheck slots are occupied; wait for running checks or resolve interrupted owners".into());
            }
        }
        let mut firing = Firing {
            id: Uuid::new_v4(),
            automation: id,
            revision: automation.execution_revision,
            trigger,
            state: FiringState::Prechecking,
            release_pending: false,
            created_ms: now_ms,
            started_ms: None,
            finished_ms: None,
            agent: None,
            run: None,
            skip: None,
            diagnostic: None,
        };
        let definition = automation.definition.clone();
        if automation.active_firing.is_some() {
            firing.state = FiringState::Skipped;
            firing.skip = Some(SkipReason::Overlap);
            firing.finished_ms = Some(now_ms);
            firing.diagnostic = Some("An earlier run is still active".into());
        } else {
            automation.active_firing = Some(firing.id);
        }
        insert_firing(&tx, &firing, &key)?;
        let accepted = if firing.state == FiringState::Prechecking && definition.precheck.is_none()
        {
            accept_firing(&tx, &mut automation, &mut firing, &definition, &key, now_ms)?
        } else {
            None
        };
        automation.updated_ms = now_ms;
        write_firing(&tx, &firing)?;
        write_automation(&tx, &automation)?;
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
        if firing.state != FiringState::Prechecking {
            return Ok(None);
        }
        let mut automation = read_automation(&tx, firing.automation)?;
        if automation.active_firing != Some(firing_id) {
            return Err("Precheck no longer owns this automation's active slot".into());
        }
        let current = automation.execution_revision == firing.revision
            && (matches!(firing.trigger, crate::automations::Trigger::Manual { .. })
                || automation.enabled);
        let accepted = if !current {
            firing.state = FiringState::Skipped;
            firing.skip = Some(SkipReason::DefinitionChanged);
            firing.diagnostic =
                Some("Automation changed or was disabled before the precheck completed".into());
            None
        } else {
            match outcome {
                Outcome::Passed => {
                    let definition = read_definition(&tx, firing.automation, firing.revision)?;
                    let key = occurrence_key(firing.revision, &firing.trigger);
                    accept_firing(&tx, &mut automation, &mut firing, &definition, &key, now_ms)?
                }
                Outcome::Stopped {
                    state,
                    skip,
                    diagnostic,
                } => {
                    firing.state = state;
                    firing.skip = skip;
                    firing.diagnostic = Some(diagnostic);
                    None
                }
            }
        };
        if !firing.state.is_active() {
            firing.finished_ms = Some(now_ms);
            automation.active_firing = None;
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
        spec.target.channel = automation.channel;
        Destination::New(spec)
    };
    let request = Submit {
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
            tx.execute_batch("RELEASE automation_accept")
                .map_err(|error| error.to_string())?;
            firing.agent = Some(accepted.agent.id);
            firing.run = Some(accepted.run.id);
            firing.state = FiringState::Accepted;
            automation.channel = Some(accepted.agent.channel);
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
                    channel: None,
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
            },
            schedule: Schedule {
                pattern: SchedulePattern::Daily { hour: 9, minute: 0 },
                timezone: "America/Vancouver".into(),
            },
            session_mode: SessionMode::Reuse,
            precheck: None,
        };
        (registry, directory, definition)
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
            Some(accepted.agent.channel)
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
                .automation_history(automation.id, None, 100)
                .unwrap()
                .firings
                .len(),
            1
        );
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
        assert!(
            registry
                .claim_automation(extra.id, 1, None, trigger, None, true, 205)
                .is_ok()
        );
        assert_eq!(registry.active_automation_firings().unwrap().len(), 4);
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
}
