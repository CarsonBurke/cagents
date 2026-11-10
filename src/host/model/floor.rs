//! Preserve only the reviewed managed presentations that old ChatClose retires.
use super::*;
use crate::{
    agent_execution::{Agent, HostedSession},
    upgrade::floor::{Presentation, Record},
};

impl Registry {
    #[cfg(test)]
    pub(crate) fn capture_update_floor(&self, record: &mut Record) -> Result<(), String> {
        let transaction = self.db.unchecked_transaction().map_err(|e| e.to_string())?;
        self.capture_update_floor_snapshot(record)?;
        transaction.commit().map_err(|e| e.to_string())
    }

    fn capture_update_floor_snapshot(&self, record: &mut Record) -> Result<(), String> {
        let views = super::views::all(&self.db)?;
        for session in &record.chats {
            let Some(agent) = self.agent_for_session(session.id)? else {
                continue;
            };
            if agent.closed
                || !agent
                    .session
                    .as_ref()
                    .is_some_and(|owned| owned.same_process(&HostedSession::Chat(session.clone())))
            {
                continue;
            }
            let id = crate::model::CardId::from_uuid(agent.id.as_uuid());
            let mut found = views.iter().filter_map(|view| {
                view.panes
                    .iter()
                    .enumerate()
                    .find(|(_, pane)| pane.id == id)
                    .map(|(ordinal, pane)| (view, ordinal, pane))
            });
            let Some((view, ordinal, pane)) = found.next() else {
                continue;
            };
            if found.next().is_some() {
                return Err("A reviewed agent has ambiguous pane ownership".into());
            }
            if let crate::model::CardKind::Chat(saved) = &pane.kind
                && crate::upgrade::floor::same_allocation(session, saved)
                && !record.aliases.contains(saved)
            {
                record.aliases.push(saved.clone());
            }
            record.presentations.push(Presentation {
                agent: agent.id,
                session: session.clone(),
                scope: view.scope.clone(),
                pane: pane.clone(),
                ordinal,
                restored: false,
            });
        }
        Ok(())
    }

    pub(crate) fn restore_update_floor(&mut self, record: &mut Record) -> Result<(), String> {
        self.restore_update_floor_inner(record)?;
        record.save()
    }

    fn restore_update_floor_inner(&mut self, record: &mut Record) -> Result<(), String> {
        let transaction = self.db.transaction().map_err(|e| e.to_string())?;
        let mut changed = false;
        for saved in &mut record.presentations {
            if saved.restored {
                continue;
            }
            let raw: Option<String> = transaction
                .query_row(
                    "SELECT record FROM agents WHERE id=?1",
                    [saved.agent.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            let Some(raw) = raw else {
                saved.restored = true;
                continue;
            };
            let agent: Agent = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            if !agent.closed
                || !agent.session.as_ref().is_some_and(|owned| {
                    owned.same_process(&HostedSession::Chat(saved.session.clone()))
                })
            {
                saved.restored = true;
                continue;
            }
            let id = saved.pane.id;
            let mut view = super::views::read(&transaction, &saved.scope)?;
            if !view.retired.contains(&id) || view.panes.iter().any(|pane| pane.id == id) {
                saved.restored = true;
                continue;
            }
            let owner: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM shared_view_panes WHERE id=?1)",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            if owner {
                return Err("The stopped agent's pane acquired another owner; retained presentation was not overwritten".into());
            }
            view.retired.retain(|retired| *retired != id);
            view.panes
                .insert(saved.ordinal.min(view.panes.len()), saved.pane.clone());
            view.revision = view
                .revision
                .checked_add(1)
                .ok_or("view revision exhausted")?;
            let scope = serde_json::to_string(&saved.scope).map_err(|e| e.to_string())?;
            transaction
                .execute(
                    "INSERT INTO shared_view_panes(id,scope) VALUES(?1,?2)",
                    params![id.to_string(), scope],
                )
                .map_err(|e| e.to_string())?;
            // Update only the exact stopped presentation. Runs, launch authority,
            // actor journals and retirement receipts remain as the old close left them.
            transaction.execute("UPDATE agents SET closed=0,record=json_set(record,'$.closed',json('false'),'$.process_end',?2) WHERE id=?1", params![saved.agent.to_string(), saved.session.id.to_string()]).map_err(|e| e.to_string())?;
            transaction
                .execute(
                    "UPDATE shared_views SET revision=?2,panes=?3,retired=?4 WHERE scope=?1",
                    params![
                        scope,
                        view.revision,
                        serde_json::to_string(&view.panes).map_err(|e| e.to_string())?,
                        serde_json::to_string(&view.retired).map_err(|e| e.to_string())?
                    ],
                )
                .map_err(|e| e.to_string())?;
            saved.restored = true;
            changed = true;
        }
        if changed {
            transaction
                .execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
                .map_err(|e| e.to_string())?;
        }
        transaction.commit().map_err(|e| e.to_string())?;
        // Durable one-shot acknowledgement prevents a later genuine pane close
        // from being undone on the next daemon start. Startup fails if it cannot
        // retain that acknowledgement, before any user can edit the restored pane.
        Ok(())
    }
}

