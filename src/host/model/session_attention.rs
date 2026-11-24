//! Session owners that must be absent before a notice can be collected.
use std::collections::{BTreeMap, BTreeSet};

use super::{Registry, execution::decode, schema, views};
use crate::{
    agent_execution::{Agent, HostedSession, Lineage},
    chat::Session,
    model::{CardKind, TerminalId, TerminalSession},
    upgrade::handoff::{Chat, Checkpoint, Terminal},
};

#[derive(Default, PartialEq, Eq)]
pub(in crate::host) struct References(BTreeSet<TerminalId>);

#[derive(PartialEq, Eq)]
pub(in crate::host) struct ModelVersion(i64, u64);

impl References {
    pub(in crate::host) fn extend(&mut self, sessions: impl IntoIterator<Item = TerminalId>) {
        self.0.extend(sessions);
    }

    pub(in crate::host) fn terminal(&mut self, terminal: &TerminalSession) {
        self.0.insert(terminal.id);
        // A retained descriptor can name its predecessor before a future
        // restoration records succession in the registry.
        self.extend(terminal.predecessor);
        self.extend(terminal.recovery_origin);
    }

    pub(in crate::host) fn chat(&mut self, chat: &Session) {
        self.0.insert(chat.id);
        if let Some(terminal) = &chat.terminal {
            self.terminal(terminal);
        }
    }

    pub(in crate::host) fn hosted(&mut self, session: &HostedSession) {
        match session {
            HostedSession::Terminal(terminal) => self.terminal(terminal),
            HostedSession::Chat(chat) => self.chat(chat),
        }
    }

    pub(in crate::host) fn pane(&mut self, pane: &CardKind) {
        match pane {
            CardKind::Terminal(terminal) => self.terminal(terminal),
            CardKind::Chat(chat) => self.chat(chat),
            _ => {}
        }
    }

    pub(in crate::host) fn recovery_terminal(&mut self, terminal: &Terminal) {
        self.terminal(&terminal.source);
        for source in &terminal.previous_sources {
            self.terminal(source);
        }
        for relaunch in &terminal.relaunches {
            self.terminal(&relaunch.source);
        }
    }

    pub(in crate::host) fn recovery_chat(&mut self, chat: &Chat) {
        self.chat(&chat.source);
        for source in &chat.previous_sources {
            self.chat(&source.source);
        }
    }

    pub(in crate::host) fn checkpoint(&mut self, checkpoint: &Checkpoint) {
        for terminal in &checkpoint.terminals {
            self.recovery_terminal(terminal);
        }
        for terminal in &checkpoint.carried {
            self.terminal(&terminal.source);
            for source in &terminal.previous_sources {
                self.terminal(source);
            }
            for relaunch in &terminal.relaunches {
                self.terminal(&relaunch.source);
            }
        }
        for chat in &checkpoint.chats {
            self.recovery_chat(chat);
        }
    }

    pub(in crate::host) fn merge(&mut self, other: &Self) {
        self.extend(other.0.iter().copied());
    }
}

impl Registry {
    pub(in crate::host) fn attention_data_version(&self) -> Result<ModelVersion, String> {
        let other_writes = self
            .db
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .map_err(|error| error.to_string())?;
        Ok(ModelVersion(other_writes, self.db.total_changes()))
    }

