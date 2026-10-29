//! Detailed history is bounded; ownership and compact replay fences are not TTLs.
use super::*;
use crate::automations::{HISTORY_RETAINED_ROWS, SkipReason, SkippedOccurrences, Trigger};

pub(super) const REMOVED_TTL_MS: u64 =
    crate::automations::REMOVED_HISTORY_RETENTION_DAYS * 24 * 60 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrecheckRetirement {
    ReleasedPending,
    Acknowledged,
    Missing,
    Protected,
}

impl PrecheckRetirement {
    pub(crate) fn is_protected(self) -> bool {
        self == Self::Protected
    }

    pub(crate) fn needs_ack(self) -> bool {
        self == Self::ReleasedPending
    }

    pub(crate) fn is_missing(self) -> bool {
        self == Self::Missing
    }
}

// SQL aliases f/a/r are shared by these two bounded writer entrypoints.
const ELIGIBLE: &str = "(a.active_firing IS NULL OR a.active_firing<>f.id)
            AND json_extract(a.record,'$.blocked') IS NULL
            AND json_extract(f.record,'$.state') NOT IN ('prechecking','accepted','running','waiting')
            AND coalesce(json_extract(f.record,'$.release_pending'),0)=0
            AND (json_extract(r.definition,'$.precheck') IS NULL
              OR (json_extract(f.record,'$.state')='skipped' AND json_extract(f.record,'$.skip') IN ('overlap','capacity','missed_deadline')
                  AND json_extract(f.record,'$.agent') IS NULL AND json_extract(f.record,'$.run') IS NULL)
              OR EXISTS(SELECT 1 FROM automation_precheck_history_cleanup c WHERE c.firing=f.id AND c.revision=f.revision))";

/// Root coordinates the published schema step; never change a historical migration.
pub(super) fn migrate(tx: &Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(
        "ALTER TABLE automation_firings ADD COLUMN previous_firing TEXT;
         WITH ordered AS (SELECT id,lag(id) OVER(PARTITION BY automation ORDER BY rowid) AS previous FROM automation_firings)
         UPDATE automation_firings SET previous_firing=(SELECT previous FROM ordered WHERE ordered.id=automation_firings.id);
         CREATE TABLE automation_retired_manual_requests (
            automation TEXT NOT NULL REFERENCES automations(id),
            request TEXT NOT NULL,
            revision INTEGER NOT NULL,
            PRIMARY KEY(automation,request)
         ) STRICT;
         CREATE TABLE automation_precheck_history_cleanup (
            firing TEXT PRIMARY KEY REFERENCES automation_firings(id) ON DELETE CASCADE,
            revision INTEGER NOT NULL,
            evidence TEXT NOT NULL CHECK(evidence IN ('retired','not_launched'))
         ) STRICT;
         CREATE INDEX automation_removed_history ON automations(deleted_ms)
             WHERE deleted_ms IS NOT NULL;",
    ).map_err(|error| error.to_string())
}

pub(super) fn require_unretired_manual_key(
    db: &Connection,
    automation: Uuid,
    key: &str,
    revision: u64,
) -> Result<(), String> {
    if !key.starts_with("manual:") {
        return Ok(());
    }
    let retired: Option<u64> = db.query_row(
        "SELECT revision FROM automation_retired_manual_requests WHERE automation=?1 AND request=?2",
        params![automation.to_string(),key], |row| row.get(0),
    ).optional().map_err(|error| error.to_string())?;
    if let Some(previous) = retired {
        if previous != revision {
            return Err(
                "This manual request ID already belongs to another automation revision".into(),
            );
        }
        return Err("This manual request was already recorded; its detailed history expired and it will not run again. Use a new request ID only for an intentional new run".into());
    }
    Ok(())
}

fn cleanup_clear(db: &Connection, firing: &Firing) -> Result<bool, String> {
    let definition = read_definition(db, firing.automation, firing.revision)?;
    // These exact scheduler outcomes never acquire a precheck owner. This also
    // recognizes published legacy rows without inventing a filesystem ACK.
    if definition.precheck.is_none()
        || (firing.state == FiringState::Skipped
            && firing.agent.is_none()
            && firing.run.is_none()
            && matches!(
                firing.skip,
                Some(SkipReason::Overlap | SkipReason::Capacity | SkipReason::MissedDeadline)
            ))
    {
        return Ok(true);
    }
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM automation_precheck_history_cleanup WHERE firing=?1 AND revision=?2)",
        params![firing.id.to_string(), firing.revision], |row| row.get(0),
    ).map_err(|error| error.to_string())
}

