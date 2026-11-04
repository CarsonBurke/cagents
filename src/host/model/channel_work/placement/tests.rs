use super::*;
use crate::{
    model::{BootId, ChannelId, HostIncarnation, HostSessionId, HostSessionProof, TerminalId},
    workspace_model::views::Scope,
};
use uuid::Uuid;

// Only the selected canonical-view storage boundary is needed here. These are
// private SQLite unit fixtures, not a Registry migration or host/RPC simulation.
fn initialize(db: &Connection) {
    db.execute_batch(
        "CREATE TABLE shared_views(scope TEXT PRIMARY KEY,revision INTEGER NOT NULL,
         panes TEXT NOT NULL,retired TEXT NOT NULL) STRICT;",
    )
    .unwrap();
}
fn memory() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    initialize(&db);
    db
}
fn card(n: u128) -> CardId {
    CardId::from_uuid(Uuid::from_u128(n))
}
fn pane(n: u128) -> Pane {
    Pane {
        id: card(n),
        title: "Fixture conversation".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        scope: None,
        kind: CardKind::Chat(crate::chat::Session {
            id: TerminalId::from_uuid(Uuid::from_u128(1000 + n)),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: crate::model::LaunchRecipe {
                    program: "fixture-codex".into(),
                    arguments: Vec::new(),
                    working_directory: "/fixture".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: Some(HostSessionProof {
                host_session_id: HostSessionId::new("fixture-host"),
                boot_id: BootId::new("fixture-boot"),
                host_incarnation: HostIncarnation::new("fixture-incarnation"),
            }),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        }),
    }
}
fn view(n: u128, panes: Vec<Pane>) -> View {
    View {
        scope: Scope::Channel(ChannelId::from_uuid(Uuid::from_u128(n))),
        revision: 9,
        panes,
        retired: Vec::new(),
    }
}
fn write(db: &Connection, view: &View) {
    db.execute(
        "INSERT INTO shared_views(scope,revision,panes,retired) VALUES(?1,?2,?3,?4)
         ON CONFLICT(scope) DO UPDATE SET revision=excluded.revision,
         panes=excluded.panes,retired=excluded.retired",
        params![
            serde_json::to_string(&view.scope).unwrap(),
            view.revision,
            serde_json::to_string(&view.panes).unwrap(),
            serde_json::to_string(&view.retired).unwrap()
        ],
    )
    .unwrap();
}
fn backfill(db: &mut Connection) {
    let tx = db.transaction().unwrap();
    migrate(&tx).unwrap();
    tx.commit().unwrap();
}
fn publish(db: &mut Connection, before: &View, after: &View) {
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    observe_view_transition(&tx, before, after).unwrap();
    write(&tx, after);
    tx.commit().unwrap();
}
fn replace_source(view: &View) -> View {
    let mut next = view.clone();
    let CardKind::Chat(session) = &mut next.panes[0].kind else {
        panic!("chat fixture")
    };
    session.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("replacement-host");
    next.revision += 1;
    next
}

#[test]
fn migration_backfills_all_current_panes_and_keeps_tokens_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.sqlite3");
    let mut db = Connection::open(&path).unwrap();
    initialize(&db);
    let a = view(10, vec![pane(1), pane(2)]);
    let b = view(11, vec![pane(3)]);
    write(&db, &a);
    write(&db, &b);
    backfill(&mut db);
    let tokens = [card(1), card(2), card(3)].map(|id| token(&db, id).unwrap());
    assert!(
        tokens
            .iter()
            .all(|value| (1..=i64::MAX as u64).contains(value))
    );
    assert_eq!(
        tokens
            .into_iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    assert_eq!(read_clock(&db).unwrap(), 3);
    assert!(token(&db, card(99)).is_err());
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM channel_work_placement_epochs",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        3
    );
    assert_eq!(
        db.prepare("PRAGMA foreign_key_list(channel_work_placement_epochs)")
            .unwrap()
            .query_map([], |_| Ok(()))
            .unwrap()
            .count(),
        0,
        "canonical transaction hooks own the active mapping lifetime"
    );
    drop(db);
    let db = Connection::open(path).unwrap();
    assert_eq!(
        tokens,
        [card(1), card(2), card(3)].map(|id| token(&db, id).unwrap())
    );
}

