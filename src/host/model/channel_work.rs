//! Canonical channel work lives with channel/view transactions, never in the
//! replaceable Git/forge cache. Every helper here is network-free.

use super::*;
use crate::agent_execution::{Authority, HostedSession};
use crate::channel_work::{
    self as work, Bucket, Entry, Home, HomeLifecycle, HomeState, Mutation, Origin, Page, Receipt,
    Reply, Snapshot, StoredOperation,
};
use crate::model::{CardId, CardKind};
use rusqlite::{Transaction, TransactionBehavior};

pub(super) mod placement;

/// Constructed only by the strictly authenticated host service. Neither the
/// capability nor this proof is serializable or retained in the work ledger.
pub(crate) struct SourceProof {
    pub(crate) authority: Authority,
    pub(crate) source: HostedSession,
    pub(crate) card: CardId,
    #[cfg(test)]
    pub(crate) view_revision: u64,
    // Durable causal identity of this pane's placement/source. Presentation
    // changes still advance the canonical view revision without replacing it.
    pub(crate) placement_epoch: u64,
}

pub(crate) struct TrustedResolvedOrigin {
    pub(crate) origin: Origin,
    pub(crate) proof: Option<SourceProof>,
}

pub(crate) struct Observed {
    pub(crate) snapshot: Snapshot,
    pub(crate) sequence: u64,
    pub(crate) changed: bool,
}

/// Only an attempted commit can make this mutation's completion uncertain.
/// The service must preserve that distinction rather than promise no effect.
#[derive(Debug)]
pub(crate) enum ApplyError {
    Refused(String),
    CommitUncertain,
}

#[cfg(test)]
mod tests;

impl From<String> for ApplyError {
    fn from(reason: String) -> Self {
        Self::Refused(reason)
    }
}

impl From<&str> for ApplyError {
    fn from(reason: &str) -> Self {
        Self::Refused(reason.into())
    }
}

/// Root's published-layout migration owns the version and invokes `migrate`.
/// None of these retained rows cascade from active projects or channels.
pub(super) const TABLES_SQL: &str = "
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

