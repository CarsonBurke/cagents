//! Execution acceptance shares the project/channel transaction boundary.

use super::{Registry, repository_key, take_ordinal};
use crate::{
    agent_execution::{
        Accepted, Agent, Authority, Destination, HostedSession, MAX_CHILDREN, MAX_PARENT_DEPTH,
        MAX_PARENT_RUNNING, MAX_RUNNING, Origin, ResultSummary, Run, State, Submit, Update,
    },
    model::{AgentId, ChannelId, RunId},
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    // Version zero is the existing unversioned project/channel registry.
    db.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE IF NOT EXISTS agents (
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
         ) STRICT;
         PRAGMA user_version = 1;
         COMMIT;",
    )
    .map_err(|error| error.to_string())
}

impl Registry {
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
        require_channel(&tx, agent.channel, &agent.spec.target.repository)?;
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

    /// Only the synchronous launch caller may clear this after proving no
    /// child was created. Restart recovery never calls this inference.
    pub(crate) fn clear_unspawned_agent_launch(
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
        if decode::<Run>(&stored)? != accepted.run
            || read_agent(&tx, accepted.agent.id)? != accepted.agent
        {
            return Err(
                "The accepted agent run changed before dispatch; nothing was submitted".into(),
            );
        }
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
        let session: String = self
            .db
            .query_row(
                "SELECT session_id FROM agent_authorities WHERE token = ?1",
                [token],
                |row| row.get(0),
            )
            .map_err(|_| "This agent session authority is no longer registered".to_owned())?;
        Ok(Origin::Child {
            parent: session
                .parse()
                .map_err(|_| "Stored agent session identity is invalid")?,
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
                require_channel(&tx, agent.channel, &agent.spec.target.repository)?;
                agent
            }
            Destination::New(spec) => {
                // Persist the resolved account for this accepted launch while
                // keeping the caller's original request as its retry key.
                let spec = selected.unwrap_or(spec);
                let children: usize = tx
                    .query_row(
                        "SELECT count(*) FROM agents WHERE origin = ?1 AND closed = 0",
                        [&origin_json],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                if children >= MAX_CHILDREN {
                    return Err("Agent child limit reached; close an existing child first".into());
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
                let channel = match spec.target.channel {
                    Some(channel) => {
                        require_channel(&tx, channel, &spec.target.repository)?;
                        channel
                    }
                    None => {
                        let channel = ChannelId::new();
                        let ordinal = take_ordinal(&tx, "next_channel_ordinal")?;
                        tx.execute(
                            "INSERT INTO channels(id, repository, title, ordinal) VALUES (?1, ?2, ?3, ?4)",
                            params![channel.to_string(), repository, format!("{} agent", spec.provider.label()), ordinal],
                        ).map_err(|error| error.to_string())?;
                        channel
                    }
                };
                let agent = Agent {
                    id: AgentId::new(),
                    origin: origin.clone(),
                    spec: spec.clone(),
                    channel,
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
        if agent.closed || run != accepted.run || agent != accepted.agent {
            return Err("The accepted execution changed before provider dispatch".into());
        }
        require_channel(&self.db, agent.channel, &agent.spec.target.repository)
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
        let origin = origin.map(encode).transpose()?;
        let mut statement = self
            .db
            .prepare(
                "SELECT record FROM agents WHERE closed = 0 AND (?1 IS NULL OR origin = ?1)
             ORDER BY rowid DESC LIMIT 1024",
            )
            .map_err(|error| error.to_string())?;
        let records = statement
            .query_map([origin], |row| row.get::<_, String>(0))
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
            .map_err(|error| error.to_string())?;
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
        require_channel(&tx, agent.channel, &agent.spec.target.repository)?;
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
        require_channel(&tx, agent.channel, &agent.spec.target.repository)?;
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
    /// the UI. The run, capacity and delegated authority change atomically.
    pub(crate) fn retire_agent_process(
        &mut self,
        source: &HostedSession,
        now_ms: u64,
    ) -> Result<bool, String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
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
                }
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

    /// Until exact live prompt evidence can be reconciled, restart records an
    /// uncertain outcome rather than authorizing another delivery of effects.
    pub(crate) fn interrupt_unproven_agent_runs(&mut self, now_ms: u64) -> Result<(), String> {
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
                if run.state != State::Accepted
                    || provider_key.is_empty()
                    || provider_key.len() > 1024
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
        tx.commit().map_err(|error| error.to_string())?;
        Ok(run)
    }
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
        let previous: Option<String> = db
            .query_row(
                "SELECT origin FROM agents WHERE session_id = ?1",
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

fn encode<T: serde::Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}

fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, String> {
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
            destination: Destination::New(Spec {
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
            }),
            prompt: "Read the fixture".into(),
            idempotency_key: "first-request".into(),
        };
        (registry, directory, request)
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
        registry.db.pragma_update(None, "user_version", 2).unwrap();
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
            2
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
        spec.target.channel = Some(ChannelId::new());
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
