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
        BootId, CardId, CardKind, ProjectId, ProjectState, TerminalId, TerminalSession,
        WorkspaceId, WorkspaceState, WorktreeId, WorktreeState,
    },
    persistence::{
        SnapshotBase, StateStore, StoreError, StoredGenerations, TranscriptPruneReport,
        TranscriptRetention,
    },
};

const PERSIST_DEBOUNCE: Duration = Duration::from_millis(250);
// A busy window must keep saving even when changes never leave a debounce gap.
const PERSIST_MAX_DELAY: Duration = Duration::from_secs(1);
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

impl From<SaveFailureCategory> for crate::diagnostics::Failure {
    fn from(category: SaveFailureCategory) -> Self {
        match category {
            SaveFailureCategory::Busy => Self::Busy,
            SaveFailureCategory::ReadOnly => Self::ReadOnly,
            SaveFailureCategory::DiskFull => Self::DiskFull,
            SaveFailureCategory::PermissionDenied => Self::PermissionDenied,
            SaveFailureCategory::StorageUnavailable => Self::StorageUnavailable,
            SaveFailureCategory::Corrupt => Self::Corrupt,
            SaveFailureCategory::Conflict => Self::Conflict,
            SaveFailureCategory::Serialization => Self::Serialization,
            SaveFailureCategory::InvalidData => Self::InvalidData,
            SaveFailureCategory::Io => Self::Io,
            SaveFailureCategory::Database => Self::Database,
        }
    }
}

/// What to do about writes storage refused by construction. Retrying the same
/// content cannot help, so the remedy is changing it.
pub const QUARANTINE_GUIDANCE: &str = "Storage refused these changes and will not retry them on \
     its own; everything else keeps saving. Changing or closing the item saves it again. If this \
     repeats, report the failure category and database path.";

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
            Self::Corrupt => {
                "Back up the database, then repair it or restore a healthy backup before retrying. Editing the item cannot repair stored data."
            }
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
    /// Writes storage refused by construction, set aside so they cannot hold
    /// the rest of the lane hostage. Orthogonal to `health`: a lane that saves
    /// everything else is healthy while still reporting these.
    pub quarantined: Vec<QuarantinedWrite>,
}

/// What one refused write would have saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteSubject {
    /// The window's layout and cards: an autosave or a conversion checkpoint.
    /// `card` is the card storage refused within it, when it named one.
    Workspace {
        card: Option<CardId>,
    },
    /// The fresh identity a card's payload was rebound to: a relaunched
    /// terminal, or a new conversation in a chat card.
    CardIdentity {
        card: CardId,
        kind: ReplacedIdentity,
    },
    Project {
        id: ProjectId,
        name: String,
    },
    Worktree {
        id: WorktreeId,
        path: PathBuf,
        branch: Option<String>,
    },
}

/// What kind of payload an identity replacement rebinds a card to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacedIdentity {
    Terminal,
    Chat,
}

impl WriteSubject {
    /// Whether the refused write is part of the window's own state, the
    /// state a restart restores and a terminal handoff has to be committed in.
    /// A refused catalog row loses that row alone.
    pub fn is_window_state(&self) -> bool {
        matches!(self, Self::Workspace { .. } | Self::CardIdentity { .. })
    }

    /// The subject as a refused barrier names it: what kind of write it was
    /// and the identifier the log and the save notice know it by. It carries
    /// no title, name or path, so it can leave the window - an updater prints
    /// it - where the notice's own wording, which does name them, cannot.
    fn summary(&self) -> String {
        match self {
            Self::Workspace { card: None } => "the window's layout and panes".to_owned(),
            Self::Workspace { card: Some(card) } => {
                format!("the window's layout and panes, at pane {card}")
            }
            Self::CardIdentity {
                card,
                kind: ReplacedIdentity::Terminal,
            } => format!("the relaunched terminal in pane {card}"),
            Self::CardIdentity {
                card,
                kind: ReplacedIdentity::Chat,
            } => format!("the new conversation in pane {card}"),
            Self::Project { id, .. } => format!("project {id}"),
            Self::Worktree { id, .. } => format!("worktree {id}"),
        }
    }
}

/// A write storage refused by construction. It is never retried on its own;
/// a newer write of the same subject supersedes it, and an explicit retry
/// offers it to storage once more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedWrite {
    pub subject: WriteSubject,
    pub category: SaveFailureCategory,
    pub refused_at_unix_ms: i64,
    /// Writes that cannot land before this one does and are held back until
    /// it is superseded: the worktree rows of a project storage has no row for.
    pub held_back: usize,
}

#[derive(Debug, Clone, Copy)]
struct PersistenceConfig {
    debounce: Duration,
    max_delay: Duration,
    retry_initial: Duration,
    retry_max: Duration,
    max_automatic_attempts: u32,
    shutdown_flush_budget: Duration,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            debounce: PERSIST_DEBOUNCE,
            max_delay: PERSIST_MAX_DELAY,
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

/// An explicit change of the identity a card's payload is bound to, which
/// ordinary snapshots are not allowed to make.
#[derive(Debug, Clone)]
struct IdentityReplacement {
    card_id: CardId,
    expected_terminal_id: TerminalId,
    replacement: Replacement,
}

#[derive(Debug, Clone)]
enum Replacement {
    /// A terminal relaunched under a fresh identity.
    Terminal(TerminalSession),
    /// A new conversation started in a chat card in place of its own.
    Chat(crate::chat::Session),
}

impl Replacement {
    fn id(&self) -> TerminalId {
        match self {
            Self::Terminal(session) => session.id,
            Self::Chat(session) => session.id,
        }
    }

    fn same_kind(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    fn kind(&self) -> ReplacedIdentity {
        match self {
            Self::Terminal(_) => ReplacedIdentity::Terminal,
            Self::Chat(_) => ReplacedIdentity::Chat,
        }
    }
}

impl IdentityReplacement {
    /// Whether a card of `kind` still holds the payload this replacement
    /// would rebind, so it still has something to replace.
    fn replaces(&self, kind: &CardKind) -> bool {
        let held = match (&self.replacement, kind) {
            (Replacement::Terminal(_), CardKind::Terminal(session)) => session.id,
            (Replacement::Chat(_), CardKind::Chat(session)) => session.id,
            _ => return false,
        };
        held == self.expected_terminal_id && held != self.replacement.id()
    }

