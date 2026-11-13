//! A closed console view may be replaced without replacing its provider.
//! Persist the exact cut before unpairing the retained actor, so a new daemon
//! can finish an interrupted replacement without opening a second view.
use super::Registry;
use crate::{
    agent_execution::{Agent, HostedSession, Surface},
    chat,
    daemon::holder_registry::{Kind, Record},
    model::{AgentId, TerminalSession},
    terminal_ipc::TerminalSpawnOptions,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const LIMIT: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::host) struct ConsoleRelaunch {
    pub actor: Uuid,
    pub owner: Record,
    pub agent: AgentId,
    pub source: chat::Session,
    pub console: TerminalSession,
    pub closed_holder: Record,
    pub target: TerminalSession,
    pub options: TerminalSpawnOptions,
    pub aliases: Vec<TerminalSession>,
    pub replacement_holder: Option<Record>,
    pub closed_replacement: Option<Record>,
}

impl ConsoleRelaunch {
    fn validate(&self) -> Result<(), String> {
        let old = self.console.continuity.live_proof();
        let next = self.target.continuity.live_proof();
        let mut recipe = self.console.launch.clone();
        recipe.program = self.target.launch.program.clone();
        if self.actor.is_nil()
            || self.owner.kind != Kind::Native
            || self.closed_holder.kind != Kind::Terminal
            || self.owner.terminal != self.source.id
            || self.closed_holder.terminal != self.source.id
            || self.console.id != self.source.id
            || self.target.id != self.source.id
            || self.source.launch.provider != crate::agent::Provider::Claude
            || !matches!(
                self.source.native_continuity,
                chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
            )
            || crate::agent_execution::console_agent(&self.console.launch) != Some(self.agent)
            || recipe != self.target.launch
            || !std::path::Path::new(&self.target.launch.program).is_absolute()
            || !old.as_ref().is_some_and(|p| {
                p.is_complete()
                    && p.boot_id == self.owner.boot
                    && p.boot_id == self.closed_holder.boot
            })
            || !next
                .as_ref()
                .is_some_and(|p| p.is_complete() && p.boot_id == self.owner.boot)
            || self.aliases.len() > 1024
            || self.aliases.iter().any(|alias| {
                alias.id != self.source.id
                    || alias
                        .continuity
                        .live_proof()
                        .is_none_or(|p| !p.is_complete() || p.boot_id != self.owner.boot)
            })
            || self.replacement_holder.as_ref().is_some_and(|p| {
                p.kind != Kind::Terminal
                    || p.terminal != self.source.id
                    || p.boot != self.owner.boot
                    || p == &self.closed_holder
            })
            || self.closed_replacement.as_ref().is_some_and(|p| {
                p.kind != Kind::Terminal
                    || p.terminal != self.source.id
                    || p.boot != self.owner.boot
                    || p == &self.closed_holder
                    || self.replacement_holder.as_ref() == Some(p)
            })
        {
            return Err(
                "Console replacement changed its authenticated provider or view identity".into(),
            );
        }
        Ok(())
    }

    /// Only the live/detached projection differs from the preissued target.
    pub(in crate::host) fn recognizes_target(&self, terminal: &TerminalSession) -> bool {
        let mut projected = terminal.clone();
        projected.continuity = self.target.continuity.clone();
        projected == self.target
    }

    pub(in crate::host) fn recognizes_issued_target(&self, terminal: &TerminalSession) -> bool {
        self.recognizes_target(terminal)
            && terminal.continuity.live_proof() == self.target.continuity.live_proof()
    }
}

pub(super) fn migrate(tx: &Transaction<'_>) -> Result<(), String> {
    tx.execute_batch("CREATE TABLE console_relaunches(actor TEXT PRIMARY KEY REFERENCES native_actor_ownerships(actor), agent TEXT NOT NULL UNIQUE REFERENCES agents(id), record TEXT NOT NULL) STRICT;")
        .map_err(|e| e.to_string())
}

fn encoded(intent: &ConsoleRelaunch) -> Result<String, String> {
    intent.validate()?;
    let encoded = serde_json::to_string(intent).map_err(|e| e.to_string())?;
    if encoded.len() > LIMIT {
        return Err("Console replacement exceeds its metadata bound".into());
    }
    Ok(encoded)
}

