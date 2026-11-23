//! The startup repair that retires worktree strips.
//!
//! Before channels carried a checkout, a worktree's panes lived in a strip of
//! their own, outside every channel. Windows now open a worktree in its
//! checkout's channel, so the daemon moves each strip that still holds panes,
//! or that an open agent is placed in, into that channel once, when it opens
//! the store. Every pane keeps its id, order, width and filing, and every agent
//! its session: the move is the same transfer a user's drag commits.
//!
//! A strip whose worktree is gone from disk is never dropped. Its channel
//! records the missing worktree as its checkout, so its panes start in the
//! project's main checkout until the worktree is recreated.
use super::{Registry, checkout_key, repository_key, take_ordinal, views};
use crate::model::{ChannelId, ChannelTitleSource};
use crate::workspace_model::views::{Change, Scope};
use crate::workspace_model::{ModelEdit, NavigationItem, canonical_checkout, checkout_branch};
use rusqlite::{OptionalExtension, Transaction, params};
use std::path::{Path, PathBuf};

/// The channel a strip's panes move to.
#[derive(Clone, Debug, PartialEq)]
struct Home {
    /// `None` for the project's main checkout. A worktree that no longer
    /// exists keeps its recorded path.
    checkout: Option<PathBuf>,
    /// The branch, else the checkout's directory name.
    title: String,
}

impl Home {
    fn of(repository: &Path, path: &Path) -> Self {
        let checkout = match canonical_checkout(repository, path) {
            Ok(checkout) => checkout,
            Err(_) if main_location(repository, path) => None,
            Err(_) => Some(path.to_path_buf()),
        };
        let title = checkout_branch(repository, path)
            .or_else(|| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| path.display().to_string());
        Self { checkout, title }
    }
}

/// Whether `path` is where the project's main checkout lives, even when it is
/// missing: the folder itself, or the directory holding a `.git` repository.
fn main_location(repository: &Path, path: &Path) -> bool {
    path == repository
        || (repository.file_name().is_some_and(|name| name == ".git")
            && repository.parent() == Some(path))
}

/// What the repair changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Adopted {
    pub(crate) strips: usize,
    pub(crate) panes: usize,
    pub(crate) created_channels: usize,
    /// Panes the transfer refused, such as an agent pane whose process changed
    /// since it was saved. They stay in their strip, which still opens.
    pub(crate) kept: usize,
}

impl Registry {
    /// Moves every worktree strip that holds panes, or that an open agent is
    /// placed in, into its checkout's channel, in one transaction. Running it
    /// again changes nothing, because a repaired strip holds nothing.
    pub(crate) fn adopt_worktree_views(&mut self) -> Result<Adopted, String> {
        let strips = strips(&self.db)?;
        if strips.is_empty() {
            return Ok(Adopted::default());
        }
        // Git's files are read before the write transaction begins.
        let homes: Vec<(Scope, Home)> = strips
            .into_iter()
            .map(|(repository, path)| {
                let home = Home::of(&repository, &path);
                (Scope::Worktree { repository, path }, home)
            })
            .collect();
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        super::schema::require_current(&transaction)?;
        let mut adopted = Adopted::default();
        for (scope, home) in &homes {
            adopt(&transaction, scope, home, &mut adopted)?;
        }
        if adopted != Adopted::default() {
            transaction
                .execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(adopted)
    }
}

/// The worktree strips of open projects that hold a pane or an open agent.
fn strips(db: &rusqlite::Connection) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut statement = db
        .prepare(
            "SELECT s.repository, s.worktree FROM view_scopes s
                 JOIN projects p ON p.repository = s.repository
             WHERE s.worktree IS NOT NULL
               AND EXISTS(SELECT 1 FROM panes WHERE panes.scope = s.id)
             UNION
             SELECT json_extract(a.record, '$.placement.Worktree.repository'),
                    json_extract(a.record, '$.placement.Worktree.path')
               FROM agents a
                 JOIN projects p
                   ON p.repository = json_extract(a.record, '$.placement.Worktree.repository')
              WHERE a.closed = 0
                AND json_type(a.record, '$.placement.Worktree') = 'object'
             ORDER BY 1, 2",
        )
        .map_err(|error| error.to_string())?;
    statement
        .query_map([], |row| {
            Ok((
                PathBuf::from(row.get::<_, String>(0)?),
                PathBuf::from(row.get::<_, String>(1)?),
            ))
        })
        .and_then(Iterator::collect)
        .map_err(|error| error.to_string())
}

