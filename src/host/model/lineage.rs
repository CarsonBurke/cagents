//! Which sessions succeed one another. A relaunched or recovered session gets
//! a new ID; recording it in its predecessor's lineage keeps what that session
//! owned. Rows are written before their session starts and never change.

use super::Registry;
use crate::{
    agent_execution::{Agent, HostedSession, Lineage, Origin},
    model::TerminalId,
};
use rusqlite::{OptionalExtension, Transaction, params};
use std::collections::BTreeMap;

/// Keeps an idempotency key that a rewritten origin would make collide out of
/// reach: requests cannot contain control characters, so no retry matches it.
const RETIRED_KEY_SEPARATOR: char = '\u{1f}';

pub(super) fn migrate(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    let current: u32 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if current < 4 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_lineages(
                 session_id TEXT PRIMARY KEY,
                 lineage TEXT NOT NULL
             ) STRICT;
             CREATE INDEX IF NOT EXISTS session_lineages_by_lineage
                 ON session_lineages(lineage);",
        )
        .map_err(|error| error.to_string())?;
        seed(tx)?;
    }
    Ok(())
}

/// Reconstructs lineages from the sessions that were issued authority, whose
/// recorded recovery origins name the root each relaunch descends from, then
/// moves every child to its parent's lineage.
fn seed(tx: &Transaction<'_>) -> Result<(), String> {
    let mut predecessors = BTreeMap::new();
    {
        let mut statement = tx
            .prepare("SELECT session_id, source FROM agent_authorities")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| error.to_string())?;
        for row in rows {
            let (id, source) = row.map_err(|error| error.to_string())?;
            let (Ok(id), Ok(source)) = (
                id.parse::<TerminalId>(),
                serde_json::from_str::<HostedSession>(&source),
            ) else {
                continue;
            };
            // A chat converted from a terminal keeps that terminal's ID and
            // records it, with the root the terminal descended from.
            let root = match &source {
                HostedSession::Terminal(terminal) => terminal.recovery_origin,
                HostedSession::Chat(chat) => chat
                    .terminal
                    .as_ref()
                    .and_then(|terminal| terminal.recovery_origin),
            };
            predecessors.insert(id, root.unwrap_or(id));
        }
    }
    let root = |id: TerminalId| {
        let mut current = id;
        for _ in 0..=predecessors.len() {
            match predecessors.get(&current) {
                Some(&next) if next != current => current = next,
                _ => break,
            }
        }
        current
    };
    for &id in predecessors.keys() {
        tx.execute(
            "INSERT OR IGNORE INTO session_lineages(session_id, lineage) VALUES (?1, ?2)",
            params![id.to_string(), root(id).to_string()],
        )
        .map_err(|error| error.to_string())?;
    }
    tx.execute(
        "INSERT OR IGNORE INTO session_lineages(session_id, lineage)
         SELECT session_id, session_id FROM agents WHERE session_id IS NOT NULL",
        [],
    )
    .map_err(|error| error.to_string())?;

    let moved = |origin: &str| -> Result<Option<(Origin, TerminalId)>, String> {
        let origin: Origin = super::execution::decode(origin)?;
        Ok(match origin {
            Origin::Child { parent } if root(parent.0) != parent.0 => Some((
                Origin::Child {
                    parent: Lineage(root(parent.0)),
                },
                parent.0,
            )),
            _ => None,
        })
    };
    let agents: Vec<(String, String, String)> = {
        let mut statement = tx
            .prepare("SELECT id, origin, record FROM agents")
            .map_err(|error| error.to_string())?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|error| error.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|error| error.to_string())?
    };
    for (id, origin, record) in agents {
        let Some((origin, _)) = moved(&origin)? else {
            continue;
        };
        let mut agent: Agent = super::execution::decode(&record)?;
        agent.origin = origin.clone();
        tx.execute(
            "UPDATE agents SET origin = ?2, record = ?3 WHERE id = ?1",
            params![
                id,
                super::execution::encode(&origin)?,
                super::execution::encode(&agent)?
            ],
        )
        .map_err(|error| error.to_string())?;
    }
    let runs: Vec<(String, String, String)> = {
        let mut statement = tx
            .prepare("SELECT id, origin, idempotency_key FROM agent_runs")
            .map_err(|error| error.to_string())?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|error| error.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|error| error.to_string())?
    };
    for (id, origin, key) in runs {
        let Some((origin, previous)) = moved(&origin)? else {
            continue;
        };
        let origin = super::execution::encode(&origin)?;
        let taken: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_runs
                 WHERE origin = ?1 AND idempotency_key = ?2)",
                params![origin, key],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        // Two relaunches of one session may each have used a key. A run whose
        // origin was already the root keeps it, or else the first one moved;
        // the other stays readable under a retired key that cannot be replayed.
        let key = if taken {
            format!("{key}{RETIRED_KEY_SEPARATOR}{previous}")
        } else {
            key
        };
        tx.execute(
            "UPDATE agent_runs SET origin = ?2, idempotency_key = ?3 WHERE id = ?1",
            params![id, origin, key],
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

impl Registry {
    /// The lineage a session joined when it was registered.
    pub(crate) fn session_lineage(&self, session: TerminalId) -> Result<Option<Lineage>, String> {
        self.db
            .query_row(
                "SELECT lineage FROM session_lineages WHERE session_id = ?1",
                [session.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .map(|lineage| {
                lineage
                    .parse()
                    .map(Lineage)
                    .map_err(|_| "Stored session lineage is invalid".to_owned())
            })
            .transpose()
    }

    /// Whether `id` was registered as a session or names a lineage.
    pub(crate) fn lineage_recorded(&self, id: TerminalId) -> Result<bool, String> {
        self.db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM session_lineages WHERE session_id = ?1 OR lineage = ?1)",
                [id.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }

    /// Each session's lineage; one never registered is its own.
    pub(crate) fn session_lineages(
        &self,
        sessions: impl IntoIterator<Item = TerminalId>,
    ) -> Result<BTreeMap<TerminalId, Lineage>, String> {
        sessions
            .into_iter()
            .map(|session| {
                Ok((
                    session,
                    self.session_lineage(session)?.unwrap_or(Lineage(session)),
                ))
            })
            .collect()
    }

    /// Records `successor` in `predecessor`'s lineage before it starts, unless
    /// `live` reports that a member of that lineage still runs: ownership
    /// passes only to a session that replaces every other. A successor that
    /// already belongs to a lineage keeps it, whatever it now claims.
    ///
    /// A predecessor that never started has no lineage of its own to pass on;
    /// its successor claims that of `root`, the chain's first session, instead,
    /// so a relaunch that failed to start does not cut the chain.
    ///
    /// The registry stays locked from reading the members to recording the
    /// successor, so of two sessions that claim one lineage, the second sees
    /// the first among the members. `live` takes the host lock; that order,
    /// registry before host, is the one every caller uses.
    pub(crate) fn succeed(
        &mut self,
        successor: TerminalId,
        predecessor: TerminalId,
        root: Option<TerminalId>,
        live: impl FnOnce(&[TerminalId]) -> bool,
    ) -> Result<Lineage, String> {
        if let Some(lineage) = self.session_lineage(successor)? {
            return Ok(lineage);
        }
        let claimed = match (self.session_lineage(predecessor)?, root) {
            (Some(lineage), _) => lineage,
            (None, Some(root)) => self.session_lineage(root)?.unwrap_or(Lineage(root)),
            (None, None) => Lineage(predecessor),
        };
        let mut members = self.lineage_members(claimed)?;
        for session in [predecessor, claimed.0] {
            if !members.contains(&session) {
                members.push(session);
            }
        }
        members.retain(|member| *member != successor);
        let lineage = if live(&members) {
            tracing::warn!(
                %successor,
                %predecessor,
                "another session of the predecessor's lineage still runs; the successor starts its own and does not inherit its agents"
            );
            Lineage(successor)
        } else {
            claimed
        };
        self.db
            .execute(
                "INSERT OR IGNORE INTO session_lineages(session_id, lineage) VALUES (?1, ?2)",
                params![successor.to_string(), lineage.to_string()],
            )
            .map_err(|error| error.to_string())?;
        Ok(lineage)
    }

    fn lineage_members(&self, lineage: Lineage) -> Result<Vec<TerminalId>, String> {
        let mut statement = self
            .db
            .prepare_cached("SELECT session_id FROM session_lineages WHERE lineage = ?1")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([lineage.to_string()], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        rows.map(|row| {
            row.map_err(|error| error.to_string())?
                .parse()
                .map_err(|_| "Stored session lineage is invalid".to_owned())
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Authority, Destination, Spec, Submit, Surface, Target},
        model::{LaunchRecipe, ProjectKind, TerminalContinuity, TerminalSession},
        workspace_model::ModelEdit,
    };

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

    fn shell(id: TerminalId, recovery_origin: Option<TerminalId>) -> TerminalSession {
        let launch = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: "/fixture".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        TerminalSession {
            owned_codex_listener: None,
            recovery_origin,
            predecessor: None,
            id,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch.clone(),
                last_exit: None,
            },
            launch,
            resume_hint: None,
            transcript_path: None,
        }
    }

    fn terminal(id: TerminalId, recovery_origin: Option<TerminalId>) -> HostedSession {
        HostedSession::Terminal(shell(id, recovery_origin))
    }

    /// A chat converted from a terminal keeps the terminal's ID.
    fn converted_chat(id: TerminalId, recovery_origin: Option<TerminalId>) -> HostedSession {
        HostedSession::Chat(crate::chat::Session {
            native_continuity: Default::default(),
            id,
            launch: crate::chat::Launch {
                provider: Provider::Claude,
                recipe: shell(id, None).launch,
                session_id: Some(uuid::Uuid::new_v4().to_string()),
            },
            proof: None,
            terminal: Some(shell(id, recovery_origin)),
            owned_codex_listener: None,
        })
    }

    fn child(lineage: TerminalId) -> Origin {
        Origin::Child {
            parent: Lineage(lineage),
        }
    }

    fn origin(registry: &Registry, authority: &Authority) -> Origin {
        registry.execution_origin(authority).unwrap()
    }

    #[test]
    fn a_successor_owns_its_predecessors_agents_only_once_no_other_member_runs() {
        let (mut registry, _directory, request) = fixture();
        let first = TerminalId::new();
        let authority = registry
            .fresh_agent_authority(&terminal(first, None))
            .unwrap();
        assert_eq!(origin(&registry, &authority), child(first));
        let delegated = registry
            .submit_agent(&child(first), &request, 1)
            .unwrap()
            .agent;

        // A relaunch: nothing else of the lineage runs.
        let second = TerminalId::new();
        let lineage = registry
            .succeed(second, first, None, |members| {
                assert_eq!(members, [first]);
                false
            })
            .unwrap();
        assert_eq!(lineage, Lineage(first));
        let successor = registry
            .fresh_agent_authority(&terminal(second, Some(first)))
            .unwrap();
        assert_eq!(origin(&registry, &successor), child(first));
        assert_eq!(
            registry.agents(Some(&child(first))).unwrap()[0].id,
            delegated.id
        );

        // A second claim while the first successor runs starts its own lineage.
        let rival = TerminalId::new();
        let lineage = registry
            .succeed(rival, first, None, |members| {
                assert!(members.contains(&first) && members.contains(&second));
                assert!(!members.contains(&rival));
                true
            })
            .unwrap();
        assert_eq!(lineage, Lineage(rival));
        let rival_authority = registry
            .fresh_agent_authority(&terminal(rival, Some(first)))
            .unwrap();
        assert_eq!(origin(&registry, &rival_authority), child(rival));

        // A session keeps its lineage through every later claim and process.
        assert_eq!(
            registry
                .succeed(second, rival, None, |_| unreachable!("already recorded"))
                .unwrap(),
            Lineage(first)
        );
        let restarted = registry
            .fresh_agent_authority(&terminal(second, Some(first)))
            .unwrap();
        assert_ne!(restarted, successor);
        assert_eq!(origin(&registry, &restarted), child(first));
        assert_eq!(
            registry
                .session_lineages([first, second, rival, TerminalId::new()])
                .unwrap()
                .into_values()
                .filter(|lineage| *lineage == Lineage(first))
                .count(),
            2
        );
    }

    #[test]
    fn a_relaunch_that_never_started_passes_its_chain_on() {
        let (mut registry, _directory, _request) = fixture();
        let root = TerminalId::new();
        registry
            .fresh_agent_authority(&terminal(root, None))
            .unwrap();
        let rival = TerminalId::new();
        assert_eq!(
            registry.succeed(rival, root, None, |_| true).unwrap(),
            Lineage(rival)
        );
        // Each relaunch below failed before the daemon recorded it.
        let (failed, next) = (TerminalId::new(), TerminalId::new());
        assert_eq!(
            registry
                .succeed(next, failed, Some(root), |members| {
                    assert!(members.contains(&root) && members.contains(&failed));
                    false
                })
                .unwrap(),
            Lineage(root)
        );
        // A fork's own lineage is recorded, so its successors keep it.
        assert_eq!(
            registry
                .succeed(TerminalId::new(), rival, Some(root), |_| false)
                .unwrap(),
            Lineage(rival)
        );
    }

    #[test]
    fn version_three_registries_move_children_to_their_parents_lineage() {
        let (mut registry, _directory, mut request) = fixture();
        let root = TerminalId::new();
        let relaunch = TerminalId::new();
        let converted = TerminalId::new();
        let unrelated = TerminalId::new();
        let relaunched = registry
            .fresh_agent_authority(&terminal(relaunch, Some(root)))
            .unwrap();
        for session in [
            terminal(root, None),
            converted_chat(converted, Some(root)),
            terminal(unrelated, None),
        ] {
            registry.fresh_agent_authority(&session).unwrap();
        }
        // Before lineages each relaunch owned its own children, and two of
        // them may have used one idempotency key.
        request.idempotency_key = "shared".into();
        let from_root = registry.submit_agent(&child(root), &request, 1).unwrap();
        let from_relaunch = registry
            .submit_agent(&child(relaunch), &request, 2)
            .unwrap();
        request.idempotency_key = "own".into();
        let from_unrelated = registry
            .submit_agent(&child(unrelated), &request, 3)
            .unwrap();
        let path = super::super::automation_proposals::published_fixture_from_rows(&registry, 3);
        drop(registry);

        let registry = Registry::at(&path).unwrap();
        for session in [root, relaunch, converted] {
            assert_eq!(
                registry.session_lineage(session).unwrap(),
                Some(Lineage(root))
            );
        }
        assert_eq!(
            registry.session_lineage(unrelated).unwrap(),
            Some(Lineage(unrelated))
        );
        assert_eq!(origin(&registry, &relaunched), child(root));
        let mut owned: Vec<_> = registry
            .agents(Some(&child(root)))
            .unwrap()
            .into_iter()
            .map(|agent| (agent.id, agent.origin))
            .collect();
        owned.sort_by_key(|(id, _)| *id);
        let mut expected = vec![
            (from_root.agent.id, child(root)),
            (from_relaunch.agent.id, child(root)),
        ];
        expected.sort_by_key(|(id, _)| *id);
        assert_eq!(owned, expected);
        assert_eq!(
            registry.agent(from_unrelated.agent.id).unwrap().origin,
            child(unrelated)
        );
        let keys = |registry: &Registry| -> Vec<(String, String)> {
            let mut statement = registry
                .db
                .prepare("SELECT id, idempotency_key FROM agent_runs ORDER BY id")
                .unwrap();
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let migrated = keys(&registry);
        let key = |run: crate::model::RunId| {
            migrated
                .iter()
                .find(|(id, _)| *id == run.to_string())
                .unwrap()
                .1
                .clone()
        };
        // Whichever run kept the key, the other's can no longer be retried.
        let mut shared = [key(from_root.run.id), key(from_relaunch.run.id)];
        shared.sort();
        assert_eq!(shared[0], "shared");
        assert!(shared[1].starts_with("shared\u{1f}"));
        assert_eq!(key(from_unrelated.run.id), "own");
        drop(registry);

        let reopened = Registry::at(&path).unwrap();
        assert_eq!(keys(&reopened), migrated);
        assert_eq!(
            reopened
                .db
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            super::super::SCHEMA_VERSION
        );
    }
}
