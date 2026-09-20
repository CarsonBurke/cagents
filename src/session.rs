use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use thiserror::Error;

use crate::{
    instance,
    model::{
        BootId, CardId, ProjectId, ProjectState, TerminalId, TerminalSession, WorkspaceId,
        WorkspaceState, WorktreeId, WorktreeState,
    },
    persistence::{
        SnapshotBase, StateStore, StoreError, StoredGenerations, TranscriptPruneReport,
        TranscriptRetention,
    },
};

const PERSIST_DEBOUNCE: Duration = Duration::from_millis(250);
const PERSIST_RETRY_INITIAL: Duration = Duration::from_millis(100);
const PERSIST_RETRY_MAX: Duration = Duration::from_secs(2);
const PERSIST_MAX_AUTOMATIC_ATTEMPTS: u32 = 4;

/// How many times one batch may be rebased onto freshly read generations.
///
/// One. A base that fell behind needs a single fresh read, because the window
/// process is the only writer of its own database and the snapshot it is
/// writing does not change. If the write is rejected a second time the
/// generations are moving underneath the writer, and reading them again would
/// be a spin rather than a recovery, so the second rejection is reported.
///
/// The budget is per batch, not per session, which is what keeps it right now
/// that a shared-model edge can drive a save with no user action behind it. A
/// busier lane produces more batches, each with its own recovery, and the
/// snapshot queue coalesces, so a burst of edges refreshes the pending content
/// of the current batch rather than consuming recoveries.
const PERSIST_REBASES_PER_BATCH: u32 = 1;

/// How long the final flush at shutdown keeps retrying before abandoning what
/// it could not write.
///
/// One attempt can spend a whole busy timeout blocked on another cagents
/// instance's write lock — 5s by default, and `MAX_BUSY_TIMEOUT` allows 10s —
/// so a budget anywhere near that ceiling would buy exactly one attempt, which
/// is the single-attempt loss this bound exists to rule out. 15s outlasts even
/// the largest configurable timeout with room for a retry, and at the default
/// it is worth several. It is also the cap on how long exit can wait: quitting
/// cannot turn into a hang on a database that will never accept the write.
const SHUTDOWN_FLUSH_BUDGET: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveFailureCategory {
    Busy,
    ReadOnly,
    DiskFull,
    PermissionDenied,
    StorageUnavailable,
    Corrupt,
    Conflict,
    Serialization,
    InvalidData,
    Io,
    Database,
}

