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
        let views = super::views::store::published_snapshot(&self.db)?;
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
            if let crate::model::CardKind::Chat(saved) = &pane.kind {
                if !crate::upgrade::floor::same_allocation(session, saved) {
                    return Err("A reviewed agent's pane names a different chat allocation".into());
                }
                if !record.aliases.contains(saved) {
                    record.aliases.push(saved.clone());
                }
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
        for saved in &record.presentations {
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
                continue;
            };
            let agent: Agent = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            if !agent.closed
                || !agent.session.as_ref().is_some_and(|owned| {
                    owned.same_process(&HostedSession::Chat(saved.session.clone()))
                })
            {
                continue;
            }
            let id = saved.pane.id;
            let mut view = super::views::read(&transaction, &saved.scope)?;
            if !view.retired.contains(&id) || view.panes.iter().any(|pane| pane.id == id) {
                continue;
            }
            if super::views::store::owner(&transaction, id)?.is_some()
                || super::views::store::resource_holder(&transaction, &saved.pane)?.is_some()
            {
                return Err("The stopped agent's pane acquired another owner; retained presentation was not overwritten".into());
            }
            let before = view.clone();
            view.retired.retain(|retired| *retired != id);
            view.panes
                .insert(saved.ordinal.min(view.panes.len()), saved.pane.clone());
            view.revision = super::views::store::next_revision(&transaction)?;
            // Retiring the old pane removed its placement generation. This is
            // a new canonical insertion, so issue its epoch in this same TX.
            super::channel_work::placement::observe_view_transition(&transaction, &before, &view)?;
            super::views::store::store(&transaction, &view)?;
            super::terminal_retention::sync_view_pins(&transaction, &view.panes)?;
            // Update only the exact stopped presentation. Runs, launch authority,
            // actor journals and retirement receipts remain as the old close left them.
            transaction.execute("UPDATE agents SET closed=0,record=json_set(record,'$.closed',json('false'),'$.process_end',?2) WHERE id=?1", params![saved.agent.to_string(), saved.session.id.to_string()]).map_err(|e| e.to_string())?;
            changed = true;
        }
        if changed {
            transaction
                .execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
                .map_err(|e| e.to_string())?;
        }
        transaction.commit().map_err(|e| e.to_string())?;
        // Do not acknowledge any entry before the whole model transaction has
        // committed. A later conflicting pane must leave earlier entries retryable.
        for saved in &mut record.presentations {
            saved.restored = true;
        }
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

    fn store_view(db: &Connection, view: &View) {
        let tx = db.unchecked_transaction().unwrap();
        let before = super::super::views::read(&tx, &view.scope).unwrap();
        super::super::channel_work::placement::observe_view_transition(&tx, &before, view).unwrap();
        super::super::views::store::store(&tx, view).unwrap();
        super::super::terminal_retention::sync_view_pins(&tx, &view.panes).unwrap();
        tx.commit().unwrap();
    }

    fn fixture() -> (Registry, Record, Agent) {
        let mut db = Connection::open_in_memory().unwrap();
        let tx = db.transaction().unwrap();
        super::super::migrate_through(&tx, 0, SCHEMA_VERSION).unwrap();
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)
            .unwrap();
        tx.commit().unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
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
        let Scope::Channel(channel) = view.scope else {
            panic!("fixture uses a channel")
        };
        db.execute("INSERT INTO projects VALUES('/work','Work',1,'git')", [])
            .unwrap();
        db.execute(
            "INSERT INTO channels VALUES(?1,'/work','Channel',1,2)",
            [channel.to_string()],
        )
        .unwrap();
        super::super::views::store::register(&db, &view.scope).unwrap();
        db.execute("UPDATE view_revision_clock SET last=1", [])
            .unwrap();
        db.execute(
            "INSERT INTO agents VALUES(?1,?4,?2,?3,0)",
            params![
                agent.id.to_string(),
                serde_json::to_string(&agent).unwrap(),
                session.id.to_string(),
                serde_json::to_string(&agent.origin).unwrap()
            ],
        )
        .unwrap();
        store_view(&db, &view);
        db.execute(
            "INSERT INTO agent_runs VALUES('unfinished',?1,'user','test','{}','cancelled',1,'{}')",
            [agent.id.to_string()],
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
    fn paired_fixture() -> (Registry, Record, Agent) {
        let (registry, mut record, mut agent) = fixture();
        let mut session = record.chats[0].clone();
        let proof = session.proof.as_ref().unwrap();
        session.terminal = Some(crate::model::TerminalSession {
            id: session.id,
            launch: LaunchRecipe::shell("/work".into()).unwrap(),
            transcript_path: None,
            resume_hint: None,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            continuity: crate::model::TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            },
        });
        agent.session = Some(HostedSession::Chat(session.clone()));
        registry
            .db
            .execute(
                "UPDATE agents SET record=?2 WHERE id=?1",
                params![agent.id.to_string(), serde_json::to_string(&agent).unwrap()],
            )
            .unwrap();
        let mut view = super::super::views::read(&registry.db, &agent.placement).unwrap();
        view.panes[0].kind = CardKind::Chat(session.clone());
        store_view(&registry.db, &view);
        record.chats[0] = session;
        (registry, record, agent)
    }

    fn close_presentation(registry: &Registry, agent: &Agent) {
        registry.db.execute(
            "UPDATE agents SET closed=1,record=json_set(record,'$.closed',json('true')) WHERE id=?1",
            [agent.id.to_string()],
        ).unwrap();
        let mut view = super::super::views::read(&registry.db, &agent.placement).unwrap();
        let id = CardId::from_uuid(agent.id.as_uuid());
        view.panes.retain(|pane| pane.id != id);
        view.retired.push(id);
        view.revision = super::super::views::store::next_revision(&registry.db).unwrap();
        store_view(&registry.db, &view);
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
    fn capture_retains_only_the_exact_saved_chat_alias_and_model_identity() {
        let (registry, mut record, agent) = fixture();
        let identity: String = registry
            .db
            .query_row("SELECT identity FROM metadata", [], |row| row.get(0))
            .unwrap();
        let mut view = super::super::views::read(&registry.db, &agent.placement).unwrap();
        let CardKind::Chat(alias) = &mut view.panes[0].kind else {
            unreachable!()
        };
        alias.proof = None;
        alias.launch.session_id = None;
        let alias = alias.clone();
        store_view(&registry.db, &view);
        registry.capture_update_floor(&mut record).unwrap();
        assert_eq!(record.aliases, vec![alias]);
        assert_eq!(record.presentations[0].pane, view.panes[0]);
        assert_eq!(record.presentations[0].session, record.chats[0]);
        assert_eq!(
            registry
                .db
                .query_row("SELECT identity FROM metadata", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            identity
        );
        let mut foreign = record.clone();
        foreign.presentations.clear();
        foreign.aliases.clear();
        let CardKind::Chat(alias) = &mut view.panes[0].kind else {
            unreachable!()
        };
        alias.id = TerminalId::new();
        store_view(&registry.db, &view);
        assert!(
            registry
                .capture_update_floor(&mut foreign)
                .unwrap_err()
                .contains("different chat allocation")
        );
        assert!(foreign.presentations.is_empty());
        assert!(foreign.aliases.is_empty());
    }

    #[test]
    fn paused_restore_preserves_siblings_retirements_identity_and_global_revision() {
        let (mut registry, mut record, agent) = fixture();
        let identity: String = registry
            .db
            .query_row("SELECT identity FROM metadata", [], |row| row.get(0))
            .unwrap();
        let mut view = super::super::views::read(&registry.db, &agent.placement).unwrap();
        let kept = view.panes[0].clone();
        let sibling = |title: &str| {
            let mut pane = kept.clone();
            pane.id = CardId::new();
            pane.title = title.into();
            let CardKind::Chat(session) = &mut pane.kind else {
                unreachable!()
            };
            session.id = TerminalId::new();
            pane
        };
        let left = sibling("left");
        let right = sibling("right");
        let retired = CardId::new();
        view.panes = vec![left.clone(), kept.clone(), right.clone()];
        view.retired = vec![retired];
        store_view(&registry.db, &view);
        registry.capture_update_floor(&mut record).unwrap();
        assert_eq!(record.presentations[0].ordinal, 1);
        close_presentation(&registry, &agent);
        registry
            .db
            .execute("UPDATE view_revision_clock SET last=100", [])
            .unwrap();
        registry.restore_update_floor_inner(&mut record).unwrap();
        let restored = super::super::views::read(&registry.db, &agent.placement).unwrap();
        assert_eq!(restored.panes, vec![left, kept, right]);
        assert_eq!(restored.retired, vec![retired]);
        assert_eq!(restored.revision, 101);
        assert!(
            registry.agent_process_ended(agent.id).unwrap(),
            "restored chat remains paused"
        );
        assert_eq!(
            registry
                .db
                .query_row("SELECT identity FROM metadata", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            identity
        );
        assert!(record.presentations.iter().all(|saved| saved.restored));
    }

    #[test]
    fn conflicting_resource_rolls_back_all_restores_and_keeps_them_unacknowledged() {
        let (mut registry, mut record, agent) = paired_fixture();
        let mut second = agent.clone();
        second.id = AgentId::new();
        let Some(HostedSession::Chat(session)) = &mut second.session else {
            unreachable!()
        };
        session.id = TerminalId::new();
        session.terminal.as_mut().unwrap().id = session.id;
        let session = session.clone();
        registry
            .db
            .execute(
                "INSERT INTO agents VALUES(?1,?2,?3,?4,0)",
                params![
                    second.id.to_string(),
                    serde_json::to_string(&second.origin).unwrap(),
                    serde_json::to_string(&second).unwrap(),
                    session.id.to_string()
                ],
            )
            .unwrap();
        let mut view = super::super::views::read(&registry.db, &agent.placement).unwrap();
        let mut pane = view.panes[0].clone();
        pane.id = CardId::from_uuid(second.id.as_uuid());
        pane.kind = CardKind::Chat(session.clone());
        view.panes.push(pane);
        store_view(&registry.db, &view);
        record.chats.push(session);
        registry.capture_update_floor(&mut record).unwrap();
        assert_eq!(record.presentations.len(), 2);
        close_presentation(&registry, &agent);
        close_presentation(&registry, &second);
        let mut view = super::super::views::read(&registry.db, &agent.placement).unwrap();
        let mut foreign = record.presentations[1].pane.clone();
        foreign.id = CardId::new();
        view.panes.push(foreign);
        store_view(&registry.db, &view);
        let candidates = record
            .chats
            .iter()
            .map(|chat| chat.terminal.as_ref().unwrap().id)
            .collect();
        let pins_before = registry.pinned_terminals(&candidates).unwrap();
        assert_eq!(
            pins_before.len(),
            1,
            "only the competing pane is pinned before recovery"
        );
        assert!(
            registry
                .restore_update_floor_inner(&mut record)
                .unwrap_err()
                .contains("another owner")
        );
        assert!(record.presentations.iter().all(|saved| !saved.restored));
        assert_eq!(
            super::super::views::read(&registry.db, &agent.placement).unwrap(),
            view
        );
        let closed: u32 = registry
            .db
            .query_row("SELECT count(*) FROM agents WHERE closed=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(closed, 2, "the earlier restore must roll back too");
        assert_eq!(
            registry.pinned_terminals(&candidates).unwrap(),
            pins_before,
            "the earlier restored terminal pin must roll back too"
        );
    }

    #[test]
    fn restored_paired_chat_repins_its_terminal_until_a_genuine_close() {
        let (mut registry, mut record, agent) = paired_fixture();
        let terminal = record.chats[0].terminal.as_ref().unwrap().id;
        let candidates = std::collections::BTreeSet::from([terminal]);
        assert_eq!(registry.pinned_terminals(&candidates).unwrap(), candidates);
        registry.capture_update_floor(&mut record).unwrap();
        registry.close_agent(&agent.origin, agent.id).unwrap();
        assert!(registry.pinned_terminals(&candidates).unwrap().is_empty());
        registry.restore_update_floor_inner(&mut record).unwrap();
        assert_eq!(registry.pinned_terminals(&candidates).unwrap(), candidates);
        assert!(registry.agent_process_ended(agent.id).unwrap());
        registry.close_agent(&agent.origin, agent.id).unwrap();
        assert!(registry.pinned_terminals(&candidates).unwrap().is_empty());
        registry.restore_update_floor_inner(&mut record).unwrap();
        assert!(registry.pinned_terminals(&candidates).unwrap().is_empty());
        assert!(registry.agent(agent.id).unwrap().closed);
    }

    #[test]
    fn restoring_an_old_close_keeps_only_the_reviewed_pane_and_never_replays_work() {
        let (mut registry, mut record, agent) = fixture();
        registry.capture_update_floor(&mut record).unwrap();
        assert_eq!(record.presentations.len(), 1);
        let id = record.presentations[0].pane.id;
        close_presentation(&registry, &agent);
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
        let closed = registry.close_agent(&agent.origin, agent.id).unwrap();
        assert!(
            closed.closed,
            "a restored paused pane must retain normal close behavior"
        );
        let retired = super::super::views::read(&registry.db, &agent.placement).unwrap();
        assert!(retired.panes.iter().all(|pane| pane.id != id));
        assert!(retired.retired.contains(&id));
        registry.restore_update_floor_inner(&mut record).unwrap();
        let closed: bool = registry
            .db
            .query_row("SELECT closed FROM agents", [], |row| row.get(0))
            .unwrap();
        assert!(closed, "later actual closes must not be undone");
    }
}
