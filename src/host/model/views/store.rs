//! Row storage for shared workspace views.
//!
//! A view is a `view_scopes` row, one `panes` row per pane and one
//! `retired_panes` row per retired id. Fields that are keyed, joined, ordered
//! or edited on their own are columns; the pane's `CardKind` is the descriptor,
//! a launch recipe or session proof that is only ever read and replaced whole.
//! The columns derived from it (`kind`, `resource`, `provider`,
//! `provider_session`) are written with it; `kind` and `resource` are checked
//! against it on read.
//!
//! Callers keep editing a [`View`] in memory; [`store`] persists the rows that
//! differ from the stored view, so an edit costs rows in proportion to what it
//! changed, not to the scope or the store.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::{
    model::{CardId, CardKind},
    workspace_model::views::{Pane, Scope, View},
};

/// The only descriptor format so far: `CardKind` as serde JSON. It changes
/// only when a field changes meaning; a reader refuses a format it does not
/// know rather than guess.
pub(in crate::host::model) const DESCRIPTOR_FORMAT: i64 = 1;

/// Gap between neighbors when positions are assigned afresh.
const POSITION_STEP: i64 = 1 << 20;

pub(in crate::host::model) use crate::workspace_model::views::OPERATION_RETENTION;

/// Rows removed by one collection pass. A backlog drains over several passes
/// instead of holding the writer for one long transaction.
const PRUNE_BATCH: i64 = 5_000;

pub(in crate::host::model) const VIEW_SCOPES: &str = "CREATE TABLE view_scopes(
    id INTEGER PRIMARY KEY,
    channel TEXT UNIQUE REFERENCES channels(id) ON DELETE CASCADE,
    repository TEXT REFERENCES projects(repository) ON DELETE CASCADE,
    worktree TEXT,
    revision INTEGER NOT NULL CHECK(revision >= 0),
    UNIQUE(repository, worktree),
    CHECK((channel IS NULL) = (repository IS NOT NULL)),
    CHECK((repository IS NULL) = (worktree IS NULL))
) STRICT";
pub(in crate::host::model) const VIEW_REVISION_CLOCK: &str = "CREATE TABLE view_revision_clock(
    id INTEGER PRIMARY KEY CHECK(id = 1),
    last INTEGER NOT NULL CHECK(last >= 0)
) STRICT";
pub(in crate::host::model) const PANES: &str = "CREATE TABLE panes(
    id TEXT PRIMARY KEY,
    scope INTEGER NOT NULL REFERENCES view_scopes(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    title TEXT NOT NULL,
    user_title TEXT,
    width_fraction REAL NOT NULL CHECK(width_fraction > 0),
    pending_terminal_start INTEGER NOT NULL CHECK(pending_terminal_start IN (0, 1)),
    filing TEXT,
    kind TEXT NOT NULL CHECK(kind IN ('terminal', 'chat', 'browser', 'pull_requests', 'repository')),
    resource TEXT,
    provider TEXT,
    provider_session TEXT,
    descriptor_format INTEGER NOT NULL,
    descriptor TEXT NOT NULL,
    UNIQUE(kind, resource)
) STRICT";
pub(in crate::host::model) const PANES_BY_SCOPE: &str =
    "CREATE INDEX panes_by_scope ON panes(scope, position, id)";
pub(in crate::host::model) const RETIRED_PANES: &str = "CREATE TABLE retired_panes(
    scope INTEGER NOT NULL REFERENCES view_scopes(id) ON DELETE CASCADE,
    pane TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    retired_at_unix_ms INTEGER NOT NULL,
    PRIMARY KEY(scope, pane)
) STRICT, WITHOUT ROWID";
pub(in crate::host::model) const VIEW_OPERATIONS: &str = "CREATE TABLE view_operations(
    operation TEXT PRIMARY KEY,
    scope INTEGER NOT NULL REFERENCES view_scopes(id) ON DELETE CASCADE,
    applied_at_unix_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID";
pub(in crate::host::model) const VIEW_OPERATIONS_BY_AGE: &str =
    "CREATE INDEX view_operations_by_age ON view_operations(applied_at_unix_ms)";
/// Pane-owned rows that schema 23 points at `panes` instead of the ownership
/// index. Their columns are unchanged.
pub(in crate::host::model) const TERMINAL_SOURCES: &str =
    "CREATE TABLE shared_view_terminal_sources(
    pane TEXT PRIMARY KEY REFERENCES panes(id) ON DELETE CASCADE,
    source TEXT NOT NULL, ended TEXT NOT NULL
) STRICT";
pub(in crate::host::model) const RETENTION_PINS: &str =
    "CREATE TABLE shared_terminal_retention_pins(
    pane TEXT PRIMARY KEY REFERENCES panes(id) ON DELETE CASCADE,
    terminal TEXT NOT NULL
) STRICT";
pub(in crate::host::model) const RETENTION_PINS_BY_TERMINAL: &str =
    "CREATE INDEX shared_terminal_retention_by_terminal
    ON shared_terminal_retention_pins(terminal)";

pub(in crate::host::model) fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn sql(error: rusqlite::Error) -> String {
    error.to_string()
}

/// The columns that name a scope: `(channel, repository, worktree)`.
fn columns(scope: &Scope) -> Result<(Option<String>, Option<&str>, Option<&str>), String> {
    Ok(match scope {
        Scope::Channel(id) => (Some(id.to_string()), None, None),
        Scope::Worktree { repository, path } => {
            (
                None,
                Some(super::repository_key(repository)?),
                Some(path.to_str().ok_or_else(|| {
                    format!("worktree path `{}` is not valid UTF-8", path.display())
                })?),
            )
        }
    })
}