#[test]
fn presentation_reorder_and_unrelated_panes_preserve_the_actor_token() {
    let mut db = memory();
    let original = view(10, vec![pane(1), pane(2)]);
    write(&db, &original);
    backfill(&mut db);
    let captured = token(&db, card(1)).unwrap();
    let other = token(&db, card(2)).unwrap();
    let mut next = replace_source(&view(10, vec![pane(2)]));
    let changed_other = next.panes.remove(0);
    next = original.clone();
    next.panes[0].title = "A current title".into();
    next.panes[0].user_title = Some("Explicit name".into());
    next.panes[0].width_fraction = 0.6;
    let CardKind::Chat(session) = &mut next.panes[0].kind else {
        panic!()
    };
    session.launch.session_id = Some("learned-thread".into());
    next.panes[1] = changed_other;
    next.panes.reverse();
    next.panes.push(pane(3));
    next.revision += 1;
    publish(&mut db, &original, &next);
    assert_eq!(token(&db, card(1)).unwrap(), captured);
    assert_ne!(token(&db, card(2)).unwrap(), other);
    assert!(token(&db, card(3)).is_ok());
    let mut removed_other = next.clone();
    removed_other.panes.retain(|pane| pane.id == card(1));
    removed_other.revision += 1;
    publish(&mut db, &next, &removed_other);
    assert_eq!(token(&db, card(1)).unwrap(), captured);
    publish(&mut db, &removed_other, &removed_other);
    assert_eq!(
        token(&db, card(1)).unwrap(),
        captured,
        "a no-op cannot rotate"
    );
}

#[test]
fn transfer_away_and_back_cannot_recreate_the_original_placement_token() {
    let mut db = memory();
    let a = view(10, vec![pane(1)]);
    let b = view(11, Vec::new());
    write(&db, &a);
    write(&db, &b);
    backfill(&mut db);
    let captured = token(&db, card(1)).unwrap();
    let mut empty_a = a.clone();
    empty_a.panes.clear();
    empty_a.retired.push(card(1));
    let mut occupied_b = b.clone();
    occupied_b.panes = a.panes.clone();
    let tx = db.transaction().unwrap();
    observe_view_transition(&tx, &a, &empty_a).unwrap();
    assert!(
        token(&tx, card(1)).is_err(),
        "source removed before target publication"
    );
    observe_view_transition(&tx, &b, &occupied_b).unwrap();
    write(&tx, &empty_a);
    write(&tx, &occupied_b);
    tx.commit().unwrap();
    let moved = token(&db, card(1)).unwrap();
    assert!(moved > captured);
    let mut returned_a = empty_a.clone();
    returned_a.retired.clear();
    returned_a.panes = a.panes.clone();
    let mut empty_b = occupied_b.clone();
    empty_b.panes.clear();
    empty_b.retired.push(card(1));
    let tx = db.transaction().unwrap();
    observe_view_transition(&tx, &occupied_b, &empty_b).unwrap();
    assert!(token(&tx, card(1)).is_err());
    observe_view_transition(&tx, &empty_a, &returned_a).unwrap();
    write(&tx, &empty_b);
    write(&tx, &returned_a);
    tx.commit().unwrap();
    assert_eq!(returned_a.panes, a.panes);
    let returned = token(&db, card(1)).unwrap();
    assert!(returned > moved);
    assert_ne!(returned, captured);
    // A new target may not appropriate a still-active source mapping. Even
    // its tentative allocator progress must roll back with the failed insert.
    let clock = read_clock(&db).unwrap();
    let tx = db.transaction().unwrap();
    assert!(observe_view_transition(&tx, &b, &occupied_b).is_err());
    tx.rollback().unwrap();
    assert_eq!(token(&db, card(1)).unwrap(), returned);
    assert_eq!(read_clock(&db).unwrap(), clock);
    // A single canonical before/after scope change also rotates its epoch.
    let tx = db.transaction().unwrap();
    observe_view_transition(&tx, &returned_a, &occupied_b).unwrap();
    assert!(token(&tx, card(1)).unwrap() > returned);
    tx.rollback().unwrap();
    assert_eq!(token(&db, card(1)).unwrap(), returned);
    assert_eq!(read_clock(&db).unwrap(), clock);
}

