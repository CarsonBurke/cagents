//! Execution acceptance shares the project/channel transaction boundary.

use super::{Registry, repository_key, take_ordinal};
use crate::{
    agent_execution::{
        Accepted, Agent, Authority, Destination, HostedSession, Lineage, MAX_CHILDREN,
        MAX_PARENT_DEPTH, MAX_PARENT_RUNNING, MAX_RUNNING, Origin, Placement, ResultSummary, Run,
        State, Submit, Update,
    },
    model::{AgentId, ChannelId, RunId},
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

const AGENTS_FOR_ORIGIN: &str =
    "SELECT record FROM agents WHERE closed = 0 AND origin = ?1 ORDER BY rowid DESC LIMIT 1024";
const ACTIVE_AGENTS: &str =
    "SELECT record FROM agents WHERE closed = 0 ORDER BY rowid DESC LIMIT 1024";

/// The host proves this actor state before inspecting or retiring authority.
#[derive(Clone, Copy)]
pub(in crate::host) enum ResidentRelease {
    ReadOnlyHistory,
    CompletedWriter,
}

/// A self capability is not a managed agent binding. A history reader cannot
/// prove teardown of a former writer, so only a completed writer may retire it.
fn resident_release_authority(
    db: &Connection,
    source: &HostedSession,
    kind: ResidentRelease,
) -> Result<Option<(String, String)>, String> {
    if !source.has_complete_proof() {
        return Err("Resident release requires a complete process identity".into());
    }
    let managed: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM agents WHERE session_id=?1 AND closed=0)
             OR EXISTS(SELECT 1 FROM agent_launches WHERE session_id=?1)",
            [source.id().to_string()],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if managed {
        return Err(
            "The actor belongs to a managed agent; use its supported owned-agent close operation"
                .into(),
        );
    }
    let lineage: String = db
        .query_row(
            "SELECT coalesce((SELECT lineage FROM session_lineages WHERE session_id=?1), ?1)",
            [source.id().to_string()],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let parent = Lineage(
        lineage
            .parse()
            .map_err(|_| "Stored session lineage is invalid")?,
    );
    let origin = encode(&Origin::Child { parent })?;
    let delegated: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM agents WHERE origin=?1 AND closed=0)
             OR EXISTS(SELECT 1 FROM agent_runs WHERE origin=?1 AND state IN ('accepted','running','waiting'))
             OR EXISTS(SELECT 1 FROM agent_launches p JOIN agents a ON p.agent=a.id WHERE a.origin=?1)",
        [&origin], |row| row.get(0),
    ).map_err(|error| error.to_string())?;
    if delegated {
        return Err("The session lineage retains delegated agents or unconfirmed work; close its owned agents first".into());
    }
    let authority: Option<(String, String)> = db
        .query_row(
            "SELECT token,source FROM agent_authorities WHERE session_id=?1",
            [source.id().to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some((_, stored)) = &authority {
        if matches!(kind, ResidentRelease::ReadOnlyHistory) {
            return Err("A retained provider or managed agent authority remains; a history reader cannot prove its teardown".into());
        }
        // Provider-confirmed thread metadata can advance after startup; the
        // immutable process recipe and daemon proof still must match exactly.
        if !source.same_process(&decode::<HostedSession>(stored)?) {
            return Err("The registered provider authority belongs to a different process; its successor was preserved".into());
        }
    }
    Ok(authority)
}

pub(super) fn migrate(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    // Version zero is the existing unversioned project/channel registry.
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS agents (
             id TEXT PRIMARY KEY,
             origin TEXT NOT NULL,
             record TEXT NOT NULL,
             session_id TEXT UNIQUE,
             closed INTEGER NOT NULL CHECK(closed IN (0, 1))
         ) STRICT;
         CREATE INDEX IF NOT EXISTS agents_by_origin ON agents(origin, closed);
         CREATE TABLE IF NOT EXISTS agent_runs (
             id TEXT PRIMARY KEY,
             agent TEXT NOT NULL REFERENCES agents(id),
             origin TEXT NOT NULL,
             idempotency_key TEXT NOT NULL,
             request TEXT NOT NULL,
             state TEXT NOT NULL,
             revision INTEGER NOT NULL,
             record TEXT NOT NULL,
             UNIQUE(origin, idempotency_key)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS agent_runs_by_agent ON agent_runs(agent);
         CREATE INDEX IF NOT EXISTS agent_runs_by_state ON agent_runs(state, origin);
         CREATE TABLE IF NOT EXISTS agent_dispatches (
             run TEXT PRIMARY KEY REFERENCES agent_runs(id),
             claimed_at INTEGER NOT NULL
         ) STRICT;
         CREATE TABLE IF NOT EXISTS agent_launches (
             agent TEXT PRIMARY KEY REFERENCES agents(id),
             session_id TEXT NOT NULL UNIQUE,
             source TEXT NOT NULL
         ) STRICT;
         CREATE TABLE IF NOT EXISTS agent_authorities (
             token TEXT PRIMARY KEY,
             session_id TEXT NOT NULL UNIQUE,
             source TEXT NOT NULL
         ) STRICT;
         CREATE TABLE IF NOT EXISTS agent_policies (
             agent TEXT PRIMARY KEY REFERENCES agents(id),
             directory TEXT,
             interactive INTEGER NOT NULL DEFAULT 0 CHECK(interactive IN (0, 1)),
             interactive_run TEXT
         ) STRICT;",
    )
    .map_err(|error| error.to_string())
}

/// Legacy records omit the name, so SQLite's NULL uniqueness semantics allow
/// them all. The index also fences writes outside the acceptance helper.
pub(super) fn migrate_names(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS agents_by_name
        ON agents(origin, json_extract(record, '$.name')) WHERE closed = 0;",
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

impl Registry {
    pub(crate) fn inspect_resident_release(
        &mut self,
        source: &HostedSession,
        kind: ResidentRelease,
    ) -> Result<(), String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        resident_release_authority(&tx, source, kind)?;
        tx.commit().map_err(|error| error.to_string())
    }

    /// The caller holds the exclusive daemon control gate through removal of
    /// the exact resident. This drains submissions that authenticated before
    /// retirement and excludes new acceptance until the capability is revoked.
    /// Do not hold HostState while acquiring this transaction.
    pub(crate) fn retire_resident_release(
        &mut self,
        source: &HostedSession,
        kind: ResidentRelease,
    ) -> Result<(), String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        if let Some((token, stored)) = resident_release_authority(&tx, source, kind)? {
            tx.execute(
                "DELETE FROM agent_authorities WHERE session_id=?1 AND token=?2 AND source=?3",
                params![source.id().to_string(), token, stored],
            )
            .map_err(|error| error.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())
    }

    /// Automatic removal must also wait for durable work not yet represented
    /// by an actor, including unconfirmed launch ownership after a restart.
    pub(crate) fn cleanup_execution_is_quiescent(&self) -> Result<bool, String> {
        self.db.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM agent_launches)
                AND NOT EXISTS(SELECT 1 FROM agent_runs WHERE state IN ('accepted', 'running', 'waiting'))
                AND NOT EXISTS(SELECT 1 FROM agents WHERE closed = 0 AND session_id IS NOT NULL)",
            [], |row| row.get(0),
        ).map_err(|error| error.to_string())
    }
    /// Target directories of durable agent work that is running, or accepted
    /// and yet to launch. Removing one would pull it out from under that work.
    pub(crate) fn active_agent_directories(&self) -> Result<Vec<std::path::PathBuf>, String> {
        let mut query = self
            .db
            .prepare(
                "SELECT a.record FROM agents a WHERE a.closed = 0 AND (
                 a.session_id IS NOT NULL
                 OR EXISTS(SELECT 1 FROM agent_launches p WHERE p.agent = a.id)
                 OR EXISTS(SELECT 1 FROM agent_runs r WHERE r.agent = a.id
                     AND r.state IN ('accepted', 'running', 'waiting')))",
            )
            .map_err(|error| error.to_string())?;
        let records = query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        records
            .iter()
            .map(|record| decode::<Agent>(record).map(|agent| agent.spec.target.directory))
            .collect()
    }
    /// Accepted work can be queued behind the daemon's update write gate before
    /// it owns a hosted actor. Do not lose it from impact merely because there
    /// is no terminal/chat to project yet. Call outside HostState; final update
    /// admission also holds the control write gate to exclude new acceptance.
    pub(crate) fn agent_update_blockers(&self) -> Result<Vec<String>, String> {
        let mut query = self.db.prepare(
            "SELECT a.record FROM agents a WHERE a.closed = 0 AND (
                 EXISTS(SELECT 1 FROM agent_launches p WHERE p.agent = a.id)
                 OR EXISTS(SELECT 1 FROM agent_runs r WHERE r.agent = a.id AND
                     (r.state = 'accepted' OR (a.session_id IS NULL AND r.state IN ('running', 'waiting'))))
             ) ORDER BY a.id LIMIT ?1",
        ).map_err(|error| error.to_string())?;
        let records = query
            .query_map([MAX_RUNNING as u64 + 1], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        let mut blockers = records
            .iter()
            .take(MAX_RUNNING)
            .map(|record| {
                let agent: Agent = decode(record)?;
                Ok(format!(
                    "{} agent {} has accepted work or launch ownership awaiting confirmation",
                    agent.spec.provider.label(),
                    agent.id
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        if records.len() > MAX_RUNNING {
            blockers.push("Additional agent launches must settle before updating".into());
        }
        Ok(blockers)
    }

    /// Caller has authenticated every retained descriptor with the actual
    /// holder. A same-ID recipe is not sufficient evidence for this operation.
    /// Authority and public/private ownership advance in one transaction; a
    /// pending launch stays private and no accepted prompt is reauthorized.
    pub(crate) fn adopt_authenticated_agent_session(
        &mut self,
        retained: &[HostedSession],
        successor: &HostedSession,
    ) -> Result<bool, String> {
        if retained.is_empty() || retained.iter().any(|source| source.id() != successor.id()) {
            return Err("Agent adoption requires exact same-session holder lineage".into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let authority: Option<String> = tx
            .query_row(
                "SELECT source FROM agent_authorities WHERE session_id = ?1",
                [successor.id().to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let recognizes = |source: &HostedSession| source == successor || retained.contains(source);
        if let Some(authority) = &authority
            && !recognizes(&decode::<HostedSession>(authority)?)
        {
            return Err("The issuing capability belongs to another session lineage".into());
        }
        let row: Option<String> = tx.query_row("SELECT record FROM agents WHERE session_id = ?1 OR id = (SELECT agent FROM agent_launches WHERE session_id = ?1)", [successor.id().to_string()], |row| row.get(0)).optional().map_err(|error| error.to_string())?;
        let mut changed = false;
        if let Some(row) = row {
            let mut agent: Agent = decode(&row)?;
            if agent.closed {
                return Err("A retired agent cannot regain process authority".into());
            }
            if let Some(source) = &agent.session {
                if !recognizes(source) {
                    return Err("The durable agent belongs to another session lineage".into());
                }
                if let Some(console) = &agent.console {
                    let HostedSession::Chat(next) = successor else {
                        return Err("A provider console needs its retained native actor".into());
                    };
                    let terminal = next
                        .terminal
                        .as_ref()
                        .filter(|terminal| {
                            terminal.id == console.id && terminal.launch == console.launch
                        })
                        .ok_or("The retained console does not match the durable provider pair")?;
                    agent.console = Some(terminal.clone());
                }
                changed = agent.session.as_ref() != Some(successor);
                agent.session = Some(successor.clone());
                changed |= super::views::adopt_authenticated_agent_pane(
                    &tx, agent.id, retained, successor,
                )?;
                tx.execute(
                    "UPDATE agents SET record = ?2 WHERE id = ?1",
                    params![agent.id.to_string(), encode(&agent)?],
                )
                .map_err(|error| error.to_string())?;
            } else {
                let pending: String = tx
                    .query_row(
                        "SELECT source FROM agent_launches WHERE agent = ?1",
                        [agent.id.to_string()],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                if !recognizes(&decode::<HostedSession>(&pending)?) {
                    return Err("The private launch belongs to another session lineage".into());
                }
                tx.execute(
                    "UPDATE agent_launches SET source = ?2 WHERE agent = ?1",
                    params![agent.id.to_string(), encode(successor)?],
                )
                .map_err(|error| error.to_string())?;
            }
        }
        if authority.is_some() {
            tx.execute(
                "UPDATE agent_authorities SET source = ?2 WHERE session_id = ?1",
                params![successor.id().to_string(), encode(successor)?],
            )
            .map_err(|error| error.to_string())?;
        }
        if changed {
            tx.execute(
                "UPDATE metadata SET sequence = sequence + 1 WHERE key = 0",
                [],
            )
            .map_err(|error| error.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())?;
        Ok(changed)
    }

    pub(crate) fn retained_agent_directory(
        &self,
        retained: &[HostedSession],
    ) -> Result<Option<crate::agent_execution::DirectoryIdentity>, String> {
        let Some(first) = retained.first() else {
            return Ok(None);
        };
        let row: Option<String> = self.db.query_row("SELECT record FROM agents WHERE session_id = ?1 OR id = (SELECT agent FROM agent_launches WHERE session_id = ?1)", [first.id().to_string()], |row| row.get(0)).optional().map_err(|error| error.to_string())?;
        let Some(row) = row else { return Ok(None) };
        let agent: Agent = decode(&row)?;
        let source = agent
            .session
            .or(self.pending_agent_launch(agent.id)?)
            .ok_or("Retained agent has no source evidence")?;
        if agent.closed || !retained.contains(&source) {
            return Err("Retained execution policy does not match the authenticated source".into());
        }
        self.agent_directory(agent.id)
    }

    pub(crate) fn pin_agent_directory(
        &mut self,
        id: AgentId,
        identity: crate::agent_execution::DirectoryIdentity,
    ) -> Result<(), String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT directory FROM agent_policies WHERE agent = ?1",
                [id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .flatten();
        if previous
            .as_deref()
            .map(decode::<crate::agent_execution::DirectoryIdentity>)
            .transpose()?
            .is_some_and(|previous| previous != identity)
        {
            return Err("The agent's approved directory identity cannot change".into());
        }
        tx.execute(
            "INSERT INTO agent_policies(agent, directory) VALUES (?1, ?2)
            ON CONFLICT(agent) DO UPDATE SET directory = excluded.directory",
            params![id.to_string(), encode(&identity)?],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())
    }

    pub(crate) fn agent_directory(
        &self,
        id: AgentId,
    ) -> Result<Option<crate::agent_execution::DirectoryIdentity>, String> {
        self.db
            .query_row(
                "SELECT directory FROM agent_policies WHERE agent = ?1",
                [id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .flatten()
            .map(|value| decode(&value))
            .transpose()
    }

    pub(crate) fn agent_for_session(
        &self,
        id: crate::model::TerminalId,
    ) -> Result<Option<Agent>, String> {
        self.db
            .query_row(
                "SELECT record FROM agents WHERE session_id = ?1 AND closed = 0",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .map(|record| decode(&record))
            .transpose()
    }

    /// Durable descriptors that require exact holder-receipt validation before
    /// teardown can retire them, including a launch not yet publicly bound.
    pub(crate) fn retained_agent_sources(
        &self,
        id: crate::model::TerminalId,
    ) -> Result<Vec<HostedSession>, String> {
        let mut sources = Vec::new();
        if let Some(source) = self.retained_agent_authority_source(id)? {
            sources.push(source);
        }
        if let Some(agent) = self.agent_for_session(id)?
            && let Some(source) = agent.session
        {
            sources.push(source);
        }
        let pending: Option<String> = self
            .db
            .query_row(
                "SELECT source FROM agent_launches WHERE session_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some(pending) = pending {
            sources.push(decode(&pending)?);
        }
        Ok(sources)
    }

    pub(crate) fn claim_agent_interaction(
        &mut self,
        id: AgentId,
        run: Option<RunId>,
    ) -> Result<(), String> {
        self.db.execute("INSERT INTO agent_policies(agent, interactive, interactive_run) VALUES (?1, 1, ?2)
            ON CONFLICT(agent) DO UPDATE SET interactive = 1, interactive_run = excluded.interactive_run",
            params![id.to_string(), run.map(|run| run.to_string())]).map_err(|error| error.to_string())?;
        Ok(())
    }

    pub(crate) fn agent_is_interactive(&self, id: AgentId) -> Result<bool, String> {
        self.db
            .query_row(
                "SELECT interactive FROM agent_policies WHERE agent = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map(|value| value.unwrap_or(false))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn agent_dispatch_is_authorized(
        &self,
        id: AgentId,
        run: RunId,
    ) -> Result<bool, String> {
        self.db.query_row("SELECT interactive = 0 OR interactive_run = ?2 FROM agent_policies WHERE agent = ?1",
            params![id.to_string(), run.to_string()], |row| row.get::<_, Option<bool>>(0))
            .optional().map(|value| value.map(|value| value.unwrap_or(false)).unwrap_or(true)).map_err(|error| error.to_string())
    }

    /// Private launch intent commits before creating a provider. It is never
    /// part of WorkspaceModel or an adoptable UI descriptor.
    pub(crate) fn reserve_agent_launch(
        &mut self,
        id: AgentId,
        source: &HostedSession,
    ) -> Result<(), String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let agent = read_agent(&tx, id)?;
        if agent.closed || agent.session.is_some() {
            return Err("Agent already has a hosted session".into());
        }
        require_placement(&tx, &agent.placement, &agent.spec.target.repository)?;
        tx.execute(
            "INSERT INTO agent_launches(agent, session_id, source) VALUES (?1, ?2, ?3)",
            params![id.to_string(), source.id().to_string(), encode(source)?],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    pub(crate) fn pending_agent_launch(
        &self,
        id: AgentId,
    ) -> Result<Option<HostedSession>, String> {
        self.db
            .query_row(
                "SELECT source FROM agent_launches WHERE agent = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .map(|record| decode(&record))
            .transpose()
    }

    /// Only a launch owner may clear this after proving no provider was
    /// created or acknowledging full teardown. A restart is never such proof.
    pub(crate) fn clear_quiescent_agent_launch(
        &mut self,
        id: AgentId,
        source: &HostedSession,
    ) -> Result<(), String> {
        self.db
            .execute(
                "DELETE FROM agent_launches WHERE agent = ?1 AND source = ?2",
                params![id.to_string(), encode(source)?],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    /// A post-commit once-only fence, shared by interactive and scheduled
    /// dispatch. The caller performs no launch/send until this commit returns.
    /// An accepted-but-unproved run is never redispatched after a restart.
    pub(crate) fn claim_agent_dispatch(
        &mut self,
        accepted: &Accepted,
        now_ms: u64,
    ) -> Result<bool, String> {
        if !accepted.created || accepted.run.state != State::Accepted {
            return Ok(false);
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let stored: String = tx
            .query_row(
                "SELECT record FROM agent_runs WHERE id = ?1",
                [accepted.run.id.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        let agent = read_agent(&tx, accepted.agent.id)?;
        if decode::<Run>(&stored)? != accepted.run || !same_dispatch_agent(&agent, &accepted.agent)
        {
            return Err(
                "The accepted agent run changed before dispatch; nothing was submitted".into(),
            );
        }
        let already_claimed: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_dispatches WHERE run=?1)",
                [accepted.run.id.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if already_claimed {
            return Ok(false);
        }
        if !Self::automation_dispatch_allowed_in(&tx, accepted, &agent, now_ms)? {
            tx.commit().map_err(|error| error.to_string())?;
            return Ok(false);
        }
        require_placement(&tx, &agent.placement, &agent.spec.target.repository)?;
        let claimed = tx
            .execute(
                "INSERT OR IGNORE INTO agent_dispatches(run, claimed_at) VALUES (?1, ?2)",
                params![accepted.run.id.to_string(), now_ms],
            )
            .map_err(|error| error.to_string())?
            == 1;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(claimed)
    }

    pub(crate) fn execution_origin(&self, authority: &Authority) -> Result<Origin, String> {
        let Some(token) = authority.token() else {
            return Ok(Origin::User);
        };
        if token.len() != 36 || uuid::Uuid::parse_str(token).is_err() {
            return Err("Invalid agent session authority".into());
        }
        // A session owns what its whole lineage started.
        let lineage: String = self
            .db
            .query_row(
                "SELECT coalesce(l.lineage, a.session_id) FROM agent_authorities a
                 LEFT JOIN session_lineages l ON l.session_id = a.session_id
                 WHERE a.token = ?1",
                [token],
                |row| row.get(0),
            )
            .map_err(|_| "This agent session authority is no longer registered".to_owned())?;
        Ok(Origin::Child {
            parent: Lineage(
                lineage
                    .parse()
                    .map_err(|_| "Stored agent session identity is invalid")?,
            ),
        })
    }

    /// The service also verifies this source against its live/adopted actor.
    /// A retained token alone does not prove that its issuing process survived.
    pub(crate) fn agent_authority_source(
        &self,
        authority: &Authority,
    ) -> Result<Option<HostedSession>, String> {
        self.execution_origin(authority)?;
        authority
            .token()
            .map(|token| {
                let source: String = self
                    .db
                    .query_row(
                        "SELECT source FROM agent_authorities WHERE token = ?1",
                        [token],
                        |row| row.get(0),
                    )
                    .map_err(|_| {
                        "This agent session authority is no longer registered".to_owned()
                    })?;
                decode(&source)
            })
            .transpose()
    }

    /// Retirement is exact-source fenced: a late close from the old daemon
    /// incarnation cannot revoke the authority of its authenticated successor.
    pub(crate) fn retire_agent_authority(&mut self, source: &HostedSession) -> Result<(), String> {
        self.db
            .execute(
                "DELETE FROM agent_authorities WHERE session_id = ?1 AND source = ?2",
                params![source.id().to_string(), encode(source)?],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Invoked only after holder authentication and a successful adoption of
    /// this exact retained source. This CAS rotates only host proof, retaining
    /// the capability already inherited by the surviving process and tools.
    pub(crate) fn adopt_agent_authority(
        &mut self,
        source: &HostedSession,
        successor: &HostedSession,
    ) -> Result<bool, String> {
        let mut normalized = successor.clone();
        match (&mut normalized, source) {
            (HostedSession::Terminal(next), HostedSession::Terminal(old)) => {
                if next.continuity.live_proof().is_none() || old.continuity.live_proof().is_none() {
                    return Err(
                        "Only proved live terminal lineage can preserve agent authority".into(),
                    );
                }
                next.continuity = old.continuity.clone();
            }
            (HostedSession::Chat(next), HostedSession::Chat(old)) => {
                if next.proof.as_ref().is_none_or(|proof| !proof.is_complete())
                    || old.proof.as_ref().is_none_or(|proof| !proof.is_complete())
                {
                    return Err(
                        "Only proved native chat lineage can preserve agent authority".into(),
                    );
                }
                next.proof = old.proof.clone();
            }
            _ => {
                return Err("Agent authority cannot change session surface during adoption".into());
            }
        }
        if normalized != *source {
            return Err("Agent authority adoption changed the retained session identity".into());
        }
        self.db
            .execute(
                "UPDATE agent_authorities SET source = ?3 WHERE session_id = ?1 AND source = ?2",
                params![source.id().to_string(), encode(source)?, encode(successor)?],
            )
            .map(|changed| changed == 1)
            .map_err(|error| error.to_string())
    }

    /// Only an actual newly spawned session or a proved exact reattachment can
    /// call this. The token never appears in `Agent` or its public descriptor.
    pub(crate) fn register_agent_authority(
        &mut self,
        session: &HostedSession,
    ) -> Result<Authority, String> {
        self.register_authority(session, false)
    }

    /// A fresh process cannot reuse the capability of an exited same-ID
    /// process. Authenticated holder adoption uses the proof-fenced CAS.
    pub(crate) fn fresh_agent_authority(
        &mut self,
        session: &HostedSession,
    ) -> Result<Authority, String> {
        self.register_authority(session, true)
    }

    fn register_authority(
        &mut self,
        session: &HostedSession,
        fresh: bool,
    ) -> Result<Authority, String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let id = session.id().to_string();
        if fresh {
            tx.execute("DELETE FROM agent_authorities WHERE session_id = ?1", [&id])
                .map_err(|error| error.to_string())?;
        }
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT token, source FROM agent_authorities WHERE session_id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some((token, source)) = existing {
            if decode::<HostedSession>(&source)? != *session {
                return Err("Agent session authority does not match the live descriptor".into());
            }
            return Ok(Authority::session(token));
        }
        let token = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO agent_authorities(token, session_id, source) VALUES (?1, ?2, ?3)",
            params![token, id, encode(session)?],
        )
        .map_err(|error| error.to_string())?;
        // A session not recorded as a successor starts its own lineage; one
        // that was keeps it, through every later process under its ID.
        tx.execute(
            "INSERT OR IGNORE INTO session_lineages(session_id, lineage) VALUES (?1, ?1)",
            [&id],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(Authority::session(token))
    }

    /// Durable acceptance precedes any process creation or prompt delivery.
    /// The caller authenticates `origin` and canonicalizes the target first.
    pub(crate) fn submit_agent(
        &mut self,
        origin: &Origin,
        request: &Submit,
        now_ms: u64,
    ) -> Result<Accepted, String> {
        self.submit_agent_with_spec(origin, request, None, now_ms)
    }

    pub(crate) fn submit_agent_with_spec(
        &mut self,
        origin: &Origin,
        request: &Submit,
        selected: Option<&crate::agent_execution::Spec>,
        now_ms: u64,
    ) -> Result<Accepted, String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let accepted = Self::submit_agent_selected_in(&tx, origin, request, selected, now_ms)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(accepted)
    }

    /// Shared with the scheduler's firing/revision claim. This performs no
    /// external I/O and never commits the transaction owned by its caller.
    pub(super) fn submit_agent_in(
        tx: &Transaction<'_>,
        origin: &Origin,
        request: &Submit,
        now_ms: u64,
    ) -> Result<Accepted, String> {
        Self::submit_agent_selected_in(tx, origin, request, None, now_ms)
    }

    fn submit_agent_selected_in(
        tx: &Transaction<'_>,
        origin: &Origin,
        request: &Submit,
        selected: Option<&crate::agent_execution::Spec>,
        now_ms: u64,
    ) -> Result<Accepted, String> {
        request.validate()?;
        let origin_json = encode(origin)?;
        let request_json = encode(request)?;
        let previous: Option<(String, String)> = tx
            .query_row(
                "SELECT request, record FROM agent_runs WHERE origin = ?1 AND idempotency_key = ?2",
                params![origin_json, request.idempotency_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some((previous, run)) = previous {
            if decode::<Submit>(&previous)? != *request {
                return Err(
                    "The idempotency key already identifies a different agent request".into(),
                );
            }
            let run: Run = decode(&run)?;
            let agent = read_agent(&tx, run.agent)?;
            return Ok(Accepted {
                agent,
                run,
                created: false,
            });
        }
        check_parent_depth(tx, origin)?;

        let (running, owned): (usize, usize) = tx
            .query_row(
                "SELECT count(*), coalesce(sum(r.origin = ?1), 0)
                 FROM agent_runs r JOIN agents a ON a.id = r.agent
                 WHERE r.state IN ('accepted', 'running', 'waiting')
                    OR (r.state = 'interrupted' AND a.closed = 0)",
                [&origin_json],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| error.to_string())?;
        if running >= MAX_RUNNING || owned >= MAX_PARENT_RUNNING {
            return Err("Agent concurrency limit reached; wait for or close existing work".into());
        }
        let agent = match &request.destination {
            Destination::Existing(id) => {
                let agent = read_agent(&tx, *id)?;
                if &agent.origin != origin {
                    return Err("This caller does not own the agent".into());
                }
                if agent.closed {
                    return Err("The agent is closed".into());
                }
                let active: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM agent_runs WHERE agent = ?1
                     AND state IN ('accepted', 'running', 'waiting', 'interrupted'))",
                        [id.to_string()],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                if active {
                    return Err("The agent has unfinished or unverified work; inspect and close it before starting another run".into());
                }
                // A removed channel must never be resurrected by a follow-up.
                require_placement(&tx, &agent.placement, &agent.spec.target.repository)?;
                agent
            }
            Destination::New(spec) => {
                // Persist the resolved account for this accepted launch while
                // keeping the caller's original request as its retry key.
                let spec = selected.unwrap_or(spec);
                // Fanout bounds delegated work, not the user's retained
                // conversations. Every origin still pays the run limits above.
                if !matches!(origin, Origin::User) {
                    let children: usize = tx
                        .query_row(
                            "SELECT count(*) FROM agents WHERE origin = ?1 AND closed = 0",
                            [&origin_json],
                            |row| row.get(0),
                        )
                        .map_err(|error| error.to_string())?;
                    if children >= MAX_CHILDREN {
                        return Err(
                            "Agent child limit reached; close an existing child first".into()
                        );
                    }
                }
                if let Some(name) = &request.name {
                    let used: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM agents WHERE origin=?1 AND closed=0 AND json_extract(record, '$.name')=?2)",
                        params![origin_json, name], |row| row.get(0),
                    ).map_err(|error| error.to_string())?;
                    if used {
                        return Err(format!("You already have an open agent named {name}"));
                    }
                }
                let repository = repository_key(&spec.target.repository)?;
                let exists: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM projects WHERE repository = ?1)",
                        [&repository],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                if !exists {
                    return Err("The target project is not open".into());
                }
                let placement = match &spec.target.placement {
                    Some(placement) => {
                        require_placement(&tx, placement, &spec.target.repository)?;
                        placement.clone()
                    }
                    None => {
                        let channel = ChannelId::new();
                        let ordinal = take_ordinal(&tx, "next_channel_ordinal")?;
                        tx.execute(
                            "INSERT INTO channels(id, repository, title, ordinal, title_source) VALUES (?1, ?2, ?3, ?4, 0)",
                            params![channel.to_string(), repository, crate::workspace_model::channel_title_from_prompt(&request.prompt).unwrap_or_else(|| format!("{} agent", spec.provider.label())), ordinal],
                        ).map_err(|error| error.to_string())?;
                        super::channel_work::created_in(tx, channel)?;
                        Placement::Channel(channel)
                    }
                };
                let agent = Agent {
                    name: request.name.clone(),
                    id: AgentId::new(),
                    origin: origin.clone(),
                    spec: spec.clone(),
                    placement,
                    session: None,
                    console: None,
                    created_ms: now_ms,
                    closed: false,
                };
                tx.execute(
                    "INSERT INTO agents(id, origin, record, closed) VALUES (?1, ?2, ?3, 0)",
                    params![agent.id.to_string(), origin_json, encode(&agent)?],
                )
                .map_err(|error| error.to_string())?;
                agent
            }
        };
        let run = Run {
            id: RunId::new(),
            agent: agent.id,
            state: State::Accepted,
            revision: 1,
            accepted_ms: now_ms,
            started_ms: None,
            finished_ms: None,
            provider_key: None,
            result: Default::default(),
        };
        tx.execute(
            "INSERT INTO agent_runs(id, agent, origin, idempotency_key, request, state, revision, record)
             VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', 1, ?6)",
            params![run.id.to_string(), agent.id.to_string(), origin_json, request.idempotency_key, request_json, encode(&run)?],
        ).map_err(|error| error.to_string())?;
        tx.execute(
            "UPDATE metadata SET sequence = sequence + 1 WHERE key = 0",
            [],
        )
        .map_err(|error| error.to_string())?;
        Ok(Accepted {
            agent,
            run,
            created: true,
        })
    }

    pub(crate) fn agent_run(&self, id: RunId) -> Result<Run, String> {
        let record: String = self
            .db
            .query_row(
                "SELECT record FROM agent_runs WHERE id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        decode(&record)
    }

    pub(crate) fn agent(&self, id: AgentId) -> Result<Agent, String> {
        read_agent(&self.db, id)
    }

    pub(crate) fn execution_sequence(&self) -> Result<u64, String> {
        self.db
            .query_row("SELECT sequence FROM metadata WHERE key = 0", [], |row| {
                row.get(0)
            })
            .map_err(|error| error.to_string())
    }

    pub(crate) fn previous_agent_acceptance(
        &self,
        origin: &Origin,
        request: &Submit,
    ) -> Result<Option<Accepted>, String> {
        let row: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT request, record FROM agent_runs WHERE origin = ?1 AND idempotency_key = ?2",
                params![encode(origin)?, request.idempotency_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        row.map(|(stored, run)| {
            if decode::<Submit>(&stored)? != *request {
                return Err(
                    "The idempotency key already identifies a different agent request".into(),
                );
            }
            let run: Run = decode(&run)?;
            Ok(Accepted {
                agent: self.agent(run.agent)?,
                run,
                created: false,
            })
        })
        .transpose()
    }

    pub(crate) fn validate_agent_dispatch(&self, accepted: &Accepted) -> Result<(), String> {
        let agent = self.agent(accepted.agent.id)?;
        let run = self.agent_run(accepted.run.id)?;
        if agent.closed || run != accepted.run || !same_dispatch_agent(&agent, &accepted.agent) {
            return Err("The accepted execution changed before provider dispatch".into());
        }
        require_placement(&self.db, &agent.placement, &agent.spec.target.repository)
    }

    pub(crate) fn agent_request(&self, run: RunId) -> Result<Submit, String> {
        let request: String = self
            .db
            .query_row(
                "SELECT request FROM agent_runs WHERE id = ?1",
                [run.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        decode(&request)
    }

    pub(crate) fn latest_agent_run(&self, agent: AgentId) -> Result<Run, String> {
        let record: String = self
            .db
            .query_row(
                "SELECT record FROM agent_runs WHERE agent = ?1 ORDER BY rowid DESC LIMIT 1",
                [agent.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        decode(&record)
    }

    pub(crate) fn agents(&self, origin: Option<&Origin>) -> Result<Vec<Agent>, String> {
        // Keep the origin predicate indexable instead of hiding it behind an
        // optional-parameter OR that scans unrelated callers' records.
        let records = match origin {
            Some(origin) => {
                let mut statement = self
                    .db
                    .prepare(AGENTS_FOR_ORIGIN)
                    .map_err(|error| error.to_string())?;
                statement
                    .query_map([encode(origin)?], |row| row.get::<_, String>(0))
                    .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
                    .map_err(|error| error.to_string())?
            }
            None => {
                let mut statement = self
                    .db
                    .prepare(ACTIVE_AGENTS)
                    .map_err(|error| error.to_string())?;
                statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
                    .map_err(|error| error.to_string())?
            }
        };
        records.iter().map(|record| decode(record)).collect()
    }

    /// Bind the daemon-issued descriptor once. Adoption may later replace it
    /// only after independently validating the exact prior descriptor lineage.
    pub(crate) fn bind_agent(
        &mut self,
        id: AgentId,
        expected: Option<&HostedSession>,
        session: HostedSession,
    ) -> Result<Agent, String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let mut agent = read_agent(&tx, id)?;
        if agent.closed || agent.session.as_ref() != expected {
            return Err("The agent session changed before attachment was saved".into());
        }
        let pending: Option<String> = tx
            .query_row(
                "SELECT source FROM agent_launches WHERE agent = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(pending) = pending
            && !decode::<HostedSession>(&pending)?.same_process(&session)
        {
            return Err("The provider does not match the committed launch intent".into());
        }
        require_placement(&tx, &agent.placement, &agent.spec.target.repository)?;
        agent.session = Some(session);
        tx.execute(
            "DELETE FROM agent_launches WHERE agent = ?1",
            [id.to_string()],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE metadata SET sequence = sequence + 1 WHERE key = 0",
            [],
        )
        .map_err(|error| error.to_string())?;
        tx.execute(
            "UPDATE agents SET record = ?2, session_id = ?3 WHERE id = ?1",
            params![
                id.to_string(),
                encode(&agent)?,
                agent
                    .session
                    .as_ref()
                    .map(|session| session.id().to_string())
            ],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(agent)
    }

    pub(crate) fn bind_agent_console(
        &mut self,
        id: AgentId,
        expected: &HostedSession,
        console: crate::model::TerminalSession,
    ) -> Result<Agent, String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let mut agent = read_agent(&tx, id)?;
        if agent.closed
            || agent.session.as_ref() != Some(expected)
            || agent.console.is_some()
            || expected.id() != console.id
        {
            return Err("The provider session changed before its console was attached".into());
        }
        require_placement(&tx, &agent.placement, &agent.spec.target.repository)?;
        agent.console = Some(console);
        tx.execute(
            "UPDATE agents SET record = ?2 WHERE id = ?1",
            params![id.to_string(), encode(&agent)?],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE metadata SET sequence = sequence + 1 WHERE key = 0",
            [],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(agent)
    }

    pub(crate) fn retained_agent_authority_source(
        &self,
        id: crate::model::TerminalId,
    ) -> Result<Option<HostedSession>, String> {
        let source: Option<String> = self
            .db
            .query_row(
                "SELECT source FROM agent_authorities WHERE session_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        source.map(|source| decode(&source)).transpose()
    }

    pub(crate) fn retained_agent_authority(
        &self,
        id: crate::model::TerminalId,
    ) -> Result<Authority, String> {
        let token = self
            .db
            .query_row(
                "SELECT token FROM agent_authorities WHERE session_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .map_err(|_| "The paired provider has no retained execution authority".to_owned())?;
        Ok(Authority::session(token))
    }

    /// Retire an exact process after confirmed teardown, including closes from
    /// the UI. Run state, capacity, delegated authority and the exact retained
    /// presentation change in the same transaction.
    pub(crate) fn retire_agent_process(
        &mut self,
        source: &HostedSession,
        now_ms: u64,
    ) -> Result<bool, String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        super::schema::require_current(&tx)?;
        let authority: Option<String> = tx
            .query_row(
                "SELECT source FROM agent_authorities WHERE session_id = ?1",
                [source.id().to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(authority) = authority
            && source.same_process(&decode::<HostedSession>(&authority)?)
        {
            tx.execute(
                "DELETE FROM agent_authorities WHERE session_id = ?1",
                [source.id().to_string()],
            )
            .map_err(|e| e.to_string())?;
        }
        let record: Option<String> = tx
            .query_row(
                "SELECT a.record FROM agents a LEFT JOIN agent_launches p ON p.agent = a.id WHERE (a.session_id = ?1 OR p.session_id = ?1) AND a.closed = 0",
                [source.id().to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let mut changed = false;
        if let Some(record) = record {
            let mut agent: Agent = decode(&record)?;
            let pending: Option<String> = tx
                .query_row(
                    "SELECT source FROM agent_launches WHERE agent = ?1",
                    [agent.id.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            let pending = pending
                .map(|record| decode::<HostedSession>(&record))
                .transpose()?;
            if agent
                .session
                .as_ref()
                .or(pending.as_ref())
                .is_some_and(|expected| expected.same_process(source))
            {
                let records = tx.prepare("SELECT record FROM agent_runs WHERE agent = ?1 AND state IN ('accepted', 'running', 'waiting')")
                    .map_err(|e| e.to_string())?.query_map([agent.id.to_string()], |row| row.get::<_, String>(0))
                    .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>).map_err(|e| e.to_string())?;
                for record in records {
                    let mut run: Run = decode(&record)?;
                    run.state = State::Cancelled;
                    if pending.is_some() {
                        run.result.diagnostic = Some("The provider was retired before its launch could be bound; no prompt was submitted".into());
                    }
                    run.finished_ms = Some(now_ms);
                    run.revision = run
                        .revision
                        .checked_add(1)
                        .ok_or("Agent run revision exhausted")?;
                    tx.execute("UPDATE agent_runs SET state = 'cancelled', revision = ?2, record = ?3 WHERE id = ?1",
                        params![run.id.to_string(), run.revision, encode(&run)?]).map_err(|e| e.to_string())?;
                    super::automation_attention::stage(&tx, &run)?;
                }
                // ChatClose and daemon shutdown retire through this path before
                // the execution service can reach close_agent. Commit the pane
                // tombstone here too, so a crash cannot publish a closed agent
                // while leaving its exact retained presentation behind.
                super::views::retire_agent(&tx, &agent)?;
                agent.closed = true;
                tx.execute(
                    "DELETE FROM agent_launches WHERE agent = ?1",
                    [agent.id.to_string()],
                )
                .map_err(|e| e.to_string())?;
                tx.execute(
                    "UPDATE agents SET closed = 1, record = ?2 WHERE id = ?1",
                    params![agent.id.to_string(), encode(&agent)?],
                )
                .map_err(|e| e.to_string())?;
                tx.execute(
                    "UPDATE metadata SET sequence = sequence + 1 WHERE key = 0",
                    [],
                )
                .map_err(|e| e.to_string())?;
                changed = true;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(changed)
    }

    /// Called after confirmed runtime teardown, never before requesting it.
    pub(crate) fn close_agent(&mut self, origin: &Origin, id: AgentId) -> Result<Agent, String> {
        if self.pending_agent_launch(id)?.is_some() {
            return Err("The pending provider launch has not been conclusively retired".into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        super::schema::require_current(&tx)?;
        let mut agent = read_agent(&tx, id)?;
        if &agent.origin != origin {
            return Err("This caller does not own the agent".into());
        }
        let active: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_runs WHERE agent = ?1
             AND state IN ('accepted', 'running', 'waiting'))",
                [id.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if active {
            return Err("The agent still has an unfinished run".into());
        }
        let pane_retired = super::views::retire_agent(&tx, &agent)?;
        if agent.closed && !pane_retired {
            tx.commit().map_err(|error| error.to_string())?;
            return Ok(agent);
        }
        agent.closed = true;
        tx.execute(
            "UPDATE metadata SET sequence = sequence + 1 WHERE key = 0",
            [],
        )
        .map_err(|error| error.to_string())?;
        tx.execute(
            "UPDATE agents SET closed = 1, record = ?2 WHERE id = ?1",
            params![id.to_string(), encode(&agent)?],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(agent)
    }

    /// Refuse known unavailable presentation authority before stopping a
    /// provider. The commit repeats the same predicate under its write TX.
    pub(crate) fn preflight_agent_close(
        &mut self,
        origin: &Origin,
        id: AgentId,
    ) -> Result<Agent, String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        let agent = read_agent(&tx, id)?;
        if &agent.origin != origin {
            return Err("This caller does not own the agent".into());
        }
        super::views::preflight_retire_agent(&tx, &agent)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(agent)
    }

    pub(crate) fn preflight_agent_retirement(
        &mut self,
        source: &HostedSession,
    ) -> Result<(), String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        let row: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT a.record,p.source FROM agents a LEFT JOIN agent_launches p ON p.agent=a.id
             WHERE (a.session_id=?1 OR p.session_id=?1) AND a.closed=0",
                [source.id().to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some((record, pending)) = row {
            let agent: Agent = decode(&record)?;
            let pending = pending
                .map(|record| decode::<HostedSession>(&record))
                .transpose()?;
            if agent
                .session
                .as_ref()
                .or(pending.as_ref())
                .is_some_and(|expected| expected.same_process(source))
            {
                super::views::preflight_retire_agent(&tx, &agent)?;
            }
        }
        tx.commit().map_err(|error| error.to_string())
    }

    /// Until exact live prompt evidence can be reconciled, restart records an
    /// uncertain outcome rather than authorizing another delivery of effects.
    pub(crate) fn interrupt_unproven_agent_runs(&mut self, now_ms: u64) -> Result<(), String> {
        self.interrupt_unproven_agent_runs_except(now_ms, &[])
    }

    /// Only authenticated retained helpers supply these exact run/session pairs.
    /// A stale descriptor, a busy flag or an unrelated run ID is not ownership.
    pub(in crate::host) fn interrupt_unproven_agent_runs_except(
        &mut self,
        now_ms: u64,
        retained: &[(RunId, HostedSession)],
    ) -> Result<(), String> {
        let records = self
            .db
            .prepare(
                "SELECT record FROM agent_runs WHERE state IN ('accepted', 'running', 'waiting')",
            )
            .map_err(|error| error.to_string())?
            .query_map([], |row| row.get::<_, String>(0))
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
            .map_err(|error| error.to_string())?;
        for record in records {
            let run: Run = decode(&record)?;
            let agent = self.agent(run.agent)?;
            if !agent.closed && retained.iter().any(|(id, source)| {
                *id == run.id && agent.session.as_ref() == Some(source)
                    && matches!(source, HostedSession::Chat(chat) if
                        matches!(chat.native_continuity, crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 })
                        && chat.has_valid_native_continuity())
            }) {
                continue;
            }
            self.update_agent_run(run.id, run.revision, Update::Finished {
                state: State::Interrupted,
                result: ResultSummary {
                    diagnostic: Some("The daemon restarted without proof of this prompt's final outcome. Its effects may have occurred; inspect the conversation before sending more work.".into()),
                    ..Default::default()
                },
            }, now_ms)?;
        }
        Ok(())
    }

    /// Advance only the exact observed revision. Duplicate provider outcomes
    /// and an older turn's delayed completion cannot overwrite a newer result.
    ///
    /// Every accepted update is a run-state transition (streamed output never
    /// reaches the registry), so each one advances the model sequence in the
    /// same transaction: the `AgentsChanged` edge announcing it is then a real
    /// edge that watchers order against, not a repeat of an older sequence.
    pub(crate) fn update_agent_run(
        &mut self,
        id: RunId,
        revision: u64,
        update: Update,
        now_ms: u64,
    ) -> Result<Run, String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let run = update_agent_run_in(&tx, id, revision, update, now_ms)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(run)
    }
}

/// A retained actor's semantic cursor and run transition share this transaction.
pub(super) fn update_agent_run_in(
    tx: &Transaction<'_>,
    id: RunId,
    revision: u64,
    update: Update,
    now_ms: u64,
) -> Result<Run, String> {
    let record: String = tx
        .query_row(
            "SELECT record FROM agent_runs WHERE id = ?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let mut run: Run = decode(&record)?;
    if run.revision != revision || run.state.is_terminal() {
        return Err("The agent run has already advanced or finished".into());
    }
    match update {
        Update::Started { provider_key } => {
            if run.state != State::Accepted || provider_key.is_empty() || provider_key.len() > 1024
            {
                return Err("The provider start does not match an accepted run".into());
            }
            run.provider_key = Some(provider_key);
            run.started_ms = Some(now_ms);
            run.state = State::Running;
        }
        Update::Waiting if run.state == State::Running => run.state = State::Waiting,
        Update::Resumed if run.state == State::Waiting => run.state = State::Running,
        Update::Finished { state, mut result } if state.is_terminal() => {
            if state == State::Succeeded && run.provider_key.is_none() {
                return Err("An unconfirmed provider turn cannot succeed".into());
            }
            result.bound();
            run.result = result;
            run.state = state;
            run.finished_ms = Some(now_ms);
        }
        _ => return Err("The provider update does not match the agent run state".into()),
    }
    run.revision = run
        .revision
        .checked_add(1)
        .ok_or("Agent run revision exhausted")?;
    tx.execute(
        "UPDATE agent_runs SET state = ?2, revision = ?3, record = ?4 WHERE id = ?1",
        params![
            id.to_string(),
            state_key(run.state),
            run.revision,
            encode(&run)?
        ],
    )
    .map_err(|error| error.to_string())?;
    super::automation_attention::stage(tx, &run)?;
    tx.execute(
        "UPDATE metadata SET sequence = sequence + 1 WHERE key = 0",
        [],
    )
    .map_err(|error| error.to_string())?;
    Ok(run)
}

/// Moving a pane changes presentation membership, not execution authority.
/// Compare every other field, including the exact session, launch and owner.
pub(super) fn same_dispatch_agent(current: &Agent, accepted: &Agent) -> bool {
    let mut expected = accepted.clone();
    expected.placement = current.placement.clone();
    current == &expected
}

fn state_key(state: State) -> &'static str {
    match state {
        State::Accepted => "accepted",
        State::Running => "running",
        State::Waiting => "waiting",
        State::Succeeded => "succeeded",
        State::Failed => "failed",
        State::Cancelled => "cancelled",
        State::Interrupted => "interrupted",
    }
}

fn check_parent_depth(db: &Connection, origin: &Origin) -> Result<(), String> {
    let mut current = origin.clone();
    let mut seen = std::collections::BTreeSet::new();
    while let Origin::Child { parent } = current {
        if !seen.insert(parent) || seen.len() > MAX_PARENT_DEPTH {
            return Err("Agent parent depth limit reached".into());
        }
        // The parent is an agent if any session of its lineage hosts one.
        let previous: Option<String> = db
            .query_row(
                "SELECT origin FROM agents WHERE session_id = ?1 OR session_id IN
                 (SELECT session_id FROM session_lineages WHERE lineage = ?1) LIMIT 1",
                [parent.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some(previous) = previous else { break };
        current = decode(&previous)?;
    }
    Ok(())
}

fn read_agent(db: &Connection, id: AgentId) -> Result<Agent, String> {
    let record: String = db
        .query_row(
            "SELECT record FROM agents WHERE id = ?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    decode(&record)
}

pub(super) fn require_placement(
    db: &Connection,
    placement: &Placement,
    repository: &std::path::Path,
) -> Result<(), String> {
    match placement {
        Placement::Channel(channel) => require_channel(db, *channel, repository),
        Placement::Worktree {
            repository: owner,
            path,
        } => {
            if owner != repository || !path.is_absolute() {
                return Err(
                    "The worktree view belongs to another project or has an invalid path".into(),
                );
            }
            let scope = encode(placement)?;
            let exists: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM shared_views JOIN shared_view_scopes USING(scope) WHERE scope=?1 AND repository=?2)",
                params![scope, repository_key(repository)?], |row| row.get(0),
            ).map_err(|error|error.to_string())?;
            if !exists {
                return Err("The target worktree view is no longer open".into());
            }
            Ok(())
        }
    }
}

fn require_channel(
    db: &Connection,
    channel: ChannelId,
    repository: &std::path::Path,
) -> Result<(), String> {
    let owner: Option<String> = db
        .query_row(
            "SELECT repository FROM channels WHERE id = ?1",
            [channel.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if owner.as_deref() != Some(repository_key(repository)?) {
        return Err("The channel is absent or belongs to another project".into());
    }
    Ok(())
}

pub(super) fn encode<T: serde::Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, String> {
    serde_json::from_str(value)
        .map_err(|error| format!("Stored agent execution is unreadable: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Spec, Surface, Target},
        model::ProjectKind,
        workspace_model::ModelEdit,
    };

    fn source() -> HostedSession {
        HostedSession::Chat(crate::chat::Session {
            native_continuity: Default::default(),
            id: crate::model::TerminalId::new(),
            launch: crate::chat::Launch {
                provider: Provider::Claude,
                recipe: crate::model::LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: "/fixture".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some(uuid::Uuid::new_v4().to_string()),
            },
            proof: Some(crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("host"),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: crate::model::HostIncarnation::new("old"),
            }),
            terminal: None,
            owned_codex_listener: None,
        })
    }

    #[test]
    fn startup_preserves_only_exact_helper_proven_run_and_session_pairs() {
        for mismatch in 0..6 {
            let (mut registry, _directory, request) = fixture();
            let accepted = registry.submit_agent(&Origin::User, &request, 1).unwrap();
            let mut source = source();
            let HostedSession::Chat(chat) = &mut source else {
                unreachable!()
            };
            chat.native_continuity =
                crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
            assert!(chat.has_valid_native_continuity());
            registry
                .bind_agent(accepted.agent.id, None, source.clone())
                .unwrap();
            let mut retained = source.clone();
            let HostedSession::Chat(chat) = &mut retained else {
                unreachable!()
            };
            let mut run = accepted.run.id;
            match mismatch {
                0 => {}
                1 => run = RunId::new(),
                2 => {
                    chat.proof.as_mut().unwrap().host_incarnation =
                        crate::model::HostIncarnation::new("stale")
                }
                3 => {
                    chat.launch
                        .recipe
                        .environment
                        .insert("CLAUDE_CONFIG_DIR".into(), "/other-account".into());
                }
                4 => chat.native_continuity = Default::default(),
                5 => chat.launch.session_id = Some(uuid::Uuid::new_v4().to_string()),
                _ => unreachable!(),
            }
            registry
                .interrupt_unproven_agent_runs_except(2, &[(run, retained)])
                .unwrap();
            assert_eq!(
                registry.agent_run(accepted.run.id).unwrap().state,
                if mismatch == 0 {
                    State::Accepted
                } else {
                    State::Interrupted
                },
                "retained mismatch case {mismatch}"
            );
            if mismatch == 0 {
                registry.interrupt_unproven_agent_runs(3).unwrap();
                assert_eq!(
                    registry.agent_run(accepted.run.id).unwrap().state,
                    State::Interrupted
                );
            }
        }
    }

    #[test]
    fn legacy_open_presentation_close_preflight_is_read_only_and_repeats_commit_predicate() {
        let (mut registry, _directory, accepted, pane) = console_pane_fixture();
        let scope =
            crate::workspace_model::views::Scope::Channel(accepted.agent.channel().unwrap());
        registry
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![ended_console(&pane)],
                closed: vec![],
            })
            .unwrap();
        let agent = registry.agent(accepted.agent.id).unwrap();
        assert!(!agent.closed);
        let before = registry.model().unwrap();
        let run = registry.agent_run(accepted.run.id).unwrap();
        assert!(
            registry
                .preflight_agent_close(&Origin::User, agent.id)
                .unwrap_err()
                .contains("historical generation proof")
        );
        assert!(
            registry
                .preflight_agent_retirement(agent.session.as_ref().unwrap())
                .unwrap_err()
                .contains("historical generation proof")
        );
        assert_eq!(registry.model().unwrap(), before);
        assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
        assert_eq!(registry.agent(agent.id).unwrap(), agent);
        registry
            .apply(&ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: registry.view_revision(&scope).unwrap(),
                operation: uuid::Uuid::new_v4(),
                change: crate::workspace_model::views::Change::Remove { id: pane.id },
            })
            .unwrap();
        assert_eq!(
            registry
                .preflight_agent_close(&Origin::User, agent.id)
                .unwrap(),
            agent
        );
        registry
            .preflight_agent_retirement(agent.session.as_ref().unwrap())
            .unwrap();
    }

    fn closed_pane_fixture() -> (
        Registry,
        tempfile::TempDir,
        Accepted,
        crate::workspace_model::views::Pane,
    ) {
        let (mut registry, directory, request) = fixture();
        let accepted = registry.submit_agent(&Origin::User, &request, 1).unwrap();
        let session = source();
        registry
            .bind_agent(accepted.agent.id, None, session.clone())
            .unwrap();
        let started = registry
            .update_agent_run(
                accepted.run.id,
                accepted.run.revision,
                Update::Started {
                    provider_key: "confirmed".into(),
                },
                2,
            )
            .unwrap();
        registry
            .update_agent_run(
                accepted.run.id,
                started.revision,
                Update::Finished {
                    state: State::Succeeded,
                    result: ResultSummary {
                        text: "Preserved review report".into(),
                        ..Default::default()
                    },
                },
                3,
            )
            .unwrap();
        let HostedSession::Chat(session) = session else {
            unreachable!()
        };
        let pane = crate::workspace_model::views::Pane {
            id: crate::model::CardId::from_uuid(accepted.agent.id.as_uuid()),
            title: "Review".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            kind: crate::model::CardKind::Chat(session),
            scope: None,
        };
        (registry, directory, accepted, pane)
    }

    fn console_pane_fixture() -> (
        Registry,
        tempfile::TempDir,
        Accepted,
        crate::workspace_model::views::Pane,
    ) {
        use crate::model::{TerminalContinuity, TerminalSession};
        let (mut registry, directory, accepted, mut pane) = closed_pane_fixture();
        let retained = registry.agent(accepted.agent.id).unwrap().session.unwrap();
        let HostedSession::Chat(chat) = &retained else {
            unreachable!()
        };
        let proof = chat.proof.clone().unwrap();
        let console = TerminalSession {
            id: chat.id,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: chat.launch.recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::Live {
                host_session_id: proof.host_session_id,
                boot_id: proof.boot_id,
                host_incarnation: proof.host_incarnation,
            },
        };
        registry
            .bind_agent_console(accepted.agent.id, &retained, console.clone())
            .unwrap();
        pane.kind = crate::model::CardKind::Terminal(console);
        (registry, directory, accepted, pane)
    }
    fn publish_console(
        registry: &mut Registry,
        scope: &crate::workspace_model::views::Scope,
        before: &crate::workspace_model::views::Pane,
        pane: &crate::workspace_model::views::Pane,
    ) -> Result<super::super::Applied, String> {
        registry.apply(&ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: registry.view_revision(scope)?,
            operation: uuid::Uuid::new_v4(),
            change: crate::workspace_model::views::Change::Update {
                before: before.clone(),
                pane: pane.clone(),
            },
        })
    }
    fn ended_console(
        pane: &crate::workspace_model::views::Pane,
    ) -> crate::workspace_model::views::Pane {
        let mut ended = pane.clone();
        let crate::model::CardKind::Terminal(session) = &mut ended.kind else {
            unreachable!()
        };
        session.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
            recipe: session.launch.clone(),
            last_exit: Some(crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 4,
            }),
        };
        ended
    }

    #[test]
    fn ended_console_provenance_survives_batched_and_late_hints_move_and_restart() {
        use crate::workspace_model::views::{Change, Scope};
        for process_retirement in [false, true] {
            let (mut registry, directory, accepted, pane) = console_pane_fixture();
            let scope = Scope::Channel(accepted.agent.channel().unwrap());
            registry
                .apply(&ModelEdit::SeedView {
                    scope: scope.clone(),
                    panes: vec![pane.clone()],
                    closed: vec![],
                })
                .unwrap();
            let target = Scope::Worktree {
                repository: directory.path().into(),
                path: directory.path().into(),
            };
            registry
                .apply(&ModelEdit::ChangeView {
                    scope: scope.clone(),
                    expected_revision: registry.view_revision(&scope).unwrap(),
                    operation: uuid::Uuid::new_v4(),
                    change: Change::Transfer {
                        id: pane.id,
                        destination: target.clone(),
                        before: None,
                    },
                })
                .unwrap();
            let mut ended = ended_console(&pane);
            let crate::model::CardKind::Terminal(session) = &mut ended.kind else {
                unreachable!()
            };
            session.resume_hint = Some("batched final provider session hint".into());
            session.transcript_path = Some(directory.path().join("console.log"));
            publish_console(&mut registry, &target, &pane, &ended).unwrap();
            let mut late = ended.clone();
            let crate::model::CardKind::Terminal(session) = &mut late.kind else {
                unreachable!()
            };
            session.resume_hint = Some("late final provider session hint".into());
            late.title = "Completed console".into();
            publish_console(&mut registry, &target, &ended, &late).unwrap();
            let run = registry.agent_run(accepted.run.id).unwrap();
            drop(registry);
            let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
            if process_retirement {
                let source = registry.agent(accepted.agent.id).unwrap().session.unwrap();
                assert!(registry.retire_agent_process(&source, 5).unwrap());
            } else {
                registry
                    .close_agent(&Origin::User, accepted.agent.id)
                    .unwrap();
            }
            let view = super::super::views::read(&registry.db, &target).unwrap();
            assert!(view.panes.is_empty());
            assert!(view.retired.contains(&pane.id));
            assert!(registry.agent(accepted.agent.id).unwrap().closed);
            assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
            let marker_count: u64 = registry
                .db
                .query_row(
                    "SELECT count(*) FROM shared_view_terminal_sources",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                marker_count, 0,
                "pane removal cascades exact generation annotation"
            );
            registry
                .close_agent(&Origin::User, accepted.agent.id)
                .unwrap();
        }
    }

    #[test]
    fn repeated_closed_agent_repairs_proven_console_but_refuses_unproven_historical_end() {
        use crate::workspace_model::views::Scope;
        for proven in [false, true] {
            let (mut registry, _directory, accepted, pane) = console_pane_fixture();
            registry
                .close_agent(&Origin::User, accepted.agent.id)
                .unwrap();
            let scope = Scope::Channel(accepted.agent.channel().unwrap());
            let ended = ended_console(&pane);
            registry
                .apply(&ModelEdit::SeedView {
                    scope: scope.clone(),
                    panes: vec![if proven { pane.clone() } else { ended.clone() }],
                    closed: vec![],
                })
                .unwrap();
            if proven {
                publish_console(&mut registry, &scope, &pane, &ended).unwrap();
            }
            if !proven {
                // Upgrade an older store containing only the already-ended
                // descriptor. Migration must not invent historical proof.
                let legacy =
                    super::super::automation_proposals::published_fixture_from_rows(&registry, 12);
                drop(registry);
                registry = Registry::at(&legacy).unwrap();
            }
            let before = registry.model().unwrap();
            let run = registry.agent_run(accepted.run.id).unwrap();
            let result = registry.close_agent(&Origin::User, accepted.agent.id);
            if proven {
                result.unwrap();
                assert!(
                    super::super::views::read(&registry.db, &scope)
                        .unwrap()
                        .panes
                        .is_empty()
                );
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .contains("historical generation proof is unavailable")
                );
                assert_eq!(
                    registry.model().unwrap(),
                    before,
                    "missing proof cannot report successful retirement or alter durable state"
                );
                assert_eq!(
                    super::super::views::read(&registry.db, &scope)
                        .unwrap()
                        .panes,
                    vec![ended]
                );
            }
            assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
        }
    }

    #[test]
    fn ended_console_successors_survive_old_close_and_stale_updates_are_refused() {
        use crate::workspace_model::views::Scope;
        for stale_before in [false, true] {
            let (mut registry, _directory, accepted, pane) = console_pane_fixture();
            let scope = Scope::Channel(accepted.agent.channel().unwrap());
            registry
                .apply(&ModelEdit::SeedView {
                    scope: scope.clone(),
                    panes: vec![pane.clone()],
                    closed: vec![],
                })
                .unwrap();
            let ended = ended_console(&pane);
            publish_console(&mut registry, &scope, &pane, &ended).unwrap();
            let mut successor = pane.clone();
            let crate::model::CardKind::Terminal(session) = &mut successor.kind else {
                unreachable!()
            };
            let crate::model::TerminalContinuity::Live {
                host_incarnation, ..
            } = &mut session.continuity
            else {
                unreachable!()
            };
            *host_incarnation = crate::model::HostIncarnation::new("successor-generation");
            publish_console(&mut registry, &scope, &ended, &successor).unwrap();
            let successor_end = ended_console(&successor);
            let before = registry.model().unwrap();
            let updated = publish_console(
                &mut registry,
                &scope,
                if stale_before { &pane } else { &successor },
                &successor_end,
            );
            let retained = if stale_before {
                assert!(
                    updated
                        .unwrap_err()
                        .contains(Registry::VIEW_SOURCE_CONFLICT)
                );
                assert_eq!(registry.model().unwrap(), before);
                successor
            } else {
                updated.unwrap();
                successor_end
            };
            // The old agent can close, but neither a proved successor nor a
            // refused stale update grants ownership of the successor's pane.
            assert!(
                registry
                    .close_agent(&Origin::User, accepted.agent.id)
                    .unwrap()
                    .closed
            );
            assert_eq!(
                super::super::views::read(&registry.db, &scope)
                    .unwrap()
                    .panes,
                vec![retained]
            );
        }
    }

    #[test]
    fn ended_console_annotation_and_presentation_roll_back_in_one_transaction() {
        use crate::workspace_model::views::Scope;
        let (mut registry, _directory, accepted, pane) = console_pane_fixture();
        let scope = Scope::Channel(accepted.agent.channel().unwrap());
        registry
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![pane.clone()],
                closed: vec![],
            })
            .unwrap();
        let before = registry.model().unwrap();
        registry.db.execute_batch("CREATE TRIGGER reject_ended_view BEFORE UPDATE ON shared_views BEGIN SELECT RAISE(FAIL,'fixture ended view failure'); END;").unwrap();
        assert!(publish_console(&mut registry, &scope, &pane, &ended_console(&pane)).is_err());
        assert_eq!(registry.model().unwrap(), before);
        let count: u64 = registry
            .db
            .query_row(
                "SELECT count(*) FROM shared_view_terminal_sources",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 0,
            "failed view publication cannot leave a provenance marker"
        );
    }

    #[test]
    fn close_retires_only_its_current_shared_pane_and_keeps_report_durably() {
        use crate::workspace_model::views::{Change, Scope};
        let (mut registry, directory, accepted, pane) = closed_pane_fixture();
        let source = Scope::Channel(accepted.agent.channel().unwrap());
        let target = Scope::Worktree {
            repository: directory.path().into(),
            path: directory.path().into(),
        };
        let mut other = pane.clone();
        other.id = crate::model::CardId::new();
        let crate::model::CardKind::Chat(session) = &mut other.kind else {
            unreachable!()
        };
        session.id = crate::model::TerminalId::new();
        for (scope, panes) in [
            (source.clone(), vec![pane.clone()]),
            (target.clone(), vec![other.clone()]),
        ] {
            registry
                .apply(&ModelEdit::SeedView {
                    scope,
                    panes,
                    closed: vec![],
                })
                .unwrap();
        }
        registry
            .apply(&ModelEdit::ChangeView {
                scope: source.clone(),
                expected_revision: registry.view_revision(&source).unwrap(),
                operation: uuid::Uuid::new_v4(),
                change: Change::Transfer {
                    id: pane.id,
                    destination: target.clone(),
                    before: None,
                },
            })
            .unwrap();
        let run = registry.agent_run(accepted.run.id).unwrap();
        registry
            .close_agent(&Origin::User, accepted.agent.id)
            .unwrap();
        let view = super::super::views::read(&registry.db, &target).unwrap();
        assert_eq!(view.panes, vec![other]);
        assert!(view.retired.contains(&pane.id));
        assert!(registry.agent(accepted.agent.id).unwrap().closed);
        assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
        let sequence = registry.execution_sequence().unwrap();
        registry
            .close_agent(&Origin::User, accepted.agent.id)
            .unwrap();
        assert_eq!(registry.execution_sequence().unwrap(), sequence);
        // An old window cannot republish the retired descriptor on reconnect.
        registry
            .apply(&ModelEdit::SeedView {
                scope: target.clone(),
                panes: vec![pane],
                closed: vec![],
            })
            .unwrap();
        drop(registry);
        let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(
            super::super::views::read(&registry.db, &target).unwrap(),
            view
        );
        assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
    }

    #[test]
    fn repeated_close_repairs_old_stale_panes_but_keeps_replacement_resources() {
        use crate::workspace_model::views::Scope;
        let (mut registry, _directory, accepted, pane) = closed_pane_fixture();
        let scope = Scope::Channel(accepted.agent.channel().unwrap());
        // Simulate the older close implementation, which left no pane tombstone.
        registry
            .close_agent(&Origin::User, accepted.agent.id)
            .unwrap();
        registry
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![pane.clone()],
                closed: vec![],
            })
            .unwrap();
        assert!(
            registry
                .close_agent(
                    &Origin::Child {
                        parent: Lineage(crate::model::TerminalId::new())
                    },
                    accepted.agent.id
                )
                .is_err()
        );
        registry
            .close_agent(&Origin::User, accepted.agent.id)
            .unwrap();
        assert!(
            super::super::views::read(&registry.db, &scope)
                .unwrap()
                .panes
                .is_empty()
        );

        for replacement in 0..3 {
            let (mut registry, _directory, accepted, mut pane) = closed_pane_fixture();
            let scope = Scope::Channel(accepted.agent.channel().unwrap());
            let crate::model::CardKind::Chat(session) = &mut pane.kind else {
                unreachable!()
            };
            match replacement {
                0 => session.id = crate::model::TerminalId::new(),
                1 => {
                    session.proof.as_mut().unwrap().host_incarnation =
                        crate::model::HostIncarnation::new("successor")
                }
                2 => session
                    .launch
                    .recipe
                    .arguments
                    .push("--different-owner".into()),
                _ => unreachable!(),
            }
            registry
                .apply(&ModelEdit::SeedView {
                    scope: scope.clone(),
                    panes: vec![pane.clone()],
                    closed: vec![],
                })
                .unwrap();
            registry
                .close_agent(&Origin::User, accepted.agent.id)
                .unwrap();
            assert_eq!(
                super::super::views::read(&registry.db, &scope)
                    .unwrap()
                    .panes,
                vec![pane]
            );
        }
    }

    #[test]
    fn pane_retirement_and_agent_close_roll_back_together() {
        use crate::workspace_model::views::Scope;
        let (mut registry, _directory, accepted, pane) = closed_pane_fixture();
        let scope = Scope::Channel(accepted.agent.channel().unwrap());
        registry
            .apply(&ModelEdit::SeedView {
                scope,
                panes: vec![pane],
                closed: vec![],
            })
            .unwrap();
        let before = registry.model().unwrap();
        registry.db.execute_batch("CREATE TEMP TRIGGER reject_agent_close BEFORE UPDATE OF closed ON agents BEGIN SELECT RAISE(FAIL, 'fixture close failure'); END;").unwrap();
        assert!(
            registry
                .close_agent(&Origin::User, accepted.agent.id)
                .unwrap_err()
                .contains("fixture close failure")
        );
        assert_eq!(registry.model().unwrap(), before);
    }

    #[test]
    fn process_retirement_and_automation_attention_commit_or_roll_back_together() {
        let (mut registry, _directory, request) = fixture();
        let Destination::New(spec) = request.destination else {
            unreachable!()
        };
        let definition = serde_json::from_value(serde_json::json!({
            "name":"Fixture", "prompt":"Fixture prompt", "agent":spec,
            "schedule":{"pattern":{"daily":{"hour":9,"minute":0}},"timezone":"UTC"},
            "session_mode":"fresh","precheck":null
        }))
        .unwrap();
        let automation = registry
            .create_automation(
                definition,
                100,
                crate::automations::DirectoryIdentity {
                    device: 1,
                    inode: 1,
                },
            )
            .unwrap();
        let accepted = registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: uuid::Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap()
            .accepted
            .unwrap();
        let retained = source();
        registry
            .bind_agent(accepted.agent.id, None, retained.clone())
            .unwrap();
        let started = registry
            .update_agent_run(
                accepted.run.id,
                accepted.run.revision,
                Update::Started {
                    provider_key: "fixture".into(),
                },
                300,
            )
            .unwrap();
        let waiting = registry
            .update_agent_run(started.id, started.revision, Update::Waiting, 400)
            .unwrap();
        let original = registry
            .pending_automation_attention(100)
            .unwrap()
            .remove(0);
        registry
            .db
            .execute_batch(
                "CREATE TEMP TRIGGER fail_attention BEFORE INSERT ON automation_run_attention
            BEGIN SELECT RAISE(ABORT, 'fixture attention staging failure'); END;",
            )
            .unwrap();
        assert!(registry.retire_agent_process(&retained, 500).is_err());
        assert_eq!(registry.agent_run(waiting.id).unwrap(), waiting);
        assert!(!registry.agent(accepted.agent.id).unwrap().closed);
        let pending = registry.pending_automation_attention(100).unwrap();
        assert_eq!(pending[0].receipt, original.receipt);
        assert_eq!(
            pending[0].event, original.event,
            "failed replacement retains the previous edge"
        );
        registry
            .db
            .execute_batch("DROP TRIGGER fail_attention")
            .unwrap();
        assert!(registry.retire_agent_process(&retained, 500).unwrap());
        assert!(registry.agent(accepted.agent.id).unwrap().closed);
        let cancelled = registry.agent_run(waiting.id).unwrap();
        assert_eq!(cancelled.state, State::Cancelled);
        let pending = registry.pending_automation_attention(100).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].receipt.run, waiting.id);
        assert_eq!(pending[0].receipt.revision, cancelled.revision);
        assert_eq!(
            pending[0].event.evidence.kind,
            crate::notifications::Kind::Cancelled
        );
        assert_eq!(pending[0].event.source_session, Some(retained.id()));
        assert!(!registry.ack_automation_attention(original.receipt).unwrap());
    }

    #[test]
    fn process_retirement_commits_closed_agent_and_moved_pane_together() {
        use crate::workspace_model::views::{Change, Scope};
        for reject_close in [false, true] {
            let (mut registry, directory, accepted, pane) = closed_pane_fixture();
            let initial = Scope::Channel(accepted.agent.channel().unwrap());
            let moved = Scope::Worktree {
                repository: directory.path().into(),
                path: directory.path().into(),
            };
            registry
                .apply(&ModelEdit::SeedView {
                    scope: initial.clone(),
                    panes: vec![pane.clone()],
                    closed: vec![],
                })
                .unwrap();
            registry
                .apply(&ModelEdit::ChangeView {
                    scope: initial.clone(),
                    expected_revision: registry.view_revision(&initial).unwrap(),
                    operation: uuid::Uuid::new_v4(),
                    change: Change::Transfer {
                        id: pane.id,
                        destination: moved.clone(),
                        before: None,
                    },
                })
                .unwrap();
            let before = registry.model().unwrap();
            let run = registry.agent_run(accepted.run.id).unwrap();
            let retained = registry.agent(accepted.agent.id).unwrap().session.unwrap();
            if reject_close {
                registry.db.execute_batch("CREATE TEMP TRIGGER reject_process_retirement BEFORE UPDATE OF closed ON agents BEGIN SELECT RAISE(FAIL, 'fixture close failure'); END;").unwrap();
                assert!(
                    registry
                        .retire_agent_process(&retained, 4)
                        .unwrap_err()
                        .contains("fixture close failure")
                );
                assert_eq!(registry.model().unwrap(), before);
            } else {
                assert!(registry.retire_agent_process(&retained, 4).unwrap());
                let view = super::super::views::read(&registry.db, &moved).unwrap();
                assert!(view.panes.is_empty());
                assert!(view.retired.contains(&pane.id));
                assert!(registry.agent(accepted.agent.id).unwrap().closed);
                assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
                drop(registry);
                let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
                assert_eq!(
                    super::super::views::read(&registry.db, &moved).unwrap(),
                    view
                );
                assert!(registry.agent(accepted.agent.id).unwrap().closed);
                assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
            }
        }
    }

    #[test]
    fn retirement_writer_admission_refuses_changed_header_or_layout_before_dml() {
        for process in [false, true] {
            for future in [false, true] {
                let (mut registry, _directory, accepted, pane) = console_pane_fixture();
                let scope = crate::workspace_model::views::Scope::Channel(
                    accepted.agent.channel().unwrap(),
                );
                registry
                    .apply(&ModelEdit::SeedView {
                        scope: scope.clone(),
                        panes: vec![pane.clone()],
                        closed: vec![],
                    })
                    .unwrap();
                publish_console(&mut registry, &scope, &pane, &ended_console(&pane)).unwrap();
                let agent = registry.agent(accepted.agent.id).unwrap();
                let source = agent.session.as_ref().unwrap();
                let authority = registry.register_agent_authority(source).unwrap();
                let before = registry.model().unwrap();
                let run = registry.agent_run(accepted.run.id).unwrap();
                let proof: (String, Option<String>) = registry
                    .db
                    .query_row(
                        "SELECT source, ended FROM shared_view_terminal_sources WHERE pane=?1",
                        [crate::model::CardId::from_uuid(agent.id.as_uuid()).to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap();
                let other = Connection::open(&registry.path).unwrap();
                if future {
                    other
                        .pragma_update(None, "user_version", super::super::SCHEMA_VERSION + 1)
                        .unwrap();
                } else {
                    other
                        .execute_batch(
                            "ALTER TABLE projects ADD COLUMN unknown_retirement_payload TEXT;",
                        )
                        .unwrap();
                }
                // total_changes also observes DML that a later error rolls back:
                // unchanged final rows alone would not prove admission came first.
                let changes = registry.db.total_changes();
                let result = if process {
                    registry.retire_agent_process(source, 4).map(drop)
                } else {
                    registry.close_agent(&Origin::User, agent.id).map(drop)
                };
                assert!(result.unwrap_err().contains(if future {
                    "newer than supported"
                } else {
                    "layout"
                }));
                assert_eq!(
                    registry.db.total_changes(),
                    changes,
                    "rejected retirement attempted DML"
                );
                assert!(registry.db.is_autocommit());
                assert_eq!(registry.model().unwrap(), before);
                assert_eq!(registry.agent(agent.id).unwrap(), agent);
                assert_eq!(registry.agent_run(run.id).unwrap(), run);
                assert_eq!(
                    registry.agent_authority_source(&authority).unwrap(),
                    Some(source.clone())
                );
                let after: (String, Option<String>) = registry
                    .db
                    .query_row(
                        "SELECT source, ended FROM shared_view_terminal_sources WHERE pane=?1",
                        [crate::model::CardId::from_uuid(agent.id.as_uuid()).to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap();
                assert_eq!(after, proof);
            }
        }
    }

    #[test]
    fn caller_filtered_agents_seek_the_existing_origin_index() {
        let (registry, _directory, _) = fixture();
        let plan = registry
            .db
            .prepare(&format!("EXPLAIN QUERY PLAN {AGENTS_FOR_ORIGIN}"))
            .unwrap()
            .query_map([encode(&Origin::User).unwrap()], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("; ");
        assert!(plan.contains("SEARCH"), "{plan}");
        assert!(plan.contains("agents_by_origin"), "{plan}");
    }

    #[test]
    fn agent_listings_preserve_origin_closed_order_limit_and_decode_errors() {
        let (mut registry, _directory, request) = fixture();
        let accepted = registry.submit_agent(&Origin::User, &request, 1).unwrap();
        let origin = Origin::Child {
            parent: Lineage(crate::model::TerminalId::new()),
        };
        let mut inserted = vec![accepted.agent.clone()];
        let tx = registry.db.transaction().unwrap();
        // Exercise the listing contract without starting thousands of providers
        // or weakening execution admission/capacity limits.
        for index in 0..2300 {
            let mut agent = accepted.agent.clone();
            agent.id = AgentId::new();
            agent.name = None;
            agent.origin = if index % 2 == 0 {
                origin.clone()
            } else {
                Origin::User
            };
            agent.closed = index % 11 == 0;
            tx.execute(
                "INSERT INTO agents(id,origin,record,closed) VALUES(?1,?2,?3,?4)",
                params![
                    agent.id.to_string(),
                    encode(&agent.origin).unwrap(),
                    encode(&agent).unwrap(),
                    agent.closed
                ],
            )
            .unwrap();
            inserted.push(agent);
        }
        tx.commit().unwrap();
        let expected = |filter: Option<&Origin>| {
            inserted
                .iter()
                .rev()
                .filter(|agent| {
                    !agent.closed && filter.is_none_or(|origin| &agent.origin == origin)
                })
                .take(1024)
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(registry.agents(None).unwrap(), expected(None));
        assert_eq!(
            registry.agents(Some(&origin)).unwrap(),
            expected(Some(&origin))
        );
        assert_eq!(
            registry.agents(Some(&Origin::User)).unwrap(),
            expected(Some(&Origin::User))
        );
        assert_eq!(registry.agents(Some(&origin)).unwrap().len(), 1024);
        assert!(
            registry
                .agents(Some(&Origin::Child {
                    parent: Lineage(crate::model::TerminalId::new())
                }))
                .unwrap()
                .is_empty()
        );
        // A malformed record belonging to another caller must not poison this
        // caller's listing; either matching path must still report decode errors.
        registry
            .db
            .execute(
                "INSERT INTO agents(id,origin,record,closed) VALUES(?1,?2,'{}',0)",
                params![AgentId::new().to_string(), encode(&Origin::User).unwrap()],
            )
            .unwrap();
        assert_eq!(
            registry.agents(Some(&origin)).unwrap(),
            expected(Some(&origin))
        );
        assert!(registry.agents(Some(&Origin::User)).is_err());
        assert!(registry.agents(None).is_err());
    }

    #[test]
    fn automatic_cleanup_waits_for_accepted_dispatch_and_bound_ownership_retirement() {
        let (mut registry, _directory, request) = fixture();
        assert!(registry.cleanup_execution_is_quiescent().unwrap());
        let accepted = registry.submit_agent(&Origin::User, &request, 1).unwrap();
        assert!(!registry.cleanup_execution_is_quiescent().unwrap());
        registry.claim_agent_dispatch(&accepted, 2).unwrap();
        let session = source();
        registry
            .reserve_agent_launch(accepted.agent.id, &session)
            .unwrap();
        assert!(!registry.cleanup_execution_is_quiescent().unwrap());
        registry
            .bind_agent(accepted.agent.id, None, session)
            .unwrap();
        let started = registry
            .update_agent_run(
                accepted.run.id,
                accepted.run.revision,
                Update::Started {
                    provider_key: "confirmed".into(),
                },
                3,
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
                4,
            )
            .unwrap();
        // A completed turn is not proof that its provider process has ended.
        assert!(!registry.cleanup_execution_is_quiescent().unwrap());
        registry
            .close_agent(&Origin::User, accepted.agent.id)
            .unwrap();
        assert!(registry.cleanup_execution_is_quiescent().unwrap());
    }

    #[test]
    fn update_blockers_include_accepted_dispatch_and_unbound_provider_work() {
        let (mut registry, _directory, mut request) = fixture();
        let accepted = registry.submit_agent(&Origin::User, &request, 1).unwrap();
        let assert_blocked = |registry: &Registry| {
            let blockers = registry.agent_update_blockers().unwrap();
            assert_eq!(blockers.len(), 1);
            assert!(blockers[0].contains(&accepted.agent.id.to_string()));
        };
        assert_blocked(&registry);
        registry.claim_agent_dispatch(&accepted, 2).unwrap();
        assert_blocked(&registry);
        let session = source();
        registry
            .reserve_agent_launch(accepted.agent.id, &session)
            .unwrap();
        assert_blocked(&registry);
        registry
            .bind_agent(accepted.agent.id, None, session)
            .unwrap();
        assert_blocked(&registry);
        registry
            .update_agent_run(
                accepted.run.id,
                accepted.run.revision,
                Update::Started {
                    provider_key: "confirmed".into(),
                },
                3,
            )
            .unwrap();
        assert!(registry.agent_update_blockers().unwrap().is_empty());

        request.idempotency_key = "unbound-provider".into();
        let unbound = registry.submit_agent(&Origin::User, &request, 4).unwrap();
        registry
            .update_agent_run(
                unbound.run.id,
                unbound.run.revision,
                Update::Started {
                    provider_key: "unbound".into(),
                },
                5,
            )
            .unwrap();
        let blockers = registry.agent_update_blockers().unwrap();
        assert_eq!(blockers.len(), 1);
        assert!(blockers[0].contains(&unbound.agent.id.to_string()));
    }

    #[test]
    fn every_recorded_run_transition_advances_the_model_sequence() {
        let (mut registry, _directory, request) = fixture();
        let accepted = registry.submit_agent(&Origin::User, &request, 1).unwrap();
        let mut sequence = registry.execution_sequence().unwrap();
        let mut revision = accepted.run.revision;
        for update in [
            Update::Started {
                provider_key: "provider".into(),
            },
            Update::Waiting,
            Update::Resumed,
            Update::Finished {
                state: State::Succeeded,
                result: Default::default(),
            },
        ] {
            revision = registry
                .update_agent_run(accepted.run.id, revision, update, 2)
                .unwrap()
                .revision;
            let next = registry.execution_sequence().unwrap();
            assert_eq!(next, sequence + 1, "each transition is one model edge");
            sequence = next;
        }
        // A rejected (stale or settled) update records nothing and so must not
        // produce an edge that watchers would mistake for a transition.
        assert!(
            registry
                .update_agent_run(accepted.run.id, revision, Update::Waiting, 3)
                .is_err()
        );
        assert_eq!(registry.execution_sequence().unwrap(), sequence);
    }

    #[test]
    fn resident_release_retires_only_its_own_completed_writer_capability() {
        let (mut registry, _directory, _) = fixture();
        let source = source();
        let token = registry.register_agent_authority(&source).unwrap();
        assert!(
            registry
                .inspect_resident_release(&source, ResidentRelease::ReadOnlyHistory)
                .is_err()
        );
        registry
            .inspect_resident_release(&source, ResidentRelease::CompletedWriter)
            .unwrap();
        assert!(
            registry.execution_origin(&token).is_ok(),
            "inspection cannot revoke a capability"
        );
        let mut confirmed = source.clone();
        let HostedSession::Chat(session) = &mut confirmed else {
            unreachable!()
        };
        session.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        registry
            .retire_resident_release(&confirmed, ResidentRelease::CompletedWriter)
            .unwrap();
        assert!(registry.execution_origin(&token).is_err());
        assert!(
            registry
                .retained_agent_authority_source(source.id())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resident_release_preserves_successor_capability() {
        let (mut registry, _directory, _) = fixture();
        let source = source();
        let mut successor = source.clone();
        let HostedSession::Chat(session) = &mut successor else {
            unreachable!()
        };
        session.proof.as_mut().unwrap().host_incarnation =
            crate::model::HostIncarnation::new("successor");
        let token = registry.fresh_agent_authority(&successor).unwrap();
        assert!(
            registry
                .retire_resident_release(&source, ResidentRelease::CompletedWriter)
                .is_err()
        );
        assert!(registry.execution_origin(&token).is_ok());
        assert_eq!(
            registry
                .retained_agent_authority_source(source.id())
                .unwrap(),
            Some(successor)
        );
    }

    #[test]
    fn resident_release_preserves_managed_pending_and_delegated_work() {
        for (delegated, bound) in [(false, false), (false, true), (true, false)] {
            let (mut registry, _directory, request) = fixture();
            let source = source();
            let token = registry.register_agent_authority(&source).unwrap();
            let origin = if delegated {
                registry.execution_origin(&token).unwrap()
            } else {
                Origin::User
            };
            let accepted = registry.submit_agent(&origin, &request, 1).unwrap();
            if !delegated {
                registry
                    .reserve_agent_launch(accepted.agent.id, &source)
                    .unwrap();
                if bound {
                    registry
                        .bind_agent(accepted.agent.id, None, source.clone())
                        .unwrap();
                }
            }
            assert!(
                registry
                    .retire_resident_release(&source, ResidentRelease::CompletedWriter)
                    .is_err()
            );
            assert!(registry.execution_origin(&token).is_ok());
            assert_eq!(
                registry.latest_agent_run(accepted.agent.id).unwrap().state,
                State::Accepted
            );
        }
    }

    #[test]
    fn authority_retirement_and_proved_successor_are_exact_source_fenced() {
        let (mut registry, _directory, _) = fixture();
        let source = source();
        let authority = registry.register_agent_authority(&source).unwrap();
        let mut successor = source.clone();
        let HostedSession::Chat(next) = &mut successor else {
            unreachable!()
        };
        next.proof.as_mut().unwrap().host_incarnation = crate::model::HostIncarnation::new("next");
        assert!(registry.register_agent_authority(&successor).is_err());
        let mut unrelated = successor.clone();
        let HostedSession::Chat(other) = &mut unrelated else {
            unreachable!()
        };
        other.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(registry.adopt_agent_authority(&source, &unrelated).is_err());
        assert!(registry.adopt_agent_authority(&source, &successor).unwrap());
        assert!(!registry.adopt_agent_authority(&source, &successor).unwrap());
        assert_eq!(
            registry.register_agent_authority(&successor).unwrap(),
            authority
        );
        registry.retire_agent_authority(&source).unwrap();
        assert_eq!(
            registry.agent_authority_source(&authority).unwrap(),
            Some(successor.clone())
        );
        registry.retire_agent_authority(&successor).unwrap();
        assert!(registry.execution_origin(&authority).is_err());
        assert!(registry.agent_authority_source(&authority).is_err());
    }

    #[test]
    fn authenticated_adoption_keeps_pending_launch_private_and_advances_bound_identity_atomically()
    {
        let (mut registry, _directory, request) = fixture();
        let accepted = registry.submit_agent(&Origin::User, &request, 1).unwrap();
        let source = source();
        registry
            .reserve_agent_launch(accepted.agent.id, &source)
            .unwrap();
        let token = registry.register_agent_authority(&source).unwrap();
        let mut next = source.clone();
        let HostedSession::Chat(session) = &mut next else {
            unreachable!()
        };
        session.proof.as_mut().unwrap().host_incarnation =
            crate::model::HostIncarnation::new("next");
        assert!(
            !registry
                .adopt_authenticated_agent_session(std::slice::from_ref(&source), &next)
                .unwrap()
        );
        assert!(
            registry.agent(accepted.agent.id).unwrap().session.is_none(),
            "pending launch is never published by adoption"
        );
        assert_eq!(
            registry.pending_agent_launch(accepted.agent.id).unwrap(),
            Some(next.clone())
        );
        assert_eq!(
            registry.agent_authority_source(&token).unwrap(),
            Some(next.clone())
        );
        registry
            .bind_agent(accepted.agent.id, None, next.clone())
            .unwrap();
        let mut final_source = next.clone();
        let HostedSession::Chat(session) = &mut final_source else {
            unreachable!()
        };
        session.proof.as_mut().unwrap().host_incarnation =
            crate::model::HostIncarnation::new("final");
        registry.db.execute_batch("CREATE TRIGGER reject_adoption BEFORE UPDATE ON agents BEGIN SELECT RAISE(ABORT, 'fixture failed ownership commit'); END;").unwrap();
        assert!(
            registry
                .adopt_authenticated_agent_session(std::slice::from_ref(&next), &final_source)
                .is_err()
        );
        assert_eq!(
            registry.agent(accepted.agent.id).unwrap().session,
            Some(next.clone())
        );
        assert_eq!(
            registry.agent_authority_source(&token).unwrap(),
            Some(next.clone())
        );
        registry
            .db
            .execute_batch("DROP TRIGGER reject_adoption;")
            .unwrap();
        assert!(
            registry
                .adopt_authenticated_agent_session(std::slice::from_ref(&source), &final_source)
                .is_err(),
            "a stale unrelated source cannot rotate current authority"
        );
        assert!(
            registry
                .adopt_authenticated_agent_session(std::slice::from_ref(&next), &final_source)
                .unwrap()
        );
        assert_eq!(
            registry.agent(accepted.agent.id).unwrap().session,
            Some(final_source.clone())
        );
        assert_eq!(
            registry.agent_authority_source(&token).unwrap(),
            Some(final_source)
        );
        assert_eq!(
            registry.agent_run(accepted.run.id).unwrap(),
            accepted.run,
            "adoption never resends or settles the accepted prompt"
        );
    }

    #[test]
    fn approved_directory_identity_survives_reopen_and_refuses_replacement() {
        let (mut registry, directory, request) = fixture();
        let accepted = registry.submit_agent(&Origin::User, &request, 1).unwrap();
        let identity = crate::agent_execution::DirectoryIdentity::from_metadata(
            &std::fs::metadata(directory.path()).unwrap(),
        );
        registry
            .pin_agent_directory(accepted.agent.id, identity)
            .unwrap();
        let changed = crate::agent_execution::DirectoryIdentity {
            inode: identity.inode.wrapping_add(1),
            ..identity
        };
        assert!(
            registry
                .pin_agent_directory(accepted.agent.id, changed)
                .is_err()
        );
        drop(registry);
        let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(
            registry.agent_directory(accepted.agent.id).unwrap(),
            Some(identity)
        );
        let context = crate::chat::protocol::ExecutionContext::restored(identity);
        context.verify_directory(directory.path()).unwrap();
        let replacement = tempfile::tempdir().unwrap();
        assert!(context.verify_directory(replacement.path()).is_err());
    }

    fn fixture() -> (Registry, tempfile::TempDir, Submit) {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        let repository = directory.path().to_owned();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: repository.clone(),
                name: "Fixture".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let request = Submit {
            name: None,
            destination: Destination::New(Spec {
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
            }),
            prompt: "Read the fixture".into(),
            idempotency_key: "first-request".into(),
        };
        (registry, directory, request)
    }

    #[test]
    fn implicit_agent_channel_keeps_one_work_home_through_retry_and_project_close() {
        use crate::channel_work::{Home, HomeLifecycle};

        let (mut registry, directory, request) = fixture();
        let accepted = registry.submit_agent(&Origin::User, &request, 1).unwrap();
        let model = registry.model().unwrap();
        let channel = model.channel(accepted.agent.channel().unwrap()).unwrap();
        let home = Home {
            registry: model.identity,
            channel: channel.id,
            creation_ordinal: channel.ordinal,
        };
        assert_eq!(
            registry.read_channel_work_home(&home).unwrap().lifecycle,
            HomeLifecycle::Active
        );
        let retried = registry.submit_agent(&Origin::User, &request, 2).unwrap();
        assert!(!retried.created);
        assert_eq!(retried.agent.id, accepted.agent.id);
        assert_eq!(retried.run.id, accepted.run.id);
        assert_eq!(
            registry
                .db
                .query_row("SELECT count(*) FROM channel_work_homes", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            1
        );
        registry
            .apply(&ModelEdit::CloseProject {
                repository: directory.path().into(),
            })
            .unwrap();
        assert!(registry.model().unwrap().channels.is_empty());
        assert_eq!(
            registry.read_channel_work_home(&home).unwrap().lifecycle,
            HomeLifecycle::Deleted
        );
        drop(registry);
        let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(
            registry.read_channel_work_home(&home).unwrap().lifecycle,
            HomeLifecycle::Deleted,
            "reopening must retain the deleted incarnation without recreating its channel"
        );
        let retained: (String, String) = registry.db.query_row(
            "SELECT repository,title FROM channel_work_homes WHERE channel=?1 AND incarnation=?2",
            params![home.channel.to_string(), home.creation_ordinal],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(
            retained,
            (
                directory.path().to_str().unwrap().into(),
                channel.title.clone()
            )
        );
    }

    #[test]
    fn failed_implicit_work_home_creation_rolls_back_agent_channel_and_run() {
        let (mut registry, _directory, request) = fixture();
        let before = registry.model().unwrap();
        registry
            .db
            .execute_batch(
                "CREATE TEMP TRIGGER refuse_implicit_home BEFORE INSERT ON main.channel_work_homes
             BEGIN SELECT RAISE(ABORT, 'fixture work home failure'); END;",
            )
            .unwrap();
        let error = registry
            .submit_agent(&Origin::User, &request, 1)
            .unwrap_err();
        assert!(error.contains("fixture work home failure"));
        let after = registry.model().unwrap();
        assert_eq!(after.sequence, before.sequence);
        assert!(after.channels.is_empty() && after.agents.is_empty());
        for table in ["channel_work_homes", "agent_runs"] {
            let count: u64 = registry
                .db
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} must roll back with implicit creation");
        }
        registry
            .db
            .execute_batch("DROP TRIGGER temp.refuse_implicit_home;")
            .unwrap();
        assert!(
            registry
                .submit_agent(&Origin::User, &request, 2)
                .unwrap()
                .created
        );
    }

    #[test]
    fn worktree_placement_requires_the_exact_open_view_without_creating_a_channel() {
        let (mut registry, directory, mut request) = fixture();
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
        let Destination::New(spec) = &mut request.destination else {
            unreachable!()
        };
        spec.target.placement = Some(scope.clone());
        let accepted = registry.submit_agent(&Origin::User, &request, 1).unwrap();
        assert_eq!(accepted.agent.placement, scope);
        assert!(accepted.agent.channel().is_none());
        assert!(registry.model().unwrap().channels.is_empty());
        require_placement(&registry.db, &scope, directory.path()).unwrap();
        let foreign = tempfile::tempdir().unwrap();
        assert!(require_placement(&registry.db, &scope, foreign.path()).is_err());
        let missing = Placement::Worktree {
            repository: directory.path().into(),
            path: directory.path().join("missing-view"),
        };
        assert!(require_placement(&registry.db, &missing, directory.path()).is_err());
        registry
            .db
            .execute(
                "DELETE FROM shared_views WHERE scope=?1",
                [encode(&scope).unwrap()],
            )
            .unwrap();
        assert!(
            require_placement(&registry.db, &scope, directory.path()).is_err(),
            "a removed view is never resurrected by a follow-up"
        );
        let mut retry = request.clone();
        retry.idempotency_key = "missing-view".into();
        assert!(registry.submit_agent(&Origin::User, &retry, 2).is_err());
        assert_eq!(registry.model().unwrap().agents.len(), 1);
        assert!(registry.model().unwrap().channels.is_empty());
    }

    #[test]
    fn durable_legacy_channel_records_preserve_retry_identity_after_reopen() {
        let (mut registry, directory, request) = fixture();
        let accepted = registry.submit_agent(&Origin::User, &request, 1).unwrap();
        let mut old_agent = serde_json::to_value(&accepted.agent).unwrap();
        old_agent.as_object_mut().unwrap().remove("placement");
        old_agent["channel"] = serde_json::to_value(accepted.agent.channel().unwrap()).unwrap();
        old_agent["spec"]["target"]
            .as_object_mut()
            .unwrap()
            .remove("placement");
        old_agent["spec"]["target"]["channel"] = serde_json::Value::Null;
        let mut old_request = serde_json::to_value(&request).unwrap();
        let target = &mut old_request["destination"]["New"]["target"];
        target.as_object_mut().unwrap().remove("placement");
        target["channel"] = serde_json::Value::Null;
        registry
            .db
            .execute(
                "UPDATE agents SET record=?2 WHERE id=?1",
                params![accepted.agent.id.to_string(), old_agent.to_string()],
            )
            .unwrap();
        registry
            .db
            .execute(
                "UPDATE agent_runs SET request=?2 WHERE id=?1",
                params![accepted.run.id.to_string(), old_request.to_string()],
            )
            .unwrap();
        drop(registry);
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        let retried = registry.submit_agent(&Origin::User, &request, 2).unwrap();
        assert!(!retried.created);
        assert_eq!(retried.agent, accepted.agent);
        assert_eq!(retried.run.id, accepted.run.id);
        assert_eq!(registry.model().unwrap().channels.len(), 1);
    }

    #[test]
    fn legacy_registry_migrates_names_and_enforces_uniqueness_at_the_store() {
        let (mut registry, _directory, request) = fixture();
        let first = registry.submit_agent(&Origin::User, &request, 1).unwrap();
        let mut legacy = serde_json::to_value(&first.agent).unwrap();
        legacy.as_object_mut().unwrap().remove("name");
        registry
            .db
            .execute(
                "UPDATE agents SET record=?2 WHERE id=?1",
                params![first.agent.id.to_string(), legacy.to_string()],
            )
            .unwrap();
        let legacy = super::super::automation_proposals::published_fixture_from_rows(&registry, 9);
        drop(registry);
        let mut registry = Registry::at(&legacy).unwrap();
        assert_eq!(registry.agent(first.agent.id).unwrap().name, None);
        let mut named = request;
        named.name = Some("reviewer".into());
        named.idempotency_key = "named".into();
        let named = registry.submit_agent(&Origin::User, &named, 2).unwrap();
        let mut changed = first.agent;
        changed.name = named.agent.name;
        assert!(
            registry
                .db
                .execute(
                    "UPDATE agents SET record=?2 WHERE id=?1",
                    params![changed.id.to_string(), encode(&changed).unwrap()]
                )
                .is_err()
        );
        assert_eq!(registry.agent(changed.id).unwrap().name, None);
    }

    #[test]
    fn invalid_names_are_rejected_before_acceptance() {
        let (mut registry, _directory, mut request) = fixture();
        for name in [
            "",
            "Reviewer",
            "1reviewer",
            "two words",
            "équipe",
            "../reviewer",
            "abcdef00-0000-4000-8000-000000000001",
            &"x".repeat(65),
        ] {
            request.name = Some(name.into());
            assert!(
                registry.submit_agent(&Origin::User, &request, 1).is_err(),
                "{name:?}"
            );
        }
        assert!(registry.agents(None).unwrap().is_empty());
        request.name = Some("reviewer-2_api".into());
        assert!(registry.submit_agent(&Origin::User, &request, 2).is_ok());
    }

    #[test]
    fn names_are_durable_owner_scoped_retry_stable_and_reusable_after_close() {
        let (mut registry, directory, mut request) = fixture();
        request.name = Some("reviewer".into());
        let first = registry.submit_agent(&Origin::User, &request, 1).unwrap();
        assert_eq!(first.agent.name.as_deref(), Some("reviewer"));
        assert_eq!(
            registry
                .submit_agent(&Origin::User, &request, 2)
                .unwrap()
                .agent
                .id,
            first.agent.id
        );
        request.idempotency_key = "second".into();
        let before = registry.model().unwrap();
        assert!(
            registry
                .submit_agent(&Origin::User, &request, 3)
                .unwrap_err()
                .contains("named reviewer")
        );
        assert_eq!(
            registry.model().unwrap(),
            before,
            "name rejection reserves no channel or capacity"
        );
        let other = Origin::Automation {
            id: uuid::Uuid::new_v4(),
        };
        assert!(registry.submit_agent(&other, &request, 4).is_ok());
        drop(registry);
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(registry.agent(first.agent.id).unwrap().name, request.name);
        assert!(registry.submit_agent(&Origin::User, &request, 5).is_err());
        registry.interrupt_unproven_agent_runs(6).unwrap();
        registry.close_agent(&Origin::User, first.agent.id).unwrap();
        let replacement = registry.submit_agent(&Origin::User, &request, 7).unwrap();
        assert_ne!(replacement.agent.id, first.agent.id);
        assert_eq!(replacement.agent.name, request.name);
        let mut legacy = serde_json::to_value(&request).unwrap();
        legacy.as_object_mut().unwrap().remove("name");
        assert_eq!(serde_json::from_value::<Submit>(legacy).unwrap().name, None);
        let mut legacy = serde_json::to_value(&first.agent).unwrap();
        legacy.as_object_mut().unwrap().remove("name");
        assert_eq!(serde_json::from_value::<Agent>(legacy).unwrap().name, None);
    }

    #[test]
    fn failed_acceptance_rolls_back_its_fresh_channel_and_capacity_reservation() {
        let (mut registry, _directory, request) = fixture();
        let before = registry.model().unwrap();
        registry
            .db
            .execute_batch(
                "CREATE TRIGGER refuse_run BEFORE INSERT ON agent_runs
             BEGIN SELECT RAISE(ABORT, 'fixture write failure'); END;",
            )
            .unwrap();
        assert!(
            registry
                .submit_agent(&Origin::User, &request, 10)
                .unwrap_err()
                .contains("fixture write failure")
        );
        assert_eq!(registry.model().unwrap(), before);
        assert_eq!(
            registry
                .db
                .query_row("SELECT count(*) FROM agents", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
        registry
            .db
            .execute_batch("DROP TRIGGER refuse_run")
            .unwrap();
        assert!(
            registry
                .submit_agent(&Origin::User, &request, 20)
                .unwrap()
                .created
        );
    }

    #[test]
    fn future_registry_schema_is_refused_without_modifying_it() {
        let (registry, directory, _) = fixture();
        let future_version = super::super::SCHEMA_VERSION + 1;
        registry
            .db
            .pragma_update(None, "user_version", future_version)
            .unwrap();
        drop(registry);
        let path = directory.path().join("model.sqlite3");
        let error = Registry::at(&path)
            .err()
            .expect("future schema must be refused");
        assert!(error.contains("newer"));
        let db = Connection::open(path).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            future_version
        );
    }

    #[test]
    fn acceptance_is_durable_and_retries_do_not_create_another_channel_or_run() {
        let (mut registry, directory, mut request) = fixture();
        assert_eq!(
            registry
                .db
                .pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        let accepted = registry.submit_agent(&Origin::User, &request, 10).unwrap();
        assert!(accepted.created);
        assert_eq!(accepted.run.state, State::Accepted);
        drop(registry);
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(
            registry
                .db
                .pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        let retried = registry.submit_agent(&Origin::User, &request, 20).unwrap();
        assert!(!retried.created);
        assert_eq!(retried.agent, accepted.agent);
        assert_eq!(retried.run, accepted.run);
        assert_eq!(registry.model().unwrap().channels.len(), 1);
        request.prompt = "Different effects".into();
        assert!(
            registry
                .submit_agent(&Origin::User, &request, 30)
                .unwrap_err()
                .contains("different")
        );
        assert_eq!(registry.model().unwrap().channels.len(), 1);
    }

    #[test]
    fn committed_dispatch_claim_is_once_only_and_restart_never_reauthorizes_it() {
        let (mut registry, directory, request) = fixture();
        let accepted = registry.submit_agent(&Origin::User, &request, 10).unwrap();
        assert!(registry.claim_agent_dispatch(&accepted, 11).unwrap());
        assert!(!registry.claim_agent_dispatch(&accepted, 12).unwrap());
        drop(registry);
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert!(!registry.claim_agent_dispatch(&accepted, 13).unwrap());
        registry.interrupt_unproven_agent_runs(14).unwrap();
        assert_eq!(
            registry.agent_run(accepted.run.id).unwrap().state,
            State::Interrupted
        );
        assert!(registry.claim_agent_dispatch(&accepted, 15).is_err());
        let retry = registry.submit_agent(&Origin::User, &request, 16).unwrap();
        assert!(!retry.created);
        assert!(!registry.claim_agent_dispatch(&retry, 17).unwrap());
    }

    #[test]
    fn rejected_capacity_and_missing_channel_leave_no_partial_acceptance() {
        let (mut registry, _directory, mut request) = fixture();
        for index in 0..MAX_PARENT_RUNNING {
            request.idempotency_key = index.to_string();
            registry.submit_agent(&Origin::User, &request, 10).unwrap();
        }
        let model = registry.model().unwrap();
        request.idempotency_key = "over-limit".into();
        assert!(
            registry
                .submit_agent(&Origin::User, &request, 20)
                .unwrap_err()
                .contains("concurrency")
        );
        assert_eq!(registry.model().unwrap(), model);
        let Destination::New(spec) = &mut request.destination else {
            unreachable!()
        };
        spec.target.placement = Some(Placement::Channel(ChannelId::new()));
        assert!(
            registry
                .submit_agent(
                    &Origin::Automation {
                        id: uuid::Uuid::new_v4()
                    },
                    &request,
                    20
                )
                .unwrap_err()
                .contains("channel")
        );
        assert_eq!(registry.model().unwrap(), model);
    }

    fn finish_capacity_run(registry: &mut Registry, accepted: &Accepted) -> Run {
        let running = registry
            .update_agent_run(
                accepted.run.id,
                accepted.run.revision,
                Update::Started {
                    provider_key: format!("fixture-turn-{}", accepted.run.id),
                },
                20,
            )
            .unwrap();
        registry
            .update_agent_run(
                running.id,
                running.revision,
                Update::Finished {
                    state: State::Succeeded,
                    result: Default::default(),
                },
                30,
            )
            .unwrap()
    }

    fn capacity_child_origin(registry: &mut Registry) -> Origin {
        let authority = registry.register_agent_authority(&source()).unwrap();
        let origin = registry.execution_origin(&authority).unwrap();
        assert!(matches!(origin, Origin::Child { .. }));
        origin
    }

    #[test]
    fn user_retains_100_completed_conversations_with_names_ownership_and_retries() {
        let (mut registry, directory, mut request) = fixture();
        let origin = registry.execution_origin(&Authority::default()).unwrap();
        assert_eq!(origin, Origin::User);
        let mut completed = Vec::new();
        let mut first_request = None;
        for index in 0..100 {
            request.idempotency_key = format!("user-{index}");
            request.name = Some(format!("conversation-{index}"));
            let accepted = registry.submit_agent(&origin, &request, 10).unwrap();
            assert!(accepted.created);
            let finished = finish_capacity_run(&mut registry, &accepted);
            if index == 0 {
                first_request = Some(request.clone());
            }
            completed.push((accepted.agent, finished));
        }
        drop(registry);
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(registry.agents(Some(&origin)).unwrap().len(), 100);
        assert_eq!(registry.model().unwrap().channels.len(), 100);
        for (agent, run) in &completed {
            assert!(!agent.closed);
            assert_eq!(agent.origin, Origin::User);
            assert_eq!(registry.agent(agent.id).unwrap(), *agent);
            assert_eq!(registry.agent_run(run.id).unwrap(), *run);
            assert_eq!(run.state, State::Succeeded);
        }
        let first_request = first_request.unwrap();
        let retried = registry.submit_agent(&origin, &first_request, 40).unwrap();
        assert!(!retried.created);
        assert_eq!(retried.agent, completed[0].0);
        assert_eq!(retried.run, completed[0].1);
        let before = registry.model().unwrap();
        let mut changed = first_request.clone();
        changed.prompt = "Different work".into();
        assert!(
            registry
                .submit_agent(&origin, &changed, 41)
                .unwrap_err()
                .contains("idempotency")
        );
        let mut duplicate_name = first_request;
        duplicate_name.idempotency_key = "duplicate-name".into();
        assert!(
            registry
                .submit_agent(&origin, &duplicate_name, 42)
                .unwrap_err()
                .contains("named conversation-0")
        );
        let child = capacity_child_origin(&mut registry);
        let foreign = Submit {
            name: None,
            destination: Destination::Existing(completed[0].0.id),
            prompt: "Foreign caller".into(),
            idempotency_key: "foreign".into(),
        };
        assert!(
            registry
                .submit_agent(&child, &foreign, 43)
                .unwrap_err()
                .contains("does not own")
        );
        assert_eq!(registry.model().unwrap(), before);
        assert_eq!(registry.agents(Some(&origin)).unwrap().len(), 100);
    }

    #[test]
    fn child_and_automation_keep_64_open_conversation_limit_after_completion() {
        for automated in [false, true] {
            let (mut registry, _directory, mut request) = fixture();
            let origin = if automated {
                Origin::Automation {
                    id: uuid::Uuid::new_v4(),
                }
            } else {
                capacity_child_origin(&mut registry)
            };
            let mut last = None;
            for index in 0..MAX_CHILDREN {
                request.idempotency_key = format!("delegated-{index}");
                let accepted = registry.submit_agent(&origin, &request, 10).unwrap();
                finish_capacity_run(&mut registry, &accepted);
                last = Some(accepted.agent.id);
            }
            let retry = registry.submit_agent(&origin, &request, 40).unwrap();
            assert!(!retry.created);
            assert_eq!(Some(retry.agent.id), last);
            let before = registry.model().unwrap();
            request.idempotency_key = "delegated-over-limit".into();
            assert!(
                registry
                    .submit_agent(&origin, &request, 41)
                    .unwrap_err()
                    .contains("child limit")
            );
            assert_eq!(registry.agents(Some(&origin)).unwrap().len(), MAX_CHILDREN);
            assert_eq!(registry.model().unwrap(), before);
            assert!(
                registry
                    .agents(Some(&origin))
                    .unwrap()
                    .iter()
                    .all(|agent| !agent.closed)
            );
            // A full delegated scope does not consume the user's open quota.
            assert!(
                registry
                    .submit_agent(&Origin::User, &request, 42)
                    .unwrap()
                    .created
            );
        }
    }

    #[test]
    fn every_origin_keeps_eight_concurrent_or_uncertain_runs() {
        for kind in 0..3 {
            let (mut registry, _directory, mut request) = fixture();
            let origin = match kind {
                0 => registry.execution_origin(&Authority::default()).unwrap(),
                1 => capacity_child_origin(&mut registry),
                _ => Origin::Automation {
                    id: uuid::Uuid::new_v4(),
                },
            };
            for index in 0..MAX_PARENT_RUNNING {
                request.idempotency_key = format!("active-{index}");
                let accepted = registry.submit_agent(&origin, &request, 10).unwrap();
                match index % 4 {
                    0 => {} // Accepted work still has an owner.
                    1 | 2 => {
                        let run = registry
                            .update_agent_run(
                                accepted.run.id,
                                1,
                                Update::Started {
                                    provider_key: format!("turn-{index}"),
                                },
                                11,
                            )
                            .unwrap();
                        if index % 4 == 2 {
                            registry
                                .update_agent_run(run.id, run.revision, Update::Waiting, 12)
                                .unwrap();
                        }
                    }
                    _ => {
                        registry
                            .update_agent_run(
                                accepted.run.id,
                                1,
                                Update::Finished {
                                    state: State::Interrupted,
                                    result: Default::default(),
                                },
                                12,
                            )
                            .unwrap();
                    }
                }
            }
            assert!(
                !registry
                    .submit_agent(&origin, &request, 15)
                    .unwrap()
                    .created
            );
            let before = registry.model().unwrap();
            request.idempotency_key = "ninth".into();
            assert!(
                registry
                    .submit_agent(&origin, &request, 20)
                    .unwrap_err()
                    .contains("concurrency limit")
            );
            assert_eq!(registry.model().unwrap(), before);
            assert_eq!(
                registry.agents(Some(&origin)).unwrap().len(),
                MAX_PARENT_RUNNING
            );
        }
    }

    #[test]
    fn global_32_run_limit_spans_user_child_and_automation_origins() {
        let (mut registry, _directory, mut request) = fixture();
        let origins = [
            registry.execution_origin(&Authority::default()).unwrap(),
            capacity_child_origin(&mut registry),
            capacity_child_origin(&mut registry),
            Origin::Automation {
                id: uuid::Uuid::new_v4(),
            },
        ];
        let mut first = None;
        for (scope, origin) in origins.iter().enumerate() {
            for index in 0..MAX_PARENT_RUNNING {
                request.idempotency_key = format!("global-{scope}-{index}");
                let accepted = registry.submit_agent(origin, &request, 10).unwrap();
                if first.is_none() {
                    first = Some(accepted);
                }
            }
        }
        assert_eq!(registry.agents(None).unwrap().len(), MAX_RUNNING);
        let other = Origin::Automation {
            id: uuid::Uuid::new_v4(),
        };
        let before = registry.model().unwrap();
        request.idempotency_key = "global-over-limit".into();
        assert!(
            registry
                .submit_agent(&other, &request, 20)
                .unwrap_err()
                .contains("concurrency limit")
        );
        assert_eq!(registry.model().unwrap(), before);
        finish_capacity_run(&mut registry, &first.unwrap());
        assert!(registry.submit_agent(&other, &request, 40).unwrap().created);
        // Completed conversation remains open alongside the 32 owned runs.
        assert_eq!(registry.agents(None).unwrap().len(), MAX_RUNNING + 1);
    }

    #[test]
    fn competing_transactions_preserve_child_and_user_run_boundaries() {
        for child_limit in [true, false] {
            let (mut registry, _directory, mut request) = fixture();
            let origin = if child_limit {
                capacity_child_origin(&mut registry)
            } else {
                Origin::User
            };
            let count = if child_limit {
                MAX_CHILDREN - 1
            } else {
                MAX_PARENT_RUNNING - 1
            };
            for index in 0..count {
                request.idempotency_key = format!("before-race-{index}");
                let accepted = registry.submit_agent(&origin, &request, 10).unwrap();
                if child_limit {
                    finish_capacity_run(&mut registry, &accepted);
                }
            }
            let before = registry.model().unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let mut workers = Vec::new();
            for index in 0..2 {
                let mut connection = registry.execution_connection().unwrap();
                let mut request = request.clone();
                request.idempotency_key = format!("race-{index}");
                let origin = origin.clone();
                let barrier = barrier.clone();
                workers.push(std::thread::spawn(move || {
                    barrier.wait();
                    connection.submit_agent(&origin, &request, 40)
                }));
            }
            let results = workers
                .into_iter()
                .map(|worker| worker.join())
                .collect::<Vec<_>>();
            let results = results.into_iter().map(Result::unwrap).collect::<Vec<_>>();
            assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
            let rejected = results
                .iter()
                .find_map(|result| result.as_ref().err())
                .unwrap();
            assert!(rejected.contains(if child_limit {
                "child limit"
            } else {
                "concurrency limit"
            }));
            assert_eq!(registry.agents(Some(&origin)).unwrap().len(), count + 1);
            assert_eq!(
                registry.model().unwrap().channels.len(),
                before.channels.len() + 1
            );
        }
    }

    #[test]
    fn exact_turn_and_revision_keep_late_results_from_overwriting_another_run() {
        let (mut registry, _directory, request) = fixture();
        let accepted = registry.submit_agent(&Origin::User, &request, 10).unwrap();
        assert!(
            registry
                .update_agent_run(
                    accepted.run.id,
                    1,
                    Update::Finished {
                        state: State::Succeeded,
                        result: Default::default(),
                    },
                    20
                )
                .is_err()
        );
        let started = registry
            .update_agent_run(
                accepted.run.id,
                1,
                Update::Started {
                    provider_key: "provider-first".into(),
                },
                20,
            )
            .unwrap();
        let finished = registry
            .update_agent_run(
                started.id,
                started.revision,
                Update::Finished {
                    state: State::Succeeded,
                    result: Default::default(),
                },
                30,
            )
            .unwrap();
        let followup = Submit {
            name: None,
            destination: Destination::Existing(accepted.agent.id),
            prompt: "Next".into(),
            idempotency_key: "next".into(),
        };
        let next = registry.submit_agent(&Origin::User, &followup, 40).unwrap();
        assert!(
            registry
                .update_agent_run(
                    finished.id,
                    finished.revision,
                    Update::Finished {
                        state: State::Failed,
                        result: Default::default(),
                    },
                    50
                )
                .is_err()
        );
        assert_eq!(registry.agent_run(next.run.id).unwrap(), next.run);
        assert_eq!(registry.agent_run(finished.id).unwrap(), finished);
        assert!(
            registry
                .submit_agent(
                    &Origin::Automation {
                        id: uuid::Uuid::new_v4()
                    },
                    &followup,
                    60
                )
                .unwrap_err()
                .contains("own")
        );
    }
}