pub(super) fn require_current(db: &Connection, intent: &ConsoleRelaunch) -> Result<(), String> {
    let current: Option<String> = db
        .query_row(
            "SELECT record FROM console_relaunches WHERE actor=?1",
            [intent.actor.to_string()],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if current.as_deref() != Some(encoded(intent)?.as_str()) {
        return Err("The exact console replacement intent changed".into());
    }
    Ok(())
}

impl Registry {
    /// A replacement which lost its startup daemon is still only a view.
    /// Caller authenticated its preissued descriptor and exact holder release.
    /// Record that cut before another holder can acquire the same terminal ID.
    pub(in crate::host) fn retry_console_relaunch(
        &mut self,
        before: &ConsoleRelaunch,
        closed: &Record,
        source: &TerminalSession,
        proof: &crate::model::HostSessionProof,
    ) -> Result<ConsoleRelaunch, String> {
        if !before.recognizes_issued_target(source)
            || before
                .replacement_holder
                .as_ref()
                .is_some_and(|owner| owner != closed)
            || !proof.is_complete()
            || proof.boot_id != before.owner.boot
        {
            return Err("Console retry lost its exact preissued worker release".into());
        }
        let mut next = before.clone();
        next.replacement_holder = None;
        next.closed_replacement = Some(closed.clone());
        next.target.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        if !next.aliases.contains(source) {
            next.aliases.push(source.clone());
        }
        let previous = encoded(before)?;
        let body = encoded(&next)?;
        if self
            .db
            .execute(
                "UPDATE console_relaunches SET record=?3 WHERE actor=?1 AND record=?2",
                params![before.actor.to_string(), previous, body],
            )
            .map_err(|e| e.to_string())?
            != 1
        {
            return Err("The console replacement changed before its retry cut".into());
        }
        Ok(next)
    }

    pub(in crate::host) fn has_console_relaunch(
        &self,
        session: crate::model::TerminalId,
    ) -> Result<bool, String> {
        self.db.query_row("SELECT EXISTS(SELECT 1 FROM console_relaunches WHERE json_extract(record,'$.source.id')=?1)", [session.to_string()], |row| row.get(0)).map_err(|e| e.to_string())
    }

    pub(in crate::host) fn console_relaunch(
        &self,
        actor: Uuid,
    ) -> Result<Option<ConsoleRelaunch>, String> {
        let raw: Option<String> = self
            .db
            .query_row(
                "SELECT record FROM console_relaunches WHERE actor=?1",
                [actor.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        raw.map(|raw| {
            if raw.len() > LIMIT {
                return Err("Stored console replacement exceeds its metadata bound".into());
            }
            let intent: ConsoleRelaunch = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            intent.validate()?;
            if intent.actor != actor {
                return Err("Console replacement key changed its actor".into());
            }
            Ok(intent)
        })
        .transpose()
    }

    /// The caller has already proved the original holder's exact close receipt.
    /// This row is written before the actor acknowledges removal of that pair.
    pub(in crate::host) fn begin_console_relaunch(
        &mut self,
        intent: &ConsoleRelaunch,
        retained: &[HostedSession],
    ) -> Result<(), String> {
        let body = encoded(intent)?;
        let ownership = self
            .native_actor_ownership(intent.actor)?
            .ok_or("The console's retained provider has no durable owner")?;
        if ownership.owner() != &intent.owner
            || !retained.contains(&HostedSession::Chat(ownership.source().clone()))
        {
            return Err("Console replacement lost its exact retained provider ownership".into());
        }
        let agent: Agent = self.agent(intent.agent)?;
        if agent.closed
            || agent.spec.surface != Surface::Terminal
            || agent.spec.provider != crate::agent::Provider::Claude
            || agent.console.as_ref() != Some(&intent.console)
            || !agent.session.as_ref().is_some_and(|s| retained.contains(s))
        {
            return Err("Console replacement does not own this agent's existing pane".into());
        }
        self.db
            .execute(
                "INSERT INTO console_relaunches(actor,agent,record) VALUES(?1,?2,?3)",
                params![intent.actor.to_string(), intent.agent.to_string(), body],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(in crate::host) fn update_console_relaunch(
        &mut self,
        before: &ConsoleRelaunch,
        after: &ConsoleRelaunch,
    ) -> Result<(), String> {
        let previous = encoded(before)?;
        let body = encoded(after)?;
        let mut expected = before.clone();
        expected.target.continuity = after.target.continuity.clone();
        expected.replacement_holder = after.replacement_holder.clone();
        if expected != *after
            || before
                .replacement_holder
                .as_ref()
                .is_some_and(|holder| after.replacement_holder.as_ref() != Some(holder))
        {
            return Err("Console replacement attempted to change its immutable intent".into());
        }
        if self
            .db
            .execute(
                "UPDATE console_relaunches SET record=?3 WHERE actor=?1 AND record=?2",
                params![before.actor.to_string(), previous, body],
            )
            .map_err(|e| e.to_string())?
            != 1
        {
            return Err("The console replacement changed before its next receipt".into());
        }
        Ok(())
    }
}