impl SaveFailureCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Busy => "database busy or locked",
            Self::ReadOnly => "database is read-only",
            Self::DiskFull => "storage is full",
            Self::PermissionDenied => "permission denied",
            Self::StorageUnavailable => "storage path is unavailable",
            Self::Corrupt => "database is corrupt",
            Self::Conflict => "saved state changed unexpectedly",
            Self::Serialization => "state could not be encoded",
            Self::InvalidData => "saved state is invalid",
            Self::Io => "storage I/O failed",
            Self::Database => "database operation failed",
        }
    }

    pub fn guidance(self) -> &'static str {
        match self {
            Self::Busy => "Close other processes using the database, then retry.",
            Self::ReadOnly | Self::PermissionDenied => {
                "Restore write permission for the database and its directory, then retry."
            }
            Self::DiskFull => "Free disk space, then retry.",
            Self::StorageUnavailable => {
                "Restore the database directory or mounted storage, then retry."
            }
            Self::Corrupt => "Back up the database and restart cagents before making more changes.",
            // Restarting is no longer the remedy: the writer re-reads the
            // stored generations and retries the same snapshot itself, so a
            // conflict only reaches the reader when the write was rejected
            // again against generations read moments earlier. Retrying is what
            // the reader can still do, since it re-arms that recovery; beyond
            // that, a second writer on this window's database is the remaining
            // explanation and the failure is worth reporting.
            Self::Conflict => {
                "Retry the save; if this repeats, close anything else using the database and \
                 report the failure category and database path."
            }
            Self::Serialization | Self::InvalidData => {
                "Restart cagents; if this repeats, report the failure category and database path."
            }
            Self::Io | Self::Database => {
                "Check the database directory and system storage, then retry."
            }
        }
    }

    fn selection_rank(self) -> u8 {
        match self {
            Self::DiskFull => 0,
            Self::ReadOnly => 1,
            Self::PermissionDenied => 2,
            Self::StorageUnavailable => 3,
            Self::Corrupt => 4,
            Self::Busy => 5,
            Self::Conflict => 6,
            Self::Serialization => 7,
            Self::InvalidData => 8,
            Self::Io => 9,
            Self::Database => 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSaveHealth {
    Healthy,
    Retrying {
        category: SaveFailureCategory,
        first_failed_at_unix_ms: i64,
        attempts: u32,
    },
    Recovered {
        recovered_at_unix_ms: i64,
    },
    RetryExhausted {
        category: SaveFailureCategory,
        first_failed_at_unix_ms: i64,
        attempts: u32,
    },
    WriterUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSaveHealthEvent {
    pub revision: u64,
    pub health: SessionSaveHealth,
    pub last_successful_flush_at_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
struct PersistenceConfig {
    debounce: Duration,
    retry_initial: Duration,
    retry_max: Duration,
    max_automatic_attempts: u32,
    shutdown_flush_budget: Duration,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            debounce: PERSIST_DEBOUNCE,
            retry_initial: PERSIST_RETRY_INITIAL,
            retry_max: PERSIST_RETRY_MAX,
            max_automatic_attempts: PERSIST_MAX_AUTOMATIC_ATTEMPTS,
            shutdown_flush_budget: SHUTDOWN_FLUSH_BUDGET,
        }
    }
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("cagents data directory is unavailable")]
    DataDirectoryUnavailable,
    #[error("failed to read Linux boot identifier: {0}")]
    BootId(std::io::Error),
    #[error("session storage failed: {0}")]
    Store(#[from] StoreError),
    #[error("session persistence worker could not start: {0}")]
    Worker(std::io::Error),
}

#[derive(Debug, Clone)]
struct TerminalIdentityReplacement {
    card_id: CardId,
    expected_terminal_id: TerminalId,
    replacement: TerminalSession,
}

#[derive(Debug)]
enum ConversionCheckpoint {
    Snapshot(Arc<WorkspaceState>),
    TerminalReplacement(TerminalIdentityReplacement),
}

fn merge_terminal_replacement(
    replacements: &mut HashMap<CardId, TerminalIdentityReplacement>,
    incoming: TerminalIdentityReplacement,
) {
    match replacements.entry(incoming.card_id) {
        std::collections::hash_map::Entry::Occupied(mut entry)
            if entry.get().replacement.id == incoming.expected_terminal_id =>
        {
            entry.get_mut().replacement = incoming.replacement;
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            entry.insert(incoming);
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(incoming);
        }
    }
}

#[derive(Debug, Default)]
struct QueueState {
    /// Snapshots travel as a shared pointer: the UI thread hands over a
    /// finished `WorkspaceState` and the writer thread reads it in place, so a
    /// change costs one `Arc` bump on this lane rather than a second deep clone
    /// of every card, title, and browser history.
    latest: Option<Arc<WorkspaceState>>,
    conversions: VecDeque<ConversionCheckpoint>,
    ordered_identities: bool,
    projects: HashMap<ProjectId, ProjectState>,
    worktrees: HashMap<WorktreeId, WorktreeState>,
    terminal_replacements: HashMap<CardId, TerminalIdentityReplacement>,
    retry_requested: bool,
    checkpoints: Vec<std::sync::mpsc::SyncSender<std::result::Result<(), String>>>,
    shutdown: bool,
}

pub struct SessionRuntime {
    queue: Arc<(Mutex<QueueState>, Condvar)>,
    last_error: Arc<Mutex<Option<String>>>,
    health: async_channel::Receiver<SessionSaveHealthEvent>,
    worker: Option<thread::JoinHandle<()>>,
    database_path: PathBuf,
    restored_projects: Vec<ProjectState>,
    restored_worktrees: Vec<WorktreeState>,
    /// One line per card that storage held but could not restore. Losing a
    /// card is a reportable event, not a silent deletion, so the shell can
    /// surface these.
    restore_warnings: Vec<String>,
    operation_store: Arc<StateStore>,
}

impl std::fmt::Debug for SessionRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionRuntime")
            .field("worker_running", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

impl SessionRuntime {
    pub fn default_database_path() -> Result<PathBuf, SessionError> {
        instance::window_database_path(instance::window_slot())
            .map_err(|error| SessionError::Store(StoreError::Io(error)))
    }

    /// Initializes a named window from a consistent SQLite snapshot, never a
    /// byte copy of a live WAL database. IDs and exact migration links survive.
    pub fn initialize_window(source: &str) -> Result<(), SessionError> {
        let destination = Self::default_database_path()?;
        let source = instance::window_database_path(source)
            .map_err(|error| SessionError::Store(StoreError::Io(error)))?;
        if destination == source || destination.exists() || !source.exists() {
            return Ok(());
        }
        StateStore::copy_snapshot(&source, &destination)?;
        Ok(())
    }
    pub fn open_default() -> Result<(Self, Option<WorkspaceState>), SessionError> {
        Self::open(Self::default_database_path()?, current_boot_id()?)
    }

    pub fn open(
        database_path: impl AsRef<Path>,
        boot_id: BootId,
    ) -> Result<(Self, Option<WorkspaceState>), SessionError> {
        let database_path = database_path.as_ref().to_path_buf();
        let store = Arc::new(StateStore::open(&database_path, boot_id)?);
        // A card storage cannot decode is dropped with a warning rather than
        // failing the open: the writer thread below must exist even then, or
        // one unreadable card would disable saving for the whole run.
        let restored = store
            .list_workspaces()?
            .into_iter()
            .find(|summary| summary.existence.is_open())
            .map(|summary| store.load_workspace_restored(summary.id, None))
            .transpose()?
            .flatten();
        let (restored, restore_warnings) = match restored {
            Some(restored) => (Some(restored.state), restored.warnings),
            None => (None, Vec::new()),
        };
        let projects = store.list_projects()?;
        let worktrees = store.list_worktrees()?;
        let writes = PendingWrites::restored(
            restored.as_ref().map_or(0, |state| state.generation),
            restored
                .as_ref()
                .map(|state| {
                    state
                        .cards
                        .iter()
                        .map(|card| (card.id, card.generation))
                        .collect()
                })
                .unwrap_or_default(),
            projects
                .iter()
                .map(|project| (project.id, project.generation))
                .collect(),
            worktrees
                .iter()
                .map(|worktree| (worktree.id, worktree.generation))
                .collect(),
        );

        let queue = Arc::new((Mutex::new(QueueState::default()), Condvar::new()));
        let last_error = Arc::new(Mutex::new(None));
        let (health_tx, health_rx) = async_channel::unbounded();
        let health = HealthPublisher::new(health_tx);
        let worker_queue = Arc::clone(&queue);
        let worker_error = Arc::clone(&last_error);
        let operation_store = Arc::clone(&store);
        let worker = thread::Builder::new()
            .name("cagents-session-writer".into())
            .spawn(move || {
                run_persistence_worker(
                    store,
                    writes,
                    worker_queue,
                    worker_error,
                    health,
                    PersistenceConfig::default(),
                );
            })
            .map_err(SessionError::Worker)?;

        Ok((
            Self {
                queue,
                last_error,
                health: health_rx,
                worker: Some(worker),
                database_path,
                restored_projects: projects,
                restored_worktrees: worktrees,
                restore_warnings,
                operation_store,
            },
            restored,
        ))
    }

    /// Replaces any uncommitted snapshot. This keeps mutations non-blocking and
    /// bounds memory while preserving the newest complete state.
    ///
    /// The snapshot is shared, not copied: the writer thread reads it through
    /// the `Arc` and never mutates it, which is why the storage generations it
    /// writes against live in the writer thread rather than in the snapshot.
    /// Navigation-only changes are complete snapshots too: card fingerprints
    /// may skip card writes, but must never skip the workspace metadata update.
    pub fn snapshot(&self, state: Arc<WorkspaceState>) {
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            queue.latest = Some(state);
            changed.notify_one();
        }
    }

    /// Preserve a kind transition and all preceding metadata on the writer
    /// lane. Unlike autosave, conversions cannot coalesce: a fast return to a
    /// terminal still needs the exact chat identity committed before it.
    pub fn snapshot_conversion(&self, state: Arc<WorkspaceState>) {
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            queue.ordered_identities = true;
            for replacement in std::mem::take(&mut queue.terminal_replacements).into_values() {
                queue
                    .conversions
                    .push_back(ConversionCheckpoint::TerminalReplacement(replacement));
            }
            if let Some(previous) = queue.latest.take() {
                queue
                    .conversions
                    .push_back(ConversionCheckpoint::Snapshot(previous));
            }
            queue
                .conversions
                .push_back(ConversionCheckpoint::Snapshot(state));
            changed.notify_one();
        }
    }

    pub fn save_project(&self, project: ProjectState) {
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            queue.projects.insert(project.id, project);
            changed.notify_one();
        }
    }

    pub fn save_worktrees(&self, worktrees: impl IntoIterator<Item = WorktreeState>) {
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            for worktree in worktrees {
                queue.worktrees.insert(worktree.id, worktree);
            }
            changed.notify_one();
        }
    }

    pub fn replace_terminal_identity(
        &self,
        card_id: CardId,
        expected_terminal_id: TerminalId,
        replacement: TerminalSession,
    ) {
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            let replacement = TerminalIdentityReplacement {
                card_id,
                expected_terminal_id,
                replacement,
            };
            if queue.ordered_identities {
                if let Some(previous) = queue.latest.take() {
                    queue
                        .conversions
                        .push_back(ConversionCheckpoint::Snapshot(previous));
                }
                queue
                    .conversions
                    .push_back(ConversionCheckpoint::TerminalReplacement(replacement));
            } else {
                merge_terminal_replacement(&mut queue.terminal_replacements, replacement);
            }
            changed.notify_one();
        }
    }

    pub fn take_catalog(&mut self) -> (Vec<ProjectState>, Vec<WorktreeState>) {
        (
            std::mem::take(&mut self.restored_projects),
            std::mem::take(&mut self.restored_worktrees),
        )
    }

    /// Cards storage held but could not restore, one human-readable line each.
    ///
    /// Empty for a clean restore. A non-empty list means the restored
    /// workspace is missing those cards and the user should be told: the
    /// alternative — the behaviour this replaced — was losing the entire
    /// session with no diagnostic anywhere.
    pub fn restore_warnings(&self) -> &[String] {
        &self.restore_warnings
    }

    /// Reclaims terminal transcript files nothing owns any more.
    ///
    /// Session lifecycle owns this because only the session databases know
    /// which terminal ids are still live. Call it once, after opening the
    /// session; it does a single directory scan and no database writes.
    ///
    /// Transcripts are shared by every window while each window keeps its own
    /// database, so the live set is the union over all of them. If any window
    /// database cannot be read, unclaimed transcripts are left alone rather
    /// than risk deleting a transcript another window is still writing.
    pub fn prune_transcripts(&self) -> Result<TranscriptPruneReport, SessionError> {
        let directories = instance::project_dirs().ok_or(SessionError::DataDirectoryUnavailable)?;
        let transcripts = directories.data_dir().join("transcripts");
        let mut also_live = std::collections::HashSet::new();
        let mut complete = true;
        match instance::window_database_paths() {
            Ok(paths) => {
                for path in paths {
                    if path == self.database_path {
                        continue;
                    }
                    match crate::persistence::read_live_terminal_ids(&path) {
                        Ok(ids) => also_live.extend(ids),
                        Err(error) => {
                            tracing::warn!(
                                database = %path.display(),
                                %error,
                                "could not read another window's terminals; \
                                 keeping every unclaimed transcript"
                            );
                            complete = false;
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not enumerate window databases; keeping every unclaimed transcript"
                );
                complete = false;
            }
        }
        let retention = TranscriptRetention {
            remove_orphans: complete,
            ..TranscriptRetention::default()
        };
        let report = self
            .operation_store
            .prune_transcripts(&transcripts, also_live, retention)?;
        if report.removed() > 0 || report.refused > 0 {
            tracing::info!(
                orphaned = report.removed_orphaned,
                oversized = report.removed_oversized,
                over_budget = report.removed_for_budget,
                refused = report.refused,
                retained = report.retained,
                retained_bytes = report.retained_bytes,
                "pruned terminal transcripts"
            );
        }
        Ok(report)
    }

    /// Shares the existing connection, never opens a second startup-reconciling store.
    pub fn operation_store(&self) -> Arc<StateStore> {
        Arc::clone(&self.operation_store)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|error| error.clone())
    }

    pub fn health_receiver(&self) -> async_channel::Receiver<SessionSaveHealthEvent> {
        self.health.clone()
    }

    /// Requests one new bounded retry cycle without waiting for storage.
    pub fn retry_save(&self) -> bool {
        let (lock, changed) = &*self.queue;
        let Ok(mut queue) = lock.lock() else {
            return false;
        };
        if queue.shutdown {
            return false;
        }
        queue.retry_requested = true;
        changed.notify_one();
        true
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// A writer-lane barrier. Failure leaves pending writes retryable and never
    /// tears down the session, so an update veto can safely resume this window.
    pub fn request_flush(
        &self,
    ) -> std::result::Result<std::sync::mpsc::Receiver<std::result::Result<(), String>>, String>
    {
        let (response, receiver) = std::sync::mpsc::sync_channel(1);
        let (lock, changed) = &*self.queue;
        let mut queue = lock
            .lock()
            .map_err(|_| "session writer queue unavailable")?;
        if queue.shutdown {
            return Err("session writer is shutting down".into());
        }
        queue.checkpoints.push(response);
        changed.notify_one();
        drop(queue);
        Ok(receiver)
    }

    pub fn flush(&self, timeout: Duration) -> std::result::Result<(), String> {
        self.request_flush()?
            .recv_timeout(timeout)
            .map_err(|error| format!("session flush: {error}"))?
    }
    pub fn shutdown(&mut self) {
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock() {
            queue.shutdown = true;
            changed.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Everything the writer lane still owes storage, plus the storage generations
/// the next attempt has to write against.
///
/// Keeping them in one place is what lets the shutdown path retry the very same
/// flush the steady-state path runs, rather than a one-shot imitation of it.
#[derive(Debug)]
struct PendingWrites {
    workspace: Option<Arc<WorkspaceState>>,
    conversions: VecDeque<ConversionCheckpoint>,
    projects: HashMap<ProjectId, ProjectState>,
    worktrees: HashMap<WorktreeId, WorktreeState>,
    terminal_replacements: HashMap<CardId, TerminalIdentityReplacement>,
    workspace_generation: u64,
    card_generations: BTreeMap<CardId, u64>,
    project_generations: HashMap<ProjectId, u64>,
    worktree_generations: HashMap<WorktreeId, u64>,
}

trait SessionStore {
    fn replace_terminal_identity(
        &self,
        card_id: CardId,
        expected_terminal_id: TerminalId,
        replacement: &TerminalSession,
    ) -> Result<(), StoreError>;
    fn save_workspace(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError>;
    fn save_project(&self, project: &ProjectState) -> Result<u64, StoreError>;
    fn save_worktree(&self, worktree: &WorktreeState) -> Result<u64, StoreError>;
    /// Reads the generations storage currently holds for a workspace, so a
    /// write rejected against a stale base can be retried against the live
    /// one.
    fn stored_generations(&self, workspace: WorkspaceId) -> Result<StoredGenerations, StoreError>;
    fn stored_project_generation(&self, project: ProjectId) -> Result<Option<u64>, StoreError>;
    fn stored_worktree_generation(&self, worktree: WorktreeId) -> Result<Option<u64>, StoreError>;
    fn shutdown(&self) -> Result<(), StoreError>;
    /// Housekeeping the writer lane runs after a flush lands. Storage with no
    /// write-ahead log of its own has nothing to do here.
    fn checkpoint_wal_if_large(&self) {}
}

impl SessionStore for StateStore {
    fn replace_terminal_identity(
        &self,
        card_id: CardId,
        expected_terminal_id: TerminalId,
        replacement: &TerminalSession,
    ) -> Result<(), StoreError> {
        Self::replace_terminal_identity(self, card_id, expected_terminal_id, replacement)
    }

    fn save_workspace(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        Self::save_workspace(self, state, base)
    }

    fn save_project(&self, project: &ProjectState) -> Result<u64, StoreError> {
        Self::save_project(self, project)
    }

    fn save_worktree(&self, worktree: &WorktreeState) -> Result<u64, StoreError> {
        Self::save_worktree(self, worktree)
    }

    fn stored_generations(&self, workspace: WorkspaceId) -> Result<StoredGenerations, StoreError> {
        Self::stored_generations(self, workspace)
    }

    fn stored_project_generation(&self, project: ProjectId) -> Result<Option<u64>, StoreError> {
        Self::stored_project_generation(self, project)
    }

    fn stored_worktree_generation(&self, worktree: WorktreeId) -> Result<Option<u64>, StoreError> {
        Self::stored_worktree_generation(self, worktree)
    }

    fn shutdown(&self) -> Result<(), StoreError> {
        Self::shutdown(self)
    }

    fn checkpoint_wal_if_large(&self) {
        report_checkpoint(Self::checkpoint_wal_if_large(self));
    }
}

impl SessionStore for Arc<StateStore> {
    fn replace_terminal_identity(
        &self,
        card_id: CardId,
        expected_terminal_id: TerminalId,
        replacement: &TerminalSession,
    ) -> Result<(), StoreError> {
        StateStore::replace_terminal_identity(self, card_id, expected_terminal_id, replacement)
    }
    fn save_workspace(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        StateStore::save_workspace(self, state, base)
    }
    fn save_project(&self, project: &ProjectState) -> Result<u64, StoreError> {
        StateStore::save_project(self, project)
    }
    fn save_worktree(&self, worktree: &WorktreeState) -> Result<u64, StoreError> {
        StateStore::save_worktree(self, worktree)
    }
    fn stored_generations(&self, workspace: WorkspaceId) -> Result<StoredGenerations, StoreError> {
        StateStore::stored_generations(self, workspace)
    }
    fn stored_project_generation(&self, project: ProjectId) -> Result<Option<u64>, StoreError> {
        StateStore::stored_project_generation(self, project)
    }
    fn stored_worktree_generation(&self, worktree: WorktreeId) -> Result<Option<u64>, StoreError> {
        StateStore::stored_worktree_generation(self, worktree)
    }
    fn shutdown(&self) -> Result<(), StoreError> {
        StateStore::shutdown(self)
    }
    fn checkpoint_wal_if_large(&self) {
        report_checkpoint(StateStore::checkpoint_wal_if_large(self));
    }
}

/// A checkpoint is housekeeping, never a save: a failure is worth a log line
/// and nothing else.
fn report_checkpoint(outcome: Result<bool, StoreError>) {
    if let Err(error) = outcome {
        tracing::warn!(%error, "session WAL checkpoint could not run");
    }
}

impl PendingWrites {
    /// A lane that owes storage nothing yet and writes against the generations
    /// the session was restored at.
    fn restored(
        workspace_generation: u64,
        card_generations: BTreeMap<CardId, u64>,
        project_generations: HashMap<ProjectId, u64>,
        worktree_generations: HashMap<WorktreeId, u64>,
    ) -> Self {
        Self {
            workspace: None,
            conversions: VecDeque::new(),
            projects: HashMap::new(),
            worktrees: HashMap::new(),
            terminal_replacements: HashMap::new(),
            workspace_generation,
            card_generations,
            project_generations,
            worktree_generations,
        }
    }

    fn is_empty(&self) -> bool {
        self.workspace.is_none()
            && self.conversions.is_empty()
            && self.projects.is_empty()
            && self.worktrees.is_empty()
            && self.terminal_replacements.is_empty()
    }

    /// The workspace every pending snapshot in this batch belongs to.
    ///
    /// One window process owns one window, so the conversion queue and the
    /// coalesced autosave are snapshots of the same workspace; the first one
    /// present names it.
    fn pending_workspace(&self) -> Option<WorkspaceId> {
        self.conversions
            .iter()
            .find_map(|checkpoint| match checkpoint {
                ConversionCheckpoint::Snapshot(snapshot) => Some(snapshot.id),
                ConversionCheckpoint::TerminalReplacement(_) => None,
            })
            .or_else(|| self.workspace.as_ref().map(|snapshot| snapshot.id))
    }

    /// Reseats the bases the next attempt writes against on the generations
    /// storage holds now, leaving every pending snapshot exactly as it is.
    ///
    /// One window process owns one window's database: `WindowLease` admits a
    /// single process per slot and every slot has its own file. A base storage
    /// rejects is therefore never a rival writer's edit - it means a commit of
    /// this lane's own landed while the lane believed it had not, or a batch
    /// applied in part. The in-memory snapshot is the authority for what the
    /// window contains and storage is only the authority for the generations
    /// that content is written against, which is what makes moving the bases
    /// under an unchanged snapshot a recovery rather than a guess.
    ///
    /// The shared workspace model does not weaken that. It lives in the
    /// daemon's own registry database, and an edge from it reaches this window
    /// as an event the window folds into its own state before persisting, so a
    /// snapshot the daemon caused is still this window's content and no other
    /// process writes these rows. Nor does it add a base: the channel list and
    /// the model identity are `metadata` rows inside the workspace commit,
    /// guarded by the workspace generation this rebase re-reads.
    ///
    /// Rebasing cannot lose a card. `save_workspace` decides which card rows to
    /// write from the snapshot's card set alone, and tombstones the stored rows
    /// that set does not contain using the generation on the stored row rather
    /// than the base, so the base changes no write decision - only the
    /// generation each decided write is checked against. A card whose stored
    /// generation moved is therefore rewritten from the snapshot at the new
    /// generation, and a card storage no longer holds is written as new. That
    /// last part is why the base must name every stored row, tombstones
    /// included: a stored card the base does not know is checked as new and
    /// rejected. `stored_generations` returns exactly that set.
    ///
    /// The read and the retry are separate transactions, which is safe for the
    /// same reason the rebase is correct rather than clever: the retry is still
    /// a compare-and-set against the generations it was given, so anything that
    /// moved in between is rejected again instead of overwritten.
    fn rebase(&mut self, store: &impl SessionStore) -> Result<(), StoreError> {
        if let Some(workspace) = self.pending_workspace() {
            let stored = store.stored_generations(workspace)?;
            self.workspace_generation = stored.workspace;
            self.card_generations = stored.cards;
        }
        // A catalog entity carries its base inside the state it writes, so
        // reseating it means replacing the remembered generation. No stored row
        // means storage has never seen the entity, and `flush` writes an entity
        // with no remembered generation as a new one.
        for id in self.projects.keys().copied() {
            let stored = store.stored_project_generation(id)?;
            match stored {
                Some(generation) => {
                    self.project_generations.insert(id, generation);
                }
                None => {
                    self.project_generations.remove(&id);
                }
            }
        }
        for id in self.worktrees.keys().copied() {
            let stored = store.stored_worktree_generation(id)?;
            match stored {
                Some(generation) => {
                    self.worktree_generations.insert(id, generation);
                }
                None => {
                    self.worktree_generations.remove(&id);
                }
            }
        }
        Ok(())
    }

    /// Applies everything still pending, leaving whatever failed queued for a
    /// later attempt. Returns one message per failed write.
    fn flush(&mut self, store: &impl SessionStore) -> Vec<StoreError> {
        let mut errors = Vec::new();
        let replacement_cards = self
            .terminal_replacements
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for card_id in replacement_cards {
            let replacement = &self.terminal_replacements[&card_id];
            match store.replace_terminal_identity(
                replacement.card_id,
                replacement.expected_terminal_id,
                &replacement.replacement,
            ) {
                Ok(()) => {
                    self.terminal_replacements.remove(&card_id);
                }
                Err(error) => errors.push(error),
            }
        }

        // Conversion checkpoints precede later autosaves, and failed writes
        // stay at the front so neither a retry nor shutdown can reorder them.
        if self.terminal_replacements.is_empty() {
            loop {
                let snapshot = match self.conversions.front() {
                    Some(ConversionCheckpoint::Snapshot(snapshot)) => snapshot,
                    Some(ConversionCheckpoint::TerminalReplacement(replacement)) => {
                        match store.replace_terminal_identity(
                            replacement.card_id,
                            replacement.expected_terminal_id,
                            &replacement.replacement,
                        ) {
                            Ok(()) => {
                                self.conversions.pop_front();
                            }
                            Err(error) => {
                                errors.push(error);
                                break;
                            }
                        }
                        continue;
                    }
                    None => match self.workspace.as_ref() {
                        Some(snapshot) => snapshot,
                        None => break,
                    },
                };
                match store.save_workspace(
                    snapshot,
                    SnapshotBase {
                        workspace: self.workspace_generation,
                        cards: &self.card_generations,
                    },
                ) {
                    Ok(commit) => {
                        self.workspace_generation = commit.generation;
                        self.card_generations = commit.card_generations;
                        if self.conversions.pop_front().is_none() {
                            self.workspace = None;
                        }
                    }
                    Err(error) => {
                        errors.push(error);
                        break;
                    }
                }
            }
        }

        let project_ids = self.projects.keys().copied().collect::<Vec<_>>();
        for id in project_ids {
            let mut project = self.projects[&id].clone();
            project.generation = self.project_generations.get(&id).copied().unwrap_or(0);
            match store.save_project(&project) {
                Ok(generation) => {
                    self.project_generations.insert(id, generation);
                    self.projects.remove(&id);
                }
                Err(error) => errors.push(error),
            }
        }

        let worktree_ids = self.worktrees.keys().copied().collect::<Vec<_>>();
        for id in worktree_ids {
            let mut worktree = self.worktrees[&id].clone();
            worktree.generation = self.worktree_generations.get(&id).copied().unwrap_or(0);
            match store.save_worktree(&worktree) {
                Ok(generation) => {
                    self.worktree_generations.insert(id, generation);
                    self.worktrees.remove(&id);
                }
                Err(error) => errors.push(error),
            }
        }

        errors
    }
}

#[derive(Debug)]
struct FailureIncident {
    category: SaveFailureCategory,
    first_failed_at_unix_ms: i64,
    attempts: u32,
}

struct HealthPublisher {
    sender: async_channel::Sender<SessionSaveHealthEvent>,
    revision: u64,
    last_successful_flush_at_unix_ms: Option<i64>,
    incident: Option<FailureIncident>,
}

impl HealthPublisher {
    fn new(sender: async_channel::Sender<SessionSaveHealthEvent>) -> Self {
        let mut publisher = Self {
            sender,
            revision: 0,
            last_successful_flush_at_unix_ms: None,
            incident: None,
        };
        publisher.publish(SessionSaveHealth::Healthy);
        publisher
    }

    fn publish(&mut self, health: SessionSaveHealth) {
        self.revision = self.revision.saturating_add(1);
        let _ = self.sender.try_send(SessionSaveHealthEvent {
            revision: self.revision,
            health,
            last_successful_flush_at_unix_ms: self.last_successful_flush_at_unix_ms,
        });
    }

    fn failed(&mut self, errors: &[StoreError], exhausted: bool) {
        let category = select_failure_category(errors);
        let incident = self.incident.get_or_insert_with(|| FailureIncident {
            category,
            first_failed_at_unix_ms: unix_millis(),
            attempts: 0,
        });
        incident.category = category;
        incident.attempts = incident.attempts.saturating_add(1);
        let health = if exhausted {
            SessionSaveHealth::RetryExhausted {
                category: incident.category,
                first_failed_at_unix_ms: incident.first_failed_at_unix_ms,
                attempts: incident.attempts,
            }
        } else {
            SessionSaveHealth::Retrying {
                category: incident.category,
                first_failed_at_unix_ms: incident.first_failed_at_unix_ms,
                attempts: incident.attempts,
            }
        };
        self.publish(health);
    }

    fn flushed(&mut self) {
        let at = unix_millis();
        self.last_successful_flush_at_unix_ms = Some(at);
        if self.incident.take().is_some() {
            self.publish(SessionSaveHealth::Recovered {
                recovered_at_unix_ms: at,
            });
        }
    }
}

fn save_failure_category(error: &StoreError) -> SaveFailureCategory {
    use rusqlite::ErrorCode;

    match error {
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _)) => match error.code {
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => SaveFailureCategory::Busy,
            ErrorCode::ReadOnly => SaveFailureCategory::ReadOnly,
            ErrorCode::DiskFull => SaveFailureCategory::DiskFull,
            ErrorCode::PermissionDenied | ErrorCode::AuthorizationForStatementDenied => {
                SaveFailureCategory::PermissionDenied
            }
            ErrorCode::CannotOpen | ErrorCode::NotFound => SaveFailureCategory::StorageUnavailable,
            ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => SaveFailureCategory::Corrupt,
            ErrorCode::SystemIoFailure => SaveFailureCategory::Io,
            _ => SaveFailureCategory::Database,
        },
        StoreError::Sqlite(_) => SaveFailureCategory::Database,
        StoreError::Serialization(_) => SaveFailureCategory::Serialization,
        StoreError::Io(error) if error.raw_os_error() == Some(libc::ENOSPC) => {
            SaveFailureCategory::DiskFull
        }
        StoreError::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            SaveFailureCategory::PermissionDenied
        }
        StoreError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            SaveFailureCategory::StorageUnavailable
        }
        StoreError::Io(_) => SaveFailureCategory::Io,
        StoreError::StaleGeneration { .. } => SaveFailureCategory::Conflict,
        StoreError::UnsupportedSchema { .. } | StoreError::InvalidState(_) => {
            SaveFailureCategory::InvalidData
        }
        StoreError::InvalidUuid { .. }
        | StoreError::InvalidBusyTimeout { .. }
        | StoreError::MutexPoisoned
        | StoreError::Closed => SaveFailureCategory::Database,
    }
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriterExit {
    Shutdown,
    Unavailable,
}

fn run_persistence_worker(
    store: impl SessionStore,
    writes: PendingWrites,
    queue: Arc<(Mutex<QueueState>, Condvar)>,
    last_error: Arc<Mutex<Option<String>>>,
    mut health: HealthPublisher,
    config: PersistenceConfig,
) {
    let exit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        persistence_loop(store, writes, queue, last_error, &mut health, config)
    }));
    if !matches!(exit, Ok(WriterExit::Shutdown)) {
        health.publish(SessionSaveHealth::WriterUnavailable);
    }
}

