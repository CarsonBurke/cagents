//! The order every window lists projects in.
//!
//! One row per arranged project, keyed by its repository like the project it
//! orders, so closing a project drops it from the order through the foreign
//! key's cascade. No rows means nobody arranged the projects.
use std::{collections::HashSet, path::PathBuf};

use rusqlite::{Connection, Transaction, params};

use super::repository_key;
use crate::workspace_model::{MAX_ORDERED_PROJECTS, ModelEdit, project_order_after};

pub(super) const TABLE: &str = "CREATE TABLE project_order(
             repository TEXT PRIMARY KEY REFERENCES projects(repository) ON DELETE CASCADE,
             position INTEGER NOT NULL
         ) STRICT;";

pub(super) fn migrate(transaction: &Transaction<'_>) -> Result<(), String> {
    transaction
        .execute_batch(TABLE)
        .map_err(|error| error.to_string())
}

/// The arranged projects, first to last.
pub(super) fn read(db: &Connection) -> Result<Vec<PathBuf>, String> {
    db.prepare_cached("SELECT repository FROM project_order ORDER BY position")
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(0))
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .map(|rows| rows.into_iter().map(PathBuf::from).collect())
        .map_err(|error| error.to_string())
}

/// Applies a project order edit by [`project_order_after`]'s rule. Whether
/// the order changed.
pub(super) fn apply(transaction: &Transaction<'_>, edit: &ModelEdit) -> Result<bool, String> {
    let (ModelEdit::ReorderProjects {
        fallback: named, ..
    }
    | ModelEdit::SeedProjectOrder { order: named }) = edit
    else {
        unreachable!("project order edits only")
    };
    if named.len() > MAX_ORDERED_PROJECTS {
        return Err(format!(
            "a project order names at most {MAX_ORDERED_PROJECTS} projects"
        ));
    }
    let open: HashSet<PathBuf> = transaction
        .prepare_cached("SELECT repository FROM projects")
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(0))
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let held = read(transaction)?;
    let Some(order) = project_order_after(&held, edit, |repository| open.contains(repository))
    else {
        return Ok(false);
    };
    transaction
        .execute("DELETE FROM project_order", [])
        .map_err(|error| error.to_string())?;
    let mut insert = transaction
        .prepare_cached("INSERT INTO project_order(repository, position) VALUES (?1, ?2)")
        .map_err(|error| error.to_string())?;
    for (position, repository) in order.iter().enumerate() {
        insert
            .execute(params![repository_key(repository)?, position as i64])
            .map_err(|error| error.to_string())?;
    }
    Ok(true)
}
