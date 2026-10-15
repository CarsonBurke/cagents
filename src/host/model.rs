//! The daemon's durable registry for the shared half of the sidebar model.
//!
//! A window is not the authority for which projects and channels exist: two
//! windows are two processes with two databases, and the CLI has neither. The
//! daemon is the one process that outlives a window and that every client can
//! already talk to, so it owns the shared model and every window mirrors it.
//!
//! The store lives in the data directory rather than the runtime directory,
//! because a channel a user created must still be there after a reboot, while
//! the runtime directory is deliberately swept when the session ends.

use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::{
    model::{ChannelId, ChannelTitleSource, ProjectKind},
    workspace_model::{ModelChannel, ModelEdit, ModelProject, WorkspaceModel},
};

mod automation_proposals;
mod automations;
pub(super) use automations::Claimed;
mod execution;
mod lineage;
mod mailbox;
mod native_actor;
mod navigation;
pub(super) use native_actor::NativeActorEvent;
mod terminal_owners;
mod views;

const SCHEMA_VERSION: u32 = 15;

/// A writer that loses a race waits rather than failing. Only the daemon writes
/// this store, so contention is between its own client threads and is measured
/// in microseconds; the timeout exists for the WAL checkpoint, not for a rival.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// The outcome of one edit.
#[derive(Debug)]
pub(crate) struct Applied {
    pub(crate) model: WorkspaceModel,
    /// Whether the edit changed anything. A retitle to the same title, or a
    /// delete of a channel that is already gone, must neither bump `sequence`
    /// nor wake a watcher: a window that reacted to it would redraw for nothing
    /// and, worse, could not tell the no-op apart from a real edge.
    pub(crate) changed: bool,
}

pub(crate) struct Registry {
    db: Connection,
    path: PathBuf,
}

impl Registry {
    /// Opens the registry in this installation's data directory.
    pub(crate) fn open() -> Result<Self, String> {
        let dirs = crate::instance::project_dirs()
            .ok_or_else(|| "the data directory is unavailable".to_owned())?;
        Self::at(&dirs.data_dir().join("workspace-model.sqlite3"))
    }