    fn apply(&self, store: &impl SessionStore) -> Result<(), StoreError> {
        match &self.replacement {
            Replacement::Terminal(session) => {
                store.replace_terminal_identity(self.card_id, self.expected_terminal_id, session)
            }
            Replacement::Chat(session) => {
                store.replace_chat_identity(self.card_id, self.expected_terminal_id, session)
            }
        }
    }
}

#[derive(Debug)]
enum ConversionCheckpoint {
    Snapshot(Arc<WorkspaceState>),
    PaneAdoptions {
        snapshot: Arc<WorkspaceState>,
        adoptions: Vec<crate::persistence::TerminalPaneAdoption>,
        restorations: Vec<crate::persistence::TerminalPaneRestoration>,
    },
    IdentityReplacement(IdentityReplacement),
    ChatRecovery(ChatRecoveryWrite),
    RetainedChatCopy {
        copy: crate::persistence::RetainedChatCopy,
        // The first failure is reported once. Data-only retries retain their
        // payload but cannot block on a second reply after the receiver leaves.
        reply: Option<std::sync::mpsc::SyncSender<std::result::Result<(), String>>>,
    },
    TerminalOwnershipRecovery {
        snapshot: Arc<WorkspaceState>,
        recovery: crate::persistence::TerminalPaneAdoption,
        reply: std::sync::mpsc::SyncSender<std::result::Result<(), String>>,
    },
}

#[derive(Debug)]
enum ChatRecoveryKind {
    Account,
    Upgrade,
    #[cfg(feature = "ui")]
    AuthenticatedUpgrade(crate::chat::client::UpgradeRecovery),
}

#[derive(Debug)]
struct ChatRecoveryWrite {
    kind: ChatRecoveryKind,
    card: CardId,
    source: crate::chat::Session,
    replacement: crate::chat::Session,
    reply: std::sync::mpsc::SyncSender<std::result::Result<(), String>>,
}

fn merge_identity_replacement(
    replacements: &mut HashMap<CardId, IdentityReplacement>,
    incoming: IdentityReplacement,
) {
    match replacements.entry(incoming.card_id) {
        std::collections::hash_map::Entry::Occupied(mut entry)
            if entry.get().replacement.id() == incoming.expected_terminal_id
                && entry.get().replacement.same_kind(&incoming.replacement) =>
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
    identity_replacements: HashMap<CardId, IdentityReplacement>,
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
            for replacement in std::mem::take(&mut queue.identity_replacements).into_values() {
                queue
                    .conversions
                    .push_back(ConversionCheckpoint::IdentityReplacement(replacement));
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

    pub fn snapshot_terminal_adoptions(
        &self,
        state: Arc<WorkspaceState>,
        adoptions: Vec<crate::persistence::TerminalPaneAdoption>,
    ) {
        self.snapshot_terminal_panes(state, adoptions, Vec::new());
    }

    pub fn snapshot_terminal_panes(
        &self,
        state: Arc<WorkspaceState>,
        adoptions: Vec<crate::persistence::TerminalPaneAdoption>,
        restorations: Vec<crate::persistence::TerminalPaneRestoration>,
    ) {
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            queue.ordered_identities = true;
            for replacement in std::mem::take(&mut queue.identity_replacements).into_values() {
                queue
                    .conversions
                    .push_back(ConversionCheckpoint::IdentityReplacement(replacement));
            }
            if let Some(previous) = queue.latest.take() {
                queue
                    .conversions
                    .push_back(ConversionCheckpoint::Snapshot(previous));
            }
            queue
                .conversions
                .push_back(ConversionCheckpoint::PaneAdoptions {
                    snapshot: state,
                    adoptions,
                    restorations,
                });
            changed.notify_one();
        }
    }

    /// Queue a reviewed ownership repair after every already-offered write.
    /// The receipt succeeds only after the full snapshot commits; neither
    /// enqueueing nor a failed attempt retires its save quarantine.
    pub fn recover_terminal_ownership(
        &self,
        snapshot: Arc<WorkspaceState>,
        recovery: crate::persistence::TerminalPaneAdoption,
    ) -> std::result::Result<std::sync::mpsc::Receiver<std::result::Result<(), String>>, String>
    {
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        let (lock, changed) = &*self.queue;
        let mut queue = lock.lock().map_err(|_| "Session writer is unavailable")?;
        if queue.shutdown {
            return Err("Session writer is shutting down".into());
        }
        queue.ordered_identities = true;
        for replacement in std::mem::take(&mut queue.identity_replacements).into_values() {
            queue
                .conversions
                .push_back(ConversionCheckpoint::IdentityReplacement(replacement));
        }
        if let Some(previous) = queue.latest.take() {
            queue
                .conversions
                .push_back(ConversionCheckpoint::Snapshot(previous));
        }
        queue
            .conversions
            .push_back(ConversionCheckpoint::TerminalOwnershipRecovery {
                snapshot,
                recovery,
                reply,
            });
        changed.notify_one();
        Ok(receiver)
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
        self.replace_identity(
            IdentityReplacement {
                card_id,
                expected_terminal_id,
                replacement: Replacement::Terminal(replacement),
            },
            None,
        );
    }

    /// Records that chat card `card_id` now holds a new conversation in place
    /// of the one with identity `expected_id`: another provider or account for
    /// a thread that had not begun, or a fresh start where a resume failed.
    /// Snapshots refuse such a change on their own, since a card silently
    /// re-pointed at another conversation would lose the one it had.
    ///
    /// `state` is the workspace that already shows the replacement. It is
    /// queued under the same lock, so the writer can never pair the
    /// replacement with an older snapshot that still holds the previous
    /// conversation and would be refused.
    pub fn replace_chat_identity(
        &self,
        card_id: CardId,
        expected_id: TerminalId,
        replacement: crate::chat::Session,
        state: Arc<WorkspaceState>,
    ) {
        self.replace_identity(
            IdentityReplacement {
                card_id,
                expected_terminal_id: expected_id,
                replacement: Replacement::Chat(replacement),
            },
            Some(state),
        );
    }

    /// Commit an exact account transition once, in order with queued snapshots.
    /// The caller keeps its candidate inactive until this receipt succeeds;
    /// failures are returned without retaining an automatic retry.
    pub fn rebind_chat_account(
        &self,
        card_id: CardId,
        source: crate::chat::Session,
        replacement: crate::chat::Session,
    ) -> std::result::Result<std::sync::mpsc::Receiver<std::result::Result<(), String>>, String>
    {
        self.rebind_chat(ChatRecoveryKind::Account, card_id, source, replacement)
    }

    /// Commit authenticated daemon-upgrade lineage before activating the chat.
    pub fn rebind_chat_upgrade(
        &self,
        card_id: CardId,
        source: crate::chat::Session,
        replacement: crate::chat::Session,
    ) -> std::result::Result<std::sync::mpsc::Receiver<std::result::Result<(), String>>, String>
    {
        self.rebind_chat(ChatRecoveryKind::Upgrade, card_id, source, replacement)
    }

    /// Preserve inert saved text independently of a refused canonical-pane
    /// snapshot. The receipt confirms only these local bytes, never an actor.
    pub fn preserve_chat_copy(
        &self,
        copy: crate::persistence::RetainedChatCopy,
    ) -> std::result::Result<std::sync::mpsc::Receiver<std::result::Result<(), String>>, String>
    {
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        let (lock, changed) = &*self.queue;
        let mut queue = lock.lock().map_err(|_| "Session writer is unavailable")?;
        if queue.shutdown {
            return Err("Session writer is shutting down".into());
        }
        queue.ordered_identities = true;
        for replacement in std::mem::take(&mut queue.identity_replacements).into_values() {
            queue
                .conversions
                .push_back(ConversionCheckpoint::IdentityReplacement(replacement));
        }
        if let Some(snapshot) = queue.latest.take() {
            queue
                .conversions
                .push_back(ConversionCheckpoint::Snapshot(snapshot));
        }
        queue
            .conversions
            .push_back(ConversionCheckpoint::RetainedChatCopy {
                copy,
                reply: Some(reply),
            });
        changed.notify_one();
        Ok(receiver)
    }

    /// Queue a grant-bound attachment and await the same one-shot durable
    /// receipt as ordinary recovery. Descriptor arguments come only from it.
    #[cfg(feature = "ui")]
    pub(crate) fn rebind_authenticated_chat_upgrade(
        &self,
        card_id: CardId,
        recovery: crate::chat::client::UpgradeRecovery,
    ) -> std::result::Result<std::sync::mpsc::Receiver<std::result::Result<(), String>>, String>
    {
        let source = recovery.source().clone();
        let replacement = recovery.replacement().clone();
        self.rebind_chat(
            ChatRecoveryKind::AuthenticatedUpgrade(recovery),
            card_id,
            source,
            replacement,
        )
    }

    fn rebind_chat(
        &self,
        kind: ChatRecoveryKind,
        card_id: CardId,
        source: crate::chat::Session,
        replacement: crate::chat::Session,
    ) -> std::result::Result<std::sync::mpsc::Receiver<std::result::Result<(), String>>, String>
    {
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        let (lock, changed) = &*self.queue;
        let mut queue = lock.lock().map_err(|_| "Session writer is unavailable")?;
        if queue.shutdown {
            return Err("Session writer is shutting down".into());
        }
        queue.ordered_identities = true;
        for replacement in std::mem::take(&mut queue.identity_replacements).into_values() {
            queue
                .conversions
                .push_back(ConversionCheckpoint::IdentityReplacement(replacement));
        }
        if let Some(snapshot) = queue.latest.take() {
            queue
                .conversions
                .push_back(ConversionCheckpoint::Snapshot(snapshot));
        }
        queue
            .conversions
            .push_back(ConversionCheckpoint::ChatRecovery(ChatRecoveryWrite {
                kind,
                card: card_id,
                source,
                replacement,
                reply,
            }));
        changed.notify_one();
        Ok(receiver)
    }

    /// Queues `replacement`, then `state` as the newest snapshot when given.
    fn replace_identity(
        &self,
        replacement: IdentityReplacement,
        state: Option<Arc<WorkspaceState>>,
    ) {
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            if queue.ordered_identities {
                if let Some(previous) = queue.latest.take() {
                    queue
                        .conversions
                        .push_back(ConversionCheckpoint::Snapshot(previous));
                }
                queue
                    .conversions
                    .push_back(ConversionCheckpoint::IdentityReplacement(replacement));
            } else {
                merge_identity_replacement(&mut queue.identity_replacements, replacement);
            }
            if let Some(state) = state {
                queue.latest = Some(state);
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
    identity_replacements: HashMap<CardId, IdentityReplacement>,
    workspace_generation: u64,
    card_generations: BTreeMap<CardId, u64>,
    /// Doubles as the set of projects storage holds a row for, which is what
    /// decides whether a worktree row can be written yet.
    project_generations: HashMap<ProjectId, u64>,
    worktree_generations: HashMap<WorktreeId, u64>,
    quarantine: Quarantine,
    /// Successful, explicit chat transitions translate only old snapshots
    /// queued while the UI awaited acknowledgment. Other card fields, including
    /// tombstones, remain exactly as submitted.
    chat_rebindings: HashMap<CardId, (crate::chat::Session, crate::chat::Session)>,
    retained_chat_copies: HashMap<CardId, crate::persistence::RetainedChatCopy>,
}

/// When storage first refused a subject, and why it refuses it now.
#[derive(Debug, Clone, Copy)]
struct Refusal {
    category: SaveFailureCategory,
    at_unix_ms: i64,
    /// Tells one cause from another without keeping the store error, which
    /// stays on this thread. A subject refused again for a new reason is a new
    /// thing to explain; one refused again for the same reason is not.
    cause: u64,
}

/// Writes storage refused by construction, one per subject.
///
/// A refused write leaves the pending set, so it no longer fails every attempt
/// and exhausts the lane for writes that have nothing to do with it (#268). It
/// is kept rather than dropped for two reasons: an explicit retry offers it to
/// storage again, and it has to stay visible until something supersedes it.
/// What supersedes it is a newer write of the same subject that storage
/// accepts. A refusal of that newer write replaces the entry but keeps the
/// time of the first one, so the reader sees how long the subject has gone
/// unsaved.
#[derive(Debug, Default)]
struct Quarantine {
    /// The newest workspace snapshot storage refused, with the card it
    /// refused within it when it named one. Every snapshot is the whole
    /// window, so any later one that lands supersedes it.
    workspace: Option<(Arc<WorkspaceState>, Option<CardId>, Refusal)>,
    identity_replacements: HashMap<CardId, (IdentityReplacement, Refusal)>,
    projects: HashMap<ProjectId, (ProjectState, Refusal)>,
    worktrees: HashMap<WorktreeId, (WorktreeState, Refusal)>,
}

impl Refusal {
    fn new(error: &StoreError) -> Self {
        use std::hash::{Hash, Hasher};

        let mut cause = std::collections::hash_map::DefaultHasher::new();
        error.to_string().hash(&mut cause);
        Self {
            category: save_failure_category(error),
            at_unix_ms: unix_millis(),
            cause: cause.finish(),
        }
    }
}

/// Records a refusal of `subject`, keeping the time of the first one.
///
/// The store error is logged in full here and nowhere else. A write refused by
/// construction fails on content cagents produced, and its error names that
/// content by identifier - which is what makes the log line worth writing - but
/// never carries the connection details a storage failure can, which is why
/// only these errors are logged verbatim.
fn refuse<K: Eq + std::hash::Hash, V>(
    entries: &mut HashMap<K, (V, Refusal)>,
    key: K,
    write: V,
    subject: WriteSubject,
    error: &StoreError,
) {
    let refusal = record_refusal(
        entries.get(&key).map(|(_, refusal)| *refusal),
        subject,
        error,
    );
    entries.insert(key, (write, refusal));
}

fn record_refusal(previous: Option<Refusal>, subject: WriteSubject, error: &StoreError) -> Refusal {
    let refusal = Refusal::new(error);
    match previous {
        None => {
            tracing::warn!(
                ?subject,
                %error,
                category = refusal.category.label(),
                "storage refused a cagents write by construction; setting it aside so the rest \
                 of the session keeps saving"
            );
            refusal
        }
        Some(first) => {
            // A subject stays refused for as long as every newer write of it
            // is refused too, and only the first refusal was ever logged above
            // debug: a window that went on to be refused for some other reason
            // left a log that still named the first, long since resolved.
            if first.cause == refusal.cause {
                tracing::debug!(?subject, %error, "storage refused a newer write of a refused subject");
            } else {
                tracing::warn!(
                    ?subject,
                    %error,
                    category = refusal.category.label(),
                    "storage refused a newer write of a refused subject, for a different reason"
                );
            }
            Refusal {
                at_unix_ms: first.at_unix_ms,
                ..refusal
            }
        }
    }
}

fn identity_subject(replacement: &IdentityReplacement) -> WriteSubject {
    WriteSubject::CardIdentity {
        card: replacement.card_id,
        kind: replacement.replacement.kind(),
    }
}

fn project_subject(project: &ProjectState) -> WriteSubject {
    WriteSubject::Project {
        id: project.id,
        name: project.name.clone(),
    }
}

fn worktree_subject(worktree: &WorktreeState) -> WriteSubject {
    WriteSubject::Worktree {
        id: worktree.id,
        path: worktree.path.clone(),
        branch: worktree.branch.clone(),
    }
}

/// Logs a refused subject that a newer write of it superseded.
fn superseded(subject: Option<WriteSubject>) {
    if let Some(subject) = subject {
        tracing::info!(
            ?subject,
            "a newer write of a refused subject landed; it is saved again"
        );
    }
}

impl Quarantine {
    fn is_empty(&self) -> bool {
        self.workspace.is_none()
            && self.identity_replacements.is_empty()
            && self.projects.is_empty()
            && self.worktrees.is_empty()
    }

    /// Why the window's own state is not fully saved, when a refusal is the
    /// reason. A barrier and a clean shutdown both vouch for that state.
    fn window_state(&self) -> Option<SaveFailureCategory> {
        self.workspace
            .iter()
            .map(|(_, _, refusal)| refusal)
            .chain(
                self.identity_replacements
                    .values()
                    .map(|(_, refusal)| refusal),
            )
            .map(|refusal| refusal.category)
            .min_by_key(|category| category.selection_rank())
    }

    /// Rewrites a replacement that continues from a refused one to start at the
    /// identity storage still holds.
    ///
    /// Replacements chain identities: once `A -> B` is refused, storage still
    /// holds `A` while the card moves on to `B -> C`. The writer lane merges such
    /// a chain while both halves are pending; this is the same merge for a first
    /// half storage already refused, and without it `B -> C` could only be
    /// refused for expecting an identity storage never held.
    fn continue_refused(&self, replacement: &mut IdentityReplacement) {
        if let Some((refused, _)) = self.identity_replacements.get(&replacement.card_id)
            && refused.replacement.id() == replacement.expected_terminal_id
            && refused.replacement.same_kind(&replacement.replacement)
        {
            replacement.expected_terminal_id = refused.expected_terminal_id;
        }
    }

    fn refuse_workspace(&mut self, snapshot: Arc<WorkspaceState>, error: &StoreError) {
        let card = match error {
            StoreError::Card { card, .. } => Some(*card),
            _ => None,
        };
        let refusal = record_refusal(
            self.workspace.as_ref().map(|(_, _, refusal)| *refusal),
            WriteSubject::Workspace { card },
            error,
        );
        self.workspace = Some((snapshot, card, refusal));
    }

    /// Clears what a snapshot storage accepted settles: the refused snapshot it
    /// replaces, and every refused identity replacement whose card it no
    /// longer holds open at the identity that replacement would replace.
    ///
    /// Storage rejects a snapshot that rebinds a card's payload to an identity
    /// other than the one it holds, so the card's identity in a landed
    /// snapshot is the one storage holds. Anything but the replaced identity
    /// leaves the refusal with nothing to replace - the card closed, converted,
    /// or first stored at its new identity - and nothing else could clear it:
    /// it would fail every barrier and the clean shutdown for the rest of the
    /// session.
    fn snapshot_landed(&mut self, snapshot: &WorkspaceState) {
        superseded(
            self.workspace
                .take()
                .map(|(_, card, _)| WriteSubject::Workspace { card }),
        );
        self.identity_replacements.retain(|card, (refused, _)| {
            let outstanding = snapshot.cards.iter().any(|state| {
                state.id == *card && state.existence.is_open() && refused.replaces(&state.kind)
            });
            if !outstanding {
                tracing::info!(
                    subject = ?identity_subject(refused),
                    "a refused identity replacement no longer applies to the saved window"
                );
            }
            outstanding
        });
    }

    fn refuse_replacement(&mut self, replacement: IdentityReplacement, error: &StoreError) {
        let subject = identity_subject(&replacement);
        refuse(
            &mut self.identity_replacements,
            replacement.card_id,
            replacement,
            subject,
            error,
        );
    }

    fn replacement_landed(&mut self, card: CardId) {
        superseded(
            self.identity_replacements
                .remove(&card)
                .map(|(refused, _)| identity_subject(&refused)),
        );
    }

    fn refuse_project(&mut self, project: ProjectState, error: &StoreError) {
        let subject = project_subject(&project);
        refuse(&mut self.projects, project.id, project, subject, error);
    }

    fn project_landed(&mut self, id: ProjectId) {
        superseded(
            self.projects
                .remove(&id)
                .map(|(project, _)| project_subject(&project)),
        );
    }

    fn refuse_worktree(&mut self, worktree: WorktreeState, error: &StoreError) {
        let subject = worktree_subject(&worktree);
        refuse(&mut self.worktrees, worktree.id, worktree, subject, error);
    }

    fn worktree_landed(&mut self, id: WorktreeId) {
        superseded(
            self.worktrees
                .remove(&id)
                .map(|(worktree, _)| worktree_subject(&worktree)),
        );
    }

    /// What the health lane reports. `held_back` counts the pending worktree
    /// rows that wait on a refused project storage holds no row for.
    fn report(
        &self,
        pending_worktrees: &HashMap<WorktreeId, WorktreeState>,
        stored_projects: &HashMap<ProjectId, u64>,
    ) -> Vec<QuarantinedWrite> {
        let entry = |subject, refusal: &Refusal, held_back| QuarantinedWrite {
            subject,
            category: refusal.category,
            refused_at_unix_ms: refusal.at_unix_ms,
            held_back,
        };
        let mut report = Vec::new();
        if let Some((_, card, refusal)) = &self.workspace {
            report.push(entry(WriteSubject::Workspace { card: *card }, refusal, 0));
        }
        let mut replacements = self.identity_replacements.values().collect::<Vec<_>>();
        replacements.sort_by_key(|(replacement, _)| replacement.card_id);
        report.extend(
            replacements
                .into_iter()
                .map(|(replacement, refusal)| entry(identity_subject(replacement), refusal, 0)),
        );
        let mut projects = self.projects.values().collect::<Vec<_>>();
        projects
            .sort_by(|(left, _), (right, _)| (&left.name, left.id).cmp(&(&right.name, right.id)));
        report.extend(projects.into_iter().map(|(project, refusal)| {
            let held_back = if stored_projects.contains_key(&project.id) {
                0
            } else {
                pending_worktrees
                    .values()
                    .filter(|worktree| worktree.project_id == project.id)
                    .count()
            };
            entry(project_subject(project), refusal, held_back)
        }));
        let mut worktrees = self.worktrees.values().collect::<Vec<_>>();
        worktrees
            .sort_by(|(left, _), (right, _)| (&left.path, left.id).cmp(&(&right.path, right.id)));
        report.extend(
            worktrees
                .into_iter()
                .map(|(worktree, refusal)| entry(worktree_subject(worktree), refusal, 0)),
        );
        report
    }
}

trait SessionStore {
    fn recover_terminal_ownership(
        &self,
        _state: &WorkspaceState,
        _base: SnapshotBase<'_>,
        _recovery: crate::persistence::TerminalPaneAdoption,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        Err(StoreError::InvalidState(
            "This store does not support terminal ownership recovery".into(),
        ))
    }
    fn preserve_chat_copy(
        &self,
        _copy: &crate::persistence::RetainedChatCopy,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        Err(StoreError::InvalidState(
            "This store does not support retained chat copies".into(),
        ))
    }

    #[cfg(feature = "ui")]
    fn rebind_authenticated_chat_upgrade(
        &self,
        _card_id: CardId,
        _recovery: &crate::chat::client::UpgradeRecovery,
    ) -> Result<(), StoreError> {
        Err(StoreError::InvalidState(
            "This store does not support authenticated chat recovery".into(),
        ))
    }
    fn rebind_chat_upgrade(
        &self,
        _card_id: CardId,
        _source: &crate::chat::Session,
        _replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        Err(StoreError::InvalidState(
            "This store does not support upgrade recovery".into(),
        ))
    }
    fn rebind_chat_account(
        &self,
        _card_id: CardId,
        _source: &crate::chat::Session,
        _replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        Err(StoreError::InvalidState(
            "This store does not support account recovery".into(),
        ))
    }
    fn replace_terminal_identity(
        &self,
        card_id: CardId,
        expected_terminal_id: TerminalId,
        replacement: &TerminalSession,
    ) -> Result<(), StoreError>;
    fn replace_chat_identity(
        &self,
        card_id: CardId,
        expected_id: TerminalId,
        replacement: &crate::chat::Session,
    ) -> Result<(), StoreError>;
    fn save_workspace(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError>;
    fn save_workspace_with_terminal_adoptions(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
        adoptions: &[crate::persistence::TerminalPaneAdoption],
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        if adoptions.is_empty() {
            self.save_workspace(state, base)
        } else {
            Err(StoreError::InvalidState(
                "This store does not support canonical pane adoption".into(),
            ))
        }
    }
    fn save_workspace_with_terminal_panes(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
        adoptions: &[crate::persistence::TerminalPaneAdoption],
        restorations: &[crate::persistence::TerminalPaneRestoration],
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        if restorations.is_empty() {
            self.save_workspace_with_terminal_adoptions(state, base, adoptions)
        } else {
            Err(StoreError::InvalidState(
                "This store does not support restored pane ownership".into(),
            ))
        }
    }
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
    fn preserve_chat_copy(
        &self,
        copy: &crate::persistence::RetainedChatCopy,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        StateStore::preserve_chat_copy(self, copy)
    }

    fn recover_terminal_ownership(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
        recovery: crate::persistence::TerminalPaneAdoption,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        StateStore::save_workspace_recovering_terminal_owner(self, state, base, recovery)
    }
    #[cfg(feature = "ui")]
    fn rebind_authenticated_chat_upgrade(
        &self,
        card_id: CardId,
        recovery: &crate::chat::client::UpgradeRecovery,
    ) -> Result<(), StoreError> {
        StateStore::rebind_authenticated_chat_upgrade(self, card_id, recovery)
    }
    fn rebind_chat_upgrade(
        &self,
        card_id: CardId,
        source: &crate::chat::Session,
        replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        StateStore::rebind_chat_upgrade(self, card_id, source, replacement)
    }
    fn rebind_chat_account(
        &self,
        card_id: CardId,
        source: &crate::chat::Session,
        replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        StateStore::rebind_chat_account(self, card_id, source, replacement)
    }
    fn replace_terminal_identity(
        &self,
        card_id: CardId,
        expected_terminal_id: TerminalId,
        replacement: &TerminalSession,
    ) -> Result<(), StoreError> {
        Self::replace_terminal_identity(self, card_id, expected_terminal_id, replacement)
    }

    fn replace_chat_identity(
        &self,
        card_id: CardId,
        expected_id: TerminalId,
        replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        Self::replace_chat_identity(self, card_id, expected_id, replacement)
    }

    fn save_workspace(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        Self::save_workspace(self, state, base)
    }

    fn save_workspace_with_terminal_adoptions(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
        adoptions: &[crate::persistence::TerminalPaneAdoption],
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        StateStore::save_workspace_with_terminal_adoptions(self, state, base, adoptions)
    }
    fn save_workspace_with_terminal_panes(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
        adoptions: &[crate::persistence::TerminalPaneAdoption],
        restorations: &[crate::persistence::TerminalPaneRestoration],
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        StateStore::save_workspace_with_terminal_panes(self, state, base, adoptions, restorations)
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
    fn preserve_chat_copy(
        &self,
        copy: &crate::persistence::RetainedChatCopy,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        StateStore::preserve_chat_copy(self, copy)
    }

    fn recover_terminal_ownership(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
        recovery: crate::persistence::TerminalPaneAdoption,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        StateStore::save_workspace_recovering_terminal_owner(self, state, base, recovery)
    }
    #[cfg(feature = "ui")]
    fn rebind_authenticated_chat_upgrade(
        &self,
        card_id: CardId,
        recovery: &crate::chat::client::UpgradeRecovery,
    ) -> Result<(), StoreError> {
        StateStore::rebind_authenticated_chat_upgrade(self, card_id, recovery)
    }
    fn rebind_chat_upgrade(
        &self,
        card_id: CardId,
        source: &crate::chat::Session,
        replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        StateStore::rebind_chat_upgrade(self, card_id, source, replacement)
    }
    fn rebind_chat_account(
        &self,
        card_id: CardId,
        source: &crate::chat::Session,
        replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        StateStore::rebind_chat_account(self, card_id, source, replacement)
    }
    fn replace_terminal_identity(
        &self,
        card_id: CardId,
        expected_terminal_id: TerminalId,
        replacement: &TerminalSession,
    ) -> Result<(), StoreError> {
        StateStore::replace_terminal_identity(self, card_id, expected_terminal_id, replacement)
    }
    fn replace_chat_identity(
        &self,
        card_id: CardId,
        expected_id: TerminalId,
        replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        StateStore::replace_chat_identity(self, card_id, expected_id, replacement)
    }
    fn save_workspace(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        StateStore::save_workspace(self, state, base)
    }
    fn save_workspace_with_terminal_adoptions(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
        adoptions: &[crate::persistence::TerminalPaneAdoption],
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        StateStore::save_workspace_with_terminal_adoptions(self, state, base, adoptions)
    }
    fn save_workspace_with_terminal_panes(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
        adoptions: &[crate::persistence::TerminalPaneAdoption],
        restorations: &[crate::persistence::TerminalPaneRestoration],
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        StateStore::save_workspace_with_terminal_panes(self, state, base, adoptions, restorations)
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
            identity_replacements: HashMap::new(),
            workspace_generation,
            card_generations,
            project_generations,
            worktree_generations,
            quarantine: Quarantine::default(),
            chat_rebindings: HashMap::new(),
            retained_chat_copies: HashMap::new(),
        }
    }

    /// Whether storage holds every pending write it can currently accept.
    ///
    /// A worktree row waiting on a project storage has no row for is not one
    /// of them: it is held back rather than failed, so it neither keeps a
    /// retry cycle going nor exhausts one. It lands in the same flush as its
    /// project, and a project still retrying keeps the lane unsettled on its
    /// own.
    fn is_settled(&self) -> bool {
        self.workspace.is_none()
            && self.conversions.is_empty()
            && self.projects.is_empty()
            && self.identity_replacements.is_empty()
            && self
                .worktrees
                .values()
                .all(|worktree| !self.project_generations.contains_key(&worktree.project_id))
    }

    /// Pending worktrees whose project storage has no row for, with nothing
    /// pending or refused that would give it one. No flush releases them. The
    /// writer never queues such a worktree; this names one if it ever does.
    fn orphaned_worktrees(&self) -> Vec<WorktreeId> {
        self.worktrees
            .values()
            .filter(|worktree| {
                let project = &worktree.project_id;
                !self.project_generations.contains_key(project)
                    && !self.projects.contains_key(project)
                    && !self.quarantine.projects.contains_key(project)
            })
            .map(|worktree| worktree.id)
            .collect()
    }

    /// The quarantine as the health lane reports it.
    fn quarantined(&self) -> Vec<QuarantinedWrite> {
        self.quarantine
            .report(&self.worktrees, &self.project_generations)
    }

    /// Offers every refused write to storage once more, for the reader who
    /// asked to retry. A subject with a newer write pending is already being
    /// retried by that write, so its refused one is not reinstated.
    fn rearm_quarantine(&mut self) {
        if let Some((snapshot, _, _)) = &self.quarantine.workspace
            && self.workspace.is_none()
            && self.conversions.is_empty()
        {
            self.workspace = Some(Arc::clone(snapshot));
        }
        for (card, (replacement, _)) in &self.quarantine.identity_replacements {
            self.identity_replacements
                .entry(*card)
                .or_insert_with(|| replacement.clone());
        }
        for (id, (project, _)) in &self.quarantine.projects {
            self.projects.entry(*id).or_insert_with(|| project.clone());
        }
        for (id, (worktree, _)) in &self.quarantine.worktrees {
            self.worktrees
                .entry(*id)
                .or_insert_with(|| worktree.clone());
        }
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
                ConversionCheckpoint::Snapshot(snapshot)
                | ConversionCheckpoint::PaneAdoptions { snapshot, .. }
                | ConversionCheckpoint::TerminalOwnershipRecovery { snapshot, .. } => {
                    Some(snapshot.id)
                }
                ConversionCheckpoint::RetainedChatCopy { copy, .. } => Some(copy.workspace),
                ConversionCheckpoint::IdentityReplacement(_)
                | ConversionCheckpoint::ChatRecovery(_) => None,
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

    fn translate_chat_rebindings(&self, snapshot: &mut Arc<WorkspaceState>) {
        let needs_translation = |state: &crate::model::CardState| {
            self.chat_rebindings.get(&state.id).is_some_and(
                |(source, _)| matches!(&state.kind, CardKind::Chat(session) if session == source),
            )
        };
        if !self.chat_rebindings.is_empty() && snapshot.cards.iter().any(needs_translation) {
            for state in &mut Arc::make_mut(snapshot).cards {
                if needs_translation(state) {
                    state.kind = CardKind::Chat(self.chat_rebindings[&state.id].1.clone());
                }
            }
        }
    }

    fn project_retained_chat_copies(
        &self,
        snapshot: &mut Arc<WorkspaceState>,
    ) -> Result<(), StoreError> {
        if !self.retained_chat_copies.is_empty() {
            let state = Arc::make_mut(snapshot);
            for copy in self.retained_chat_copies.values() {
                copy.project(state)?;
            }
        }
        Ok(())
    }

    fn acknowledge_retained_copy_closures(&mut self, snapshot: &WorkspaceState) {
        self.retained_chat_copies.retain(|id, _| {
            !snapshot
                .cards
                .iter()
                .any(|card| card.id == *id && !card.existence.is_open())
        });
    }

    /// Writes the conversion checkpoints in order, then the coalesced
    /// autosave, stopping at the first write that failed for a reason that may
    /// clear. That write goes back where it came from, so neither a retry nor
    /// shutdown can reorder the queue.
    ///
    /// A checkpoint storage refused by construction does not stop the ones
    /// after it. Each snapshot is the whole window and storage validates it on
    /// its own, so a later one is not built on the refused one; it is what
    /// replaces the refused content.
    fn flush_checkpoints(&mut self, store: &impl SessionStore) -> Result<(), StoreError> {
        loop {
            let autosave = self.conversions.is_empty();
            let Some(checkpoint) = self
                .conversions
                .pop_front()
                .or_else(|| self.workspace.take().map(ConversionCheckpoint::Snapshot))
            else {
                return Ok(());
            };
            let (checkpoint, adoptions, restorations) = match checkpoint {
                ConversionCheckpoint::PaneAdoptions {
                    snapshot,
                    adoptions,
                    restorations,
                } => (
                    ConversionCheckpoint::Snapshot(snapshot),
                    adoptions,
                    restorations,
                ),
                other => (other, Vec::new(), Vec::new()),
            };
            match checkpoint {
                ConversionCheckpoint::PaneAdoptions { .. } => unreachable!(),
                ConversionCheckpoint::TerminalOwnershipRecovery {
                    mut snapshot,
                    recovery,
                    reply,
                } => {
                    self.translate_chat_rebindings(&mut snapshot);
                    if let Err(error) = self.project_retained_chat_copies(&mut snapshot) {
                        let _ = reply.send(Err(error.to_string()));
                        continue;
                    }
                    let refused = self.quarantine.workspace.as_ref().filter(|(state, card, _)| {
                        state.id == snapshot.id
                            && *card == Some(recovery.canonical)
                            && state.cards.iter().any(|card| {
                                card.id == recovery.canonical
                                    && card.existence.is_open()
                                    && matches!(&card.kind, CardKind::Terminal(session) if session.id == recovery.terminal)
                            })
                    }).map(|(state, _, _)| Arc::clone(state));
                    let result = match refused {
                        None => Err("The exact refused terminal pane changed; review the current save notice.".into()),
                        Some(refused) => match store.recover_terminal_ownership(
                            &snapshot,
                            SnapshotBase {
                                workspace: self.workspace_generation,
                                cards: &self.card_generations,
                            },
                            recovery,
                        ) {
                            Ok(commit) => {
                                self.workspace_generation = commit.generation;
                                self.card_generations = commit.card_generations;
                                // An explicit Retry may have rearmed this old
                                // refused snapshot while the repair was queued.
                                // Retire only that offer, preserving a newer
                                // autosave that arrived during the repair.
                                if self.workspace.as_ref().is_some_and(|pending| Arc::ptr_eq(pending, &refused)) {
                                    self.workspace = None;
                                }
                                self.quarantine.snapshot_landed(&snapshot);
                                self.acknowledge_retained_copy_closures(&snapshot);
                                Ok(())
                            }
                            Err(error) => Err(error.to_string()),
                        },
                    };
                    if result.is_err() {
                        // Preserve the work captured by the reviewed action,
                        // but let ordinary identity validation guard every
                        // later retry. Only a new reviewed action can transfer.
                        self.conversions
                            .push_front(ConversionCheckpoint::Snapshot(snapshot));
                    }
                    // A failed reviewed action is one-shot. It cannot become
                    // an unreviewed later transfer after an I/O retry.
                    let _ = reply.send(result);
                }
                ConversionCheckpoint::RetainedChatCopy { copy, reply } => {
                    match store.preserve_chat_copy(&copy) {
                        Ok(commit) => {
                            self.workspace_generation = commit.generation;
                            self.card_generations = commit.card_generations;
                            self.retained_chat_copies.insert(copy.copy.id, copy);
                            if let Some(reply) = reply {
                                let _ = reply.send(Ok(()));
                            }
                        }
                        Err(error) => {
                            if let Some(reply) = reply {
                                let _ = reply.send(Err(error.to_string()));
                            }
                            if !error.refuses_content() {
                                // This operation owns inert bytes only. Its
                                // stable identity permits the usual bounded
                                // I/O retry and explicit Retry Saving safely.
                                self.conversions.push_front(
                                    ConversionCheckpoint::RetainedChatCopy { copy, reply: None },
                                );
                                return Err(error);
                            }
                        }
                    }
                }
                ConversionCheckpoint::ChatRecovery(recovery) => {
                    let result = match recovery.kind {
                        ChatRecoveryKind::Account => store.rebind_chat_account(
                            recovery.card,
                            &recovery.source,
                            &recovery.replacement,
                        ),
                        ChatRecoveryKind::Upgrade => store.rebind_chat_upgrade(
                            recovery.card,
                            &recovery.source,
                            &recovery.replacement,
                        ),
                        #[cfg(feature = "ui")]
                        ChatRecoveryKind::AuthenticatedUpgrade(witness) => {
                            if witness.source() != &recovery.source
                                || witness.replacement() != &recovery.replacement
                            {
                                Err(StoreError::InvalidState(
                                    "Authenticated recovery payload changed before commit".into(),
                                ))
                            } else {
                                store.rebind_authenticated_chat_upgrade(recovery.card, &witness)
                            }
                        }
                    };
                    if result.is_ok() {
                        self.chat_rebindings
                            .insert(recovery.card, (recovery.source, recovery.replacement));
                    }
                    // An explicit identity change is one-shot. In particular an
                    // I/O failure must not silently commit after the UI has
                    // refused activation and retired its candidate.
                    let _ = recovery
                        .reply
                        .send(result.map_err(|error| error.to_string()));
                }
                ConversionCheckpoint::IdentityReplacement(mut replacement) => {
                    self.quarantine.continue_refused(&mut replacement);
                    match replacement.apply(store) {
                        Ok(()) => self.quarantine.replacement_landed(replacement.card_id),
                        Err(error) if error.refuses_content() => {
                            self.quarantine.refuse_replacement(replacement, &error);
                        }
                        Err(error) => {
                            self.conversions
                                .push_front(ConversionCheckpoint::IdentityReplacement(replacement));
                            return Err(error);
                        }
                    }
                }
                ConversionCheckpoint::Snapshot(mut snapshot) => {
                    self.translate_chat_rebindings(&mut snapshot);
                    if let Err(error) = self.project_retained_chat_copies(&mut snapshot) {
                        self.quarantine.refuse_workspace(snapshot, &error);
                        continue;
                    }
                    match store.save_workspace_with_terminal_panes(
                        &snapshot,
                        SnapshotBase {
                            workspace: self.workspace_generation,
                            cards: &self.card_generations,
                        },
                        &adoptions,
                        &restorations,
                    ) {
                        Ok(commit) => {
                            self.workspace_generation = commit.generation;
                            self.card_generations = commit.card_generations;
                            self.quarantine.snapshot_landed(&snapshot);
                            self.acknowledge_retained_copy_closures(&snapshot);
                        }
                        Err(error) if error.refuses_content() => {
                            self.quarantine.refuse_workspace(snapshot, &error);
                        }
                        Err(error) => {
                            if autosave {
                                self.workspace = Some(snapshot);
                            } else {
                                self.conversions.push_front(
                                    if adoptions.is_empty() && restorations.is_empty() {
                                        ConversionCheckpoint::Snapshot(snapshot)
                                    } else {
                                        ConversionCheckpoint::PaneAdoptions {
                                            snapshot,
                                            adoptions,
                                            restorations,
                                        }
                                    },
                                );
                            }
                            return Err(error);
                        }
                    }
                }
            }
        }
    }

    fn reject_queued_terminal_recoveries(&mut self, error: &StoreError) {
        self.conversions = std::mem::take(&mut self.conversions).into_iter().map(|checkpoint| {
            match checkpoint {
                ConversionCheckpoint::TerminalOwnershipRecovery { snapshot, reply, .. } => {
                    let _ = reply.send(Err(format!(
                        "Recovery could not begin because an earlier session write failed: {error}. Retry save, then review recovery again."
                    )));
                    ConversionCheckpoint::Snapshot(snapshot)
                }
                other => other,
            }
        }).collect();
    }

    /// Applies everything still pending. A write that failed for a reason that
    /// may clear stays queued for a later attempt and is returned; one storage
    /// refused by construction moves to the quarantine instead, so it cannot
    /// fail every later attempt on behalf of writes that would land.
    fn flush(&mut self, store: &impl SessionStore) -> Vec<StoreError> {
        let mut errors = Vec::new();
        for (card_id, mut replacement) in std::mem::take(&mut self.identity_replacements) {
            self.quarantine.continue_refused(&mut replacement);
            match replacement.apply(store) {
                Ok(()) => self.quarantine.replacement_landed(card_id),
                Err(error) if error.refuses_content() => {
                    self.quarantine.refuse_replacement(replacement, &error);
                }
                Err(error) => {
                    errors.push(error);
                    self.identity_replacements.insert(card_id, replacement);
                }
            }
        }

        // Conversion checkpoints precede the coalesced autosave, and every one
        // of them follows the unordered replacements.
        if self.identity_replacements.is_empty()
            && let Err(error) = self.flush_checkpoints(store)
        {
            errors.push(error);
        }
        if let Some(error) = errors.first() {
            self.reject_queued_terminal_recoveries(error);
        }

        for (id, mut project) in std::mem::take(&mut self.projects) {
            project.generation = self.project_generations.get(&id).copied().unwrap_or(0);
            match store.save_project(&project) {
                Ok(generation) => {
                    self.project_generations.insert(id, generation);
                    self.quarantine.project_landed(id);
                }
                Err(error) if error.refuses_content() => {
                    self.quarantine.refuse_project(project, &error);
                }
                Err(error) => {
                    errors.push(error);
                    self.projects.insert(id, project);
                }
            }
        }

        // Projects go first because a worktree row references its project's
        // row. A worktree whose project storage still has no row waits for it,
        // whatever keeps the project out - a retry under way, a refusal, or a
        // project never saved - rather than failing on the missing parent: it
        // lands in the flush that lands its project.
        for (id, mut worktree) in std::mem::take(&mut self.worktrees) {
            if !self.project_generations.contains_key(&worktree.project_id) {
                self.worktrees.insert(id, worktree);
                continue;
            }
            worktree.generation = self.worktree_generations.get(&id).copied().unwrap_or(0);
            match store.save_worktree(&worktree) {
                Ok(generation) => {
                    self.worktree_generations.insert(id, generation);
                    self.quarantine.worktree_landed(id);
                }
                Err(error) if error.refuses_content() => {
                    self.quarantine.refuse_worktree(worktree, &error);
                }
                Err(error) => {
                    errors.push(error);
                    self.worktrees.insert(id, worktree);
                }
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
    /// The lane state an event repeats when only the quarantine changed.
    health: SessionSaveHealth,
    quarantined: Vec<QuarantinedWrite>,
}

impl HealthPublisher {
    fn new(sender: async_channel::Sender<SessionSaveHealthEvent>) -> Self {
        let mut publisher = Self {
            sender,
            revision: 0,
            last_successful_flush_at_unix_ms: None,
            incident: None,
            health: SessionSaveHealth::Healthy,
            quarantined: Vec::new(),
        };
        publisher.publish(SessionSaveHealth::Healthy);
        publisher
    }

    fn publish(&mut self, health: SessionSaveHealth) {
        self.revision = self.revision.saturating_add(1);
        let _ = self.sender.try_send(SessionSaveHealthEvent {
            revision: self.revision,
            health: health.clone(),
            last_successful_flush_at_unix_ms: self.last_successful_flush_at_unix_ms,
            quarantined: self.quarantined.clone(),
        });
        // Recovery is an edge, not a state. Repeating it beside a later
        // quarantine change would bring back a notice the reader already
        // acknowledged.
        self.health = match health {
            SessionSaveHealth::Recovered { .. } => SessionSaveHealth::Healthy,
            health => health,
        };
    }

    fn quarantine(&mut self, quarantined: Vec<QuarantinedWrite>) {
        if quarantined != self.quarantined {
            self.quarantined = quarantined;
            self.publish(self.health.clone());
        }
    }

    /// Answers an explicit retry that published nothing since `since`.
    ///
    /// The reader's notice waits for the next event before it offers a retry
    /// again, and a retry storage refuses the same way leaves the lane and the
    /// quarantine exactly as they were, which no other event would report.
    fn answer_retry(&mut self, since: u64) {
        if self.revision == since {
            self.publish(self.health.clone());
        }
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
            crate::diagnostics::record(crate::diagnostics::Event::PersistenceRecovered);
            self.publish(SessionSaveHealth::Recovered {
                recovered_at_unix_ms: at,
            });
        }
    }
}

fn save_failure_category(error: &StoreError) -> SaveFailureCategory {
    use rusqlite::ErrorCode;

    match error {
        StoreError::Card { error, .. } => save_failure_category(error),
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
            ErrorCode::ConstraintViolation => SaveFailureCategory::InvalidData,
            _ => SaveFailureCategory::Database,
        },
        StoreError::CorruptRow { .. }
        | StoreError::InvalidUuid { .. }
        | StoreError::Sqlite(
            rusqlite::Error::QueryReturnedNoRows
            | rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::IntegralValueOutOfRange(..),
        ) => SaveFailureCategory::Corrupt,
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
        StoreError::InvalidBusyTimeout { .. } | StoreError::MutexPoisoned | StoreError::Closed => {
            SaveFailureCategory::Database
        }
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
    Shutdown(crate::diagnostics::Event),
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
    crate::diagnostics::record(crate::diagnostics::Event::PersistenceStarted);
    let exit = {
        let _recovered = crate::instance::recovered_panic_scope();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            persistence_loop(store, writes, queue, last_error, &mut health, config)
        }))
    };
    if !matches!(exit, Ok(WriterExit::Shutdown(_))) {
        crate::diagnostics::record(crate::diagnostics::Event::WriterUnavailable);
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
    let mut oldest_change: Option<Instant> = None;
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
            && state.identity_replacements.is_empty()
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
            || !state.identity_replacements.is_empty();
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
        for replacement in std::mem::take(&mut state.identity_replacements).into_values() {
            merge_identity_replacement(&mut writes.identity_replacements, replacement);
        }
        let retry_requested = std::mem::take(&mut state.retry_requested);
        let retry_answered_after = retry_requested.then_some(health.revision);
        if retry_requested && !(writes.is_settled() && writes.quarantine.is_empty()) {
            // An explicit retry is also the one way a refused write is offered
            // to storage again: the reader may have fixed what storage refused.
            writes.rearm_quarantine();
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
                config,
                &mut oldest_change,
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
            health.quarantine(writes.quarantined());
            if writes.is_settled() {
                health.flushed();
                // Commits accumulate in the WAL, which SQLite never shrinks on
                // its own; this costs one `stat` per flush and only does real
                // work once the sidecar has grown past its threshold.
                store.checkpoint_wal_if_large();
                due = None;
                oldest_change = None;
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
                    crate::diagnostics::record(crate::diagnostics::Event::PersistenceFailed {
                        category: failure.unwrap_or(SaveFailureCategory::Database).into(),
                        exhausted: retry_exhausted,
                    });
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
        if let Some(revision) = retry_answered_after {
            health.answer_retry(revision);
        }
        for checkpoint in checkpoints {
            let result = if !writes.is_settled() {
                Err(save_refusal(failure))
            } else if let Some(category) = writes.quarantine.window_state() {
                // A barrier vouches for the window's state - a terminal handoff
                // releases the daemon's replay evidence on the strength of it -
                // so a refused snapshot fails it. A refused catalog row does
                // not: waiting cannot save it, and the window state is whole.
                let refusal = window_state_refusal(category, &writes.quarantined());
                tracing::warn!(%refusal, "a save barrier was refused");
                Err(refusal)
            } else {
                Ok(())
            };
            let _ = checkpoint.send(result);
        }

        if shutdown {
            // The steady-state lane retries a failed batch until it lands, so
            // the last flush of the session has to as well: the single attempt
            // this once made discarded the user's final snapshot the first time
            // storage was momentarily unwritable. Refused writes are not part
            // of that batch, so they cannot spend the budget either.
            let deadline = Instant::now() + config.shutdown_flush_budget;
            // The last flush of the session gets a recovery of its own,
            // whatever the steady-state lane already spent: losing the final
            // snapshot to a base that fell behind is exactly the loss this
            // retry loop exists to prevent.
            let mut shutdown_rebases = PERSIST_REBASES_PER_BATCH;
            while !writes.is_settled() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                // A failing batch is usually blocked behind a lock another
                // process holds, so retrying instantly would only burn the
                // budget; the steady-state backoff already paces that wait.
                thread::sleep(retry_delay.min(remaining));
                let errors = flush_rebasing_stale_bases(&mut writes, &store, &mut shutdown_rebases);
                if !errors.is_empty() {
                    failure = Some(select_failure_category(&errors));
                }
                record_flush_errors(&last_error, &errors);
                health.quarantine(writes.quarantined());
                if writes.is_settled() {
                    health.flushed();
                } else {
                    health.failed(&errors, false);
                }
                retry_delay = retry_delay.saturating_mul(2).min(config.retry_max);
            }
            let refused = writes.quarantined();
            if !refused.is_empty() {
                tracing::warn!(
                    refused = ?refused,
                    "the session ended with writes storage refused; they were not saved"
                );
            }
            let orphaned = writes.orphaned_worktrees();
            if !orphaned.is_empty() {
                tracing::warn!(
                    ?orphaned,
                    "the session ended with worktrees whose project was never saved; they were \
                     not saved"
                );
            }
            // `clean_shutdown` asserts that storage holds the window this
            // session ended with, so only a complete flush earns it; otherwise
            // the next open must recover rather than trust the database. A
            // refused catalog row does not undo that: it is lost either way,
            // and nothing a recovery reconciles depends on it.
            let mut clean_marker = false;
            if writes.is_settled() && writes.quarantine.window_state().is_none() {
                match store.shutdown() {
                    Ok(()) => clean_marker = true,
                    Err(error) => {
                        failure = Some(save_failure_category(&error));
                        if let Ok(mut message) = last_error.lock() {
                            *message = Some(error.to_string());
                        }
                    }
                }
            }
            let unsaved = usize::from(writes.workspace.is_some())
                .saturating_add(writes.conversions.len())
                .saturating_add(writes.projects.len())
                .saturating_add(writes.worktrees.len())
                .saturating_add(writes.identity_replacements.len());
            let complete = clean_marker && unsaved == 0 && refused.is_empty();
            let final_flush = crate::diagnostics::Event::FinalFlush {
                complete,
                unsaved: u32::try_from(unsaved).unwrap_or(u32::MAX),
                refused: u32::try_from(refused.len()).unwrap_or(u32::MAX),
                category: if complete {
                    None
                } else {
                    Some(
                        failure
                            .or_else(|| refused.first().map(|write| write.category))
                            .map(Into::into)
                            .unwrap_or(crate::diagnostics::Failure::Unknown),
                    )
                },
            };
            crate::diagnostics::record(final_flush);
            return WriterExit::Shutdown(final_flush);
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
    config: PersistenceConfig,
    oldest_change: &mut Option<Instant>,
) -> Option<Instant> {
    if received_changes && !retry_exhausted && attempts_this_cycle == 0 {
        // Preserve the trailing debounce for short bursts, but never move the
        // oldest pending batch's cap, even if this arrival is already overdue.
        // A failed flush keeps its existing retry deadline and backoff instead.
        let first = oldest_change.get_or_insert(now);
        Some((now + config.debounce).min(*first + config.max_delay))
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

/// Why a barrier cannot vouch for a window whose state storage refused.
///
/// It names each refused part of that state. The guidance asks the reader to
/// change or close "the item", and whoever reads this is often not looking at
/// the window - an updater relays it to a terminal - so without the names
/// there is nothing to act on.
fn window_state_refusal(category: SaveFailureCategory, refused: &[QuarantinedWrite]) -> String {
    let subjects = refused
        .iter()
        .filter(|write| write.subject.is_window_state())
        .map(|write| write.subject.summary())
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "workspace could not be saved: storage refused part of the window's own state ({}): \
         {subjects}. {}",
        category.label(),
        QUARANTINE_GUIDANCE
    )
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

    use rusqlite::{Connection, OptionalExtension};

    use tempfile::tempdir;

    use crate::model::{
        CardId, CardKind, CardState, Existence, Exposure, LaunchRecipe, Residency,
        TerminalContinuity, TerminalId, TerminalSession, Tombstone, WorkspaceId,
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
        /// A write storage refuses by construction.
        Refuse,
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
        /// Counts the clean shutdowns the lane recorded.
        shutdowns: Arc<AtomicUsize>,
        /// Counts every write the lane offered, whatever its outcome.
        attempts: Arc<AtomicUsize>,
        shutdown_failure: bool,
    }

    impl FakeStore {
        fn new(writes: impl IntoIterator<Item = FakeWrite>) -> Self {
            Self {
                writes: Mutex::new(writes.into_iter().collect()),
                exhausted: FakeWrite::Succeed,
                rebases: Arc::new(AtomicUsize::new(0)),
                shutdowns: Arc::new(AtomicUsize::new(0)),
                attempts: Arc::new(AtomicUsize::new(0)),
                shutdown_failure: false,
            }
        }

        fn always(write: FakeWrite) -> Self {
            Self {
                writes: Mutex::new(VecDeque::new()),
                exhausted: write,
                rebases: Arc::new(AtomicUsize::new(0)),
                shutdowns: Arc::new(AtomicUsize::new(0)),
                attempts: Arc::new(AtomicUsize::new(0)),
                shutdown_failure: false,
            }
        }

        fn write(&self) -> Result<(), StoreError> {
            self.attempts.fetch_add(1, Ordering::Relaxed);
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
                FakeWrite::Refuse => Err(StoreError::InvalidState("injected refusal".to_owned())),
                FakeWrite::Panic => panic!("injected writer panic"),
            }
        }
    }

    impl SessionStore for FakeStore {
        fn preserve_chat_copy(
            &self,
            copy: &crate::persistence::RetainedChatCopy,
        ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
            self.write()?;
            Ok(crate::persistence::WorkspaceCommit {
                generation: 1,
                card_generations: BTreeMap::from([(copy.source_card, 1), (copy.copy.id, 1)]),
            })
        }

        fn rebind_chat_upgrade(
            &self,
            _card_id: CardId,
            _source: &crate::chat::Session,
            _replacement: &crate::chat::Session,
        ) -> Result<(), StoreError> {
            self.write()
        }

        fn rebind_chat_account(
            &self,
            _card_id: CardId,
            _source: &crate::chat::Session,
            _replacement: &crate::chat::Session,
        ) -> Result<(), StoreError> {
            self.write()
        }

        fn replace_terminal_identity(
            &self,
            _card_id: CardId,
            _expected_terminal_id: TerminalId,
            _replacement: &TerminalSession,
        ) -> Result<(), StoreError> {
            self.write()
        }

        fn replace_chat_identity(
            &self,
            _card_id: CardId,
            _expected_id: TerminalId,
            _replacement: &crate::chat::Session,
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
            self.shutdowns.fetch_add(1, Ordering::Relaxed);
            if self.shutdown_failure {
                Err(StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "TOKEN=secret clean-marker error",
                )))
            } else {
                Ok(())
            }
        }
    }

    fn fast_persistence_config(max_automatic_attempts: u32) -> PersistenceConfig {
        PersistenceConfig {
            debounce: Duration::ZERO,
            max_delay: PERSIST_MAX_DELAY,
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
            unset_environment: Default::default(),
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
            chat_drafts: HashMap::new(),
            chat_draft_attachments: HashMap::new(),
            agent_updates: BTreeMap::new(),
            workspace_history: Vec::new(),
            repository_navigation: HashMap::new(),
            workspace_model_identity: None,
            local_only_cards: HashSet::new(),
            pending_chat_admissions: Default::default(),
            pending_terminal_creations: HashSet::new(),
            cards: vec![CardState {
                id: CardId::new(),
                generation: 0,
                title: "shell".into(),
                width_fraction: 0.7,
                existence: Existence::Open,
                exposure: Exposure::Visible,
                residency: Residency::Hot,
                kind: CardKind::Terminal(TerminalSession {
                    owned_codex_listener: None,
                    recovery_origin: None,
                    predecessor: None,
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
        let config = PersistenceConfig::default();
        let mut oldest_change = Some(now - Duration::from_millis(100));
        let old_debounce = now + Duration::from_millis(10);
        let new_debounce = now + Duration::from_millis(250);
        assert_eq!(
            schedule_flush_after_changes(
                Some(old_debounce),
                true,
                false,
                0,
                now,
                config,
                &mut oldest_change,
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
                config,
                &mut oldest_change,
            ),
            Some(retry_due)
        );
        assert_eq!(oldest_change, Some(now - Duration::from_millis(100)));
        assert_eq!(
            schedule_flush_after_changes(None, true, true, 4, now, config, &mut oldest_change,),
            None,
            "new changes must not restart an exhausted retry cycle"
        );
    }

    #[test]
    fn continuous_changes_cannot_postpone_the_oldest_pending_save() {
        let start = Instant::now();
        let config = PersistenceConfig::default();
        let mut oldest_change = None;
        let mut due = None;
        for elapsed_ms in [0, 200, 400, 600, 800, 1_000, 1_200] {
            let now = start + Duration::from_millis(elapsed_ms);
            due =
                schedule_flush_after_changes(due, true, false, 0, now, config, &mut oldest_change);
            assert_eq!(
                due,
                Some((now + config.debounce).min(start + config.max_delay))
            );
            assert_eq!(oldest_change, Some(start));
            if elapsed_ms >= 1_000 {
                assert!(due.unwrap() <= now, "an overdue arrival must still flush");
            }
        }
        let overdue = due;
        assert_eq!(
            schedule_flush_after_changes(
                due,
                false,
                false,
                0,
                start + Duration::from_secs(2),
                config,
                &mut oldest_change,
            ),
            overdue,
            "a timer wake must preserve the pending deadline"
        );
    }

    #[test]
    fn idle_wakes_do_not_start_a_save_batch() {
        let now = Instant::now();
        let config = PersistenceConfig::default();
        let mut oldest_change = None;
        assert_eq!(
            schedule_flush_after_changes(None, false, false, 0, now, config, &mut oldest_change),
            None
        );
        assert_eq!(oldest_change, None);
        let next = now + Duration::from_secs(10);
        assert_eq!(
            schedule_flush_after_changes(None, true, false, 0, next, config, &mut oldest_change),
            Some(next + config.debounce)
        );
        assert_eq!(oldest_change, Some(next));
    }

    #[test]
    fn continuous_snapshots_commit_before_the_producer_stops() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("continuous.sqlite3");
        let boot = BootId::new("continuous-save");
        let store = StateStore::open(&path, boot.clone()).unwrap();
        let mut state = workspace();
        let mut first = state.clone();
        first.cards[0].title = "ordered first".into();
        let mut second = state.clone();
        second.cards[0].title = "ordered second".into();
        let queue = Arc::new((
            Mutex::new(QueueState {
                conversions: VecDeque::from([
                    ConversionCheckpoint::Snapshot(Arc::new(first)),
                    ConversionCheckpoint::Snapshot(Arc::new(second)),
                ]),
                ..QueueState::default()
            }),
            Condvar::new(),
        ));
        let last_error = Arc::new(Mutex::new(None));
        let worker_queue = Arc::clone(&queue);
        let worker_error = Arc::clone(&last_error);
        let worker = thread::spawn(move || {
            let (health_tx, _health_rx) = async_channel::unbounded();
            persistence_loop(
                store,
                empty_writes(),
                worker_queue,
                worker_error,
                &mut HealthPublisher::new(health_tx),
                PersistenceConfig {
                    // A deadline shorter than this trailing gap proves the cap,
                    // even if a loaded test host pauses the producer briefly.
                    debounce: Duration::from_secs(30),
                    max_delay: Duration::from_millis(50),
                    ..PersistenceConfig::default()
                },
            )
        });
        let reader = Connection::open(&path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut committed_while_producing = false;
        let mut sequence = 0;
        while Instant::now() < deadline {
            state.cards[0].title = format!("streaming {sequence}");
            sequence += 1;
            {
                let mut pending = queue.0.lock().unwrap();
                pending.latest = Some(Arc::new(state.clone()));
                queue.1.notify_one();
            }
            let generation = reader
                .query_row(
                    "SELECT generation FROM workspaces WHERE workspace_id = ?1",
                    [state.id.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .unwrap();
            // Both ordered snapshots and a coalesced autosave must have
            // committed while updates continue, without a flush barrier.
            if generation.is_some_and(|generation| generation >= 3) {
                committed_while_producing = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        state.cards[0].title = "final snapshot".into();
        {
            let mut pending = queue.0.lock().unwrap();
            pending.latest = Some(Arc::new(state.clone()));
            pending.shutdown = true;
            queue.1.notify_one();
        }
        assert!(matches!(worker.join().unwrap(), WriterExit::Shutdown(_)));
        assert!(last_error.lock().unwrap().is_none());
        assert!(
            committed_while_producing,
            "continuous changes starved saves until the producer stopped"
        );
        let reopened = StateStore::open(&path, boot).unwrap();
        let restored = reopened.load_workspace(state.id, None).unwrap().unwrap();
        assert_eq!(restored.cards[0].title, "final snapshot");
        reopened.shutdown().unwrap();
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
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: vec!["-l".into()],
            working_directory: PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        let terminal = |id| TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
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

        merge_identity_replacement(
            &mut replacements,
            IdentityReplacement {
                card_id,
                expected_terminal_id: original_id,
                replacement: Replacement::Terminal(terminal(middle_id)),
            },
        );
        merge_identity_replacement(
            &mut replacements,
            IdentityReplacement {
                card_id,
                expected_terminal_id: middle_id,
                replacement: Replacement::Terminal(terminal(final_id)),
            },
        );

        let replacement = &replacements[&card_id];
        assert_eq!(replacement.expected_terminal_id, original_id);
        assert_eq!(replacement.replacement.id(), final_id);
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

    /// A chat card moved to another provider or account before its thread
    /// began saves as its new conversation, whether or not the lane is keeping
    /// conversions in order, and the next launch restores that conversation.
    #[test]
    fn a_replaced_chat_conversation_saves_with_its_snapshot_and_restores() {
        for ordered in [false, true] {
            let directory = tempdir().unwrap();
            let path = directory.path().join("chat-replacement.sqlite3");
            let boot = BootId::new("chat-replacement");
            let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
            let mut state = workspace();
            let card = state.cards[0].id;
            let CardKind::Terminal(terminal) = state.cards[0].kind.clone() else {
                unreachable!()
            };
            let chat = |provider: crate::agent::Provider, id: TerminalId| crate::chat::Session {
                native_continuity: Default::default(),
                owned_codex_listener: None,
                id,
                launch: crate::chat::Launch {
                    provider,
                    recipe: LaunchRecipe {
                        program: provider.executable().into(),
                        arguments: Vec::new(),
                        ..terminal.launch.clone()
                    },
                    session_id: None,
                },
                proof: None,
                terminal: None,
            };
            let codex = chat(crate::agent::Provider::Codex, TerminalId::new());
            state.cards[0].kind = CardKind::Chat(codex.clone());
            state.cards[0].title = "Codex chat".into();
            runtime.snapshot(Arc::new(state.clone()));
            runtime.flush(Duration::from_secs(10)).unwrap();
            if ordered {
                runtime.snapshot_conversion(Arc::new(state.clone()));
            }
            // An autosave still queued from before the switch.
            state.focused = Some(card);
            runtime.snapshot(Arc::new(state.clone()));

            let claude = chat(crate::agent::Provider::Claude, TerminalId::new());
            state.cards[0].kind = CardKind::Chat(claude.clone());
            state.cards[0].title = "Claude chat".into();
            runtime.replace_chat_identity(card, codex.id, claude.clone(), Arc::new(state.clone()));
            runtime.flush(Duration::from_secs(10)).unwrap();
            runtime.shutdown();
            assert_eq!(runtime.last_error(), None, "ordered: {ordered}");

            let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
            let restored = restored.unwrap();
            assert_eq!(
                restored.cards[0].kind,
                CardKind::Chat(claude),
                "ordered: {ordered}"
            );
            assert_eq!(restored.cards[0].title, "Claude chat");
            reopened.shutdown();
        }
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
            native_continuity: Default::default(),
            owned_codex_listener: None,
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
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
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
            kind: Default::default(),
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

    /// Worktree ids are derived from their path, so creating a worktree where a
    /// deleted one lived reuses the tombstoned id. That save used to be refused
    /// forever, exhausting the lane and stranding every catalog write after it
    /// (#245).
    #[test]
    fn worktree_recreated_at_a_deleted_path_reopens_its_catalog_row() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot = BootId::new("boot-recreate");
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let project = ProjectState {
            kind: Default::default(),
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
            path: PathBuf::from("/repo/cagents/worktrees/feature"),
            branch: Some("feature".into()),
            existence: Existence::Open,
        };
        let deleted = WorktreeState {
            existence: Existence::Closed {
                tombstone: crate::model::Tombstone {
                    operation_id: None,
                    closed_at_unix_ms: 1,
                    reason: Some("worktree deleted by user".into()),
                },
            },
            ..worktree.clone()
        };
        let timeout = Duration::from_secs(5);
        runtime.save_project(project);
        runtime.save_worktrees([worktree.clone()]);
        runtime.flush(timeout).unwrap();
        runtime.save_worktrees([deleted]);
        runtime.flush(timeout).unwrap();

        runtime.save_worktrees([worktree.clone()]);
        runtime.flush(timeout).unwrap();
        runtime.shutdown();
        assert_eq!(runtime.last_error(), None);

        let (mut reopened, _) = SessionRuntime::open(&path, boot).unwrap();
        let (_, worktrees) = reopened.take_catalog();
        assert_eq!(
            worktrees,
            vec![WorktreeState {
                generation: 3,
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
    fn final_clean_marker_failure_is_classified_even_with_no_pending_writes() {
        let mut store = FakeStore::new([]);
        store.shutdown_failure = true;
        let queue = Arc::new((
            Mutex::new(QueueState {
                shutdown: true,
                ..QueueState::default()
            }),
            Condvar::new(),
        ));
        let last_error = Arc::new(Mutex::new(None));
        let (health_tx, _health_rx) = async_channel::unbounded();
        let mut health = HealthPublisher::new(health_tx);
        let outcome = persistence_loop(
            store,
            PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
            queue,
            Arc::clone(&last_error),
            &mut health,
            fast_persistence_config(1),
        );
        assert_eq!(
            outcome,
            WriterExit::Shutdown(crate::diagnostics::Event::FinalFlush {
                complete: false,
                unsaved: 0,
                refused: 0,
                category: Some(crate::diagnostics::Failure::PermissionDenied)
            })
        );
        assert!(
            last_error
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .contains("TOKEN")
        );
        let WriterExit::Shutdown(event) = outcome else {
            panic!("writer failed unexpectedly");
        };
        assert!(!serde_json::to_string(&event).unwrap().contains("TOKEN"));
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

        let outcome = persistence_loop(
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
        assert!(matches!(
            outcome,
            WriterExit::Shutdown(crate::diagnostics::Event::FinalFlush {
                complete: false,
                unsaved: 1,
                refused: 0,
                category: Some(crate::diagnostics::Failure::Busy)
            })
        ));
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

        let outcome = persistence_loop(
            store,
            PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
            queue,
            Arc::clone(&last_error),
            &mut health,
            PersistenceConfig::default(),
        );
        release.join().unwrap();
        assert!(matches!(
            outcome,
            WriterExit::Shutdown(crate::diagnostics::Event::FinalFlush {
                complete: true,
                unsaved: 0,
                refused: 0,
                category: None,
            })
        ));
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

    fn project(name: &str) -> ProjectState {
        ProjectState {
            kind: Default::default(),
            id: ProjectId::new(),
            generation: 0,
            name: name.into(),
            repository: PathBuf::from("/repo").join(name),
            existence: Existence::Open,
        }
    }

    fn worktree(project: &ProjectState, name: &str) -> WorktreeState {
        WorktreeState {
            id: WorktreeId::new(),
            project_id: project.id,
            generation: 0,
            path: project.repository.join("worktrees").join(name),
            branch: Some(name.into()),
            existence: Existence::Open,
        }
    }

    fn closed() -> Existence {
        Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: Some("closed by the test".into()),
            },
        }
    }

    /// The last event the lane published so far.
    fn latest_health(
        receiver: &async_channel::Receiver<SessionSaveHealthEvent>,
    ) -> SessionSaveHealthEvent {
        let mut latest = None;
        while let Ok(event) = receiver.try_recv() {
            latest = Some(event);
        }
        latest.expect("the lane publishes an event for every change the test waits on")
    }

    /// The first event, up to a deadline, that satisfies `wanted`.
    fn health_until(
        receiver: &async_channel::Receiver<SessionSaveHealthEvent>,
        wanted: impl Fn(&SessionSaveHealthEvent) -> bool,
    ) -> SessionSaveHealthEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match receiver.try_recv() {
                Ok(event) if wanted(&event) => return event,
                Ok(_) => {}
                Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "the lane never reached the awaited health"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    fn foreign_key_violation() -> StoreError {
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
            Some("FOREIGN KEY constraint failed".into()),
        ))
    }

    #[derive(Debug, Clone, Copy)]
    enum ProjectWrite {
        Land,
        Busy,
        Refuse,
    }

    /// A catalog store that enforces the worktree -> project reference the way
    /// SQLite does, and scripts what each project's writes do. A project with
    /// no script left lands.
    #[derive(Default)]
    struct CatalogStore {
        projects: Mutex<HashMap<ProjectId, VecDeque<ProjectWrite>>>,
        stored_projects: Mutex<HashSet<ProjectId>>,
        saved_worktrees: Mutex<Vec<WorktreeId>>,
        missing_parents: AtomicUsize,
    }

    impl CatalogStore {
        fn new<const N: usize>(project: ProjectId, writes: [ProjectWrite; N]) -> Self {
            Self {
                projects: Mutex::new(HashMap::from([(project, writes.into())])),
                ..Self::default()
            }
        }

        fn saved_worktrees(&self) -> Vec<WorktreeId> {
            self.saved_worktrees.lock().unwrap().clone()
        }
    }

    impl SessionStore for CatalogStore {
        fn replace_terminal_identity(
            &self,
            _card_id: CardId,
            _expected_terminal_id: TerminalId,
            _replacement: &TerminalSession,
        ) -> Result<(), StoreError> {
            Ok(())
        }

        fn replace_chat_identity(
            &self,
            _card_id: CardId,
            _expected_id: TerminalId,
            _replacement: &crate::chat::Session,
        ) -> Result<(), StoreError> {
            Ok(())
        }

        fn save_workspace(
            &self,
            _state: &WorkspaceState,
            _base: SnapshotBase<'_>,
        ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
            Ok(crate::persistence::WorkspaceCommit::default())
        }

        fn save_project(&self, project: &ProjectState) -> Result<u64, StoreError> {
            let write = self
                .projects
                .lock()
                .unwrap()
                .get_mut(&project.id)
                .and_then(VecDeque::pop_front)
                .unwrap_or(ProjectWrite::Land);
            match write {
                ProjectWrite::Land => {
                    self.stored_projects.lock().unwrap().insert(project.id);
                    Ok(project.generation + 1)
                }
                ProjectWrite::Busy => Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                    None,
                ))),
                ProjectWrite::Refuse => Err(StoreError::InvalidState(format!(
                    "tombstoned project {} cannot be resurrected",
                    project.id
                ))),
            }
        }

        fn save_worktree(&self, worktree: &WorktreeState) -> Result<u64, StoreError> {
            if !self
                .stored_projects
                .lock()
                .unwrap()
                .contains(&worktree.project_id)
            {
                self.missing_parents.fetch_add(1, Ordering::Relaxed);
                return Err(foreign_key_violation());
            }
            self.saved_worktrees.lock().unwrap().push(worktree.id);
            Ok(worktree.generation + 1)
        }

        fn stored_generations(
            &self,
            _workspace: WorkspaceId,
        ) -> Result<StoredGenerations, StoreError> {
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

    fn empty_writes() -> PendingWrites {
        PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new())
    }

    #[test]
    fn chat_recovery_io_failure_is_acknowledged_without_a_later_retry() {
        for kind in [ChatRecoveryKind::Account, ChatRecoveryKind::Upgrade] {
            let store = FakeStore::new([FakeWrite::FailDiskFull, FakeWrite::Succeed]);
            let source = crate::chat::Session {
                id: TerminalId::new(),
                proof: None,
                terminal: None,
                native_continuity: Default::default(),
                owned_codex_listener: None,
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Codex,
                    session_id: Some("saved-conversation".into()),
                    recipe: LaunchRecipe {
                        program: "codex".into(),
                        arguments: Vec::new(),
                        working_directory: "/project".into(),
                        environment: BTreeMap::from([("CODEX_HOME".into(), "/removed".into())]),
                        unset_environment: Default::default(),
                    },
                },
            };
            let mut replacement = source.clone();
            replacement.id = TerminalId::new();
            replacement
                .launch
                .recipe
                .environment
                .insert("CODEX_HOME".into(), "/selected".into());
            let (reply, receipt) = std::sync::mpsc::sync_channel(1);
            let mut writes = empty_writes();
            writes
                .conversions
                .push_back(ConversionCheckpoint::ChatRecovery(ChatRecoveryWrite {
                    kind,
                    card: CardId::new(),
                    source,
                    replacement,
                    reply,
                }));
            assert!(
                writes.flush(&store).is_empty(),
                "failure belongs to the explicit request"
            );
            assert!(receipt.recv().unwrap().is_err());
            assert!(writes.is_settled());
            writes.rearm_quarantine();
            assert!(writes.flush(&store).is_empty());
            assert_eq!(
                store.attempts.load(Ordering::Relaxed),
                1,
                "a rejected candidate must never commit later"
            );
            assert!(writes.chat_rebindings.is_empty());
        }
    }

    #[test]
    fn upgrade_recovery_translates_queued_source_snapshots_before_the_next_save() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("session.db"), BootId::new("boot")).unwrap();
        let proof = |name| crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new(name),
            boot_id: BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new(name),
        };
        let source = crate::chat::Session {
            id: TerminalId::new(),
            proof: Some(proof("old")),
            terminal: None,
            native_continuity: crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                session_id: None,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: "/project".into(),
                    environment: BTreeMap::new(),
                    unset_environment: Default::default(),
                },
            },
        };
        let mut replacement = source.clone();
        replacement.proof = Some(proof("new"));
        // Upgrade recovery advances the same still-unnamed retained actor.
        let mut snapshot = workspace();
        let card = snapshot.cards[0].id;
        snapshot.cards[0].kind = CardKind::Chat(source.clone());
        let mut writes = empty_writes();
        writes.workspace = Some(Arc::new(snapshot.clone()));
        assert!(writes.flush(&store).is_empty());

        let (raw_reply, raw_receipt) = std::sync::mpsc::sync_channel(1);
        writes
            .conversions
            .push_back(ConversionCheckpoint::ChatRecovery(ChatRecoveryWrite {
                kind: ChatRecoveryKind::Upgrade,
                card,
                source: source.clone(),
                replacement: replacement.clone(),
                reply: raw_reply,
            }));
        assert!(writes.flush(&store).is_empty());
        assert!(
            raw_receipt.recv().unwrap().is_err(),
            "raw retained descriptors cannot authorize an upgrade or queued-snapshot translation"
        );
        assert!(writes.chat_rebindings.is_empty());
        assert_eq!(
            store.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(source.clone())
        );

        let handle = crate::chat::client::ClientHandle::fixture_upgrade_handshake(
            source.clone(),
            replacement.clone(),
            crate::chat::Snapshot {
                session_id: replacement.launch.session_id.clone(),
                ready: true,
                ..Default::default()
            },
        )
        .unwrap();
        let recovery = handle.authenticated_upgrade_recovery().unwrap().clone();
        assert!(recovery.validate());
        assert_eq!(recovery.source(), &source);
        assert_eq!(recovery.replacement(), &replacement);
        let (reply, receipt) = std::sync::mpsc::sync_channel(1);
        writes
            .conversions
            .push_back(ConversionCheckpoint::ChatRecovery(ChatRecoveryWrite {
                kind: ChatRecoveryKind::AuthenticatedUpgrade(recovery),
                card,
                source,
                replacement: replacement.clone(),
                reply,
            }));
        // An autosave produced while Ready awaits the receipt still carries
        // the old descriptor. It must not undo the authenticated handoff.
        snapshot.cards[0].title = "Saved during recovery".into();
        writes.workspace = Some(Arc::new(snapshot));
        assert!(writes.flush(&store).is_empty());
        receipt.recv().unwrap().unwrap();
        let saved = store.load_card(card, None).unwrap().unwrap();
        assert_eq!(saved.kind, CardKind::Chat(replacement));
        assert_eq!(saved.title, "Saved during recovery");
        assert!(writes.is_settled());
    }

    #[test]
    fn retained_copy_keeps_its_typed_payload_for_io_retry_but_not_content_refusal() {
        for transient in [false, true] {
            let mut state = workspace();
            let origin = state.cards[0].id;
            let source = crate::chat::Session {
                id: TerminalId::new(),
                proof: None,
                terminal: None,
                native_continuity: Default::default(),
                owned_codex_listener: None,
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Claude,
                    session_id: Some("saved-thread".into()),
                    recipe: LaunchRecipe {
                        program: "claude".into(),
                        arguments: Vec::new(),
                        working_directory: "/project".into(),
                        environment: BTreeMap::new(),
                        unset_environment: Default::default(),
                    },
                },
            };
            state.cards[0].id = CardId::new();
            state.cards[0].kind = CardKind::Chat(source.clone());
            let copy = crate::persistence::RetainedChatCopy {
                workspace: state.id,
                source_card: origin,
                source,
                copy: state.cards[0].clone(),
                draft: "Keep exact retry bytes".into(),
                attachments: Vec::new(),
                scope: None,
                channel: None,
            };
            let store = FakeStore::new([
                if transient {
                    FakeWrite::FailDiskFull
                } else {
                    FakeWrite::Refuse
                },
                FakeWrite::Succeed,
            ]);
            let (reply, receipt) = std::sync::mpsc::sync_channel(1);
            let mut writes = empty_writes();
            writes
                .conversions
                .push_back(ConversionCheckpoint::RetainedChatCopy {
                    copy: copy.clone(),
                    reply: Some(reply),
                });
            let failed = writes.flush(&store);
            assert_eq!(!failed.is_empty(), transient);
            assert!(
                receipt
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap()
                    .is_err()
            );
            drop(receipt);
            if transient {
                assert!(
                    matches!(writes.conversions.front(), Some(ConversionCheckpoint::RetainedChatCopy { copy: retained, reply: None }) if retained == &copy)
                );
                // The existing explicit retry path does not reconstruct the
                // failed canonical snapshot or a different source descriptor.
                writes.rearm_quarantine();
                assert!(writes.flush(&store).is_empty());
                assert_eq!(writes.retained_chat_copies[&copy.copy.id], copy);
                assert_eq!(store.attempts.load(Ordering::Relaxed), 2);
            } else {
                assert!(writes.is_settled());
                writes.rearm_quarantine();
                assert!(writes.flush(&store).is_empty());
                assert_eq!(store.attempts.load(Ordering::Relaxed), 1);
            }
        }
    }

    #[test]
    fn retained_copy_explicit_retry_commits_real_bytes_after_its_initial_error() {
        struct FailFirstCopy {
            store: Arc<StateStore>,
            attempts: Arc<AtomicUsize>,
        }
        impl SessionStore for FailFirstCopy {
            fn preserve_chat_copy(
                &self,
                copy: &crate::persistence::RetainedChatCopy,
            ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
                if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(StoreError::Io(std::io::Error::from_raw_os_error(
                        libc::ENOSPC,
                    )));
                }
                self.store.preserve_chat_copy(copy)
            }
            fn replace_terminal_identity(
                &self,
                card: CardId,
                expected: TerminalId,
                replacement: &TerminalSession,
            ) -> Result<(), StoreError> {
                self.store
                    .replace_terminal_identity(card, expected, replacement)
            }
            fn replace_chat_identity(
                &self,
                card: CardId,
                expected: TerminalId,
                replacement: &crate::chat::Session,
            ) -> Result<(), StoreError> {
                self.store
                    .replace_chat_identity(card, expected, replacement)
            }
            fn save_workspace(
                &self,
                state: &WorkspaceState,
                base: SnapshotBase<'_>,
            ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
                self.store.save_workspace(state, base)
            }
            fn save_project(&self, project: &ProjectState) -> Result<u64, StoreError> {
                self.store.save_project(project)
            }
            fn save_worktree(&self, worktree: &WorktreeState) -> Result<u64, StoreError> {
                self.store.save_worktree(worktree)
            }
            fn stored_generations(
                &self,
                workspace: WorkspaceId,
            ) -> Result<StoredGenerations, StoreError> {
                self.store.stored_generations(workspace)
            }
            fn stored_project_generation(
                &self,
                project: ProjectId,
            ) -> Result<Option<u64>, StoreError> {
                self.store.stored_project_generation(project)
            }
            fn stored_worktree_generation(
                &self,
                worktree: WorktreeId,
            ) -> Result<Option<u64>, StoreError> {
                self.store.stored_worktree_generation(worktree)
            }
            fn shutdown(&self) -> Result<(), StoreError> {
                self.store.shutdown()
            }
        }

        let directory = tempdir().unwrap();
        let path = directory.path().join("late-copy.db");
        let scope = directory.path().join("project");
        std::fs::create_dir(&scope).unwrap();
        let store = Arc::new(StateStore::open(&path, BootId::new("boot")).unwrap());
        let source = crate::chat::Session {
            id: TerminalId::new(),
            proof: Some(crate::model::HostSessionProof {
                host_session_id: "old".into(),
                boot_id: "boot".into(),
                host_incarnation: "old".into(),
            }),
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                session_id: Some("11111111-1111-4111-8111-111111111111".into()),
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: scope.clone(),
                    environment: BTreeMap::new(),
                    unset_environment: Default::default(),
                },
            },
        };
        let mut original = workspace();
        let origin = original.cards[0].id;
        original.cards[0].kind = CardKind::Chat(source.clone());
        original
            .chat_drafts
            .insert(origin, "Canonical draft stays here".into());
        let committed = store
            .save_workspace(
                &original,
                SnapshotBase {
                    workspace: 0,
                    cards: &BTreeMap::new(),
                },
            )
            .unwrap();
        let canonical_before = store.load_card(origin, None).unwrap().unwrap();
        let mut card = original.cards[0].clone();
        card.id = CardId::new();
        let copy = crate::persistence::RetainedChatCopy {
            workspace: original.id,
            source_card: origin,
            source: source.clone(),
            copy: card,
            draft: "Retry exact bytes  \nUnicode 🦀".into(),
            attachments: Vec::new(),
            scope: Some(scope.clone()),
            channel: None,
        };
        let (reply, receipt) = std::sync::mpsc::sync_channel(1);
        let queue = Arc::new((
            Mutex::new(QueueState {
                conversions: VecDeque::from([ConversionCheckpoint::RetainedChatCopy {
                    copy: copy.clone(),
                    reply: Some(reply),
                }]),
                ordered_identities: true,
                ..QueueState::default()
            }),
            Condvar::new(),
        ));
        let attempts = Arc::new(AtomicUsize::new(0));
        let wrapped = FailFirstCopy {
            store: Arc::clone(&store),
            attempts: Arc::clone(&attempts),
        };
        let (health_tx, health_rx) = async_channel::unbounded();
        let health = HealthPublisher::new(health_tx);
        let worker_queue = Arc::clone(&queue);
        let worker = thread::spawn(move || {
            run_persistence_worker(
                wrapped,
                PendingWrites::restored(
                    committed.generation,
                    committed.card_generations,
                    HashMap::new(),
                    HashMap::new(),
                ),
                worker_queue,
                Arc::new(Mutex::new(None)),
                health,
                fast_persistence_config(1),
            )
        });

        let first_receipt = receipt.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            first_receipt.is_err(),
            "the failed first attempt is never a successful receipt"
        );
        let failed = health_until(&health_rx, |event| {
            matches!(
                event.health,
                SessionSaveHealth::RetryExhausted {
                    category: SaveFailureCategory::DiskFull,
                    attempts: 1,
                    ..
                }
            )
        });
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(store.load_card(copy.copy.id, None).unwrap().is_none());
        assert_eq!(
            store.load_card(origin, None).unwrap().unwrap(),
            canonical_before
        );

        // This is the queue action used by Retry Saving. No second copy,
        // different descriptor, replacement snapshot or actor action is sent.
        let (lock, changed) = &*queue;
        lock.lock().unwrap().retry_requested = true;
        changed.notify_one();
        let recovered = health_until(&health_rx, |event| {
            matches!(event.health, SessionSaveHealth::Recovered { .. })
        });
        assert!(recovered.revision > failed.revision);
        assert!(recovered.last_successful_flush_at_unix_ms.is_some());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(
            matches!(
                receipt.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Disconnected)
            ),
            "late recovery is a health event, not a changed first receipt"
        );
        let saved = store.load_workspace(copy.workspace, None).unwrap().unwrap();
        assert_eq!(
            saved
                .cards
                .iter()
                .find(|card| card.id == copy.copy.id)
                .unwrap()
                .kind,
            CardKind::Chat(source.clone())
        );
        assert_eq!(saved.chat_drafts[&copy.copy.id], copy.draft);
        assert!(saved.local_only_cards.contains(&copy.copy.id));
        assert_eq!(saved.pending_chat_admissions[&copy.copy.id], source);
        assert_eq!(saved.card_scopes[&copy.copy.id], scope);
        assert_eq!(saved.chat_drafts[&origin], "Canonical draft stays here");
        assert_eq!(
            store.load_card(origin, None).unwrap().unwrap(),
            canonical_before
        );

        lock.lock().unwrap().shutdown = true;
        changed.notify_one();
        worker.join().unwrap();
        drop(store);
        let reopened = StateStore::open(&path, BootId::new("boot")).unwrap();
        let restored = reopened
            .load_workspace(copy.workspace, None)
            .unwrap()
            .unwrap();
        assert!(
            restored
                .cards
                .iter()
                .any(|card| card.id == copy.copy.id && card.existence.is_open())
        );
        assert_eq!(restored.chat_drafts[&copy.copy.id], copy.draft);
        assert!(restored.local_only_cards.contains(&copy.copy.id));
        assert_eq!(restored.pending_chat_admissions[&copy.copy.id], copy.source);
        assert_eq!(restored.card_scopes[&copy.copy.id], scope);
        assert_eq!(
            reopened.load_card(origin, None).unwrap().unwrap().kind,
            canonical_before.kind
        );
    }

    #[test]
    fn retained_copy_writer_survives_refused_canonical_snapshots_and_explicit_close() {
        for after_upgrade in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("retained-copy.db");
            let (mut runtime, _) = SessionRuntime::open(&path, BootId::new("boot")).unwrap();
            let source = crate::chat::Session {
                id: TerminalId::new(),
                proof: Some(crate::model::HostSessionProof {
                    host_session_id: "old".into(),
                    boot_id: "boot".into(),
                    host_incarnation: "old".into(),
                }),
                terminal: None,
                native_continuity: Default::default(),
                owned_codex_listener: None,
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Claude,
                    session_id: Some("11111111-1111-4111-8111-111111111111".into()),
                    recipe: LaunchRecipe {
                        program: "claude".into(),
                        arguments: Vec::new(),
                        working_directory: "/project".into(),
                        environment: BTreeMap::new(),
                        unset_environment: Default::default(),
                    },
                },
            };
            let mut original = workspace();
            let origin = original.cards[0].id;
            original.cards[0].kind = CardKind::Chat(source.clone());
            original
                .chat_drafts
                .insert(origin, "Canonical bytes".into());
            runtime.snapshot(Arc::new(original.clone()));
            runtime.flush(Duration::from_secs(5)).unwrap();
            let mut canonical = source.clone();
            if after_upgrade {
                canonical.proof.as_mut().unwrap().host_incarnation = "new".into();
                runtime
                    .rebind_chat_upgrade(origin, source.clone(), canonical.clone())
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
            }
            let before = runtime
                .operation_store
                .load_card(origin, None)
                .unwrap()
                .unwrap();
            let mut card = original.cards[0].clone();
            card.id = CardId::new();
            let copy = crate::persistence::RetainedChatCopy {
                workspace: original.id,
                source_card: origin,
                source: source.clone(),
                copy: card,
                draft: "Saved draft  \nUnicode 🦀".into(),
                attachments: Vec::new(),
                scope: None,
                channel: None,
            };
            // This newer shared actor is deliberately not an ordinary snapshot
            // identity change. Its refusal must not prevent copying local text.
            let mut refused = original.clone();
            let mut other = canonical.clone();
            other.id = TerminalId::new();
            other.launch.recipe.arguments.push("--other-recipe".into());
            refused.cards[0].kind = CardKind::Chat(other);
            runtime.snapshot_conversion(Arc::new(refused.clone()));
            runtime
                .preserve_chat_copy(copy.clone())
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(
                runtime
                    .operation_store
                    .load_card(origin, None)
                    .unwrap()
                    .unwrap(),
                before
            );
            assert_eq!(
                runtime
                    .operation_store
                    .load_workspace(original.id, None)
                    .unwrap()
                    .unwrap()
                    .chat_drafts[&copy.copy.id],
                copy.draft
            );
            runtime.snapshot(Arc::new(refused));
            assert!(runtime.flush(Duration::from_secs(5)).is_err());
            assert!(
                runtime
                    .operation_store
                    .load_card(copy.copy.id, None)
                    .unwrap()
                    .unwrap()
                    .existence
                    .is_open()
            );
            // A later stale autosave omits the newly retained copy. The writer
            // keeps the committed bytes, markers and navigation reachable.
            runtime.snapshot(Arc::new(original));
            runtime.flush(Duration::from_secs(5)).unwrap();
            let mut saved = runtime
                .operation_store
                .load_workspace(copy.workspace, None)
                .unwrap()
                .unwrap();
            assert_eq!(saved.chat_drafts[&copy.copy.id], copy.draft);
            assert_eq!(saved.pending_chat_admissions[&copy.copy.id], source);
            assert!(saved.local_only_cards.contains(&copy.copy.id));
            assert!(saved.card_scopes.contains_key(&copy.copy.id));
            assert_eq!(
                saved
                    .cards
                    .iter()
                    .find(|card| card.id == origin)
                    .unwrap()
                    .kind,
                CardKind::Chat(canonical.clone())
            );
            saved
                .cards
                .iter_mut()
                .find(|card| card.id == copy.copy.id)
                .unwrap()
                .existence = Existence::Closed {
                tombstone: crate::model::Tombstone {
                    operation_id: None,
                    closed_at_unix_ms: 1,
                    reason: Some("discarded retained text".into()),
                },
            };
            saved.chat_drafts.remove(&copy.copy.id);
            saved.pending_chat_admissions.remove(&copy.copy.id);
            saved.local_only_cards.remove(&copy.copy.id);
            saved.card_scopes.remove(&copy.copy.id);
            runtime.snapshot(Arc::new(saved));
            runtime.flush(Duration::from_secs(5)).unwrap();
            assert!(
                runtime
                    .preserve_chat_copy(copy.clone())
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .is_err()
            );
            runtime.shutdown();
            let reopened = StateStore::open(&path, BootId::new("boot")).unwrap();
            let restored = reopened
                .load_workspace(copy.workspace, None)
                .unwrap()
                .unwrap();
            assert!(
                !restored
                    .cards
                    .iter()
                    .find(|card| card.id == copy.copy.id)
                    .unwrap()
                    .existence
                    .is_open()
            );
            assert!(!restored.chat_drafts.contains_key(&copy.copy.id));
        }
    }

    #[test]
    fn only_a_refusal_of_the_content_itself_is_permanent() {
        let constraint = |extended_code| {
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(extended_code),
                None,
            ))
        };
        for permanent in [
            StoreError::InvalidState("tombstoned card cannot be resurrected".into()),
            StoreError::Serialization(serde_json::from_str::<u8>("x").unwrap_err()),
            constraint(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE),
            constraint(rusqlite::ffi::SQLITE_CONSTRAINT_CHECK),
            constraint(rusqlite::ffi::SQLITE_TOOBIG),
            foreign_key_violation(),
            StoreError::Card {
                card: CardId::new(),
                error: Box::new(StoreError::InvalidState("invalid conversion".into())),
            },
        ] {
            assert!(permanent.refuses_content(), "{permanent:?}");
        }
        for transient in [
            StoreError::CorruptRow {
                field: "card existence".into(),
                reason: "malformed JSON".into(),
            },
            StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows),
            constraint(rusqlite::ffi::SQLITE_BUSY),
            constraint(rusqlite::ffi::SQLITE_READONLY),
            constraint(rusqlite::ffi::SQLITE_FULL),
            constraint(rusqlite::ffi::SQLITE_CORRUPT),
            StoreError::Io(std::io::Error::from_raw_os_error(libc::ENOSPC)),
            StoreError::StaleGeneration {
                entity: "workspace".into(),
                expected: 1,
                actual: 2,
            },
            StoreError::UnsupportedSchema {
                found: 99,
                supported: 1,
            },
            StoreError::Closed,
            StoreError::Card {
                card: CardId::new(),
                error: Box::new(constraint(rusqlite::ffi::SQLITE_BUSY)),
            },
        ] {
            assert!(!transient.refuses_content(), "{transient:?}");
        }
        assert_eq!(
            save_failure_category(&constraint(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE)),
            SaveFailureCategory::InvalidData
        );
        assert_eq!(
            save_failure_category(&foreign_key_violation()),
            SaveFailureCategory::InvalidData
        );
    }