#[test]
fn replaced_source_and_eligibility_changes_do_not_revive_old_tokens() {
    let mut db = memory();
    let original = view(10, vec![pane(1)]);
    write(&db, &original);
    backfill(&mut db);
    let captured = token(&db, card(1)).unwrap();
    let replacement = replace_source(&original);
    publish(&mut db, &original, &replacement);
    let replaced = token(&db, card(1)).unwrap();
    assert_ne!(replaced, captured);
    publish(&mut db, &replacement, &original);
    let returned = token(&db, card(1)).unwrap();
    assert_ne!(returned, captured);
    assert_ne!(returned, replaced);
    let mut pending = original.clone();
    pending.panes[0].pending_terminal_start = true;
    publish(&mut db, &original, &pending);
    let pending_token = token(&db, card(1)).unwrap();
    assert_ne!(pending_token, returned);
    let mut scoped = pending.clone();
    scoped.panes[0].scope = Some("/other-fixture-directory".into());
    publish(&mut db, &pending, &scoped);
    let scoped_token = token(&db, card(1)).unwrap();
    assert_ne!(scoped_token, pending_token);
    let mut other_kind = scoped.clone();
    other_kind.panes[0].kind = CardKind::Browser(crate::model::BrowserSession {
        id: crate::model::BrowserId::new(),
        current_url: "https://example.test/fixture".parse().unwrap(),
        history: Vec::new(),
        history_index: 0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
    });
    publish(&mut db, &scoped, &other_kind);
    let replaced_kind = token(&db, card(1)).unwrap();
    assert_ne!(replaced_kind, scoped_token);
    publish(&mut db, &other_kind, &scoped);
    assert_ne!(token(&db, card(1)).unwrap(), scoped_token);
    assert_ne!(token(&db, card(1)).unwrap(), replaced_kind);
}

#[test]
fn removal_deletes_active_mapping_and_same_id_reinsertion_gets_a_new_epoch() {
    let mut db = memory();
    let original = view(10, vec![pane(1)]);
    write(&db, &original);
    backfill(&mut db);
    let captured = token(&db, card(1)).unwrap();
    let clock = read_clock(&db).unwrap();
    let mut removed = original.clone();
    removed.panes.clear();
    removed.retired.push(card(1));
    publish(&mut db, &original, &removed);
    assert!(token(&db, card(1)).is_err());
    assert_eq!(
        read_clock(&db).unwrap(),
        clock,
        "removal needs no allocation"
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM channel_work_placement_epochs",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    db.execute("DELETE FROM shared_views", []).unwrap();
    assert!(token(&db, card(1)).is_err());
    let empty = view(10, Vec::new());
    publish(&mut db, &empty, &original);
    let returned = token(&db, card(1)).unwrap();
    assert!(returned > captured);
    assert_eq!(read_clock(&db).unwrap(), returned as i64);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM channel_work_placement_epochs",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn independent_wal_writer_publishes_epochs_and_rollback_preserves_both_tables() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.sqlite3");
    let mut reader = Connection::open(&path).unwrap();
    reader.pragma_update(None, "journal_mode", "WAL").unwrap();
    initialize(&reader);
    let original = view(10, vec![pane(1)]);
    write(&reader, &original);
    backfill(&mut reader);
    let captured = token(&reader, card(1)).unwrap();
    let initial_clock = read_clock(&reader).unwrap();
    let mut writer = Connection::open(path).unwrap();
    let mut presentation = original.clone();
    presentation.panes[0].title = "Independent window title".into();
    presentation.revision += 1;
    publish(&mut writer, &original, &presentation);
    assert_eq!(token(&reader, card(1)).unwrap(), captured);
    assert_eq!(read_clock(&reader).unwrap(), initial_clock);
    let replacement = replace_source(&presentation);
    {
        let tx = writer.transaction().unwrap();
        observe_view_transition(&tx, &presentation, &replacement).unwrap();
        write(&tx, &replacement);
        assert!(token(&tx, card(1)).unwrap() > captured);
        assert!(read_clock(&tx).unwrap() > initial_clock);
        // Dropping the transaction represents a later failed canonical write.
    }
    assert_eq!(token(&reader, card(1)).unwrap(), captured);
    assert_eq!(read_clock(&reader).unwrap(), initial_clock);
    assert_eq!(
        super::super::super::views::all(&reader).unwrap(),
        vec![presentation.clone()]
    );
    publish(&mut writer, &presentation, &replacement);
    let current = token(&reader, card(1)).unwrap();
    assert!(current > captured);
    let mut removed = replacement.clone();
    removed.panes.clear();
    publish(&mut writer, &replacement, &removed);
    assert!(token(&reader, card(1)).is_err());
    assert_eq!(read_clock(&reader).unwrap(), current as i64);
    publish(&mut writer, &removed, &replacement);
    let returned = token(&reader, card(1)).unwrap();
    assert!(returned > current);
    drop(writer);
    drop(reader);
    let reopened = Connection::open(directory.path().join("fixture.sqlite3")).unwrap();
    assert_eq!(token(&reopened, card(1)).unwrap(), returned);
    assert_eq!(read_clock(&reopened).unwrap(), returned as i64);
}