fn persistence_loop(
    store: impl SessionStore,
    mut writes: PendingWrites,
    queue: Arc<(Mutex<QueueState>, Condvar)>,
    last_error: Arc<Mutex<Option<String>>>,
    health: &mut HealthPublisher,
    config: PersistenceConfig,
) -> WriterExit {
    let mut due: Option<Instant> = None;
    let mut retry_delay = config.retry_initial;
    let mut attempts_this_cycle = 0_u32;
    let mut retry_exhausted = false;
    let mut rebases_remaining = PERSIST_REBASES_PER_BATCH;

    loop {
        let (lock, changed) = &*queue;
        let mut state = match lock.lock() {
            Ok(state) => state,
            Err(_) => return WriterExit::Unavailable,
        };
        while state.latest.is_none()
            && state.conversions.is_empty()
            && state.projects.is_empty()
            && state.worktrees.is_empty()
            && state.terminal_replacements.is_empty()
            && !state.retry_requested
            && state.checkpoints.is_empty()
            && !state.shutdown
        {
            state = if let Some(deadline) = due {
                let timeout = deadline.saturating_duration_since(Instant::now());
                match changed.wait_timeout(state, timeout) {
                    Ok((next, result)) => {
                        state = next;
                        if result.timed_out() {
                            break;
                        }
                        continue;
                    }
                    Err(_) => return WriterExit::Unavailable,
                }
            } else {
                match changed.wait(state) {
                    Ok(next) => next,
                    Err(_) => return WriterExit::Unavailable,
                }
            };
        }

        let received_changes = state.latest.is_some()
            || !state.conversions.is_empty()
            || !state.projects.is_empty()
            || !state.worktrees.is_empty()
            || !state.terminal_replacements.is_empty();
        if !state.conversions.is_empty() {
            if let Some(previous) = writes.workspace.take() {
                writes
                    .conversions
                    .push_back(ConversionCheckpoint::Snapshot(previous));
            }
            writes.conversions.append(&mut state.conversions);
        }
        if let Some(latest) = state.latest.take() {
            writes.workspace = Some(latest);
        }
        writes.projects.extend(std::mem::take(&mut state.projects));
        writes
            .worktrees
            .extend(std::mem::take(&mut state.worktrees));
        for replacement in std::mem::take(&mut state.terminal_replacements).into_values() {
            merge_terminal_replacement(&mut writes.terminal_replacements, replacement);
        }
        let retry_requested = std::mem::take(&mut state.retry_requested);
        if retry_requested && !writes.is_empty() {
            retry_exhausted = false;
            attempts_this_cycle = 0;
            // An explicit retry is the reader re-arming the lane, so it also
            // restores the recovery the failed cycle may have spent.
            rebases_remaining = PERSIST_REBASES_PER_BATCH;
            retry_delay = config.retry_initial;
            due = Some(Instant::now());
        } else {
            due = schedule_flush_after_changes(
                due,
                received_changes,
                retry_exhausted,
                attempts_this_cycle,
                Instant::now(),
                config.debounce,
            );
        }
        let shutdown = state.shutdown;
        let checkpoints = std::mem::take(&mut state.checkpoints);
        let should_persist = shutdown
            || !checkpoints.is_empty()
            || due.is_some_and(|deadline| Instant::now() >= deadline);
        drop(state);

        // A refusal has to name a cause, and the raw store error may not leave
        // this thread: it retains the connection string a failing SQLite call
        // reports, secrets included, which is why the health lane publishes a
        // category instead. The category is the same redacted classification,
        // so it travels to a dialog and to the log safely.
        let mut failure = None;
        if should_persist {
            let errors = flush_rebasing_stale_bases(&mut writes, &store, &mut rebases_remaining);
            record_flush_errors(&last_error, &errors);
            if writes.is_empty() {
                health.flushed();
                // Commits accumulate in the WAL, which SQLite never shrinks on
                // its own; this costs one `stat` per flush and only does real
                // work once the sidecar has grown past its threshold.
                store.checkpoint_wal_if_large();
                due = None;
                retry_delay = config.retry_initial;
                attempts_this_cycle = 0;
                rebases_remaining = PERSIST_REBASES_PER_BATCH;
                retry_exhausted = false;
            } else {
                attempts_this_cycle = attempts_this_cycle.saturating_add(1);
                retry_exhausted = attempts_this_cycle >= config.max_automatic_attempts;
                failure = Some(select_failure_category(&errors));
                // A failing write lane used to be entirely silent: the error
                // reached `last_error` for the UI alone, so a window that had
                // stopped saving left nothing in the log to explain it after
                // the fact. One line per attempt would be a flood, so only the
                // first failure of a cycle and the attempt that gives up are
                // recorded.
                if attempts_this_cycle == 1 || retry_exhausted {
                    tracing::warn!(
                        attempts = attempts_this_cycle,
                        exhausted = retry_exhausted,
                        category = failure.map(SaveFailureCategory::label),
                        "a cagents workspace write could not be stored"
                    );
                }
                health.failed(&errors, retry_exhausted);
                due = (!retry_exhausted).then(|| Instant::now() + retry_delay);
                retry_delay = retry_delay.saturating_mul(2).min(config.retry_max);
            }
        }
        for checkpoint in checkpoints {
            let result = if writes.is_empty() {
                Ok(())
            } else {
                Err(save_refusal(failure))
            };
            let _ = checkpoint.send(result);
        }

        if shutdown {
            // The steady-state lane retries a failed batch until it lands, so
            // the last flush of the session has to as well: the single attempt
            // this once made discarded the user's final snapshot the first time
            // storage was momentarily unwritable.
            let deadline = Instant::now() + config.shutdown_flush_budget;
            // The last flush of the session gets a recovery of its own,
            // whatever the steady-state lane already spent: losing the final
            // snapshot to a base that fell behind is exactly the loss this
            // retry loop exists to prevent.
            let mut shutdown_rebases = PERSIST_REBASES_PER_BATCH;
            while !writes.is_empty() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                // A failing batch is usually blocked behind a lock another
                // process holds, so retrying instantly would only burn the
                // budget; the steady-state backoff already paces that wait.
                thread::sleep(retry_delay.min(remaining));
                let errors = flush_rebasing_stale_bases(&mut writes, &store, &mut shutdown_rebases);
                record_flush_errors(&last_error, &errors);
                if writes.is_empty() {
                    health.flushed();
                } else {
                    health.failed(&errors, false);
                }
                retry_delay = retry_delay.saturating_mul(2).min(config.retry_max);
            }
            // `clean_shutdown` asserts that storage holds everything this
            // session produced, so only a complete flush earns it; otherwise
            // the next open must recover rather than trust the database.
            if writes.is_empty()
                && let Err(error) = store.shutdown()
                && let Ok(mut message) = last_error.lock()
            {
                *message = Some(error.to_string());
            }
            return WriterExit::Shutdown;
        }
    }
}