    /// Exclude independent model writers between validation and the journal
    /// delete. A busy model abandons this proof rather than waiting.
    pub(in crate::host) fn lock_attention_version(
        &self,
        expected: &ModelVersion,
    ) -> Result<Option<rusqlite::Transaction<'_>>, String> {
        use std::time::Duration;
        let timeout: u64 = self
            .db
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .map_err(|error| error.to_string())?;
        self.db
            .busy_timeout(Duration::ZERO)
            .map_err(|error| error.to_string())?;
        let attempt = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        );
        self.db
            .busy_timeout(Duration::from_millis(timeout))
            .map_err(|error| error.to_string())?;
        let lease = match attempt {
            Ok(lease) => lease,
            Err(error)
                if matches!(
                    error.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.to_string()),
        };
        Ok((self.attention_data_version()? == *expected).then_some(lease))
    }

    /// All model references and lineage assignments come from one SQLite
    /// snapshot. The caller rejects it if another writer changes that census.
    pub(in crate::host) fn attention_orphans(
        &self,
        mut references: References,
        sessions: impl IntoIterator<Item = TerminalId>,
    ) -> Result<(ModelVersion, BTreeSet<TerminalId>), String> {
        let version = self.attention_data_version()?;
        let transaction = self
            .db
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        schema::require_current(&transaction)?;
        for view in views::occupied(&transaction)? {
            for pane in view.panes {
                references.pane(&pane.kind);
            }
        }
        // Local pin publication covers a pane whose window has not yet saved
        // its descriptor, and remains authoritative across disconnects.
        {
            let mut statement = transaction
                .prepare(
                    "SELECT terminal FROM terminal_retention_pins
                 UNION SELECT terminal FROM shared_terminal_retention_pins",
                )
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?;
            for row in rows {
                let terminal = row
                    .map_err(|error| error.to_string())?
                    .parse::<TerminalId>()
                    .map_err(|error| error.to_string())?;
                references.extend([terminal]);
            }
        }
        add_open_agents(&transaction, &mut references)?;
        add_recovery_sources(&transaction, &mut references)?;
        // The registry retains authenticated native descriptors even when the
        // provider was not adopted into the current daemon's inventory.
        let mut after = None;
        loop {
            let owners = self.native_actor_ownerships(after, 128)?;
            let Some(last) = owners.last() else {
                break;
            };
            after = Some(last.actor());
            for owner in owners {
                match self.native_actor_retirement(owner.actor())? {
                    None => references.chat(owner.source()),
                    Some(retired)
                        if retired.is_pending() && retired.confirmed_session().is_some() =>
                    {
                        references.chat(retired.source());
                    }
                    Some(_) => {}
                }
            }
        }

        let candidates: BTreeMap<TerminalId, Lineage> = self.session_lineages(sessions)?;
        let protected: BTreeSet<Lineage> =
            self.session_lineages(references.0)?.into_values().collect();
        let missing = candidates
            .into_iter()
            .filter_map(|(id, lineage)| (!protected.contains(&lineage)).then_some(id))
            .collect();
        transaction.commit().map_err(|error| error.to_string())?;
        Ok((version, missing))
    }
}

fn add_open_agents(db: &rusqlite::Connection, references: &mut References) -> Result<(), String> {
    // The normal list endpoint deliberately caps at 1024. Absence requires
    // every record, including an accepted launch not yet bound to its agent.
    let mut agents = db
        .prepare(
            "SELECT a.record, p.source FROM agents a LEFT JOIN agent_launches p ON a.id=p.agent
         WHERE a.closed=0",
        )
        .map_err(|error| error.to_string())?;
    let rows = agents
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .map_err(|error| error.to_string())?;
    for row in rows {
        let (record, pending) = row.map_err(|error| error.to_string())?;
        let agent: Agent = decode(&record)?;
        if let Some(bound) = agent.session {
            references.hosted(&bound);
        }
        if let Some(console) = agent.console {
            references.terminal(&console);
        }
        if let Some(pending) = pending {
            references.hosted(&decode(&pending)?);
        }
    }
    Ok(())
}

