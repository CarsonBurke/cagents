//! Schema 22 <-> 23 conversion, applied-operation retention and the row-level
//! writer, against real stores on disk.
use super::*;
use crate::host::model::{Registry, SCHEMA_VERSION, channel_work, migrate_through};
use crate::model::{BrowserId, BrowserRestoreFidelity, BrowserSession, ChannelId};
use crate::workspace_model::{ModelEdit, views::Change};
use std::path::Path;

fn terminal(title: &str) -> Pane {
    let recipe = crate::model::LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec![],
        working_directory: "/repo".into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    Pane {
        id: CardId::new(),
        title: title.into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        scope: Some("/repo".into()),
        kind: CardKind::Terminal(crate::model::TerminalSession {
            id: crate::model::TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        }),
    }
}

fn browser(title: &str) -> Pane {
    browser_of(title, BrowserId::new())
}

fn browser_of(title: &str, id: BrowserId) -> Pane {
    Pane {
        id: CardId::new(),
        title: title.into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: Some("/repo".into()),
        kind: CardKind::Browser(BrowserSession {
            id,
            current_url: "https://example.com".parse().unwrap(),
            history: vec![],
            history_index: 0,
            scroll_x: 0.,
            scroll_y: 0.,
            fidelity: BrowserRestoreFidelity::UrlOnly,
        }),
    }
}

/// A published schema 22 store with project `/repo` and two of its channels.
fn schema_22(path: &Path) -> (ChannelId, ChannelId) {
    let mut db = Connection::open(path).unwrap();
    let tx = db.transaction().unwrap();
    migrate_through(&tx, 0, 22).unwrap();
    tx.execute(
        "INSERT INTO projects(repository,name,ordinal,kind) VALUES('/repo','repo',1,'folder')",
        [],
    )
    .unwrap();
    let channels = (ChannelId::new(), ChannelId::new());
    for (ordinal, channel) in [(1, channels.0), (2, channels.1)] {
        tx.execute(
            "INSERT INTO channels(id,repository,title,ordinal,title_source) VALUES(?1,'/repo','c',?2,1)",
            params![channel.to_string(), ordinal],
        )
        .unwrap();
        channel_work::created_in(&tx, channel).unwrap();
    }
    tx.pragma_update(None, "user_version", 22).unwrap();
    tx.commit().unwrap();
    channels
}

/// Writes a view the way schema 22's daemon did.
fn put_22(
    db: &Connection,
    scope: &Scope,
    revision: u64,
    panes: &[Pane],
    retired: &[CardId],
    edits: &[Uuid],
) {
    let key = scope_key(scope).unwrap();
    let (channel, repository, _) = columns(scope).unwrap();
    db.execute(
        "INSERT INTO shared_view_scopes(scope,channel,repository) VALUES(?1,?2,?3)",
        params![key, channel, repository],
    )
    .unwrap();
    db.execute(
        "INSERT INTO shared_views(scope,revision,panes,retired) VALUES(?1,?2,?3,?4)",
        params![
            key,
            revision as i64,
            serde_json::to_string(panes).unwrap(),
            serde_json::to_string(retired).unwrap()
        ],
    )
    .unwrap();
    for pane in panes {
        db.execute(
            "INSERT INTO shared_view_panes(id,scope) VALUES(?1,?2)",
            params![pane.id.to_string(), key],
        )
        .unwrap();
        // Schema 22 gave every pane a placement epoch as it was published.
        let epoch: i64 = db
            .query_row(
                "UPDATE channel_work_placement_clock SET last_epoch = last_epoch + 1
                 RETURNING last_epoch",
                [],
                |row| row.get(0),
            )
            .unwrap();
        db.execute(
            "INSERT INTO channel_work_placement_epochs(pane, epoch) VALUES(?1, ?2)",
            params![pane.id.to_string(), epoch],
        )
        .unwrap();
    }
    for edit in edits {
        db.execute(
            "INSERT INTO shared_view_edits(operation,scope) VALUES(?1,?2)",
            params![edit.to_string(), key],
        )
        .unwrap();
    }
}