/// Whether every failure in a batch is a rejected base generation, the one
/// cause re-reading generations can address.
///
/// Nothing else qualifies. A full disk, a read-only or corrupt database, or a
/// snapshot that does not encode is unaffected by which generation the write is
/// checked against, so rebasing on one of those would replace a failure the
/// reader has to be told about with a retry that silently repeats it. A batch
/// that mixes a stale base with a harder failure is left alone for the same
/// reason: the harder failure is the one that has to be fixed, and
/// `select_failure_category` already ranks it above a conflict. Once the harder
/// write succeeds, a batch of nothing but stale bases is what remains, and that
/// one does rebase.
fn only_stale_bases(errors: &[StoreError]) -> bool {
    !errors.is_empty()
        && errors
            .iter()
            .all(|error| matches!(error, StoreError::StaleGeneration { .. }))
}

/// Flushes the batch, and on a rejected base re-reads the generations storage
/// holds and flushes the very same content against them once.
///
/// `rebases_remaining` is deliberately a separate budget from the retry count
/// for transient errors. A transient failure is worth several spaced attempts
/// because the condition clears on its own; a rejected base is worth one
/// immediate retry, because the snapshot does not change and the fresh base
/// either lets it through or something is moving the generations under this
/// lane. The budget is only spent once a rebase has actually reseated the
/// bases, so a store that momentarily cannot be read keeps its recovery for
/// the ordinary retry instead of losing it to a failed read.
fn flush_rebasing_stale_bases(
    writes: &mut PendingWrites,
    store: &impl SessionStore,
    rebases_remaining: &mut u32,
) -> Vec<StoreError> {
    let errors = writes.flush(store);
    if *rebases_remaining == 0 || !only_stale_bases(&errors) {
        return errors;
    }
    if let Err(error) = writes.rebase(store) {
        // The recovery could not read storage. The batch's own cause is still
        // the conflict, so it is reported as one, and the read is retried with
        // the next attempt rather than charged to the rebase budget.
        tracing::warn!(
            %error,
            "could not re-read the generations a cagents workspace write was rejected against"
        );
        return errors;
    }
    *rebases_remaining -= 1;
    // A conflict the lane resolves by itself is not a failure the reader has to
    // act on, so it never reaches the health lane. It is not nothing either:
    // this is the state #124 was reported from, and the log is the only place a
    // resolved conflict can be seen after the fact.
    tracing::info!(
        rejections = errors.len(),
        "a cagents workspace write was rejected against a stale base; \
         retrying it against the stored generations"
    );
    writes.flush(store)
}