fn add_recovery_sources(
    db: &rusqlite::Connection,
    references: &mut References,
) -> Result<(), String> {
    let mut authorities = db
        .prepare("SELECT source FROM agent_authorities")
        .map_err(|error| error.to_string())?;
    let rows = authorities
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    for row in rows {
        references.hosted(&decode(&row.map_err(|error| error.to_string())?)?);
    }

    let mut pane_starts = db
        .prepare("SELECT candidate, current_allocation FROM pane_chat_starts")
        .map_err(|error| error.to_string())?;
    let rows = pane_starts
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    for row in rows {
        let (candidate, current) = row.map_err(|error| error.to_string())?;
        let source: Session = decode(&candidate)?;
        if source.launch.session_id.is_some() || source.native_continuity.retains_provider() {
            references.chat(&source);
            references.extend([current
                .parse::<TerminalId>()
                .map_err(|error| error.to_string())?]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Origin, Spec, Target},
        model::{AgentId, CardId, ChannelId, LaunchRecipe, TerminalContinuity},
        workspace_model::{
            ModelEdit,
            views::{Pane, Scope},
        },
    };
    use rusqlite::params;

    fn chat(id: TerminalId) -> Session {
        Session {
            id,
            launch: crate::chat::Launch {
                provider: Provider::Claude,
                session_id: Some("saved-thread".into()),
                recipe: LaunchRecipe::shell("/project".into()).unwrap(),
                draft_model: None,
            },
            terminal: None,
            proof: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        }
    }

    fn pane(kind: CardKind) -> Pane {
        Pane {
            id: CardId::new(),
            title: String::new(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            kind,
            scope: None,
        }
    }

    #[test]
    fn every_reconciled_session_reference_protects_its_lineage() {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("workspace.sqlite3")).unwrap();
        let repository = std::path::PathBuf::from("/project");
        let channel = ChannelId::new();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: repository.clone(),
                name: "Project".into(),
                kind: Default::default(),
            })
            .unwrap();
        registry
            .apply(&ModelEdit::CreateChannel {
                id: channel,
                repository: repository.clone(),
                title: "Channel".into(),
                title_source: Default::default(),
                checkout: None,
            })
            .unwrap();
        let worktree = Scope::Worktree {
            repository: repository.clone(),
            path: "/project/worktree".into(),
        };
        let old = TerminalId::new();
        let saved = TerminalId::new();
        assert_eq!(
            registry.succeed(saved, old, None, |_| false).unwrap(),
            Lineage(old)
        );
        registry
            .apply(&ModelEdit::SeedView {
                scope: Scope::Channel(channel),
                panes: vec![pane(CardKind::Chat(chat(saved)))],
                closed: vec![],
            })
            .unwrap();

        let recipe = LaunchRecipe::shell(repository.clone()).unwrap();
        let worktree_terminal = TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope: worktree.clone(),
                panes: vec![pane(CardKind::Terminal(worktree_terminal.clone()))],
                closed: vec![],
            })
            .unwrap();
        let live = TerminalId::new();
        let local_pin = TerminalId::new();
        let bound = TerminalId::new();
        let pending = TerminalId::new();
        let restorable = TerminalId::new();
        let lost = TerminalId::new();
        registry.pin_window_terminals(&[local_pin]).unwrap();
        let agent = Agent {
            id: AgentId::new(),
            name: None,
            origin: Origin::User,
            spec: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository: repository.clone(),
                    directory: repository,
                    placement: None,
                },
                surface: Default::default(),
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: worktree,
            session: Some(HostedSession::Chat(chat(bound))),
            console: None,
            created_ms: 1,
            closed: false,
        };
        registry
            .db
            .execute(
                "INSERT INTO agents VALUES(?1,?2,?3,?4,0)",
                params![
                    agent.id.to_string(),
                    serde_json::to_string(&agent.origin).unwrap(),
                    serde_json::to_string(&agent).unwrap(),
                    bound.to_string()
                ],
            )
            .unwrap();
        registry
            .db
            .execute(
                "INSERT INTO agent_launches VALUES(?1,?2,?3)",
                params![
                    agent.id.to_string(),
                    pending.to_string(),
                    serde_json::to_string(&HostedSession::Chat(chat(pending))).unwrap()
                ],
            )
            .unwrap();
        registry
            .bind_pane_chat_start(CardId::new(), None, &chat(restorable))
            .unwrap();

        let mut references = References::default();
        references.extend([live]);
        let candidate_ids = [
            old,
            saved,
            worktree_terminal.id,
            live,
            local_pin,
            bound,
            pending,
            restorable,
            lost,
        ];
        let (_, orphans) = registry
            .attention_orphans(references, candidate_ids)
            .unwrap();
        assert_eq!(orphans, BTreeSet::from([lost]));
    }
}