fn adopt(
    transaction: &Transaction<'_>,
    scope: &Scope,
    home: &Home,
    adopted: &mut Adopted,
) -> Result<(), String> {
    let Scope::Worktree { repository, path } = scope else {
        return Ok(());
    };
    let channel = home_channel(transaction, repository, home, adopted)?;
    let destination = Scope::Channel(channel);
    let mut recorder = views::Recorder::default();
    let strip = views::read(transaction, scope)?;
    let mut moved = 0;
    for pane in &strip.panes {
        let change = Change::Transfer {
            id: pane.id,
            destination: destination.clone(),
            before: None,
        };
        if let Some(reason) = views::refusal(transaction, scope, &change)? {
            tracing::warn!(pane = %pane.id, %reason, "a worktree strip's pane was left in place");
            adopted.kept += 1;
            continue;
        }
        let edit = ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: views::read(transaction, scope)?.revision,
            operation: uuid::Uuid::new_v4(),
            change,
        };
        views::apply_recorded(transaction, &edit, &mut recorder)?;
        moved += 1;
    }
    let left: Vec<crate::model::CardId> = views::read(transaction, scope)?
        .panes
        .iter()
        .map(|pane| pane.id)
        .collect();
    place_agents(transaction, scope, &destination, &left)?;
    place_in_navigation(transaction, repository, path, channel)?;
    adopted.strips += 1;
    adopted.panes += moved;
    Ok(())
}

/// The worktree's oldest channel, or a new one named after its branch. A main
/// checkout's strip gets a channel of its own rather than joining whichever
/// conversation happens to be oldest.
fn home_channel(
    transaction: &Transaction<'_>,
    repository: &Path,
    home: &Home,
    adopted: &mut Adopted,
) -> Result<ChannelId, String> {
    let key = repository_key(repository)?;
    if let Some(checkout) = &home.checkout {
        let existing: Option<String> = transaction
            .query_row(
                "SELECT id FROM channels WHERE repository = ?1 AND checkout = ?2
                 ORDER BY ordinal, id LIMIT 1",
                params![key, checkout_key(checkout)?],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some(id) = existing {
            return super::parse_channel_id(&id);
        }
    }
    let id = ChannelId::new();
    let ordinal = take_ordinal(transaction, "next_channel_ordinal")?;
    transaction
        .execute(
            "INSERT INTO channels(id, repository, title, ordinal, title_source, checkout)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id.to_string(),
                key,
                home.title,
                ordinal,
                ChannelTitleSource::Manual as i64,
                home.checkout.as_deref().map(checkout_key).transpose()?,
            ],
        )
        .map_err(|error| error.to_string())?;
    adopted.created_channels += 1;
    Ok(id)
}

/// Agents still placed in the strip move with it, unless their own card stayed
/// behind. The rest of each record, including a stop's `process_end`, is kept.
fn place_agents(
    transaction: &Transaction<'_>,
    scope: &Scope,
    destination: &Scope,
    left: &[crate::model::CardId],
) -> Result<(), String> {
    let Scope::Worktree { repository, path } = scope else {
        return Ok(());
    };
    let placed: Vec<String> = transaction
        .prepare(
            "SELECT id FROM agents
              WHERE closed = 0
                AND json_extract(record, '$.placement.Worktree.repository') = ?1
                AND json_extract(record, '$.placement.Worktree.path') = ?2",
        )
        .and_then(|mut statement| {
            statement
                .query_map(
                    params![repository_key(repository)?, repository_key(path)?],
                    |row| row.get(0),
                )?
                .collect()
        })
        .map_err(|error| error.to_string())?;
    let placement = serde_json::to_string(destination).map_err(|error| error.to_string())?;
    for id in placed {
        let agent = id
            .parse::<uuid::Uuid>()
            .map_err(|error| format!("stored agent id `{id}` is unreadable: {error}"))?;
        if left.contains(&crate::model::CardId::from_uuid(agent)) {
            continue;
        }
        transaction
            .execute(
                "UPDATE agents SET record = json_set(record, '$.placement', json(?2)) WHERE id = ?1",
                params![id, placement],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// A project arranged by hand shows the channel where its worktree was.
fn place_in_navigation(
    transaction: &Transaction<'_>,
    repository: &Path,
    path: &Path,
    channel: ChannelId,
) -> Result<(), String> {
    let key = repository_key(repository)?;
    let stored: Option<String> = transaction
        .query_row(
            "SELECT items FROM navigation_orders WHERE repository = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some(stored) = stored else {
        return Ok(());
    };
    let mut items: Vec<NavigationItem> =
        serde_json::from_str(&stored).map_err(|error| error.to_string())?;
    if items.contains(&NavigationItem::Channel(channel)) {
        return Ok(());
    }
    let Some(at) = items
        .iter()
        .position(|item| matches!(item, NavigationItem::Worktree(listed) if listed == path))
    else {
        return Ok(());
    };
    items.insert(at, NavigationItem::Channel(channel));
    transaction
        .execute(
            "UPDATE navigation_orders SET items = ?2 WHERE repository = ?1",
            params![
                key,
                serde_json::to_string(&items).map_err(|error| error.to_string())?
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}