    #[test]
    fn corrupt_stored_rows_keep_valid_edits_pending_for_recovery() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("corrupt-row.sqlite3");
        let store = StateStore::open(&path, BootId::new("corrupt-row")).unwrap();
        let mut state = workspace();
        let mut writes = empty_writes();
        writes.workspace = Some(Arc::new(state.clone()));
        assert!(writes.flush(&store).is_empty());
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute("UPDATE cards SET existence_json = 'broken-json'", [])
            .unwrap();
        state.cards[0].title = "valid edit waiting for repair".into();
        writes.workspace = Some(Arc::new(state.clone()));
        let errors = writes.flush(&store);
        assert_eq!(errors.len(), 1);
        assert_eq!(
            save_failure_category(&errors[0]),
            SaveFailureCategory::Corrupt
        );
        assert!(!errors[0].refuses_content());
        assert!(writes.workspace.is_some(), "valid edits remain queued");
        assert!(
            writes.quarantine.workspace.is_none(),
            "corruption is not blamed on the edited card"
        );
        connection
            .execute(
                "UPDATE cards SET existence_json = ?1",
                [serde_json::to_string(&Existence::Open).unwrap()],
            )
            .unwrap();
        assert!(writes.flush(&store).is_empty());
        assert!(writes.workspace.is_none());
        assert_eq!(
            store.load_workspace(state.id, None).unwrap().unwrap().cards[0].title,
            state.cards[0].title
        );
        assert!(SaveFailureCategory::Corrupt.guidance().contains("repair"));
    }

