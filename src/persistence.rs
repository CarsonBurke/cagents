use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{File, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;

use crate::{
    model::{
        BootId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind, CardState, ChannelId,
        Existence, Exposure, HostSessionProof, OperationId, ProjectChannel, ProjectId, ProjectState,
        Residency, TerminalContinuity, TerminalId, TerminalSession, Tombstone, TranscriptMetadata,
        WorkspaceId, WorkspaceState, WorktreeId, WorktreeState, normalize_project_channels,
    },
    secure_file::{
        migrate_existing_private_file, open_existing_private_file, prepare_private_directory,
        validate_and_privatize_open_file,
    },
};

pub const CURRENT_SCHEMA_VERSION: u32 = 4;
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Metadata field names for a workspace's channel rows and its selected
/// channel.
///
/// They still spell the concept `thread`, the name the project-level container
/// carried before it was renamed to a channel. A key-value row has no serde
/// alias to fall back on, so writing a new spelling would orphan the
/// navigation state of every workspace an earlier build saved, and reading
/// both spellings forever costs a permanent second lookup for no user-visible
/// gain. The durable key is therefore deliberately independent of the Rust
/// vocabulary.
const CHANNELS_FIELD: &str = "threads";
const ACTIVE_CHANNEL_FIELD: &str = "active_thread";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("SQLite state store error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("state serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("state storage I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid {kind} UUID `{value}`: {source}")]
    InvalidUuid {
        kind: &'static str,
        value: String,
        source: uuid::Error,
    },
    #[error("state store schema {found} is newer than supported schema {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("stale generation for {entity}: expected {expected}, actual {actual}")]
    StaleGeneration {
        entity: String,
        expected: u64,
        actual: u64,
    },
    #[error("invalid state: {0}")]
    InvalidState(String),
    #[error("busy timeout must be greater than zero and no more than {maximum:?}")]
    InvalidBusyTimeout { maximum: Duration },
    #[error("state store mutex was poisoned")]
    MutexPoisoned,
    #[error("state store has already been shut down")]
    Closed,
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone)]
pub struct StoreOptions {
    pub busy_timeout: Duration,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    pub dirty_shutdown: bool,
    pub boot_changed: bool,
    pub operations_requiring_reconciliation: usize,
    pub terminals_downgraded: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistedCardKind {
    Terminal,
    Chat,
    Browser,
    PullRequests,
    Repository,
}

impl PersistedCardKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Chat => "chat",
            Self::Browser => "browser",
            Self::PullRequests => "pull_requests",
            Self::Repository => "repository",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "terminal" => Ok(Self::Terminal),
            "chat" => Ok(Self::Chat),
            "browser" => Ok(Self::Browser),
            "pull_requests" => Ok(Self::PullRequests),
            "repository" => Ok(Self::Repository),
            other => Err(StoreError::InvalidState(format!(
                "unknown persisted card kind `{other}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CardSummary {
    pub id: CardId,
    pub generation: u64,
    pub ordinal: usize,
    pub title: String,
    pub width_fraction: f32,
    pub existence: Existence,
    pub kind: PersistedCardKind,
}

/// A restored workspace together with the cards that could not be restored
/// into it.
///
/// The warnings exist so a failed card is a reportable event rather than a
/// silent deletion: the shell shows them, and the writer lane keeps running.
#[derive(Debug, Clone, PartialEq)]
pub struct RestoredWorkspace {
    pub state: WorkspaceState,
    /// One human-readable line per card that was left out, newest problem last.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceManifest {
    pub id: WorkspaceId,
    pub generation: u64,
    pub schema_version: u32,
    pub existence: Existence,
    pub focused: Option<CardId>,
    pub active_worktree: Option<PathBuf>,
    pub worktree_focus: HashMap<PathBuf, CardId>,
    pub project_order: Vec<ProjectId>,
    pub channels: Vec<ProjectChannel>,
    pub active_channel: Option<ChannelId>,
    pub cards: Vec<CardSummary>,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSummary {
    pub id: WorkspaceId,
    pub generation: u64,
    pub existence: Existence,
    pub open_card_count: usize,
    pub focused: Option<CardId>,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOperationStatus {
    Pending,
    NeedsReconciliation,
    Applied,
    RolledBack,
}

impl PendingOperationStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::NeedsReconciliation => "needs_reconciliation",
            Self::Applied => "applied",
            Self::RolledBack => "rolled_back",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "needs_reconciliation" => Ok(Self::NeedsReconciliation),
            "applied" => Ok(Self::Applied),
            "rolled_back" => Ok(Self::RolledBack),
            other => Err(StoreError::InvalidState(format!(
                "unknown pending-operation status `{other}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingOperation {
    pub id: OperationId,
    pub entity_kind: String,
    pub entity_id: String,
    pub action: String,
    pub base_generation: u64,
    pub payload: Value,
    pub status: PendingOperationStatus,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
}

/// The storage generations a snapshot is written against.
///
/// The persistence lane owns these, not the UI. A snapshot travels to the
/// writer thread as a shared `Arc<WorkspaceState>` that nobody may mutate, so
/// the base generations ride alongside it instead of being stamped into the
/// snapshot's `generation` fields. [`WorkspaceState::generation`] and
/// [`CardState::generation`] are consequently ignored by the save path; they
/// are populated by the load path for the benefit of callers that want them.
#[derive(Debug, Clone, Copy)]
pub struct SnapshotBase<'a> {
    /// The workspace generation the snapshot was derived from.
    pub workspace: u64,
    /// Per-card generations from the previous commit. A card missing from the
    /// map is new, and must therefore be absent from storage.
    pub cards: &'a BTreeMap<CardId, u64>,
}

/// What storage holds after a workspace snapshot commits. The writer lane keeps
/// this as the base for its next snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceCommit {
    pub generation: u64,
    /// Every card row the workspace owns after the commit, tombstones included,
    /// so a later snapshot that still carries a closed card has a valid base.
    pub card_generations: BTreeMap<CardId, u64>,
}

struct StoreInner {
    connection: Connection,
    closed: bool,
}

/// A single serialized SQLite state store. All connection access passes through
/// one mutex, so callers cannot accidentally create a second writer lane.
pub struct StateStore {
    inner: Mutex<StoreInner>,
    /// Retained so the store can size its own WAL sidecar without asking a
    /// caller for the path it was opened with.
    path: PathBuf,
    boot_id: BootId,
    recovery: RecoveryReport,
}

impl StateStore {
    /// Copies a live store through SQLite's transactionally consistent VACUUM
    /// snapshot. The source is never startup-reconciled or checkpointed here.
    pub fn copy_snapshot(source: &Path, destination: &Path) -> Result<()> {
        drop(open_existing_private_file(source)?);
        let parent = destination
            .parent()
            .ok_or_else(|| StoreError::InvalidState("window database has no parent".into()))?;
        prepare_private_directory(parent)?;
        let temporary = tempfile::Builder::new()
            .prefix(".window-")
            .tempfile_in(parent)?;
        // VACUUM INTO accepts an existing empty file; tempfile keeps it private.
        let connection = Connection::open_with_flags(
            source,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(DEFAULT_BUSY_TIMEOUT)?;
        connection.execute(
            "VACUUM INTO ?1",
            [temporary.path().to_string_lossy().as_ref()],
        )?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(destination)
            .map_err(|error| StoreError::Io(error.error))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn load_window_view(&self) -> Result<Option<String>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        metadata(&inner.connection, "window_view_v1")
    }

    pub fn save_window_view(&self, value: &str) -> Result<()> {
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        set_metadata(&transaction, "window_view_v1", value)?;
        transaction.commit()?;
        Ok(())
    }
    pub fn open(path: impl AsRef<Path>, boot_id: BootId) -> Result<Self> {
        Self::open_with_options(path, boot_id, StoreOptions::default())
    }

    pub fn open_with_options(
        path: impl AsRef<Path>,
        boot_id: BootId,
        options: StoreOptions,
    ) -> Result<Self> {
        if boot_id.as_str().is_empty() {
            return Err(StoreError::InvalidState(
                "current boot ID must not be empty".into(),
            ));
        }
        if options.busy_timeout.is_zero() || options.busy_timeout > MAX_BUSY_TIMEOUT {
            return Err(StoreError::InvalidBusyTimeout {
                maximum: MAX_BUSY_TIMEOUT,
            });
        }

        let path = path.as_ref();
        prepare_private_database(path)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut connection = Connection::open_with_flags(path, flags)?;
        configure_connection(&connection, options.busy_timeout)?;
        enforce_sqlite_sidecars(path)?;
        migrate(&mut connection)?;
        let recovery = reconcile_startup(&mut connection, &boot_id)?;
        drop(open_existing_private_file(path)?);
        enforce_sqlite_sidecars(path)?;

        Ok(Self {
            inner: Mutex::new(StoreInner {
                connection,
                closed: false,
            }),
            path: path.to_path_buf(),
            boot_id,
            recovery,
        })
    }

    pub fn recovery_report(&self) -> &RecoveryReport {
        &self.recovery
    }

    pub fn current_boot_id(&self) -> &BootId {
        &self.boot_id
    }

    pub fn schema_version(&self) -> Result<u32> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        Ok(inner
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    /// Commits a workspace snapshot.
    ///
    /// `base` supplies the generations the snapshot was derived from; the
    /// `generation` fields inside `state` are ignored. Cards whose persisted
    /// content fingerprint is unchanged are left untouched, so a quiet
    /// workspace updates only its workspace row and navigation metadata per
    /// flush instead of rewriting every card and payload row.
    pub fn save_workspace(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
    ) -> Result<WorkspaceCommit> {
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let commit = save_workspace_transaction(&transaction, state, base)?;
        transaction.commit()?;
        Ok(commit)
    }

    /// Atomically replaces the terminal identity owned by an existing terminal card.
    ///
    /// This is the explicit relaunch cutover path. Normal workspace snapshots still
    /// reject payload identity changes through `ensure_payload_identity`.
    pub fn replace_terminal_identity(
        &self,
        card_id: CardId,
        expected_terminal_id: TerminalId,
        replacement: &TerminalSession,
    ) -> Result<()> {
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        replace_terminal_identity_transaction(
            &transaction,
            card_id,
            expected_terminal_id,
            replacement,
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn save_project(&self, project: &ProjectState) -> Result<u64> {
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let project_id = project.id.to_string();
        let actual = select_generation(&transaction, SELECT_PROJECT_GENERATION, &project_id)?;
        check_generation("project", project.id, project.generation, actual)?;
        reject_tombstone_resurrection(
            "project",
            project.id,
            select_existence(&transaction, SELECT_PROJECT_EXISTENCE, &project_id)?,
            &project.existence,
        )?;
        let next = next_generation(project.generation, "project", project.id)?;
        transaction
            .prepare_cached(
                "INSERT INTO projects (
                    project_id, generation, name, repository_json, existence_json,
                    updated_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(project_id) DO UPDATE SET
                    generation = excluded.generation,
                    name = excluded.name,
                    repository_json = excluded.repository_json,
                    existence_json = excluded.existence_json,
                    updated_at_unix_ms = excluded.updated_at_unix_ms",
            )?
            .execute(params![
                project_id,
                u64_to_i64(next, "project generation")?,
                project.name,
                to_json(&project.repository)?,
                to_json(&project.existence)?,
                now_unix_ms(),
            ])?;
        transaction.commit()?;
        Ok(next)
    }

    pub fn load_project(&self, id: ProjectId) -> Result<Option<ProjectState>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        inner
            .connection
            .query_row(
                "SELECT generation, name, repository_json, existence_json
                 FROM projects WHERE project_id = ?1",
                [id.to_string()],
                |row| {
                    let generation: i64 = row.get(0)?;
                    let name: String = row.get(1)?;
                    let repository: String = row.get(2)?;
                    let existence: String = row.get(3)?;
                    Ok((generation, name, repository, existence))
                },
            )
            .optional()?
            .map(|(generation, name, repository, existence)| {
                Ok(ProjectState {
                    id,
                    generation: i64_to_u64(generation, "project generation")?,
                    name,
                    repository: from_json("project repository", &repository)?,
                    existence: from_json("project existence", &existence)?,
                })
            })
            .transpose()
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectState>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let mut statement = inner.connection.prepare(
            "SELECT project_id, generation, name, repository_json, existence_json
             FROM projects
             ORDER BY updated_at_unix_ms DESC, project_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut projects = Vec::new();
        for row in rows {
            let (id, generation, name, repository, existence) = row?;
            projects.push(ProjectState {
                id: parse_id("project", &id)?,
                generation: i64_to_u64(generation, "project generation")?,
                name,
                repository: from_json("project repository", &repository)?,
                existence: from_json("project existence", &existence)?,
            });
        }
        Ok(projects)
    }

    pub fn save_worktree(&self, worktree: &WorktreeState) -> Result<u64> {
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let worktree_id = worktree.id.to_string();
        let actual = select_generation(&transaction, SELECT_WORKTREE_GENERATION, &worktree_id)?;
        check_generation("worktree", worktree.id, worktree.generation, actual)?;
        reject_tombstone_resurrection(
            "worktree",
            worktree.id,
            select_existence(&transaction, SELECT_WORKTREE_EXISTENCE, &worktree_id)?,
            &worktree.existence,
        )?;
        let next = next_generation(worktree.generation, "worktree", worktree.id)?;
        transaction
            .prepare_cached(
                "INSERT INTO worktrees (
                    worktree_id, project_id, generation, path_json, branch, existence_json,
                    updated_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(worktree_id) DO UPDATE SET
                    project_id = excluded.project_id,
                    generation = excluded.generation,
                    path_json = excluded.path_json,
                    branch = excluded.branch,
                    existence_json = excluded.existence_json,
                    updated_at_unix_ms = excluded.updated_at_unix_ms",
            )?
            .execute(params![
                worktree_id,
                worktree.project_id.to_string(),
                u64_to_i64(next, "worktree generation")?,
                to_json(&worktree.path)?,
                worktree.branch,
                to_json(&worktree.existence)?,
                now_unix_ms(),
            ])?;
        transaction.commit()?;
        Ok(next)
    }

    pub fn load_worktree(&self, id: WorktreeId) -> Result<Option<WorktreeState>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        inner
            .connection
            .query_row(
                "SELECT project_id, generation, path_json, branch, existence_json
                 FROM worktrees WHERE worktree_id = ?1",
                [id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?
            .map(|(project_id, generation, path, branch, existence)| {
                Ok(WorktreeState {
                    id,
                    project_id: parse_id("project", &project_id)?,
                    generation: i64_to_u64(generation, "worktree generation")?,
                    path: from_json("worktree path", &path)?,
                    branch,
                    existence: from_json("worktree existence", &existence)?,
                })
            })
            .transpose()
    }

    pub fn list_worktrees(&self) -> Result<Vec<WorktreeState>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let mut statement = inner.connection.prepare(
            "SELECT worktree_id, project_id, generation, path_json, branch, existence_json
             FROM worktrees
             ORDER BY updated_at_unix_ms DESC, worktree_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut worktrees = Vec::new();
        for row in rows {
            let (id, project_id, generation, path, branch, existence) = row?;
            worktrees.push(WorktreeState {
                id: parse_id("worktree", &id)?,
                project_id: parse_id("project", &project_id)?,
                generation: i64_to_u64(generation, "worktree generation")?,
                path: from_json("worktree path", &path)?,
                branch,
                existence: from_json("worktree existence", &existence)?,
            });
        }
        Ok(worktrees)
    }

    /// Lists only light manifests; card payloads, terminal recipes, browser
    /// histories, and transcripts remain untouched until explicitly loaded.
    pub fn list_workspaces(&self) -> Result<Vec<WorkspaceSummary>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let mut statement = inner.connection.prepare(
            "SELECT w.workspace_id, w.generation, w.existence_json, w.focused_card_id,
                    w.updated_at_unix_ms,
                    SUM(CASE WHEN c.existence_json = '\"open\"' THEN 1 ELSE 0 END)
             FROM workspaces w
             LEFT JOIN cards c ON c.workspace_id = w.workspace_id
             GROUP BY w.workspace_id
             ORDER BY w.updated_at_unix_ms DESC, w.workspace_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;

        let mut summaries = Vec::new();
        for row in rows {
            let (id, generation, existence, focused, updated_at, open_cards) = row?;
            summaries.push(WorkspaceSummary {
                id: parse_id("workspace", &id)?,
                generation: i64_to_u64(generation, "workspace generation")?,
                existence: from_json("workspace existence", &existence)?,
                open_card_count: i64_to_usize(open_cards, "open card count")?,
                focused: focused
                    .as_deref()
                    .map(|value| parse_id("card", value))
                    .transpose()?,
                updated_at_unix_ms: updated_at,
            });
        }
        Ok(summaries)
    }

    pub fn load_workspace_manifest(&self, id: WorkspaceId) -> Result<Option<WorkspaceManifest>> {
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner.connection.transaction()?;
        let manifest = load_workspace_manifest_connection(&transaction, id)?;
        transaction.commit()?;
        Ok(manifest)
    }

    pub fn load_card(
        &self,
        id: CardId,
        proof: Option<&HostSessionProof>,
    ) -> Result<Option<CardState>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        load_card_connection(&inner.connection, id, &self.boot_id, proof)
    }

    pub fn load_workspace(
        &self,
        id: WorkspaceId,
        proof: Option<&HostSessionProof>,
    ) -> Result<Option<WorkspaceState>> {
        Ok(self
            .load_workspace_restored(id, proof)?
            .map(|restored| restored.state))
    }

    /// Loads a workspace, tolerating individual cards that cannot be restored.
    ///
    /// Card decoding is fallible *per card*. One unreadable card — a terminal
    /// whose recipe row is gone, a payload written by a newer build, a browser
    /// history that no longer validates — used to abort the entire restore,
    /// which the shell then presents as an empty workspace with no projects and
    /// no worktrees, on every launch, with the cause never logged anywhere.
    /// Everything that decodes is now restored and every card that did not is
    /// reported in [`RestoredWorkspace::warnings`] for the shell to surface.
    ///
    /// A corrupt *manifest* is deliberately still all-or-nothing. That is a
    /// different class of failure: without a trustworthy manifest there is no
    /// card list, ordering, focus, or channel layout to restore cards *into*, so
    /// a partial result could not be distinguished from the user's real
    /// workspace and the next save would commit it over the intact rows.
    ///
    /// A card left out here is absent from the next snapshot, so the save path
    /// tombstones it — the row and its payload stay, marked closed. That is
    /// why the failure is *reported* rather than swallowed: the user has to
    /// learn a card was lost, at the moment it happens, instead of finding it
    /// closed later with no explanation.
    pub fn load_workspace_restored(
        &self,
        id: WorkspaceId,
        proof: Option<&HostSessionProof>,
    ) -> Result<Option<RestoredWorkspace>> {
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner.connection.transaction()?;
        let Some(manifest) = load_workspace_manifest_connection(&transaction, id)? else {
            return Ok(None);
        };

        let mut cards = Vec::with_capacity(manifest.cards.len());
        let mut warnings = Vec::new();
        for summary in &manifest.cards {
            match load_card_connection(&transaction, summary.id, &self.boot_id, proof) {
                Ok(Some(card)) => cards.push(card),
                Ok(None) => {
                    tracing::error!(
                        workspace = %id,
                        card = %summary.id,
                        "workspace manifest references a card row that no longer exists"
                    );
                    warnings.push(format!(
                        "{} ({}): the card record is missing from the database",
                        summary.title, summary.id
                    ));
                }
                Err(error) => {
                    tracing::error!(
                        workspace = %id,
                        card = %summary.id,
                        kind = summary.kind.as_str(),
                        %error,
                        "card could not be restored and was left out of the workspace"
                    );
                    warnings.push(format!("{} ({}): {error}", summary.title, summary.id));
                }
            }
        }
        transaction.commit()?;

        let mut state = WorkspaceState {
            schema_version: manifest.schema_version,
            id: manifest.id,
            generation: manifest.generation,
            existence: manifest.existence,
            cards,
            focused: manifest.focused,
            active_worktree: manifest.active_worktree,
            worktree_focus: manifest.worktree_focus,
            project_order: manifest.project_order,
            channels: manifest.channels,
            active_channel: manifest.active_channel,
        };
        if !warnings.is_empty() {
            retarget_dropped_cards(&mut state);
        }
        Ok(Some(RestoredWorkspace { state, warnings }))
    }

    pub fn save_transcript_metadata(
        &self,
        terminal_id: TerminalId,
        metadata: &TranscriptMetadata,
        expected_generation: u64,
    ) -> Result<u64> {
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = terminal_id.to_string();
        let actual = select_generation(&transaction, SELECT_TRANSCRIPT_GENERATION, &id)?;
        check_generation("transcript", terminal_id, expected_generation, actual)?;
        let next = next_generation(expected_generation, "transcript", terminal_id)?;
        transaction
            .prepare_cached(
                "INSERT INTO transcript_metadata (
                    terminal_id, generation, path_json, byte_len, durable_offset,
                    updated_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(terminal_id) DO UPDATE SET
                    generation = excluded.generation,
                    path_json = excluded.path_json,
                    byte_len = excluded.byte_len,
                    durable_offset = excluded.durable_offset,
                    updated_at_unix_ms = excluded.updated_at_unix_ms",
            )?
            .execute(params![
                id,
                u64_to_i64(next, "transcript generation")?,
                to_json(&metadata.path)?,
                u64_to_i64(metadata.byte_len, "transcript byte length")?,
                u64_to_i64(metadata.durable_offset, "transcript durable offset")?,
                metadata.updated_at_unix_ms,
            ])?;
        // The transcript path is part of the owning card's content fingerprint,
        // and this write bypassed the snapshot lane, so clear the fingerprint to
        // keep the "content_hash always describes the stored content" invariant
        // total. Without this, a later snapshot carrying the previous transcript
        // path could be skipped and silently leave the newer path in place.
        transaction.execute(INVALIDATE_CARD_HASH_BY_TERMINAL, [&id])?;
        transaction.commit()?;
        Ok(next)
    }

    pub fn load_transcript_metadata(
        &self,
        terminal_id: TerminalId,
    ) -> Result<Option<(u64, TranscriptMetadata)>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        inner
            .connection
            .query_row(
                "SELECT generation, path_json, byte_len, durable_offset, updated_at_unix_ms
                 FROM transcript_metadata WHERE terminal_id = ?1",
                [terminal_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?
            .map(|(generation, path, byte_len, durable_offset, updated_at)| {
                Ok((
                    i64_to_u64(generation, "transcript generation")?,
                    TranscriptMetadata {
                        path: from_json("transcript path", &path)?,
                        byte_len: i64_to_u64(byte_len, "transcript byte length")?,
                        durable_offset: i64_to_u64(durable_offset, "transcript durable offset")?,
                        updated_at_unix_ms: updated_at,
                    },
                ))
            })
            .transpose()
    }

    pub fn begin_operation(&self, operation: &PendingOperation) -> Result<()> {
        if operation.status != PendingOperationStatus::Pending {
            return Err(StoreError::InvalidState(
                "new journal operations must have pending status".into(),
            ));
        }
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // A resolved NotCreated operation is the only identity that may be retried.
        // The write transaction closes the check/insert race across store connections.
        if operation.entity_kind == "worktree_creation" {
            let occupied: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM pending_operations
                 WHERE entity_kind = ?1 AND entity_id = ?2 AND action = ?3
                 AND status != 'rolled_back')",
                params![operation.entity_kind, operation.entity_id, operation.action],
                |row| row.get(0),
            )?;
            if occupied {
                return Err(StoreError::InvalidState(
                    "worktree operation lease is occupied".into(),
                ));
            }
        }
        transaction.execute(
            "INSERT INTO pending_operations (
                operation_id, entity_kind, entity_id, action, base_generation, payload_json,
                status, created_at_unix_ms, updated_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                operation.id.to_string(),
                operation.entity_kind,
                operation.entity_id,
                operation.action,
                u64_to_i64(operation.base_generation, "operation base generation")?,
                to_json(&operation.payload)?,
                operation.status.as_str(),
                operation.created_at_unix_ms,
                operation.updated_at_unix_ms,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn resolve_operation(
        &self,
        id: OperationId,
        resolution: PendingOperationStatus,
    ) -> Result<()> {
        if !matches!(
            resolution,
            PendingOperationStatus::Applied | PendingOperationStatus::RolledBack
        ) {
            return Err(StoreError::InvalidState(
                "journal resolution must be applied or rolled_back".into(),
            ));
        }
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let changed = inner.connection.execute(
            "UPDATE pending_operations
             SET status = ?1, updated_at_unix_ms = ?2
             WHERE operation_id = ?3 AND status IN ('pending', 'needs_reconciliation')",
            params![resolution.as_str(), now_unix_ms(), id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::InvalidState(format!(
                "operation {id} is missing or already resolved"
            )));
        }
        Ok(())
    }

    /// Compare-and-swap the durable evidence and its generic journal status.
    /// A stale worker/reconciler must never overwrite newer evidence.
    pub fn update_operation(
        &self,
        id: OperationId,
        expected_payload: &Value,
        payload: &Value,
        status: PendingOperationStatus,
    ) -> Result<()> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let changed = inner.connection.execute(
            "UPDATE pending_operations SET payload_json = ?1, status = ?2,
             updated_at_unix_ms = ?3 WHERE operation_id = ?4 AND payload_json = ?5
             AND (status IN ('pending', 'needs_reconciliation')
                  OR (status = 'applied' AND ?2 = 'applied'))",
            params![
                to_json(payload)?,
                status.as_str(),
                now_unix_ms(),
                id.to_string(),
                to_json(expected_payload)?
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidState(format!(
                "stale or resolved operation {id}"
            )));
        }
        Ok(())
    }

    pub fn operations_requiring_reconciliation(&self) -> Result<Vec<PendingOperation>> {
        self.operation_rows(true)
    }

    pub fn journaled_worktree_operations(&self) -> Result<Vec<PendingOperation>> {
        self.operation_rows(false)
    }

    fn operation_rows(&self, unresolved_only: bool) -> Result<Vec<PendingOperation>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let mut statement = inner.connection.prepare(
            "SELECT operation_id, entity_kind, entity_id, action, base_generation, payload_json,
                    status, created_at_unix_ms, updated_at_unix_ms
             FROM pending_operations
             WHERE (?1 AND status IN ('pending', 'needs_reconciliation'))
                OR (NOT ?1 AND entity_kind = 'worktree_creation')
             ORDER BY created_at_unix_ms, operation_id",
        )?;
        let rows = statement.query_map([unresolved_only], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
            ))
        })?;
        let mut operations = Vec::new();
        for row in rows {
            let (id, entity_kind, entity_id, action, generation, payload, status, created, updated) =
                row?;
            operations.push(PendingOperation {
                id: parse_id("operation", &id)?,
                entity_kind,
                entity_id,
                action,
                base_generation: i64_to_u64(generation, "operation base generation")?,
                // Retain malformed bytes as data so one damaged payload does not
                // hide every other operation's recovery row.
                payload: serde_json::from_str(&payload).unwrap_or(Value::String(payload)),
                status: PendingOperationStatus::parse(&status)?,
                created_at_unix_ms: created,
                updated_at_unix_ms: updated,
            });
        }
        Ok(operations)
    }

    /// Records a clean shutdown and closes the store.
    ///
    /// There is no queue to flush here: the writer lane in `session` owns the
    /// debounce and calls [`StateStore::save_workspace`] directly, so anything
    /// still unwritten at this point is the caller's to persist first.
    pub fn shutdown(&self) -> Result<()> {
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        set_metadata(&transaction, "clean_shutdown", "1")?;
        set_metadata(
            &transaction,
            "last_shutdown_unix_ms",
            &now_unix_ms().to_string(),
        )?;
        transaction.commit()?;
        // Fold the WAL back into the database and reset it to zero length.
        // Without this the sidecar keeps its high-water mark forever — a 400 KB
        // database was shipping a 4 MB WAL — and a checkpoint failure is not
        // worth failing a clean shutdown over, so it only warns.
        checkpoint_wal(&inner.connection);
        inner.closed = true;
        Ok(())
    }

    /// Truncates the WAL once it has grown past
    /// [`WAL_CHECKPOINT_THRESHOLD_BYTES`], so a session that runs for days does
    /// not keep a multi-megabyte sidecar around between shutdowns.
    ///
    /// Returns whether a checkpoint was attempted. Sizing the sidecar with one
    /// `stat` keeps the common case free: the writer lane calls this after a
    /// flush, and a checkpoint is real work that acquires the write lock.
    pub fn checkpoint_wal_if_large(&self) -> Result<bool> {
        let wal = sqlite_sidecar_path(&self.path, "-wal");
        let oversized = std::fs::symlink_metadata(&wal)
            .map(|metadata| metadata.is_file() && metadata.len() >= WAL_CHECKPOINT_THRESHOLD_BYTES)
            .unwrap_or(false);
        if !oversized {
            return Ok(false);
        }
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        checkpoint_wal(&inner.connection);
        Ok(true)
    }

    /// Reclaims terminal transcript files the store can no longer account for.
    ///
    /// Transcripts are write-only diagnostics living beside the database, and
    /// nothing ever deleted them: a transcript whose terminal is gone stayed on
    /// disk forever, and a file written before the per-transcript cap existed
    /// was never reclaimed either. Measured on a normal installation this had
    /// reached 1.7 GB.
    ///
    /// The policy, in order:
    /// 1. a transcript whose terminal id is in neither `terminal_recipes` nor
    ///    `transcript_metadata` is orphaned and removed;
    /// 2. a transcript larger than [`TranscriptRetention::max_file_bytes`] is
    ///    removed — the writer refuses to append to an over-cap file, so its
    ///    bytes are dead weight;
    /// 3. if the survivors still exceed
    ///    [`TranscriptRetention::max_total_bytes`], the oldest go until they
    ///    fit.
    ///
    /// Nothing outside `directory` can be touched: entries come from one
    /// `read_dir` of it, each candidate is a single path component joined onto
    /// it, and a symlink — for the directory itself or for any entry — is
    /// refused rather than followed. Files that are not `<terminal-id>.log` are
    /// left alone; this directory is not assumed to be exclusively ours.
    ///
    /// `also_live` names terminal ids owned by databases other than this one.
    /// Every window has its own database but they all share one transcript
    /// directory, so an id this store does not know is not necessarily
    /// orphaned; see [`read_live_terminal_ids`] and
    /// [`TranscriptRetention::remove_orphans`].
    pub fn prune_transcripts(
        &self,
        directory: &Path,
        also_live: HashSet<String>,
        retention: TranscriptRetention,
    ) -> Result<TranscriptPruneReport> {
        let mut live = also_live;
        live.extend(self.live_terminal_ids()?);
        prune_transcript_directory(directory, &live, retention)
    }

    /// Every terminal id storage still knows about, from both tables that can
    /// name one: a recipe row can outlive its transcript metadata and vice
    /// versa, and either is reason enough to keep the file.
    pub fn live_terminal_ids(&self) -> Result<HashSet<String>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let mut live = HashSet::new();
        collect_live_terminal_ids(&inner.connection, &mut live)?;
        Ok(live)
    }

    fn lock_inner(&self) -> Result<MutexGuard<'_, StoreInner>> {
        self.inner.lock().map_err(|_| StoreError::MutexPoisoned)
    }
}

impl Drop for StateStore {
    fn drop(&mut self) {
        // A store that was never cleanly shut down — a discarded window, a
        // failed startup, a test fixture — still leaves its WAL behind, so the
        // truncating checkpoint happens here too. `shutdown` already did it and
        // marked the store closed, which is what keeps this from repeating it.
        let Ok(inner) = self.inner.lock() else {
            return;
        };
        if !inner.closed {
            checkpoint_wal(&inner.connection);
        }
    }
}

/// How much WAL the writer lane tolerates before folding it back into the
/// database.
///
/// SQLite's own auto-checkpoint keeps the WAL near 1000 pages but never shrinks
/// the file, so the sidecar sits at its high-water mark for the life of the
/// process. Truncating above 8 MiB bounds that without making the write lock
/// contended: a quiet session never reaches it, and a busy one pays one
/// checkpoint per 8 MiB of committed frames.
const WAL_CHECKPOINT_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

/// Folds the WAL back into the database and resets it to zero length,
/// reporting rather than propagating failure.
///
/// Checkpointing is pure housekeeping: every commit is already durable in the
/// WAL, so a checkpoint that loses a race with another connection's reader must
/// not turn into a save or shutdown error. `PRAGMA wal_checkpoint` reports that
/// race in its `busy` column instead of failing, which is why the row is read.
fn checkpoint_wal(connection: &Connection) {
    let outcome = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    });
    match outcome {
        Ok((busy, frames, checkpointed)) if busy == 0 => {
            tracing::debug!(frames, checkpointed, "checkpointed the session WAL");
        }
        Ok((_, frames, checkpointed)) => {
            tracing::warn!(
                frames,
                checkpointed,
                "session WAL checkpoint was blocked by another reader; the WAL was left in place"
            );
        }
        Err(error) => {
            tracing::warn!(%error, "session WAL checkpoint failed");
        }
    }
}

/// Suffix of a terminal transcript file, as written by the terminal host.
const TRANSCRIPT_SUFFIX: &str = ".log";

/// Terminal ids a window database still knows about, read without opening a
/// second writable store.
///
/// Every window has its own database and they all share one transcript
/// directory, so pruning has to ask every database before it can call a
/// transcript orphaned. The connection is read-only and `NOFOLLOW`, exactly
/// like [`StateStore::copy_snapshot`]: it never migrates, reconciles, or
/// writes to a database another window owns.
pub fn read_live_terminal_ids(path: &Path) -> Result<HashSet<String>> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(DEFAULT_BUSY_TIMEOUT)?;
    let mut live = HashSet::new();
    collect_live_terminal_ids(&connection, &mut live)?;
    Ok(live)
}