fn scope_from(
    channel: Option<String>,
    repository: Option<String>,
    worktree: Option<String>,
) -> Result<Scope, String> {
    match (channel, repository, worktree) {
        (Some(channel), None, None) => Ok(Scope::Channel(super::parse_channel_id(&channel)?)),
        (None, Some(repository), Some(path)) => Ok(Scope::Worktree {
            repository: PathBuf::from(repository),
            path: PathBuf::from(path),
        }),
        _ => Err("stored view scope names neither a channel nor a worktree".into()),
    }
}

pub(in crate::host::model) fn scope_id(
    db: &Connection,
    scope: &Scope,
) -> Result<Option<i64>, String> {
    let (channel, repository, worktree) = columns(scope)?;
    match channel {
        Some(channel) => db
            .prepare_cached("SELECT id FROM view_scopes WHERE channel = ?1")
            .and_then(|mut statement| statement.query_row([channel], |row| row.get(0)).optional()),
        None => db
            .prepare_cached("SELECT id FROM view_scopes WHERE repository = ?1 AND worktree = ?2")
            .and_then(|mut statement| {
                statement
                    .query_row(params![repository, worktree], |row| row.get(0))
                    .optional()
            }),
    }
    .map_err(sql)
}

/// The scope's row, created at revision 0 when absent. A view that has never
/// been written is still empty at revision 0, as it was before it existed.
pub(in crate::host::model) fn register(db: &Connection, scope: &Scope) -> Result<i64, String> {
    if let Some(id) = scope_id(db, scope)? {
        return Ok(id);
    }
    let (channel, repository, worktree) = columns(scope)?;
    db.prepare_cached(
        "INSERT INTO view_scopes(channel, repository, worktree, revision) VALUES (?1, ?2, ?3, 0)",
    )
    .and_then(|mut statement| statement.execute(params![channel, repository, worktree]))
    .map_err(sql)?;
    Ok(db.last_insert_rowid())
}

/// The next model-wide view revision. Every scope draws from one clock, so a
/// scope that is deleted and created again never repeats a revision that a
/// stale client still holds for its earlier life.
pub(in crate::host::model) fn next_revision(db: &Connection) -> Result<u64, String> {
    db.prepare_cached("UPDATE view_revision_clock SET last = last + 1 WHERE id = 1 RETURNING last")
        .and_then(|mut statement| statement.query_row([], |row| row.get::<_, i64>(0)))
        .map(|last| last as u64)
        .map_err(sql)
}

struct Row {
    id: String,
    position: i64,
    title: String,
    user_title: Option<String>,
    width_fraction: f64,
    pending_terminal_start: bool,
    filing: Option<String>,
    kind: String,
    resource: Option<String>,
    descriptor_format: i64,
    descriptor: String,
}

const ROW_COLUMNS: &str = "id, position, title, user_title, width_fraction, \
    pending_terminal_start, filing, kind, resource, descriptor_format, descriptor";

