//! Published channel-work layouts, retained only for store conversion.
//! The running daemon neither reads nor maintains these tables.

use rusqlite::Transaction;

/// Schema 17 added channel work: the PRs a channel declared it was working on,
/// the receipts of those declarations, and each channel's incarnation. Schema
/// 24 drops it with the feature. Every published layout is derived from these
/// steps, so they keep their DDL. An upgrade drops the tables straight after,
/// so they are created empty rather than filled from the channels.
pub(super) const TABLES: &str = "
CREATE TABLE channel_work_homes(
    channel TEXT NOT NULL,
    incarnation INTEGER NOT NULL,
    repository TEXT NOT NULL,
    title TEXT NOT NULL,
    lifecycle TEXT NOT NULL,
    revision TEXT NOT NULL,
    PRIMARY KEY(channel, incarnation)
) STRICT;
CREATE TABLE channel_work_entries(
    channel TEXT NOT NULL,
    incarnation INTEGER NOT NULL,
    target TEXT NOT NULL,
    entry TEXT NOT NULL,
    bucket TEXT NOT NULL CHECK(bucket IN ('ongoing','needs_confirmation','history')),
    sequence INTEGER NOT NULL,
    PRIMARY KEY(channel, incarnation, target),
    FOREIGN KEY(channel, incarnation)
        REFERENCES channel_work_homes(channel, incarnation)
) STRICT;
CREATE TABLE channel_work_operations(
    operation TEXT PRIMARY KEY,
    channel TEXT NOT NULL,
    incarnation INTEGER NOT NULL,
    record TEXT NOT NULL,
    FOREIGN KEY(channel, incarnation)
        REFERENCES channel_work_homes(channel, incarnation)
) STRICT;
CREATE INDEX channel_work_entries_order
    ON channel_work_entries(channel, incarnation, bucket, sequence DESC);";

/// Schema 18's placement generations for channel work's callers. See
/// [`TABLES`].
pub(super) const PLACEMENT: &str = "
CREATE TABLE channel_work_placement_epochs(
    pane TEXT PRIMARY KEY,
    epoch INTEGER NOT NULL CHECK(epoch > 0)
) STRICT;
CREATE TABLE channel_work_placement_clock(
    id INTEGER PRIMARY KEY CHECK(id=1),
    last_epoch INTEGER NOT NULL CHECK(last_epoch>=0)
) STRICT;
INSERT INTO channel_work_placement_clock(id,last_epoch) VALUES(1,0);";

/// Schema 24 retires the feature, including history that used to retain an
/// otherwise empty channel. Each DROP participates in the migration transaction.
pub(super) fn remove(tx: &Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(
        "DROP TABLE channel_work_operations;
         DROP TABLE channel_work_entries;
         DROP TABLE channel_work_homes;
         DROP TABLE channel_work_placement_epochs;
         DROP TABLE channel_work_placement_clock;",
    )
    .map_err(|error| error.to_string())
}