fn collect_live_terminal_ids(connection: &Connection, live: &mut HashSet<String>) -> Result<()> {
    for sql in [
        "SELECT terminal_id FROM terminal_recipes",
        "SELECT terminal_id FROM transcript_metadata",
    ] {
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            live.insert(row?);
        }
    }
    Ok(())
}

/// What pruning may keep on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptRetention {
    /// Largest single transcript kept, matching the 1 MiB cap the terminal
    /// transcript writer enforces. A larger file was produced by a build that
    /// predates the cap; the writer will never append to it again, so keeping
    /// it only costs disk.
    pub max_file_bytes: u64,
    /// Ceiling on the whole transcript directory. Transcripts are diagnostics
    /// for terminals the user still has open, so the budget is generous enough
    /// to hold a hundred capped transcripts and no more.
    pub max_total_bytes: u64,
    /// Whether an id nobody claims may be deleted.
    ///
    /// Set this only when the live id set is known to be complete. Every
    /// window keeps its own database but they all write into one transcript
    /// directory, so if one window's database could not be read, an unclaimed
    /// transcript may still belong to a terminal that window owns.
    pub remove_orphans: bool,
}

impl Default for TranscriptRetention {
    fn default() -> Self {
        Self {
            max_file_bytes: 1 << 20,
            max_total_bytes: 128 << 20,
            remove_orphans: true,
        }
    }
}