fn schedule_flush_after_changes(
    current_due: Option<Instant>,
    received_changes: bool,
    retry_exhausted: bool,
    attempts_this_cycle: u32,
    now: Instant,
    debounce: Duration,
) -> Option<Instant> {
    if received_changes && !retry_exhausted && attempts_this_cycle == 0 {
        // Preserve the trailing debounce for ordinary changes. Once a flush
        // has failed, keep its retry deadline stable instead of letting a
        // stream of new snapshots postpone recovery forever.
        Some(now + debounce)
    } else {
        current_due
    }
}

fn record_flush_errors(last_error: &Mutex<Option<String>>, errors: &[StoreError]) {
    if let Ok(mut slot) = last_error.lock() {
        *slot = (!errors.is_empty()).then(|| {
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        });
    }
}

/// The one category a batch of failures is reported as.
///
/// The most actionable failure wins rather than the newest: a full disk beside
/// an incidental busy database is what the reader has to fix.
fn select_failure_category(errors: &[StoreError]) -> SaveFailureCategory {
    errors
        .iter()
        .map(save_failure_category)
        .min_by_key(|category| category.selection_rank())
        .unwrap_or(SaveFailureCategory::Database)
}

/// Why an explicit save checkpoint could not be honoured.
///
/// The refusal reaches a dialog that asks the reader to resolve something, so
/// it has to say what and what to do about it. The category carries both and
/// is already redacted, unlike the store error it was derived from.
fn save_refusal(failure: Option<SaveFailureCategory>) -> String {
    match failure {
        Some(category) => format!(
            "workspace could not be saved: {}. {}",
            category.label(),
            category.guidance()
        ),
        None => "workspace could not be saved; its last attempt is still pending".to_owned(),
    }
}

