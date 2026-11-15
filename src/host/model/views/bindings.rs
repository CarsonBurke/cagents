//! Durable generation fences for terminal panes. Neither a missing resident
//! nor eviction from the host's publication cache erases a newer generation.
use super::*;
use crate::{
    model::{CardId, CardKind, TerminalSession},
    workspace_model::views::{CapturedTerminalSource, Pane, TerminalRemovalBasis},
};

pub(in crate::host::model) const TABLE: &str = "CREATE TABLE terminal_pane_bindings(
    pane TEXT PRIMARY KEY REFERENCES panes(id) ON DELETE CASCADE,
    birth_sequence INTEGER NOT NULL CHECK(birth_sequence >= 0),
    birth_revision INTEGER NOT NULL CHECK(birth_revision >= 0),
    source TEXT NOT NULL,
    basis TEXT
) STRICT";

pub(in crate::host::model) fn migrate(db: &Connection) -> Result<(), String> {
    db.execute_batch(TABLE).map_err(|e| e.to_string())?;
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM panes WHERE kind='terminal'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if count != 0 {
        // Cold conversions may discard an old view revision clock. Metadata's
        // registry-wide watermark survives them, and never grants an old
        // gesture authority over a newly initialized generation.
        db.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
            .map_err(|e| e.to_string())?;
        db.execute(
            "INSERT INTO terminal_pane_bindings(pane,birth_sequence,birth_revision,source,basis)
             SELECT p.id,m.sequence,s.revision,p.descriptor,NULL
             FROM panes p JOIN view_scopes s ON s.id=p.scope JOIN metadata m ON m.key=0
             WHERE p.kind='terminal'",
            [],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[derive(Clone)]
pub(in crate::host::model) struct Binding {
    pub birth_sequence: u64,
    pub birth_revision: u64,
    pub source: TerminalSession,
    pub basis: Option<TerminalRemovalBasis>,
}

pub(in crate::host::model) fn read(
    db: &Connection,
    pane: CardId,
) -> Result<Option<Binding>, String> {
    let raw: Option<(i64, i64, String, Option<String>)> = db
        .query_row(
            "SELECT birth_sequence,birth_revision,source,basis FROM terminal_pane_bindings WHERE pane=?1",
            [pane.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    raw.map(|(sequence, revision, source, basis)| {
        let CardKind::Terminal(source) =
            serde_json::from_str(&source).map_err(|e| e.to_string())?
        else {
            return Err("Stored terminal binding has another pane kind".into());
        };
        Ok(Binding {
            birth_sequence: sequence as u64,
            birth_revision: revision as u64,
            source,
            basis: basis
                .map(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
                .transpose()?,
        })
    })
    .transpose()
}

pub(in crate::host::model) fn write(
    db: &Connection,
    pane: &Pane,
    sequence: u64,
    revision: u64,
    basis: Option<&TerminalRemovalBasis>,
) -> Result<(), String> {
    db.execute(
        "INSERT INTO terminal_pane_bindings(pane,birth_sequence,birth_revision,source,basis)
         VALUES(?1,?2,?3,?4,?5) ON CONFLICT(pane) DO UPDATE SET
         birth_sequence=excluded.birth_sequence,birth_revision=excluded.birth_revision,
         source=excluded.source,basis=excluded.basis",
        params![
            pane.id.to_string(),
            sequence as i64,
            revision as i64,
            serde_json::to_string(&pane.kind).map_err(|e| e.to_string())?,
            basis
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| e.to_string())?
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Called after pane rows are stored, in the same edit transaction. Runtime
/// publication replaces the unresolved basis before committing its generation.
pub(in crate::host::model) fn sync(
    db: &Connection,
    before: &View,
    after: &View,
) -> Result<(), String> {
    for pane in &after.panes {
        if !matches!(pane.kind, CardKind::Terminal(_)) {
            db.execute(
                "DELETE FROM terminal_pane_bindings WHERE pane=?1",
                [pane.id.to_string()],
            )
            .map_err(|e| e.to_string())?;
            continue;
        }
        let preserved = before.scope == after.scope
            && before
                .panes
                .iter()
                .find(|p| p.id == pane.id)
                .is_some_and(|p| crate::workspace_model::views::same_removal_source(p, pane))
            && read(db, pane.id)?.is_some();
        if !preserved {
            let sequence: i64 = db
                .query_row("SELECT sequence+1 FROM metadata WHERE key=0", [], |r| {
                    r.get(0)
                })
                .map_err(|e| e.to_string())?;
            let basis = read(db, pane.id)?
                .filter(|binding| {
                    let previous = Pane {
                        kind: CardKind::Terminal(binding.source.clone()),
                        ..pane.clone()
                    };
                    crate::workspace_model::views::same_removal_source(&previous, pane)
                })
                .and_then(|binding| binding.basis);
            write(db, pane, sequence as u64, after.revision, basis.as_ref())?;
        }
    }
    Ok(())
}

pub(in crate::host::model) fn captured(
    db: &Connection,
    capture: &CapturedTerminalSource,
    expected: Option<u64>,
) -> Result<Binding, String> {
    let identity: String = db
        .query_row("SELECT identity FROM metadata WHERE key=0", [], |r| {
            r.get(0)
        })
        .map_err(|e| e.to_string())?;
    if identity != capture.registry.to_string()
        || project(db, &capture.scope)? != capture.repository
        || owner(db, capture.before.id)?.as_ref() != Some(&capture.scope)
    {
        return Err(Registry::VIEW_REMOVAL_CONFLICT.into());
    }
    let view = super::read(db, &capture.scope)?;
    let current = view
        .panes
        .iter()
        .find(|p| p.id == capture.before.id)
        .ok_or(Registry::VIEW_REMOVAL_CONFLICT)?;
    if !crate::workspace_model::views::same_removal_source(&capture.before, current)
        || current.pending_terminal_start
    {
        return Err(Registry::VIEW_REMOVAL_CONFLICT.into());
    }
    let binding = read(db, capture.before.id)?.ok_or(Registry::VIEW_REMOVAL_CONFLICT)?;
    if binding.birth_sequence > capture.model_sequence
        || binding.birth_revision > capture.view_revision
        || expected.is_some_and(|token| token != binding.birth_sequence)
    {
        return Err(Registry::VIEW_REMOVAL_CONFLICT.into());
    }
    Ok(binding)
}