/// What one pruning pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranscriptPruneReport {
    pub removed_orphaned: usize,
    pub removed_oversized: usize,
    pub removed_for_budget: usize,
    pub retained: usize,
    pub retained_bytes: u64,
    /// Entries refused for being symlinks or otherwise not plain files. They
    /// are never removed and never followed.
    pub refused: usize,
}

impl TranscriptPruneReport {
    pub fn removed(&self) -> usize {
        self.removed_orphaned + self.removed_oversized + self.removed_for_budget
    }
}

/// A transcript that survived the per-file rules, and what the total budget
/// needs to know about it.
struct RetainedTranscript {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

fn prune_transcript_directory(
    directory: &Path,
    live: &HashSet<String>,
    retention: TranscriptRetention,
) -> Result<TranscriptPruneReport> {
    let mut report = TranscriptPruneReport::default();
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(StoreError::InvalidState(format!(
                "transcript directory {} is a symbolic link",
                directory.display()
            )));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(StoreError::InvalidState(format!(
                "transcript path {} is not a directory",
                directory.display()
            )));
        }
        Ok(_) => {}
        // Nothing has ever written a transcript here. Not an error.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error.into()),
    }

    let mut retained: Vec<RetainedTranscript> = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(TRANSCRIPT_SUFFIX))
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        // `name` is a single component from this directory's own listing, so
        // joining it cannot escape the directory.
        let path = directory.join(&name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not inspect transcript");
                report.refused += 1;
                continue;
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            tracing::warn!(
                path = %path.display(),
                "refusing to prune a transcript entry that is not a plain file"
            );
            report.refused += 1;
            continue;
        }

        if retention.remove_orphans && !live.contains(id) {
            if remove_transcript(&path, "its terminal no longer exists") {
                report.removed_orphaned += 1;
            } else {
                report.refused += 1;
            }
            continue;
        }
        if metadata.len() > retention.max_file_bytes {
            if remove_transcript(&path, "it exceeds the per-transcript cap") {
                report.removed_oversized += 1;
            } else {
                report.refused += 1;
            }
            continue;
        }
        retained.push(RetainedTranscript {
            path,
            bytes: metadata.len(),
            modified: metadata.modified().unwrap_or(UNIX_EPOCH),
        });
    }

    let mut total: u64 = retained.iter().map(|entry| entry.bytes).sum();
    if total > retention.max_total_bytes {
        retained.sort_by_key(|entry| entry.modified);
        let mut index = 0;
        while total > retention.max_total_bytes && index < retained.len() {
            let entry = &retained[index];
            if remove_transcript(&entry.path, "the transcript directory is over budget") {
                total -= entry.bytes;
                report.removed_for_budget += 1;
            } else {
                report.refused += 1;
            }
            index += 1;
        }
        retained.drain(..index);
    }
    report.retained = retained.len();
    report.retained_bytes = total;
    Ok(report)
}

/// Unlinks one transcript, reporting whether it is gone.
///
/// A failure here is not fatal: the file simply stays, and the next pass tries
/// again. `remove_file` never follows a final symlink, and the caller has
/// already refused those anyway.
fn remove_transcript(path: &Path, reason: &str) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => {
            tracing::info!(path = %path.display(), reason, "pruned terminal transcript");
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not prune terminal transcript");
            false
        }
    }
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

fn enforce_sqlite_sidecars(path: &Path) -> io::Result<()> {
    for suffix in ["-wal", "-shm"] {
        let sidecar = sqlite_sidecar_path(path, suffix);
        if migrate_existing_private_file(&sidecar)? {
            drop(open_existing_private_file(&sidecar)?);
        }
    }
    Ok(())
}

fn prepare_private_database(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    prepare_private_directory(parent)?;
    enforce_sqlite_sidecars(path)?;
    if migrate_existing_private_file(path)? {
        drop(open_existing_private_file(path)?);
    } else {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        validate_and_privatize_open_file(path, &file)?;
    }
    Ok(())
}

fn ensure_open(inner: &StoreInner) -> Result<()> {
    if inner.closed {
        Err(StoreError::Closed)
    } else {
        Ok(())
    }
}

fn configure_connection(connection: &Connection, busy_timeout: Duration) -> Result<()> {
    connection.busy_timeout(busy_timeout)?;
    // `NORMAL` is the documented companion to WAL, and deliberately not `FULL`.
    // Under WAL a commit is durable across an application crash either way: the
    // committed frames are already in the WAL file and the next open replays
    // them. `FULL` only additionally survives a kernel panic or power loss, and
    // it buys that by forcing an fsync on every single commit — which this
    // application performs as often as every 250 ms while the user is simply
    // interacting with the UI. Losing the last quarter second of window layout
    // to a power cut is the right trade; do not "fix" this back to FULL.
    //
    // The two pragmas are coupled: `NORMAL` is only this safe *because* the
    // journal is WAL, so both are verified below rather than assumed.
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;",
    )?;

    let foreign_keys: i64 =
        connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(StoreError::InvalidState(
            "SQLite foreign key enforcement could not be enabled".into(),
        ));
    }
    let journal_mode: String =
        connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::InvalidState(format!(
            "SQLite journal mode is `{journal_mode}`, but write-ahead logging is required"
        )));
    }
    // 0 = OFF, 1 = NORMAL, 2 = FULL, 3 = EXTRA.
    let synchronous: i64 = connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
    if synchronous != 1 {
        return Err(StoreError::InvalidState(format!(
            "SQLite synchronous mode is {synchronous}, but NORMAL (1) is required"
        )));
    }
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let current: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > CURRENT_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found: current,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }
    if current == CURRENT_SCHEMA_VERSION {
        return Ok(());
    }

    // SQLite cannot alter a CHECK constraint. Rebuild its parent table with
    // foreign keys temporarily disabled outside the transaction, preserving
    // every child row and checking the complete graph before commit.
    connection.pragma_update(None, "foreign_keys", false)?;
    let result = (|| -> Result<()> {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if current < 1 {
            migrate_to_v1(&transaction)?;
        }
        if current < 2 {
            migrate_to_v2(&transaction)?;
        }
        if current < 3 {
            migrate_to_v3(&transaction)?;
        }
        if current < 4 {
            migrate_to_v4(&transaction)?;
        }
        let violation = transaction
            .prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_some();
        if violation {
            return Err(StoreError::InvalidState(
                "migration violates foreign keys".into(),
            ));
        }
        transaction.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)?;
        transaction.commit()?;
        Ok(())
    })();
    connection.pragma_update(None, "foreign_keys", true)?;
    result?;
    Ok(())
}

fn migrate_to_v1(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "CREATE TABLE metadata (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
         ) STRICT;

         CREATE TABLE projects (
            project_id TEXT PRIMARY KEY NOT NULL,
            generation INTEGER NOT NULL CHECK (generation >= 0),
            name TEXT NOT NULL,
            repository_json TEXT NOT NULL,
            existence_json TEXT NOT NULL,
            updated_at_unix_ms INTEGER NOT NULL
         ) STRICT;

         CREATE TABLE worktrees (
            worktree_id TEXT PRIMARY KEY NOT NULL,
            project_id TEXT NOT NULL REFERENCES projects(project_id),
            generation INTEGER NOT NULL CHECK (generation >= 0),
            path_json TEXT NOT NULL,
            branch TEXT,
            existence_json TEXT NOT NULL,
            updated_at_unix_ms INTEGER NOT NULL
         ) STRICT;

         CREATE TABLE workspaces (
            workspace_id TEXT PRIMARY KEY NOT NULL,
            generation INTEGER NOT NULL CHECK (generation >= 0),
            schema_version INTEGER NOT NULL CHECK (schema_version >= 0),
            existence_json TEXT NOT NULL,
            focused_card_id TEXT,
            updated_at_unix_ms INTEGER NOT NULL
         ) STRICT;

         CREATE TABLE cards (
            card_id TEXT PRIMARY KEY NOT NULL,
            workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
            ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
            generation INTEGER NOT NULL CHECK (generation >= 0),
            title TEXT NOT NULL,
            width_fraction REAL NOT NULL CHECK (width_fraction > 0.0),
            existence_json TEXT NOT NULL,
            kind_tag TEXT NOT NULL CHECK (
                kind_tag IN ('terminal', 'browser', 'pull_requests', 'repository')
            ),
            content_json TEXT,
            updated_at_unix_ms INTEGER NOT NULL
         ) STRICT;

         CREATE TABLE terminal_recipes (
            terminal_id TEXT PRIMARY KEY NOT NULL,
            card_id TEXT UNIQUE NOT NULL REFERENCES cards(card_id),
            generation INTEGER NOT NULL CHECK (generation >= 0),
            recipe_json TEXT NOT NULL,
            continuity_json TEXT NOT NULL,
            resume_hint TEXT,
            updated_at_unix_ms INTEGER NOT NULL
         ) STRICT;

         CREATE TABLE transcript_metadata (
            terminal_id TEXT PRIMARY KEY NOT NULL REFERENCES terminal_recipes(terminal_id),
            generation INTEGER NOT NULL CHECK (generation >= 0),
            path_json TEXT NOT NULL,
            byte_len INTEGER NOT NULL CHECK (byte_len >= 0),
            durable_offset INTEGER NOT NULL CHECK (durable_offset >= 0),
            updated_at_unix_ms INTEGER NOT NULL
         ) STRICT;

         CREATE TABLE browser_restore (
            browser_id TEXT PRIMARY KEY NOT NULL,
            card_id TEXT UNIQUE NOT NULL REFERENCES cards(card_id),
            generation INTEGER NOT NULL CHECK (generation >= 0),
            current_url TEXT NOT NULL,
            history_json TEXT NOT NULL,
            history_index INTEGER NOT NULL CHECK (history_index >= 0),
            scroll_x REAL NOT NULL,
            scroll_y REAL NOT NULL,
            fidelity TEXT NOT NULL CHECK (fidelity IN ('history_and_scroll', 'url_only')),
            updated_at_unix_ms INTEGER NOT NULL
         ) STRICT;

         CREATE TABLE pending_operations (
            operation_id TEXT PRIMARY KEY NOT NULL,
            entity_kind TEXT NOT NULL,
            entity_id TEXT NOT NULL,
            action TEXT NOT NULL,
            base_generation INTEGER NOT NULL CHECK (base_generation >= 0),
            payload_json TEXT NOT NULL,
            status TEXT NOT NULL CHECK (
                status IN ('pending', 'needs_reconciliation', 'applied', 'rolled_back')
            ),
            created_at_unix_ms INTEGER NOT NULL,
            updated_at_unix_ms INTEGER NOT NULL
         ) STRICT;",
    )?;
    Ok(())
}

fn migrate_to_v2(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "CREATE INDEX worktrees_project_idx ON worktrees(project_id);
         CREATE INDEX cards_workspace_order_idx ON cards(workspace_id, ordinal);
         CREATE INDEX pending_operations_status_idx
            ON pending_operations(status, created_at_unix_ms);
         CREATE INDEX terminal_recipes_card_idx ON terminal_recipes(card_id);
         CREATE INDEX browser_restore_card_idx ON browser_restore(card_id);",
    )?;
    Ok(())
}

fn migrate_to_v3(transaction: &Transaction<'_>) -> Result<()> {
    // `content_hash` fingerprints everything a card write puts in the database
    // (see `card_content_hash`), which is what lets a flush leave an unchanged
    // card's rows alone instead of rewriting them four times a second. Rows
    // written before this migration hash as NULL, which never matches a
    // computed fingerprint, so an upgraded database simply rewrites each card
    // once and is then in the fast path.
    transaction.execute_batch("ALTER TABLE cards ADD COLUMN content_hash INTEGER;")?;
    Ok(())
}

fn migrate_to_v4(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "CREATE TABLE cards_v4 (
            card_id TEXT PRIMARY KEY NOT NULL,
            workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
            ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
            generation INTEGER NOT NULL CHECK (generation >= 0),
            title TEXT NOT NULL,
            width_fraction REAL NOT NULL CHECK (width_fraction > 0.0),
            existence_json TEXT NOT NULL,
            kind_tag TEXT NOT NULL CHECK (
                kind_tag IN ('terminal', 'chat', 'browser', 'pull_requests', 'repository')
            ),
            content_json TEXT,
            updated_at_unix_ms INTEGER NOT NULL,
            content_hash INTEGER
         ) STRICT;
         INSERT INTO cards_v4 SELECT * FROM cards;
         DROP TABLE cards;
         ALTER TABLE cards_v4 RENAME TO cards;
         CREATE INDEX cards_workspace_order_idx ON cards(workspace_id, ordinal);",
    )?;
    Ok(())
}

fn reconcile_startup(connection: &mut Connection, boot_id: &BootId) -> Result<RecoveryReport> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let clean_shutdown = metadata(&transaction, "clean_shutdown")?
        .map(|value| value == "1")
        .unwrap_or(true);
    let prior_boot = metadata(&transaction, "last_boot_id")?;
    let boot_changed = prior_boot
        .as_deref()
        .is_some_and(|prior| prior != boot_id.as_str());
    let dirty_shutdown = !clean_shutdown;

    let operations_requiring_reconciliation = if dirty_shutdown || boot_changed {
        transaction.execute(
            "UPDATE pending_operations
             SET status = 'needs_reconciliation', updated_at_unix_ms = ?1
             WHERE status = 'pending'",
            [now_unix_ms()],
        )?
    } else {
        0
    };

    let mut terminals_downgraded = 0;
    if dirty_shutdown || boot_changed {
        let terminal_rows = {
            let mut statement = transaction.prepare(
                "SELECT terminal_id, recipe_json, continuity_json FROM terminal_recipes",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };

        for (terminal_id, recipe_json, continuity_json) in terminal_rows {
            let recipe = from_json("terminal recipe", &recipe_json)?;
            let continuity: TerminalContinuity =
                from_json("terminal continuity", &continuity_json)?;
            let downgraded = match continuity {
                TerminalContinuity::Live {
                    host_session_id,
                    boot_id: recorded_boot,
                    host_incarnation,
                } if !boot_changed => Some(TerminalContinuity::DetachedSameBoot {
                    host_session_id,
                    boot_id: recorded_boot,
                    host_incarnation,
                }),
                TerminalContinuity::Live { .. } | TerminalContinuity::DetachedSameBoot { .. }
                    if boot_changed =>
                {
                    Some(TerminalContinuity::RelaunchAvailable {
                        recipe,
                        last_exit: None,
                    })
                }
                _ => None,
            };
            if let Some(downgraded) = downgraded {
                transaction.execute(
                    "UPDATE terminal_recipes
                     SET continuity_json = ?1, updated_at_unix_ms = ?2
                     WHERE terminal_id = ?3",
                    params![to_json(&downgraded)?, now_unix_ms(), terminal_id],
                )?;
                // The downgrade rewrote a value the owning card's fingerprint
                // covers, so the fingerprint must stop describing the row. Left
                // alone, a reattach that rebuilds the pre-crash `Live`
                // continuity fingerprints identically, the snapshot lane skips
                // the write as unchanged, and the downgraded value nobody
                // agrees with stays in storage permanently.
                transaction.execute(INVALIDATE_CARD_HASH_BY_TERMINAL, [&terminal_id])?;
                terminals_downgraded += 1;
            }
        }
    }

    set_metadata(&transaction, "clean_shutdown", "0")?;
    set_metadata(&transaction, "last_boot_id", boot_id.as_str())?;
    set_metadata(
        &transaction,
        "last_startup_unix_ms",
        &now_unix_ms().to_string(),
    )?;
    transaction.commit()?;

    Ok(RecoveryReport {
        dirty_shutdown,
        boot_changed,
        operations_requiring_reconciliation,
        terminals_downgraded,
    })
}

/// Statements on the save path are spelled out as literals rather than built
/// with `format!`. That keeps every one of them cacheable by SQLite's
/// per-connection statement cache — the save path runs several times a second,
/// so parsing the same SQL over and over was pure overhead — and it keeps table
/// and column names structurally incapable of being interpolated.
const SELECT_WORKSPACE_GENERATION: &str =
    "SELECT generation FROM workspaces WHERE workspace_id = ?1";
const SELECT_WORKSPACE_EXISTENCE: &str =
    "SELECT existence_json FROM workspaces WHERE workspace_id = ?1";
const SELECT_PROJECT_GENERATION: &str = "SELECT generation FROM projects WHERE project_id = ?1";
const SELECT_PROJECT_EXISTENCE: &str = "SELECT existence_json FROM projects WHERE project_id = ?1";
const SELECT_WORKTREE_GENERATION: &str = "SELECT generation FROM worktrees WHERE worktree_id = ?1";
const SELECT_WORKTREE_EXISTENCE: &str =
    "SELECT existence_json FROM worktrees WHERE worktree_id = ?1";
const SELECT_TRANSCRIPT_GENERATION: &str =
    "SELECT generation FROM transcript_metadata WHERE terminal_id = ?1";
const SELECT_CARD_WORKSPACE: &str = "SELECT workspace_id FROM cards WHERE card_id = ?1";

const UPSERT_WORKSPACE: &str = "INSERT INTO workspaces (
        workspace_id, generation, schema_version, existence_json, focused_card_id,
        updated_at_unix_ms
     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
     ON CONFLICT(workspace_id) DO UPDATE SET
        generation = excluded.generation,
        schema_version = excluded.schema_version,
        existence_json = excluded.existence_json,
        focused_card_id = excluded.focused_card_id,
        updated_at_unix_ms = excluded.updated_at_unix_ms";