fn row(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Row> {
    Ok(Row {
        id: row.get(offset)?,
        position: row.get(offset + 1)?,
        title: row.get(offset + 2)?,
        user_title: row.get(offset + 3)?,
        width_fraction: row.get(offset + 4)?,
        pending_terminal_start: row.get::<_, i64>(offset + 5)? != 0,
        filing: row.get(offset + 6)?,
        kind: row.get(offset + 7)?,
        resource: row.get(offset + 8)?,
        descriptor_format: row.get(offset + 9)?,
        descriptor: row.get(offset + 10)?,
    })
}

impl Row {
    fn pane(self) -> Result<Pane, String> {
        if self.descriptor_format != DESCRIPTOR_FORMAT {
            return Err(format!(
                "saved pane {} has descriptor format {}, which this version cannot read",
                self.id, self.descriptor_format
            ));
        }
        let kind: CardKind = serde_json::from_str(&self.descriptor)
            .map_err(|error| format!("saved pane {} is unreadable: {error}", self.id))?;
        let derived = Derived::of(&kind)?;
        if derived.kind != self.kind || derived.resource != self.resource {
            return Err(format!(
                "saved pane {} disagrees with its own descriptor",
                self.id
            ));
        }
        Ok(Pane {
            id: self
                .id
                .parse()
                .map_err(|error| format!("saved pane id `{}` is unreadable: {error}", self.id))?,
            title: self.title,
            user_title: self.user_title,
            pending_terminal_start: self.pending_terminal_start,
            width_fraction: self.width_fraction as f32,
            kind,
            scope: self.filing.map(PathBuf::from),
        })
    }
}

/// The columns a descriptor determines.
#[derive(PartialEq)]
struct Derived {
    kind: &'static str,
    resource: Option<String>,
    provider: Option<String>,
    provider_session: Option<String>,
}

impl Derived {
    fn of(kind: &CardKind) -> Result<Self, String> {
        Ok(match kind {
            CardKind::Terminal(session) => Self {
                kind: "terminal",
                resource: Some(session.id.to_string()),
                provider: None,
                provider_session: None,
            },
            CardKind::Chat(session) => Self {
                kind: "chat",
                resource: Some(session.id.to_string()),
                provider: Some(
                    serde_json::to_value(session.launch.provider)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .ok_or("chat provider has no stored name")?,
                ),
                provider_session: session.launch.session_id.clone(),
            },
            CardKind::Browser(session) => Self {
                kind: "browser",
                resource: Some(session.id.to_string()),
                provider: None,
                provider_session: None,
            },
            CardKind::PullRequests(_) => Self {
                kind: "pull_requests",
                resource: None,
                provider: None,
                provider_session: None,
            },
            CardKind::Repository(_) => Self {
                kind: "repository",
                resource: None,
                provider: None,
                provider_session: None,
            },
            _ => return Err("utility panes belong to the client window".into()),
        })
    }
}

fn retired_of(db: &Connection, scope: i64) -> Result<Vec<CardId>, String> {
    let mut statement = db
        .prepare_cached("SELECT pane FROM retired_panes WHERE scope = ?1 ORDER BY ordinal")
        .map_err(sql)?;
    let ids = statement
        .query_map([scope], |row| row.get::<_, String>(0))
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .map_err(sql)?;
    ids.into_iter()
        .map(|id| {
            id.parse()
                .map_err(|error| format!("retired pane id `{id}` is unreadable: {error}"))
        })
        .collect()
}

fn panes_of(db: &Connection, scope: i64) -> Result<Vec<(i64, Pane)>, String> {
    let mut statement = db
        .prepare_cached(&format!(
            "SELECT {ROW_COLUMNS} FROM panes WHERE scope = ?1 ORDER BY position, id"
        ))
        .map_err(sql)?;
    let rows = statement
        .query_map([scope], |r| row(r, 0))
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .map_err(sql)?;
    rows.into_iter()
        .map(|row| Ok((row.position, row.pane()?)))
        .collect()
}

/// The stored view, or an empty one at revision 0 when the scope has none.
pub(in crate::host::model) fn read(db: &Connection, scope: &Scope) -> Result<View, String> {
    let Some(id) = scope_id(db, scope)? else {
        return Ok(View::empty(scope.clone()));
    };
    read_at(db, id, scope.clone())
}

fn read_at(db: &Connection, id: i64, scope: Scope) -> Result<View, String> {
    let revision: i64 = db
        .prepare_cached("SELECT revision FROM view_scopes WHERE id = ?1")
        .and_then(|mut statement| statement.query_row([id], |row| row.get(0)))
        .map_err(sql)?;
    Ok(View {
        scope,
        revision: revision as u64,
        panes: panes_of(db, id)?
            .into_iter()
            .map(|(_, pane)| pane)
            .collect(),
        retired: retired_of(db, id)?,
    })
}

/// Every stored view, in one pass over each table.
pub(in crate::host::model) fn all(db: &Connection) -> Result<Vec<View>, String> {
    collect(db, false)
}

/// The views holding at least one pane. Nearly every scope a long-lived store
/// knows is an emptied worktree, so a search for a pane reads only these.
pub(in crate::host::model) fn occupied(db: &Connection) -> Result<Vec<View>, String> {
    collect(db, true)
}

fn collect(db: &Connection, occupied: bool) -> Result<Vec<View>, String> {
    let filter = if occupied {
        "WHERE EXISTS(SELECT 1 FROM panes p WHERE p.scope = s.id)"
    } else {
        ""
    };
    let mut statement = db
        .prepare_cached(&format!(
            "SELECT id, channel, repository, worktree, revision FROM view_scopes s {filter}
             ORDER BY id"
        ))
        .map_err(sql)?;
    let scopes = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .map_err(sql)?;
    let mut views = Vec::with_capacity(scopes.len());
    let mut index = HashMap::with_capacity(scopes.len());
    for (id, channel, repository, worktree, revision) in scopes {
        index.insert(id, views.len());
        views.push(View {
            scope: scope_from(channel, repository, worktree)?,
            revision: revision as u64,
            panes: Vec::new(),
            retired: Vec::new(),
        });
    }
    let mut panes = db
        .prepare_cached(&format!(
            "SELECT scope, {ROW_COLUMNS} FROM panes ORDER BY scope, position, id"
        ))
        .map_err(sql)?;
    let rows = panes
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, row(r, 1)?)))
        .map_err(sql)?;
    for entry in rows {
        let (scope, row) = entry.map_err(sql)?;
        let view = index
            .get(&scope)
            .ok_or("a saved pane names a view that does not exist")?;
        views[*view].panes.push(row.pane()?);
    }
    let mut retired = db
        .prepare_cached("SELECT scope, pane FROM retired_panes ORDER BY scope, ordinal")
        .map_err(sql)?;
    let rows = retired
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .map_err(sql)?;
    for entry in rows {
        let (scope, pane) = entry.map_err(sql)?;
        // Emptied views are skipped by `occupied`; their tombstones with them.
        let Some(view) = index.get(&scope) else {
            continue;
        };
        views[*view].retired.push(
            pane.parse()
                .map_err(|error| format!("retired pane id `{pane}` is unreadable: {error}"))?,
        );
    }
    Ok(views)
}

