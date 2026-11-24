use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Transaction, TransactionBehavior};

use super::{Recorder, Registry, Scope, repository_key, schema};
use crate::{
    process::{ProcessExecutor, SystemProcessExecutor},
    repository::{CancellationToken, RepositoryService, parse_worktree_list},
    workspace_model::delta::EditEffects,
};

const BATCH: usize = 1_000;

// Recheck the durable guards at deletion: an agent or automation can claim
// the view without changing its revision. Disabled definitions still name it.
const UNUSED_WORKTREE: &str = "s.worktree IS NOT NULL
    AND NOT EXISTS (SELECT 1 FROM panes WHERE scope = s.id)
    AND NOT EXISTS (SELECT 1 FROM retired_panes WHERE scope = s.id)
    AND s.id NOT IN (SELECT scope FROM view_operations)
    AND NOT EXISTS (SELECT 1 FROM agents a WHERE a.closed = 0
        AND json_extract(a.record, '$.placement.Worktree.repository') = s.repository
        AND json_extract(a.record, '$.placement.Worktree.path') = s.worktree)
    AND NOT EXISTS (SELECT 1 FROM automations a
        WHERE a.deleted_ms IS NULL AND a.repository = s.repository
        AND (json_extract(a.record, '$.definition.agent.target.directory') = s.worktree
            OR (json_extract(a.record, '$.definition.agent.target.placement.Worktree.repository') = s.repository
                AND json_extract(a.record, '$.definition.agent.target.placement.Worktree.path') = s.worktree)))";

type RemovalEdge = Option<(u64, EditEffects)>;

impl Registry {
    /// Compacts one bounded batch on the collector's dedicated connection.
    pub(crate) fn compact_worktree_views(&self) -> Result<RemovalEdge, String> {
        self.compact_worktree_scopes(None)
    }

    /// Compacts the exact removed worktree on a dedicated connection.
    pub(crate) fn compact_worktree_view(
        &self,
        repository: &Path,
        path: &Path,
    ) -> Result<RemovalEdge, String> {
        let path = path
            .to_str()
            .ok_or("The worktree path is not valid UTF-8")?;
        self.compact_worktree_scopes(Some([repository_key(repository)?, path]))
    }

    fn compact_worktree_scopes(&self, target: Option<[&str; 2]>) -> Result<RemovalEdge, String> {
        let mut statement = self
            .db
            .prepare_cached(&format!(
                "SELECT s.id, s.revision, s.repository, s.worktree FROM view_scopes s
                 WHERE {UNUSED_WORKTREE} {} ORDER BY s.id",
                if target.is_some() {
                    "AND s.repository = ?1 AND s.worktree = ?2"
                } else {
                    ""
                },
            ))
            .map_err(|error| error.to_string())?;
        let candidates = statement
            .query_map(
                rusqlite::params_from_iter(target.into_iter().flatten()),
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        PathBuf::from(row.get::<_, String>(2)?),
                        PathBuf::from(row.get::<_, String>(3)?),
                    ))
                },
            )
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
            .map_err(|error| error.to_string())?;
        drop(statement);

        // Check Git and the filesystem before acquiring the SQLite writer.
        // Limit rows that can be deleted, so live candidates cannot starve a
        // later dead one. One Git inventory serves all candidates of a project.
        let mut registrations = HashMap::new();
        let dead: Vec<_> = candidates
            .into_iter()
            .filter_map(|(id, revision, repository, path)| {
                if !directory_absent(&path) {
                    return None;
                }
                let registered = registrations
                    .entry(repository)
                    .or_insert_with_key(|repository| registered_worktrees(repository));
                registered
                    .as_ref()
                    .is_some_and(|paths| !paths.contains(&path))
                    .then_some(((id, revision), path))
            })
            .take(BATCH)
            // Recheck after the Git reads in case a checkout appeared while
            // we were selecting the batch. Keep path I/O off the writer.
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|(basis, path)| directory_absent(&path).then_some(basis))
            .collect();
        if dead.is_empty() {
            return Ok(None);
        }
        self.delete_compacted_scopes(&dead)
    }

    fn delete_compacted_scopes(&self, dead: &[(i64, i64)]) -> Result<RemovalEdge, String> {
        let candidates = serde_json::to_string(dead).map_err(|error| error.to_string())?;
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        schema::require_current(&tx)?;
        let rows = tx
            .prepare(&format!(
                "DELETE FROM view_scopes AS s WHERE (s.id, s.revision) IN (
                SELECT json_extract(value, '$[0]'), json_extract(value, '$[1]') FROM json_each(?1)
             ) AND {UNUSED_WORKTREE} RETURNING repository, worktree, revision"
            ))
            .and_then(|mut statement| {
                statement
                    .query_map([candidates], |row| {
                        Ok((
                            PathBuf::from(row.get::<_, String>(0)?),
                            PathBuf::from(row.get::<_, String>(1)?),
                            row.get::<_, u64>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(|error| error.to_string())?;
        if rows.is_empty() {
            return Ok(None);
        }

        let mut recorder = Recorder::default();
        for (repository, path, revision) in rows {
            recorder.revised(&Scope::Worktree { repository, path }, Some(revision), None);
        }
        let effects = recorder.finish(&tx)?;
        // Publish the sequence of this transaction, even if another connection
        // recreates a scope before the edge is delivered.
        let sequence = tx
            .query_row(
                "UPDATE metadata SET sequence = sequence + 1 WHERE key = 0 RETURNING sequence",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        super::super::execution_deltas::discard_execution_delta_in(&tx)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(Some((sequence, effects)))
    }
}

fn directory_absent(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(metadata) => !metadata.is_dir(),
        Err(error) => matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
        ),
    }
}

/// An unreadable inventory preserves the project's candidates for another pass.
fn registered_worktrees(repository: &Path) -> Option<HashSet<PathBuf>> {
    // A key can be the common Git directory, a checkout, or a plain folder.
    // Check the markers so a folder that has since acquired Git still counts.
    if !repository.try_exists().ok()?
        || (!repository.join(".git").try_exists().ok()?
            && !repository.join("HEAD").try_exists().ok()?)
    {
        return Some(HashSet::new());
    }
    let command = RepositoryService::system("git")
        .worktree_list_command(repository)
        .timeout(Duration::from_secs(30));
    let output = SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .and_then(|output| output.require_success(&command.program))
        .ok()?;
    let worktrees = parse_worktree_list(&output.stdout).ok()?;
    Some(worktrees.into_iter().map(|tree| tree.path).collect())
}