const UPSERT_CARD: &str = "INSERT INTO cards (
        card_id, workspace_id, ordinal, generation, title, width_fraction, existence_json,
        kind_tag, content_json, content_hash, updated_at_unix_ms
     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
     ON CONFLICT(card_id) DO UPDATE SET
        workspace_id = excluded.workspace_id,
        ordinal = excluded.ordinal,
        generation = excluded.generation,
        title = excluded.title,
        width_fraction = excluded.width_fraction,
        existence_json = excluded.existence_json,
        kind_tag = excluded.kind_tag,
        content_json = excluded.content_json,
        content_hash = excluded.content_hash,
        updated_at_unix_ms = excluded.updated_at_unix_ms";

/// Tombstoning rewrites `existence_json` without recomputing the fingerprint,
/// so the fingerprint is cleared: `content_hash` must never describe anything
/// other than the row it sits on.
const TOMBSTONE_CARD: &str = "UPDATE cards
     SET generation = ?1, existence_json = ?2, content_hash = NULL, updated_at_unix_ms = ?3
     WHERE card_id = ?4";

/// Clearing `cards.content_hash` is how a write that bypasses the snapshot lane
/// keeps the fingerprint's contract: it must never describe anything other than
/// the row it sits on. A cleared fingerprint matches no computed one, so the
/// next snapshot takes the full write path instead of skipping it.
const INVALIDATE_CARD_HASH: &str = "UPDATE cards SET content_hash = NULL WHERE card_id = ?1";

/// The same clear, for writers that reach a card through the terminal it owns.
const INVALIDATE_CARD_HASH_BY_TERMINAL: &str = "UPDATE cards SET content_hash = NULL
     WHERE card_id = (SELECT card_id FROM terminal_recipes WHERE terminal_id = ?1)";

const UPSERT_TERMINAL_RECIPE: &str = "INSERT INTO terminal_recipes (
        terminal_id, card_id, generation, recipe_json, continuity_json, resume_hint,
        updated_at_unix_ms
     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
     ON CONFLICT(terminal_id) DO UPDATE SET
        card_id = excluded.card_id,
        generation = excluded.generation,
        recipe_json = excluded.recipe_json,
        continuity_json = excluded.continuity_json,
        resume_hint = excluded.resume_hint,
        updated_at_unix_ms = excluded.updated_at_unix_ms";

const UPSERT_TRANSCRIPT_PATH: &str = "INSERT INTO transcript_metadata (
        terminal_id, generation, path_json, byte_len, durable_offset, updated_at_unix_ms
     ) VALUES (?1, ?2, ?3, 0, 0, ?4)
     ON CONFLICT(terminal_id) DO UPDATE SET
        path_json = excluded.path_json,
        updated_at_unix_ms = excluded.updated_at_unix_ms";

const UPSERT_BROWSER_RESTORE: &str = "INSERT INTO browser_restore (
        browser_id, card_id, generation, current_url, history_json, history_index,
        scroll_x, scroll_y, fidelity, updated_at_unix_ms
     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
     ON CONFLICT(browser_id) DO UPDATE SET
        card_id = excluded.card_id,
        generation = excluded.generation,
        current_url = excluded.current_url,
        history_json = excluded.history_json,
        history_index = excluded.history_index,
        scroll_x = excluded.scroll_x,
        scroll_y = excluded.scroll_y,
        fidelity = excluded.fidelity,
        updated_at_unix_ms = excluded.updated_at_unix_ms";

/// A payload table whose primary identity a card owns for life.
struct PayloadIdentity {
    /// Column name, used only in diagnostics.
    id_column: &'static str,
    card_by_payload: &'static str,
    payload_by_card: &'static str,
}

const TERMINAL_PAYLOAD_IDENTITY: PayloadIdentity = PayloadIdentity {
    id_column: "terminal_id",
    card_by_payload: "SELECT card_id FROM terminal_recipes WHERE terminal_id = ?1",
    payload_by_card: "SELECT terminal_id FROM terminal_recipes WHERE card_id = ?1",
};

const BROWSER_PAYLOAD_IDENTITY: PayloadIdentity = PayloadIdentity {
    id_column: "browser_id",
    card_by_payload: "SELECT card_id FROM browser_restore WHERE browser_id = ?1",
    payload_by_card: "SELECT browser_id FROM browser_restore WHERE card_id = ?1",
};

fn save_workspace_transaction(
    transaction: &Transaction<'_>,
    state: &WorkspaceState,
    base: SnapshotBase<'_>,
) -> Result<WorkspaceCommit> {
    validate_workspace(state)?;
    let workspace_id = state.id.to_string();
    let actual = select_generation(transaction, SELECT_WORKSPACE_GENERATION, &workspace_id)?;
    check_generation("workspace", state.id, base.workspace, actual)?;
    reject_tombstone_resurrection(
        "workspace",
        state.id,
        select_existence(transaction, SELECT_WORKSPACE_EXISTENCE, &workspace_id)?,
        &state.existence,
    )?;
    let next = next_generation(base.workspace, "workspace", state.id)?;
    let timestamp = now_unix_ms();
    let focused = state
        .focused
        .filter(|focused| {
            state
                .cards
                .iter()
                .any(|card| card.id == *focused && card.existence.is_open())
        })
        .map(|id| id.to_string());

    transaction
        .prepare_cached(UPSERT_WORKSPACE)?
        .execute(params![
            workspace_id,
            u64_to_i64(next, "workspace generation")?,
            i64::from(state.schema_version),
            to_json(&state.existence)?,
            focused,
            timestamp,
        ])?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:active_worktree"),
        &to_json(&state.active_worktree)?,
    )?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:worktree_focus"),
        &to_json(&state.worktree_focus)?,
    )?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:project_order"),
        &to_json(&state.project_order)?,
    )?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:{CHANNELS_FIELD}"),
        &to_json(&state.channels)?,
    )?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:{ACTIVE_CHANNEL_FIELD}"),
        &to_json(&state.active_channel)?,
    )?;

    let existing = existing_cards(transaction, &workspace_id)?;
    let incoming_ids = state
        .cards
        .iter()
        .map(|card| card.id)
        .collect::<BTreeSet<_>>();
    let mut card_generations = BTreeMap::new();

    for (ordinal, card) in state.cards.iter().enumerate() {
        let kind = card_kind(&card.kind);
        let card_id = card.id.to_string();
        let expected = base.cards.get(&card.id).copied().unwrap_or(0);
        let stored = existing.get(&card.id);

        if stored.is_none() {
            // Only a card this workspace has never owned can collide with a row
            // belonging to another workspace; every entry in `existing` is ours
            // by construction, which keeps this lookup off the steady-state
            // path entirely.
            if let Some(other) = transaction
                .prepare_cached(SELECT_CARD_WORKSPACE)?
                .query_row([&card_id], |row| row.get::<_, String>(0))
                .optional()?
                && other != workspace_id
            {
                return Err(StoreError::InvalidState(format!(
                    "card {card_id} already belongs to workspace {other}"
                )));
            }
        }

        if let Some(stored) = stored {
            if matches!(stored.existence, Existence::Closed { .. })
                && !matches!(card.existence, Existence::Closed { .. })
            {
                return Err(StoreError::InvalidState(format!(
                    "tombstoned card {card_id} cannot be resurrected"
                )));
            }
            if expected != stored.generation {
                return Err(StoreError::StaleGeneration {
                    entity: format!("card {card_id}"),
                    expected,
                    actual: stored.generation,
                });
            }
            if stored.kind != kind {
                validate_card_conversion(transaction, card, stored.kind)?;
            }
        } else if expected != 0 {
            return Err(StoreError::StaleGeneration {
                entity: format!("new card {card_id}"),
                expected,
                actual: 0,
            });
        }
        if let CardKind::Chat(session) = &card.kind {
            validate_chat_identity(
                transaction,
                card.id,
                session,
                stored.map(|stored| stored.kind),
            )?;
        }

        let write = CardWrite::build(&workspace_id, &card_id, ordinal, card, kind)?;
        if stored.is_some_and(|stored| stored.content_hash == Some(write.content_hash)) {
            // Every value this write would bind, apart from `generation` and
            // `updated_at_unix_ms`, is already stored byte for byte. Both of
            // those columns describe the last *content* change, so leaving the
            // rows untouched is not an approximation of the write — it is the
            // write, with zero dirtied pages and zero WAL frames.
            card_generations.insert(card.id, expected);
            continue;
        }

        let card_generation = next_generation(expected, "card", card.id)?;
        transaction.prepare_cached(UPSERT_CARD)?.execute(params![
            card_id,
            workspace_id,
            write.ordinal,
            u64_to_i64(card_generation, "card generation")?,
            card.title,
            f64::from(card.width_fraction),
            write.existence_json,
            kind.as_str(),
            write.content_json,
            write.content_hash,
            timestamp,
        ])?;
        save_card_payload(transaction, &card_id, &write, card_generation, timestamp)?;
        card_generations.insert(card.id, card_generation);
    }

    for (card_id, stored) in &existing {
        if incoming_ids.contains(card_id) {
            continue;
        }
        if matches!(stored.existence, Existence::Closed { .. }) {
            card_generations.insert(*card_id, stored.generation);
            continue;
        }
        let generation = next_generation(stored.generation, "card", *card_id)?;
        let tombstone = Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: timestamp,
                reason: Some("absent from a newer workspace snapshot".into()),
            },
        };
        transaction
            .prepare_cached(TOMBSTONE_CARD)?
            .execute(params![
                u64_to_i64(generation, "card generation")?,
                to_json(&tombstone)?,
                timestamp,
                card_id.to_string(),
            ])?;
        card_generations.insert(*card_id, generation);
    }

    Ok(WorkspaceCommit {
        generation: next,
        card_generations,
    })
}