/// The view that holds pane `id`, if any.
pub(in crate::host::model) fn owner(db: &Connection, id: CardId) -> Result<Option<Scope>, String> {
    db.prepare_cached(
        "SELECT s.channel, s.repository, s.worktree FROM panes p
         JOIN view_scopes s ON s.id = p.scope WHERE p.id = ?1",
    )
    .and_then(|mut statement| {
        statement
            .query_row([id.to_string()], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()
    })
    .map_err(sql)?
    .map(|(channel, repository, worktree)| scope_from(channel, repository, worktree))
    .transpose()
}

/// Another pane presenting the same hosted resource as `pane`, in any view.
/// Matches `same_resource`: resources are compared only within one kind.
pub(in crate::host::model) fn resource_holder(
    db: &Connection,
    pane: &Pane,
) -> Result<Option<CardId>, String> {
    let derived = Derived::of(&pane.kind)?;
    let Some(resource) = derived.resource else {
        return Ok(None);
    };
    db.prepare_cached("SELECT id FROM panes WHERE kind = ?1 AND resource = ?2 AND id <> ?3")
        .and_then(|mut statement| {
            statement
                .query_row(
                    params![derived.kind, resource, pane.id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()
        })
        .map_err(sql)?
        .map(|id| {
            id.parse()
                .map_err(|error| format!("saved pane id `{id}` is unreadable: {error}"))
        })
        .transpose()
}

/// Every scope of `repository`: its worktrees and its channels.
pub(in crate::host::model) fn project_scopes(
    db: &Connection,
    repository: &str,
) -> Result<Vec<Scope>, String> {
    let mut statement = db
        .prepare_cached(
            "SELECT s.channel, s.repository, s.worktree FROM view_scopes s
             LEFT JOIN channels c ON c.id = s.channel
             WHERE s.repository = ?1 OR c.repository = ?1 ORDER BY s.id",
        )
        .map_err(sql)?;
    let rows = statement
        .query_map([repository], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .map_err(sql)?;
    rows.into_iter()
        .map(|(channel, repository, worktree)| scope_from(channel, repository, worktree))
        .collect()
}

/// Whether the worktree view of `repository` at `path` exists.
pub(in crate::host::model) fn worktree_exists(
    db: &Connection,
    repository: &str,
    path: &str,
) -> Result<bool, String> {
    db.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM view_scopes WHERE repository = ?1 AND worktree = ?2)",
    )
    .and_then(|mut statement| statement.query_row(params![repository, path], |row| row.get(0)))
    .map_err(sql)
}

pub(in crate::host::model) fn operation_seen(
    db: &Connection,
    operation: Uuid,
) -> Result<bool, String> {
    db.prepare_cached("SELECT EXISTS(SELECT 1 FROM view_operations WHERE operation = ?1)")
        .and_then(|mut statement| statement.query_row([operation.to_string()], |row| row.get(0)))
        .map_err(sql)
}

pub(in crate::host::model) fn record_operation(
    db: &Connection,
    operation: Uuid,
    scope: i64,
) -> Result<(), String> {
    db.prepare_cached(
        "INSERT INTO view_operations(operation, scope, applied_at_unix_ms) VALUES (?1, ?2, ?3)",
    )
    .and_then(|mut statement| {
        statement.execute(params![operation.to_string(), scope, now_unix_ms()])
    })
    .map_err(sql)?;
    Ok(())
}

/// Forgets at most one batch of operations applied before `horizon_unix_ms`.
/// Returns how many it removed.
pub(in crate::host::model) fn prune_operations(
    db: &Connection,
    horizon_unix_ms: i64,
) -> Result<usize, String> {
    db.prepare_cached(
        "DELETE FROM view_operations WHERE operation IN (
             SELECT operation FROM view_operations WHERE applied_at_unix_ms < ?1
             ORDER BY applied_at_unix_ms LIMIT ?2)",
    )
    .and_then(|mut statement| statement.execute(params![horizon_unix_ms, PRUNE_BATCH]))
    .map_err(sql)
}

/// Persists `after` over the stored view of its scope, writing only the rows
/// that differ. The scope row must exist ([`register`]).
///
/// A pane that `after` holds and another scope's row holds is moved here by
/// updating its row, never by deleting it, so the rows that hang off a pane
/// (terminal sources, retention pins) survive a transfer.
pub(in crate::host::model) fn store(db: &Connection, after: &View) -> Result<(), String> {
    let scope =
        scope_id(db, &after.scope)?.ok_or("a view must be registered before it is written")?;
    let stored = panes_of(db, scope)?;
    let positions = positions(&stored, &after.panes);
    let before: HashMap<CardId, (i64, &Pane)> = stored
        .iter()
        .map(|(position, pane)| (pane.id, (*position, pane)))
        .collect();
    let kept: HashSet<CardId> = after.panes.iter().map(|pane| pane.id).collect();
    for (_, pane) in &stored {
        if !kept.contains(&pane.id) {
            db.prepare_cached("DELETE FROM panes WHERE id = ?1 AND scope = ?2")
                .and_then(|mut statement| statement.execute(params![pane.id.to_string(), scope]))
                .map_err(sql)?;
        }
    }
    for (pane, position) in after.panes.iter().zip(positions) {
        match before.get(&pane.id) {
            Some((old, previous)) if *old == position && *previous == pane => {}
            Some((_, previous)) if *previous == pane => {
                db.prepare_cached("UPDATE panes SET position = ?2 WHERE id = ?1")
                    .and_then(|mut statement| {
                        statement.execute(params![pane.id.to_string(), position])
                    })
                    .map_err(sql)?;
            }
            _ => put(db, scope, position, pane)?,
        }
    }
    store_retired(db, scope, &after.retired)?;
    db.prepare_cached("UPDATE view_scopes SET revision = ?2 WHERE id = ?1 AND revision <> ?2")
        .and_then(|mut statement| statement.execute(params![scope, after.revision as i64]))
        .map_err(sql)?;
    Ok(())
}

/// Moves pane `id`'s row to `destination` without deleting it, so the rows
/// that hang off the pane stay with it. The destination's next [`store`]
/// gives it its position there.
pub(in crate::host::model) fn move_pane(
    db: &Connection,
    id: CardId,
    destination: i64,
) -> Result<(), String> {
    db.prepare_cached("UPDATE panes SET scope = ?2 WHERE id = ?1")
        .and_then(|mut statement| statement.execute(params![id.to_string(), destination]))
        .map_err(sql)?;
    Ok(())
}

/// Inserts the pane, or replaces every column of the row that holds its id,
/// in this scope or another.
fn put(db: &Connection, scope: i64, position: i64, pane: &Pane) -> Result<(), String> {
    pane.validate()?;
    insert(db, scope, position, pane)
}

/// [`put`] without validation, for panes an earlier schema already accepted.
fn insert(db: &Connection, scope: i64, position: i64, pane: &Pane) -> Result<(), String> {
    let derived = Derived::of(&pane.kind)?;
    let filing = pane
        .scope
        .as_ref()
        .map(|path| {
            path.to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("pane filing `{}` is not valid UTF-8", path.display()))
        })
        .transpose()?;
    let descriptor = serde_json::to_string(&pane.kind).map_err(|error| error.to_string())?;
    db.prepare_cached(
        "INSERT INTO panes(id, scope, position, title, user_title, width_fraction,
             pending_terminal_start, filing, kind, resource, provider, provider_session,
             descriptor_format, descriptor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(id) DO UPDATE SET scope = excluded.scope, position = excluded.position,
             title = excluded.title, user_title = excluded.user_title,
             width_fraction = excluded.width_fraction,
             pending_terminal_start = excluded.pending_terminal_start,
             filing = excluded.filing, kind = excluded.kind, resource = excluded.resource,
             provider = excluded.provider, provider_session = excluded.provider_session,
             descriptor_format = excluded.descriptor_format, descriptor = excluded.descriptor",
    )
    .and_then(|mut statement| {
        statement.execute(params![
            pane.id.to_string(),
            scope,
            position,
            pane.title,
            pane.user_title,
            pane.width_fraction as f64,
            pane.pending_terminal_start as i64,
            filing,
            derived.kind,
            derived.resource,
            derived.provider,
            derived.provider_session,
            DESCRIPTOR_FORMAT,
            descriptor,
        ])
    })
    .map_err(sql)?;
    Ok(())
}

