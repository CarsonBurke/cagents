// Published history migration contracts, included in host::model::schema::tests.
#[test]
fn history_migration_preserves_each_published_12_through_19_atomically_and_once() {
    use crate::model::{
        BootId, CardId, CardKind, HostIncarnation, HostSessionId, LaunchRecipe, TerminalContinuity,
        TerminalExit, TerminalId, TerminalSession,
    };
    use crate::workspace_model::views::{Pane, Scope};
    let _clear = Clear;
    for version in 12..=19 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite3");
        let (identity, channel) = legacy(&path, version);
        let scope = Scope::Channel(channel);
        let scope_key = serde_json::to_string(&scope).unwrap();
        let retired = (0..1500).map(|_| CardId::new()).collect::<Vec<_>>();
        let recipe = LaunchRecipe {
            program: "/bin/true".into(),
            arguments: vec![],
            working_directory: "/kept".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let source = TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            continuity: TerminalContinuity::Live {
                host_session_id: HostSessionId::new("history-host"),
                boot_id: BootId::new("history-boot"),
                host_incarnation: HostIncarnation::new("history-incarnation"),
            },
        };
        let mut ended = source.clone();
        ended.continuity = TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: Some(TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 4,
            }),
        };
        let pane = Pane {
            id: CardId::new(),
            title: "Retained ended fixture".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            scope: None,
            kind: CardKind::Terminal(ended.clone()),
        };
        let proof = (
            serde_json::to_string(&source).unwrap(),
            serde_json::to_string(&ended).unwrap(),
        );
        let mut db = Connection::open(&path).unwrap();
        let tx = db.transaction().unwrap();
        tx.execute(
            "INSERT INTO shared_view_scopes(scope,channel) VALUES(?1,?2)",
            rusqlite::params![scope_key, channel.to_string()],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO shared_views(scope,revision,panes,retired) VALUES(?1,9,?2,?3)",
            rusqlite::params![
                scope_key,
                serde_json::to_string(&vec![pane.clone()]).unwrap(),
                serde_json::to_string(&retired).unwrap()
            ],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO shared_view_panes(id,scope) VALUES(?1,?2)",
            rusqlite::params![pane.id.to_string(), scope_key],
        )
        .unwrap();
        if version >= 13 {
            tx.execute(
                "INSERT INTO shared_view_terminal_sources(pane,source,ended) VALUES(?1,?2,?3)",
                rusqlite::params![pane.id.to_string(), proof.0, proof.1],
            )
            .unwrap();
        }
        if version >= 18 {
            tx.execute(
                "INSERT INTO channel_work_placement_epochs(pane,epoch) VALUES(?1,1)",
                [pane.id.to_string()],
            )
            .unwrap();
            tx.execute(
                "UPDATE channel_work_placement_clock SET last_epoch=1 WHERE id=1",
                [],
            )
            .unwrap();
        }
        for _ in 0..2100 {
            tx.execute(
                "INSERT INTO shared_view_edits(operation,scope) VALUES(?1,?2)",
                rusqlite::params![uuid::Uuid::new_v4().to_string(), scope_key],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let prior_tables = db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*' AND name NOT IN ('metadata','shared_views','shared_view_edits') ORDER BY name")
            .unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        let preserved = history_preserved_rows(&db, &prior_tables, version);
        drop(db);
        let before = bytes(&path);
        for step in (version + 1)..=SCHEMA_VERSION {
            FAIL_STEP.with(|failure| failure.set(Some(step)));
            assert!(
                Registry::at(&path)
                    .err()
                    .unwrap()
                    .contains("injected migration failure")
            );
            FAIL_STEP.with(|failure| failure.set(None));
            assert_eq!(
                bytes(&path),
                before,
                "failed migration must preserve admitted published baseline {version}"
            );
            assert!(!wal(&path).exists());
        }
        let registry = Registry::at(&path).unwrap();
        require_current(&registry.db).unwrap();
        assert_eq!(
            history_preserved_rows(&registry.db, &prior_tables, version),
            preserved,
            "history migration preserves all pre-existing unrelated rows including CoreHome and placement clocks"
        );
        assert_eq!(registry.model().unwrap().identity.to_string(), identity);
        assert_eq!(registry.model().unwrap().sequence, 8);
        let view = super::super::views::read(&registry.db, &scope).unwrap();
        assert_eq!(view.revision, 10);
        assert_eq!(view.panes, vec![pane.clone()]);
        assert_eq!(view.retired, retired[retired.len() - 256..]);
        assert_eq!(
            registry
                .db
                .query_row::<u64, _, _>("SELECT count(*) FROM retired_view_panes", [], |row| row
                    .get(0))
                .unwrap(),
            1500
        );
        assert_eq!(
            registry
                .db
                .query_row::<u64, _, _>("SELECT count(*) FROM shared_view_edits", [], |row| row
                    .get(0))
                .unwrap(),
            2048,
            "recent receipt bound follows atomic permanent identity transfer"
        );
        assert_eq!(
            registry
                .db
                .query_row::<u64, _, _>(
                    "SELECT count(*) FROM compacted_view_operations",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
            2100 - 2048
        );
        assert_eq!(
            registry
                .db
                .query_row::<u64, _, _>(
                    "SELECT count(*) FROM compacted_view_operation_scopes",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            registry
                .db
                .pragma_query_value::<u32, _>(None, "synchronous", |row| row.get(0))
                .unwrap(),
            2
        );
        if version >= 13 {
            assert_eq!(
                registry
                    .db
                    .query_row::<(String, String), _, _>(
                        "SELECT source,ended FROM shared_view_terminal_sources WHERE pane=?1",
                        [pane.id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?))
                    )
                    .unwrap(),
                proof
            );
        } else {
            assert_eq!(
                registry
                    .db
                    .query_row::<u64, _, _>(
                        "SELECT count(*) FROM shared_view_terminal_sources",
                        [],
                        |row| row.get(0)
                    )
                    .unwrap(),
                0,
                "published12 must not invent historical terminal proof"
            );
        }
        let model = registry.model().unwrap();
        drop(registry);
        let reopened = Registry::at(&path).unwrap();
        assert_eq!(reopened.model().unwrap(), model);
        assert_eq!(
            super::super::views::read(&reopened.db, &scope).unwrap(),
            view
        );
    }
}

#[test]
fn unpublished_history_fourteen_is_still_refused_without_relabeling_native_actor_fourteen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unpublished-history14.sqlite3");
    legacy(&path, 13);
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE retired_view_panes(scope TEXT NOT NULL REFERENCES shared_view_scopes(scope) ON DELETE CASCADE,id TEXT NOT NULL,PRIMARY KEY(scope,id)) STRICT; CREATE INDEX agents_open_by_recency ON agents(closed) WHERE closed=0; PRAGMA user_version=14;").unwrap();
    drop(db);
    let before = bytes(&path);
    assert!(Registry::at(&path).err().unwrap().contains("layout"));
    assert_eq!(bytes(&path), before);
    assert!(!wal(&path).exists());
}