#[test]
fn missing_malformed_and_exhausted_epochs_refuse_without_partial_publication() {
    let mut db = memory();
    let original = view(10, vec![pane(1), pane(2)]);
    write(&db, &original);
    backfill(&mut db);
    assert!(observe_view_transition(&db, &original, &replace_source(&original)).is_err());
    db.execute(
        "DELETE FROM channel_work_placement_epochs WHERE pane=?1",
        [card(1).to_string()],
    )
    .unwrap();
    let tx = db.transaction().unwrap();
    assert!(observe_view_transition(&tx, &original, &original).is_err());
    assert!(observe_view_transition(&tx, &original, &replace_source(&original)).is_err());
    tx.rollback().unwrap();
    assert!(
        token(&db, card(1)).is_err(),
        "missing existing rows are never lazily seeded"
    );
    db.execute(
        "INSERT INTO channel_work_placement_epochs VALUES(?1,1)",
        [card(1).to_string()],
    )
    .unwrap();
    db.execute(
        "UPDATE channel_work_placement_clock SET last_epoch=?1",
        [i64::MAX - 1],
    )
    .unwrap();
    let first = token(&db, card(1)).unwrap();
    let second = token(&db, card(2)).unwrap();
    let clock = read_clock(&db).unwrap();
    let mut changed = replace_source(&original);
    changed.panes[1].pending_terminal_start = true;
    {
        let tx = db.transaction().unwrap();
        // Card1 can take the last value; card2 then exhausts the singleton.
        // Drop must roll back the earlier allocation and its mapping update.
        assert!(observe_view_transition(&tx, &original, &changed).is_err());
        assert_eq!(read_clock(&tx).unwrap(), i64::MAX);
        assert_eq!(token(&tx, card(1)).unwrap(), i64::MAX as u64);
        assert_eq!(token(&tx, card(2)).unwrap(), second);
    }
    assert_eq!(token(&db, card(1)).unwrap(), first);
    assert_eq!(token(&db, card(2)).unwrap(), second);
    assert_eq!(read_clock(&db).unwrap(), clock);
    assert_eq!(
        super::super::super::views::all(&db).unwrap(),
        vec![original.clone()]
    );
    db.execute(
        "UPDATE channel_work_placement_clock SET last_epoch=?1",
        [i64::MAX],
    )
    .unwrap();
    db.execute(
        "UPDATE channel_work_placement_epochs SET epoch=?1 WHERE pane=?2",
        params![i64::MAX, card(2).to_string()],
    )
    .unwrap();
    assert_eq!(token(&db, card(2)).unwrap(), i64::MAX as u64);
    let tx = db.transaction().unwrap();
    assert!(observe_view_transition(&tx, &original, &changed).is_err());
    tx.rollback().unwrap();
    assert_eq!(read_clock(&db).unwrap(), i64::MAX);
    for value in ["0", "-1", "1.5", "'not-an-integer'", "NULL", "6"] {
        let mut corrupt = Connection::open_in_memory().unwrap();
        corrupt.execute_batch("CREATE TABLE channel_work_placement_epochs(pane TEXT PRIMARY KEY,epoch);
            CREATE TABLE channel_work_placement_clock(id INTEGER PRIMARY KEY CHECK(id=1),last_epoch INTEGER NOT NULL CHECK(last_epoch>=0)) STRICT;
            INSERT INTO channel_work_placement_clock VALUES(1,5);").unwrap();
        corrupt
            .execute(
                &format!("INSERT INTO channel_work_placement_epochs VALUES(?1,{value})"),
                [card(1).to_string()],
            )
            .unwrap();
        assert!(
            token(&corrupt, card(1)).is_err(),
            "malformed or beyond-clock epoch {value}"
        );
        let existing = view(10, vec![pane(1)]);
        let tx = corrupt.transaction().unwrap();
        assert!(
            observe_view_transition(&tx, &existing, &existing).is_err(),
            "unchanged epoch {value}"
        );
    }
}

#[test]
fn migration_refuses_ambiguous_or_invalid_panes_and_rolls_back_its_table() {
    let mut db = memory();
    let original = view(10, vec![pane(1)]);
    write(&db, &original);
    let ambiguous = view(11, original.panes.clone());
    write(&db, &ambiguous);
    {
        let tx = db.transaction().unwrap();
        assert!(migrate(&tx).is_err());
    }
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN ('channel_work_placement_epochs','channel_work_placement_clock')",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    db.execute(
        "DELETE FROM shared_views WHERE scope=?1",
        [serde_json::to_string(&ambiguous.scope).unwrap()],
    )
    .unwrap();
    let mut invalid = original.clone();
    invalid.panes[0].id = card(0);
    write(&db, &invalid);
    {
        let tx = db.transaction().unwrap();
        assert!(migrate(&tx).is_err());
    }
    write(&db, &original);
    backfill(&mut db);
    let captured = token(&db, card(1)).unwrap();
    let mut duplicate = original.clone();
    duplicate.panes.push(pane(1));
    let tx = db.transaction().unwrap();
    assert!(observe_view_transition(&tx, &original, &duplicate).is_err());
    tx.rollback().unwrap();
    assert_eq!(token(&db, card(1)).unwrap(), captured);
}