fn store_retired(db: &Connection, scope: i64, retired: &[CardId]) -> Result<(), String> {
    let stored = retired_of(db, scope)?;
    if stored == retired {
        return Ok(());
    }
    let keep: HashSet<&CardId> = retired.iter().collect();
    for id in &stored {
        if !keep.contains(id) {
            db.prepare_cached("DELETE FROM retired_panes WHERE scope = ?1 AND pane = ?2")
                .and_then(|mut statement| statement.execute(params![scope, id.to_string()]))
                .map_err(sql)?;
        }
    }
    let remaining: Vec<&CardId> = stored.iter().filter(|id| keep.contains(id)).collect();
    // Retirement only appends and removes, so the survivors keep their order
    // and the new ids follow them. Anything else is renumbered.
    let appended =
        retired.len() >= remaining.len() && retired.iter().zip(&remaining).all(|(a, b)| a == *b);
    if !appended {
        db.prepare_cached("DELETE FROM retired_panes WHERE scope = ?1")
            .and_then(|mut statement| statement.execute([scope]))
            .map_err(sql)?;
    }
    let mut next: i64 = db
        .prepare_cached("SELECT coalesce(max(ordinal), 0) FROM retired_panes WHERE scope = ?1")
        .and_then(|mut statement| statement.query_row([scope], |row| row.get(0)))
        .map_err(sql)?;
    let start = if appended { remaining.len() } else { 0 };
    let now = now_unix_ms();
    for id in &retired[start..] {
        next += 1;
        db.prepare_cached(
            "INSERT INTO retired_panes(scope, pane, ordinal, retired_at_unix_ms)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .and_then(|mut statement| statement.execute(params![scope, id.to_string(), next, now]))
        .map_err(sql)?;
    }
    Ok(())
}

/// Positions for `after`'s panes that keep as many stored positions as the
/// new order allows: the panes on a longest run whose stored positions still
/// increase keep theirs, and the rest take midpoints between their
/// neighbors. Only when two neighbors are adjacent is the scope renumbered,
/// which costs a row per pane in the scope, a handful.
fn positions(stored: &[(i64, Pane)], after: &[Pane]) -> Vec<i64> {
    let old: HashMap<CardId, i64> = stored.iter().map(|(p, pane)| (pane.id, *p)).collect();
    let candidates: Vec<Option<i64>> = after
        .iter()
        .map(|pane| old.get(&pane.id).copied())
        .collect();
    let keep = increasing_run(&candidates);
    let mut result = vec![0; after.len()];
    let mut index = 0;
    while index < after.len() {
        if keep[index] {
            result[index] = candidates[index].unwrap();
            index += 1;
            continue;
        }
        // A gap of panes to place between the kept neighbors around it.
        let start = index;
        while index < after.len() && !keep[index] {
            index += 1;
        }
        let low = if start == 0 { 0 } else { result[start - 1] };
        let high = if index < after.len() {
            candidates[index].unwrap()
        } else {
            low + POSITION_STEP * (index - start + 1) as i64
        };
        let count = (index - start) as i64;
        let step = (high - low) / (count + 1);
        if step == 0 {
            return (1..=after.len() as i64)
                .map(|i| i * POSITION_STEP)
                .collect();
        }
        for (offset, slot) in result[start..index].iter_mut().enumerate() {
            *slot = low + step * (offset as i64 + 1);
        }
    }
    result
}

/// Marks a longest strictly increasing subsequence of the known positions.
fn increasing_run(positions: &[Option<i64>]) -> Vec<bool> {
    // Patience sorting over the known entries; `tails[k]` is the index ending
    // the best run of length k + 1.
    let mut tails: Vec<usize> = Vec::new();
    let mut previous = vec![usize::MAX; positions.len()];
    for (index, position) in positions.iter().enumerate() {
        let Some(position) = position else {
            continue;
        };
        let at = tails.partition_point(|&tail| positions[tail].unwrap() < *position);
        if at > 0 {
            previous[index] = tails[at - 1];
        }
        if at == tails.len() {
            tails.push(index);
        } else {
            tails[at] = index;
        }
    }
    let mut keep = vec![false; positions.len()];
    let mut cursor = tails.last().copied();
    while let Some(index) = cursor {
        keep[index] = true;
        cursor = (previous[index] != usize::MAX).then(|| previous[index]);
    }
    keep
}

/// Schema 22's view tables, as `views::migrate` created them. The converter
/// back to 22 writes exactly these, so a rolled-back daemon finds the layout
/// it published.
pub(in crate::host::model) const V22_VIEW_TABLES: &str =
    "CREATE TABLE IF NOT EXISTS shared_view_scopes(
            scope TEXT PRIMARY KEY,
            channel TEXT REFERENCES channels(id) ON DELETE CASCADE,
            repository TEXT REFERENCES projects(repository) ON DELETE CASCADE,
            CHECK((channel IS NULL) != (repository IS NULL))
        ) STRICT;
        CREATE TABLE IF NOT EXISTS shared_views(
            scope TEXT PRIMARY KEY REFERENCES shared_view_scopes(scope) ON DELETE CASCADE,
            revision INTEGER NOT NULL, panes TEXT NOT NULL, retired TEXT NOT NULL
        ) STRICT;
        CREATE TABLE IF NOT EXISTS shared_view_panes(
            id TEXT PRIMARY KEY,
            scope TEXT NOT NULL REFERENCES shared_view_scopes(scope) ON DELETE CASCADE
        ) STRICT;
        CREATE TABLE IF NOT EXISTS shared_view_edits(
            operation TEXT PRIMARY KEY,
            scope TEXT NOT NULL REFERENCES shared_view_scopes(scope) ON DELETE CASCADE
        ) STRICT;";