/// Every schema 22 view row, in a stable order.
fn rows_22(db: &Connection) -> Vec<Vec<String>> {
    [
        "SELECT scope, coalesce(channel, ''), coalesce(repository, '') FROM shared_view_scopes",
        "SELECT scope, revision, panes, retired FROM shared_views",
        "SELECT id, scope, '' FROM shared_view_panes",
        "SELECT operation, scope, '' FROM shared_view_edits",
        "SELECT pane, source, ended FROM shared_view_terminal_sources",
        "SELECT pane, terminal, '' FROM shared_terminal_retention_pins",
    ]
    .iter()
    .flat_map(|query| {
        let mut statement = db.prepare(&format!("{query} ORDER BY 1, 2")).unwrap();
        let width = statement.column_count();
        statement
            .query_map([], |row| {
                (0..width)
                    .map(|column| {
                        row.get::<_, rusqlite::types::Value>(column)
                            .map(|value| match value {
                                rusqlite::types::Value::Text(text) => text,
                                other => format!("{other:?}"),
                            })
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    })
    .collect()
}

fn sorted(mut views: Vec<View>) -> Vec<View> {
    views.sort_by_key(|view| scope_key(&view.scope).unwrap());
    views
}

fn user_version(path: &Path) -> u32 {
    Connection::open(path)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

fn change(registry: &Registry, scope: &Scope, change: Change) -> ModelEdit {
    ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: registry.view_revision(scope).unwrap(),
        operation: Uuid::new_v4(),
        change,
    }
}

#[test]
fn schema_22_views_convert_whole_and_convert_back_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model.sqlite3");
    let (a, b) = schema_22(&path);
    let (a, b) = (Scope::Channel(a), Scope::Channel(b));
    let worktree = Scope::Worktree {
        repository: "/repo".into(),
        path: "/repo/worktree".into(),
    };
    let a_panes: Vec<Pane> = (0..5).map(|index| browser(&format!("a{index}"))).collect();
    let b_panes = vec![browser("b0")];
    // Retirement order is history, not identity order.
    let mut retired: Vec<CardId> = (0..4).map(|_| CardId::new()).collect();
    retired.sort_by_key(|id| std::cmp::Reverse(id.to_string()));
    let edits: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
    {
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        put_22(&db, &a, 41, &a_panes, &retired, &edits[..2]);
        put_22(&db, &b, 7, &b_panes, &[], &edits[2..]);
        put_22(&db, &worktree, 0, &[], &[], &[]);
        db.execute(
            "INSERT INTO shared_view_terminal_sources(pane,source,ended) VALUES(?1,'source','ended')",
            [a_panes[2].id.to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO shared_terminal_retention_pins(pane,terminal) VALUES(?1,'terminal')",
            [a_panes[3].id.to_string()],
        )
        .unwrap();
    }
    let original = rows_22(&Connection::open(&path).unwrap());
    let expected = sorted(vec![
        View {
            scope: a.clone(),
            revision: 41,
            panes: a_panes.clone(),
            retired: retired.clone(),
        },
        View {
            scope: b.clone(),
            revision: 7,
            panes: b_panes.clone(),
            retired: vec![],
        },
        View {
            scope: worktree.clone(),
            revision: 0,
            panes: vec![],
            retired: vec![],
        },
    ]);

    let registry = Registry::at(&path).unwrap();
    assert_eq!(sorted(all(&registry.db).unwrap()), expected);
    for edit in &edits {
        assert!(
            operation_seen(&registry.db, *edit).unwrap(),
            "an applied edit was forgotten"
        );
    }
    let clock: i64 = registry
        .db
        .query_row("SELECT last FROM view_revision_clock", [], |row| row.get(0))
        .unwrap();
    assert_eq!(clock, 41, "the clock must start past every stored revision");
    let owned: (String, String) = registry
        .db
        .query_row(
            "SELECT (SELECT pane FROM shared_view_terminal_sources),
                    (SELECT pane FROM shared_terminal_retention_pins)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        owned,
        (a_panes[2].id.to_string(), a_panes[3].id.to_string())
    );
    drop(registry);

    assert_eq!(Registry::downgrade_at(&path, 22).unwrap(), Some(23));
    assert_eq!(user_version(&path), 22);
    assert_eq!(
        rows_22(&Connection::open(&path).unwrap()),
        original,
        "an unedited store must convert back to exactly the rows schema 22 wrote"
    );
    assert_eq!(Registry::downgrade_at(&path, 22).unwrap(), None);
    let registry = Registry::at(&path).unwrap();
    assert_eq!(sorted(all(&registry.db).unwrap()), expected);
}

#[test]
fn edits_made_under_schema_23_survive_the_conversion_back() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model.sqlite3");
    let (a, b) = schema_22(&path);
    let (a, b) = (Scope::Channel(a), Scope::Channel(b));
    let mut panes: Vec<Pane> = (0..4).map(|index| browser(&format!("a{index}"))).collect();
    panes[1] = terminal("pinned");
    let CardKind::Terminal(session) = &panes[1].kind else {
        unreachable!()
    };
    let pinned_terminal = session.id.to_string();
    {
        let db = Connection::open(&path).unwrap();
        put_22(&db, &a, 3, &panes, &[], &[]);
        db.execute(
            "INSERT INTO shared_terminal_retention_pins(pane,terminal) VALUES(?1,?2)",
            params![panes[1].id.to_string(), pinned_terminal],
        )
        .unwrap();
    }
    let mut registry = Registry::at(&path).unwrap();
    let inserted = browser("first");
    let mut operations = Vec::new();
    for step in [
        Change::Insert {
            pane: inserted.clone(),
            before: Some(panes[0].id),
        },
        Change::Remove { id: panes[3].id },
        Change::Transfer {
            id: panes[1].id,
            destination: b.clone(),
            before: None,
        },
    ] {
        let edit = change(&registry, &a, step);
        let ModelEdit::ChangeView { operation, .. } = &edit else {
            unreachable!()
        };
        operations.push(*operation);
        registry.apply(&edit).unwrap();
    }
    let edited = sorted(all(&registry.db).unwrap());
    let a_view = edited.iter().find(|view| view.scope == a).unwrap();
    assert_eq!(
        a_view.panes,
        vec![inserted.clone(), panes[0].clone(), panes[2].clone()]
    );
    drop(registry);

    assert_eq!(Registry::downgrade_at(&path, 22).unwrap(), Some(23));
    let db = Connection::open(&path).unwrap();
    for view in &edited {
        let (revision, saved, retired): (i64, String, String) = db
            .query_row(
                "SELECT revision, panes, retired FROM shared_views WHERE scope = ?1",
                [scope_key(&view.scope).unwrap()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(revision as u64, view.revision);
        assert_eq!(
            serde_json::from_str::<Vec<Pane>>(&saved).unwrap(),
            view.panes
        );
        assert_eq!(
            serde_json::from_str::<Vec<CardId>>(&retired).unwrap(),
            view.retired
        );
        for pane in &view.panes {
            let owner: String = db
                .query_row(
                    "SELECT scope FROM shared_view_panes WHERE id = ?1",
                    [pane.id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(owner, scope_key(&view.scope).unwrap());
        }
    }
    for operation in &operations {
        let seen: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM shared_view_edits WHERE operation = ?1)",
                [operation.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            seen,
            "an operation applied under 23 must still be recognized by 22"
        );
    }
    // The pin followed its pane across the transfer and back to schema 22.
    let pinned: (String, String) = db
        .query_row(
            "SELECT pane, terminal FROM shared_terminal_retention_pins",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(pinned, (panes[1].id.to_string(), pinned_terminal));
    drop(db);
    let registry = Registry::at(&path).unwrap();
    assert_eq!(sorted(all(&registry.db).unwrap()), edited);
}

/// Conversion refusals leave the published schema 22 store exactly as it was.
fn refuses(path: &Path, reason: &str) {
    let before = rows_22(&Connection::open(path).unwrap());
    let error = Registry::at(path)
        .err()
        .expect("the conversion must refuse");
    assert!(error.contains(reason), "{error}");
    assert_eq!(user_version(path), 22);
    assert_eq!(rows_22(&Connection::open(path).unwrap()), before);
}

/// Lists `pane` in `scope`'s saved array as well, as a damaged schema 22 store
/// could: its index has one row per id, so only the array says so.
fn also_list_22(db: &Connection, scope: &Scope, panes: &[Pane]) {
    db.execute(
        "UPDATE shared_views SET panes = ?2 WHERE scope = ?1",
        params![
            scope_key(scope).unwrap(),
            serde_json::to_string(panes).unwrap()
        ],
    )
    .unwrap();
}

fn epochs(db: &Connection) -> Vec<String> {
    db.prepare("SELECT pane FROM channel_work_placement_epochs ORDER BY pane")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn ids(registry: &Registry, scope: &Scope) -> Vec<CardId> {
    read(&registry.db, scope)
        .unwrap()
        .panes
        .iter()
        .map(|pane| pane.id)
        .collect()
}

/// One edit to `scope`, to show a repaired view still accepts them.
fn still_edits(registry: &mut Registry, scope: &Scope) {
    registry
        .apply(&change(
            registry,
            scope,
            Change::Insert {
                pane: browser("after"),
                before: None,
            },
        ))
        .unwrap();
}

#[test]
fn a_pane_listed_in_two_views_stays_open_in_the_view_schema_22_used() {
    for indexed_in_second in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.sqlite3");
        let (a, b) = schema_22(&path);
        let (a, b) = (Scope::Channel(a), Scope::Channel(b));
        let shared = terminal("shared");
        let CardKind::Terminal(session) = &shared.kind else {
            unreachable!()
        };
        let pinned_terminal = session.id.to_string();
        let own = browser("own");
        {
            let db = Connection::open(&path).unwrap();
            put_22(&db, &a, 3, std::slice::from_ref(&shared), &[], &[]);
            put_22(&db, &b, 5, std::slice::from_ref(&own), &[], &[]);
            also_list_22(&db, &b, &[shared.clone(), own.clone()]);
            if indexed_in_second {
                db.execute(
                    "UPDATE shared_view_panes SET scope = ?2 WHERE id = ?1",
                    params![shared.id.to_string(), scope_key(&b).unwrap()],
                )
                .unwrap();
            }
            db.execute(
                "INSERT INTO shared_terminal_retention_pins(pane,terminal) VALUES(?1,?2)",
                params![shared.id.to_string(), pinned_terminal],
            )
            .unwrap();
            db.execute(
                "INSERT INTO shared_view_terminal_sources(pane,source,ended) VALUES(?1,'source','ended')",
                [shared.id.to_string()],
            )
            .unwrap();
        }
        let mut registry = Registry::at(&path).expect("a damaged store still converts");
        let (kept, left, kept_revision) = if indexed_in_second {
            (&b, &a, 5)
        } else {
            (&a, &b, 3)
        };
        assert!(
            ids(&registry, kept).contains(&shared.id),
            "the pane left the view its index named"
        );
        assert!(!ids(&registry, left).contains(&shared.id));
        assert!(ids(&registry, &b).contains(&own.id));
        // What hung off the pane stays with it.
        let hung: (String, String, String) = registry
            .db
            .query_row(
                "SELECT (SELECT pane FROM shared_terminal_retention_pins WHERE terminal = ?2),
                        (SELECT pane FROM shared_view_terminal_sources),
                        (SELECT pane FROM channel_work_placement_epochs WHERE pane = ?1)",
                params![shared.id.to_string(), pinned_terminal],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let id = shared.id.to_string();
        assert_eq!(hung, (id.clone(), id.clone(), id));
        // The view it left retires it and moves past every stored revision,
        // so a stale window's edit there conflicts instead of vanishing.
        let left_view = read(&registry.db, left).unwrap();
        assert!(left_view.retired.contains(&shared.id));
        assert!(left_view.revision > 5, "{}", left_view.revision);
        assert_eq!(read(&registry.db, kept).unwrap().revision, kept_revision);
        still_edits(&mut registry, kept);
        still_edits(&mut registry, left);

        // Once it closes where it stayed, a stale window cannot seed it back
        // into the view it left.
        registry
            .apply(&change(&registry, kept, Change::Remove { id: shared.id }))
            .unwrap();
        registry
            .apply(&ModelEdit::SeedView {
                scope: left.clone(),
                panes: vec![shared.clone()],
                closed: vec![],
            })
            .unwrap();
        assert!(
            !ids(&registry, left).contains(&shared.id),
            "a closed pane came back"
        );
        assert!(!ids(&registry, kept).contains(&shared.id));
    }
}

#[test]
fn a_pane_listed_twice_in_one_view_is_kept_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model.sqlite3");
    let (a, _) = schema_22(&path);
    let a = Scope::Channel(a);
    let (first, second) = (browser("first"), browser("second"));
    {
        let db = Connection::open(&path).unwrap();
        put_22(&db, &a, 2, &[first.clone(), second.clone()], &[], &[]);
        also_list_22(&db, &a, &[first.clone(), second.clone(), first.clone()]);
    }
    let mut registry = Registry::at(&path).unwrap();
    let view = read(&registry.db, &a).unwrap();
    assert_eq!(view.panes, vec![first, second]);
    assert!(view.retired.is_empty(), "a pane still open was retired");
    still_edits(&mut registry, &a);
}

#[test]
fn a_second_pane_for_one_resource_gives_way_to_the_first_and_cannot_return() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model.sqlite3");
    let (a, b) = schema_22(&path);
    // Schema 22 found a resource's holder in scope order.
    let mut scopes = [Scope::Channel(a), Scope::Channel(b)];
    scopes.sort_by_key(|scope| scope_key(scope).unwrap());
    let [a, b] = scopes;
    let resource = BrowserId::new();
    let (one, two, three) = (
        browser_of("one", resource),
        browser("unrelated"),
        browser_of("three", resource),
    );
    {
        let db = Connection::open(&path).unwrap();
        put_22(&db, &a, 1, &[one.clone(), two.clone()], &[], &[]);
        put_22(&db, &b, 1, std::slice::from_ref(&three), &[], &[]);
    }
    let mut registry = Registry::at(&path).expect("a damaged store still converts");
    assert_eq!(
        read(&registry.db, &a).unwrap().panes,
        vec![one.clone(), two.clone()]
    );
    let gave_way = read(&registry.db, &b).unwrap();
    assert_eq!(gave_way.panes, vec![]);
    assert_eq!(gave_way.retired, vec![three.id]);
    assert!(gave_way.revision > 1);
    assert_eq!(
        epochs(&registry.db),
        {
            let mut kept = vec![one.id.to_string(), two.id.to_string()];
            kept.sort();
            kept
        },
        "the pane that gave way kept a placement"
    );
    still_edits(&mut registry, &a);
    still_edits(&mut registry, &b);
    // Its holder closing does not let a stale window seed it back.
    registry
        .apply(&change(&registry, &a, Change::Remove { id: one.id }))
        .unwrap();
    registry
        .apply(&ModelEdit::SeedView {
            scope: b.clone(),
            panes: vec![three.clone()],
            closed: vec![],
        })
        .unwrap();
    assert!(
        !ids(&registry, &b).contains(&three.id),
        "a closed pane came back"
    );
    drop(registry);

    // Schema 22 gets the repaired views back.
    Registry::downgrade_at(&path, 22).unwrap();
    let db = Connection::open(&path).unwrap();
    let (panes, retired): (String, String) = db
        .query_row(
            "SELECT panes, retired FROM shared_views WHERE scope = ?1",
            [scope_key(&b).unwrap()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let panes: Vec<Pane> = serde_json::from_str(&panes).unwrap();
    let retired: Vec<CardId> = serde_json::from_str(&retired).unwrap();
    assert!(panes.iter().all(|pane| pane.id != three.id));
    assert!(retired.contains(&three.id));
}

#[test]
fn an_unreadable_view_refuses_conversion_and_leaves_the_store_at_22() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model.sqlite3");
    let (a, _) = schema_22(&path);
    {
        let db = Connection::open(&path).unwrap();
        put_22(&db, &Scope::Channel(a), 1, &[], &[], &[]);
        db.execute("UPDATE shared_views SET panes = 'not json'", [])
            .unwrap();
    }
    refuses(&path, "a saved view is unreadable");
}

#[test]
fn an_index_row_for_a_pane_no_view_holds_is_forgotten_with_what_hung_off_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model.sqlite3");
    let (a, _) = schema_22(&path);
    let open = browser("open");
    let gone = CardId::new();
    {
        let db = Connection::open(&path).unwrap();
        put_22(
            &db,
            &Scope::Channel(a),
            2,
            std::slice::from_ref(&open),
            &[],
            &[],
        );
        db.execute(
            "INSERT INTO shared_view_panes(id,scope) VALUES(?1,?2)",
            params![gone.to_string(), scope_key(&Scope::Channel(a)).unwrap()],
        )
        .unwrap();
        for pane in [open.id, gone] {
            db.execute(
                "INSERT INTO shared_terminal_retention_pins(pane,terminal) VALUES(?1,?2)",
                params![pane.to_string(), Uuid::new_v4().to_string()],
            )
            .unwrap();
        }
    }
    let registry = Registry::at(&path).unwrap();
    assert_eq!(
        read(&registry.db, &Scope::Channel(a)).unwrap().panes,
        vec![open.clone()]
    );
    let pinned: Vec<String> = registry
        .db
        .prepare("SELECT pane FROM shared_terminal_retention_pins")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(pinned, vec![open.id.to_string()]);
}

#[test]
fn conversion_back_goes_no_further_than_schema_22() {
    let directory = tempfile::tempdir().unwrap();
    let absent = directory.path().join("absent.sqlite3");
    assert_eq!(Registry::downgrade_at(&absent, 22).unwrap(), None);
    assert!(
        !absent.exists(),
        "converting nothing must not create a store"
    );
    let path = directory.path().join("model.sqlite3");
    drop(Registry::at(&path).unwrap());
    let error = Registry::downgrade_at(&path, 21).unwrap_err();
    assert!(error.contains("only as far as schema 22"), "{error}");
    assert_eq!(user_version(&path), SCHEMA_VERSION);
}

/// What the installer records before a replacement daemon starts, and so what
/// a rollback converts back to.
#[test]
fn the_stored_schema_is_what_the_store_holds_and_nothing_when_there_is_none() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model.sqlite3");
    assert_eq!(Registry::stored_schema_at(&path).unwrap(), None);
    assert!(!path.exists(), "reading the schema must not create a store");
    // A creation that stopped before its first schema is no store either.
    std::fs::File::create(&path).unwrap();
    assert_eq!(Registry::stored_schema_at(&path).unwrap(), None);
    std::fs::remove_file(&path).unwrap();
    schema_22(&path);
    assert_eq!(Registry::stored_schema_at(&path).unwrap(), Some(22));
    drop(Registry::at(&path).unwrap());
    assert_eq!(
        Registry::stored_schema_at(&path).unwrap(),
        Some(SCHEMA_VERSION)
    );
    Registry::downgrade_at(&path, 22).unwrap();
    assert_eq!(Registry::stored_schema_at(&path).unwrap(), Some(22));
}

/// A fresh store with project `/repo` and one channel view.
fn fresh() -> (Registry, tempfile::TempDir, Scope) {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: "/repo".into(),
            name: "repo".into(),
            kind: Default::default(),
        })
        .unwrap();
    let channel = ChannelId::new();
    registry
        .apply(&ModelEdit::CreateChannel {
            id: channel,
            repository: "/repo".into(),
            title: "view".into(),
            title_source: Default::default(),
        })
        .unwrap();
    (registry, directory, Scope::Channel(channel))
}

fn seed(registry: &mut Registry, scope: &Scope, panes: Vec<Pane>) {
    registry
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes,
            closed: vec![],
        })
        .unwrap();
}

fn age(registry: &Registry, operation: Uuid) {
    let changed = registry
        .db
        .execute(
            "UPDATE view_operations SET applied_at_unix_ms = applied_at_unix_ms - ?2 WHERE operation = ?1",
            params![operation.to_string(), OPERATION_RETENTION.as_millis() as i64 + 1],
        )
        .unwrap();
    assert_eq!(changed, 1);
}

#[test]
fn an_operation_is_forgotten_after_retention_and_a_late_resend_meets_a_revision_conflict() {
    let (mut registry, _directory, scope) = fresh();
    let (p, q, r) = (browser("p"), browser("q"), browser("r"));
    seed(&mut registry, &scope, vec![p.clone(), q.clone(), r.clone()]);
    // Moves are not idempotent: applying `old` again would reorder the view.
    let old = change(
        &registry,
        &scope,
        Change::Move {
            id: r.id,
            before: Some(p.id),
        },
    );
    registry.apply(&old).unwrap();
    let recent = change(
        &registry,
        &scope,
        Change::Move {
            id: p.id,
            before: Some(r.id),
        },
    );
    registry.apply(&recent).unwrap();
    let ids = |registry: &Registry| -> Vec<CardId> {
        read(&registry.db, &scope)
            .unwrap()
            .panes
            .iter()
            .map(|pane| pane.id)
            .collect()
    };
    assert_eq!(ids(&registry), vec![p.id, r.id, q.id]);
    let (
        ModelEdit::ChangeView {
            operation: old_id, ..
        },
        ModelEdit::ChangeView {
            operation: recent_id,
            ..
        },
    ) = (&old, &recent)
    else {
        unreachable!()
    };
    // Inside retention a resend is recognized and changes nothing.
    let view = read(&registry.db, &scope).unwrap();
    registry.apply(&old).unwrap();
    assert_eq!(read(&registry.db, &scope).unwrap(), view);

    age(&registry, *old_id);
    assert_eq!(registry.collect_view_operations().unwrap(), 1);
    assert!(!registry.view_operation_seen(*old_id).unwrap());
    assert!(
        registry.view_operation_seen(*recent_id).unwrap(),
        "a recent operation was forgotten"
    );
    assert_eq!(registry.collect_view_operations().unwrap(), 0);
    // Past retention only the revision check stands. Its conflict is
    // retryable, so a client that rebased would apply the move again; that is
    // why windows stop resending long before retention ends (`REPLAY_LIMIT`).
    let error = registry.apply(&old).unwrap_err();
    assert!(error.contains(Registry::VIEW_REVISION_CONFLICT), "{error}");
    assert_eq!(read(&registry.db, &scope).unwrap(), view);
}

#[test]
fn collection_forgets_one_bounded_batch_at_a_time() {
    let (mut registry, _directory, scope) = fresh();
    seed(&mut registry, &scope, vec![]);
    let id = scope_id(&registry.db, &scope).unwrap().unwrap();
    let old = now_unix_ms() - OPERATION_RETENTION.as_millis() as i64 - 1;
    {
        let tx = registry.db.transaction().unwrap();
        for index in 0..PRUNE_BATCH + 1 {
            tx.execute(
                "INSERT INTO view_operations(operation, scope, applied_at_unix_ms) VALUES (?1, ?2, ?3)",
                params![Uuid::new_v4().to_string(), id, old - index],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    assert_eq!(
        registry.collect_view_operations().unwrap(),
        PRUNE_BATCH as usize
    );
    assert_eq!(registry.collect_view_operations().unwrap(), 1);
    assert_eq!(registry.collect_view_operations().unwrap(), 0);
}

#[test]
fn a_view_written_again_after_its_scope_is_gone_never_repeats_a_revision() {
    let (mut registry, _directory, scope) = fresh();
    seed(&mut registry, &scope, vec![browser("first life")]);
    let first = registry.view_revision(&scope).unwrap();
    assert!(first > 0);
    registry
        .db
        .execute(
            "DELETE FROM view_scopes WHERE id = ?1",
            [scope_id(&registry.db, &scope).unwrap().unwrap()],
        )
        .unwrap();
    assert_eq!(registry.view_revision(&scope).unwrap(), 0);
    seed(&mut registry, &scope, vec![browser("second life")]);
    assert!(
        registry.view_revision(&scope).unwrap() > first,
        "a client still holding the first life's revision would accept the second"
    );
}

#[test]
fn a_move_or_retitle_rewrites_only_the_rows_that_changed() {
    let (mut registry, _directory, scope) = fresh();
    seed(
        &mut registry,
        &scope,
        (0..10).map(|index| browser(&index.to_string())).collect(),
    );
    let mut view = read(&registry.db, &scope).unwrap();
    let original_revision = view.revision;
    let inventory = || {
        registry
            .db
            .query_row(
                "SELECT COUNT(*), MIN(before_revision), MAX(after_revision)
                 FROM temp.execution_view_changes",
                [],
                |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, Option<u64>>(1)?,
                        row.get::<_, Option<u64>>(2)?,
                    ))
                },
            )
            .unwrap()
    };
    let moved = view.panes.remove(7);
    view.panes.insert(2, moved);
    view.revision = next_revision(&registry.db).unwrap();
    let before = registry.db.total_changes();
    store(&registry.db, &view).unwrap();
    assert_eq!(
        registry.db.total_changes() - before,
        3,
        "one position, the revision, and one connection-local inventory update"
    );
    assert_eq!(
        inventory(),
        (1, Some(original_revision), Some(view.revision))
    );
    assert_eq!(read(&registry.db, &scope).unwrap(), view);

    view.panes[5].title = "renamed".into();
    view.revision = next_revision(&registry.db).unwrap();
    let before = registry.db.total_changes();
    store(&registry.db, &view).unwrap();
    assert_eq!(
        registry.db.total_changes() - before,
        3,
        "one pane, the revision, and one connection-local inventory update"
    );
    assert_eq!(
        inventory(),
        (1, Some(original_revision), Some(view.revision))
    );
    assert_eq!(read(&registry.db, &scope).unwrap(), view);
}

#[test]
fn the_holder_of_a_resource_is_found_through_its_index() {
    let (mut registry, _directory, scope) = fresh();
    let resource = BrowserId::new();
    let held = browser_of("held", resource);
    seed(&mut registry, &scope, vec![held.clone(), browser("other")]);
    let rival = browser_of("rival", resource);
    assert_eq!(
        resource_holder(&registry.db, &rival).unwrap(),
        Some(held.id)
    );
    assert_eq!(
        resource_holder(&registry.db, &held).unwrap(),
        None,
        "a pane never holds against itself"
    );
    assert_eq!(
        resource_holder(&registry.db, &browser("new")).unwrap(),
        None
    );
    let plan: Vec<String> = registry
        .db
        .prepare("EXPLAIN QUERY PLAN SELECT id FROM panes WHERE kind = ?1 AND resource = ?2 AND id <> ?3")
        .unwrap()
        .query_map(params!["browser", "x", "y"], |row| row.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        plan.iter().any(|line| line.starts_with("SEARCH panes")),
        "{plan:?}"
    );
}

/// Converts a copy of a real schema 22 store forward and back, keeping it in
/// a scratch directory: `CAGENTS_STORE_COPY=/path/to/workspace-model.sqlite3
/// cargo test ... real_store -- --ignored --nocapture`. The named file is only
/// read.
#[test]
#[ignore = "needs CAGENTS_STORE_COPY, a copy of a real schema 22 store"]
fn a_real_store_converts_forward_and_back_without_losing_a_pane() {
    let source = std::env::var_os("CAGENTS_STORE_COPY").expect("CAGENTS_STORE_COPY");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model.sqlite3");
    Connection::open_with_flags(&source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    assert_eq!(
        user_version(&path),
        22,
        "the copy must be a schema 22 store"
    );
    let original = rows_22(&Connection::open(&path).unwrap());
    let expected: Vec<View> = {
        let db = Connection::open(&path).unwrap();
        let mut statement = db
            .prepare("SELECT scope, revision, panes, retired FROM shared_views")
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .unwrap()
            .map(|row| {
                let (scope, revision, panes, retired) = row.unwrap();
                View {
                    scope: serde_json::from_str(&scope).unwrap(),
                    revision: revision as u64,
                    panes: serde_json::from_str(&panes).unwrap(),
                    retired: serde_json::from_str(&retired).unwrap(),
                }
            })
            .collect()
    };
    let edits: i64 = Connection::open(&path)
        .unwrap()
        .query_row("SELECT count(*) FROM shared_view_edits", [], |row| {
            row.get(0)
        })
        .unwrap();
    let size = |path: &Path| std::fs::metadata(path).unwrap().len();
    let before = size(&path);

    let started = std::time::Instant::now();
    let registry = Registry::at(&path).unwrap();
    let converted = started.elapsed();
    assert_eq!(sorted(all(&registry.db).unwrap()), sorted(expected.clone()));
    let operations: i64 = registry
        .db
        .query_row("SELECT count(*) FROM view_operations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(operations, edits);
    let started = std::time::Instant::now();
    registry.model().unwrap();
    let read_model = started.elapsed();
    drop(registry);
    let vacuumed = directory.path().join("vacuumed.sqlite3");
    Connection::open(&path)
        .unwrap()
        .execute("VACUUM INTO ?1", [vacuumed.to_str().unwrap()])
        .unwrap();

    let started = std::time::Instant::now();
    assert_eq!(Registry::downgrade_at(&path, 22).unwrap(), Some(23));
    let back = started.elapsed();
    assert_eq!(rows_22(&Connection::open(&path).unwrap()), original);
    println!(
        "views {} panes {} operations {edits}: converted in {converted:?}, model read in {read_model:?}, \
         converted back in {back:?}; {before} bytes at 22, {} bytes at 23 after VACUUM",
        expected.len(),
        expected.iter().map(|view| view.panes.len()).sum::<usize>(),
        size(&vacuumed),
    );
}