/// Kind changes are restricted to an exact conversation handoff, within the
/// same workspace/card CAS transaction as the following payload write.
fn validate_card_conversion(
    transaction: &Transaction<'_>,
    card: &CardState,
    previous: PersistedCardKind,
) -> Result<()> {
    let invalid = || {
        StoreError::InvalidState(format!(
            "card {} has an invalid chat/terminal conversion",
            card.id
        ))
    };
    match (previous, &card.kind) {
        (PersistedCardKind::Terminal, CardKind::Chat(session)) => {
            let source = session.terminal.as_ref().ok_or_else(invalid)?;
            let (id, recipe): (String, String) = transaction.query_row(
                "SELECT terminal_id, recipe_json FROM terminal_recipes WHERE card_id = ?1",
                [card.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if source.id != session.id
                || id != source.id.to_string()
                || from_json::<crate::model::LaunchRecipe>("terminal recipe", &recipe)?
                    != source.launch
                || !source_chat_launch_matches(source, session)
                || exact_chat_resume(session).is_none()
            {
                return Err(invalid());
            }
        }
        (PersistedCardKind::Chat, CardKind::Terminal(terminal)) => {
            let content: String = transaction.query_row(
                "SELECT content_json FROM cards WHERE card_id = ?1",
                [card.id.to_string()],
                |row| row.get(0),
            )?;
            let session: crate::chat::Session = from_json("chat card", &content)?;
            let reattach = session.launch.provider == crate::agent::Provider::Codex
                && session.terminal.as_ref().is_some_and(|source| {
                    terminal.id == source.id
                        && terminal.launch == source.launch
                        && terminal.continuity.live_proof().is_some()
                        && terminal.continuity.live_proof() == source.continuity.live_proof()
                });
            if !reattach
                && (terminal.id == session.id
                    || exact_chat_resume(&session).as_ref() != Some(&terminal.launch))
            {
                return Err(invalid());
            }
            if session
                .terminal
                .as_ref()
                .is_some_and(|source| source.transcript_path != terminal.transcript_path)
            {
                return Err(invalid());
            }
            if terminal.id != session.id {
                let owned: Option<String> = transaction
                    .query_row(
                        "SELECT card_id FROM terminal_recipes WHERE terminal_id = ?1",
                        [terminal.id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if owned.is_some() {
                    return Err(invalid());
                }
                // Move the terminal key and its transcript metadata together.
                // Deferral preserves offsets instead of deleting transcript evidence.
                transaction.pragma_update(None, "defer_foreign_keys", true)?;
                transaction.execute(
                    "UPDATE terminal_recipes SET terminal_id = ?1 WHERE card_id = ?2 AND terminal_id = ?3",
                    params![terminal.id.to_string(), card.id.to_string(), session.id.to_string()],
                )?;
                transaction.execute(
                    "UPDATE transcript_metadata SET terminal_id = ?1 WHERE terminal_id = ?2",
                    params![terminal.id.to_string(), session.id.to_string()],
                )?;
            }
        }
        _ => return Err(invalid()),
    }
    Ok(())
}

fn source_chat_launch_matches(source: &TerminalSession, session: &crate::chat::Session) -> bool {
    if let Some(recipe) = source.agent_resume_recipe() {
        return recipe == session.launch.recipe
            && crate::model::agent_recovery::resume_recipe(&recipe)
                == session.launch.terminal_recipe();
    }
    source
        .resume_hint
        .as_deref()
        .and_then(crate::model::agent_recovery::observed_launch_from_hint)
        .as_ref()
        == Some(&session.launch.recipe)
        || source.launch == session.launch.recipe
}

fn exact_chat_resume(session: &crate::chat::Session) -> Option<crate::model::LaunchRecipe> {
    session.launch.terminal_recipe()
}

fn validate_chat_identity(
    transaction: &Transaction<'_>,
    card: CardId,
    session: &crate::chat::Session,
    previous: Option<PersistedCardKind>,
) -> Result<()> {
    if session
        .terminal
        .as_ref()
        .is_some_and(|source| source.id != session.id)
    {
        return Err(StoreError::InvalidState(format!(
            "chat card {card} changed source identity"
        )));
    }
    if previous == Some(PersistedCardKind::Chat) {
        let content: String = transaction.query_row(
            "SELECT content_json FROM cards WHERE card_id = ?1",
            [card.to_string()],
            |row| row.get(0),
        )?;
        let stored: crate::chat::Session = from_json("chat card", &content)?;
        if stored.id != session.id
            || stored.launch.provider != session.launch.provider
            || stored.launch.recipe != session.launch.recipe
            || stored.terminal != session.terminal
            || (stored
                .launch
                .session_id
                .as_ref()
                .is_some_and(|id| Some(id) != session.launch.session_id.as_ref())
                && (stored.proof.is_none() || stored.proof != session.proof))
        {
            return Err(StoreError::InvalidState(format!(
                "chat card {card} changed conversation identity"
            )));
        }
    }
    Ok(())
}

/// Everything a card write binds, materialized once.
///
/// The content fingerprint is derived from exactly these values, which is what
/// makes skipping an unchanged card provably equivalent to writing it: a field
/// that would change the stored bytes cannot fail to change the fingerprint,
/// because the fingerprint is computed from the stored bytes themselves rather
/// than from a hand-maintained list of [`CardState`] fields.
struct CardWrite<'card> {
    ordinal: i64,
    existence_json: String,
    content_json: Option<String>,
    payload: CardPayloadWrite<'card>,
    content_hash: i64,
}

enum CardPayloadWrite<'card> {
    Terminal {
        terminal_id: String,
        recipe_json: String,
        continuity_json: String,
        resume_hint: Option<&'card str>,
        transcript_path_json: Option<String>,
    },
    Browser {
        browser_id: String,
        current_url: String,
        history_json: String,
        history_index: i64,
        scroll_x: f64,
        scroll_y: f64,
        fidelity: &'static str,
    },
    /// Chat, pull-request and repository cards keep their whole payload in the
    /// `cards.content_json` column, so there is no side table to write.
    Inline,
}

impl<'card> CardWrite<'card> {
    fn build(
        workspace_id: &str,
        card_id: &str,
        ordinal: usize,
        card: &'card CardState,
        kind: PersistedCardKind,
    ) -> Result<Self> {
        let ordinal = usize_to_i64(ordinal, "card ordinal")?;
        let existence_json = to_json(&card.existence)?;
        let content_json = match &card.kind {
            CardKind::Chat(session) => Some(to_json(session)?),
            CardKind::PullRequests(view) => Some(to_json(view)?),
            CardKind::Repository(view) => Some(to_json(view)?),
            CardKind::Terminal(_) | CardKind::Browser(_) => None,
        };
        let payload = match &card.kind {
            CardKind::Terminal(terminal) => CardPayloadWrite::Terminal {
                terminal_id: terminal.id.to_string(),
                recipe_json: to_json(&terminal.launch)?,
                continuity_json: to_json(&terminal.continuity)?,
                resume_hint: terminal.resume_hint.as_deref(),
                transcript_path_json: terminal.transcript_path.as_ref().map(to_json).transpose()?,
            },
            CardKind::Browser(browser) => {
                let descriptor = browser.durable_descriptor();
                validate_browser_descriptor(&descriptor)?;
                CardPayloadWrite::Browser {
                    browser_id: descriptor.id.to_string(),
                    current_url: descriptor.current_url.as_str().to_owned(),
                    history_json: to_json(&descriptor.history)?,
                    history_index: usize_to_i64(descriptor.history_index, "browser history index")?,
                    scroll_x: descriptor.scroll_x,
                    scroll_y: descriptor.scroll_y,
                    fidelity: match descriptor.fidelity {
                        BrowserRestoreFidelity::HistoryAndScroll => "history_and_scroll",
                        BrowserRestoreFidelity::UrlOnly => "url_only",
                        BrowserRestoreFidelity::ExactLive => {
                            return Err(StoreError::InvalidState(
                                "exact-live browser fidelity is not durable".into(),
                            ));
                        }
                    },
                }
            }
            CardKind::Chat(_) | CardKind::PullRequests(_) | CardKind::Repository(_) => {
                CardPayloadWrite::Inline
            }
        };

        let mut hasher = ContentHasher::new();
        hasher
            .text(workspace_id)
            .text(card_id)
            .integer(ordinal.cast_unsigned())
            .text(&card.title)
            .real(f64::from(card.width_fraction))
            .text(&existence_json)
            .text(kind.as_str())
            .optional_text(content_json.as_deref());
        match &payload {
            CardPayloadWrite::Terminal {
                terminal_id,
                recipe_json,
                continuity_json,
                resume_hint,
                transcript_path_json,
            } => {
                hasher
                    .text("terminal")
                    .text(terminal_id)
                    .text(recipe_json)
                    .text(continuity_json)
                    .optional_text(*resume_hint)
                    .optional_text(transcript_path_json.as_deref());
            }
            CardPayloadWrite::Browser {
                browser_id,
                current_url,
                history_json,
                history_index,
                scroll_x,
                scroll_y,
                fidelity,
            } => {
                hasher
                    .text("browser")
                    .text(browser_id)
                    .text(current_url)
                    .text(history_json)
                    .integer(history_index.cast_unsigned())
                    .real(*scroll_x)
                    .real(*scroll_y)
                    .text(fidelity);
            }
            CardPayloadWrite::Inline => {
                hasher.text("inline");
            }
        }

        Ok(Self {
            ordinal,
            existence_json,
            content_json,
            payload,
            content_hash: hasher.finish(),
        })
    }
}

/// A deterministic 64-bit content fingerprint.
///
/// The value is persisted in `cards.content_hash`, so the algorithm has to stay
/// byte-identical across builds and toolchains; `std`'s `DefaultHasher`
/// explicitly makes no such promise. FNV-1a plus a splitmix64 avalanche costs a
/// few instructions per byte and diffuses well enough that two distinct card
/// payloads collide with probability around 2^-64, and the only consequence of
/// a collision would be one skipped rewrite.
struct ContentHasher(u64);

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

impl ContentHasher {
    fn new() -> Self {
        Self(FNV_OFFSET_BASIS)
    }

    fn byte(&mut self, byte: u8) {
        self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
    }

    fn integer(&mut self, value: u64) -> &mut Self {
        for byte in value.to_le_bytes() {
            self.byte(byte);
        }
        self
    }

    fn real(&mut self, value: f64) -> &mut Self {
        self.integer(value.to_bits())
    }

    /// Length-prefixed so that field boundaries cannot alias: `("ab", "c")` must
    /// not fingerprint the same as `("a", "bc")`.
    fn text(&mut self, value: &str) -> &mut Self {
        self.integer(value.len() as u64);
        for byte in value.as_bytes() {
            self.byte(*byte);
        }
        self
    }

    fn optional_text(&mut self, value: Option<&str>) -> &mut Self {
        match value {
            Some(value) => self.integer(1).text(value),
            None => self.integer(0),
        }
    }

    fn finish(&self) -> i64 {
        let mut value = self.0;
        value ^= value >> 30;
        value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value ^= value >> 27;
        value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^= value >> 31;
        value.cast_signed()
    }
}

fn save_card_payload(
    transaction: &Transaction<'_>,
    card_id: &str,
    write: &CardWrite<'_>,
    generation: u64,
    timestamp: i64,
) -> Result<()> {
    match &write.payload {
        CardPayloadWrite::Terminal {
            terminal_id,
            recipe_json,
            continuity_json,
            resume_hint,
            transcript_path_json,
        } => {
            ensure_payload_identity(
                transaction,
                &TERMINAL_PAYLOAD_IDENTITY,
                terminal_id,
                card_id,
            )?;
            transaction
                .prepare_cached(UPSERT_TERMINAL_RECIPE)?
                .execute(params![
                    terminal_id,
                    card_id,
                    u64_to_i64(generation, "terminal recipe generation")?,
                    recipe_json,
                    continuity_json,
                    resume_hint,
                    timestamp,
                ])?;
            if let Some(path_json) = transcript_path_json {
                transaction
                    .prepare_cached(UPSERT_TRANSCRIPT_PATH)?
                    .execute(params![terminal_id, 0_i64, path_json, timestamp])?;
            }
        }
        CardPayloadWrite::Browser {
            browser_id,
            current_url,
            history_json,
            history_index,
            scroll_x,
            scroll_y,
            fidelity,
        } => {
            ensure_payload_identity(transaction, &BROWSER_PAYLOAD_IDENTITY, browser_id, card_id)?;
            transaction
                .prepare_cached(UPSERT_BROWSER_RESTORE)?
                .execute(params![
                    browser_id,
                    card_id,
                    u64_to_i64(generation, "browser generation")?,
                    current_url,
                    history_json,
                    history_index,
                    scroll_x,
                    scroll_y,
                    fidelity,
                    timestamp,
                ])?;
        }
        CardPayloadWrite::Inline => {}
    }
    Ok(())
}

fn replace_terminal_identity_transaction(
    transaction: &Transaction<'_>,
    card_id: CardId,
    expected_terminal_id: TerminalId,
    replacement: &TerminalSession,
) -> Result<()> {
    if replacement.id == expected_terminal_id {
        return Err(StoreError::InvalidState(format!(
            "terminal relaunch for card {card_id} must use a fresh terminal identity"
        )));
    }
    let TerminalContinuity::RelaunchAvailable { recipe, .. } = &replacement.continuity else {
        return Err(StoreError::InvalidState(format!(
            "replacement terminal {} must be relaunchable",
            replacement.id
        )));
    };
    if recipe != &replacement.launch {
        return Err(StoreError::InvalidState(format!(
            "replacement terminal {} has mismatched launch recipes",
            replacement.id
        )));
    }

    if let Some(existing_card) = transaction
        .query_row(
            "SELECT card_id FROM terminal_recipes WHERE terminal_id = ?1",
            [replacement.id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        && existing_card != card_id.to_string()
    {
        return Err(StoreError::InvalidState(format!(
            "terminal_id {} already belongs to card {existing_card}",
            replacement.id
        )));
    }

    let Some(kind) = transaction
        .query_row(
            "SELECT kind_tag FROM cards WHERE card_id = ?1",
            [card_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        // A terminal can exit before its first debounced workspace snapshot.
        // The ordered snapshot following this operation establishes the fresh
        // identity without ever persisting the superseded one.
        return Ok(());
    };
    if kind != PersistedCardKind::Terminal.as_str() {
        return Err(StoreError::InvalidState(format!(
            "card {card_id} is not a terminal card"
        )));
    }

    let (actual_terminal_id, generation) = transaction
        .query_row(
            "SELECT terminal_id, generation FROM terminal_recipes WHERE card_id = ?1",
            [card_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidState(format!("terminal card {card_id} has no terminal payload"))
        })?;
    if actual_terminal_id == replacement.id.to_string() {
        return Ok(());
    }
    if actual_terminal_id != expected_terminal_id.to_string() {
        return Err(StoreError::InvalidState(format!(
            "terminal card {card_id} expected identity {expected_terminal_id}, found {actual_terminal_id}"
        )));
    }
    let recipe_json = to_json(&replacement.launch)?;
    let continuity_json = to_json(&replacement.continuity)?;
    let timestamp = now_unix_ms();
    transaction.execute(
        "DELETE FROM transcript_metadata WHERE terminal_id = ?1",
        [expected_terminal_id.to_string()],
    )?;
    let deleted = transaction.execute(
        "DELETE FROM terminal_recipes WHERE terminal_id = ?1 AND card_id = ?2",
        params![expected_terminal_id.to_string(), card_id.to_string()],
    )?;
    if deleted != 1 {
        return Err(StoreError::InvalidState(format!(
            "terminal identity {expected_terminal_id} changed during relaunch"
        )));
    }
    transaction.execute(
        "INSERT INTO terminal_recipes (
            terminal_id, card_id, generation, recipe_json, continuity_json, resume_hint,
            updated_at_unix_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            replacement.id.to_string(),
            card_id.to_string(),
            generation,
            recipe_json,
            continuity_json,
            replacement.resume_hint,
            timestamp,
        ],
    )?;
    if let Some(path) = &replacement.transcript_path {
        transaction.execute(
            "INSERT INTO transcript_metadata (
                terminal_id, generation, path_json, byte_len, durable_offset,
                updated_at_unix_ms
             ) VALUES (?1, ?2, ?3, 0, 0, ?4)",
            params![
                replacement.id.to_string(),
                generation,
                to_json(path)?,
                timestamp
            ],
        )?;
    }
    // This path rewrote the card's terminal payload behind the snapshot lane's
    // back, so the card's fingerprint no longer describes what is stored.
    // Clearing it forces the next snapshot to take the full write path, which
    // is what makes `ensure_payload_identity` still reject a snapshot that is
    // carrying the superseded terminal identity.
    transaction.execute(INVALIDATE_CARD_HASH, [card_id.to_string()])?;
    Ok(())
}

fn validate_workspace(state: &WorkspaceState) -> Result<()> {
    let mut ids = BTreeSet::new();
    for card in &state.cards {
        if !card.width_fraction.is_finite() || card.width_fraction <= 0.0 {
            return Err(StoreError::InvalidState(format!(
                "card {} has invalid width fraction {}",
                card.id, card.width_fraction
            )));
        }
        if !ids.insert(card.id) {
            return Err(StoreError::InvalidState(format!(
                "workspace {} contains duplicate card {}",
                state.id, card.id
            )));
        }
    }
    if let Some(focused) = state.focused
        && !state
            .cards
            .iter()
            .any(|card| card.id == focused && card.existence.is_open())
    {
        return Err(StoreError::InvalidState(format!(
            "workspace {} focuses missing or closed card {}",
            state.id, focused
        )));
    }
    let mut channel_ids = BTreeSet::new();
    let mut owned = BTreeSet::new();
    for channel in &state.channels {
        if !channel_ids.insert(channel.id) {
            return Err(StoreError::InvalidState(format!(
                "duplicate channel {}",
                channel.id
            )));
        }
        for id in &channel.cards {
            if !ids.contains(id) || !owned.insert(*id) {
                return Err(StoreError::InvalidState(format!(
                    "channel {} references missing or multiply owned card {id}",
                    channel.id
                )));
            }
        }
        if channel.focused.is_some_and(|id| {
            !channel.cards.contains(&id)
                || !state
                    .cards
                    .iter()
                    .any(|card| card.id == id && card.existence.is_open())
        }) {
            return Err(StoreError::InvalidState(format!(
                "channel {} has invalid focus",
                channel.id
            )));
        }
    }
    if state
        .active_channel
        .is_some_and(|id| !channel_ids.contains(&id))
    {
        return Err(StoreError::InvalidState("active channel is missing".into()));
    }
    Ok(())
}

/// Repairs the navigation references a dropped card leaves dangling.
///
/// Restoring a workspace without one of its cards is only useful if the result
/// is still *savable*: [`validate_workspace`] rejects a focus or a channel
/// membership that names a card the workspace does not contain, so leaving
/// them behind would trade one lost card for a workspace that can never be
/// written again.
fn retarget_dropped_cards(state: &mut WorkspaceState) {
    let open: HashSet<CardId> = state
        .cards
        .iter()
        .filter(|card| card.existence.is_open())
        .map(|card| card.id)
        .collect();
    if state
        .focused
        .is_some_and(|focused| !open.contains(&focused))
    {
        state.focused = state
            .cards
            .iter()
            .find(|card| card.existence.is_open())
            .map(|card| card.id);
    }
    normalize_project_channels(&mut state.channels, &mut state.active_channel, &open);
}

/// The stored shape of a card, as far as the save path needs to know it.
struct ExistingCard {
    kind: PersistedCardKind,
    existence: Existence,
    generation: u64,
    /// `None` for rows written before schema v3, and for rows whose content was
    /// changed by a path that does not maintain the fingerprint. Either way it
    /// never matches a computed fingerprint, so the card is simply rewritten.
    content_hash: Option<i64>,
}

fn existing_cards(
    transaction: &Transaction<'_>,
    workspace_id: &str,
) -> Result<BTreeMap<CardId, ExistingCard>> {
    let mut statement = transaction.prepare_cached(
        "SELECT card_id, kind_tag, existence_json, generation, content_hash
         FROM cards WHERE workspace_id = ?1",
    )?;
    let rows = statement.query_map([workspace_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Option<i64>>(4)?,
        ))
    })?;
    let mut cards = BTreeMap::new();
    for row in rows {
        let (id, kind, existence, generation, content_hash) = row?;
        cards.insert(
            parse_id("card", &id)?,
            ExistingCard {
                kind: PersistedCardKind::parse(&kind)?,
                existence: from_json("card existence", &existence)?,
                generation: i64_to_u64(generation, "card generation")?,
                content_hash,
            },
        );
    }
    Ok(cards)
}

fn ensure_payload_identity(
    transaction: &Transaction<'_>,
    identity: &PayloadIdentity,
    payload_id: &str,
    card_id: &str,
) -> Result<()> {
    let id_column = identity.id_column;
    if let Some(existing_card) = transaction
        .prepare_cached(identity.card_by_payload)?
        .query_row([payload_id], |row| row.get::<_, String>(0))
        .optional()?
        && existing_card != card_id
    {
        return Err(StoreError::InvalidState(format!(
            "{id_column} {payload_id} already belongs to card {existing_card}"
        )));
    }

    if let Some(existing_id) = transaction
        .prepare_cached(identity.payload_by_card)?
        .query_row([card_id], |row| row.get::<_, String>(0))
        .optional()?
        && existing_id != payload_id
    {
        return Err(StoreError::InvalidState(format!(
            "card {card_id} cannot replace stable {id_column} {existing_id} with {payload_id}"
        )));
    }
    Ok(())
}

fn load_workspace_manifest_connection(
    connection: &Connection,
    id: WorkspaceId,
) -> Result<Option<WorkspaceManifest>> {
    let workspace = connection
        .query_row(
            "SELECT generation, schema_version, existence_json, focused_card_id,
                    updated_at_unix_ms
             FROM workspaces WHERE workspace_id = ?1",
            [id.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((generation, schema_version, existence, focused, updated_at)) = workspace else {
        return Ok(None);
    };

    let mut statement = connection.prepare(
        "SELECT card_id, generation, ordinal, title, width_fraction, existence_json, kind_tag
         FROM cards WHERE workspace_id = ?1 ORDER BY ordinal, card_id",
    )?;
    let rows = statement.query_map([id.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, f64>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    let mut cards = Vec::new();
    for row in rows {
        let (card_id, card_generation, ordinal, title, width, card_existence, kind) = row?;
        cards.push(CardSummary {
            id: parse_id("card", &card_id)?,
            generation: i64_to_u64(card_generation, "card generation")?,
            ordinal: i64_to_usize(ordinal, "card ordinal")?,
            title,
            width_fraction: width as f32,
            existence: from_json("card existence", &card_existence)?,
            kind: PersistedCardKind::parse(&kind)?,
        });
    }

    let mut channels = metadata(connection, &format!("workspace:{id}:navigation:{CHANNELS_FIELD}"))?
        .map(|value| from_json("workspace channels", &value))
        .transpose()?
        .unwrap_or_default();
    let mut active_channel = metadata(
        connection,
        &format!("workspace:{id}:navigation:{ACTIVE_CHANNEL_FIELD}"),
    )?
    .map(|value| from_json("workspace active channel", &value))
    .transpose()?
    .flatten();
    normalize_project_channels(
        &mut channels,
        &mut active_channel,
        &cards
            .iter()
            .filter(|card| card.existence.is_open())
            .map(|card| card.id)
            .collect(),
    );

    Ok(Some(WorkspaceManifest {
        id,
        generation: i64_to_u64(generation, "workspace generation")?,
        schema_version: u32::try_from(schema_version).map_err(|_| {
            StoreError::InvalidState(format!("invalid workspace schema version {schema_version}"))
        })?,
        existence: from_json("workspace existence", &existence)?,
        focused: focused
            .as_deref()
            .map(|value| parse_id("card", value))
            .transpose()?,
        active_worktree: metadata(
            connection,
            &format!("workspace:{id}:navigation:active_worktree"),
        )?
        .map(|value| from_json("workspace active worktree", &value))
        .transpose()?
        .flatten(),
        worktree_focus: metadata(
            connection,
            &format!("workspace:{id}:navigation:worktree_focus"),
        )?
        .map(|value| from_json("workspace worktree focus", &value))
        .transpose()?
        .unwrap_or_default(),
        project_order: metadata(
            connection,
            &format!("workspace:{id}:navigation:project_order"),
        )?
        .map(|value| from_json("workspace project order", &value))
        .transpose()?
        .unwrap_or_default(),
        channels,
        active_channel,
        cards,
        updated_at_unix_ms: updated_at,
    }))
}

fn load_card_connection(
    connection: &Connection,
    id: CardId,
    current_boot: &BootId,
    proof: Option<&HostSessionProof>,
) -> Result<Option<CardState>> {
    let row = connection
        .query_row(
            "SELECT generation, title, width_fraction, existence_json, kind_tag, content_json
             FROM cards WHERE card_id = ?1",
            [id.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((generation, title, width, existence, kind, content)) = row else {
        return Ok(None);
    };
    let kind = match PersistedCardKind::parse(&kind)? {
        PersistedCardKind::Terminal => {
            CardKind::Terminal(load_terminal(connection, id, current_boot, proof)?)
        }
        PersistedCardKind::Chat => CardKind::Chat(from_json(
            "chat card",
            content.as_deref().ok_or_else(|| {
                StoreError::InvalidState(format!("chat card {id} has no payload"))
            })?,
        )?),
        PersistedCardKind::Browser => CardKind::Browser(load_browser(connection, id)?),
        PersistedCardKind::PullRequests => CardKind::PullRequests(from_json(
            "pull request card",
            content.as_deref().ok_or_else(|| {
                StoreError::InvalidState(format!("pull request card {id} has no payload"))
            })?,
        )?),
        PersistedCardKind::Repository => CardKind::Repository(from_json(
            "repository card",
            content.as_deref().ok_or_else(|| {
                StoreError::InvalidState(format!("repository card {id} has no payload"))
            })?,
        )?),
    };

    Ok(Some(CardState {
        id,
        generation: i64_to_u64(generation, "card generation")?,
        title,
        width_fraction: width as f32,
        existence: from_json("card existence", &existence)?,
        exposure: Exposure::Offscreen,
        residency: Residency::Cold,
        kind,
    }))
}

fn load_terminal(
    connection: &Connection,
    card_id: CardId,
    current_boot: &BootId,
    proof: Option<&HostSessionProof>,
) -> Result<TerminalSession> {
    // `.optional()` rather than `?`: a terminal card whose recipe row is gone
    // is a per-card restore failure the caller can report, not a raw
    // `QueryReturnedNoRows` that used to abort the whole workspace restore.
    let recipe = connection
        .query_row(
            "SELECT terminal_id, recipe_json, continuity_json, resume_hint
             FROM terminal_recipes WHERE card_id = ?1",
            [card_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((terminal_id, recipe_json, continuity_json, resume_hint)) = recipe else {
        return Err(StoreError::InvalidState(format!(
            "terminal card {card_id} has no stored launch recipe"
        )));
    };
    let terminal_id: TerminalId = parse_id("terminal", &terminal_id)?;
    let launch = from_json("terminal recipe", &recipe_json)?;
    let stored = from_json("terminal continuity", &continuity_json)?;
    let continuity = continuity_for_restore(stored, &launch, current_boot, proof);
    let transcript_path = connection
        .query_row(
            "SELECT path_json FROM transcript_metadata WHERE terminal_id = ?1",
            [terminal_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|path| from_json("transcript path", &path))
        .transpose()?;

    Ok(TerminalSession {
        id: terminal_id,
        launch,
        transcript_path,
        resume_hint,
        continuity,
    })
}

fn continuity_for_restore(
    continuity: TerminalContinuity,
    recipe: &crate::model::LaunchRecipe,
    current_boot: &BootId,
    proof: Option<&HostSessionProof>,
) -> TerminalContinuity {
    match continuity {
        TerminalContinuity::Live {
            host_session_id,
            boot_id,
            host_incarnation,
        }
        | TerminalContinuity::DetachedSameBoot {
            host_session_id,
            boot_id,
            host_incarnation,
        } => {
            if &boot_id != current_boot {
                return TerminalContinuity::RelaunchAvailable {
                    recipe: recipe.clone(),
                    last_exit: None,
                };
            }
            let exact_match = proof.is_some_and(|proof| {
                proof.is_complete()
                    && proof.host_session_id == host_session_id
                    && proof.boot_id == boot_id
                    && proof.host_incarnation == host_incarnation
            });
            if exact_match {
                TerminalContinuity::Live {
                    host_session_id,
                    boot_id,
                    host_incarnation,
                }
            } else {
                TerminalContinuity::DetachedSameBoot {
                    host_session_id,
                    boot_id,
                    host_incarnation,
                }
            }
        }
        TerminalContinuity::RelaunchAvailable { last_exit, .. } => {
            TerminalContinuity::RelaunchAvailable {
                recipe: recipe.clone(),
                last_exit,
            }
        }
        other @ (TerminalContinuity::Exited { .. } | TerminalContinuity::Failed { .. }) => other,
    }
}

fn load_browser(connection: &Connection, card_id: CardId) -> Result<BrowserSession> {
    // Optional for the same reason as the terminal recipe: a missing payload
    // row is a diagnosable per-card failure, not a bare `QueryReturnedNoRows`.
    let row = connection
        .query_row(
            "SELECT browser_id, current_url, history_json, history_index, scroll_x, scroll_y,
                    fidelity
             FROM browser_restore WHERE card_id = ?1",
            [card_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, f64>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((browser_id, current_url, history_json, history_index, scroll_x, scroll_y, fidelity)) =
        row
    else {
        return Err(StoreError::InvalidState(format!(
            "browser card {card_id} has no stored restore state"
        )));
    };
    let fidelity = match fidelity.as_str() {
        "history_and_scroll" => BrowserRestoreFidelity::HistoryAndScroll,
        "url_only" => BrowserRestoreFidelity::UrlOnly,
        other => {
            return Err(StoreError::InvalidState(format!(
                "invalid browser restore fidelity `{other}`"
            )));
        }
    };
    let mut history: Vec<url::Url> = from_json("browser history", &history_json)?;
    let mut history_index = i64_to_usize(history_index, "browser history index")?;
    // A vector written by a build that predates the cap is trimmed on the way
    // in, so an oversized history is reclaimed instead of carried forever.
    crate::model::trim_browser_history(&mut history, &mut history_index);
    let browser = BrowserSession {
        id: parse_id("browser", &browser_id)?,
        current_url: url::Url::parse(&current_url).map_err(|error| {
            StoreError::InvalidState(format!("invalid persisted browser URL: {error}"))
        })?,
        history,
        history_index,
        scroll_x,
        scroll_y,
        fidelity,
    };
    validate_browser_descriptor(&browser)?;
    Ok(browser)
}

fn validate_browser_descriptor(browser: &BrowserSession) -> Result<()> {
    if browser.history.is_empty() {
        return Err(StoreError::InvalidState(format!(
            "browser {} has empty durable history",
            browser.id
        )));
    }
    if browser.history_index >= browser.history.len() {
        return Err(StoreError::InvalidState(format!(
            "browser {} history index {} exceeds history length {}",
            browser.id,
            browser.history_index,
            browser.history.len()
        )));
    }
    if !browser.scroll_x.is_finite() || !browser.scroll_y.is_finite() {
        return Err(StoreError::InvalidState(format!(
            "browser {} has non-finite scroll coordinates",
            browser.id
        )));
    }
    Ok(())
}

fn card_kind(kind: &CardKind) -> PersistedCardKind {
    match kind {
        CardKind::Terminal(_) => PersistedCardKind::Terminal,
        CardKind::Chat(_) => PersistedCardKind::Chat,
        CardKind::Browser(_) => PersistedCardKind::Browser,
        CardKind::PullRequests(_) => PersistedCardKind::PullRequests,
        CardKind::Repository(_) => PersistedCardKind::Repository,
    }
}

fn select_generation(
    transaction: &Transaction<'_>,
    sql: &'static str,
    id: &str,
) -> Result<Option<u64>> {
    transaction
        .prepare_cached(sql)?
        .query_row([id], |row| row.get::<_, i64>(0))
        .optional()?
        .map(|value| i64_to_u64(value, "stored generation"))
        .transpose()
}

fn select_existence(
    transaction: &Transaction<'_>,
    sql: &'static str,
    id: &str,
) -> Result<Option<Existence>> {
    transaction
        .prepare_cached(sql)?
        .query_row([id], |row| row.get::<_, String>(0))
        .optional()?
        .map(|value| from_json("entity existence", &value))
        .transpose()
}

fn reject_tombstone_resurrection(
    kind: &str,
    id: impl std::fmt::Display,
    stored: Option<Existence>,
    incoming: &Existence,
) -> Result<()> {
    if matches!(stored, Some(Existence::Closed { .. }))
        && !matches!(incoming, Existence::Closed { .. })
    {
        return Err(StoreError::InvalidState(format!(
            "tombstoned {kind} {id} cannot be resurrected"
        )));
    }
    Ok(())
}

fn check_generation(
    kind: &str,
    id: impl std::fmt::Display,
    expected: u64,
    actual: Option<u64>,
) -> Result<()> {
    let actual = actual.unwrap_or(0);
    if expected != actual {
        return Err(StoreError::StaleGeneration {
            entity: format!("{kind} {id}"),
            expected,
            actual,
        });
    }
    Ok(())
}

fn next_generation(expected: u64, kind: &str, id: impl std::fmt::Display) -> Result<u64> {
    expected.checked_add(1).ok_or_else(|| {
        StoreError::InvalidState(format!("{kind} {id} exhausted its generation counter"))
    })
}

fn metadata(connection: &Connection, key: &str) -> Result<Option<String>> {
    Ok(connection
        .prepare_cached("SELECT value FROM metadata WHERE key = ?1")?
        .query_row([key], |row| row.get(0))
        .optional()?)
}

fn set_metadata(transaction: &Transaction<'_>, key: &str, value: &str) -> Result<()> {
    transaction
        .prepare_cached(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )?
        .execute(params![key, value])?;
    Ok(())
}

fn to_json(value: &impl Serialize) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

fn from_json<T: DeserializeOwned>(kind: &str, value: &str) -> Result<T> {
    serde_json::from_str(value)
        .map_err(|error| StoreError::InvalidState(format!("invalid persisted {kind}: {error}")))
}

fn parse_id<T>(kind: &'static str, value: &str) -> Result<T>
where
    T: FromStr<Err = uuid::Error>,
{
    value.parse().map_err(|source| StoreError::InvalidUuid {
        kind,
        value: value.to_owned(),
        source,
    })
}

fn now_unix_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn u64_to_i64(value: u64, kind: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| StoreError::InvalidState(format!("{kind} exceeds SQLite INTEGER range")))
}

fn usize_to_i64(value: usize, kind: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| StoreError::InvalidState(format!("{kind} exceeds SQLite INTEGER range")))
}

fn i64_to_u64(value: i64, kind: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| StoreError::InvalidState(format!("negative {kind}")))
}

fn i64_to_usize(value: i64, kind: &str) -> Result<usize> {
    usize::try_from(value).map_err(|_| StoreError::InvalidState(format!("invalid {kind}")))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::PathBuf,
    };

    use tempfile::tempdir;
    use url::Url;

    use super::*;
    use crate::model::{
        BrowserId, BrowserSession, CardState, HostIncarnation, HostSessionId, LaunchRecipe,
        TerminalSession,
    };

    fn boot(value: &str) -> BootId {
        BootId::new(value)
    }

    fn recipe() -> LaunchRecipe {
        LaunchRecipe {
            program: "/usr/bin/fish".into(),
            arguments: vec!["--login".into()],
            working_directory: PathBuf::from("/tmp/project"),
            environment: BTreeMap::from([("TERM".into(), "xterm-256color".into())]),
        }
    }

    fn workspace_with_terminal(boot_id: &BootId) -> WorkspaceState {
        let launch = recipe();
        let card_id = CardId::new();
        WorkspaceState {
            schema_version: 1,
            id: WorkspaceId::new(),
            generation: 0,
            existence: Existence::Open,
            cards: vec![CardState {
                id: card_id,
                generation: 0,
                title: "terminal".into(),
                width_fraction: 0.6,
                existence: Existence::Open,
                exposure: Exposure::Visible,
                residency: Residency::Hot,
                kind: CardKind::Terminal(TerminalSession {
                    id: TerminalId::new(),
                    launch: launch.clone(),
                    transcript_path: Some(PathBuf::from("/tmp/transcript")),
                    resume_hint: Some("provider-session".into()),
                    continuity: TerminalContinuity::Live {
                        host_session_id: HostSessionId::new("session-a"),
                        boot_id: boot_id.clone(),
                        host_incarnation: HostIncarnation::new("incarnation-a"),
                    },
                }),
            }],
            focused: Some(card_id),
            active_worktree: None,
            worktree_focus: HashMap::new(),
            project_order: Vec::new(),
            channels: Vec::new(),
            active_channel: None,
        }
    }

    /// Tracks base generations across successive saves the way the writer lane
    /// in `session` does, so tests exercise the same call shape production does.
    #[derive(Debug, Default)]
    struct SaveLane {
        workspace: u64,
        cards: BTreeMap<CardId, u64>,
    }

    impl SaveLane {
        fn save(&mut self, store: &StateStore, state: &WorkspaceState) -> Result<WorkspaceCommit> {
            let commit = store.save_workspace(
                state,
                SnapshotBase {
                    workspace: self.workspace,
                    cards: &self.cards,
                },
            )?;
            self.workspace = commit.generation;
            self.cards.clone_from(&commit.card_generations);
            Ok(commit)
        }
    }

    fn agent_workspace(provider: crate::agent::Provider) -> (WorkspaceState, crate::chat::Session) {
        let mut workspace = workspace_with_terminal(&boot("chat-boot"));
        let CardKind::Terminal(source) = &mut workspace.cards[0].kind else {
            unreachable!()
        };
        let id = "77777777-1111-4111-8111-111111111111";
        source.launch.program = provider.executable().into();
        source.launch.arguments = vec![
            if provider == crate::agent::Provider::Codex {
                "resume"
            } else {
                "--resume"
            }
            .into(),
            id.into(),
        ];
        source.resume_hint = crate::model::agent_recovery::hint_from_launch(&source.launch);
        let chat = crate::chat::Session {
            id: source.id,
            launch: crate::chat::Launch {
                provider,
                recipe: source.launch.clone(),
                session_id: Some(id.into()),
            },
            proof: None,
            terminal: Some(source.clone()),
        };
        (workspace, chat)
    }

    #[test]
    fn project_channels_round_trip_and_invalid_ownership_is_atomic() {
        let directory = tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("channels.sqlite3"), boot("boot-a")).unwrap();
        let mut workspace = workspace_with_terminal(&boot("boot-a"));
        let card = workspace.cards[0].id;
        let unrelated = workspace_with_terminal(&boot("boot-a")).cards.remove(0);
        let unrelated_id = unrelated.id;
        workspace.cards.push(unrelated);
        let project_id = ProjectId::new();
        let id = ChannelId::new();
        workspace.channels.push(ProjectChannel {
            id,
            project_id,
            title: "Investigate startup".into(),
            cards: vec![card],
            focused: Some(card),
        });
        workspace.active_channel = Some(id);
        workspace.active_worktree = Some("/tmp".into());
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        let manifest = store
            .load_workspace_manifest(workspace.id)
            .unwrap()
            .unwrap();
        assert_eq!(manifest.channels, workspace.channels);
        assert_eq!(manifest.active_channel, Some(id));
        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(restored.channels, workspace.channels);
        assert_eq!(restored.active_channel, Some(id));
        assert_eq!(
            restored
                .cards
                .iter()
                .map(|card| card.id)
                .collect::<Vec<_>>(),
            vec![card, unrelated_id]
        );
        workspace.channels.push(ProjectChannel {
            id: ChannelId::new(),
            project_id,
            title: "Conflicting owner".into(),
            cards: vec![card],
            focused: Some(card),
        });
        workspace.active_channel = None;
        assert!(matches!(
            lane.save(&store, &workspace),
            Err(StoreError::InvalidState(_))
        ));
        let after = store
            .load_workspace_manifest(workspace.id)
            .unwrap()
            .unwrap();
        assert_eq!(after.channels, manifest.channels);
        assert_eq!(after.active_channel, Some(id));
        assert_eq!(after.generation, manifest.generation);
    }

    #[test]
    fn chat_conversion_round_trip_preserves_transcript_and_rejects_stale_card_cas() {
        let directory = tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("chat.sqlite3"), boot("chat-boot")).unwrap();
        let (mut workspace, chat) = agent_workspace(crate::agent::Provider::Omp);
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        let terminal_generation = lane.cards.clone();
        let metadata = TranscriptMetadata {
            path: chat
                .terminal
                .as_ref()
                .unwrap()
                .transcript_path
                .clone()
                .unwrap(),
            byte_len: 128,
            durable_offset: 96,
            updated_at_unix_ms: 1,
        };
        store
            .save_transcript_metadata(chat.id, &metadata, 0)
            .unwrap();
        workspace.cards[0].kind = CardKind::Chat(chat.clone());
        lane.save(&store, &workspace).unwrap();
        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(restored.cards[0].kind, CardKind::Chat(chat.clone()));
        let mut terminal = chat.terminal.clone().unwrap();
        terminal.id = TerminalId::new();
        terminal.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: terminal.launch.clone(),
            last_exit: None,
        };
        workspace.cards[0].kind = CardKind::Terminal(terminal.clone());
        assert!(matches!(
            store.save_workspace(
                &workspace,
                SnapshotBase {
                    workspace: lane.workspace,
                    cards: &terminal_generation,
                }
            ),
            Err(StoreError::StaleGeneration { .. })
        ));
        assert_eq!(
            store
                .load_card(workspace.cards[0].id, None)
                .unwrap()
                .unwrap()
                .kind,
            CardKind::Chat(chat.clone())
        );
        lane.save(&store, &workspace).unwrap();
        assert_eq!(
            store
                .load_card(workspace.cards[0].id, None)
                .unwrap()
                .unwrap()
                .kind,
            CardKind::Terminal(terminal.clone())
        );
        let (_, restored_metadata) = store
            .load_transcript_metadata(terminal.id)
            .unwrap()
            .unwrap();
        assert_eq!(restored_metadata.path, metadata.path);
        assert_eq!(restored_metadata.byte_len, 128);
        assert_eq!(restored_metadata.durable_offset, 96);
        assert!(store.load_transcript_metadata(chat.id).unwrap().is_none());
    }

    #[test]
    fn chat_conversion_rejects_wrong_conversation_and_allows_codex_live_return() {
        let directory = tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("chat.sqlite3"), boot("chat-boot")).unwrap();
        let (mut workspace, chat) = agent_workspace(crate::agent::Provider::Codex);
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        let mut wrong = chat.clone();
        wrong.launch.session_id = Some("88888888-1111-4111-8111-111111111111".into());
        workspace.cards[0].kind = CardKind::Chat(wrong);
        assert!(matches!(
            lane.save(&store, &workspace),
            Err(StoreError::InvalidState(_))
        ));
        workspace.cards[0].kind = CardKind::Chat(chat.clone());
        lane.save(&store, &workspace).unwrap();
        let mut wrong = chat.clone();
        wrong.id = TerminalId::new();
        workspace.cards[0].kind = CardKind::Chat(wrong);
        assert!(matches!(
            lane.save(&store, &workspace),
            Err(StoreError::InvalidState(_))
        ));
        let terminal = chat.terminal.unwrap();
        workspace.cards[0].kind = CardKind::Terminal(terminal.clone());
        lane.save(&store, &workspace).unwrap();
        let proof = terminal.continuity.live_proof().unwrap();
        assert_eq!(
            store
                .load_card(workspace.cards[0].id, Some(&proof))
                .unwrap()
                .unwrap()
                .kind,
            CardKind::Terminal(terminal)
        );
    }

    #[test]
    fn v4_migration_preserves_populated_terminal_and_browser_children() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("legacy.sqlite3");
        let mut connection = Connection::open(&path).unwrap();
        configure_connection(&connection, DEFAULT_BUSY_TIMEOUT).unwrap();
        let transaction = connection.transaction().unwrap();
        migrate_to_v1(&transaction).unwrap();
        migrate_to_v2(&transaction).unwrap();
        migrate_to_v3(&transaction).unwrap();
        let (mut workspace, chat) = agent_workspace(crate::agent::Provider::Omp);
        let mut browser = workspace.cards[0].clone();
        browser.id = CardId::new();
        let url = Url::parse("https://example.com/retained").unwrap();
        browser.kind = CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: url.clone(),
            history: vec![url],
            history_index: 0,
            scroll_x: 12.0,
            scroll_y: 90.0,
            fidelity: BrowserRestoreFidelity::HistoryAndScroll,
        });
        workspace.cards.push(browser.clone());
        let commit = save_workspace_transaction(
            &transaction,
            &workspace,
            SnapshotBase {
                workspace: 0,
                cards: &BTreeMap::new(),
            },
        )
        .unwrap();
        transaction.pragma_update(None, "user_version", 3).unwrap();
        transaction.commit().unwrap();
        drop(connection);
        let store = StateStore::open(&path, boot("chat-boot")).unwrap();
        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(restored.cards[1].kind, browser.kind);
        assert_eq!(
            store
                .load_transcript_metadata(chat.id)
                .unwrap()
                .unwrap()
                .1
                .path,
            chat.terminal
                .as_ref()
                .unwrap()
                .transcript_path
                .clone()
                .unwrap()
        );
        let mut lane = SaveLane {
            workspace: commit.generation,
            cards: commit.card_generations,
        };
        workspace.cards[0].kind = CardKind::Chat(chat.clone());
        lane.save(&store, &workspace).unwrap();
        assert_eq!(
            store
                .load_card(workspace.cards[0].id, None)
                .unwrap()
                .unwrap()
                .kind,
            CardKind::Chat(chat)
        );
    }

    fn stored_card_row(store: &StateStore, card_id: CardId) -> (i64, i64, Option<i64>) {
        let inner = store.lock_inner().unwrap();
        inner
            .connection
            .query_row(
                "SELECT generation, updated_at_unix_ms, content_hash
                 FROM cards WHERE card_id = ?1",
                [card_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }

    fn stored_continuity_json(store: &StateStore, terminal_id: TerminalId) -> String {
        let inner = store.lock_inner().unwrap();
        inner
            .connection
            .query_row(
                "SELECT continuity_json FROM terminal_recipes WHERE terminal_id = ?1",
                [terminal_id.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn store_creates_new_storage_with_private_modes() {
        let root = tempdir().unwrap();
        let directory = root.path().join("new-storage");
        let path = directory.join("state.sqlite3");

        let _store = StateStore::open(&path, boot("boot-new-private")).unwrap();

        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn store_migrates_existing_storage_to_private_modes() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE existing_data (value INTEGER);")
            .unwrap();
        drop(connection);
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let _store = StateStore::open(&path, boot("boot-private")).unwrap();

        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for suffix in ["-wal", "-shm"] {
            let sidecar = sqlite_sidecar_path(&path, suffix);
            if sidecar.exists() {
                assert_eq!(
                    fs::metadata(sidecar).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn store_rejects_database_and_sidecar_symlinks() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("target.sqlite3");
        drop(Connection::open(&target).unwrap());
        let database_link = directory.path().join("database-link.sqlite3");
        symlink(&target, &database_link).unwrap();

        assert!(matches!(
            StateStore::open(&database_link, boot("boot-link")),
            Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied
        ));

        let path = directory.path().join("state.sqlite3");
        drop(Connection::open(&path).unwrap());
        let victim = directory.path().join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        symlink(&victim, sqlite_sidecar_path(&path, "-wal")).unwrap();

        assert!(matches!(
            StateStore::open(&path, boot("boot-sidecar-link")),
            Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied
        ));
        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
    }

    #[test]
    fn store_rejects_symlinked_storage_directory() {
        let directory = tempdir().unwrap();
        let real = directory.path().join("real");
        fs::create_dir(&real).unwrap();
        let linked = directory.path().join("linked");
        symlink(&real, &linked).unwrap();

        assert!(matches!(
            StateStore::open(linked.join("state.sqlite3"), boot("boot-directory-link")),
            Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn migration_is_transactional_and_reaches_current_version() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let mut connection = Connection::open(&path).unwrap();
        configure_connection(&connection, DEFAULT_BUSY_TIMEOUT).unwrap();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        migrate_to_v1(&transaction).unwrap();
        transaction.pragma_update(None, "user_version", 1).unwrap();
        transaction.commit().unwrap();
        drop(connection);

        let store = StateStore::open(&path, boot("boot-a")).unwrap();
        assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
        let inner = store.lock_inner().unwrap();
        let index_count: i64 = inner
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'cards_workspace_order_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1);
        let hash_columns: i64 = inner
            .connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('cards') WHERE name = 'content_hash'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hash_columns, 1);
    }

    #[test]
    fn workspace_round_trip_derives_runtime_observations() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("boot-a");
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        let mut workspace = workspace_with_terminal(&boot_id);
        let browser_card = CardState {
            id: CardId::new(),
            generation: 0,
            title: "browser".into(),
            width_fraction: 0.4,
            existence: Existence::Open,
            exposure: Exposure::Prefetch,
            residency: Residency::Hibernated,
            kind: CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: Url::parse("https://example.com/two").unwrap(),
                history: vec![
                    Url::parse("https://example.com/one").unwrap(),
                    Url::parse("https://example.com/two").unwrap(),
                ],
                history_index: 1,
                scroll_x: 12.0,
                scroll_y: 450.0,
                fidelity: BrowserRestoreFidelity::ExactLive,
            }),
        };
        workspace.active_worktree = Some(PathBuf::from("/tmp/worktree-b"));
        workspace
            .worktree_focus
            .insert(PathBuf::from("/tmp/worktree-b"), workspace.cards[0].id);
        workspace.focused = Some(browser_card.id);
        workspace.cards.push(browser_card);
        workspace.project_order = vec![ProjectId::new(), ProjectId::new()];
        let mut lane = SaveLane::default();
        assert_eq!(lane.save(&store, &workspace).unwrap().generation, 1);

        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(restored.generation, 1);
        assert_eq!(restored.focused, workspace.focused);
        assert_eq!(restored.active_worktree, workspace.active_worktree);
        assert_eq!(restored.worktree_focus, workspace.worktree_focus);
        assert_eq!(restored.project_order, workspace.project_order);
        assert!(
            restored
                .cards
                .iter()
                .all(|card| card.exposure == Exposure::Offscreen
                    && card.residency == Residency::Cold)
        );
        let CardKind::Browser(browser) = &restored.cards[1].kind else {
            panic!("browser card expected");
        };
        assert_eq!(browser.fidelity, BrowserRestoreFidelity::HistoryAndScroll);
        assert_eq!(browser.history_index, 1);
        assert_eq!(browser.scroll_y, 450.0);

        let card_generations = lane.cards.clone();
        workspace.active_worktree = Some(PathBuf::from("/tmp/empty-worktree"));
        workspace
            .worktree_focus
            .insert(PathBuf::from("/tmp/worktree-b"), workspace.cards[1].id);
        workspace.project_order.reverse();
        let commit = lane.save(&store, &workspace).unwrap();
        assert_eq!(commit.card_generations, card_generations);
        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(restored.focused, workspace.focused);
        assert_eq!(restored.active_worktree, workspace.active_worktree);
        assert_eq!(restored.worktree_focus, workspace.worktree_focus);
        assert_eq!(restored.project_order, workspace.project_order);
        assert_eq!(restored.generation, 2);
    }

    #[test]
    fn missing_navigation_metadata_restores_legacy_defaults() {
        let directory = tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("legacy.sqlite3"), boot("boot-a")).unwrap();
        let mut workspace = workspace_with_terminal(&boot("boot-a"));
        workspace.active_worktree = Some(PathBuf::from("/tmp/worktree-b"));
        workspace
            .worktree_focus
            .insert(PathBuf::from("/tmp/worktree-b"), workspace.cards[0].id);
        workspace.project_order = vec![ProjectId::new()];
        let channel_id = ChannelId::new();
        workspace.channels.push(ProjectChannel {
            id: channel_id,
            project_id: workspace.project_order[0],
            title: "Legacy".into(),
            cards: vec![workspace.cards[0].id],
            focused: workspace.focused,
        });
        workspace.active_channel = Some(channel_id);
        SaveLane::default().save(&store, &workspace).unwrap();
        {
            let inner = store.lock_inner().unwrap();
            inner
                .connection
                .execute(
                    "DELETE FROM metadata WHERE key IN (?1, ?2, ?3, ?4, ?5)",
                    params![
                        format!("workspace:{}:navigation:active_worktree", workspace.id),
                        format!("workspace:{}:navigation:worktree_focus", workspace.id),
                        format!("workspace:{}:navigation:project_order", workspace.id),
                        format!("workspace:{}:navigation:{CHANNELS_FIELD}", workspace.id),
                        format!("workspace:{}:navigation:{ACTIVE_CHANNEL_FIELD}", workspace.id),
                    ],
                )
                .unwrap();
        }
        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(restored.active_worktree, None);
        assert!(restored.worktree_focus.is_empty());
        assert!(restored.project_order.is_empty());
        assert!(restored.channels.is_empty());
        assert_eq!(restored.active_channel, None);
        assert_eq!(restored.cards[0].id, workspace.cards[0].id);
    }

    /// Restores the navigation rows a build from before the rename left on
    /// disk.
    ///
    /// The metadata keys and their JSON are written here by hand, exactly as
    /// the earlier build spelled them, so this fails if the durable
    /// representation ever drifts from what existing installations hold. An
    /// installed workspace has to come back with the same channel identity,
    /// chats, focused chat, title, and selection, and stay savable afterwards.
    #[test]
    fn workspace_stored_before_the_channel_rename_restores_every_channel_field() {
        let directory = tempdir().unwrap();
        let store = StateStore::open(
            directory.path().join("legacy-channels.sqlite3"),
            boot("boot-a"),
        )
        .unwrap();
        let mut workspace = workspace_with_terminal(&boot("boot-a"));
        let planner = workspace.cards[0].id;
        let reader = workspace_with_terminal(&boot("boot-a")).cards.remove(0);
        let reader_id = reader.id;
        workspace.cards.push(reader);
        let project_id = ProjectId::new();
        workspace.project_order = vec![project_id];
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();

        let channel_id = ChannelId::new();
        {
            let mut inner = store.lock_inner().unwrap();
            let transaction = inner.connection.transaction().unwrap();
            set_metadata(
                &transaction,
                &format!("workspace:{}:navigation:threads", workspace.id),
                &format!(
                    r#"[{{"id":"{channel_id}","project_id":"{project_id}","title":"Investigate startup","cards":["{planner}","{reader_id}"],"focused":"{reader_id}"}}]"#
                ),
            )
            .unwrap();
            set_metadata(
                &transaction,
                &format!("workspace:{}:navigation:active_thread", workspace.id),
                &format!("\"{channel_id}\""),
            )
            .unwrap();
            transaction.commit().unwrap();
        }

        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(restored.active_channel, Some(channel_id));
        assert_eq!(restored.channels.len(), 1);
        assert_eq!(restored.channels[0].id, channel_id);
        assert_eq!(restored.channels[0].project_id, project_id);
        assert_eq!(restored.channels[0].title, "Investigate startup");
        assert_eq!(restored.channels[0].cards, vec![planner, reader_id]);
        assert_eq!(restored.channels[0].focused, Some(reader_id));

        lane.save(&store, &restored).unwrap();
        let stored = {
            let inner = store.lock_inner().unwrap();
            metadata(
                &inner.connection,
                &format!("workspace:{}:navigation:threads", workspace.id),
            )
            .unwrap()
        };
        assert_eq!(
            stored,
            Some(to_json(&restored.channels).unwrap()),
            "saving must keep writing the durable spelling an older build reads"
        );
        let reloaded = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(reloaded.channels, restored.channels);
        assert_eq!(reloaded.active_channel, Some(channel_id));
    }

    #[test]
    fn corrupt_navigation_metadata_is_not_treated_as_missing() {
        let directory = tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("corrupt.sqlite3"), boot("boot-a")).unwrap();
        let workspace = WorkspaceState::default();
        SaveLane::default().save(&store, &workspace).unwrap();
        for (field, value) in [
            ("active_worktree", "{"),
            ("active_worktree", "42"),
            ("worktree_focus", r#"{"/tmp/worktree-b":"not-a-card-id"}"#),
        ] {
            {
                let mut inner = store.lock_inner().unwrap();
                let transaction = inner.connection.transaction().unwrap();
                set_metadata(
                    &transaction,
                    &format!("workspace:{}:navigation:active_worktree", workspace.id),
                    "null",
                )
                .unwrap();
                set_metadata(
                    &transaction,
                    &format!("workspace:{}:navigation:{field}", workspace.id),
                    value,
                )
                .unwrap();
                transaction.commit().unwrap();
            }
            assert!(matches!(
                store.load_workspace(workspace.id, None),
                Err(StoreError::InvalidState(_))
            ));
        }
    }

    #[test]
    fn dirty_shutdown_marks_pending_operations_for_reconciliation() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("boot-a");
        {
            let store = StateStore::open(&path, boot_id.clone()).unwrap();
            let operation = PendingOperation {
                id: OperationId::new(),
                entity_kind: "card".into(),
                entity_id: CardId::new().to_string(),
                action: "close".into(),
                base_generation: 1,
                payload: serde_json::json!({"reason": "user"}),
                status: PendingOperationStatus::Pending,
                created_at_unix_ms: now_unix_ms(),
                updated_at_unix_ms: now_unix_ms(),
            };
            store.begin_operation(&operation).unwrap();
        }

        let recovered = StateStore::open(&path, boot_id).unwrap();
        assert!(recovered.recovery_report().dirty_shutdown);
        assert_eq!(
            recovered.operations_requiring_reconciliation().unwrap()[0].status,
            PendingOperationStatus::NeedsReconciliation
        );
    }

    #[test]
    fn stale_workspace_generation_is_rejected() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("boot-a");
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        let workspace = workspace_with_terminal(&boot_id);
        let base = BTreeMap::new();
        let stale = SnapshotBase {
            workspace: 0,
            cards: &base,
        };
        store.save_workspace(&workspace, stale).unwrap();
        let error = store.save_workspace(&workspace, stale).unwrap_err();
        assert!(matches!(
            error,
            StoreError::StaleGeneration {
                expected: 0,
                actual: 1,
                ..
            }
        ));
    }

    #[test]
    fn normal_snapshot_cannot_replace_stable_terminal_identity() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("boot-a");
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        let workspace = workspace_with_terminal(&boot_id);
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();

        let mut next = store.load_workspace(workspace.id, None).unwrap().unwrap();
        let CardKind::Terminal(session) = &mut next.cards[0].kind else {
            panic!("expected terminal card");
        };
        session.id = TerminalId::new();
        let error = lane.save(&store, &next).unwrap_err();

        assert!(matches!(error, StoreError::InvalidState(_)));
    }

    #[test]
    fn explicit_terminal_relaunch_atomically_replaces_identity_and_transcript() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("boot-a");
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        let workspace = workspace_with_terminal(&boot_id);
        let card_id = workspace.cards[0].id;
        let CardKind::Terminal(original) = &workspace.cards[0].kind else {
            panic!("expected terminal card");
        };
        let original_id = original.id;
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();

        let replacement_id = TerminalId::new();
        let launch = recipe();
        let replacement = TerminalSession {
            id: replacement_id,
            launch: launch.clone(),
            transcript_path: Some(PathBuf::from("/tmp/relaunched-transcript")),
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        };
        store
            .replace_terminal_identity(card_id, original_id, &replacement)
            .unwrap();

        let loaded = store.load_workspace(workspace.id, None).unwrap().unwrap();
        let CardKind::Terminal(loaded_terminal) = &loaded.cards[0].kind else {
            panic!("expected terminal card");
        };
        assert_eq!(loaded_terminal, &replacement);

        let inner = store.lock_inner().unwrap();
        let old_recipe_count: i64 = inner
            .connection
            .query_row(
                "SELECT COUNT(*) FROM terminal_recipes WHERE terminal_id = ?1",
                [original_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let old_transcript_count: i64 = inner
            .connection
            .query_row(
                "SELECT COUNT(*) FROM transcript_metadata WHERE terminal_id = ?1",
                [original_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_recipe_count, 0);
        assert_eq!(old_transcript_count, 0);
        drop(inner);

        // The relaunch cleared the card's fingerprint, so this snapshot takes
        // the full write path and re-establishes it.
        assert_eq!(stored_card_row(&store, card_id).2, None);
        lane.save(&store, &loaded).unwrap();
        assert!(stored_card_row(&store, card_id).2.is_some());
    }

    #[test]
    fn failed_terminal_identity_replacement_leaves_original_payload_intact() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("boot-a");
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        let workspace = workspace_with_terminal(&boot_id);
        let card_id = workspace.cards[0].id;
        let CardKind::Terminal(original) = &workspace.cards[0].kind else {
            panic!("expected terminal card");
        };
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();

        let launch = recipe();
        let replacement = TerminalSession {
            id: TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        };
        let error = store
            .replace_terminal_identity(card_id, TerminalId::new(), &replacement)
            .unwrap_err();
        assert!(matches!(error, StoreError::InvalidState(_)));

        let loaded = store.load_workspace(workspace.id, None).unwrap().unwrap();
        let CardKind::Terminal(loaded_terminal) = &loaded.cards[0].kind else {
            panic!("expected terminal card");
        };
        assert_eq!(loaded_terminal.id, original.id);
        assert_eq!(
            loaded_terminal.transcript_path,
            Some(PathBuf::from("/tmp/transcript"))
        );
    }

    #[test]
    fn omitted_cards_become_tombstones_and_cannot_resurrect() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("boot-a");
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        let original = workspace_with_terminal(&boot_id);
        let card_id = original.cards[0].id;
        let mut lane = SaveLane::default();
        lane.save(&store, &original).unwrap();

        let mut next = store.load_workspace(original.id, None).unwrap().unwrap();
        next.cards.clear();
        next.focused = None;
        let commit = lane.save(&store, &next).unwrap();
        assert_eq!(commit.card_generations.get(&card_id), Some(&2));
        let tombstoned = store.load_card(card_id, None).unwrap().unwrap();
        assert!(matches!(tombstoned.existence, Existence::Closed { .. }));

        let mut resurrection = store.load_workspace(original.id, None).unwrap().unwrap();
        let mut old_card = original.cards[0].clone();
        old_card.generation = resurrection.generation;
        resurrection.cards.push(old_card);
        let error = lane.save(&store, &resurrection).unwrap_err();
        assert!(matches!(error, StoreError::InvalidState(_)));
    }

    #[test]
    fn reboot_offers_relaunch_and_never_claims_live_resume() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_a = boot("boot-a");
        let workspace = workspace_with_terminal(&boot_a);
        let workspace_id = workspace.id;
        {
            let store = StateStore::open(&path, boot_a.clone()).unwrap();
            SaveLane::default().save(&store, &workspace).unwrap();
            store.shutdown().unwrap();
        }

        let store = StateStore::open(&path, boot("boot-b")).unwrap();
        assert!(store.recovery_report().boot_changed);
        let restored = store.load_workspace(workspace_id, None).unwrap().unwrap();
        let CardKind::Terminal(terminal) = &restored.cards[0].kind else {
            panic!("terminal card expected");
        };
        assert!(matches!(
            terminal.continuity,
            TerminalContinuity::RelaunchAvailable { .. }
        ));
    }

    #[test]
    fn same_boot_live_continuity_requires_complete_matching_host_proof() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("boot-a");
        let workspace = workspace_with_terminal(&boot_id);
        let workspace_id = workspace.id;
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        SaveLane::default().save(&store, &workspace).unwrap();

        let without_proof = store.load_workspace(workspace_id, None).unwrap().unwrap();
        let CardKind::Terminal(terminal) = &without_proof.cards[0].kind else {
            panic!("terminal card expected");
        };
        assert!(matches!(
            terminal.continuity,
            TerminalContinuity::DetachedSameBoot { .. }
        ));

        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("session-a"),
            boot_id,
            host_incarnation: HostIncarnation::new("incarnation-a"),
        };
        let with_proof = store
            .load_workspace(workspace_id, Some(&proof))
            .unwrap()
            .unwrap();
        let CardKind::Terminal(terminal) = &with_proof.cards[0].kind else {
            panic!("terminal card expected");
        };
        assert!(matches!(
            terminal.continuity,
            TerminalContinuity::Live { .. }
        ));
    }

    #[test]
    fn recovery_downgrade_cannot_be_masked_by_an_unchanged_card_fingerprint() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("boot-a");
        let workspace = workspace_with_terminal(&boot_id);
        let workspace_id = workspace.id;
        let card_id = workspace.cards[0].id;
        {
            // Dropped without `shutdown`, so the next open sees a dirty
            // shutdown and downgrades the stored `Live` continuity.
            let store = StateStore::open(&path, boot_id.clone()).unwrap();
            SaveLane::default().save(&store, &workspace).unwrap();
        }

        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        assert!(store.recovery_report().dirty_shutdown);
        assert_eq!(store.recovery_report().terminals_downgraded, 1);
        assert_eq!(
            stored_card_row(&store, card_id).2,
            None,
            "the recovery pass rewrote continuity, so the card fingerprint must \
             no longer claim to describe the stored row"
        );

        // The app reattaches to the same host session, so the restored card
        // carries exactly the `Live` triple the pre-crash snapshot did and
        // fingerprints identically to what storage was last written with.
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("session-a"),
            boot_id: boot_id.clone(),
            host_incarnation: HostIncarnation::new("incarnation-a"),
        };
        let reattached = store
            .load_workspace(workspace_id, Some(&proof))
            .unwrap()
            .unwrap();
        let CardKind::Terminal(terminal) = &reattached.cards[0].kind else {
            panic!("terminal card expected");
        };
        assert!(matches!(
            terminal.continuity,
            TerminalContinuity::Live { .. }
        ));

        let mut lane = SaveLane {
            workspace: reattached.generation,
            cards: reattached
                .cards
                .iter()
                .map(|card| (card.id, card.generation))
                .collect(),
        };
        lane.save(&store, &reattached).unwrap();

        assert_eq!(
            stored_continuity_json(&store, terminal.id),
            to_json(&terminal.continuity).unwrap(),
            "the snapshot must not be skipped over the downgraded continuity \
             the recovery pass left behind"
        );
    }

    #[test]
    fn wal_is_paired_with_normal_synchronous() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let store = StateStore::open(&path, boot("boot-pragmas")).unwrap();
        let inner = store.lock_inner().unwrap();
        let journal_mode: String = inner
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        let synchronous: i64 = inner
            .connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        // 1 == NORMAL. FULL would fsync on every one of the writer lane's
        // quarter-second flushes.
        assert_eq!(synchronous, 1);
    }

    #[test]
    fn unchanged_cards_are_left_alone_while_changed_cards_advance() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("boot-a");
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        let mut workspace = workspace_with_terminal(&boot_id);
        let quiet_card = CardState {
            id: CardId::new(),
            generation: 0,
            title: "browser".into(),
            width_fraction: 0.4,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
            kind: CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: Url::parse("https://example.com/one").unwrap(),
                history: vec![Url::parse("https://example.com/one").unwrap()],
                history_index: 0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                fidelity: BrowserRestoreFidelity::HistoryAndScroll,
            }),
        };
        let quiet_id = quiet_card.id;
        let busy_id = workspace.cards[0].id;
        workspace.cards.push(quiet_card);

        let mut lane = SaveLane::default();
        assert_eq!(lane.save(&store, &workspace).unwrap().generation, 1);
        let quiet_after_first = stored_card_row(&store, quiet_id);
        let busy_after_first = stored_card_row(&store, busy_id);
        assert_eq!(quiet_after_first.0, 1);
        assert!(quiet_after_first.2.is_some());

        // A snapshot that changes nothing must not rewrite a single card row,
        // even though the workspace generation advances.
        let commit = lane.save(&store, &workspace).unwrap();
        assert_eq!(commit.generation, 2);
        assert_eq!(commit.card_generations[&quiet_id], 1);
        assert_eq!(commit.card_generations[&busy_id], 1);
        assert_eq!(stored_card_row(&store, quiet_id), quiet_after_first);
        assert_eq!(stored_card_row(&store, busy_id), busy_after_first);

        // Changing one card advances only that card.
        let mut changed = workspace.clone();
        changed.cards[0].title = "renamed".into();
        let commit = lane.save(&store, &changed).unwrap();
        assert_eq!(commit.card_generations[&busy_id], 2);
        assert_eq!(commit.card_generations[&quiet_id], 1);
        assert_eq!(stored_card_row(&store, quiet_id), quiet_after_first);
        assert_eq!(stored_card_row(&store, busy_id).0, 2);

        // And a change confined to a side-table payload is still detected.
        let mut relaunch_ready = changed.clone();
        let CardKind::Terminal(session) = &mut relaunch_ready.cards[0].kind else {
            panic!("expected terminal card");
        };
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: session.launch.clone(),
            last_exit: None,
        };
        let commit = lane.save(&store, &relaunch_ready).unwrap();
        assert_eq!(commit.card_generations[&busy_id], 3);
        assert_eq!(commit.card_generations[&quiet_id], 1);

        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        let CardKind::Terminal(terminal) = &restored.cards[0].kind else {
            panic!("expected terminal card");
        };
        assert!(matches!(
            terminal.continuity,
            TerminalContinuity::RelaunchAvailable { .. }
        ));
        assert_eq!(restored.cards[0].title, "renamed");
        assert_eq!(restored.cards[0].generation, 3);
        assert_eq!(restored.cards[1].generation, 1);
    }

    #[test]
    fn cards_added_after_the_first_commit_are_accepted() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("boot-a");
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        let mut workspace = workspace_with_terminal(&boot_id);
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();

        // A card born after the workspace has a non-zero generation is new, and
        // its base generation is therefore zero — not the workspace's.
        let latecomer = CardState {
            id: CardId::new(),
            generation: 0,
            title: "repository".into(),
            width_fraction: 0.3,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
            kind: CardKind::Repository(crate::model::RepositoryView {
                project: ProjectId::new(),
                repository: PathBuf::from("/repo"),
                worktree: PathBuf::from("/repo"),
            }),
        };
        let latecomer_id = latecomer.id;
        workspace.cards.push(latecomer);
        let commit = lane.save(&store, &workspace).unwrap();
        assert_eq!(commit.card_generations[&latecomer_id], 1);

        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(restored.cards.len(), 2);
        assert_eq!(restored.cards[1].id, latecomer_id);
    }

    #[test]
    fn an_undecodable_card_costs_only_that_card() {
        let directory = tempdir().unwrap();
        let boot_id = boot("boot-a");
        let store = StateStore::open(directory.path().join("state.sqlite3"), boot_id.clone())
            .expect("store must open");
        let mut workspace = workspace_with_terminal(&boot_id);
        let survivor = workspace.cards[0].id;
        let mut broken = workspace.cards[0].clone();
        broken.id = CardId::new();
        broken.title = "broken".into();
        let broken_terminal = TerminalId::new();
        let CardKind::Terminal(session) = &mut broken.kind else {
            unreachable!("the fixture card is a terminal")
        };
        session.id = broken_terminal;
        session.transcript_path = None;
        let broken_card = broken.id;
        workspace.cards.push(broken);
        workspace.focused = Some(broken_card);
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();

        // Exactly the corruption that used to discard the whole session: the
        // card row survives, its launch recipe does not.
        {
            let mut inner = store.lock_inner().unwrap();
            let transaction = inner.connection.transaction().unwrap();
            transaction
                .execute(
                    "DELETE FROM terminal_recipes WHERE terminal_id = ?1",
                    [broken_terminal.to_string()],
                )
                .unwrap();
            transaction.commit().unwrap();
        }

        let restored = store
            .load_workspace_restored(workspace.id, None)
            .expect("a single bad card must not fail the restore")
            .expect("the workspace still exists");
        assert_eq!(restored.state.cards.len(), 1);
        assert_eq!(restored.state.cards[0].id, survivor);
        assert_eq!(restored.warnings.len(), 1);
        assert!(
            restored.warnings[0].contains("launch recipe")
                && restored.warnings[0].contains(&broken_card.to_string()),
            "the warning must name the card and the reason: {:?}",
            restored.warnings
        );
        assert_eq!(
            restored.state.focused,
            Some(survivor),
            "focus must move off the card that could not be restored"
        );

        // And the session must still be savable afterwards; a lost card that
        // also disabled saving is the failure this replaced.
        let mut recovered = SaveLane {
            workspace: restored.state.generation,
            cards: restored
                .state
                .cards
                .iter()
                .map(|card| (card.id, card.generation))
                .collect(),
        };
        recovered
            .save(&store, &restored.state)
            .expect("a partially restored workspace must still save");
    }

    #[test]
    fn a_clean_shutdown_leaves_no_write_ahead_log() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("boot-a");
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        SaveLane::default()
            .save(&store, &workspace_with_terminal(&boot_id))
            .unwrap();
        let wal = sqlite_sidecar_path(&path, "-wal");
        assert!(
            fs::metadata(&wal).unwrap().len() > 0,
            "the commit must be in the WAL before it is checkpointed"
        );

        store.shutdown().unwrap();

        assert_eq!(
            fs::metadata(&wal)
                .map(|metadata| metadata.len())
                .unwrap_or(0),
            0,
            "a clean shutdown must fold the WAL back into the database"
        );
    }

    fn transcript_directory() -> (tempfile::TempDir, PathBuf) {
        let root = tempdir().unwrap();
        let directory = root.path().join("transcripts");
        fs::create_dir(&directory).unwrap();
        (root, directory)
    }

    #[test]
    fn pruning_reclaims_orphans_and_oversized_files_and_never_follows_a_symlink() {
        let (root, directory) = transcript_directory();
        let live = TerminalId::new().to_string();
        let oversized = TerminalId::new().to_string();
        let orphan = TerminalId::new().to_string();
        let disguised = TerminalId::new().to_string();
        fs::write(directory.join(format!("{live}.log")), b"kept").unwrap();
        fs::write(directory.join(format!("{oversized}.log")), vec![b'x'; 4096]).unwrap();
        fs::write(directory.join(format!("{orphan}.log")), b"gone").unwrap();
        let victim = root.path().join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        symlink(&victim, directory.join(format!("{disguised}.log"))).unwrap();

        let claimed = HashSet::from([live.clone(), oversized.clone()]);
        let report = prune_transcript_directory(
            &directory,
            &claimed,
            TranscriptRetention {
                max_file_bytes: 1024,
                max_total_bytes: 1 << 20,
                remove_orphans: true,
            },
        )
        .unwrap();

        assert_eq!(report.removed_orphaned, 1);
        assert_eq!(report.removed_oversized, 1);
        assert_eq!(report.refused, 1);
        assert_eq!(report.retained, 1);
        assert!(directory.join(format!("{live}.log")).is_file());
        assert!(!directory.join(format!("{orphan}.log")).exists());
        assert!(!directory.join(format!("{oversized}.log")).exists());
        assert_eq!(
            fs::read(&victim).unwrap(),
            b"unchanged",
            "a symlinked entry must never be followed"
        );
        assert!(
            fs::symlink_metadata(directory.join(format!("{disguised}.log"))).is_ok(),
            "a refused entry is left exactly as it was"
        );
    }

    #[test]
    fn pruning_keeps_unclaimed_transcripts_when_ownership_is_unproven() {
        let (_root, directory) = transcript_directory();
        let unclaimed = TerminalId::new().to_string();
        fs::write(
            directory.join(format!("{unclaimed}.log")),
            b"another window's",
        )
        .unwrap();

        let report = prune_transcript_directory(
            &directory,
            &HashSet::new(),
            TranscriptRetention {
                remove_orphans: false,
                ..TranscriptRetention::default()
            },
        )
        .unwrap();

        assert_eq!(report.removed_orphaned, 0);
        assert_eq!(report.retained, 1);
        assert!(directory.join(format!("{unclaimed}.log")).is_file());
    }

    #[test]
    fn pruning_enforces_the_directory_budget_oldest_first() {
        let (_root, directory) = transcript_directory();
        let older = TerminalId::new().to_string();
        let newer = TerminalId::new().to_string();
        for (id, age) in [(&older, 1_000_u64), (&newer, 2_000)] {
            let path = directory.join(format!("{id}.log"));
            fs::write(&path, vec![b'y'; 512]).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(age))
                .unwrap();
        }

        let claimed = HashSet::from([older.clone(), newer.clone()]);
        let report = prune_transcript_directory(
            &directory,
            &claimed,
            TranscriptRetention {
                max_file_bytes: 1024,
                max_total_bytes: 512,
                remove_orphans: true,
            },
        )
        .unwrap();

        assert_eq!(report.removed_for_budget, 1);
        assert_eq!(report.retained, 1);
        assert_eq!(report.retained_bytes, 512);
        assert!(!directory.join(format!("{older}.log")).exists());
        assert!(directory.join(format!("{newer}.log")).is_file());
    }

    #[test]
    fn a_stored_terminal_keeps_its_transcript() {
        let (_root, directory) = transcript_directory();
        let store_directory = tempdir().unwrap();
        let boot_id = boot("boot-a");
        let store = StateStore::open(
            store_directory.path().join("state.sqlite3"),
            boot_id.clone(),
        )
        .unwrap();
        let workspace = workspace_with_terminal(&boot_id);
        let CardKind::Terminal(session) = &workspace.cards[0].kind else {
            unreachable!("the fixture card is a terminal")
        };
        let live = session.id.to_string();
        SaveLane::default().save(&store, &workspace).unwrap();
        let orphan = TerminalId::new().to_string();
        fs::write(directory.join(format!("{live}.log")), b"live").unwrap();
        fs::write(directory.join(format!("{orphan}.log")), b"orphan").unwrap();

        let report = store
            .prune_transcripts(&directory, HashSet::new(), TranscriptRetention::default())
            .unwrap();

        assert_eq!(report.removed_orphaned, 1);
        assert_eq!(report.retained, 1);
        assert!(directory.join(format!("{live}.log")).is_file());
        assert!(!directory.join(format!("{orphan}.log")).exists());
    }
}