fn measured_skip(db: &Connection, firing: &Firing) -> Result<bool, String> {
    Ok(firing.state == FiringState::Skipped
        && !firing.release_pending
        && firing.agent.is_none()
        && firing.run.is_none()
        && firing.skip.is_some()
        && firing.skip != Some(SkipReason::MissedDeadline)
        && matches!(firing.trigger, Trigger::Scheduled { .. })
        && cleanup_clear(db, firing)?)
}

fn summary(firing: &Firing) -> Result<SkippedOccurrences, String> {
    let Trigger::Scheduled { occurrence } = &firing.trigger else {
        return Err("Only exact scheduled occurrences can be grouped".into());
    };
    let value = firing
        .skipped_occurrences
        .clone()
        .unwrap_or(SkippedOccurrences {
            count: 1,
            first_occurrence_ms: occurrence.due_ms,
            last_occurrence_ms: occurrence.due_ms,
        });
    if value.count == 0
        || value.first_occurrence_ms > value.last_occurrence_ms
        || value.last_occurrence_ms != occurrence.due_ms
    {
        return Err("Stored skipped-occurrence range is invalid".into());
    }
    Ok(value)
}

fn merge(tx: &Transaction<'_>, previous: &mut Firing, next: &Firing) -> Result<bool, String> {
    if previous.automation != next.automation
        || previous.revision != next.revision
        || previous.skip != next.skip
        || !measured_skip(tx, previous)?
        || !measured_skip(tx, next)?
    {
        return Ok(false);
    }
    let predecessor: Option<String> = tx
        .query_row(
            "SELECT previous_firing FROM automation_firings WHERE id=?1",
            [next.id.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if predecessor.as_deref() != Some(previous.id.to_string().as_str()) {
        return Ok(false);
    }
    let old = summary(previous)?;
    let added = summary(next)?;
    if old.last_occurrence_ms >= added.first_occurrence_ms {
        return Ok(false);
    }
    let key: String = tx
        .query_row(
            "SELECT occurrence_key FROM automation_firings WHERE id=?1",
            [next.id.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    // A cleanup-confirmed next row is removed atomically before its unique key
    // is transferred. Stable first row identity/name/created time survive.
    tx.execute("UPDATE automation_firings SET previous_firing=?2 WHERE automation=?3 AND previous_firing=?1",
        params![next.id.to_string(), previous.id.to_string(), previous.automation.to_string()]).map_err(|error|error.to_string())?;
    tx.execute(
        "DELETE FROM automation_firings WHERE id=?1",
        [next.id.to_string()],
    )
    .map_err(|error| error.to_string())?;
    previous.skipped_occurrences = Some(SkippedOccurrences {
        count: old
            .count
            .checked_add(added.count)
            .ok_or("Skipped occurrence count overflow")?,
        first_occurrence_ms: old.first_occurrence_ms,
        last_occurrence_ms: added.last_occurrence_ms,
    });
    previous.trigger = next.trigger.clone();
    previous.finished_ms = next.finished_ms;
    previous.diagnostic = next.diagnostic.clone();
    tx.execute(
        "UPDATE automation_firings SET occurrence_key=?2 WHERE id=?1",
        params![previous.id.to_string(), key],
    )
    .map_err(|error| error.to_string())?;
    write_firing(tx, previous)?;
    Ok(true)
}

/// Exact claim-time refusal before a precheck could ever own this firing.
pub(super) fn record_never_launched(tx: &Transaction<'_>, firing: &Firing) -> Result<(), String> {
    tx.execute("INSERT INTO automation_precheck_history_cleanup(firing,revision,evidence) VALUES(?1,?2,'not_launched')",
        params![firing.id.to_string(),firing.revision]).map_err(|error|error.to_string())?;
    Ok(())
}

pub(super) fn insert_or_coalesce(
    tx: &Transaction<'_>,
    firing: &mut Firing,
    key: &str,
) -> Result<(), String> {
    let latest: Option<String> = tx
        .query_row(
            "SELECT record FROM automation_firings WHERE automation=?1 ORDER BY rowid DESC LIMIT 1",
            [firing.automation.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if measured_skip(tx, firing)? {
        firing.skipped_occurrences = Some(summary(firing)?);
    }
    insert_firing(tx, firing, key)?;
    if let Some(raw) = latest {
        let mut previous: Firing = decode(&raw)?;
        if merge(tx, &mut previous, firing)? {
            *firing = previous;
        }
    }
    Ok(())
}

/// Coalesce only adjacent insertion-order rows after actual cleanup, including
/// a newer consecutive group which could not merge while this owner was live.
fn compact(tx: &Transaction<'_>, firing: &mut Firing) -> Result<(), String> {
    let previous:Option<String>=tx.query_row(
        "SELECT record FROM automation_firings WHERE automation=?1 AND id=(SELECT previous_firing FROM automation_firings WHERE id=?2)",
        params![firing.automation.to_string(),firing.id.to_string()],|row|row.get(0))
        .optional().map_err(|error|error.to_string())?;
    if let Some(raw) = previous {
        let mut old: Firing = decode(&raw)?;
        if merge(tx, &mut old, firing)? {
            *firing = old;
        }
    }
    // Normal writes have <=500 eligible rows. Legacy cleanup can make more
    // adjacent rows eligible; bound this one transaction's compaction work.
    for _ in 0..HISTORY_RETAINED_ROWS {
        let next:Option<String>=tx.query_row(
            "SELECT record FROM automation_firings WHERE automation=?1 AND previous_firing=?2 ORDER BY rowid LIMIT 1",
            params![firing.automation.to_string(),firing.id.to_string()],|row|row.get(0))
            .optional().map_err(|error|error.to_string())?;
        let Some(raw) = next else { break };
        if !merge(tx, firing, &decode::<Firing>(&raw)?)? {
            break;
        }
    }
    Ok(())
}

// Keep scoped writes indexable: an optional-filter OR scans unrelated
// automations even when its bound identity is present. The global startup
// pass deliberately retains its independent, unscoped query.
pub(super) fn prune_candidates_sql(scoped: bool) -> String {
    let (scope, firing_scope) = if scoped {
        ("automation=?1", "f.automation=?1")
    } else {
        ("?1 IS NULL", "?1 IS NULL")
    };
    format!(
        "WITH latest AS (
          SELECT automation,max(rowid) AS tail FROM automation_firings
          WHERE {scope} GROUP BY automation
        ) INSERT INTO automation_history_prune SELECT id FROM (
          SELECT f.id,a.deleted_ms,row_number() OVER(PARTITION BY f.automation ORDER BY
            (f.rowid=latest.tail) DESC,
            coalesce(json_extract(f.record,'$.finished_ms'),f.created_ms) DESC,f.rowid DESC) AS rank
          FROM automation_firings f JOIN automations a ON a.id=f.automation
          JOIN latest ON latest.automation=f.automation
          JOIN automation_revisions r ON r.automation=f.automation AND r.revision=f.revision
          WHERE {firing_scope}
            AND {ELIGIBLE}
        ) WHERE rank>?3 OR deleted_ms<?2"
    )
}

pub(super) fn prune_revisions_sql(scoped: bool) -> String {
    let scope = if scoped {
        "automation=?1"
    } else {
        "?1 IS NULL"
    };
    format!(
        "DELETE FROM automation_revisions WHERE {scope}
        AND NOT EXISTS(SELECT 1 FROM automation_firings f WHERE f.automation=automation_revisions.automation AND f.revision=automation_revisions.revision)
        AND NOT EXISTS(SELECT 1 FROM automations a WHERE a.id=automation_revisions.automation
          AND (a.deleted_ms IS NULL OR a.deleted_ms>=?2) AND json_extract(a.record,'$.execution_revision')=automation_revisions.revision)"
    )
}

pub(super) fn prune(
    tx: &Transaction<'_>,
    automation: Option<Uuid>,
    now_ms: u64,
    limit: usize,
) -> Result<(), String> {
    super::super::schema::require_current(tx)?;
    let limit = limit.max(1);
    let cutoff = now_ms.saturating_sub(REMOVED_TTL_MS);
    // Protected rows do not consume any settled-history slots. One latest
    // insertion barrier is retained inside the limit for live automations so
    // deleting a manual/success boundary cannot later join nonconsecutive skips.
    tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS automation_history_prune(id TEXT PRIMARY KEY); DELETE FROM automation_history_prune;")
        .map_err(|error|error.to_string())?;
    tx.execute(
        &prune_candidates_sql(automation.is_some()),
        params![automation.map(|id| id.to_string()), cutoff, limit],
    )
    .map_err(|error| error.to_string())?;
    tx.execute("INSERT INTO automation_retired_manual_requests(automation,request,revision)
        SELECT automation,occurrence_key,revision FROM automation_firings WHERE occurrence_key LIKE 'manual:%'
          AND id IN (SELECT id FROM automation_history_prune)
        ON CONFLICT(automation,request) DO NOTHING",[]).map_err(|error|error.to_string())?;
    tx.execute(
        "DELETE FROM automation_firings WHERE id IN (SELECT id FROM automation_history_prune)",
        [],
    )
    .map_err(|error| error.to_string())?;
    tx.execute(
        &prune_revisions_sql(automation.is_some()),
        params![automation.map(|id| id.to_string()), cutoff],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

impl Registry {
    /// Missing means the exact firing is genuinely absent, not active or
    /// protected. A verified filesystem receipt may then be reclaimed, but
    /// must not manufacture a cleanup ACK for another owner or revision.
    pub(crate) fn automation_precheck_retirement(
        &self,
        id: Uuid,
        revision: u64,
    ) -> Result<PrecheckRetirement, String> {
        let raw: Option<String> = self
            .db
            .query_row(
                "SELECT record FROM automation_firings WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some(raw) = raw else {
            return Ok(PrecheckRetirement::Missing);
        };
        let firing: Firing = decode(&raw)?;
        let owner: Automation = decode(
            &self
                .db
                .query_row(
                    "SELECT record FROM automations WHERE id=?1",
                    [firing.automation.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|error| error.to_string())?,
        )?;
        if firing.revision != revision
            || owner.active_firing == Some(id)
            || owner.blocked.is_some()
            || firing.state.is_active()
            || firing.release_pending
        {
            return Ok(PrecheckRetirement::Protected);
        }
        let acknowledged: Option<u64> = self
            .db
            .query_row(
                "SELECT revision FROM automation_precheck_history_cleanup WHERE firing=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        Ok(match acknowledged {
            Some(exact) if exact == revision => PrecheckRetirement::Acknowledged,
            Some(_) => PrecheckRetirement::Protected,
            None => PrecheckRetirement::ReleasedPending,
        })
    }

    pub(crate) fn maintain_automation_history(&mut self, now_ms: u64) -> Result<(), String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        prune(&tx, None, now_ms, HISTORY_RETAINED_ROWS)?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// Called by the existing coordinator at most once a minute. The partial
    /// deleted_ms index skips live automations; protected-only tombstones do
    /// not monopolize the batch and starve later eligible removed history.
    pub(crate) fn maintain_removed_automation_history(
        &mut self,
        now_ms: u64,
    ) -> Result<(), String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        super::super::schema::require_current(&tx)?;
        let cutoff = now_ms.saturating_sub(REMOVED_TTL_MS);
        let ids=tx.prepare(&format!("SELECT a.id FROM automations a WHERE a.deleted_ms<?1 AND (
            EXISTS(SELECT 1 FROM automation_firings f JOIN automation_revisions r
              ON r.automation=f.automation AND r.revision=f.revision
              WHERE f.automation=a.id AND {ELIGIBLE})
            OR EXISTS(SELECT 1 FROM automation_revisions r WHERE r.automation=a.id
              AND NOT EXISTS(SELECT 1 FROM automation_firings f WHERE f.automation=r.automation AND f.revision=r.revision)))
            ORDER BY a.deleted_ms,a.id LIMIT 8"))
            .map_err(|e|e.to_string())?.query_map([cutoff],|row| row.get::<_,String>(0))
            .map_err(|e|e.to_string())?.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
        for id in ids {
            prune(
                &tx,
                Some(
                    id.parse()
                        .map_err(|_| "Stored automation identity is invalid")?,
                ),
                now_ms,
                HISTORY_RETAINED_ROWS,
            )?;
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// Only the broker's before-Worker::launch branch may record this evidence.
    /// It proves no launch was attempted, not merely that a pathname is absent.
    pub(crate) fn confirm_unlaunched_automation_precheck(
        &mut self,
        id: Uuid,
        revision: u64,
        now_ms: u64,
    ) -> Result<(), String> {
        self.confirm_precheck_history_cleanup(id, revision, now_ms, "not_launched")
    }

    /// Call only after the exact existing Worker::retire returned true.
    pub(crate) fn confirm_retired_automation_precheck(
        &mut self,
        id: Uuid,
        revision: u64,
        now_ms: u64,
    ) -> Result<(), String> {
        self.confirm_precheck_history_cleanup(id, revision, now_ms, "retired")
    }

    fn confirm_precheck_history_cleanup(
        &mut self,
        id: Uuid,
        revision: u64,
        now_ms: u64,
        evidence: &str,
    ) -> Result<(), String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        super::super::schema::require_current(&tx)?;
        let raw: Option<String> = tx
            .query_row(
                "SELECT record FROM automation_firings WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        // A repeated exact retire after a committed ACK may find already-pruned
        // history. It deletes/creates nothing and confers no new authority.
        let Some(raw) = raw else { return Ok(()) };
        let mut firing: Firing = decode(&raw)?;
        // Management deliberately excludes removed automations. Cleanup must
        // still inspect their retained owner record after a legal tombstone.
        let owner: Automation = decode(
            &tx.query_row(
                "SELECT record FROM automations WHERE id=?1",
                [firing.automation.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| error.to_string())?,
        )?;
        if firing.revision != revision
            || owner.active_firing == Some(id)
            || owner.blocked.is_some()
            || firing.state.is_active()
            || firing.release_pending
            || (evidence == "not_launched" && firing.run.is_some())
        {
            return Err("Precheck cleanup no longer matches released ownership".into());
        }
        tx.execute("INSERT INTO automation_precheck_history_cleanup(firing,revision,evidence) VALUES(?1,?2,?3)
          ON CONFLICT(firing) DO UPDATE SET evidence=excluded.evidence WHERE revision=excluded.revision",
          params![id.to_string(),revision,evidence]).map_err(|e|e.to_string())?;
        compact(&tx, &mut firing)?;
        prune(&tx, Some(firing.automation), now_ms, HISTORY_RETAINED_ROWS)?;
        tx.commit().map_err(|e| e.to_string())
    }
}

#[cfg(test)]
impl Registry {
    /// Keep fault injection connection-local so the strict main-schema contract
    /// remains intact while the real broker exercises its durable ACK path.
    pub(in crate::host) fn set_automation_history_ack_failure_for_test(
        &self,
        refuse: bool,
    ) -> Result<(), String> {
        self.db.execute_batch(if refuse {
            "CREATE TEMP TRIGGER refuse_history_ack BEFORE INSERT ON main.automation_precheck_history_cleanup BEGIN SELECT RAISE(ABORT,'fixture durable ACK failure'); END;"
        } else {
            "DROP TRIGGER temp.refuse_history_ack"
        }).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
impl Registry {
    /// Fault the actual DELETE on this execution connection without changing
    /// the published main-schema layout or bypassing strict admission.
    pub(in crate::host) fn set_automation_history_prune_failure_for_test(
        &self,
        firing: Option<Uuid>,
    ) -> Result<(), String> {
        let sql = if let Some(firing) = firing {
            // Uuid Display is canonical; this fault cannot hit another owner's
            // required lifecycle transaction or alter the main-schema layout.
            format!(
                "CREATE TEMP TRIGGER refuse_history_prune BEFORE DELETE ON main.automation_firings WHEN OLD.id='{firing}' BEGIN SELECT RAISE(ABORT,'fixture history DELETE failure'); END;"
            )
        } else {
            "DROP TRIGGER temp.refuse_history_prune".to_owned()
        };
        self.db
            .execute_batch(&sql)
            .map_err(|error| error.to_string())
    }
}