#[test]
fn missing_or_corrupt_clock_refuses_tokens_and_even_empty_transitions() {
    let mut missing = memory();
    missing.execute_batch("CREATE TABLE channel_work_placement_epochs(pane TEXT PRIMARY KEY,epoch INTEGER NOT NULL CHECK(epoch>0)) STRICT;").unwrap();
    assert!(token(&missing, card(1)).is_err());
    let empty = view(10, Vec::new());
    let tx = missing.transaction().unwrap();
    assert!(observe_view_transition(&tx, &empty, &empty).is_err());
    tx.rollback().unwrap();
    for rows in [
        "",
        "(1,-1)",
        "(1,NULL)",
        "(1,'bad')",
        "(1,1.5)",
        "(2,1)",
        "('bad',1)",
        "(1,1),(2,2)",
        "(1,1),(1,2)",
    ] {
        let mut corrupt = Connection::open_in_memory().unwrap();
        corrupt.execute_batch("CREATE TABLE channel_work_placement_epochs(pane TEXT PRIMARY KEY,epoch INTEGER NOT NULL CHECK(epoch>0)) STRICT;
            CREATE TABLE channel_work_placement_clock(id,last_epoch);").unwrap();
        corrupt
            .execute(
                "INSERT INTO channel_work_placement_epochs VALUES(?1,1)",
                [card(1).to_string()],
            )
            .unwrap();
        if !rows.is_empty() {
            corrupt
                .execute_batch(&format!(
                    "INSERT INTO channel_work_placement_clock VALUES {rows};"
                ))
                .unwrap();
        }
        assert!(token(&corrupt, card(1)).is_err(), "bad singleton {rows}");
        let existing = view(10, vec![pane(1)]);
        let tx = corrupt.transaction().unwrap();
        assert!(observe_view_transition(&tx, &existing, &existing).is_err());
        assert!(observe_view_transition(&tx, &empty, &empty).is_err());
    }
}

#[test]
fn many_same_and_distinct_closed_panes_keep_only_current_active_mappings() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.sqlite3");
    let mut db = Connection::open(&path).unwrap();
    initialize(&db);
    let anchor = view(10, vec![pane(2)]);
    write(&db, &anchor);
    backfill(&mut db);
    let retained = token(&db, card(2)).unwrap();
    let mut last = retained;
    for round in 0..64 {
        for id in [1, 100 + round] {
            let mut occupied = anchor.clone();
            occupied.panes.push(pane(id));
            publish(&mut db, &anchor, &occupied);
            let current = token(&db, card(id)).unwrap();
            assert!(
                current > last,
                "fresh global epoch even for repeated sameCardId"
            );
            last = current;
            assert_eq!(token(&db, card(2)).unwrap(), retained);
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM channel_work_placement_epochs",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                occupied.panes.len() as i64
            );
            publish(&mut db, &occupied, &anchor);
            assert!(token(&db, card(id)).is_err());
            assert_eq!(token(&db, card(2)).unwrap(), retained);
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM channel_work_placement_epochs",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                anchor.panes.len() as i64
            );
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM channel_work_placement_clock",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                1
            );
        }
    }
    assert_eq!(read_clock(&db).unwrap(), last as i64);
    drop(db);
    let reopened = Connection::open(path).unwrap();
    assert_eq!(read_clock(&reopened).unwrap(), last as i64);
    assert_eq!(token(&reopened, card(2)).unwrap(), retained);
    assert!(token(&reopened, card(1)).is_err());
    assert_eq!(
        reopened
            .query_row(
                "SELECT count(*) FROM channel_work_placement_epochs",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}