pub(in crate::host) fn capture_presentations(record: &mut Record) -> Result<(), String> {
    let dirs = crate::instance::project_dirs()
        .ok_or_else(|| "the data directory is unavailable".to_owned())?;
    capture_presentations_at(&dirs.data_dir().join("workspace-model.sqlite3"), record)
}

fn capture_presentations_at(path: &Path, record: &mut Record) -> Result<(), String> {
    let registry = Registry {
        db: super::schema::open_readonly(path)?,
        path: path.to_owned(),
    };
    let snapshot = registry
        .db
        .unchecked_transaction()
        .map_err(|e| e.to_string())?;
    super::schema::require_published_snapshot(&snapshot)?;
    registry.capture_update_floor_snapshot(record)?;
    snapshot.commit().map_err(|e| e.to_string())
}

pub(in crate::host) fn restore_presentations() -> Result<(), String> {
    prune_history();
    let mut ledger = crate::upgrade::floor::load()?;
    if ledger
        .records
        .iter()
        .all(|record| record.presentations.iter().all(|pane| pane.restored))
    {
        return Ok(());
    }
    let mut registry = Registry::open()?;
    for record in &mut ledger.records {
        registry.restore_update_floor(record)?;
    }
    Ok(())
}

/// Pruning is maintenance, never a condition for opening the app. If a saved
/// store cannot be inspected, keep the evidence until its next successful read.
fn prune_history() {
    let result = (|| -> Result<(), String> {
        let mut referenced = std::collections::BTreeSet::new();
        let mut remember = |kind: &crate::model::CardKind| match kind {
            crate::model::CardKind::Chat(session) => {
                referenced.insert(session.id);
                if let Some(terminal) = &session.terminal {
                    referenced.insert(terminal.id);
                }
            }
            crate::model::CardKind::Terminal(session) => {
                referenced.insert(session.id);
            }
            _ => (),
        };
        let registry = Registry::open()?;
        for view in super::views::occupied(&registry.db)? {
            for pane in &view.panes {
                remember(&pane.kind);
            }
        }
        let mut budget = crate::persistence::MigrationInspectionBudget::new(
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        );
        for path in crate::instance::window_database_paths().map_err(|error| error.to_string())? {
            if !path.exists() {
                continue;
            }
            for pane in
                crate::persistence::read_labeled_migration_sessions_with_budget(&path, &mut budget)
                    .map_err(|error| error.to_string())?
            {
                remember(&pane.kind);
            }
        }
        crate::upgrade::floor::prune_unreferenced(&referenced)
    })();
    if let Err(error) = result {
        tracing::warn!(%error, "Stopped update history was kept until its panes can be inspected");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Origin, Spec, Surface, Target},
        chat::{Launch, NativeContinuity, Session},
        model::{
            AgentId, BootId, CardId, CardKind, ChannelId, HostIncarnation, HostSessionId,
            HostSessionProof, LaunchRecipe, TerminalId,
        },
        workspace_model::views::{Pane, Scope, View},
    };

    fn fixture() -> (Registry, Record, Agent) {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE metadata(key INTEGER PRIMARY KEY,sequence INTEGER); INSERT INTO metadata VALUES(0,0); CREATE TABLE agents(id TEXT PRIMARY KEY,origin TEXT,record TEXT,session_id TEXT UNIQUE,closed INTEGER); CREATE TABLE shared_view_scopes(scope TEXT PRIMARY KEY); CREATE TABLE shared_views(scope TEXT PRIMARY KEY,revision INTEGER,panes TEXT,retired TEXT); CREATE TABLE shared_view_panes(id TEXT PRIMARY KEY,scope TEXT); CREATE TABLE agent_runs(id TEXT PRIMARY KEY,state TEXT); INSERT INTO agent_runs VALUES('unfinished','cancelled'); CREATE TABLE agent_authorities(token TEXT PRIMARY KEY); ").unwrap();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("old"),
        };
        let session = Session {
            id: TerminalId::new(),
            launch: Launch {
                provider: Provider::Claude,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: vec![],
                    working_directory: "/work".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some("recorded".into()),
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: NativeContinuity::ReconnectOnly,
        };
        let agent = Agent {
            id: AgentId::new(),
            name: None,
            origin: Origin::User,
            spec: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository: "/work".into(),
                    directory: "/work".into(),
                    placement: None,
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: Scope::Channel(ChannelId::new()),
            session: Some(HostedSession::Chat(session.clone())),
            console: None,
            created_ms: 1,
            closed: false,
        };
        let id = CardId::from_uuid(agent.id.as_uuid());
        let pane = Pane {
            id,
            title: "kept".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            kind: CardKind::Chat(session.clone()),
            scope: None,
        };
        let view = View {
            scope: agent.placement.clone(),
            revision: 1,
            panes: vec![pane],
            retired: vec![],
        };
        let scope = serde_json::to_string(&view.scope).unwrap();
        db.execute("INSERT INTO shared_view_scopes VALUES(?1)", [&scope])
            .unwrap();
        db.execute(
            "INSERT INTO agents VALUES(?1,'user',?2,?3,0)",
            params![
                agent.id.to_string(),
                serde_json::to_string(&agent).unwrap(),
                session.id.to_string()
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO shared_views VALUES(?1,1,?2,'[]')",
            params![scope, serde_json::to_string(&view.panes).unwrap()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO shared_view_panes VALUES(?1,?2)",
            params![id.to_string(), scope],
        )
        .unwrap();
        (
            Registry {
                db,
                path: PathBuf::new(),
            },
            Record::new(proof, vec![session], vec![]),
            agent,
        )
    }
    #[test]
    fn capturing_live_schema_twenty_two_never_admits_a_new_writer_or_migrates_it() {
        let (fixture, mut record, agent) = fixture();
        let view = super::super::views::all(&fixture.db).unwrap().remove(0);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("workspace-model.sqlite3");
        let mut previous_writer = Connection::open(&path).unwrap();
        let tx = previous_writer.transaction().unwrap();
        super::super::migrate_through(&tx, 0, 22).unwrap();
        tx.pragma_update(None, "user_version", 22).unwrap();
        tx.commit().unwrap();
        previous_writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .unwrap();
        let scope = serde_json::to_string(&view.scope).unwrap();
        let Scope::Channel(channel) = view.scope else {
            panic!("the captured fixture must have a channel pane");
        };
        previous_writer
            .execute("INSERT INTO projects VALUES('/work','Work',1,'git')", [])
            .unwrap();
        previous_writer
            .execute(
                "INSERT INTO channels VALUES(?1,'/work','Channel',1,2)",
                [channel.to_string()],
            )
            .unwrap();
        previous_writer
            .execute(
                "INSERT INTO shared_view_scopes(scope,channel) VALUES(?1,?2)",
                params![scope, channel.to_string()],
            )
            .unwrap();
        previous_writer
            .execute(
                "INSERT INTO shared_views VALUES(?1,1,?2,'[]')",
                params![scope, serde_json::to_string(&view.panes).unwrap()],
            )
            .unwrap();
        previous_writer
            .execute(
                "INSERT INTO shared_view_panes VALUES(?1,?2)",
                params![view.panes[0].id.to_string(), scope],
            )
            .unwrap();
        previous_writer
            .execute(
                "INSERT INTO agents VALUES(?1,?2,?3,?4,0)",
                params![
                    agent.id.to_string(),
                    serde_json::to_string(&agent.origin).unwrap(),
                    serde_json::to_string(&agent).unwrap(),
                    record.chats[0].id.to_string(),
                ],
            )
            .unwrap();
        // A raw descriptor must not close while any SQLite connection to the
        // same inode retains POSIX locks. Pin both files until after the writer
        // drops, including during assertion unwinding.
        struct PinnedWriter {
            db: Connection,
            main: fs::File,
            wal: fs::File,
        }
        fn bytes(file: &fs::File) -> Vec<u8> {
            use std::os::unix::fs::FileExt;
            let mut bytes = vec![0; file.metadata().unwrap().len() as usize];
            file.read_exact_at(&mut bytes, 0).unwrap();
            bytes
        }
        let mut previous = PinnedWriter {
            db: previous_writer,
            main: fs::File::open(&path).unwrap(),
            wal: fs::File::open(path.with_extension("sqlite3-wal")).unwrap(),
        };
        let before = (bytes(&previous.main), bytes(&previous.wal));
        // Retain a real old writer transaction while the new build captures
        // presentations. A migration would need this writer and change schema22.
        let writer = previous
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        capture_presentations_at(&path, &mut record).unwrap();
        assert_eq!(record.presentations.len(), 1);
        assert_eq!(record.presentations[0].pane, view.panes[0]);
        assert_eq!(
            writer
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            22
        );
        assert_eq!(
            writer
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name='console_relaunches'",
                    [],
                    |row| row.get::<_, u32>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!((bytes(&previous.main), bytes(&previous.wal)), before);
        writer
            .execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
            .unwrap();
        writer.commit().unwrap();
    }

    #[test]
    fn restoring_an_old_close_keeps_only_the_reviewed_pane_and_never_replays_work() {
        let (mut registry, mut record, agent) = fixture();
        registry.capture_update_floor(&mut record).unwrap();
        assert_eq!(record.presentations.len(), 1);
        let id = record.presentations[0].pane.id;
        registry
            .db
            .execute(
                "UPDATE agents SET closed=1,record=json_set(record,'$.closed',json('true'))",
                [],
            )
            .unwrap();
        registry
            .db
            .execute(
                "UPDATE shared_views SET panes='[]',retired=?1",
                [serde_json::to_string(&vec![id]).unwrap()],
            )
            .unwrap();
        registry
            .db
            .execute("DELETE FROM shared_view_panes", [])
            .unwrap();
        registry.restore_update_floor_inner(&mut record).unwrap();
        let view = super::super::views::read(&registry.db, &agent.placement).unwrap();
        assert_eq!(view.panes.len(), 1);
        assert!(!view.retired.contains(&id));
        assert!(
            !registry
                .agent_for_session(record.chats[0].id)
                .unwrap()
                .unwrap()
                .closed
        );
        let state: String = registry
            .db
            .query_row("SELECT state FROM agent_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(state, "cancelled");
        let authorities: u64 = registry
            .db
            .query_row("SELECT count(*) FROM agent_authorities", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(authorities, 0);
        registry
            .db
            .execute("UPDATE agents SET closed=1", [])
            .unwrap();
        registry.restore_update_floor_inner(&mut record).unwrap();
        let closed: bool = registry
            .db
            .query_row("SELECT closed FROM agents", [], |row| row.get(0))
            .unwrap();
        assert!(closed, "later actual closes must not be undone");
    }
}