pub(in crate::host::model) const V22_TERMINAL_SOURCES: &str =
    "CREATE TABLE IF NOT EXISTS shared_view_terminal_sources(
            pane TEXT PRIMARY KEY REFERENCES shared_view_panes(id) ON DELETE CASCADE,
            source TEXT NOT NULL, ended TEXT NOT NULL
        ) STRICT;";
pub(in crate::host::model) const V22_RETENTION_PINS: &str =
    "CREATE TABLE shared_terminal_retention_pins(
            pane TEXT PRIMARY KEY REFERENCES shared_view_panes(id) ON DELETE CASCADE,
            terminal TEXT NOT NULL
        ) STRICT;
        CREATE INDEX shared_terminal_retention_by_terminal
            ON shared_terminal_retention_pins(terminal);";

fn scope_key(scope: &Scope) -> Result<String, String> {
    serde_json::to_string(scope).map_err(|error| error.to_string())
}

/// Recreates the pane-owned tables under `definitions`, keeping the rows of
/// every pane `owner` holds. Their foreign keys name the ownership table,
/// which the conversion replaces; SQLite cannot repoint one in place, and a
/// rename would rewrite the stored definition into a form no published layout
/// has.
fn rebuild_pane_owned(
    tx: &rusqlite::Transaction<'_>,
    definitions: &str,
    owner: &str,
) -> Result<(), String> {
    tx.execute_batch(
        "CREATE TEMP TABLE converted_terminal_sources AS SELECT pane, source, ended
             FROM main.shared_view_terminal_sources;
         CREATE TEMP TABLE converted_retention_pins AS SELECT pane, terminal
             FROM main.shared_terminal_retention_pins;
         DROP TABLE main.shared_view_terminal_sources;
         DROP TABLE main.shared_terminal_retention_pins;",
    )
    .map_err(sql)?;
    tx.execute_batch(definitions).map_err(sql)?;
    tx.execute_batch(&format!(
        "INSERT INTO main.shared_view_terminal_sources(pane, source, ended)
             SELECT pane, source, ended FROM temp.converted_terminal_sources
             WHERE pane IN (SELECT id FROM main.{owner});
         INSERT INTO main.shared_terminal_retention_pins(pane, terminal)
             SELECT pane, terminal FROM temp.converted_retention_pins
             WHERE pane IN (SELECT id FROM main.{owner});
         DROP TABLE temp.converted_terminal_sources;
         DROP TABLE temp.converted_retention_pins;"
    ))
    .map_err(sql)
}