    #[test]
    fn missing_or_undecodable_stored_values_are_corruption_not_refused_content() {
        let bad_id = "not-a-uuid";
        for error in [
            StoreError::InvalidUuid {
                kind: "card",
                value: bad_id.into(),
                source: bad_id.parse::<uuid::Uuid>().unwrap_err(),
            },
            StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows),
            StoreError::Sqlite(rusqlite::Error::InvalidColumnType(
                0,
                "card_id".into(),
                rusqlite::types::Type::Blob,
            )),
        ] {
            assert!(!error.refuses_content());
            assert_eq!(save_failure_category(&error), SaveFailureCategory::Corrupt);
        }
    }

    /// #268: a write storage refuses by construction used to exhaust the
    /// lane, after which nothing else the window produced was written.
    #[test]
    fn a_refused_catalog_write_is_set_aside_while_later_writes_land() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("refused-catalog.sqlite3");
        let boot = BootId::new("boot-refused-catalog");
        let timeout = Duration::from_secs(5);
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let health = runtime.health_receiver();
        let poisoned = project("poisoned");
        runtime.save_project(poisoned.clone());
        runtime.flush(timeout).unwrap();
        runtime.save_project(ProjectState {
            existence: closed(),
            ..poisoned.clone()
        });
        runtime.flush(timeout).unwrap();

        runtime.save_project(poisoned.clone());
        runtime
            .flush(timeout)
            .expect("a refused catalog row leaves the window state whole");
        let refused = latest_health(&health);
        assert_eq!(refused.health, SessionSaveHealth::Healthy);
        assert_eq!(refused.quarantined.len(), 1);
        assert_eq!(
            refused.quarantined[0].subject,
            WriteSubject::Project {
                id: poisoned.id,
                name: "poisoned".into(),
            }
        );
        assert_eq!(
            refused.quarantined[0].category,
            SaveFailureCategory::InvalidData
        );

        let unrelated = project("unrelated");
        let unrelated_worktree = worktree(&unrelated, "feature");
        let later = workspace();
        runtime.save_project(unrelated.clone());
        runtime.save_worktrees([unrelated_worktree.clone()]);
        runtime.snapshot(Arc::new(later.clone()));
        runtime.flush(timeout).unwrap();
        runtime.shutdown();
        assert_eq!(runtime.last_error(), None);

        let (mut reopened, restored) = SessionRuntime::open(&path, boot.clone()).unwrap();
        assert_eq!(restored.map(|state| state.id), Some(later.id));
        let (projects, worktrees) = reopened.take_catalog();
        assert!(projects.iter().any(|project| project.id == unrelated.id));
        assert!(
            projects
                .iter()
                .any(|project| project.id == poisoned.id && !project.existence.is_open()),
            "the refused resurrection never reached storage"
        );
        assert_eq!(
            worktrees
                .iter()
                .map(|worktree| worktree.id)
                .collect::<Vec<_>>(),
            vec![unrelated_worktree.id]
        );
        reopened.shutdown();
        let store = StateStore::open(&path, boot).unwrap();
        assert!(
            !store.recovery_report().dirty_shutdown,
            "a refused catalog row does not cost the session its clean shutdown"
        );
    }

    #[test]
    fn a_worktree_waits_for_its_project_instead_of_failing_on_it() {
        let parent = project("parent");
        let child = worktree(&parent, "child");
        let store = CatalogStore::new(parent.id, [ProjectWrite::Busy]);
        let mut writes = empty_writes();
        writes.projects.insert(parent.id, parent.clone());
        writes.worktrees.insert(child.id, child.clone());

        let errors = writes.flush(&store);
        assert_eq!(errors.len(), 1, "only the project failed: {errors:?}");
        assert_eq!(save_failure_category(&errors[0]), SaveFailureCategory::Busy);
        assert!(
            !writes.is_settled(),
            "the project's retry keeps the lane busy"
        );
        assert!(
            writes.quarantine.is_empty(),
            "a busy database refuses nothing"
        );
        assert!(writes.orphaned_worktrees().is_empty());

        assert!(writes.flush(&store).is_empty());
        assert!(writes.is_settled());
        assert_eq!(store.saved_worktrees(), vec![child.id]);
        assert_eq!(
            store.missing_parents.load(Ordering::Relaxed),
            0,
            "the lane never offers a worktree ahead of its project"
        );
    }

    #[test]
    fn only_the_worktrees_of_a_refused_project_are_held_back() {
        let refused = project("refused");
        let held = worktree(&refused, "held");
        let unrelated = project("unrelated");
        let landed = worktree(&unrelated, "landed");
        let store = CatalogStore::new(refused.id, [ProjectWrite::Refuse]);
        let mut writes = empty_writes();
        writes.projects.insert(refused.id, refused.clone());
        writes.worktrees.insert(held.id, held.clone());
        writes.projects.insert(unrelated.id, unrelated.clone());
        writes.worktrees.insert(landed.id, landed.clone());

        let errors = writes.flush(&store);
        assert!(errors.is_empty(), "a refusal is not a retry: {errors:?}");
        assert!(
            writes.is_settled(),
            "a held-back write neither retries nor exhausts the lane"
        );
        assert_eq!(
            writes.quarantined(),
            vec![QuarantinedWrite {
                subject: WriteSubject::Project {
                    id: refused.id,
                    name: "refused".into(),
                },
                category: SaveFailureCategory::InvalidData,
                refused_at_unix_ms: writes.quarantine.projects[&refused.id].1.at_unix_ms,
                held_back: 1,
            }]
        );
        assert_eq!(store.saved_worktrees(), vec![landed.id]);
        assert_eq!(store.missing_parents.load(Ordering::Relaxed), 0);
        assert!(
            writes.orphaned_worktrees().is_empty(),
            "the refusal explains the held-back worktree"
        );

        // A newer write of the refused project supersedes the refusal and
        // releases the worktree it held back, in the same flush.
        writes.projects.insert(refused.id, refused.clone());
        assert!(writes.flush(&store).is_empty());
        assert!(writes.is_settled());
        assert!(writes.quarantine.is_empty());
        assert_eq!(store.saved_worktrees(), vec![landed.id, held.id]);
        assert_eq!(store.missing_parents.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_worktree_whose_project_was_never_queued_is_named() {
        let unsaved = project("unsaved");
        let orphan = worktree(&unsaved, "orphan");
        let store = CatalogStore::default();
        let mut writes = empty_writes();
        writes.worktrees.insert(orphan.id, orphan.clone());

        assert!(writes.flush(&store).is_empty());
        assert!(writes.is_settled());
        assert!(store.saved_worktrees().is_empty());
        assert_eq!(writes.orphaned_worktrees(), vec![orphan.id]);
    }

    #[test]
    fn a_refused_update_of_a_stored_project_holds_nothing_back() {
        let parent = project("parent");
        let child = worktree(&parent, "child");
        let store = CatalogStore::new(parent.id, [ProjectWrite::Refuse]);
        store.stored_projects.lock().unwrap().insert(parent.id);
        let mut writes = PendingWrites::restored(
            0,
            BTreeMap::new(),
            HashMap::from([(parent.id, 1)]),
            HashMap::new(),
        );
        writes.projects.insert(parent.id, parent.clone());
        writes.worktrees.insert(child.id, child.clone());

        assert!(writes.flush(&store).is_empty());
        assert!(writes.is_settled());
        assert_eq!(store.saved_worktrees(), vec![child.id]);
        assert_eq!(writes.quarantined()[0].held_back, 0);
    }

    #[test]
    fn a_refused_checkpoint_does_not_stop_the_snapshots_after_it() {
        let store = FakeStore::new([FakeWrite::Refuse, FakeWrite::Succeed, FakeWrite::Succeed]);
        let attempts = Arc::clone(&store.attempts);
        let mut writes = empty_writes();
        writes
            .conversions
            .push_back(ConversionCheckpoint::Snapshot(Arc::new(workspace())));
        writes
            .conversions
            .push_back(ConversionCheckpoint::Snapshot(Arc::new(workspace())));
        writes.workspace = Some(Arc::new(workspace()));

        assert!(writes.flush(&store).is_empty());
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
        assert!(writes.is_settled());
        assert!(
            writes.quarantine.is_empty(),
            "a later snapshot that landed supersedes the refused one"
        );
    }

    #[test]
    fn a_refused_autosave_stays_reported_until_a_later_one_lands() {
        let store = FakeStore::new([FakeWrite::Refuse, FakeWrite::Refuse, FakeWrite::Succeed]);
        let mut writes = empty_writes();
        writes.workspace = Some(Arc::new(workspace()));
        assert!(writes.flush(&store).is_empty());
        assert!(writes.is_settled());
        let first = writes.quarantined();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].subject, WriteSubject::Workspace { card: None });
        assert_eq!(
            writes.quarantine.window_state(),
            Some(SaveFailureCategory::InvalidData)
        );

        // The reader's retry offers the refused snapshot again; refused
        // again, it keeps the time storage first refused it.
        writes.rearm_quarantine();
        assert!(!writes.is_settled());
        assert!(writes.flush(&store).is_empty());
        assert_eq!(writes.quarantined(), first);

        writes.workspace = Some(Arc::new(workspace()));
        assert!(writes.flush(&store).is_empty());
        assert!(writes.quarantine.is_empty());
        assert_eq!(writes.quarantine.window_state(), None);
    }

    #[test]
    fn a_refused_snapshot_fails_a_barrier_and_the_clean_shutdown() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("refused-workspace.sqlite3");
        let boot = BootId::new("boot-refused-workspace");
        let timeout = Duration::from_secs(5);
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let health = runtime.health_receiver();
        let mut state = workspace();
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();
        let card = state.cards.remove(0);
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();

        // Bringing a closed card back is refused by construction.
        let resurrected = card.id;
        state.cards.push(card);
        runtime.snapshot(Arc::new(state.clone()));
        let refusal = runtime
            .flush(timeout)
            .expect_err("a barrier cannot vouch for a window storage refused");
        assert!(refusal.contains("refused part of the window"), "{refusal}");
        assert!(
            refusal.contains(&format!(
                "the window's layout and panes, at pane {resurrected}"
            )),
            "a refused barrier names what the reader has to change: {refusal}"
        );
        assert!(refusal.contains(QUARANTINE_GUIDANCE), "{refusal}");
        let refused = latest_health(&health);
        assert_eq!(refused.health, SessionSaveHealth::Healthy);
        assert_eq!(
            refused
                .quarantined
                .iter()
                .map(|write| &write.subject)
                .collect::<Vec<_>>(),
            vec![&WriteSubject::Workspace {
                card: Some(resurrected)
            }]
        );

        // The catalog still saves beside it.
        let unrelated = project("unrelated");
        runtime.save_project(unrelated.clone());
        runtime.flush(timeout).unwrap_err();
        runtime.shutdown();

        let store = StateStore::open(&path, boot).unwrap();
        assert!(
            store.recovery_report().dirty_shutdown,
            "storage does not hold the window the session ended with"
        );
        assert!(
            store
                .list_projects()
                .unwrap()
                .iter()
                .any(|project| project.id == unrelated.id)
        );
    }

    #[test]
    fn ownership_recovery_unblocks_the_actual_update_flush_without_losing_unsaved_work() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("ownership.sqlite3");
        let boot = BootId::new("ownership-recovery");
        let timeout = Duration::from_secs(5);
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let health = runtime.health_receiver();
        let mut state = workspace();
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();
        state.cards[0].existence = Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: 773,
                reason: Some("closed by user".into()),
            },
        };
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();
        let mut live = state.cards[0].clone();
        live.id = CardId::new();
        live.title = "Unsaved terminal".into();
        live.existence = Existence::Open;
        let CardKind::Terminal(terminal) = &live.kind else {
            unreachable!()
        };
        let recovery = crate::persistence::TerminalPaneAdoption {
            alias: state.cards[0].id,
            canonical: live.id,
            terminal: terminal.id,
        };
        state.focused = Some(live.id);
        state.chat_drafts.insert(live.id, "refused draft".into());
        state.cards.push(live);
        runtime.snapshot(Arc::new(state.clone()));
        assert!(
            runtime
                .flush(timeout)
                .unwrap_err()
                .contains("refused part of the window")
        );
        let refused = latest_health(&health).quarantined;
        assert_eq!(
            refused[0].subject,
            WriteSubject::Workspace {
                card: Some(recovery.canonical)
            }
        );
        assert!(runtime.retry_save());
        assert!(runtime.flush(timeout).is_err());
        assert_eq!(latest_health(&health).quarantined, refused);
        state.chat_drafts.insert(
            recovery.canonical,
            "latest unsaved draft\nkept exactly".into(),
        );
        let receipt = runtime
            .recover_terminal_ownership(Arc::new(state.clone()), recovery)
            .unwrap();
        receipt.recv_timeout(timeout).unwrap().unwrap();
        runtime.flush(timeout).unwrap();
        assert!(latest_health(&health).quarantined.is_empty());
        runtime.shutdown();
        let store = StateStore::open(&path, boot).unwrap();
        assert!(!store.recovery_report().dirty_shutdown);
        let restored = store.load_workspace(state.id, None).unwrap().unwrap();
        assert_eq!(restored.chat_drafts, state.chat_drafts);
        assert_eq!(restored.focused, state.focused);
        assert_eq!(
            store.load_card(recovery.alias, None).unwrap().unwrap().kind,
            state.cards[0].kind
        );
        assert!(
            store
                .load_card(recovery.canonical, None)
                .unwrap()
                .unwrap()
                .existence
                .is_open()
        );
    }

    fn refused_ownership_writes(
        store: &StateStore,
    ) -> (
        PendingWrites,
        Arc<WorkspaceState>,
        crate::persistence::TerminalPaneAdoption,
    ) {
        let mut writes = empty_writes();
        let mut state = workspace();
        writes.workspace = Some(Arc::new(state.clone()));
        assert!(writes.flush(store).is_empty());
        state.cards[0].existence = Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: 773,
                reason: None,
            },
        };
        writes.workspace = Some(Arc::new(state.clone()));
        assert!(writes.flush(store).is_empty());
        let mut live = state.cards[0].clone();
        live.id = CardId::new();
        live.existence = Existence::Open;
        let CardKind::Terminal(terminal) = &live.kind else {
            unreachable!()
        };
        let recovery = crate::persistence::TerminalPaneAdoption {
            alias: state.cards[0].id,
            canonical: live.id,
            terminal: terminal.id,
        };
        state.focused = Some(live.id);
        state.cards.push(live);
        let snapshot = Arc::new(state);
        writes.workspace = Some(Arc::clone(&snapshot));
        assert!(writes.flush(store).is_empty());
        assert!(writes.quarantine.workspace.is_some());
        (writes, snapshot, recovery)
    }

    #[test]
    fn ownership_recovery_retires_only_the_old_rearmed_snapshot_and_preserves_newer_work() {
        for newer in [false, true] {
            let directory = tempdir().unwrap();
            let store = StateStore::open(
                directory.path().join("state.sqlite3"),
                BootId::new("rearmed"),
            )
            .unwrap();
            let (mut writes, refused, recovery) = refused_ownership_writes(&store);
            writes.rearm_quarantine();
            let unrelated = project("unrelated refusal");
            writes
                .quarantine
                .refuse_project(unrelated, &StoreError::InvalidState("unrelated".into()));
            let mut reviewed = (*refused).clone();
            reviewed
                .chat_drafts
                .insert(recovery.canonical, "reviewed draft".into());
            if newer {
                let mut latest = reviewed.clone();
                latest.chat_drafts.insert(
                    recovery.canonical,
                    "newer draft arriving during recovery".into(),
                );
                writes.workspace = Some(Arc::new(latest));
            }
            let (reply, receipt) = std::sync::mpsc::sync_channel(1);
            writes
                .conversions
                .push_back(ConversionCheckpoint::TerminalOwnershipRecovery {
                    snapshot: Arc::new(reviewed),
                    recovery,
                    reply,
                });
            assert!(writes.flush(&store).is_empty());
            receipt.recv().unwrap().unwrap();
            assert!(writes.quarantine.workspace.is_none());
            assert_eq!(writes.quarantine.projects.len(), 1);
            let saved = store.load_workspace(refused.id, None).unwrap().unwrap();
            assert_eq!(
                saved.chat_drafts[&recovery.canonical],
                if newer {
                    "newer draft arriving during recovery"
                } else {
                    "reviewed draft"
                }
            );
        }
    }

    #[test]
    fn failed_ownership_recovery_is_acknowledged_without_a_later_unreviewed_transfer() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let store = StateStore::open_with_options(
            &path,
            BootId::new("one-shot-recovery"),
            crate::persistence::StoreOptions {
                busy_timeout: Duration::from_millis(10),
            },
        )
        .unwrap();
        let (mut writes, snapshot, recovery) = refused_ownership_writes(&store);
        let refused = writes.quarantined();
        let blocker = Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (reply, receipt) = std::sync::mpsc::sync_channel(1);
        writes
            .conversions
            .push_back(ConversionCheckpoint::TerminalOwnershipRecovery {
                snapshot: Arc::clone(&snapshot),
                recovery,
                reply,
            });
        assert!(!writes.flush(&store).is_empty());
        assert!(receipt.recv().unwrap().is_err());
        assert_eq!(writes.quarantined(), refused);
        blocker.execute_batch("ROLLBACK").unwrap();
        assert!(writes.flush(&store).is_empty());
        assert_eq!(writes.quarantined(), refused);
        assert!(store.load_card(recovery.canonical, None).unwrap().is_none());
        let (reply, receipt) = std::sync::mpsc::sync_channel(1);
        writes
            .conversions
            .push_back(ConversionCheckpoint::TerminalOwnershipRecovery {
                snapshot,
                recovery,
                reply,
            });
        assert!(writes.flush(&store).is_empty());
        receipt.recv().unwrap().unwrap();
        assert!(writes.quarantine.workspace.is_none());
    }

    #[test]
    fn an_earlier_io_failure_rejects_queued_recovery_but_retains_its_current_work() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("queued-recovery.sqlite3");
        let store = StateStore::open_with_options(
            &path,
            BootId::new("queued-recovery"),
            crate::persistence::StoreOptions {
                busy_timeout: Duration::from_millis(10),
            },
        )
        .unwrap();
        let (mut writes, refused, recovery) = refused_ownership_writes(&store);
        let mut current = (*refused).clone();
        current
            .chat_drafts
            .insert(recovery.canonical, "new work captured by the review".into());
        let blocker = Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        writes
            .conversions
            .push_back(ConversionCheckpoint::Snapshot(Arc::clone(&refused)));
        let (reply, receipt) = std::sync::mpsc::sync_channel(1);
        writes
            .conversions
            .push_back(ConversionCheckpoint::TerminalOwnershipRecovery {
                snapshot: Arc::new(current),
                recovery,
                reply,
            });
        assert!(!writes.flush(&store).is_empty());
        assert!(
            receipt
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap_err()
                .contains("earlier session write failed")
        );
        assert!(writes.conversions.iter().all(|checkpoint| !matches!(
            checkpoint,
            ConversionCheckpoint::TerminalOwnershipRecovery { .. }
        )));
        blocker.execute_batch("ROLLBACK").unwrap();
        assert!(writes.flush(&store).is_empty());
        assert!(store.load_card(recovery.canonical, None).unwrap().is_none());
        let (latest, card, _) = writes.quarantine.workspace.as_ref().unwrap();
        assert_eq!(*card, Some(recovery.canonical));
        assert_eq!(
            latest.chat_drafts[&recovery.canonical],
            "new work captured by the review"
        );
    }

    #[test]
    fn writer_commits_canonical_terminal_adoption_and_later_snapshots() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("canonical-adoption.sqlite3");
        let boot = BootId::new("boot-canonical-adoption");
        let timeout = Duration::from_secs(5);
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let health = runtime.health_receiver();
        let mut state = workspace();
        let alias = state.cards[0].id;
        let CardKind::Terminal(session) = &state.cards[0].kind else {
            panic!("terminal fixture")
        };
        let terminal = session.id;
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();

        let mut canonical = state.cards[0].clone();
        canonical.id = CardId::new();
        let canonical_id = canonical.id;
        state.cards[0].existence = closed();
        state.cards.push(canonical);
        state.focused = Some(canonical_id);
        runtime.snapshot_terminal_adoptions(
            Arc::new(state.clone()),
            vec![crate::persistence::TerminalPaneAdoption {
                alias,
                canonical: canonical_id,
                terminal,
            }],
        );
        runtime.flush(timeout).unwrap();
        assert!(latest_health(&health).quarantined.is_empty());

        state.cards[1].title = "canonical shell".into();
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();
        runtime.shutdown();
        let store = StateStore::open(&path, boot).unwrap();
        assert!(!store.recovery_report().dirty_shutdown);
        let restored = store.load_workspace(state.id, None).unwrap().unwrap();
        assert_eq!(restored.focused, Some(canonical_id));
        assert!(restored.cards.iter().any(|card| card.id == canonical_id
            && card.title == "canonical shell"
            && card.existence.is_open()));
        assert!(
            !store
                .load_card(alias, None)
                .unwrap()
                .unwrap()
                .existence
                .is_open()
        );
    }

    #[test]
    fn writer_resolves_terminal_restoration_after_pending_closure_and_before_later_snapshots() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("canonical-restoration.sqlite3");
        let boot = BootId::new("boot-canonical-restoration");
        let timeout = Duration::from_secs(5);
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let health = runtime.health_receiver();
        let mut state = workspace();
        let alias = state.cards[0].id;
        let CardKind::Terminal(session) = &state.cards[0].kind else {
            panic!("terminal fixture")
        };
        let terminal = session.id;
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();

        let mut canonical = state.cards[0].clone();
        canonical.id = CardId::new();
        let canonical_id = canonical.id;
        state.cards[0].existence = closed();
        state.focused = None;
        // This close may still be coalesced when restoration arrives. The
        // checkpoint must commit it before resolving the actual recipe owner.
        runtime.snapshot(Arc::new(state.clone()));
        state.cards.push(canonical);
        state.focused = Some(canonical_id);
        let restoration = crate::persistence::TerminalPaneRestoration {
            canonical: canonical_id,
            terminal,
        };
        runtime.snapshot_terminal_panes(Arc::new(state.clone()), Vec::new(), vec![restoration]);
        state.cards[1].title = "canonical shell".into();
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();
        assert!(latest_health(&health).quarantined.is_empty());
        // Repeated model observations are idempotent even when the historical
        // closed alias still exists in the snapshot.
        runtime.snapshot_terminal_panes(
            Arc::new(state.clone()),
            Vec::new(),
            vec![restoration, restoration],
        );
        runtime.flush(timeout).unwrap();
        // A successful idempotent checkpoint does not change save health, so
        // the lane need not publish another event. The barrier still refuses
        // quarantined writes and errors, and disk assertions below verify it.
        assert!(runtime.last_error().is_none());
        while let Ok(event) = health.try_recv() {
            assert_eq!(event.health, SessionSaveHealth::Healthy);
            assert!(event.quarantined.is_empty());
        }
        runtime.shutdown();
        let store = StateStore::open(&path, boot).unwrap();
        assert!(!store.recovery_report().dirty_shutdown);
        let restored = store.load_workspace(state.id, None).unwrap().unwrap();
        assert_eq!(restored.focused, Some(canonical_id));
        assert!(restored.cards.iter().any(|card| card.id == canonical_id
            && card.title == "canonical shell"
            && card.existence.is_open()));
        assert!(
            !store
                .load_card(alias, None)
                .unwrap()
                .unwrap()
                .existence
                .is_open()
        );
    }

    #[test]
    fn a_later_snapshot_supersedes_a_refused_one() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("superseded-workspace.sqlite3");
        let boot = BootId::new("boot-superseded-workspace");
        let timeout = Duration::from_secs(5);
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let health = runtime.health_receiver();
        let mut state = workspace();
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();
        let card = state.cards.remove(0);
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();
        state.cards.push(card);
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap_err();

        state.cards.clear();
        state.focused = None;
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();
        assert!(latest_health(&health).quarantined.is_empty());
        runtime.shutdown();

        let store = StateStore::open(&path, boot).unwrap();
        assert!(!store.recovery_report().dirty_shutdown);
    }

    /// A barrier refuses for what is unsaved now, not for what was once
    /// refused: the refused snapshot is the whole window, so the first later
    /// one storage accepts is everything the barrier vouches for. An update is
    /// such a barrier, and one it blocks for good cannot deliver the fix.
    #[test]
    fn a_barrier_passes_once_the_window_saves_after_a_refused_focus() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("refused-focus.sqlite3");
        let boot = BootId::new("boot-refused-focus");
        let timeout = Duration::from_secs(5);
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let health = runtime.health_receiver();
        let mut state = workspace();
        state.focused = Some(state.cards[0].id);
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();

        // Storage refuses a window that focuses a pane it holds closed.
        state.cards[0].existence = closed();
        runtime.snapshot(Arc::new(state.clone()));
        let refusal = runtime.flush(timeout).unwrap_err();
        assert!(
            refusal.contains("(saved state is invalid): the window's layout and panes."),
            "{refusal}"
        );
        // Asking again changes nothing: the same window is still unsaved.
        runtime.flush(timeout).unwrap_err();

        state.focused = None;
        runtime.snapshot(Arc::new(state));
        runtime.flush(timeout).unwrap();
        assert!(latest_health(&health).quarantined.is_empty());
        runtime.flush(timeout).unwrap();
        runtime.shutdown();

        let store = StateStore::open(&path, boot).unwrap();
        assert!(!store.recovery_report().dirty_shutdown);
    }

    /// A subject refused again keeps the time of its first refusal, and the
    /// lane can still tell that the reason it is refused for has changed.
    #[test]
    fn a_refusal_for_a_new_reason_is_told_apart_from_a_repeated_one() {
        let refused = |reason: &str| StoreError::InvalidState(reason.to_owned());
        let mut quarantine = Quarantine::default();
        quarantine.refuse_workspace(Arc::new(workspace()), &refused("first"));
        let (_, _, first) = quarantine.workspace.clone().unwrap();
        quarantine.refuse_workspace(Arc::new(workspace()), &refused("first"));
        let (_, _, repeated) = quarantine.workspace.clone().unwrap();
        quarantine.refuse_workspace(Arc::new(workspace()), &refused("second"));
        let (_, _, changed) = quarantine.workspace.clone().unwrap();

        assert_eq!(repeated.cause, first.cause);
        assert_ne!(changed.cause, first.cause);
        assert_eq!(changed.at_unix_ms, first.at_unix_ms);
    }

    /// A relaunch storage refused leaves the stored identity where it was, and
    /// the next relaunch continues from the refused one's identity. Without the
    /// merge the second replacement could only be refused for expecting an
    /// identity storage never held, and every snapshot carrying it after that.
    #[test]
    fn a_relaunch_after_a_refused_one_starts_from_the_stored_identity() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("refused-relaunch.sqlite3");
        let boot = BootId::new("boot-refused-relaunch");
        let timeout = Duration::from_secs(5);
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let health = runtime.health_receiver();
        let mut state = workspace();
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();
        let card_id = state.cards[0].id;
        let CardKind::Terminal(original) = state.cards[0].kind.clone() else {
            unreachable!()
        };
        let relaunch = |id| TerminalSession {
            owned_codex_listener: None,
            id,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: original.launch.clone(),
                last_exit: None,
            },
            ..original.clone()
        };

        // Storage refuses a relaunch whose recipe does not match its launch.
        let mut refused = relaunch(TerminalId::new());
        refused.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: LaunchRecipe {
                program: "/bin/false".into(),
                ..original.launch.clone()
            },
            last_exit: None,
        };
        runtime.replace_terminal_identity(card_id, original.id, refused.clone());
        runtime.flush(timeout).unwrap_err();
        assert_eq!(
            latest_health(&health)
                .quarantined
                .iter()
                .map(|write| &write.subject)
                .collect::<Vec<_>>(),
            vec![&WriteSubject::CardIdentity {
                card: card_id,
                kind: ReplacedIdentity::Terminal,
            }]
        );

        let latest = relaunch(TerminalId::new());
        runtime.replace_terminal_identity(card_id, refused.id, latest.clone());
        state.cards[0].kind = CardKind::Terminal(latest.clone());
        runtime.snapshot(Arc::new(state));
        runtime.flush(timeout).unwrap();
        assert!(latest_health(&health).quarantined.is_empty());
        runtime.shutdown();

        let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
        assert_eq!(restored.unwrap().cards[0].kind, CardKind::Terminal(latest));
        reopened.shutdown();
    }

    #[test]
    fn closing_a_card_drops_its_refused_relaunch() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("closed-refused-relaunch.sqlite3");
        let boot = BootId::new("boot-closed-refused-relaunch");
        let timeout = Duration::from_secs(5);
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let health = runtime.health_receiver();
        let mut state = workspace();
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();
        let card_id = state.cards[0].id;
        let CardKind::Terminal(original) = state.cards[0].kind.clone() else {
            unreachable!()
        };
        // Storage refuses a relaunch whose recipe does not match its launch.
        let refused = TerminalSession {
            owned_codex_listener: None,
            id: TerminalId::new(),
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: LaunchRecipe {
                    program: "/bin/false".into(),
                    ..original.launch.clone()
                },
                last_exit: None,
            },
            ..original.clone()
        };
        runtime.replace_terminal_identity(card_id, original.id, refused.clone());
        runtime.flush(timeout).unwrap_err();

        // A snapshot that still holds the card at the identity storage kept
        // lands, and the relaunch it never took stays refused.
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap_err();
        assert_eq!(
            latest_health(&health)
                .quarantined
                .iter()
                .map(|write| &write.subject)
                .collect::<Vec<_>>(),
            vec![&WriteSubject::CardIdentity {
                card: card_id,
                kind: ReplacedIdentity::Terminal,
            }]
        );

        // The window moves on to the relaunch, and storage refuses every
        // snapshot that rebinds the card to it.
        state.cards[0].kind = CardKind::Terminal(refused);
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap_err();
        assert_eq!(
            latest_health(&health)
                .quarantined
                .iter()
                .map(|write| &write.subject)
                .collect::<Vec<_>>(),
            vec![
                &WriteSubject::Workspace {
                    card: Some(card_id)
                },
                &WriteSubject::CardIdentity {
                    card: card_id,
                    kind: ReplacedIdentity::Terminal,
                },
            ]
        );

        // Closing the card, as the notice says, stores its tombstone over the
        // identity storage kept, and leaves no identity to replace.
        state.cards[0].existence = closed();
        runtime.snapshot(Arc::new(state));
        runtime.flush(timeout).unwrap();
        assert!(latest_health(&health).quarantined.is_empty());
        runtime.shutdown();

        let store = StateStore::open(&path, boot).unwrap();
        assert!(!store.recovery_report().dirty_shutdown);
        let stored = store.load_card(card_id, None).unwrap().unwrap();
        assert!(!stored.existence.is_open());
        assert!(matches!(stored.kind, CardKind::Terminal(session) if session.id == original.id));
    }

    #[test]
    fn a_card_first_saved_at_its_new_identity_drops_its_refused_relaunch() {
        let directory = tempdir().unwrap();
        let path = directory
            .path()
            .join("first-saved-refused-relaunch.sqlite3");
        let boot = BootId::new("boot-first-saved-refused-relaunch");
        let timeout = Duration::from_secs(5);
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let health = runtime.health_receiver();
        let mut state = workspace();
        let card_id = state.cards[0].id;
        let CardKind::Terminal(original) = state.cards[0].kind.clone() else {
            unreachable!()
        };
        // The terminal exits before its first snapshot, and storage refuses
        // the relaunch's content before it ever looks for the card.
        let fresh_id = TerminalId::new();
        runtime.replace_terminal_identity(
            card_id,
            original.id,
            TerminalSession {
                owned_codex_listener: None,
                id: fresh_id,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe: LaunchRecipe {
                        program: "/bin/false".into(),
                        ..original.launch.clone()
                    },
                    last_exit: None,
                },
                ..original.clone()
            },
        );
        runtime.flush(timeout).unwrap_err();
        assert_eq!(latest_health(&health).quarantined.len(), 1);

        // The first snapshot stores the card at the fresh identity, which
        // leaves the refused relaunch nothing to replace.
        state.cards[0].kind = CardKind::Terminal(TerminalSession {
            owned_codex_listener: None,
            id: fresh_id,
            ..original
        });
        runtime.snapshot(Arc::new(state));
        runtime.flush(timeout).unwrap();
        assert!(latest_health(&health).quarantined.is_empty());
        runtime.shutdown();

        let store = StateStore::open(&path, boot).unwrap();
        assert!(!store.recovery_report().dirty_shutdown);
    }

    #[test]
    fn closing_a_chat_card_drops_its_refused_conversation() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("closed-refused-conversation.sqlite3");
        let boot = BootId::new("boot-closed-refused-conversation");
        let timeout = Duration::from_secs(5);
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let health = runtime.health_receiver();
        let mut state = workspace();
        let card = state.cards[0].id;
        let CardKind::Terminal(terminal) = state.cards[0].kind.clone() else {
            unreachable!()
        };
        let chat = |session_id: Option<&str>| crate::chat::Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: LaunchRecipe {
                    program: crate::agent::Provider::Codex.executable().into(),
                    arguments: Vec::new(),
                    ..terminal.launch.clone()
                },
                session_id: session_id.map(str::to_owned),
            },
            proof: None,
            terminal: None,
        };
        let original = chat(None);
        state.cards[0].kind = CardKind::Chat(original.clone());
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(timeout).unwrap();

        // Storage refuses a replacement conversation that has already begun.
        // The window still shows the conversation storage holds.
        runtime.replace_chat_identity(
            card,
            original.id,
            chat(Some("begun")),
            Arc::new(state.clone()),
        );
        runtime.flush(timeout).unwrap_err();
        assert_eq!(
            latest_health(&health)
                .quarantined
                .iter()
                .map(|write| &write.subject)
                .collect::<Vec<_>>(),
            vec![&WriteSubject::CardIdentity {
                card,
                kind: ReplacedIdentity::Chat,
            }]
        );

        state.cards[0].existence = closed();
        runtime.snapshot(Arc::new(state));
        runtime.flush(timeout).unwrap();
        assert!(latest_health(&health).quarantined.is_empty());
        runtime.shutdown();
    }

    #[test]
    fn a_retry_storage_refuses_again_is_still_answered() {
        let store = FakeStore::always(FakeWrite::Refuse);
        let attempts = Arc::clone(&store.attempts);
        let refused = project("refused");
        let queue = Arc::new((
            Mutex::new(QueueState {
                projects: HashMap::from([(refused.id, refused)]),
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
                empty_writes(),
                worker_queue,
                Arc::new(Mutex::new(None)),
                health,
                fast_persistence_config(2),
            );
        });
        let quarantined = health_until(&health_rx, |event| !event.quarantined.is_empty());

        let (lock, changed) = &*queue;
        lock.lock().unwrap().retry_requested = true;
        changed.notify_one();
        // The reader's notice waits on this event to offer a retry again.
        let answer = health_until(&health_rx, |event| event.revision > quarantined.revision);
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
        assert_eq!(answer.health, SessionSaveHealth::Healthy);
        assert_eq!(answer.quarantined, quarantined.quarantined);

        lock.lock().unwrap().shutdown = true;
        changed.notify_one();
        worker.join().unwrap();
    }

    #[test]
    fn shutdown_skips_refused_writes() {
        for (window_state, clean) in [(false, true), (true, false)] {
            let store = FakeStore::always(FakeWrite::Refuse);
            let attempts = Arc::clone(&store.attempts);
            let shutdowns = Arc::clone(&store.shutdowns);
            let queue = Arc::new((
                Mutex::new(QueueState {
                    latest: window_state.then(|| Arc::new(workspace())),
                    projects: HashMap::from_iter((!window_state).then(|| {
                        let project = project("refused");
                        (project.id, project)
                    })),
                    shutdown: true,
                    ..QueueState::default()
                }),
                Condvar::new(),
            ));
            let (health_tx, _health_rx) = async_channel::unbounded();
            let mut health = HealthPublisher::new(health_tx);
            let started = Instant::now();

            persistence_loop(
                store,
                empty_writes(),
                queue,
                Arc::new(Mutex::new(None)),
                &mut health,
                PersistenceConfig {
                    // Long enough that spending it would time the test out.
                    shutdown_flush_budget: Duration::from_secs(60),
                    ..fast_persistence_config(2)
                },
            );

            assert!(started.elapsed() < Duration::from_secs(5));
            assert_eq!(
                attempts.load(Ordering::Relaxed),
                1,
                "a refused write is offered once, not retried until the budget runs out"
            );
            assert_eq!(shutdowns.load(Ordering::Relaxed), usize::from(clean));
        }
    }

    #[test]
    fn a_retry_offers_refused_writes_to_storage_again() {
        let store = FakeStore::new([FakeWrite::Refuse, FakeWrite::Succeed]);
        let attempts = Arc::clone(&store.attempts);
        let refused = project("refused");
        let queue = Arc::new((
            Mutex::new(QueueState {
                projects: HashMap::from([(refused.id, refused.clone())]),
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
                empty_writes(),
                worker_queue,
                Arc::new(Mutex::new(None)),
                health,
                fast_persistence_config(2),
            );
        });

        let quarantined = health_until(&health_rx, |event| !event.quarantined.is_empty());
        assert_eq!(quarantined.health, SessionSaveHealth::Healthy);
        // Nothing retries a refusal on its own.
        thread::sleep(Duration::from_millis(50));
        assert_eq!(attempts.load(Ordering::Relaxed), 1);

        let (lock, changed) = &*queue;
        lock.lock().unwrap().retry_requested = true;
        changed.notify_one();
        let cleared = health_until(&health_rx, |event| event.quarantined.is_empty());
        assert_eq!(cleared.health, SessionSaveHealth::Healthy);
        assert_eq!(attempts.load(Ordering::Relaxed), 2);

        lock.lock().unwrap().shutdown = true;
        changed.notify_one();
        worker.join().unwrap();
    }

    #[test]
    fn a_quarantine_change_never_repeats_a_recovery() {
        let (health_tx, health_rx) = async_channel::unbounded();
        let mut publisher = HealthPublisher::new(health_tx);
        publisher.failed(&[StoreError::Closed], false);
        publisher.flushed();
        let _ = latest_health(&health_rx);

        let refused = QuarantinedWrite {
            subject: WriteSubject::Workspace { card: None },
            category: SaveFailureCategory::InvalidData,
            refused_at_unix_ms: 1,
            held_back: 0,
        };
        publisher.quarantine(vec![refused.clone()]);
        let event = latest_health(&health_rx);
        assert_eq!(event.health, SessionSaveHealth::Healthy);
        assert_eq!(event.quarantined, vec![refused.clone()]);

        publisher.quarantine(vec![refused]);
        assert!(
            health_rx.try_recv().is_err(),
            "an unchanged quarantine publishes nothing"
        );
    }
}