#[test]
fn malformed_published19_operation_refuses_whole_history_upgrade_without_deleting_legacy_rows() {
    use crate::workspace_model::views::Scope;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid-operation.sqlite3");
    let (_, channel) = legacy(&path, 19);
    let scope = serde_json::to_string(&Scope::Channel(channel)).unwrap();
    let mut db = Connection::open(&path).unwrap();
    let tx = db.transaction().unwrap();
    tx.execute(
        "INSERT INTO shared_view_scopes(scope,channel) VALUES(?1,?2)",
        rusqlite::params![scope, channel.to_string()],
    )
    .unwrap();
    for index in 0..2100 {
        let operation = if index == 1 {
            "not-a-uuid".into()
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        tx.execute(
            "INSERT INTO shared_view_edits(operation,scope) VALUES(?1,?2)",
            rusqlite::params![operation, scope],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let before = bytes(&path);
    assert!(
        Registry::at(&path)
            .err()
            .unwrap()
            .contains("invalid workspace operation UUID")
    );
    assert_eq!(bytes(&path), before);
    assert!(!wal(&path).exists());
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        19
    );
    assert_eq!(
        db.query_row::<u64, _, _>("SELECT count(*) FROM shared_view_edits", [], |row| row
            .get(0))
            .unwrap(),
        2100
    );
    assert_eq!(db.query_row::<u64, _, _>("SELECT count(*) FROM sqlite_schema WHERE name IN ('retired_view_panes','compacted_view_operations','compacted_view_operation_scopes')", [], |row| row.get(0)).unwrap(), 0);
}

// Compare every pre-existing unrelated table, including populated CoreHome and
// placement clock/epoch rows, without treating newly introduced tables as old.
fn history_preserved_rows(
    db: &Connection,
    names: &[String],
    version: u32,
) -> Vec<(String, Vec<Vec<String>>)> {
    names
        .iter()
        .map(|name| {
            let quoted = format!("\"{}\"", name.replace('"', "\"\""));
            let projection = if name == "automation_firings" && version < 22 {
                db.prepare(&format!("PRAGMA table_info({quoted})"))
                    .unwrap()
                    .query_map([], |row| row.get::<_, String>(1))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
                    .into_iter()
                    .filter(|column| column != "previous_firing")
                    .map(|column| format!("\"{}\"", column.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(",")
            } else {
                "*".into()
            };
            let select = format!("SELECT {projection} FROM {quoted}");
            let columns = db.prepare(&select).unwrap().column_count();
            let order = (1..=columns)
                .map(|column| column.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut query = db.prepare(&format!("{select} ORDER BY {order}")).unwrap();
            let rows = query
                .query_map([], |row| {
                    (0..columns)
                        .map(|column| row.get_ref(column).map(|value| format!("{value:?}")))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            (name.clone(), rows)
        })
        .collect()
}

// Real source APIs populate this private fixture, then the existing published
// constructor copies only the exact tables/columns of20,21or22. No newer store
// is stripped or relabeled. No Worker, provider, terminal or scheduler launches.
fn history_predecessor_source() -> (
    Registry,
    tempfile::TempDir,
    crate::workspace_model::views::Scope,
) {
    use crate::agent_execution::Update;
    use crate::automations::{DirectoryIdentity, FiringState, Occurrence, Trigger};
    use crate::model::{
        CardId, CardKind, LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession,
    };
    use crate::workspace_model::views::{Pane, Scope};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.sqlite3");
    let (_, channel) = legacy(&path, SCHEMA_VERSION);
    let mut registry = Registry::at(&path).unwrap();
    let scope = Scope::Channel(channel);
    let recipe = LaunchRecipe {
        program: "/bin/true".into(),
        arguments: vec![],
        working_directory: "/kept".into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let terminal = TerminalId::new();
    let pane = Pane {
        id: CardId::new(),
        title: "Saved synthetic terminal".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Terminal(TerminalSession {
            id: terminal,
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
        }),
    };
    registry
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![pane],
            closed: vec![],
        })
        .unwrap();
    registry.pin_window_terminals(&[terminal]).unwrap();
    let definition: crate::automations::Definition = serde_json::from_value(serde_json::json!({
        "name":"Synthetic upgrade source", "prompt":"No provider launched",
        "agent":{"provider":"Claude","account":null,"target":{"repository":"/kept","directory":"/kept","channel":null},"surface":"Chat","model":null,"permission_mode":null},
        "schedule":{"pattern":{"daily":{"hour":9,"minute":0}},"timezone":"UTC"},"session_mode":"fresh","precheck":null
    })).unwrap();
    let identity = DirectoryIdentity {
        device: 1,
        inode: 1,
    };
    let automation = registry
        .create_automation(definition.clone(), 100, identity.clone())
        .unwrap();
    let accepted = registry
        .claim_automation(
            automation.id,
            automation.execution_revision,
            None,
            Trigger::Manual {
                request: uuid::Uuid::new_v4(),
            },
            None,
            true,
            200,
        )
        .unwrap()
        .accepted
        .unwrap();
    let running = registry
        .update_agent_run(
            accepted.run.id,
            accepted.run.revision,
            Update::Started {
                provider_key: "synthetic".into(),
            },
            300,
        )
        .unwrap();
    registry
        .update_agent_run(running.id, running.revision, Update::Waiting, 400)
        .unwrap();
    assert!(
        !registry
            .pending_automation_attention(100)
            .unwrap()
            .is_empty()
    );

    // Exact22 lineage/cleanup payloads are real source transactions. The first
    // precheck never reaches Worker::launch, so its cleanup evidence is honest.
    let mut checked_definition = definition.clone();
    checked_definition.precheck = Some(crate::automations::Precheck {
        command: "exit 0".into(),
        timeout_seconds: 5,
    });
    let checked = registry
        .create_automation(checked_definition, 500, identity.clone())
        .unwrap();
    let first = registry
        .claim_automation(
            checked.id,
            checked.execution_revision,
            None,
            Trigger::Manual {
                request: uuid::Uuid::new_v4(),
            },
            None,
            true,
            600,
        )
        .unwrap()
        .firing;
    assert!(
        registry
            .finish_automation_precheck(
                first.id,
                crate::host::automations::precheck::Outcome::Stopped {
                    state: FiringState::Failed,
                    skip: None,
                    diagnostic: "Synthetic prelaunch refusal".into()
                },
                700
            )
            .unwrap()
            .is_none()
    );
    registry
        .confirm_unlaunched_automation_precheck(first.id, first.revision, 701)
        .unwrap();
    registry
        .claim_automation(
            checked.id,
            checked.execution_revision,
            None,
            Trigger::Manual {
                request: uuid::Uuid::new_v4(),
            },
            None,
            true,
            800,
        )
        .unwrap();

    registry
        .apply(&ModelEdit::OpenProject {
            repository: "/attention".into(),
            name: "Independent attention source".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let mut no_run_definition = definition;
    no_run_definition.agent.target.repository = "/attention".into();
    no_run_definition.agent.target.directory = "/attention".into();
    let no_run = registry
        .create_automation(no_run_definition, 900, identity)
        .unwrap();
    registry
        .set_automation_enabled(
            no_run.id,
            no_run.revision,
            Some(Occurrence {
                due_ms: 10_000,
                local: "future fixture".into(),
            }),
            true,
            901,
        )
        .unwrap();
    registry
        .apply(&ModelEdit::CloseProject {
            repository: "/attention".into(),
        })
        .unwrap();
    assert!(
        !registry
            .pending_automation_notifications(100)
            .unwrap()
            .is_empty()
    );
    (registry, directory, scope)
}

#[test]
fn actual_twenty_through_twenty_two_predecessors_keep_payloads_and_fail_atomically_before_twenty_three()
 {
    use crate::model::CardId;
    let _clear = Clear;
    let (source, _directory, scope) = history_predecessor_source();
    let expected_no_run = source.pending_automation_notifications(100).unwrap();
    let expected_run = source
        .pending_automation_attention(100)
        .unwrap()
        .into_iter()
        .map(|item| (item.receipt, item.event))
        .collect::<Vec<_>>();
    for version in 20..=22 {
        let path =
            super::super::automation_proposals::published_fixture_from_rows(&source, version);
        let mut db = Connection::open(&path).unwrap();
        require_open_layout(&db, version).unwrap();
        assert_eq!(inspect(&db).unwrap().version, version);
        assert_eq!(db.query_row::<u64,_,_>("SELECT count(*) FROM sqlite_schema WHERE name IN ('retired_view_panes','compacted_view_operations','compacted_view_operation_scopes')", [], |row| row.get(0)).unwrap(), 0);
        let retired = (0..1500).map(|_| CardId::new()).collect::<Vec<_>>();
        let key = serde_json::to_string(&scope).unwrap();
        let operations = (0..2100).map(|_| uuid::Uuid::new_v4()).collect::<Vec<_>>();
        let tx = db.transaction().unwrap();
        tx.execute(
            "UPDATE shared_views SET retired=?2 WHERE scope=?1",
            rusqlite::params![key, serde_json::to_string(&retired).unwrap()],
        )
        .unwrap();
        for operation in &operations {
            tx.execute(
                "INSERT INTO shared_view_edits(operation,scope) VALUES(?1,?2)",
                rusqlite::params![operation.to_string(), key],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let sequence: u64 = db
            .query_row("SELECT sequence FROM metadata WHERE key=0", [], |row| {
                row.get(0)
            })
            .unwrap();
        let revision: u64 = db
            .query_row(
                "SELECT revision FROM shared_views WHERE scope=?1",
                [&key],
                |row| row.get(0),
            )
            .unwrap();
        let panes: String = db
            .query_row(
                "SELECT panes FROM shared_views WHERE scope=?1",
                [&key],
                |row| row.get(0),
            )
            .unwrap();
        let names = db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*' AND name NOT IN ('metadata','shared_views','shared_view_edits') ORDER BY name").unwrap().query_map([], |row| row.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        let before_rows = history_preserved_rows(&db, &names, version);
        let previous = db.prepare("SELECT id,lag(id) OVER(PARTITION BY automation ORDER BY rowid) FROM automation_firings ORDER BY rowid").unwrap().query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert!(previous.iter().any(|(_, previous)| previous.is_some()));
        for table in [
            "terminal_retention_pins",
            "shared_terminal_retention_pins",
            "automation_notifications",
            "channel_work_homes",
            "channel_work_placement_epochs",
        ] {
            assert!(
                db.query_row::<u64, _, _>(&format!("SELECT count(*) FROM {table}"), [], |row| row
                    .get(0))
                    .unwrap()
                    > 0,
                "real{version} fixture must populate {table}"
            );
        }
        if version >= 21 {
            assert!(
                db.query_row::<u64, _, _>(
                    "SELECT count(*) FROM automation_run_attention",
                    [],
                    |row| row.get(0)
                )
                .unwrap()
                    > 0
            );
        }
        if version >= 22 {
            assert!(
                db.query_row::<u64, _, _>(
                    "SELECT count(*) FROM automation_precheck_history_cleanup",
                    [],
                    |row| row.get(0)
                )
                .unwrap()
                    > 0
            );
        }
        drop(db);
        let before = bytes(&path);
        for step in (version + 1)..=23 {
            FAIL_STEP.with(|failure| failure.set(Some(step)));
            let error = Registry::at(&path)
                .err()
                .expect("each actual successor step must fail atomically");
            FAIL_STEP.with(|failure| failure.set(None));
            assert!(error.contains(&format!("failure at {step}")), "{error}");
            assert_eq!(
                bytes(&path),
                before,
                "true{version} predecessor bytes after failed{step}"
            );
            assert!(!wal(&path).exists());
            let retained = Connection::open(&path).unwrap();
            require_open_layout(&retained, version).unwrap();
            assert_eq!(inspect(&retained).unwrap().version, version);
            assert_eq!(
                history_preserved_rows(&retained, &names, version),
                before_rows
            );
        }
        let registry = Registry::at(&path).unwrap();
        require_current(&registry.db).unwrap();
        assert_eq!(inspect(&registry.db).unwrap().version, 23);
        assert_eq!(registry.model().unwrap().sequence, sequence + 1);
        assert_eq!(
            history_preserved_rows(&registry.db, &names, version),
            before_rows,
            "typed source records, pending receipts, pins, CoreHome and placement survive"
        );
        let view = super::super::views::read(&registry.db, &scope).unwrap();
        assert_eq!(view.revision, revision + 1);
        assert_eq!(serde_json::to_string(&view.panes).unwrap(), panes);
        assert_eq!(view.retired, retired[1244..]);
        assert_eq!(
            registry
                .db
                .query_row::<u64, _, _>("SELECT count(*) FROM retired_view_panes", [], |row| row
                    .get(0))
                .unwrap(),
            1500
        );
        assert_eq!(
            registry
                .db
                .query_row::<u64, _, _>("SELECT count(*) FROM shared_view_edits", [], |row| row
                    .get(0))
                .unwrap(),
            2048
        );
        assert_eq!(
            registry
                .db
                .query_row::<u64, _, _>(
                    "SELECT count(*) FROM compacted_view_operations",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
            52
        );
        for operation in &operations {
            assert!(registry.view_operation_seen(*operation).unwrap());
        }
        assert_eq!(
            registry.pending_automation_notifications(100).unwrap(),
            expected_no_run
        );
        let attention = registry
            .pending_automation_attention(100)
            .unwrap()
            .into_iter()
            .map(|item| (item.receipt, item.event))
            .collect::<Vec<_>>();
        assert_eq!(
            attention,
            if version >= 21 {
                expected_run.clone()
            } else {
                vec![]
            },
            "upgrade cannot invent run-attention history for20"
        );
        let after_previous = registry
            .db
            .prepare("SELECT id,previous_firing FROM automation_firings ORDER BY rowid")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(after_previous, previous);
        assert_eq!(
            registry
                .db
                .query_row::<u64, _, _>(
                    "SELECT count(*) FROM pragma_foreign_key_check",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
            0
        );
        let model = registry.model().unwrap();
        drop(registry);
        let reopened = Registry::at(&path).unwrap();
        require_current(&reopened.db).unwrap();
        assert_eq!(reopened.model().unwrap(), model);
        assert_eq!(
            super::super::views::read(&reopened.db, &scope).unwrap(),
            view
        );
        assert_eq!(
            history_preserved_rows(&reopened.db, &names, version),
            before_rows
        );
    }
}