/// Schema 23: one row per scope, pane, retired id and applied operation, from
/// schema 22's per-scope JSON arrays.
///
/// The arrays say which panes are open, so every pane they hold stays open.
/// Where they hold one pane twice, or two panes for one resource, the
/// conversion keeps what schema 22 acted on (see below) rather than refuse
/// and leave the daemon without a model. Schema 22's ownership index is used
/// only to choose between two views; an index row for a pane no view holds
/// leaves with the rows that hung off it, as deleting it would have.
pub(in crate::host::model) fn migrate(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(&format!(
        "{VIEW_SCOPES}; {VIEW_REVISION_CLOCK}; {PANES}; {PANES_BY_SCOPE}; {RETIRED_PANES};
         {VIEW_OPERATIONS}; {VIEW_OPERATIONS_BY_AGE};
         CREATE TEMP TABLE converted_scopes(key TEXT PRIMARY KEY, id INTEGER NOT NULL) STRICT;"
    ))
    .map_err(sql)?;
    let now = now_unix_ms();
    let views = {
        let mut statement = tx
            .prepare(
                "SELECT s.scope, s.channel, s.repository, v.revision, v.panes, v.retired
                 FROM shared_view_scopes s JOIN shared_views v USING(scope) ORDER BY s.scope",
            )
            .map_err(sql)?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
            .map_err(sql)?
    };
    // Schema 22 allowed two damaged states its daemon resolved on use rather
    // than refused: a pane in two views' lists, and two panes presenting one
    // resource. Neither fits these tables, and refusing would leave the daemon
    // without a model, so each keeps the place schema 22 acted on. A pane stays
    // in the view its pane index named, where its edits, transfers and pins
    // went, or else in the first view that lists it; a resource stays with the
    // first pane that presents it, the one schema 22 found as its holder.
    // Every pane id stays open in some view, and every resource in some pane.
    // A view a pane leaves retires its id, so a stale window's seed cannot put
    // it back there once it closes where it stayed, and takes a new revision,
    // so a stale window's edit to it conflicts and rebases instead of being
    // acknowledged as a no-op.
    let mut views = views
        .into_iter()
        .map(|(key, channel, repository, revision, saved, retired)| {
            let saved: Vec<Pane> = serde_json::from_str(&saved).map_err(|error| {
                format!("a saved view is unreadable; the store was not converted: {error}")
            })?;
            Ok((key, channel, repository, revision, saved, retired))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let indexed: HashMap<String, String> = {
        let mut statement = tx
            .prepare("SELECT id, scope FROM shared_view_panes")
            .map_err(sql)?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .and_then(Iterator::collect::<rusqlite::Result<_>>)
            .map_err(sql)?
    };
    let mut holders: HashMap<CardId, Vec<&str>> = Default::default();
    for (key, _, _, _, saved, _) in &views {
        for pane in saved {
            holders.entry(pane.id).or_default().push(key);
        }
    }
    let keeper: HashMap<CardId, String> = holders
        .into_iter()
        .map(|(id, keys)| {
            let kept = indexed
                .get(&id.to_string())
                .filter(|named| keys.contains(&named.as_str()))
                .map_or(keys[0], String::as_str);
            (id, kept.to_owned())
        })
        .collect();
    let mut left: HashMap<String, Vec<CardId>> = HashMap::new();
    for (key, _, _, _, saved, _) in &mut views {
        let before = saved.len();
        // Within one list too, the first entry for an id is the pane.
        let mut seen = HashSet::new();
        saved.retain(|pane| {
            if keeper[&pane.id] != *key {
                left.entry(key.clone()).or_default().push(pane.id);
                return false;
            }
            seen.insert(pane.id)
        });
        if saved.len() != before {
            tracing::warn!(
                scope = %key,
                panes = before - saved.len(),
                "a pane listed again in another view stays only where it was used"
            );
        }
    }
    let mut clock = 0;
    let mut repaired = Vec::new();
    for (key, channel, repository, revision, saved, retired) in views {
        let scope: Scope = serde_json::from_str(&key)
            .map_err(|error| format!("a saved view scope is unreadable: {error}"))?;
        let (expected_channel, expected_repository, _) = columns(&scope)?;
        let owned = match &scope {
            Scope::Channel(_) => {
                channel.as_deref() == expected_channel.as_deref() && repository.is_none()
            }
            Scope::Worktree { .. } => {
                channel.is_none() && repository.as_deref() == expected_repository
            }
        };
        if !owned {
            return Err(
                "a saved view scope disagrees with its owner; the store was not converted".into(),
            );
        }
        let id = register(tx, &scope)?;
        tx.execute(
            "INSERT INTO temp.converted_scopes(key, id) VALUES (?1, ?2)",
            params![key, id],
        )
        .map_err(sql)?;
        let mut retired: Vec<CardId> = serde_json::from_str(&retired).map_err(|error| {
            format!("a saved view is unreadable; the store was not converted: {error}")
        })?;
        let mut gone = left.remove(&key).unwrap_or_default();
        let mut position = 0;
        for pane in &saved {
            if let Some(holder) = resource_holder(tx, pane)? {
                tracing::warn!(
                    pane = %pane.id,
                    %holder,
                    "a second pane for one resource gives way to the pane that held it"
                );
                gone.push(pane.id);
                continue;
            }
            position += POSITION_STEP;
            insert(tx, id, position, pane).map_err(|error| {
                format!(
                    "saved pane {} cannot be converted; the store was not converted: {error}",
                    pane.id
                )
            })?;
        }
        if !gone.is_empty() {
            repaired.push(id);
        }
        for pane in gone {
            if !retired.contains(&pane) {
                retired.push(pane);
            }
        }
        for (index, pane) in retired.iter().enumerate() {
            tx.execute(
                "INSERT INTO retired_panes(scope, pane, ordinal, retired_at_unix_ms) VALUES (?1, ?2, ?3, ?4)",
                params![id, pane.to_string(), index as i64 + 1, now],
            )
            .map_err(|error| format!("a saved view retires a pane twice: {error}"))?;
        }
        tx.execute(
            "UPDATE view_scopes SET revision = ?2 WHERE id = ?1",
            params![id, revision],
        )
        .map_err(sql)?;
        clock = clock.max(revision);
    }
    // An applied operation's age is unknown; its retention starts now.
    tx.execute(
        "INSERT INTO view_operations(operation, scope, applied_at_unix_ms)
         SELECT e.operation, k.id, ?1 FROM shared_view_edits e
         JOIN temp.converted_scopes k ON k.key = e.scope",
        [now],
    )
    .map_err(sql)?;
    for id in repaired {
        clock += 1;
        tx.execute(
            "UPDATE view_scopes SET revision = ?2 WHERE id = ?1",
            params![id, clock],
        )
        .map_err(sql)?;
    }
    tx.execute(
        "INSERT INTO view_revision_clock(id, last) VALUES (1, ?1)",
        [clock],
    )
    .map_err(sql)?;
    rebuild_pane_owned(
        tx,
        &format!("{TERMINAL_SOURCES}; {RETENTION_PINS}; {RETENTION_PINS_BY_TERMINAL};"),
        "panes",
    )?;
    // This is the historical 22 -> 23 conversion: schema 23 still requires
    // placement epochs. A pane that gave way above keeps none. Schema 24
    // removes the placement tables in its separate migration.
    tx.execute(
        "DELETE FROM channel_work_placement_epochs WHERE pane NOT IN (SELECT id FROM panes)",
        [],
    )
    .map_err(sql)?;
    tx.execute_batch(
        "DROP TABLE temp.converted_scopes;
         DROP TABLE shared_view_edits; DROP TABLE shared_views;
         DROP TABLE shared_view_panes; DROP TABLE shared_view_scopes;",
    )
    .map_err(sql)
}

/// Converts a schema 23 store back to schema 22's tables, for a daemon that
/// predates schema 23. Keeps every view, pane, retired id and applied
/// operation; drops only what 22 cannot hold (application and retirement
/// times, the revision clock).
pub(in crate::host::model) fn downgrade(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    let views = all(tx)?;
    let ids: HashMap<String, i64> = {
        let mut statement = tx
            .prepare("SELECT id, channel, repository, worktree FROM view_scopes")
            .map_err(sql)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
            .map_err(sql)?;
        rows.into_iter()
            .map(|(id, channel, repository, worktree)| {
                Ok((scope_key(&scope_from(channel, repository, worktree)?)?, id))
            })
            .collect::<Result<_, String>>()?
    };
    tx.execute_batch(V22_VIEW_TABLES).map_err(sql)?;
    tx.execute_batch(
        "CREATE TEMP TABLE converted_scopes(key TEXT PRIMARY KEY, id INTEGER NOT NULL) STRICT;",
    )
    .map_err(sql)?;
    for view in &views {
        let key = scope_key(&view.scope)?;
        let (channel, repository, _) = columns(&view.scope)?;
        tx.execute(
            "INSERT INTO shared_view_scopes(scope, channel, repository) VALUES (?1, ?2, ?3)",
            params![key, channel, repository],
        )
        .map_err(sql)?;
        tx.execute(
            "INSERT INTO shared_views(scope, revision, panes, retired) VALUES (?1, ?2, ?3, ?4)",
            params![
                key,
                view.revision as i64,
                serde_json::to_string(&view.panes).map_err(|error| error.to_string())?,
                serde_json::to_string(&view.retired).map_err(|error| error.to_string())?
            ],
        )
        .map_err(sql)?;
        for pane in &view.panes {
            tx.execute(
                "INSERT INTO shared_view_panes(id, scope) VALUES (?1, ?2)",
                params![pane.id.to_string(), key],
            )
            .map_err(sql)?;
        }
        tx.execute(
            "INSERT INTO temp.converted_scopes(key, id) VALUES (?1, ?2)",
            params![key, ids[&key]],
        )
        .map_err(sql)?;
    }
    tx.execute(
        "INSERT INTO shared_view_edits(operation, scope)
         SELECT o.operation, k.key FROM view_operations o
         JOIN temp.converted_scopes k ON k.id = o.scope",
        [],
    )
    .map_err(sql)?;
    rebuild_pane_owned(
        tx,
        &format!("{V22_TERMINAL_SOURCES} {V22_RETENTION_PINS}"),
        "shared_view_panes",
    )?;
    tx.execute_batch(
        "DROP TABLE temp.converted_scopes;
         DROP TABLE view_operations; DROP TABLE retired_panes; DROP TABLE panes;
         DROP TABLE view_scopes; DROP TABLE view_revision_clock;",
    )
    .map_err(sql)
}

/// Rewrites a scope's panes without bumping its revision or validating them,
/// as stale or damaged stores hold them.
#[cfg(test)]
pub(in crate::host::model) fn overwrite_panes(db: &Connection, scope: &Scope, panes: &[Pane]) {
    let id = register(db, scope).unwrap();
    db.execute("DELETE FROM panes WHERE scope = ?1", [id])
        .unwrap();
    for (index, pane) in panes.iter().enumerate() {
        insert(db, id, (index as i64 + 1) * POSITION_STEP, pane).unwrap();
    }
}

#[cfg(test)]
mod conversion_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn check(stored: &[i64], after: &[Option<usize>]) -> Vec<i64> {
        // `after` lists stored indexes (Some) or new panes (None).
        let panes: Vec<Pane> = (0..stored.len() + after.len()).map(|_| pane()).collect();
        let stored_rows: Vec<(i64, Pane)> = stored
            .iter()
            .zip(&panes)
            .map(|(position, pane)| (*position, pane.clone()))
            .collect();
        let mut fresh = stored.len();
        let after_panes: Vec<Pane> = after
            .iter()
            .map(|entry| match entry {
                Some(index) => panes[*index].clone(),
                None => {
                    fresh += 1;
                    panes[fresh - 1].clone()
                }
            })
            .collect();
        let result = positions(&stored_rows, &after_panes);
        assert!(
            result.windows(2).all(|pair| pair[0] < pair[1]),
            "{result:?}"
        );
        assert!(result.iter().all(|position| *position > 0), "{result:?}");
        result
    }

    fn pane() -> Pane {
        Pane {
            id: CardId::new(),
            title: String::new(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            kind: CardKind::Browser(crate::model::BrowserSession {
                id: crate::model::BrowserId::new(),
                current_url: "https://example.com".parse().unwrap(),
                history: vec![],
                history_index: 0,
                scroll_x: 0.,
                scroll_y: 0.,
                fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
            }),
            scope: None,
        }
    }

    #[test]
    fn positions_keep_unmoved_panes_and_place_the_rest_between_neighbors() {
        let step = POSITION_STEP;
        // Appending keeps every stored position.
        let result = check(&[step, 2 * step], &[Some(0), Some(1), None]);
        assert_eq!(&result[..2], &[step, 2 * step]);
        // Inserting first or between takes a midpoint and moves nothing else.
        let result = check(&[step, 2 * step], &[None, Some(0), Some(1)]);
        assert_eq!(&result[1..], &[step, 2 * step]);
        let result = check(&[step, 2 * step], &[Some(0), None, Some(1)]);
        assert_eq!((result[0], result[2]), (step, 2 * step));
        // Moving one pane rewrites only that pane.
        let result = check(&[step, 2 * step, 3 * step], &[Some(2), Some(0), Some(1)]);
        assert_eq!(&result[1..], &[step, 2 * step]);
        // Adjacent neighbors renumber the scope.
        let result = check(&[1, 2], &[Some(0), None, Some(1)]);
        assert_eq!(result, vec![step, 2 * step, 3 * step]);
        // An empty scope starts at one step.
        assert_eq!(check(&[], &[None, None]), vec![step, 2 * step]);
    }
}
