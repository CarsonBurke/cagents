//! Durable manual navigation order, independent of channel creation ranks.
use super::repository_key;
use crate::workspace_model::{
    ModelEdit, NavigationItem, merge_navigation_fallback, reorder_navigation,
};
use rusqlite::{OptionalExtension, Transaction, params};

const MAX_ITEMS: usize = 4096;

pub(super) fn apply(transaction: &Transaction<'_>, edit: &ModelEdit) -> Result<bool, String> {
    Ok(match edit {
        ModelEdit::ResetNavigation { repository } => {
            transaction
                .execute(
                    "DELETE FROM navigation_orders WHERE repository = ?1",
                    [repository_key(repository)?],
                )
                .map_err(|error| error.to_string())?
                > 0
        }
        ModelEdit::ReorderNavigation {
            repository,
            source,
            before,
            fallback,
        } => {
            if fallback.len() > MAX_ITEMS {
                return Err("too many navigation items".into());
            }
            let repository = repository_key(repository)?;
            let exists: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM projects WHERE repository = ?1)",
                    [repository],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            if !exists {
                false
            } else {
                let valid_channels: std::collections::HashSet<String> = transaction
                    .prepare("SELECT id FROM channels WHERE repository = ?1")
                    .map_err(|error| error.to_string())?
                    .query_map([repository], |row| row.get(0))
                    .and_then(Iterator::collect::<rusqlite::Result<_>>)
                    .map_err(|error| error.to_string())?;
                for item in fallback
                    .iter()
                    .chain(std::iter::once(source))
                    .chain(before.iter())
                {
                    match item {
                        NavigationItem::Worktree(path) if !path.is_absolute() => {
                            return Err("worktree navigation paths must be absolute".into());
                        }
                        NavigationItem::Channel(id) => {
                            if !valid_channels.contains(&id.to_string()) {
                                return Err(
                                    "navigation channel does not belong to this project".into()
                                );
                            }
                        }
                        _ => {}
                    }
                }
                let stored: Option<String> = transaction
                    .query_row(
                        "SELECT items FROM navigation_orders WHERE repository = ?1",
                        [repository],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|error| error.to_string())?;
                let mut items: Vec<NavigationItem> = stored
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(|error| error.to_string())?
                    .unwrap_or_else(|| fallback.clone());
                items.retain(|item| match item {
                    NavigationItem::Channel(id) => valid_channels.contains(&id.to_string()),
                    NavigationItem::Worktree(_) => true,
                });
                merge_navigation_fallback(&mut items, fallback);
                if items.len() > MAX_ITEMS {
                    return Err("too many retained navigation items; use Reset sidebar order in the project menu".into());
                }
                let moved = reorder_navigation(&mut items, source, before.as_ref());
                if stored.is_none() && !moved {
                    return Ok(false);
                }
                let encoded = serde_json::to_string(&items).map_err(|error| error.to_string())?;
                if stored.as_deref() == Some(encoded.as_str()) {
                    false
                } else {
                    transaction.execute("INSERT INTO navigation_orders(repository, items) VALUES (?1, ?2) ON CONFLICT(repository) DO UPDATE SET items = excluded.items", params![repository, encoded]).map_err(|error| error.to_string())? > 0
                }
            }
        }

        _ => unreachable!("navigation edits only"),
    })
}