    /// Opens or creates the registry at `path`.
    pub(crate) fn at(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            create_private_directory(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        prepare_private_file(path)
            .map_err(|error| format!("cannot prepare {}: {error}", path.display()))?;
        let db = Connection::open(path).map_err(|error| error.to_string())?;
        configure(&db)?;
        migrate(&db)?;
        Ok(Self {
            db,
            path: path.to_owned(),
        })
    }

    /// Independent WAL connection for low-rate execution transactions. The
    /// caller can hold its own mutex without stalling terminal delivery behind
    /// the global host lock while SQLite commits or waits for another writer.
    pub(crate) fn execution_connection(&self) -> Result<Self, String> {
        Self::at(&self.path)
    }

    /// The whole model, ordered by the ranks the registry assigned.
    pub(crate) fn model(&self) -> Result<WorkspaceModel, String> {
        let (identity, sequence) = self
            .db
            .query_row(
                "SELECT identity, sequence FROM metadata WHERE key = 0",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(|error| error.to_string())?;
        let identity = identity.parse::<Uuid>().map_err(|error| {
            format!("stored registry identity `{identity}` is unreadable: {error}")
        })?;
        let mut projects = self
            .db
            .prepare("SELECT repository, name, ordinal, kind FROM projects ORDER BY ordinal")
            .map_err(|error| error.to_string())?;
        let projects = projects
            .query_map([], |row| {
                Ok(ModelProject {
                    repository: PathBuf::from(row.get::<_, String>(0)?),
                    name: row.get(1)?,
                    ordinal: row.get::<_, i64>(2)? as u32,
                    kind: match row.get::<_, String>(3)?.as_str() {
                        "git" => ProjectKind::Git,
                        "folder" => ProjectKind::Folder,
                        value => {
                            return Err(rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                format!("unknown project kind {value}").into(),
                            ));
                        }
                    },
                })
            })
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
            .map_err(|error| error.to_string())?;
        let mut channels = self
            .db
            .prepare("SELECT id, repository, title, ordinal, title_source FROM channels ORDER BY ordinal")
            .map_err(|error| error.to_string())?;
        let rows = channels
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
            .map_err(|error| error.to_string())?;
        let channels = rows
            .into_iter()
            .map(|(id, repository, title, ordinal, source)| {
                Ok(ModelChannel {
                    title_source: match source {
                        0 => ChannelTitleSource::Prompt,
                        1 => ChannelTitleSource::Provider,
                        2 => ChannelTitleSource::Manual,
                        _ => return Err("invalid stored channel title source".into()),
                    },
                    id: parse_channel_id(&id)?,
                    repository: PathBuf::from(repository),
                    title,
                    ordinal: ordinal as u32,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let closed_projects = self
            .db
            .prepare("SELECT repository FROM closed_projects ORDER BY repository")
            .map_err(|error| error.to_string())?
            .query_map([], |row| row.get::<_, String>(0))
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let navigation_orders = self
            .db
            .prepare("SELECT repository, items FROM navigation_orders ORDER BY repository")
            .map_err(|error| error.to_string())?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|(repository, items)| {
                Ok(crate::workspace_model::NavigationOrder {
                    repository: PathBuf::from(repository),
                    items: serde_json::from_str(&items).map_err(|error| error.to_string())?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(WorkspaceModel {
            navigation_orders,
            views: views::all(&self.db)?,
            agents: self.agents(None)?,
            identity,
            sequence: sequence as u64,
            projects,
            channels,
            closed_projects,
        })
    }

    /// Applies one edit as a single transaction, bumping `sequence` when it
    /// changed anything.
    ///
    /// The caller has already resolved the edit's repository key; this never
    /// touches the filesystem, so an edit is decided entirely from the store.
    pub(crate) fn apply(&mut self, edit: &ModelEdit) -> Result<Applied, String> {
        let transaction = self.db.transaction().map_err(|error| error.to_string())?;
        let changed = match edit {
            ModelEdit::SeedView { .. } | ModelEdit::ChangeView { .. } => views::apply(&transaction, edit)?,
            ModelEdit::ResetNavigation { .. } | ModelEdit::ReorderNavigation { .. } => navigation::apply(&transaction, edit)?,
            ModelEdit::AgentsChanged => {
                return Err("Agent model changes are committed by the execution service".into());
            }
            ModelEdit::OpenProject {
                repository,
                name,
                kind,
            }
            | ModelEdit::ReopenProject {
                repository,
                name,
                kind,
            } => {
                let key = repository_key(repository)?;
                let explicit = matches!(edit, ModelEdit::ReopenProject { .. });
                let tombstoned: bool = transaction
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM closed_projects WHERE repository = ?1)",
                        params![key],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                if tombstoned && !explicit {
                    false
                } else {
                    let reopened = if explicit {
                        transaction
                            .execute(
                                "DELETE FROM closed_projects WHERE repository = ?1",
                                params![key],
                            )
                            .map_err(|error| error.to_string())?
                            > 0
                    } else {
                        false
                    };
                    let existing: Option<String> = transaction
                        .query_row(
                            "SELECT name FROM projects WHERE repository = ?1",
                            params![key],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(|error| error.to_string())?;
                    match existing {
                        Some(current) if current == *name => reopened,
                        Some(_) => {
                            transaction
                                .execute(
                                    "UPDATE projects SET name = ?2 WHERE repository = ?1",
                                    params![key, name],
                                )
                                .map_err(|error| error.to_string())?;
                            true
                        }
                        None => {
                            let ordinal = take_ordinal(&transaction, "next_project_ordinal")?;
                            transaction
                                .execute(
                                    "INSERT INTO projects(repository, name, ordinal, kind) \
                                 VALUES (?1, ?2, ?3, ?4)",
                                    params![
                                        key,
                                        name,
                                        ordinal,
                                        match kind {
                                            ProjectKind::Git => "git",
                                            ProjectKind::Folder => "folder",
                                        }
                                    ],
                                )
                                .map_err(|error| error.to_string())?;
                            true
                        }
                    }
                }
            }
            // The channels go with it through the foreign key's cascade, which
            // `foreign_keys = ON` is verified to enforce: a channel in a project
            // nobody has open is reachable from no window's sidebar.
            ModelEdit::CloseProject { repository } => {
                let key = repository_key(repository)?;
                let removed = transaction
                    .execute("DELETE FROM projects WHERE repository = ?1", params![key])
                    .map_err(|error| error.to_string())?
                    > 0;
                let recorded = transaction
                    .execute(
                        "INSERT OR IGNORE INTO closed_projects(repository) VALUES (?1)",
                        params![key],
                    )
                    .map_err(|error| error.to_string())?
                    > 0;
                if removed || recorded {
                    automations::close_project(
                        &transaction,
                        &key,
                        crate::chat::now_ms().unwrap_or(0),
                    )?;
                }
                removed || recorded
            }
            ModelEdit::CreateChannel {
                id,
                repository,
                title,
                title_source,
            } => {
                let key = repository_key(repository)?;
                let exists: bool = transaction
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM channels WHERE id = ?1)",
                        params![id.to_string()],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                if exists {
                    false
                } else {
                    let owned: bool = transaction
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM projects WHERE repository = ?1)",
                            params![key],
                            |row| row.get(0),
                        )
                        .map_err(|error| error.to_string())?;
                    if !owned {
                        return Err(format!(
                            "no project owns `{key}`; open the project before creating a channel \
                             in it"
                        ));
                    }
                    let ordinal = take_ordinal(&transaction, "next_channel_ordinal")?;
                    transaction
                        .execute(
                            "INSERT INTO channels(id, repository, title, ordinal, title_source) \
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![id.to_string(), key, title, ordinal, *title_source as i64],
                        )
                        .map_err(|error| error.to_string())?;
                    true
                }
            }
            // An unknown id is a no-op rather than an error: a retitle that
            // raced a delete must not resurrect the channel the delete removed,
            // and the window that issued it has nothing useful to do with a
            // failure.
            ModelEdit::RetitleChannel { id, title } => {
                transaction
                    .execute(
                        "UPDATE channels SET title = ?2, title_source = 2 WHERE id = ?1 AND (title <> ?2 OR title_source <> 2)",
                        params![id.to_string(), title],
                    )
                    .map_err(|error| error.to_string())?
                    > 0
            }
            ModelEdit::SuggestChannelTitle { id, title, source } => {
                if *source == ChannelTitleSource::Manual || title.is_empty() {
                    return Err("channel title suggestions require a nonempty inferred title".into());
                }
                transaction.execute(
                    "UPDATE channels SET title = ?2, title_source = ?3 WHERE id = ?1 AND (title = '' OR title_source < ?3)",
                    params![id.to_string(), title, *source as i64],
                ).map_err(|error| error.to_string())? > 0
            }
            ModelEdit::DeleteChannel { id } => {
                transaction
                    .execute(
                        "DELETE FROM channels WHERE id = ?1",
                        params![id.to_string()],
                    )
                    .map_err(|error| error.to_string())?
                    > 0
            }
        };
        if changed {
            transaction
                .execute(
                    "UPDATE metadata SET sequence = sequence + 1 WHERE key = 0",
                    [],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(Applied {
            model: self.model()?,
            changed,
        })
    }
}

/// Reserves the next rank of `column` and returns it.
///
/// A rank is never reused, so deleting the newest channel does not hand its
/// number to the next one: two windows compare ranks to agree on order, and a
/// reused rank would make a new channel sort where the deleted one used to be.
fn take_ordinal(transaction: &rusqlite::Transaction<'_>, column: &str) -> Result<i64, String> {
    let ordinal: i64 = transaction
        .query_row(
            &format!("SELECT {column} FROM metadata WHERE key = 0"),
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            &format!("UPDATE metadata SET {column} = {column} + 1 WHERE key = 0"),
            [],
        )
        .map_err(|error| error.to_string())?;
    Ok(ordinal)
}

/// The stored spelling of a repository key.
///
/// Every key that can reach the daemon over the wire is already UTF-8, because
/// serde cannot serialize a non-UTF-8 `PathBuf` at all. Storing text rather than
/// bytes therefore loses nothing and keeps the store readable with `sqlite3`,
/// which is worth a great deal when diagnosing what two windows disagree about.
fn repository_key(repository: &Path) -> Result<&str, String> {
    repository.to_str().ok_or_else(|| {
        format!(
            "repository path `{}` is not valid UTF-8",
            repository.display()
        )
    })
}

fn parse_channel_id(value: &str) -> Result<ChannelId, String> {
    value
        .parse::<ChannelId>()
        .map_err(|error| format!("stored channel id `{value}` is unreadable: {error}"))
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new()
        .recursive(true)
        .mode(DIRECTORY_MODE)
        .create(path)
    {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Creates the store file with a private mode, or re-asserts the mode of one
/// that already exists.
///
/// The model names every repository the user has open, which is nobody else's
/// business on a shared machine.
fn prepare_private_file(path: &Path) -> io::Result<()> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))
        }
        Err(error) => Err(error),
    }
}

/// Applies the pragmas this store depends on, and verifies each one.
///
/// `foreign_keys` is load-bearing rather than hygienic: closing a project
/// removes its channels through the cascade, so an unenforced foreign key would
/// silently leave channels behind that no sidebar can reach. `FULL` makes
/// execution acceptance durable before launching a child or submitting its
/// prompt, including a power failure after the external side effect begins.
fn configure(db: &Connection) -> Result<(), String> {
    db.busy_timeout(BUSY_TIMEOUT)
        .map_err(|error| error.to_string())?;
    db.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;",
    )
    .map_err(|error| error.to_string())?;
    let foreign_keys: i64 = db
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if foreign_keys != 1 {
        return Err("SQLite foreign key enforcement could not be enabled".into());
    }
    let journal_mode: String = db
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(format!(
            "SQLite journal mode is `{journal_mode}`, but write-ahead logging is required"
        ));
    }
    // 0 = OFF, 1 = NORMAL, 2 = FULL, 3 = EXTRA.
    let synchronous: i64 = db
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if synchronous != 2 {
        return Err(format!(
            "SQLite synchronous mode is {synchronous}, but FULL (2) is required"
        ));
    }
    Ok(())
}

fn migrate(db: &Connection) -> Result<(), String> {
    let version: u32 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if version > SCHEMA_VERSION {
        return Err(format!(
            "workspace registry schema {version} is newer than supported schema {SCHEMA_VERSION}"
        ));
    }
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects(
             repository TEXT PRIMARY KEY,
             name TEXT NOT NULL,
             ordinal INTEGER NOT NULL
         ) STRICT;
         CREATE TABLE IF NOT EXISTS navigation_orders(
             repository TEXT PRIMARY KEY REFERENCES projects(repository) ON DELETE CASCADE,
             items TEXT NOT NULL
         ) STRICT;
         CREATE TABLE IF NOT EXISTS channels(
             id TEXT PRIMARY KEY,
             repository TEXT NOT NULL REFERENCES projects(repository) ON DELETE CASCADE,
             title TEXT NOT NULL,
             ordinal INTEGER NOT NULL
         ) STRICT;
         CREATE INDEX IF NOT EXISTS channels_by_repository ON channels(repository);
         CREATE TABLE IF NOT EXISTS closed_projects(
             repository TEXT PRIMARY KEY
         ) STRICT;
         CREATE TABLE IF NOT EXISTS metadata(
             key INTEGER PRIMARY KEY CHECK(key = 0),
             identity TEXT NOT NULL,
             sequence INTEGER NOT NULL,
             next_project_ordinal INTEGER NOT NULL,
             next_channel_ordinal INTEGER NOT NULL
         ) STRICT;",
    )
    .map_err(|error| error.to_string())?;
    let has_kind: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('projects') WHERE name = 'kind')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if !has_kind {
        db.execute_batch("ALTER TABLE projects ADD COLUMN kind TEXT NOT NULL DEFAULT 'git' CHECK(kind IN ('git', 'folder'));")
            .map_err(|error| error.to_string())?;
    }
    // The identity is minted here rather than in `at`, because this is the only
    // place that can tell a store being created from one being reopened: the
    // ignored insert is what makes minting happen exactly once, and a reopened
    // registry has to keep the identity every window recorded against it.
    db.execute(
        "INSERT OR IGNORE INTO metadata(
             key, identity, sequence, next_project_ordinal, next_channel_ordinal
         ) VALUES (0, ?1, 0, 1, 1)",
        [Uuid::new_v4().to_string()],
    )
    .map_err(|error| error.to_string())?;
    if version < 1 {
        execution::migrate(db)?;
    }
    if version < 2 {
        automations::migrate(db)?;
    }
    if version < 3 {
        let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let current: u32 = tx
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if current < 3 {
            tx.execute_batch("ALTER TABLE channels ADD COLUMN title_source INTEGER NOT NULL DEFAULT 2 CHECK(title_source BETWEEN 0 AND 2); PRAGMA user_version = 3;")
                .map_err(|error| error.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())?;
    }
    if version < 4 {
        lineage::migrate(db)?;
    }
    if version < 5 {
        mailbox::migrate(db)?;
    }
    if version < 6 {
        terminal_owners::migrate(db)?;
    }
    if version < 8 {
        views::migrate(db)?;
    }
    if version < 9 {
        db.pragma_update(None, "user_version", 9)
            .map_err(|error| error.to_string())?;
    }
    if version < 10 {
        execution::migrate_names(db)?;
    }
    if version < 11 {
        automation_proposals::migrate(db)?;
    }
    if version < 12 {
        mailbox::migrate_attention(db)?;
    }
    if version < 15 {
        native_actor::migrate(db)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_rejects_cross_project_channels_and_relative_paths_without_writing() {
        use crate::workspace_model::NavigationItem;
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("navigation.sqlite3")).unwrap();
        registry.apply(&open("/repo")).unwrap();
        registry.apply(&open("/other")).unwrap();
        let channel = ChannelId::new();
        registry
            .apply(&create(channel, "/other", "Other channel"))
            .unwrap();
        let sequence = registry.model().unwrap().sequence;
        for source in [
            NavigationItem::Channel(channel),
            NavigationItem::Worktree("relative".into()),
        ] {
            let edit = ModelEdit::ReorderNavigation {
                repository: "/repo".into(),
                source: source.clone(),
                before: None,
                fallback: vec![source],
            };
            assert!(registry.apply(&edit).is_err());
            let restored = registry.model().unwrap();
            assert_eq!(restored.sequence, sequence);
            assert!(restored.navigation_orders.is_empty());
        }
    }

    #[test]
    fn navigation_reorder_is_shared_idempotent_and_survives_restart() {
        use crate::workspace_model::NavigationItem;
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("navigation.sqlite3");
        let mut registry = Registry::at(&database).unwrap();
        registry.apply(&open("/repo")).unwrap();
        let channel = ChannelId::new();
        registry
            .apply(&create(channel, "/repo", "Channel"))
            .unwrap();
        let first = NavigationItem::Worktree("/repo".into());
        let second = NavigationItem::Worktree("/repo/.worktrees/feature".into());
        let chat = NavigationItem::Channel(channel);
        let no_move = ModelEdit::ReorderNavigation {
            repository: "/repo".into(),
            source: second.clone(),
            before: None,
            fallback: vec![chat.clone(), first.clone(), second.clone()],
        };
        assert!(!registry.apply(&no_move).unwrap().changed);
        assert!(registry.model().unwrap().navigation_orders.is_empty());
        let edit = ModelEdit::ReorderNavigation {
            repository: "/repo".into(),
            source: second.clone(),
            before: Some(chat.clone()),
            fallback: vec![chat.clone(), first.clone(), second.clone()],
        };
        let applied = registry.apply(&edit).unwrap();
        assert!(applied.changed);
        assert_eq!(
            applied.model.navigation_orders[0].items,
            vec![second.clone(), chat.clone(), first.clone()]
        );
        assert!(!registry.apply(&edit).unwrap().changed);
        let mut folded = WorkspaceModel::default();
        folded.fold(applied.model.sequence, &edit);
        assert_eq!(folded.navigation_orders, applied.model.navigation_orders);
        drop(registry);
        let mut registry = Registry::at(&database).unwrap();
        assert_eq!(
            registry.model().unwrap().navigation_orders,
            applied.model.navigation_orders
        );
        // A stale window moves one sibling without replacing another window's order.
        let next = ModelEdit::ReorderNavigation {
            repository: "/repo".into(),
            source: first.clone(),
            before: Some(second.clone()),
            fallback: vec![chat.clone(), first.clone(), second.clone()],
        };
        assert_eq!(
            registry.apply(&next).unwrap().model.navigation_orders[0].items,
            vec![first, second, chat]
        );
        let reset = ModelEdit::ResetNavigation {
            repository: "/repo".into(),
        };
        assert!(registry.apply(&reset).unwrap().changed);
        assert!(!registry.apply(&reset).unwrap().changed);
        drop(registry);
        let mut registry = Registry::at(&database).unwrap();
        assert!(registry.model().unwrap().navigation_orders.is_empty());
        assert!(registry.apply(&edit).unwrap().changed);
        registry
            .apply(&ModelEdit::CloseProject {
                repository: "/repo".into(),
            })
            .unwrap();
        assert!(registry.model().unwrap().navigation_orders.is_empty());
    }

    #[test]
    fn inferred_names_upgrade_once_and_manual_renames_win_even_when_text_matches() {
        let (mut registry, _) = registry();
        registry.apply(&open("/project")).unwrap();
        let id = ChannelId::new();
        registry
            .apply(&ModelEdit::CreateChannel {
                id,
                repository: "/project".into(),
                title: String::new(),
                title_source: Default::default(),
            })
            .unwrap();
        let suggest = |title: &str, source| ModelEdit::SuggestChannelTitle {
            id,
            title: title.into(),
            source,
        };
        assert!(
            registry
                .apply(&suggest("Fix navigation", ChannelTitleSource::Prompt))
                .unwrap()
                .changed
        );
        assert!(
            registry
                .apply(&suggest(
                    "Repair channel navigation",
                    ChannelTitleSource::Provider
                ))
                .unwrap()
                .changed
        );
        assert!(
            !registry
                .apply(&suggest("Later prompt", ChannelTitleSource::Prompt))
                .unwrap()
                .changed
        );
        assert!(
            registry
                .apply(&ModelEdit::RetitleChannel {
                    id,
                    title: "Repair channel navigation".into()
                })
                .unwrap()
                .changed
        );
        assert!(
            !registry
                .apply(&suggest(
                    "Stale provider title",
                    ChannelTitleSource::Provider
                ))
                .unwrap()
                .changed
        );
        let channel = registry.model().unwrap().channels.remove(0);
        assert_eq!(channel.title, "Repair channel navigation");
        assert_eq!(channel.title_source, ChannelTitleSource::Manual);
        registry.apply(&ModelEdit::DeleteChannel { id }).unwrap();
        assert!(
            !registry
                .apply(&suggest("Do not resurrect", ChannelTitleSource::Provider))
                .unwrap()
                .changed
        );
    }

    #[test]
    fn legacy_channel_names_migrate_as_authoritative() {
        let (mut registry, directory) = registry();
        registry.apply(&open("/project")).unwrap();
        let id = ChannelId::new();
        registry
            .apply(&ModelEdit::CreateChannel {
                id,
                repository: "/project".into(),
                title: "My channel".into(),
                title_source: Default::default(),
            })
            .unwrap();
        registry
            .db
            .execute_batch(
                "ALTER TABLE channels DROP COLUMN title_source; PRAGMA user_version = 2;",
            )
            .unwrap();
        drop(registry);
        let mut restored = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert!(
            !restored
                .apply(&ModelEdit::SuggestChannelTitle {
                    id,
                    title: "Provider title".into(),
                    source: ChannelTitleSource::Provider
                })
                .unwrap()
                .changed
        );
        assert_eq!(restored.model().unwrap().channels[0].title, "My channel");
        assert_eq!(
            restored.model().unwrap().channels[0].title_source,
            ChannelTitleSource::Manual
        );
    }

    fn registry() -> (Registry, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        (registry, directory)
    }

    fn open(repository: &str) -> ModelEdit {
        ModelEdit::OpenProject {
            kind: Default::default(),
            repository: PathBuf::from(repository),
            name: repository.rsplit('/').next().unwrap().to_owned(),
        }
    }

    fn create(id: ChannelId, repository: &str, title: &str) -> ModelEdit {
        ModelEdit::CreateChannel {
            id,
            repository: PathBuf::from(repository),
            title: title.to_owned(),
            title_source: Default::default(),
        }
    }

    /// Every real edit advances the sequence a watcher orders edges by, and
    /// creating a channel that already exists is not a real edit: the CLI, the
    /// window that made it and a reconnecting window all send the same create.
    #[test]
    fn project_kind_survives_registry_reopen_and_upserts_until_explicit_removal() {
        let (mut registry, directory) = registry();
        let path = directory.path().join("model.sqlite3");
        registry
            .apply(&ModelEdit::OpenProject {
                repository: "/folder".into(),
                name: "Folder".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        drop(registry);
        let mut registry = Registry::at(&path).unwrap();
        let mut observed = registry.model().unwrap();
        assert_eq!(observed.projects[0].kind, ProjectKind::Folder);
        let repeat = ModelEdit::ReopenProject {
            repository: "/folder".into(),
            name: "Renamed".into(),
            kind: ProjectKind::Git,
        };
        let applied = registry.apply(&repeat).unwrap();
        observed.fold(applied.model.sequence, &repeat);
        assert_eq!(applied.model.projects[0].kind, ProjectKind::Folder);
        assert_eq!(observed.projects[0].kind, ProjectKind::Folder);
        registry
            .apply(&ModelEdit::CloseProject {
                repository: "/folder".into(),
            })
            .unwrap();
        let reopened = registry.apply(&repeat).unwrap();
        assert_eq!(reopened.model.projects[0].kind, ProjectKind::Git);
    }

    #[test]
    fn registry_project_kind_migration_defaults_existing_rows_to_git() {
        let (mut registry, directory) = registry();
        registry.apply(&open("/old/.git")).unwrap();
        registry
            .db
            .execute_batch("ALTER TABLE projects DROP COLUMN kind;")
            .unwrap();
        drop(registry);
        let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(registry.model().unwrap().projects[0].kind, ProjectKind::Git);
    }

    #[test]
    fn sequence_advances_once_per_real_edit_and_a_repeated_create_is_not_one() {
        let (mut registry, _directory) = registry();
        assert_eq!(registry.model().unwrap().sequence, 0);
        let first = registry.apply(&open("/tmp/repo")).unwrap();
        assert!(first.changed);
        assert_eq!(first.model.sequence, 1);

        let id = ChannelId::new();
        let created = registry.apply(&create(id, "/tmp/repo", "probe")).unwrap();
        assert!(created.changed);
        assert_eq!(created.model.sequence, 2);
        assert_eq!(created.model.channels.len(), 1);

        let repeated = registry.apply(&create(id, "/tmp/repo", "other")).unwrap();
        assert!(!repeated.changed, "a create is idempotent by id");
        assert_eq!(
            repeated.model.sequence, 2,
            "a no-op cannot bump the sequence"
        );
        assert_eq!(repeated.model.channels.len(), 1);
        assert_eq!(
            repeated.model.channels[0].title, "probe",
            "the first create's title stands; a repeat is not a retitle"
        );
    }

    /// A channel lives inside a project. Closing the project has to take its
    /// channels with it, or they would survive as rows no sidebar can reach.
    #[test]
    fn closing_a_project_takes_its_channels_and_leaves_its_neighbours() {
        let (mut registry, _directory) = registry();
        registry.apply(&open("/tmp/one")).unwrap();
        registry.apply(&open("/tmp/two")).unwrap();
        let doomed = ChannelId::new();
        let survivor = ChannelId::new();
        registry.apply(&create(doomed, "/tmp/one", "a")).unwrap();
        registry.apply(&create(survivor, "/tmp/two", "b")).unwrap();

        let closed = registry
            .apply(&ModelEdit::CloseProject {
                repository: PathBuf::from("/tmp/one"),
            })
            .unwrap();
        assert!(closed.changed);
        assert!(closed.model.channel(doomed).is_none());
        assert!(closed.model.channel(survivor).is_some());
        assert_eq!(closed.model.projects.len(), 1);
        assert_eq!(closed.model.closed_projects, [PathBuf::from("/tmp/one")]);

        let reopened = Registry::at(&_directory.path().join("model.sqlite3"))
            .unwrap()
            .model()
            .unwrap();
        assert_eq!(reopened.closed_projects, [PathBuf::from("/tmp/one")]);
        let stale = registry.apply(&open("/tmp/one")).unwrap();
        assert!(!stale.changed, "a queued upsert cannot undo a removal");
        assert_eq!(stale.model.closed_projects, [PathBuf::from("/tmp/one")]);
        let opened = registry
            .apply(&ModelEdit::ReopenProject {
                kind: Default::default(),
                repository: PathBuf::from("/tmp/one"),
                name: "one".into(),
            })
            .unwrap();
        assert!(opened.model.closed_projects.is_empty());
    }

    /// An edit that changes nothing must be indistinguishable from not having
    /// been sent, because a retitle racing a delete and a second window's
    /// duplicate delete both arrive as one.
    #[test]
    fn an_edit_on_an_unknown_channel_changes_nothing_and_does_not_advance() {
        let (mut registry, _directory) = registry();
        registry.apply(&open("/tmp/repo")).unwrap();
        let sequence = registry.model().unwrap().sequence;
        let unknown = ChannelId::new();

        let retitled = registry
            .apply(&ModelEdit::RetitleChannel {
                id: unknown,
                title: "ghost".into(),
            })
            .unwrap();
        assert!(!retitled.changed);
        assert_eq!(retitled.model.sequence, sequence);
        assert!(retitled.model.channel(unknown).is_none());

        let deleted = registry
            .apply(&ModelEdit::DeleteChannel { id: unknown })
            .unwrap();
        assert!(!deleted.changed);
        assert_eq!(deleted.model.sequence, sequence);
    }

    /// A retitle to the title the channel already has is equally a no-op: every
    /// window derives a title from its own cards, and the second one to derive
    /// the same title must not wake every other window.
    #[test]
    fn retitling_to_the_current_title_changes_nothing() {
        let (mut registry, _directory) = registry();
        registry.apply(&open("/tmp/repo")).unwrap();
        let id = ChannelId::new();
        registry.apply(&create(id, "/tmp/repo", "")).unwrap();
        let renamed = registry
            .apply(&ModelEdit::RetitleChannel {
                id,
                title: "Explain the parser".into(),
            })
            .unwrap();
        assert!(renamed.changed);

        let repeated = registry
            .apply(&ModelEdit::RetitleChannel {
                id,
                title: "Explain the parser".into(),
            })
            .unwrap();
        assert!(!repeated.changed);
        assert_eq!(repeated.model.sequence, renamed.model.sequence);
    }

    /// The identity is what lets a window tell "the registry I have been
    /// talking to" from a replaced daemon or a wiped data directory. It must be
    /// minted once and survive every reopening, because a window that saw a new
    /// identity treats everything it holds as never shared.
    #[test]
    fn the_identity_is_minted_once_and_never_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.sqlite3");
        let minted = {
            let registry = Registry::at(&path).unwrap();
            registry.model().unwrap().identity
        };
        assert_ne!(
            minted,
            crate::workspace_model::NO_REGISTRY,
            "a real registry never presents the nil identity, which is what a \
             window reads as having no registry at all"
        );
        let reopened = Registry::at(&path).unwrap().model().unwrap().identity;
        assert_eq!(minted, reopened);

        let replacement = Registry::at(&directory.path().join("other.sqlite3"))
            .unwrap()
            .model()
            .unwrap()
            .identity;
        assert_ne!(
            minted, replacement,
            "a different store is a different registry, or a window would trust \
             a wiped data directory about channels it never shared with it"
        );
    }

    /// A rank is the creation order two windows agree on, so it has to keep
    /// increasing and a deleted channel's number can never come back: reusing it
    /// would make a brand new channel sort where the deleted one used to be.
    #[test]
    fn a_rank_increases_and_is_never_reused_after_a_delete() {
        let (mut registry, _directory) = registry();
        registry.apply(&open("/tmp/repo")).unwrap();
        let older = ChannelId::new();
        let newer = ChannelId::new();
        registry.apply(&create(older, "/tmp/repo", "a")).unwrap();
        let second = registry.apply(&create(newer, "/tmp/repo", "b")).unwrap();
        let older_rank = second.model.channel(older).unwrap().ordinal;
        let newer_rank = second.model.channel(newer).unwrap().ordinal;
        assert!(
            newer_rank > older_rank,
            "a later channel outranks an earlier"
        );

        registry
            .apply(&ModelEdit::DeleteChannel { id: newer })
            .unwrap();
        let replacement = ChannelId::new();
        let after = registry
            .apply(&create(replacement, "/tmp/repo", "c"))
            .unwrap();
        assert!(
            after.model.channel(replacement).unwrap().ordinal > newer_rank,
            "the deleted channel's rank is retired with it"
        );
    }

    /// A channel in a project nobody has open is reachable from no sidebar, so
    /// creating one is refused rather than stored where it cannot be seen.
    #[test]
    fn a_channel_needs_its_project() {
        let (mut registry, _directory) = registry();
        let error = registry
            .apply(&create(ChannelId::new(), "/tmp/unknown", "orphan"))
            .expect_err("an unowned channel is refused");
        assert!(error.contains("no project owns"), "{error}");
        assert_eq!(registry.model().unwrap().sequence, 0);
    }

    /// The registry is durable: a channel the CLI created with no window running
    /// has to still be there when a window finally starts, which means surviving
    /// the daemon that created it.
    #[test]
    fn the_model_survives_reopening_the_store() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.sqlite3");
        let id = ChannelId::new();
        {
            let mut registry = Registry::at(&path).unwrap();
            registry.apply(&open("/tmp/repo")).unwrap();
            registry.apply(&create(id, "/tmp/repo", "probe")).unwrap();
        }
        let reopened = Registry::at(&path).unwrap();
        let model = reopened.model().unwrap();
        assert_eq!(model.channel(id).unwrap().title, "probe");
        assert_eq!(
            model.channel(id).unwrap().ordinal,
            1,
            "the rank is durable too"
        );
    }
}