/// Restore schema 23's editable placement invariant during rollback. Its
/// opener repairs homes, but never missing pane epochs. The retired work
/// declarations cannot be recovered; they have no meaning in schema 24.
pub(super) fn restore(tx: &Transaction<'_>) -> Result<(), String> {
    // Refuse a pane that the older writer could not assign a placement to,
    // before publishing any conversion. Normalized keys already guarantee
    // that a pane has exactly one owner.
    for view in super::views::all(tx)? {
        for pane in &view.panes {
            pane.validate()?;
            if pane.id.as_uuid().is_nil() || view.retired.contains(&pane.id) {
                return Err("Cannot restore an ambiguous canonical pane placement".into());
            }
        }
    }
    tx.execute_batch(TABLES)
        .map_err(|error| error.to_string())?;
    tx.execute_batch(PLACEMENT)
        .map_err(|error| error.to_string())?;
    // Like the original placement migration, start at one, give every live
    // pane its own epoch, and leave the clock at the last allocated value.
    // There are no old claims to retain: schema 24 removed their protocol.
    tx.execute_batch(
        "INSERT INTO channel_work_placement_epochs(pane,epoch)
             SELECT id,row_number() OVER (ORDER BY id) FROM panes;
         UPDATE channel_work_placement_clock
             SET last_epoch=(SELECT count(*) FROM panes) WHERE id=1;",
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::{Registry, SCHEMA_VERSION, migrate_through, schema, views};
    use crate::{
        model::{BrowserId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind, ChannelId},
        workspace_model::{
            ModelEdit,
            views::{Change, Pane, Scope, View},
        },
    };
    use rusqlite::{Connection, params, types::Value};
    use std::{collections::BTreeMap, path::Path};
    use uuid::Uuid;

    fn pane() -> Pane {
        Pane {
            id: CardId::new(),
            title: "Preserved browser".into(),
            user_title: Some("User's title".into()),
            pending_terminal_start: false,
            width_fraction: 0.75,
            scope: Some("/repo/0/worktree".into()),
            kind: CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: "https://example.com/kept".parse().unwrap(),
                history: vec![],
                history_index: 0,
                scroll_x: 0.,
                scroll_y: 0.,
                fidelity: BrowserRestoreFidelity::UrlOnly,
            }),
        }
    }

    /// Match the long-lived workspace in docs/persistence.md: 18 projects,
    /// 76 channels, 1856 scopes, 97 open panes and 45,970 operation receipts.
    /// Channel-work declarations include all buckets, receipts and empty views.
    fn published_23(path: &Path) -> (Scope, ChannelId) {
        let mut db = Connection::open(path).unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let tx = db.transaction().unwrap();
        migrate_through(&tx, 0, 23).unwrap();
        tx.execute(
            "UPDATE metadata SET sequence=49123,next_project_ordinal=19,next_channel_ordinal=77",
            [],
        )
        .unwrap();
        for index in 0..18 {
            tx.execute(
                "INSERT INTO projects(repository,name,ordinal,kind) VALUES(?1,?2,?3,'folder')",
                params![
                    format!("/repo/{index}"),
                    format!("Project {index}"),
                    index + 1
                ],
            )
            .unwrap();
        }
        let mut scopes = Vec::new();
        let mut empty_channel = None;
        for index in 0..76 {
            let channel = ChannelId::new();
            let repository = format!("/repo/{}", index % 18);
            tx.execute("INSERT INTO channels(id,repository,title,ordinal,title_source) VALUES(?1,?2,'My channel',?3,1)", params![channel.to_string(),repository,index+1]).unwrap();
            tx.execute(
                "INSERT INTO channel_work_homes VALUES(?1,?2,?3,'My channel','active','3')",
                params![channel.to_string(), index + 1, repository],
            )
            .unwrap();
            for (sequence, bucket) in [(1, "ongoing"), (2, "needs_confirmation"), (3, "history")] {
                tx.execute(
                    "INSERT INTO channel_work_entries VALUES(?1,?2,?3,'{}',?4,?5)",
                    params![
                        channel.to_string(),
                        index + 1,
                        format!("pr-{sequence}"),
                        bucket,
                        sequence
                    ],
                )
                .unwrap();
                tx.execute(
                    "INSERT INTO channel_work_operations VALUES(?1,?2,?3,'{}')",
                    params![Uuid::new_v4().to_string(), channel.to_string(), index + 1],
                )
                .unwrap();
            }
            scopes.push(Scope::Channel(channel));
            empty_channel = Some(channel);
        }
        for index in 0..1780 {
            scopes.push(Scope::Worktree {
                repository: format!("/repo/{}", index % 18).into(),
                path: format!("/repo/{}/worktree-{index}", index % 18).into(),
            });
        }
        for (index, scope) in scopes.iter().enumerate() {
            views::store::register(&tx, scope).unwrap();
            // Leave the last channel empty despite its history, while a few
            // worktrees hold panes as well as the first 75 channels.
            let populated = index < 75 || (76..98).contains(&index);
            views::store::store(
                &tx,
                &View {
                    scope: scope.clone(),
                    revision: index as u64 + 11,
                    panes: if populated { vec![pane()] } else { vec![] },
                    retired: vec![CardId::new()],
                },
            )
            .unwrap();
        }
        tx.execute_batch(
            "UPDATE view_revision_clock SET last=2000;
            INSERT INTO channel_work_placement_epochs(pane,epoch)
                SELECT id,row_number() OVER (ORDER BY id) FROM panes;
            UPDATE channel_work_placement_clock SET last_epoch=(SELECT count(*) FROM panes);
            WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<45970)
            INSERT INTO view_operations(operation,scope,applied_at_unix_ms)
                SELECT printf('%08x-0000-4000-8000-000000000000',i),1+(i%1856),2000000000000 FROM n;
            INSERT INTO native_actor_cursors VALUES('actor','owner',7,X'0001ff');
            INSERT INTO native_actor_ownerships VALUES('actor','ownership receipt');
            INSERT INTO native_actor_retirements VALUES('actor','retirement receipt');
            INSERT INTO navigation_orders VALUES('/repo/0','[]');
            INSERT INTO closed_projects VALUES('/old-project');
            PRAGMA user_version=23;",
        )
        .unwrap();
        tx.commit().unwrap();
        (scopes[0].clone(), empty_channel.unwrap())
    }

    fn rows(db: &Connection) -> BTreeMap<String, Vec<Vec<Value>>> {
        let names: Vec<String> = db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*' AND name NOT GLOB 'channel_work*' ORDER BY name").unwrap().query_map([],|row|row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        names
            .into_iter()
            .map(|name| {
                let mut statement = db.prepare(&format!("SELECT * FROM {name}")).unwrap();
                let width = statement.column_count();
                let mut result: Vec<Vec<Value>> = statement
                    .query_map([], |row| {
                        (0..width)
                            .map(|column| row.get(column))
                            .collect::<rusqlite::Result<_>>()
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                // Debug ordering is only for a stable equality comparison; no SQL
                // interpretation or persistence depends on this representation.
                result.sort_by_cached_key(|row| format!("{row:?}"));
                (name, result)
            })
            .collect()
    }

    fn assert_epochs(db: &Connection, target: u32, expected: i64) {
        let (count,distinct,min,max): (i64,i64,Option<i64>,Option<i64>) = db.query_row("SELECT count(*),count(DISTINCT epoch),min(epoch),max(epoch) FROM channel_work_placement_epochs",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
        assert_eq!((count, distinct), (expected, expected));
        assert_eq!(
            (min, max),
            if expected == 0 {
                (None, None)
            } else {
                (Some(1), Some(expected))
            }
        );
        assert_eq!(
            db.query_row(
                "SELECT last_epoch FROM channel_work_placement_clock WHERE id=1",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            expected
        );
        let pane_table = if target == 23 {
            "panes"
        } else {
            "shared_view_panes"
        };
        assert_eq!(db.query_row(&format!("SELECT count(*) FROM {pane_table} p LEFT JOIN channel_work_placement_epochs e ON e.pane=p.id WHERE e.pane IS NULL"),[],|row|row.get::<_,i64>(0)).unwrap(),0);
        for table in [
            "channel_work_homes",
            "channel_work_entries",
            "channel_work_operations",
        ] {
            assert_eq!(
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            target
        );
    }

    fn sorted(mut views: Vec<View>) -> Vec<View> {
        views.sort_by_cached_key(|view| serde_json::to_string(&view.scope).unwrap());
        views
    }

    #[test]
    fn realistic_migration_and_downgrades_preserve_current_workspace_and_receipts() {
        for target in [23, 22] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("workspace-model.sqlite3");
            let (scope, empty_channel) = published_23(&path);
            let before = rows(&Connection::open(&path).unwrap());
            let mut registry = Registry::at(&path).unwrap();
            schema::require_current(&registry.db).unwrap();
            assert_eq!(rows(&registry.db), before, "only channel work is removed");
            assert_eq!(
                registry
                    .db
                    .query_row(
                        "SELECT count(*) FROM sqlite_schema WHERE name GLOB 'channel_work*'",
                        [],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
            let mut updated = views::read(&registry.db, &scope).unwrap().panes[0].clone();
            let previous = updated.clone();
            updated.title = "Written after schema 24 upgrade".into();
            assert!(
                registry
                    .apply(&ModelEdit::ChangeView {
                        scope: scope.clone(),
                        expected_revision: registry.view_revision(&scope).unwrap(),
                        operation: Uuid::new_v4(),
                        change: Change::Update {
                            before: previous,
                            pane: updated
                        },
                    })
                    .unwrap()
                    .changed
            );
            let model = registry.model().unwrap();
            let empty = ModelEdit::RetireEmptyChannel {
                home: crate::workspace_model::ChannelIncarnation {
                    registry: model.identity,
                    channel: empty_channel,
                    creation_ordinal: 76,
                },
                expected_view_revision: registry
                    .view_revision(&Scope::Channel(empty_channel))
                    .unwrap(),
            };
            assert!(
                registry.apply(&empty).unwrap().changed,
                "old history must not retain an empty channel"
            );
            registry.db.execute("UPDATE native_actor_retirements SET receipt='receipt written after upgrade' WHERE actor='actor'",[]).unwrap();
            let current = rows(&registry.db);
            let views = views::all(&registry.db).unwrap();
            let mut model = registry.model().unwrap();
            model.views = sorted(model.views);
            drop(registry);
            assert_eq!(
                Registry::downgrade_at(&path, target).unwrap(),
                Some(SCHEMA_VERSION)
            );
            let db = Connection::open(&path).unwrap();
            assert_epochs(&db, target, 97);
            assert_eq!(
                db.query_row(
                    "SELECT receipt FROM native_actor_retirements WHERE actor='actor'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
                "receipt written after upgrade"
            );
            if target == 23 {
                assert_eq!(
                    rows(&db),
                    current,
                    "rollback must keep writes made since upgrade"
                );
            }
            drop(db);
            // A further downgrade composes with the view-store converter;
            // re-upgrading keeps its semantic views and all daemon receipts.
            let reopened = Registry::at(&path).unwrap();
            let mut restored_model = reopened.model().unwrap();
            restored_model.views = sorted(restored_model.views);
            assert_eq!(restored_model, model);
            assert_eq!(sorted(views::all(&reopened.db).unwrap()), sorted(views));
            schema::require_current(&reopened.db).unwrap();
        }
    }

    #[test]
    fn empty_store_downgrade_restores_zero_clock_without_inventing_panes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty.sqlite3");
        drop(Registry::at(&path).unwrap());
        Registry::downgrade_at(&path, 23).unwrap();
        assert_epochs(&Connection::open(&path).unwrap(), 23, 0);
    }

    #[test]
    fn invalid_placement_downgrade_rolls_back_without_publishing_old_tables() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid.sqlite3");
        published_23(&path);
        let registry = Registry::at(&path).unwrap();
        registry
            .db
            .execute(
                "UPDATE panes SET id=?1 WHERE id=(SELECT id FROM panes LIMIT 1)",
                [Uuid::nil().to_string()],
            )
            .unwrap();
        let before = rows(&registry.db);
        drop(registry);
        let error = Registry::downgrade_at(&path, 23).unwrap_err();
        assert!(error.contains("ambiguous canonical pane"), "{error}");
        let db = Connection::open(&path).unwrap();
        assert_eq!(rows(&db), before);
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name GLOB 'channel_work*'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
}