/// Metadata only: migration never infers a PR or actor from HEAD, cwd or scans.
pub(super) fn migrate(tx: &Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(TABLES_SQL)
        .map_err(|error| error.to_string())?;
    tx.execute(
        "INSERT INTO channel_work_homes(channel,incarnation,repository,title,lifecycle,revision)
         SELECT id,ordinal,repository,title,'active','0' FROM channels",
        [],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn registry_identity(db: &Connection) -> Result<Uuid, String> {
    let identity: String = db
        .query_row("SELECT identity FROM metadata WHERE key=0", [], |row| {
            row.get(0)
        })
        .map_err(|error| error.to_string())?;
    identity
        .parse()
        .map_err(|_| "The registry identity is unreadable".into())
}

fn lifecycle(value: &str) -> Result<HomeLifecycle, String> {
    match value {
        "active" => Ok(HomeLifecycle::Active),
        "archived" => Ok(HomeLifecycle::Archived),
        "deleted" => Ok(HomeLifecycle::Deleted),
        _ => Err("The channel work lifecycle is unreadable".into()),
    }
}

fn home_state(db: &Connection, home: &Home) -> Result<(HomeLifecycle, u64), String> {
    if registry_identity(db)? != home.registry {
        return Err("The channel belongs to another registry".into());
    }
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT lifecycle,revision FROM channel_work_homes
             WHERE channel=?1 AND incarnation=?2",
            params![home.channel.to_string(), home.creation_ordinal],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let (state, revision) = row.ok_or("The channel work home no longer exists")?;
    let revision = revision
        .parse()
        .map_err(|_| "The channel work revision is unreadable")?;
    Ok((lifecycle(&state)?, revision))
}

fn require_current_home(db: &Connection, home: &Home) -> Result<u64, String> {
    let (state, revision) = home_state(db, home)?;
    if state != HomeLifecycle::Active {
        return Err("The channel work home is archived or deleted".into());
    }
    let current: Option<u32> = db
        .query_row(
            "SELECT ordinal FROM channels WHERE id=?1",
            [home.channel.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if current != Some(home.creation_ordinal) {
        return Err("The channel incarnation changed".into());
    }
    Ok(revision)
}

/// Retire current participation while preserving attribution and corrections.
/// Each changed row receives a unique per-home cursor, even when one canonical
/// lifecycle transaction retires many entries. Root bumps metadata.sequence
/// once for the enclosing model edit, not once per affected relation.
fn retire_entries_in(tx: &Transaction<'_>, home: &Home, mut revision: u64) -> Result<u64, String> {
    // Sequence changes as rows retire, so walk the immutable canonical target
    // key instead. Keep at most one small batch of decoded JSON in memory.
    let mut after: Option<String> = None;
    loop {
        let rows = {
            let mut statement = tx
                .prepare(
                    "SELECT target,entry FROM channel_work_entries
                 WHERE channel=?1 AND incarnation=?2
                   AND (?3 IS NULL OR target COLLATE BINARY > ?3)
                 ORDER BY target COLLATE BINARY LIMIT 8",
                )
                .map_err(|error| error.to_string())?;
            statement
                .query_map(
                    params![home.channel.to_string(), home.creation_ordinal, after],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .map_err(|error| error.to_string())?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|error| error.to_string())?
        };
        if rows.is_empty() {
            break;
        }
        for (target, raw) in rows {
            // Advance even when this row was already ended; otherwise an
            // all-ended batch would repeat forever or hide later live work.
            after = Some(target.clone());
            let mut entry: Entry = decode(&raw)?;
            entry.validate().map_err(|error| error.to_string())?;
            if encode(&entry.target)? != target {
                return Err("The channel work target key is inconsistent".into());
            }
            if entry.user_ended && entry.claims.iter().all(|claim| !claim.active) {
                continue;
            }
            entry.user_ended = true;
            for claim in &mut entry.claims {
                claim.active = false;
            }
            entry.revision = entry
                .revision
                .checked_add(1)
                .ok_or("The work entry revision is exhausted")?;
            revision = revision
                .checked_add(1)
                .ok_or("The channel work revision is exhausted")?;
            entry.validate().map_err(|error| error.to_string())?;
            tx.execute(
                "UPDATE channel_work_entries SET entry=?4,sequence=?5,bucket='history' WHERE channel=?1 AND incarnation=?2 AND target=?3",
                params![home.channel.to_string(),home.creation_ordinal,target,encode(&entry)?,revision],
            ).map_err(|error| error.to_string())?;
        }
    }
    revision
        .checked_add(1)
        .ok_or_else(|| "The channel work revision is exhausted".into())
}

/// Called in the SAME transaction after inserting a new canonical channel.
/// A reused UUID receives a new home; it never overwrites an old tombstone.
pub(super) fn created_in(tx: &Transaction<'_>, channel: ChannelId) -> Result<(), String> {
    tx.execute(
        "INSERT INTO channel_work_homes(channel,incarnation,repository,title,lifecycle,revision)
         SELECT id,ordinal,repository,title,'active','0' FROM channels WHERE id=?1
         ON CONFLICT(channel,incarnation) DO NOTHING",
        [channel.to_string()],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// Root invokes this after canonical manual/inferred title edits in the same
/// model transaction. An old incarnation's historical title never changes.
pub(super) fn renamed_in(tx: &Transaction<'_>, channel: ChannelId) -> Result<(), String> {
    tx.execute(
        "UPDATE channel_work_homes SET title=(SELECT title FROM channels WHERE id=?1)
         WHERE channel=?1 AND incarnation=(SELECT ordinal FROM channels WHERE id=?1)",
        [channel.to_string()],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// Preserve the last canonical title/project before active-channel FK cascades.
/// A parent project close must invoke this for each child before its DELETE.
pub(super) fn deleted_in(tx: &Transaction<'_>, channel: ChannelId) -> Result<(), String> {
    let ordinal: Option<u32> = tx
        .query_row(
            "SELECT ordinal FROM channels WHERE id=?1",
            [channel.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some(creation_ordinal) = ordinal else {
        return Ok(());
    };
    let home = Home {
        registry: registry_identity(tx)?,
        channel,
        creation_ordinal,
    };
    let (old, revision) = home_state(tx, &home)?;
    if old == HomeLifecycle::Deleted {
        return Ok(());
    }
    let revision = retire_entries_in(tx, &home, revision)?;
    tx.execute(
        "UPDATE channel_work_homes SET
             repository=(SELECT repository FROM channels WHERE id=?1),
             title=(SELECT title FROM channels WHERE id=?1), lifecycle='deleted',revision=?2
         WHERE channel=?1 AND incarnation=(SELECT ordinal FROM channels WHERE id=?1)",
        params![channel.to_string(), revision.to_string()],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// An archive keeps the canonical home/history but forbids stale work writes.
/// Root owns the corresponding explicit navigation lifecycle operation.
pub(super) fn archived_in(tx: &Transaction<'_>, home: &Home) -> Result<bool, String> {
    let (state, revision) = home_state(tx, home)?;
    if state == HomeLifecycle::Archived {
        return Ok(false);
    }
    require_current_home(tx, home)?;
    let revision = retire_entries_in(tx, home, revision)?;
    tx.execute(
        "UPDATE channel_work_homes SET
             repository=(SELECT repository FROM channels WHERE id=?1),
             title=(SELECT title FROM channels WHERE id=?1), lifecycle='archived',revision=?3
         WHERE channel=?1 AND incarnation=?2",
        params![
            home.channel.to_string(),
            home.creation_ordinal,
            revision.to_string()
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(true)
}

/// The caller already owns its model Immediate transaction. A work-bearing
/// home survives its final conversation. A concurrent pane/work writer wins in
/// one order or the other; a cached UI `has_work` flag grants no authority.
pub(super) fn may_auto_retire_in(
    tx: &Transaction<'_>,
    home: &Home,
    expected_view_revision: u64,
) -> Result<bool, String> {
    require_current_home(tx, home)?;
    let view = views::read(
        tx,
        &crate::workspace_model::views::Scope::Channel(home.channel),
    )?;
    if view.revision != expected_view_revision || !view.panes.is_empty() {
        return Ok(false);
    }
    let has_history: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM channel_work_entries WHERE channel=?1 AND incarnation=?2)
             OR EXISTS(SELECT 1 FROM channel_work_operations WHERE channel=?1 AND incarnation=?2)",
            params![home.channel.to_string(), home.creation_ordinal],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    Ok(!has_history)
}

fn encode(value: &impl serde::Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}

fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, String> {
    serde_json::from_str(value).map_err(|_| "The channel work record is unreadable".into())
}

/// The host first checks this source against its live/adopted actor. This
/// database resolver then proves one canonical scope in the same read snapshot.
fn resolve_in(
    db: &Connection,
    authority: &Authority,
    source: &HostedSession,
) -> Result<TrustedResolvedOrigin, String> {
    if !source.has_complete_proof() {
        return Err("The actor source has no complete process proof".into());
    }
    let token = authority
        .token()
        .ok_or("The actor authority is unavailable")?;
    let registered: Option<(String, String)> = db
        .query_row(
            "SELECT a.source,coalesce(l.lineage,a.session_id) FROM agent_authorities a
         LEFT JOIN session_lineages l ON l.session_id=a.session_id
         WHERE a.token=?1 AND a.session_id=?2",
            params![token, source.id().to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let (registered, lineage) = registered.ok_or("The actor authority was retired")?;
    let registered: HostedSession = decode(&registered)?;
    if !registered.same_process(source) {
        return Err("The actor authority belongs to a different process".into());
    }
    let lineage = crate::agent_execution::Lineage(
        lineage
            .parse()
            .map_err(|_| "The actor lineage is unreadable")?,
    );
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM agents WHERE session_id=?1 AND closed=0",
            [source.id().to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let agent: Option<crate::agent_execution::Agent> = raw.as_deref().map(decode).transpose()?;
    let mut display_sources = vec![source.clone()];
    if let Some(agent) = &agent {
        let original = agent
            .session
            .as_ref()
            .is_some_and(|session| session.same_process(source));
        let console = agent
            .console
            .as_ref()
            .map(|session| HostedSession::Terminal(session.clone()));
        if !original
            && !console
                .as_ref()
                .is_some_and(|console| console.same_process(source))
        {
            return Err("The managed actor source changed".into());
        }
        if original {
            if let Some(console) = console.filter(HostedSession::has_complete_proof) {
                display_sources.push(console);
            }
        }
    }
    let mut matches = Vec::new();
    for view in views::all(db)? {
        for pane in view.panes {
            let candidate = match pane.kind {
                CardKind::Chat(session) => HostedSession::Chat(session),
                CardKind::Terminal(session) => HostedSession::Terminal(session),
                _ => continue,
            };
            if display_sources
                .iter()
                .any(|source| source.id() == candidate.id())
            {
                matches.push((view.scope.clone(), view.revision, pane.id, candidate));
            }
        }
    }
    if matches.len() != 1
        || !display_sources
            .iter()
            .any(|source| source.same_process(&matches[0].3))
    {
        return Err("The actor no longer has one exact canonical channel pane".into());
    }
    let (scope, _view_revision, card, _) = matches.remove(0);
    let crate::workspace_model::views::Scope::Channel(channel) = scope else {
        return Err("The actor is not displayed in a channel".into());
    };
    let creation_ordinal: u32 = db
        .query_row(
            "SELECT ordinal FROM channels WHERE id=?1",
            [channel.to_string()],
            |row| row.get(0),
        )
        .map_err(|_| "The actor channel no longer exists")?;
    let home = Home {
        registry: registry_identity(db)?,
        channel,
        creation_ordinal,
    };
    require_current_home(db, &home)?;
    let placement_epoch = placement::token(db, card)?;
    let run = if let Some(agent) = &agent {
        let raw: Option<String> = db
            .query_row(
                "SELECT record FROM agent_runs WHERE agent=?1 ORDER BY rowid DESC LIMIT 1",
                [agent.id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        raw.as_deref()
            .map(decode::<crate::agent_execution::Run>)
            .transpose()?
            .map(|run| run.id)
    } else {
        None
    };
    Ok(TrustedResolvedOrigin {
        origin: Origin::Actor {
            home,
            actor: work::Contributor {
                session: source.id(),
                lineage,
                agent: agent.map(|agent| agent.id),
                run,
            },
            contexts: Vec::new(),
        },
        proof: Some(SourceProof {
            authority: authority.clone(),
            source: source.clone(),
            card,
            #[cfg(test)]
            view_revision: _view_revision,
            placement_epoch,
        }),
    })
}

fn require_origin(
    db: &Connection,
    resolved: &TrustedResolvedOrigin,
    home: &Home,
) -> Result<(), String> {
    match (&resolved.origin, &resolved.proof) {
        (Origin::User, None) => Ok(()),
        (
            Origin::Actor {
                home: source_home, ..
            },
            Some(proof),
        ) => {
            let current = resolve_in(db, &proof.authority, &proof.source)?;
            let Origin::Actor {
                home: current_home, ..
            } = &current.origin
            else {
                unreachable!()
            };
            let current_proof = current
                .proof
                .as_ref()
                .ok_or("The actor source is unavailable")?;
            if source_home != home
                || current_home != home
                || current.origin.source() != resolved.origin.source()
                || current_proof.card != proof.card
                || current_proof.placement_epoch != proof.placement_epoch
            {
                return Err("The actor's captured channel context changed".into());
            }
            Ok(())
        }
        _ => Err("The channel work origin is not authenticated".into()),
    }
}

fn bucket_key(entry: &Entry) -> &'static str {
    match entry.bucket() {
        Bucket::Ongoing => "ongoing",
        Bucket::NeedsConfirmation => "needs_confirmation",
        Bucket::History => "history",
    }
}

fn read_entry(target: String, raw: String, bucket: String) -> Result<Entry, String> {
    let entry: Entry = decode(&raw)?;
    entry.validate().map_err(|error| error.to_string())?;
    if encode(&entry.target)? != target || bucket_key(&entry) != bucket {
        return Err("The channel work target or presentation key is inconsistent".into());
    }
    Ok(entry)
}

/// Current and historical projections have independent strict keyset cursors
/// and budgets. A lookahead row is consumed once and is never skipped: the
/// continuation always names the last INCLUDED row, not the lookahead.
fn read_bucket(
    db: &Connection,
    home: &Home,
    before: Option<u64>,
    limit: u16,
    historical: bool,
) -> Result<(Vec<Entry>, Option<u64>), String> {
    let before = before
        .map(i64::try_from)
        .transpose()
        .map_err(|_| "The channel work cursor is invalid")?;
    let mut statement = db
        .prepare(
            "SELECT target,sequence FROM channel_work_entries
         WHERE channel=?1 AND incarnation=?2
           AND ((?5 AND bucket='history') OR
                (NOT ?5 AND bucket IN ('ongoing','needs_confirmation')))
           AND (?3 IS NULL OR sequence<?3)
         ORDER BY sequence DESC LIMIT ?4",
        )
        .map_err(|error| error.to_string())?;
    let mut rows = statement
        .query(params![
            home.channel.to_string(),
            home.creation_ordinal,
            before,
            u32::from(limit) + 1,
            historical,
        ])
        .map_err(|error| error.to_string())?;
    // Sort only small canonical keys. Selecting Entry JSON here would let
    // SQLite's combined-current-bucket sorter retain an unbounded payload.
    let mut point = db
        .prepare(
            "SELECT entry,bucket FROM channel_work_entries
         WHERE channel=?1 AND incarnation=?2 AND target=?3",
        )
        .map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    let mut bytes = 0_usize;
    let mut last = None;
    let mut next = None;
    while let Some(row) = rows.next().map_err(|error| error.to_string())? {
        if entries.len() == usize::from(limit) {
            next = last;
            break;
        }
        let target: String = row.get(0).map_err(|error| error.to_string())?;
        let sequence: u64 = row.get(1).map_err(|error| error.to_string())?;
        let (raw, bucket): (String, String) = point
            .query_row(
                params![home.channel.to_string(), home.creation_ordinal, target],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| error.to_string())?;
        let entry = read_entry(target, raw, bucket)?;
        let size = work::encoded_size(&entry).map_err(|error| error.to_string())?;
        let combined = bytes
            .checked_add(size)
            .ok_or("The channel work projection is too large")?;
        if combined > work::MAX_BUCKET_BYTES {
            if entries.is_empty() {
                return Err("A channel work entry exceeds the projection budget".into());
            }
            next = last;
            break;
        }
        bytes = combined;
        entries.push(entry);
        last = Some(sequence);
    }
    Ok((entries, next))
}

fn read_in(db: &Connection, home: &Home, page: Page) -> Result<Snapshot, String> {
    home.validate().map_err(|error| error.to_string())?;
    page.validate().map_err(|error| error.to_string())?;
    let (lifecycle, revision) = home_state(db, home)?;
    let (entries, next_current) = read_bucket(db, home, page.current_before, page.limit, false)?;
    let (history, next_history) = read_bucket(db, home, page.before, page.limit, true)?;
    let snapshot = Snapshot {
        home: *home,
        lifecycle,
        revision,
        entries,
        next_current,
        history,
        next_history,
    };
    snapshot.validate().map_err(|error| error.to_string())?;
    Ok(snapshot)
}

fn read_target_in(
    db: &Connection,
    home: &Home,
    target: &work::Target,
) -> Result<work::TargetSnapshot, String> {
    home.validate().map_err(|error| error.to_string())?;
    let target = target.canonicalized().map_err(|error| error.to_string())?;
    let (lifecycle, revision) = home_state(db, home)?;
    let key = encode(&target)?;
    let raw: Option<(String, String)> = db
        .query_row(
            "SELECT entry,bucket FROM channel_work_entries
         WHERE channel=?1 AND incarnation=?2 AND target=?3",
            params![home.channel.to_string(), home.creation_ordinal, key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let entry = raw
        .map(|(raw, bucket)| read_entry(key, raw, bucket))
        .transpose()?;
    let result = work::TargetSnapshot {
        home: *home,
        lifecycle,
        revision,
        target,
        entry,
    };
    result.validate().map_err(|error| error.to_string())?;
    Ok(result)
}

fn operation_in(
    db: &Connection,
    home: &Home,
    operation: Uuid,
) -> Result<Option<StoredOperation>, String> {
    home_state(db, home)?;
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM channel_work_operations
             WHERE operation=?1 AND channel=?2 AND incarnation=?3",
            params![
                operation.to_string(),
                home.channel.to_string(),
                home.creation_ordinal
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let record: Option<StoredOperation> = raw.as_deref().map(decode).transpose()?;
    if record.as_ref().is_some_and(|record| {
        record.mutation.home != *home
            || record.mutation.operation != operation
            || record.receipt.home != *home
            || record.receipt.operation != operation
    }) {
        return Err("The channel work operation record is inconsistent".into());
    }
    Ok(record)
}

impl Registry {
    /// Root must first perform strict live/adopted host authentication. This
    /// method never falls back from an unknown actor token to User authority.
    pub(crate) fn resolve_channel_work_source(
        &self,
        authority: &Authority,
        source: &HostedSession,
    ) -> Result<TrustedResolvedOrigin, String> {
        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        let result = resolve_in(&tx, authority, source)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(result)
    }

    /// Identity, incarnation, revision and all returned rows share one SQLite
    /// snapshot, including when another independent WAL writer commits.
    pub(crate) fn read_channel_work(&self, home: &Home, page: Page) -> Result<Snapshot, String> {
        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        let result = read_in(&tx, home, page)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(result)
    }

    /// Metadata-only recovery read. Retained archived/deleted homes still own
    /// original receipts; projection availability is unrelated to lookup.
    pub(crate) fn read_channel_work_home(&self, home: &Home) -> Result<HomeState, String> {
        home.validate().map_err(|error| error.to_string())?;
        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        let (lifecycle, revision) = home_state(&tx, home)?;
        let result = HomeState {
            home: *home,
            lifecycle,
            revision,
        };
        tx.commit().map_err(|error| error.to_string())?;
        Ok(result)
    }

    /// Select across every tracked PR, independently of visible projection
    /// pages. JSON ranking scans this home's rows (O(N)); it returns at most
    /// four small keys, then decodes at most four bounded Entries.
    /// This read does not create ownership or advance observation attempts.
    pub(crate) fn read_channel_work_refresh_candidates(
        &self,
        home: &Home,
    ) -> Result<Vec<Entry>, String> {
        home.validate().map_err(|error| error.to_string())?;
        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        let revision = require_current_home(&tx, home)?;
        // -> returns JSON integer text, preserving full u64 precision.
        // json_extract/->> can round adjacent values above i64::MAX to REAL.
        // Decimal length and binary order compare canonical unsigned fields
        // exactly; the selected Entries undergo typed validation below.
        let keys = {
            let mut statement = tx
                .prepare(
                    "SELECT target FROM (
                   SELECT target,
                          COALESCE(entry -> '$.observation.generation','0') AS generation,
                          COALESCE(entry -> '$.observation.observed_at_ms','0') AS observed
                   FROM channel_work_entries
                   WHERE channel=?1 AND incarnation=?2
                     AND json_type(target,'$.PullRequest')='object'
                 ) ORDER BY length(generation),generation COLLATE BINARY,
                            length(observed),observed COLLATE BINARY,
                            target COLLATE BINARY LIMIT 4",
                )
                .map_err(|error| error.to_string())?;
            statement
                .query_map(
                    params![home.channel.to_string(), home.creation_ordinal],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|error| error.to_string())?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|error| error.to_string())?
        };
        let entries = {
            let mut point = tx
                .prepare(
                    "SELECT entry,bucket FROM channel_work_entries
                 WHERE channel=?1 AND incarnation=?2 AND target=?3",
                )
                .map_err(|error| error.to_string())?;
            let mut entries = Vec::with_capacity(keys.len());
            for key in keys {
                let (raw, bucket): (String, String) = point
                    .query_row(
                        params![home.channel.to_string(), home.creation_ordinal, key],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(|error| error.to_string())?;
                let entry = read_entry(key, raw, bucket)?;
                if !matches!(entry.target, work::Target::PullRequest(_))
                    || entry.revision > revision
                    || entry
                        .contexts
                        .iter()
                        .any(|context| context.registry != home.registry)
                {
                    return Err("The refresh candidate has inconsistent channel context".into());
                }
                entries.push(entry);
            }
            entries
        };
        tx.commit().map_err(|error| error.to_string())?;
        Ok(entries)
    }

    /// Exact canonical target lookup for mutation CAS. Absence is explicit;
    /// it never follows from a bounded projection page omitting the target.
    pub(crate) fn read_channel_work_target(
        &self,
        home: &Home,
        target: &work::Target,
    ) -> Result<work::TargetSnapshot, String> {
        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        let result = read_target_in(&tx, home, target)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(result)
    }

    /// A missing receipt does not prove that another connection has not yet
    /// submitted the operation. It never authorizes an automatic new-key retry.
    pub(crate) fn channel_work_receipt(
        &self,
        resolved: &TrustedResolvedOrigin,
        home: &Home,
        operation: Uuid,
    ) -> Result<Option<Receipt>, String> {
        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        // Read-only reconciliation follows the proved lineage across a real
        // run/channel move. It never grants old-home mutation authority.
        let current_home = match &resolved.origin {
            Origin::User => home,
            Origin::Actor { home, .. } => home,
        };
        require_origin(&tx, resolved, current_home)?;
        let record = operation_in(&tx, home, operation)?;
        if let Some(record) = &record {
            let allowed = match (&resolved.origin, &record.source) {
                (Origin::User, _) => true,
                (Origin::Actor { actor, .. }, work::Source::Session(original)) => {
                    actor.lineage == original.lineage
                }
                _ => false,
            };
            if !allowed {
                return Err("The work receipt belongs to another actor".into());
            }
        }
        let result = record.map(|record| record.receipt);
        tx.commit().map_err(|error| error.to_string())?;
        Ok(result)
    }

    pub(crate) fn apply_channel_work(
        &mut self,
        resolved: &TrustedResolvedOrigin,
        request: &Mutation,
    ) -> Result<Reply, ApplyError> {
        let request = request
            .clone()
            .canonicalized()
            .map_err(|error| error.to_string())?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        schema::require_current(&tx)?;
        // Reauthentication precedes replay as well as mutation. A retired
        // process does not recover authority from an old operation receipt.
        require_origin(&tx, resolved, &request.home)?;
        let page = Page::default();
        if let Some(record) = operation_in(&tx, &request.home, request.operation)? {
            let current = read_in(&tx, &request.home, page)?;
            let reply = work::replay_response(&record, &request, &resolved.origin, current)
                .map_err(|error| error.to_string())?;
            tx.commit().map_err(|_| ApplyError::CommitUncertain)?;
            return Ok(reply);
        }
        // UUID reuse from a different home must not become another operation.
        let used: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM channel_work_operations WHERE operation=?1)",
                [request.operation.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if used {
            return Err("The work operation already belongs to another context".into());
        }
        let revision = require_current_home(&tx, &request.home)?;
        let target = encode(request.change.target())?;
        let old: Option<String> = tx.query_row(
            "SELECT entry FROM channel_work_entries WHERE channel=?1 AND incarnation=?2 AND target=?3",
            params![request.home.channel.to_string(), request.home.creation_ordinal, target],
            |row| row.get(0),
        ).optional().map_err(|error| error.to_string())?;
        let old: Option<Entry> = old.as_deref().map(decode).transpose()?;
        if old.as_ref().map_or(0, |entry| entry.revision) != request.expected_revision {
            return Err("This work entry changed; refresh before changing it".into());
        }
        let transition = work::reduce_entry(
            &HomeState {
                home: request.home.clone(),
                lifecycle: HomeLifecycle::Active,
                revision,
            },
            old.as_ref(),
            &request,
            &resolved.origin,
        )
        .map_err(|error| error.to_string())?;
        let next_revision = if transition.changed {
            revision
                .checked_add(1)
                .ok_or("The channel work revision is exhausted")?
        } else {
            revision
        };
        if transition.changed {
            tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
                .map_err(|error| error.to_string())?;
        }
        let sequence: u64 = tx
            .query_row("SELECT sequence FROM metadata WHERE key=0", [], |row| {
                row.get(0)
            })
            .map_err(|error| error.to_string())?;
        if transition.changed {
            tx.execute(
                "INSERT INTO channel_work_entries(channel,incarnation,target,entry,sequence,bucket)
                 VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(channel,incarnation,target)
                 DO UPDATE SET entry=excluded.entry,sequence=excluded.sequence,bucket=excluded.bucket",
                params![
                    request.home.channel.to_string(),
                    request.home.creation_ordinal,
                    target,
                    encode(&transition.entry)?,
                    next_revision,
                    bucket_key(&transition.entry)
                ],
            )
            .map_err(|error| error.to_string())?;
            tx.execute(
                "UPDATE channel_work_homes SET revision=?3 WHERE channel=?1 AND incarnation=?2",
                params![
                    request.home.channel.to_string(),
                    request.home.creation_ordinal,
                    next_revision.to_string()
                ],
            )
            .map_err(|error| error.to_string())?;
        }
        let receipt = Receipt {
            operation: request.operation,
            home: request.home.clone(),
            revision: transition.entry.revision,
            sequence,
            changed: transition.changed,
        };
        let record = StoredOperation::capture(&request, &resolved.origin, receipt.clone())
            .map_err(|error| error.to_string())?;
        tx.execute(
            "INSERT INTO channel_work_operations(operation,channel,incarnation,record) VALUES(?1,?2,?3,?4)",
            params![request.operation.to_string(), request.home.channel.to_string(),
                    request.home.creation_ordinal, encode(&record)?],
        ).map_err(|error| error.to_string())?;
        let snapshot = read_in(&tx, &request.home, page)?;
        tx.commit().map_err(|_| ApplyError::CommitUncertain)?;
        Ok(Reply { receipt, snapshot })
    }

    /// A bounded targeted-forge worker captures the target revision before I/O.
    /// Its generation must advance for each newly scheduled poll, not each
    /// response arrival. CAS plus generation ordering discards stale results;
    /// the caller must not present a rejected result as the latest status.
    /// This internal input cannot manufacture an ownership claim or target.
    pub(crate) fn observe_channel_work_authenticated(
        &mut self,
        resolved: &TrustedResolvedOrigin,
        home: &Home,
        target: &work::Target,
        expected_entry_revision: u64,
        generation: u64,
        outcome: work::PollOutcome,
    ) -> Result<Observed, ApplyError> {
        let target = target.canonicalized().map_err(|error| error.to_string())?;
        if !matches!(target, work::Target::PullRequest(_)) {
            return Err("Forge observations require an existing PR target".into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        schema::require_current(&tx)?;
        require_origin(&tx, resolved, home)?;
        let revision = require_current_home(&tx, home)?;
        let target = encode(&target)?;
        let raw: Option<String> = tx.query_row(
            "SELECT entry FROM channel_work_entries WHERE channel=?1 AND incarnation=?2 AND target=?3",
            params![home.channel.to_string(), home.creation_ordinal, target], |row| row.get(0),
        ).optional().map_err(|error| error.to_string())?;
        let entry: Entry = decode(&raw.ok_or("This PR is not tracked by the channel")?)?;
        if entry.revision != expected_entry_revision {
            return Err("The tracked PR changed while its status was being read".into());
        }
        let update = work::observe_entry(
            &HomeState {
                home: *home,
                lifecycle: HomeLifecycle::Active,
                revision,
            },
            &entry,
            generation,
            outcome,
        )
        .map_err(|error| error.to_string())?;
        if update.changed {
            let revision = revision
                .checked_add(1)
                .ok_or("The channel work revision is exhausted")?;
            tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
                .map_err(|error| error.to_string())?;
            tx.execute(
                "UPDATE channel_work_entries SET entry=?4,sequence=?5,bucket=?6
                 WHERE channel=?1 AND incarnation=?2 AND target=?3",
                params![
                    home.channel.to_string(),
                    home.creation_ordinal,
                    target,
                    encode(&update.entry)?,
                    revision,
                    bucket_key(&update.entry)
                ],
            )
            .map_err(|error| error.to_string())?;
            tx.execute(
                "UPDATE channel_work_homes SET revision=?3 WHERE channel=?1 AND incarnation=?2",
                params![
                    home.channel.to_string(),
                    home.creation_ordinal,
                    revision.to_string()
                ],
            )
            .map_err(|error| error.to_string())?;
        }
        let snapshot = read_in(&tx, home, Page::default())?;
        let sequence = tx
            .query_row("SELECT sequence FROM metadata WHERE key=0", [], |row| {
                row.get(0)
            })
            .map_err(|error| error.to_string())?;
        tx.commit().map_err(|_| ApplyError::CommitUncertain)?;
        Ok(Observed {
            snapshot,
            sequence,
            changed: update.changed,
        })
    }

    #[cfg(test)]
    fn observe_channel_work(
        &mut self,
        home: &Home,
        target: &work::Target,
        expected_entry_revision: u64,
        generation: u64,
        outcome: work::PollOutcome,
    ) -> Result<Snapshot, ApplyError> {
        self.observe_channel_work_authenticated(
            &TrustedResolvedOrigin {
                origin: Origin::User,
                proof: None,
            },
            home,
            target,
            expected_entry_revision,
            generation,
            outcome,
        )
        .map(|observed| observed.snapshot)
    }
}
