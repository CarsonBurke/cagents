//! A closed historical execution can accompany an actually retired cleanup
//! actor. The pending intent itself still grants no history or input access.
use super::{NativeActorOwnership, NativeActorRetirement, Registry};
use crate::{
    agent_execution::{Agent, HostedSession, Run, State},
    chat::{NativeContinuity, Session},
    model::BootId,
};
use rusqlite::{OptionalExtension, params};

/// Constructed only by the exact durable history/retirement join below. The
/// caller must independently authenticate the private helper retirement and
/// current account; this witness is never provider execution authority.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::host) struct ArchivedHistory {
    agent: Agent,
    runs: Vec<(String, String, String)>,
    ownership: NativeActorOwnership,
    retirement: NativeActorRetirement,
    source: Session,
    boot: BootId,
}

impl ArchivedHistory {
    pub(in crate::host) fn matches(
        &self,
        source: &Session,
        ownership: &NativeActorOwnership,
        retirement: &NativeActorRetirement,
    ) -> bool {
        &self.source == source && &self.ownership == ownership && &self.retirement == retirement
    }
}

impl Registry {
    pub(in crate::host) fn archived_history(
        &self,
        source: &Session,
        ownership: &NativeActorOwnership,
        retirement: &NativeActorRetirement,
        boot: &BootId,
    ) -> Result<Option<ArchivedHistory>, String> {
        if !ownership.is_pending()
            || !retirement.is_pending()
            || ownership.actor() != retirement.actor()
            || ownership.owner() != retirement.owner()
            || &ownership.owner().boot != boot
            || source != retirement.source()
            || source.launch.provider != crate::agent::Provider::Claude
            || source.id != ownership.source().id
            || source.launch != ownership.source().launch
            || source.terminal.is_some()
            || source.owned_codex_listener.is_some()
            || ownership.source().terminal.is_some()
            || ownership.source().owned_codex_listener.is_some()
            || !matches!(
                source.native_continuity,
                NativeContinuity::RetainedClaudeActor { protocol: 2 }
            )
            || !source.has_valid_native_continuity()
            || !matches!(
                ownership.source().native_continuity,
                NativeContinuity::ReconnectOnly
                    | NativeContinuity::RetainedClaudeActor { protocol: 2 }
            )
            || ![source, ownership.source()].iter().all(|session| {
                session
                    .proof
                    .as_ref()
                    .is_some_and(|proof| proof.is_complete() && &proof.boot_id == boot)
            })
            || retirement.confirmed_session().is_none()
            || retirement.confirmed_session() != source.launch.session_id.as_deref()
            || ownership.scope().is_err()
            || ownership.scope() != retirement.scope()
        {
            return Ok(None);
        }
        // A single snapshot joins the relevant rows, rather than a global
        // sequence which unrelated agent activity could continually invalidate.
        let tx = self.db.unchecked_transaction().map_err(|e| e.to_string())?;
        if self.native_actor_ownership(ownership.actor())?.as_ref() != Some(ownership)
            || self.native_actor_retirement(ownership.actor())?.as_ref() != Some(retirement)
        {
            return Ok(None);
        }
        {
            let mut statement = tx
                .prepare("SELECT actor,ownership FROM native_actor_ownerships LIMIT 4097")
                .map_err(|e| e.to_string())?;
            let mut rows = statement.query([]).map_err(|e| e.to_string())?;
            let (mut count, mut bytes, mut owners) = (0, 0usize, 0);
            while let Some(row) = rows.next().map_err(|e| e.to_string())? {
                let key: String = row.get(0).map_err(|e| e.to_string())?;
                let encoded: String = row.get(1).map_err(|e| e.to_string())?;
                count += 1;
                bytes = bytes.saturating_add(encoded.len());
                if count > 4096 || encoded.len() > 256 * 1024 || bytes > 64 * 1024 * 1024 {
                    return Err("Archived history exceeds its ownership metadata bound".into());
                }
                let other: NativeActorOwnership = super::execution::decode(&encoded)?;
                if other.actor().is_nil() || key != other.actor().to_string() {
                    return Err("Archived history changed its actor census identity".into());
                }
                if &other.owner().boot != boot {
                    continue;
                }
                if other.source().id == source.id {
                    owners += 1;
                } else if other.source().launch.provider == source.launch.provider
                    && other.source().launch.session_id == source.launch.session_id
                    && other.scope().ok().zip(ownership.scope().ok()).is_some_and(
                        |(left, right)| {
                            left.canonical_home == right.canonical_home
                                && left.device == right.device
                                && left.inode == right.inode
                        },
                    )
                    && (other.is_pending()
                        || self.native_actor_retirement(other.actor())?.is_none())
                {
                    return Ok(None);
                }
            }
            if owners != 1 {
                return Ok(None);
            }
        }
        let record: Option<(String, String)> = tx
            .query_row(
                "SELECT id,record FROM agents WHERE session_id=?1 AND closed=1",
                [source.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some((key, record)) = record else {
            return Ok(None);
        };
        let agent: Agent = super::execution::decode(&record)?;
        let Some(HostedSession::Chat(historical)) = &agent.session else {
            return Ok(None);
        };
        if key != agent.id.to_string()
            || !agent.closed
            || agent.console.is_some()
            || historical.id != source.id
            || historical.launch.provider != source.launch.provider
            || historical.launch.recipe != source.launch.recipe
            || historical.launch.session_id.is_some()
            || historical.native_continuity != source.native_continuity
            || historical.terminal.is_some()
            || historical.owned_codex_listener.is_some()
            || !historical
                .proof
                .as_ref()
                .is_some_and(|proof| proof.is_complete() && &proof.boot_id != boot)
        {
            return Ok(None);
        }
        let blocked: bool = tx
            .query_row(
                "SELECT (SELECT count(*) FROM agents WHERE session_id=?1 AND closed=1) != 1
             OR EXISTS(SELECT 1 FROM agent_authorities WHERE session_id=?1)
             OR EXISTS(SELECT 1 FROM agent_launches WHERE session_id=?1 OR agent=?2)
             OR EXISTS(SELECT 1 FROM shared_view_panes WHERE id=?2)
             OR EXISTS(SELECT 1 FROM agents WHERE session_id=?1 AND closed=0)",
                params![source.id.to_string(), agent.id.to_string()],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if blocked
            || super::views::all(&tx)?.iter().any(|view| {
                view.panes.iter().any(|pane| {
                    matches!(&pane.kind,
                crate::model::CardKind::Chat(chat) if chat.id == source.id)
                })
            })
        {
            return Ok(None);
        }
        let runs = {
            let mut statement = tx
                .prepare(
                    "SELECT id,state,record FROM agent_runs WHERE agent=?1 ORDER BY id LIMIT 4097",
                )
                .map_err(|e| e.to_string())?;
            let mut rows = statement
                .query([agent.id.to_string()])
                .map_err(|e| e.to_string())?;
            let mut runs = Vec::new();
            let mut bytes = 0usize;
            while let Some(row) = rows.next().map_err(|e| e.to_string())? {
                let (id, state, record): (String, String, String) = (
                    row.get(0).map_err(|e| e.to_string())?,
                    row.get(1).map_err(|e| e.to_string())?,
                    row.get(2).map_err(|e| e.to_string())?,
                );
                bytes = bytes.saturating_add(record.len());
                if runs.len() >= 4096 || record.len() > 256 * 1024 || bytes > 64 * 1024 * 1024 {
                    return Err("Archived history exceeds its completed-run metadata bound".into());
                }
                let run: Run = super::execution::decode(&record)?;
                let terminal = match run.state {
                    State::Succeeded => "succeeded",
                    State::Failed => "failed",
                    State::Cancelled => "cancelled",
                    State::Interrupted => "interrupted",
                    _ => return Ok(None),
                };
                if run.id.to_string() != id || run.agent != agent.id || terminal != state {
                    return Err("Archived history changed its completed-run identity".into());
                }
                runs.push((id, state, record));
            }
            runs
        };
        if runs.is_empty() {
            return Ok(None);
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(Some(ArchivedHistory {
            agent,
            runs,
            ownership: ownership.clone(),
            retirement: retirement.clone(),
            source: source.clone(),
            boot: boot.clone(),
        }))
    }

    pub(in crate::host) fn recheck_archived_history(
        &self,
        witness: &ArchivedHistory,
    ) -> Result<(), String> {
        if self
            .archived_history(
                &witness.source,
                &witness.ownership,
                &witness.retirement,
                &witness.boot,
            )?
            .as_ref()
            != Some(witness)
        {
            return Err("Archived history changed during exact resume preparation".into());
        }
        Ok(())
    }
}
