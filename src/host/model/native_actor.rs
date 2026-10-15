//! Durable semantic receipts for an authenticated helper-owned protocol actor.
//! The helper keeps its outbox until all required stores confirm their writes.
use super::{Registry, execution};
use crate::{agent_execution::{HostedSession, Run, Update}, chat, daemon::holder_registry::{Kind, Record}, model::RunId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::host) enum NativeActorEvent {
    Execution { run: RunId, update: Update },
    Attention(crate::notifications::Evidence),
}

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    let tx = rusqlite::Transaction::new_unchecked(db, TransactionBehavior::Immediate).map_err(|error| error.to_string())?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS native_actor_cursors (
        actor TEXT PRIMARY KEY, owner TEXT NOT NULL, sequence INTEGER NOT NULL CHECK(sequence > 0), digest BLOB NOT NULL
    ) STRICT; PRAGMA user_version = 15;").map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

impl Registry {
    /// `owner` and `source` must already be authenticated by the helper socket
    /// and exact daemon lease. No user descriptor can create this authority.
    ///
    /// This commits the run transition and semantic cursor together. A repeated
    /// entry is harmless after a crash between commit and the helper's ACK.
    /// Attention still needs its journal commit before the caller ACKs: poll
    /// the helper's UNACKNOWLEDGED outbox, never start after this DB cursor.
    pub(in crate::host) fn commit_native_actor_event(
        &mut self, actor: Uuid, owner: &Record, source: &chat::Session,
        sequence: u64, event: &NativeActorEvent, now_ms: u64,
    ) -> Result<bool, String> {
        if actor.is_nil() || sequence == 0 || sequence > i64::MAX as u64
            || owner.kind != Kind::Native || owner.application != crate::instance::APPLICATION_ID
            || owner.terminal != source.id || !source.proof.as_ref().is_some_and(|proof| proof.is_complete() && proof.boot_id == owner.boot)
        { return Err("Native actor receipt has invalid ownership or sequence".into()); }
        let identity = serde_json::to_string(owner).map_err(|error| error.to_string())?;
        let digest = Sha256::digest(serde_json::to_vec(event).map_err(|error| error.to_string())?).to_vec();
        let tx = self.db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|error| error.to_string())?;
        let previous: Option<(String, u64, Vec<u8>)> = tx.query_row(
            "SELECT owner, sequence, digest FROM native_actor_cursors WHERE actor = ?1", [actor.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(|error| error.to_string())?;
        let cursor = if let Some((previous_owner, cursor, previous_digest)) = previous {
            if previous_owner != identity { return Err("Native actor receipt belongs to another exact helper".into()); }
            if sequence <= cursor {
                if sequence == cursor && previous_digest != digest { return Err("Native actor reused a semantic sequence for different evidence".into()); }
                return Ok(false);
            }
            cursor
        } else { 0 };
        if sequence != cursor + 1 { return Err("Native actor outbox has a missing semantic event; no cursor advanced".into()); }
        let mut changed = false;
        if let NativeActorEvent::Execution { run, update } = event {
            let record: String = tx.query_row("SELECT record FROM agent_runs WHERE id = ?1", [run.to_string()], |row| row.get(0)).map_err(|error| error.to_string())?;
            let current: Run = serde_json::from_str(&record).map_err(|error| error.to_string())?;
            let record: String = tx.query_row("SELECT record FROM agents WHERE id = ?1", [current.agent.to_string()], |row| row.get(0)).map_err(|error| error.to_string())?;
            let agent: crate::agent_execution::Agent = serde_json::from_str(&record).map_err(|error| error.to_string())?;
            if agent.closed || !agent.session.as_ref().is_some_and(|expected| expected.same_process(&HostedSession::Chat(source.clone()))) {
                return Err("Native actor outcome does not belong to this exact open agent".into());
            }
            // A user may have cancelled the run while an outcome was pending.
            // Its committed terminal decision is retained; no old event can
            // mutate the next accepted run or resurrect the cancelled one.
            if !current.state.is_terminal() {
                execution::update_agent_run_in(&tx, *run, current.revision, update.clone(), now_ms)?;
                changed = true;
            }
        }
        tx.execute("INSERT INTO native_actor_cursors(actor, owner, sequence, digest) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(actor) DO UPDATE SET sequence = excluded.sequence, digest = excluded.digest",
            params![actor.to_string(), identity, sequence, digest]).map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::Provider, agent_execution::{Destination, Origin, ResultSummary, Spec, State, Submit, Surface, Target}, model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, ProjectKind, TerminalId}, workspace_model::ModelEdit};