pub fn current_boot_id() -> Result<BootId, SessionError> {
    let value =
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(SessionError::BootId)?;
    Ok(BootId::new(value.trim()))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashSet, VecDeque},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use rusqlite::Connection;

    use tempfile::tempdir;

    use crate::model::{
        CardId, CardKind, CardState, Existence, Exposure, LaunchRecipe, Residency,
        TerminalContinuity, TerminalId, TerminalSession, WorkspaceId,
    };

    use super::*;

    #[derive(Debug, Clone, Copy)]
    enum FakeWrite {
        Succeed,
        Fail,
        /// A failure no rebase can address.
        FailDiskFull,
        /// A base generation storage rejected.
        FailStale,
        Panic,
    }

    struct FakeStore {
        writes: Mutex<VecDeque<FakeWrite>>,
        /// What every write past the scripted ones does, so a test can describe
        /// a store that never accepts a write without scripting each attempt.
        exhausted: FakeWrite,
        /// Counts the generation re-reads the writer lane performed, which is
        /// what tells a rebase apart from an ordinary retry.
        rebases: Arc<AtomicUsize>,
    }

    impl FakeStore {
        fn new(writes: impl IntoIterator<Item = FakeWrite>) -> Self {
            Self {
                writes: Mutex::new(writes.into_iter().collect()),
                exhausted: FakeWrite::Succeed,
                rebases: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn always(write: FakeWrite) -> Self {
            Self {
                writes: Mutex::new(VecDeque::new()),
                exhausted: write,
                rebases: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn write(&self) -> Result<(), StoreError> {
            match self
                .writes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(self.exhausted)
            {
                FakeWrite::Succeed => Ok(()),
                FakeWrite::Fail => Err(StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "TOKEN=secret terminal output must never reach health",
                ))),
                FakeWrite::FailDiskFull => Err(StoreError::Io(std::io::Error::from_raw_os_error(
                    libc::ENOSPC,
                ))),
                FakeWrite::FailStale => Err(StoreError::StaleGeneration {
                    entity: "workspace".to_owned(),
                    expected: 1,
                    actual: 2,
                }),
                FakeWrite::Panic => panic!("injected writer panic"),
            }
        }
    }

    impl SessionStore for FakeStore {
        fn replace_terminal_identity(
            &self,
            _card_id: CardId,
            _expected_terminal_id: TerminalId,
            _replacement: &TerminalSession,
        ) -> Result<(), StoreError> {
            self.write()
        }

        fn save_workspace(
            &self,
            _state: &WorkspaceState,
            _base: SnapshotBase<'_>,
        ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
            self.write()?;
            Ok(crate::persistence::WorkspaceCommit::default())
        }

        fn save_project(&self, _project: &ProjectState) -> Result<u64, StoreError> {
            self.write().map(|()| 1)
        }

        fn save_worktree(&self, _worktree: &WorktreeState) -> Result<u64, StoreError> {
            self.write().map(|()| 1)
        }

        fn stored_generations(
            &self,
            _workspace: WorkspaceId,
        ) -> Result<StoredGenerations, StoreError> {
            self.rebases.fetch_add(1, Ordering::Relaxed);
            Ok(StoredGenerations::default())
        }

        fn stored_project_generation(
            &self,
            _project: ProjectId,
        ) -> Result<Option<u64>, StoreError> {
            Ok(None)
        }

        fn stored_worktree_generation(
            &self,
            _worktree: WorktreeId,
        ) -> Result<Option<u64>, StoreError> {
            Ok(None)
        }

        fn shutdown(&self) -> Result<(), StoreError> {
            Ok(())
        }
    }

    fn fast_persistence_config(max_automatic_attempts: u32) -> PersistenceConfig {
        PersistenceConfig {
            debounce: Duration::ZERO,
            retry_initial: Duration::from_millis(1),
            retry_max: Duration::from_millis(1),
            max_automatic_attempts,
            shutdown_flush_budget: Duration::from_millis(20),
        }
    }

    fn next_health(
        receiver: &async_channel::Receiver<SessionSaveHealthEvent>,
    ) -> SessionSaveHealthEvent {
        receiver
            .recv_blocking()
            .expect("health publisher must remain connected")
    }

    fn workspace() -> WorkspaceState {
        let launch = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-l".into()],
            working_directory: PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        WorkspaceState {
            schema_version: 1,
            id: WorkspaceId::new(),
            generation: 0,
            existence: Existence::Open,
            focused: None,
            active_worktree: None,
            worktree_focus: HashMap::new(),
            project_order: Vec::new(),
            channels: Vec::new(),
            active_channel: None,
            card_scopes: HashMap::new(),
            workspace_model_identity: None,
            cards: vec![CardState {
                id: CardId::new(),
                generation: 0,
                title: "shell".into(),
                width_fraction: 0.7,
                existence: Existence::Open,
                exposure: Exposure::Visible,
                residency: Residency::Hot,
                kind: CardKind::Terminal(TerminalSession {
                    recovery_origin: None,
                    id: TerminalId::new(),
                    launch: launch.clone(),
                    transcript_path: None,
                    resume_hint: None,
                    continuity: TerminalContinuity::RelaunchAvailable {
                        recipe: launch,
                        last_exit: None,
                    },
                }),
            }],
        }
    }

    #[test]
    fn new_changes_trail_debounce_without_postponing_an_active_retry() {
        let now = Instant::now();
        let old_debounce = now + Duration::from_millis(10);
        let new_debounce = now + Duration::from_millis(250);
        assert_eq!(
            schedule_flush_after_changes(
                Some(old_debounce),
                true,
                false,
                0,
                now,
                Duration::from_millis(250),
            ),
            Some(new_debounce)
        );

        let retry_due = now + Duration::from_millis(100);
        assert_eq!(
            schedule_flush_after_changes(
                Some(retry_due),
                true,
                false,
                1,
                now,
                Duration::from_millis(250),
            ),
            Some(retry_due)
        );
    }

    #[test]
    fn health_category_tracks_the_current_batch_deterministically() {
        let (health_tx, health_rx) = async_channel::unbounded();
        let mut publisher = HealthPublisher::new(health_tx);
        assert!(matches!(
            next_health(&health_rx).health,
            SessionSaveHealth::Healthy
        ));

        publisher.failed(
            &[StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "secret details",
            ))],
            false,
        );
        let first = next_health(&health_rx);
        let SessionSaveHealth::Retrying {
            first_failed_at_unix_ms,
            ..
        } = first.health
        else {
            panic!("first failure must be retrying");
        };

        publisher.failed(
            &[
                StoreError::StaleGeneration {
                    entity: "workspace id".to_owned(),
                    expected: 1,
                    actual: 2,
                },
                StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "different secret details",
                )),
            ],
            true,
        );
        assert_eq!(
            next_health(&health_rx).health,
            SessionSaveHealth::RetryExhausted {
                category: SaveFailureCategory::PermissionDenied,
                first_failed_at_unix_ms,
                attempts: 2,
            }
        );

        publisher.failed(
            &[StoreError::StaleGeneration {
                entity: "workspace id".to_owned(),
                expected: 1,
                actual: 2,
            }],
            true,
        );
        assert_eq!(
            next_health(&health_rx).health,
            SessionSaveHealth::RetryExhausted {
                category: SaveFailureCategory::Conflict,
                first_failed_at_unix_ms,
                attempts: 3,
            }
        );
    }

    #[test]
    fn a_refused_checkpoint_names_the_cause_without_the_store_error() {
        let (response, receiver) = std::sync::mpsc::sync_channel(1);
        let queue = Arc::new((
            Mutex::new(QueueState {
                latest: Some(Arc::new(workspace())),
                checkpoints: vec![response],
                ..QueueState::default()
            }),
            Condvar::new(),
        ));
        let (health_tx, _health_rx) = async_channel::unbounded();
        let worker_queue = Arc::clone(&queue);
        let worker = thread::spawn(move || {
            run_persistence_worker(
                FakeStore::new([FakeWrite::Fail]),
                PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
                worker_queue,
                Arc::new(Mutex::new(None)),
                HealthPublisher::new(health_tx),
                fast_persistence_config(2),
            );
        });

        let refusal = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("the checkpoint is answered rather than dropped")
            .expect_err("a failed flush cannot report a saved workspace");
        assert!(
            refusal.contains(SaveFailureCategory::PermissionDenied.label())
                && refusal.contains(SaveFailureCategory::PermissionDenied.guidance()),
            "a refusal that asks the reader to resolve something must say what and how: {refusal}"
        );
        assert!(
            !refusal.contains("TOKEN=secret"),
            "the raw store error retains secrets and must not reach a dialog: {refusal}"
        );

        let (lock, changed) = &*queue;
        lock.lock().unwrap().shutdown = true;
        changed.notify_one();
        worker.join().unwrap();
    }

    #[test]
    fn health_contract_reports_failure_exhaustion_retry_and_recovery() {
        let queue = Arc::new((
            Mutex::new(QueueState {
                latest: Some(Arc::new(workspace())),
                ..QueueState::default()
            }),
            Condvar::new(),
        ));
        let last_error = Arc::new(Mutex::new(None));
        let (health_tx, health_rx) = async_channel::unbounded();
        let health = HealthPublisher::new(health_tx);
        let worker_queue = Arc::clone(&queue);
        let worker_error = Arc::clone(&last_error);
        let worker = thread::spawn(move || {
            run_persistence_worker(
                FakeStore::new([FakeWrite::Fail, FakeWrite::Fail, FakeWrite::Succeed]),
                PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
                worker_queue,
                worker_error,
                health,
                fast_persistence_config(2),
            );
        });

        assert!(matches!(
            next_health(&health_rx).health,
            SessionSaveHealth::Healthy
        ));
        let retrying = next_health(&health_rx);
        let SessionSaveHealth::Retrying {
            category,
            first_failed_at_unix_ms,
            attempts,
        } = retrying.health
        else {
            panic!("first failed flush must enter retrying health");
        };
        assert_eq!(category, SaveFailureCategory::PermissionDenied);
        assert_eq!(attempts, 1);
        assert_eq!(retrying.last_successful_flush_at_unix_ms, None);

        let exhausted = next_health(&health_rx);
        assert_eq!(
            exhausted.health,
            SessionSaveHealth::RetryExhausted {
                category,
                first_failed_at_unix_ms,
                attempts: 2,
            }
        );
        assert!(!format!("{exhausted:?}").contains("TOKEN=secret"));
        assert!(
            last_error
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|error| { error.contains("TOKEN=secret") })
        );

        let (lock, changed) = &*queue;
        lock.lock().unwrap().retry_requested = true;
        changed.notify_one();
        let recovered = next_health(&health_rx);
        let SessionSaveHealth::Recovered {
            recovered_at_unix_ms,
        } = recovered.health
        else {
            panic!("successful manual retry must report recovery");
        };
        assert_eq!(
            recovered.last_successful_flush_at_unix_ms,
            Some(recovered_at_unix_ms)
        );
        assert!(last_error.lock().unwrap().is_none());

        lock.lock().unwrap().shutdown = true;
        changed.notify_one();
        worker.join().unwrap();
    }

    #[test]
    fn injected_writer_panic_reports_unavailable() {
        let queue = Arc::new((
            Mutex::new(QueueState {
                latest: Some(Arc::new(workspace())),
                ..QueueState::default()
            }),
            Condvar::new(),
        ));
        let (health_tx, health_rx) = async_channel::unbounded();
        let health = HealthPublisher::new(health_tx);
        let worker = thread::spawn(move || {
            run_persistence_worker(
                FakeStore::new([FakeWrite::Panic]),
                PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
                queue,
                Arc::new(Mutex::new(None)),
                health,
                fast_persistence_config(2),
            );
        });

        assert!(matches!(
            next_health(&health_rx).health,
            SessionSaveHealth::Healthy
        ));
        assert!(matches!(
            next_health(&health_rx).health,
            SessionSaveHealth::WriterUnavailable
        ));
        worker.join().unwrap();
    }

    #[test]
    fn chained_terminal_retries_preserve_the_persisted_expected_identity() {
        let card_id = CardId::new();
        let original_id = TerminalId::new();
        let middle_id = TerminalId::new();
        let final_id = TerminalId::new();
        let launch = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-l".into()],
            working_directory: PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        let terminal = |id| TerminalSession {
            recovery_origin: None,
            id,
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch.clone(),
                last_exit: None,
            },
        };
        let mut replacements = HashMap::new();

        merge_terminal_replacement(
            &mut replacements,
            TerminalIdentityReplacement {
                card_id,
                expected_terminal_id: original_id,
                replacement: terminal(middle_id),
            },
        );
        merge_terminal_replacement(
            &mut replacements,
            TerminalIdentityReplacement {
                card_id,
                expected_terminal_id: middle_id,
                replacement: terminal(final_id),
            },
        );

        let replacement = &replacements[&card_id];
        assert_eq!(replacement.expected_terminal_id, original_id);
        assert_eq!(replacement.replacement.id, final_id);
    }

    #[test]
    fn writer_coalesces_and_restores_latest_snapshot() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot = BootId::new("boot-test");
        let (mut runtime, restored) = SessionRuntime::open(&path, boot.clone()).unwrap();
        assert!(restored.is_none());

        let mut latest = workspace();
        latest.focused = latest.cards.first().map(|card| card.id);
        let latest = Arc::new(latest);
        runtime.snapshot(Arc::clone(&latest));
        runtime.snapshot(Arc::clone(&latest));
        runtime.shutdown();

        let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
        let restored = restored.unwrap();
        assert_eq!(restored.id, latest.id);
        assert_eq!(restored.focused, latest.focused);
        assert_eq!(
            restored
                .cards
                .iter()
                .map(|card| card.id)
                .collect::<HashSet<_>>(),
            latest.cards.iter().map(|card| card.id).collect()
        );
        reopened.shutdown();
    }

    #[test]
    fn window_snapshots_keep_identity_and_reopen_independently_while_source_is_live() {
        let directory = tempdir().unwrap();
        let primary = directory.path().join("sessions.sqlite3");
        let secondary = directory
            .path()
            .join("windows/monitor-two/sessions.sqlite3");
        let boot = BootId::new("window-test");
        let (mut first, _) = SessionRuntime::open(&primary, boot.clone()).unwrap();
        let mut one = workspace();
        first.snapshot(Arc::new(one.clone()));
        first.flush(Duration::from_secs(5)).unwrap();
        first
            .operation_store()
            .save_window_view("first-monitor")
            .unwrap();
        StateStore::copy_snapshot(&primary, &secondary).unwrap();
        let (mut second, restored) = SessionRuntime::open(&secondary, boot.clone()).unwrap();
        let mut two = restored.unwrap();
        assert_eq!(one.id, two.id);
        assert_eq!(one.cards[0].id, two.cards[0].id);
        assert_eq!(one.cards[0].kind, two.cards[0].kind);
        one.cards[0].width_fraction = 0.3;
        one.focused = Some(one.cards[0].id);
        two.cards[0].width_fraction = 0.8;
        two.focused = None;
        first.snapshot(Arc::new(one.clone()));
        second.snapshot(Arc::new(two.clone()));
        second
            .operation_store()
            .save_window_view("second-monitor")
            .unwrap();
        first.flush(Duration::from_secs(5)).unwrap();
        second.flush(Duration::from_secs(5)).unwrap();
        first.shutdown();
        // Closing one writer does not close or corrupt the other's live store.
        two.cards[0].width_fraction = 0.9;
        second.snapshot(Arc::new(two.clone()));
        second.flush(Duration::from_secs(5)).unwrap();
        second.shutdown();
        let (mut first, restored_one) = SessionRuntime::open(&primary, boot.clone()).unwrap();
        let (mut second, restored_two) = SessionRuntime::open(&secondary, boot).unwrap();
        assert_eq!(restored_one.unwrap().cards[0].width_fraction, 0.3);
        let restored_two = restored_two.unwrap();
        assert_eq!(restored_two.cards[0].width_fraction, 0.9);
        assert_eq!(restored_two.focused, None);
        assert_eq!(
            first
                .operation_store()
                .load_window_view()
                .unwrap()
                .as_deref(),
            Some("first-monitor")
        );
        assert_eq!(
            second
                .operation_store()
                .load_window_view()
                .unwrap()
                .as_deref(),
            Some("second-monitor")
        );
        first.shutdown();
        second.shutdown();
    }

    #[test]
    fn conversion_checkpoints_preserve_discovered_identity_before_fast_cli_return() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("chat-conversions.sqlite3");
        let boot = BootId::new("chat-conversions");
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let mut state = workspace();
        let CardKind::Terminal(mut terminal) = state.cards[0].kind.clone() else {
            unreachable!()
        };
        terminal.launch.program = "omp".into();
        terminal.launch.arguments.clear();
        terminal.transcript_path = None;
        let mut chat = crate::chat::Session {
            id: terminal.id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Omp,
                recipe: terminal.launch.clone(),
                session_id: None,
            },
            proof: None,
            terminal: None,
        };
        state.cards[0].kind = CardKind::Chat(chat.clone());
        runtime.snapshot_conversion(Arc::new(state.clone()));
        let exact_id = "77777777-1111-4111-8111-111111111111";
        chat.launch.session_id = Some(exact_id.into());
        state.cards[0].kind = CardKind::Chat(chat);
        runtime.snapshot(Arc::new(state.clone()));
        terminal.id = TerminalId::new();
        terminal.launch.arguments = vec!["--resume".into(), exact_id.into()];
        terminal.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: terminal.launch.clone(),
            last_exit: None,
        };
        state.cards[0].kind = CardKind::Terminal(terminal.clone());
        runtime.snapshot_conversion(Arc::new(state.clone()));
        // Even an immediate CLI retry must run after the return checkpoint.
        let previous_id = terminal.id;
        terminal.id = TerminalId::new();
        runtime.replace_terminal_identity(state.cards[0].id, previous_id, terminal.clone());
        state.cards[0].kind = CardKind::Terminal(terminal.clone());
        state.cards[0].title = "Latest terminal title".into();
        runtime.snapshot(Arc::new(state));
        runtime.shutdown();
        assert_eq!(runtime.last_error(), None);
        let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
        let restored = restored.unwrap();
        assert_eq!(restored.cards[0].kind, CardKind::Terminal(terminal));
        assert_eq!(restored.cards[0].title, "Latest terminal title");
        reopened.shutdown();
    }

    #[test]
    fn terminal_identity_replacement_precedes_snapshot_on_writer_lane() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("terminal-relaunch.sqlite3");
        let boot = BootId::new("boot-terminal-relaunch");
        let initial = workspace();
        {
            let (mut runtime, restored) = SessionRuntime::open(&path, boot.clone()).unwrap();
            assert!(restored.is_none());
            runtime.snapshot(Arc::new(initial.clone()));
            runtime.shutdown();
        }

        let (mut runtime, restored) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let mut relaunched = restored.unwrap();
        let card_id = relaunched.cards[0].id;
        let CardKind::Terminal(session) = &mut relaunched.cards[0].kind else {
            panic!("expected terminal card");
        };
        let old_id = session.id;
        let new_id = TerminalId::new();
        let recipe = session.launch.clone();
        let replacement = TerminalSession {
            recovery_origin: None,
            id: new_id,
            launch: recipe.clone(),
            transcript_path: Some(PathBuf::from("/tmp/relaunched.log")),
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        *session = replacement.clone();

        runtime.replace_terminal_identity(card_id, old_id, replacement);
        runtime.snapshot(Arc::new(relaunched));
        runtime.shutdown();
        assert_eq!(runtime.last_error(), None);

        let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
        let restored = restored.unwrap();
        let CardKind::Terminal(session) = &restored.cards[0].kind else {
            panic!("expected terminal card");
        };
        assert_eq!(session.id, new_id);
        assert_eq!(
            session.transcript_path,
            Some(PathBuf::from("/tmp/relaunched.log"))
        );
        reopened.shutdown();
    }

    #[test]
    fn writer_restores_project_and_worktree_catalog() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot = BootId::new("boot-catalog");
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let project = ProjectState {
            id: ProjectId::new(),
            generation: 0,
            name: "cagents".into(),
            repository: PathBuf::from("/repo/cagents"),
            existence: Existence::Open,
        };
        let worktree = WorktreeState {
            id: WorktreeId::new(),
            project_id: project.id,
            generation: 0,
            path: PathBuf::from("/repo/cagents"),
            branch: Some("main".into()),
            existence: Existence::Open,
        };
        runtime.save_project(project.clone());
        runtime.save_worktrees([worktree.clone()]);
        runtime.shutdown();

        let (mut reopened, _) = SessionRuntime::open(&path, boot).unwrap();
        let (projects, worktrees) = reopened.take_catalog();
        assert_eq!(
            projects,
            vec![ProjectState {
                generation: 1,
                ..project
            }]
        );
        assert_eq!(
            worktrees,
            vec![WorktreeState {
                generation: 1,
                ..worktree
            }]
        );
        reopened.shutdown();
    }

    #[test]
    fn transient_write_failure_retries_without_losing_snapshot() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("retry.sqlite3");
        let store = StateStore::open_with_options(
            &path,
            BootId::new("boot-retry"),
            crate::persistence::StoreOptions {
                busy_timeout: Duration::from_millis(10),
            },
        )
        .unwrap();
        let blocker = Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

        let queue = Arc::new((Mutex::new(QueueState::default()), Condvar::new()));
        queue.0.lock().unwrap().latest = Some(Arc::new(workspace()));
        let last_error = Arc::new(Mutex::new(None));
        let worker_queue = Arc::clone(&queue);
        let worker_error = Arc::clone(&last_error);
        let worker = thread::spawn(move || {
            let (health_tx, _health_rx) = async_channel::unbounded();
            let mut health = HealthPublisher::new(health_tx);
            persistence_loop(
                store,
                PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
                worker_queue,
                worker_error,
                &mut health,
                PersistenceConfig::default(),
            );
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while last_error.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline, "initial write did not fail");
            thread::sleep(Duration::from_millis(10));
        }
        blocker.execute_batch("ROLLBACK").unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let count: i64 = blocker
                .query_row("SELECT COUNT(*) FROM workspaces", [], |row| row.get(0))
                .unwrap();
            if count == 1 {
                break;
            }
            assert!(Instant::now() < deadline, "snapshot was not retried");
            thread::sleep(Duration::from_millis(10));
        }
        let (lock, changed) = &*queue;
        lock.lock().unwrap().shutdown = true;
        changed.notify_one();
        worker.join().unwrap();
        assert!(last_error.lock().unwrap().is_none());
    }

    #[test]
    fn failed_final_write_does_not_mark_store_clean() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("failed-shutdown.sqlite3");
        StateStore::open(&path, BootId::new("boot-shutdown"))
            .unwrap()
            .shutdown()
            .unwrap();
        let store = StateStore::open_with_options(
            &path,
            BootId::new("boot-shutdown"),
            crate::persistence::StoreOptions {
                busy_timeout: Duration::from_millis(10),
            },
        )
        .unwrap();
        let blocker = Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let queue = Arc::new((
            Mutex::new(QueueState {
                latest: Some(Arc::new(workspace())),
                shutdown: true,
                ..QueueState::default()
            }),
            Condvar::new(),
        ));
        let last_error = Arc::new(Mutex::new(None));
        let (health_tx, _health_rx) = async_channel::unbounded();
        let mut health = HealthPublisher::new(health_tx);

        persistence_loop(
            store,
            PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
            queue,
            Arc::clone(&last_error),
            &mut health,
            PersistenceConfig {
                // The blocker never lets go, so the budget only decides how long
                // this test waits for the flush to give up.
                shutdown_flush_budget: Duration::from_millis(200),
                ..PersistenceConfig::default()
            },
        );
        assert!(last_error.lock().unwrap().is_some());
        blocker.execute_batch("ROLLBACK").unwrap();
        drop(blocker);

        let reopened = StateStore::open(&path, BootId::new("boot-shutdown")).unwrap();
        assert!(reopened.recovery_report().dirty_shutdown);
    }

    #[test]
    fn retried_shutdown_flush_persists_the_snapshot_and_records_a_clean_shutdown() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("retried-shutdown.sqlite3");
        let boot = BootId::new("boot-shutdown-retry");
        StateStore::open(&path, boot.clone())
            .unwrap()
            .shutdown()
            .unwrap();
        let store = StateStore::open_with_options(
            &path,
            boot.clone(),
            crate::persistence::StoreOptions {
                busy_timeout: Duration::from_millis(10),
            },
        )
        .unwrap();
        let blocker = Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        // Released only after the first attempt has already failed, so nothing
        // is persisted unless the shutdown flush retries.
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            blocker.execute_batch("ROLLBACK").unwrap();
        });

        let snapshot = Arc::new(workspace());
        let queue = Arc::new((
            Mutex::new(QueueState {
                latest: Some(Arc::clone(&snapshot)),
                shutdown: true,
                ..QueueState::default()
            }),
            Condvar::new(),
        ));
        let last_error = Arc::new(Mutex::new(None));
        let (health_tx, _health_rx) = async_channel::unbounded();
        let mut health = HealthPublisher::new(health_tx);

        persistence_loop(
            store,
            PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
            queue,
            Arc::clone(&last_error),
            &mut health,
            PersistenceConfig::default(),
        );
        release.join().unwrap();
        assert!(last_error.lock().unwrap().is_none());

        let reopened = StateStore::open(&path, boot).unwrap();
        assert!(
            !reopened.recovery_report().dirty_shutdown,
            "a shutdown flush that eventually succeeded must still record a \
             clean shutdown"
        );
        assert!(
            reopened
                .load_workspace(snapshot.id, None)
                .unwrap()
                .is_some(),
            "the final snapshot must survive a shutdown flush that failed once"
        );
    }

    #[test]
    fn a_stale_base_is_rebased_and_the_snapshot_still_lands() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("stale-base.sqlite3");
        let boot = BootId::new("boot-stale-base");
        let store = StateStore::open(&path, boot.clone()).unwrap();
        let mut state = workspace();
        // A commit the writer lane never learns about: storage ends up one
        // generation ahead of the bases the lane is about to write against,
        // which is what a batch that applied without the lane seeing it leaves
        // behind.
        store
            .save_workspace(
                &state,
                SnapshotBase {
                    workspace: 0,
                    cards: &BTreeMap::new(),
                },
            )
            .unwrap();

        state.cards[0].title = "written after the conflict".into();
        state.focused = Some(state.cards[0].id);
        let snapshot = Arc::new(state);
        let queue = Arc::new((
            Mutex::new(QueueState {
                latest: Some(Arc::clone(&snapshot)),
                shutdown: true,
                ..QueueState::default()
            }),
            Condvar::new(),
        ));
        let last_error = Arc::new(Mutex::new(None));
        let (health_tx, _health_rx) = async_channel::unbounded();
        let mut health = HealthPublisher::new(health_tx);

        persistence_loop(
            store,
            PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
            queue,
            Arc::clone(&last_error),
            &mut health,
            fast_persistence_config(4),
        );
        assert_eq!(
            last_error.lock().unwrap().clone(),
            None,
            "a base the writer can re-read is not a save failure"
        );

        let reopened = StateStore::open(&path, boot).unwrap();
        let stored = reopened.load_workspace(snapshot.id, None).unwrap().unwrap();
        assert_eq!(stored.cards.len(), snapshot.cards.len());
        assert_eq!(stored.cards[0].id, snapshot.cards[0].id);
        assert_eq!(stored.cards[0].title, snapshot.cards[0].title);
        assert_eq!(stored.focused, snapshot.focused);
    }

    #[test]
    fn a_checkpoint_is_honoured_after_the_base_went_stale() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("stale-checkpoint.sqlite3");
        let boot = BootId::new("boot-stale-checkpoint");
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let mut state = workspace();
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(Duration::from_secs(5)).unwrap();

        // Advance the stored generation behind the writer lane's back, through
        // the very store it writes through. The lane's base is now one commit
        // behind, and the close path cannot tell: it only asks for a barrier.
        let mut cards = BTreeMap::new();
        cards.insert(state.cards[0].id, 1);
        runtime
            .operation_store()
            .save_workspace(
                &state,
                SnapshotBase {
                    workspace: 1,
                    cards: &cards,
                },
            )
            .unwrap();

        state.cards[0].width_fraction = 0.25;
        runtime.snapshot(Arc::new(state.clone()));
        runtime
            .flush(Duration::from_secs(5))
            .expect("a checkpoint must still be satisfied after the base went stale");
        assert_eq!(runtime.last_error(), None);
        runtime.shutdown();

        let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
        let restored = restored.expect("the workspace survives the conflict");
        assert_eq!(restored.cards[0].width_fraction, 0.25);
        reopened.shutdown();
    }

    #[test]
    fn a_failure_no_rebase_can_fix_is_reported_instead_of_rebased() {
        let store = FakeStore::always(FakeWrite::FailDiskFull);
        let rebases = Arc::clone(&store.rebases);
        let (response, receiver) = std::sync::mpsc::sync_channel(1);
        let queue = Arc::new((
            Mutex::new(QueueState {
                latest: Some(Arc::new(workspace())),
                checkpoints: vec![response],
                shutdown: true,
                ..QueueState::default()
            }),
            Condvar::new(),
        ));
        let (health_tx, health_rx) = async_channel::unbounded();
        let mut health = HealthPublisher::new(health_tx);

        persistence_loop(
            store,
            PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
            queue,
            Arc::new(Mutex::new(None)),
            &mut health,
            fast_persistence_config(2),
        );

        let refusal = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("the checkpoint is answered rather than dropped")
            .expect_err("a full disk cannot report a saved workspace");
        assert!(
            refusal.contains(SaveFailureCategory::DiskFull.label())
                && refusal.contains(SaveFailureCategory::DiskFull.guidance()),
            "a full disk has to reach the reader as one: {refusal}"
        );
        assert_eq!(
            rebases.load(Ordering::Relaxed),
            0,
            "re-reading generations cannot free disk space, so no rebase may be attempted"
        );
        let mut categories = Vec::new();
        while let Ok(event) = health_rx.try_recv() {
            match event.health {
                SessionSaveHealth::Retrying { category, .. }
                | SessionSaveHealth::RetryExhausted { category, .. } => categories.push(category),
                _ => {}
            }
        }
        assert!(
            !categories.is_empty()
                && categories
                    .iter()
                    .all(|category| *category == SaveFailureCategory::DiskFull),
            "a hard failure must never be reported as a conflict: {categories:?}"
        );
    }

    #[test]
    fn a_conflict_the_rebase_cannot_resolve_still_reports_its_category() {
        let store = FakeStore::always(FakeWrite::FailStale);
        let rebases = Arc::clone(&store.rebases);
        let queue = Arc::new((
            Mutex::new(QueueState {
                latest: Some(Arc::new(workspace())),
                ..QueueState::default()
            }),
            Condvar::new(),
        ));
        let (health_tx, health_rx) = async_channel::unbounded();
        let health = HealthPublisher::new(health_tx);
        let worker_queue = Arc::clone(&queue);
        let worker = thread::spawn(move || {
            run_persistence_worker(
                store,
                PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
                worker_queue,
                Arc::new(Mutex::new(None)),
                health,
                fast_persistence_config(2),
            );
        });

        let exhausted = loop {
            match next_health(&health_rx).health {
                SessionSaveHealth::RetryExhausted {
                    category, attempts, ..
                } => break (category, attempts),
                _ => continue,
            }
        };
        assert_eq!(
            exhausted,
            (SaveFailureCategory::Conflict, 2),
            "a conflict the rebase could not resolve still has to surface as one"
        );
        assert_eq!(
            rebases.load(Ordering::Relaxed),
            1,
            "a batch is rebased once however often it is retried"
        );

        let (lock, changed) = &*queue;
        lock.lock().unwrap().shutdown = true;
        changed.notify_one();
        worker.join().unwrap();
    }
}