    #[test]
    fn native_actor_outcomes_commit_with_cursor_and_replay_never_changes_the_run_twice() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        let mut registry = Registry::at(&path).unwrap();
        registry.apply(&ModelEdit::OpenProject { repository: directory.path().into(), name: "Remote actor fixture".into(), kind: ProjectKind::Folder }).unwrap();
        let accepted = registry.submit_agent(&Origin::User, &Submit {
            name: None, destination: Destination::New(Spec {
                provider: Provider::Claude, account: None, target: Target { repository: directory.path().into(), directory: directory.path().into(), placement: None },
                surface: Surface::Chat, model: None, permission_mode: None, reasoning_effort: None,
            }), prompt: "One accepted prompt".into(), idempotency_key: "native-actor".into(),
        }, 1).unwrap();
        let proof = HostSessionProof { host_session_id: HostSessionId::new("host"), boot_id: BootId::new("boot"), host_incarnation: HostIncarnation::new("daemon") };
        let source = chat::Session { id: TerminalId::new(), launch: chat::Launch { provider: Provider::Claude,
            recipe: LaunchRecipe { program: "claude".into(), arguments: Vec::new(), working_directory: directory.path().into(), environment: Default::default(), unset_environment: Default::default() },
            session_id: Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into()),
        }, proof: Some(proof.clone()), terminal: None, owned_codex_listener: None, native_continuity: Default::default() };
        registry.bind_agent(accepted.agent.id, None, HostedSession::Chat(source.clone())).unwrap();
        let owner = Record { version: 4, build: None, kind: Kind::Native, application: crate::instance::APPLICATION_ID.into(), boot: proof.boot_id,
            terminal: source.id, holder: crate::daemon::holder_registry::Process { pid: 42, start: 7 }, root: crate::daemon::holder_registry::Process { pid: 43, start: 8 } };
        let actor = Uuid::new_v4();
        let started = NativeActorEvent::Execution { run: accepted.run.id, update: Update::Started { provider_key: "same-command-provider-id".into() } };
        registry.db.execute_batch("CREATE TRIGGER fail_cursor BEFORE INSERT ON native_actor_cursors BEGIN SELECT RAISE(ABORT, 'fixture cursor write failure'); END;").unwrap();
        assert!(registry.commit_native_actor_event(actor, &owner, &source, 1, &started, 2).is_err());
        assert_eq!(registry.agent_run(accepted.run.id).unwrap(), accepted.run, "failed receipt must roll back the semantic transition too");
        registry.db.execute_batch("DROP TRIGGER fail_cursor;").unwrap();
        assert!(registry.commit_native_actor_event(actor, &owner, &source, 1, &started, 2).unwrap());
        let running = registry.agent_run(accepted.run.id).unwrap();
        drop(registry);
        let mut registry = Registry::at(&path).unwrap();
        assert!(!registry.commit_native_actor_event(actor, &owner, &source, 1, &started, 3).unwrap());
        assert_eq!(registry.agent_run(accepted.run.id).unwrap(), running);
        let finished = NativeActorEvent::Execution { run: accepted.run.id, update: Update::Finished { state: State::Succeeded, result: ResultSummary { text: "Exact retained result".into(), ..Default::default() } } };
        assert!(registry.commit_native_actor_event(actor, &owner, &source, 2, &finished, 4).unwrap());
        let done = registry.agent_run(accepted.run.id).unwrap();
        assert_eq!(done.state, State::Succeeded);
        assert_eq!(done.result.text, "Exact retained result");
        assert!(!registry.commit_native_actor_event(actor, &owner, &source, 1, &started, 5).unwrap());
        assert!(registry.commit_native_actor_event(actor, &owner, &source, 2, &started, 5).is_err(), "same sequence cannot name another event");
        let mut different_owner = owner.clone(); different_owner.holder.start += 1;
        assert!(registry.commit_native_actor_event(actor, &different_owner, &source, 3, &started, 5).is_err());
        assert!(registry.commit_native_actor_event(actor, &owner, &source, 4, &started, 5).is_err(), "event gaps must not ACK away uncommitted outcomes");
        let mut different_account = source.clone(); different_account.launch.recipe.environment.insert("CLAUDE_CONFIG_DIR".into(), "/another-account".into());
        assert!(registry.commit_native_actor_event(actor, &owner, &different_account, 3, &started, 5).is_err());
        assert_eq!(registry.agent_run(accepted.run.id).unwrap(), done);
    }
}
