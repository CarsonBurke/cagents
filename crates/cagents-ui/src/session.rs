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
// Exported at the shared library's root so its expansion names that crate.
use cagents::invalid_state;

const PERSIST_DEBOUNCE: Duration = Duration::from_millis(250);
// A busy window must keep saving even when changes never leave a debounce gap.
const PERSIST_MAX_DELAY: Duration = Duration::from_secs(1);
/// Original changes represented by a prepared snapshot, before capture waited
/// out the ordinary autosave debounce. These times never imply a manual retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotChanges {
    pub first: Instant,
    pub latest: Instant,
}

impl SnapshotChanges {
    pub fn at(now: Instant) -> Self {
        Self {
            first: now,
            latest: now,
        }
    }
    pub fn merge(self, other: Self) -> Self {
        Self {
            first: self.first.min(other.first),
            latest: self.latest.max(other.latest),
        }
    }
    pub fn deadline(self) -> Instant {
        (self.latest + PERSIST_DEBOUNCE).min(self.first + PERSIST_MAX_DELAY)
    }
}

/// A complete immutable snapshot and the original autosave debt it carries.
/// Constructing this value does not acknowledge or enqueue the snapshot.
pub struct SnapshotOffer {
    pub state: Arc<WorkspaceState>,
    pub changes: Option<SnapshotChanges>,
}

impl From<Arc<WorkspaceState>> for SnapshotOffer {
    fn from(state: Arc<WorkspaceState>) -> Self {
        Self {
            state,
            changes: None,
        }
    }
}

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
     repeats, report the rule it broke, which cagents diagnostics keeps, and the database path.";

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
    /// The latest content of one open pane. The rest of the window saved
    /// around it; storage keeps what it last held for this pane.
    Pane {
        card: CardId,
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
        matches!(
            self,
            Self::Workspace { .. } | Self::Pane { .. } | Self::CardIdentity { .. }
        )
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
            Self::Pane { card } => format!("the latest changes to pane {card}"),
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

    /// The subject as private diagnostics record it: its kind and identifier.
    fn diagnostic(&self) -> (crate::diagnostics::RefusedWrite, Option<uuid::Uuid>) {
        use crate::diagnostics::RefusedWrite;

        match self {
            Self::Workspace { card } => (RefusedWrite::Workspace, card.map(CardId::as_uuid)),
            Self::Pane { card } => (RefusedWrite::Pane, Some(card.as_uuid())),
            Self::CardIdentity {
                card,
                kind: ReplacedIdentity::Terminal,
            } => (RefusedWrite::RelaunchedTerminal, Some(card.as_uuid())),
            Self::CardIdentity {
                card,
                kind: ReplacedIdentity::Chat,
            } => (RefusedWrite::NewConversation, Some(card.as_uuid())),
            Self::Project { id, .. } => (RefusedWrite::Project, Some(id.as_uuid())),
            Self::Worktree { id, .. } => (RefusedWrite::Worktree, Some(id.as_uuid())),
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
    /// The rule storage refused the write for, in words that name none of
    /// its content: see [`crate::persistence::RefusedCheck`].
    pub check: crate::diagnostics::Check,
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

/// A chat card whose shared view holds conversation `canonical` where this
/// window last saved conversation `expected`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatIdentityAdoption {
    pub card: CardId,
    pub expected: TerminalId,
    pub canonical: crate::chat::Session,
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
    /// The conversation the card's shared view now holds, which another
    /// client put in place of the one this window last saved.
    SharedChat(crate::chat::Session),
}

impl Replacement {
    fn id(&self) -> TerminalId {
        match self {
            Self::Terminal(session) => session.id,
            Self::Chat(session) | Self::SharedChat(session) => session.id,
        }
    }

    /// Whether both rebind the same kind of payload, so one can continue the
    /// other's chain. Who authorized each step does not matter: a chain is
    /// applied under the authority of its newest step.
    fn same_kind(&self, other: &Self) -> bool {
        self.kind() == other.kind()
    }

    fn kind(&self) -> ReplacedIdentity {
        match self {
            Self::Terminal(_) => ReplacedIdentity::Terminal,
            Self::Chat(_) | Self::SharedChat(_) => ReplacedIdentity::Chat,
        }
    }
}

impl IdentityReplacement {
    /// Whether a card of `kind` still holds the payload this replacement
    /// would rebind, so it still has something to replace.
    fn replaces(&self, kind: &CardKind) -> bool {
        let held = match (&self.replacement, kind) {
            (Replacement::Terminal(_), CardKind::Terminal(session)) => session.id,
            (Replacement::Chat(_) | Replacement::SharedChat(_), CardKind::Chat(session)) => {
                session.id
            }
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
            Replacement::SharedChat(session) => {
                store.adopt_chat_identity(self.card_id, self.expected_terminal_id, session)
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
    Resume,
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
    snapshot_changes: Option<SnapshotChanges>,
    conversions: VecDeque<ConversionCheckpoint>,
    ordered_identities: bool,
    projects: HashMap<ProjectId, ProjectState>,
    worktrees: HashMap<WorktreeId, WorktreeState>,
    identity_replacements: HashMap<CardId, IdentityReplacement>,
    /// Conversations shared views re-pointed chat cards to, waiting for the
    /// write that shows them. The writer never takes them on its own: they
    /// are queued ahead of that write under its lock, so the writer can pair
    /// them neither with an older state that still shows the replaced
    /// conversations nor with a state that shows a change queued after them.
    staged_adoptions: Vec<IdentityReplacement>,
    retry_requested: bool,
    checkpoints: Vec<std::sync::mpsc::SyncSender<std::result::Result<(), String>>>,
    shutdown: bool,
}

impl QueueState {
    fn record_snapshot_changes(&mut self, changes: Option<SnapshotChanges>) {
        let changes = changes.unwrap_or_else(|| SnapshotChanges::at(Instant::now()));
        self.snapshot_changes = Some(
            self.snapshot_changes
                .map_or(changes, |pending| pending.merge(changes)),
        );
    }
    /// Queues `replacement` behind everything already offered: after the
    /// pending snapshot while conversions are kept in order, merged with the
    /// other pending replacements otherwise.
    fn queue_identity_replacement(&mut self, replacement: IdentityReplacement) {
        if self.ordered_identities {
            if let Some(previous) = self.latest.take() {
                self.conversions
                    .push_back(ConversionCheckpoint::Snapshot(previous));
            }
            self.conversions
                .push_back(ConversionCheckpoint::IdentityReplacement(replacement));
        } else {
            merge_identity_replacement(&mut self.identity_replacements, replacement);
        }
    }

    /// Switches the lane to keeping writes in order, behind everything
    /// already offered: the merged replacements, which arrived with the
    /// pending snapshot that shows them, then that snapshot, then the staged
    /// adoptions, which only the write being queued shows.
    fn keep_writes_in_order(&mut self) {
        self.ordered_identities = true;
        for replacement in std::mem::take(&mut self.identity_replacements).into_values() {
            self.conversions
                .push_back(ConversionCheckpoint::IdentityReplacement(replacement));
        }
        if let Some(previous) = self.latest.take() {
            self.conversions
                .push_back(ConversionCheckpoint::Snapshot(previous));
        }
        self.admit_staged_adoptions();
    }

    /// Queues the staged adoptions ahead of the write being queued.
    fn admit_staged_adoptions(&mut self) {
        for adoption in std::mem::take(&mut self.staged_adoptions) {
            self.queue_identity_replacement(adoption);
        }
    }
}

/// Opaque queue incarnation, independent of a potentially reused StateStore.
#[derive(Debug, Clone)]
pub struct WriterIdentity(std::sync::Weak<(Mutex<QueueState>, Condvar)>);

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
    #[cfg(all(test, not(feature = "integrated-browser")))]
    pub(crate) fn poison_queue_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.queue.0.lock().unwrap();
            panic!("test poisoned session queue");
        }));
        self.queue.1.notify_all();
    }

    pub fn writer_identity(&self) -> WriterIdentity {
        WriterIdentity(Arc::downgrade(&self.queue))
    }
    pub fn owns_writer(&self, identity: &WriterIdentity) -> bool {
        identity.0.ptr_eq(&Arc::downgrade(&self.queue))
    }

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
        let _ = self.offer_snapshot(state.into());
    }

    pub fn offer_snapshot(&self, offer: SnapshotOffer) -> Result<(), String> {
        let changes = offer
            .changes
            .or_else(|| Some(SnapshotChanges::at(Instant::now())));
        let (lock, changed) = &*self.queue;
        let mut queue = lock.lock().map_err(|_| "Session writer is unavailable")?;
        if queue.shutdown {
            return Err("Session writer is shutting down".into());
        }
        queue.admit_staged_adoptions();
        queue.record_snapshot_changes(changes);
        let previous = queue.latest.replace(offer.state);
        changed.notify_one();
        drop(queue);
        drop(previous);
        Ok(())
    }

    /// Preserve a kind transition and all preceding metadata on the writer
    /// lane. Unlike autosave, conversions cannot coalesce: a fast return to a
    /// terminal still needs the exact chat identity committed before it.
    pub fn snapshot_conversion(&self, state: Arc<WorkspaceState>) {
        let _ = self.offer_conversion(state.into());
    }

    pub fn offer_conversion(&self, offer: SnapshotOffer) -> Result<(), String> {
        let changes = offer
            .changes
            .or_else(|| Some(SnapshotChanges::at(Instant::now())));
        let (lock, changed) = &*self.queue;
        let mut queue = lock.lock().map_err(|_| "Session writer is unavailable")?;
        if queue.shutdown {
            return Err("Session writer is shutting down".into());
        }
        queue.keep_writes_in_order();
        queue.record_snapshot_changes(changes);
        queue
            .conversions
            .push_back(ConversionCheckpoint::Snapshot(offer.state));
        changed.notify_one();
        Ok(())
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
        let _ = self.offer_terminal_panes(state.into(), adoptions, restorations);
    }

    pub fn offer_terminal_panes(
        &self,
        offer: SnapshotOffer,
        adoptions: Vec<crate::persistence::TerminalPaneAdoption>,
        restorations: Vec<crate::persistence::TerminalPaneRestoration>,
    ) -> Result<(), String> {
        let changes = offer
            .changes
            .or_else(|| Some(SnapshotChanges::at(Instant::now())));
        let (lock, changed) = &*self.queue;
        let mut queue = lock.lock().map_err(|_| "Session writer is unavailable")?;
        if queue.shutdown {
            return Err("Session writer is shutting down".into());
        }
        queue.keep_writes_in_order();
        queue.record_snapshot_changes(changes);
        queue
            .conversions
            .push_back(ConversionCheckpoint::PaneAdoptions {
                snapshot: offer.state,
                adoptions,
                restorations,
            });
        changed.notify_one();
        Ok(())
    }

    /// Queue a reviewed ownership repair after every already-offered write.
    /// The receipt succeeds only after the full snapshot commits; neither
    /// enqueueing nor a failed attempt retires its save quarantine.
    pub fn recover_terminal_ownership(
        &self,
        snapshot: impl Into<SnapshotOffer>,
        recovery: crate::persistence::TerminalPaneAdoption,
    ) -> std::result::Result<std::sync::mpsc::Receiver<std::result::Result<(), String>>, String>
    {
        let offer = snapshot.into();
        let changes = offer
            .changes
            .or_else(|| Some(SnapshotChanges::at(Instant::now())));
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        let (lock, changed) = &*self.queue;
        let mut queue = lock.lock().map_err(|_| "Session writer is unavailable")?;
        if queue.shutdown {
            return Err("Session writer is shutting down".into());
        }
        queue.keep_writes_in_order();
        queue.record_snapshot_changes(changes);
        queue
            .conversions
            .push_back(ConversionCheckpoint::TerminalOwnershipRecovery {
                snapshot: offer.state,
                recovery,
                reply,
            });
        changed.notify_one();
        Ok(receiver)
    }

    pub fn save_project(&self, project: ProjectState) {
        let change_time = SnapshotChanges::at(Instant::now());
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            queue.projects.insert(project.id, project);
            queue.record_snapshot_changes(Some(change_time));
            changed.notify_one();
        }
    }

    pub fn save_worktrees(&self, worktrees: impl IntoIterator<Item = WorktreeState>) {
        let change_time = SnapshotChanges::at(Instant::now());
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            let mut inserted = false;
            for worktree in worktrees {
                inserted = true;
                queue.worktrees.insert(worktree.id, worktree);
            }
            if inserted {
                queue.record_snapshot_changes(Some(change_time));
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
        let _ = self.offer_chat_identity(card_id, expected_id, replacement, state.into());
    }

    pub fn offer_chat_identity(
        &self,
        card_id: CardId,
        expected_id: TerminalId,
        replacement: crate::chat::Session,
        offer: SnapshotOffer,
    ) -> Result<(), String> {
        let changes = offer
            .changes
            .or_else(|| Some(SnapshotChanges::at(Instant::now())));
        let (lock, changed) = &*self.queue;
        let mut queue = lock.lock().map_err(|_| "Session writer is unavailable")?;
        if queue.shutdown {
            return Err("Session writer is shutting down".into());
        }
        queue.admit_staged_adoptions();
        queue.queue_identity_replacement(IdentityReplacement {
            card_id,
            expected_terminal_id: expected_id,
            replacement: Replacement::Chat(replacement),
        });
        queue.record_snapshot_changes(changes);
        let previous = queue.latest.replace(offer.state);
        changed.notify_one();
        drop(queue);
        drop(previous);
        Ok(())
    }

    /// Records the conversations chat cards' shared views now hold in place
    /// of the ones this window last saved for them, which other clients
    /// started or replaced. Snapshots refuse such a change on their own.
    ///
    /// They are staged for the next write of the window's state, which must
    /// follow: it queues them ahead of itself under its own lock.
    pub fn stage_chat_adoptions(&self, adoptions: impl IntoIterator<Item = ChatIdentityAdoption>) {
        let (lock, _) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            queue
                .staged_adoptions
                .extend(adoptions.into_iter().map(|adoption| IdentityReplacement {
                    card_id: adoption.card,
                    expected_terminal_id: adoption.expected,
                    replacement: Replacement::SharedChat(adoption.canonical),
                }));
        }
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

    /// Commit a daemon-prepared resume successor before activating the chat.
    pub fn rebind_chat_resume(
        &self,
        card_id: CardId,
        source: crate::chat::Session,
        replacement: crate::chat::Session,
    ) -> std::result::Result<std::sync::mpsc::Receiver<std::result::Result<(), String>>, String>
    {
        self.rebind_chat(ChatRecoveryKind::Resume, card_id, source, replacement)
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
        let change_time = SnapshotChanges::at(Instant::now());
        let (lock, changed) = &*self.queue;
        let mut queue = lock.lock().map_err(|_| "Session writer is unavailable")?;
        if queue.shutdown {
            return Err("Session writer is shutting down".into());
        }
        queue.keep_writes_in_order();
        queue
            .conversions
            .push_back(ConversionCheckpoint::RetainedChatCopy {
                copy,
                reply: Some(reply),
            });
        queue.record_snapshot_changes(Some(change_time));
        changed.notify_one();
        Ok(receiver)
    }

    /// Queue a grant-bound attachment and await the same one-shot durable
    /// receipt as ordinary recovery. Descriptor arguments come only from it.
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
        let change_time = SnapshotChanges::at(Instant::now());
        let (lock, changed) = &*self.queue;
        let mut queue = lock.lock().map_err(|_| "Session writer is unavailable")?;
        if queue.shutdown {
            return Err("Session writer is shutting down".into());
        }
        queue.keep_writes_in_order();
        queue
            .conversions
            .push_back(ConversionCheckpoint::ChatRecovery(ChatRecoveryWrite {
                kind,
                card: card_id,
                source,
                replacement,
                reply,
            }));
        queue.record_snapshot_changes(Some(change_time));
        changed.notify_one();
        Ok(receiver)
    }

    /// Queues `replacement`, then `state` as the newest snapshot when given.
    fn replace_identity(
        &self,
        replacement: IdentityReplacement,
        state: Option<Arc<WorkspaceState>>,
    ) {
        self.replace_identities([replacement], state);
    }

    /// Queues `replacements` in order, then `state` as the newest snapshot
    /// when given.
    fn replace_identities(
        &self,
        replacements: impl IntoIterator<Item = IdentityReplacement>,
        state: Option<Arc<WorkspaceState>>,
    ) {
        let change_time = SnapshotChanges::at(Instant::now());
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            queue.admit_staged_adoptions();

            for replacement in replacements {
                queue.queue_identity_replacement(replacement);
            }
            let previous = state.and_then(|state| queue.latest.replace(state));
            queue.record_snapshot_changes(Some(change_time));
            changed.notify_one();
            drop(queue);
            drop(previous);
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
            // Nothing will write them now; what storage holds should still
            // follow what the shared views showed.
            queue.admit_staged_adoptions();
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
#[derive(Debug, Clone)]
struct Refusal {
    category: SaveFailureCategory,
    /// The rule the write broke, which is safe to show and record.
    check: crate::diagnostics::Check,
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
    /// Open panes whose content storage refused within the newest snapshot
    /// that landed, which is kept for an explicit retry. Storage still holds
    /// each pane's earlier content; any later snapshot that lands supersedes
    /// the set.
    panes: Option<(Arc<WorkspaceState>, BTreeMap<CardId, Refusal>)>,
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
            // Only a refusal by construction is set aside, and each names
            // the rule it broke.
            check: error.refused_check().map_or_else(
                || crate::diagnostics::Check::new("an unnamed storage rule"),
                Into::into,
            ),
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
        entries.get(&key).map(|(_, refusal)| refusal.clone()),
        subject,
        error,
    );
    entries.insert(key, (write, refusal));
}

/// Logs a refusal of `subject` and returns it, keeping the time of the first
/// of an unbroken run of refusals.
///
/// The log is not where a refusal is found after the fact: it is written only
/// under local debugging, and a window started by `cagents ui` has nowhere to
/// write it. Private diagnostics keep the subject and the rule it broke each
/// time the rule is new. A subject whose every edit is refused for the same
/// rule is one record, not one per edit, so it cannot push the window's other
/// records out of the bounded ring.
fn record_refusal(previous: Option<Refusal>, subject: WriteSubject, error: &StoreError) -> Refusal {
    let refusal = Refusal::new(error);
    if names_a_new_rule(previous.as_ref(), &refusal) {
        crate::diagnostics::record(refusal_event(&subject, &refusal));
    }
    match previous {
        None => {
            tracing::warn!(
                ?subject,
                %error,
                category = refusal.category.label(),
                check = %refusal.check,
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
                    check = %refusal.check,
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

/// Whether `refusal` breaks a rule other than the one `previous` refusal of the
/// same subject broke, which is when private diagnostics record it. Another
/// value in the same rule, such as a newer edit of the refused pane, is not.
fn names_a_new_rule(previous: Option<&Refusal>, refusal: &Refusal) -> bool {
    previous.is_none_or(|previous| previous.check != refusal.check)
}

/// The private diagnostic of a refusal: what was refused, by identifier, and
/// the rule it broke. Never a title, name, path or any other content.
fn refusal_event(subject: &WriteSubject, refusal: &Refusal) -> crate::diagnostics::Event {
    let (write, id) = subject.diagnostic();
    crate::diagnostics::Event::StorageRefused {
        write,
        id,
        check: refusal.check.clone(),
    }
}

/// Records a refusal of a one-shot write. Its error goes back to whoever asked
/// for it, and nothing retries it or keeps it on a notice, so this record is
/// the only one that outlasts the reply.
fn record_one_shot_refusal(
    write: crate::diagnostics::RefusedWrite,
    id: uuid::Uuid,
    error: &StoreError,
) {
    if let Some(check) = error.refused_check() {
        tracing::warn!(?write, %id, %error, "storage refused a one-shot cagents write");
        crate::diagnostics::record(crate::diagnostics::Event::StorageRefused {
            write,
            id: Some(id),
            check: check.into(),
        });
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
            && self.panes.is_none()
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
            .chain(self.panes.iter().flat_map(|(_, panes)| panes.values()))
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
            self.workspace
                .as_ref()
                .map(|(_, _, refusal)| refusal.clone()),
            WriteSubject::Workspace { card },
            error,
        );
        self.workspace = Some((snapshot, card, refusal));
    }

    /// Clears what a snapshot storage accepted settles: the refused snapshot it
    /// replaces, the panes it refused before and saved now, and every refused
    /// identity replacement whose card it no longer holds open at the
    /// identity that replacement would replace. `refused` names the open panes
    /// storage kept at their earlier content instead, which stay refused.
    ///
    /// Storage rejects a snapshot that rebinds a card's payload to an identity
    /// other than the one it holds, so the card's identity in a landed
    /// snapshot is the one storage holds, unless storage refused that card.
    /// Anything but the replaced identity leaves the refusal with nothing to
    /// replace - the card closed, converted, or first stored at its new
    /// identity - and nothing else could clear it: it would fail every
    /// barrier and the clean shutdown for the rest of the session.
    fn snapshot_landed(&mut self, snapshot: &Arc<WorkspaceState>, refused: &[StoreError]) {
        superseded(
            self.workspace
                .take()
                .map(|(_, card, _)| WriteSubject::Workspace { card }),
        );
        let mut previous = self
            .panes
            .take()
            .map(|(_, panes)| panes)
            .unwrap_or_default();
        let panes = refused
            .iter()
            .filter_map(|error| match error {
                StoreError::Card { card, .. } => Some((
                    *card,
                    record_refusal(
                        previous.remove(card),
                        WriteSubject::Pane { card: *card },
                        error,
                    ),
                )),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        for card in previous.into_keys() {
            superseded(Some(WriteSubject::Pane { card }));
        }
        self.identity_replacements.retain(|card, (replacement, _)| {
            let outstanding = panes.contains_key(card)
                || snapshot.cards.iter().any(|state| {
                    state.id == *card
                        && state.existence.is_open()
                        && replacement.replaces(&state.kind)
                });
            if !outstanding {
                tracing::info!(
                    subject = ?identity_subject(replacement),
                    "a refused identity replacement no longer applies to the saved window"
                );
            }
            outstanding
        });
        if !panes.is_empty() {
            self.panes = Some((Arc::clone(snapshot), panes));
        }
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
            check: refusal.check.clone(),
            refused_at_unix_ms: refusal.at_unix_ms,
            held_back,
        };
        let mut report = Vec::new();
        if let Some((_, card, refusal)) = &self.workspace {
            report.push(entry(WriteSubject::Workspace { card: *card }, refusal, 0));
        }
        if let Some((_, panes)) = &self.panes {
            report.extend(
                panes
                    .iter()
                    .map(|(card, refusal)| entry(WriteSubject::Pane { card: *card }, refusal, 0)),
            );
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
        Err(invalid_state!(
            "This store does not support terminal ownership recovery"
        ))
    }
    fn preserve_chat_copy(
        &self,
        _copy: &crate::persistence::RetainedChatCopy,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        Err(invalid_state!(
            "This store does not support retained chat copies"
        ))
    }

    fn rebind_authenticated_chat_upgrade(
        &self,
        _card_id: CardId,
        _recovery: &crate::chat::client::UpgradeRecovery,
    ) -> Result<(), StoreError> {
        Err(invalid_state!(
            "This store does not support authenticated chat recovery"
        ))
    }
    fn rebind_chat_upgrade(
        &self,
        _card_id: CardId,
        _source: &crate::chat::Session,
        _replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        Err(invalid_state!(
            "This store does not support upgrade recovery"
        ))
    }
    fn rebind_chat_resume(
        &self,
        _card_id: CardId,
        _source: &crate::chat::Session,
        _replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        Err(invalid_state!(
            "This store does not support resume recovery"
        ))
    }
    fn rebind_chat_account(
        &self,
        _card_id: CardId,
        _source: &crate::chat::Session,
        _replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        Err(invalid_state!(
            "This store does not support account recovery"
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
    fn adopt_chat_identity(
        &self,
        _card_id: CardId,
        _expected_id: TerminalId,
        _canonical: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        Err(invalid_state!(
            "This store does not support shared chat identities"
        ))
    }
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
            Err(invalid_state!(
                "This store does not support canonical pane adoption"
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
            Err(invalid_state!(
                "This store does not support restored pane ownership"
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
    fn rebind_chat_resume(
        &self,
        card_id: CardId,
        source: &crate::chat::Session,
        replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        StateStore::rebind_chat_resume(self, card_id, source, replacement)
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

    fn adopt_chat_identity(
        &self,
        card_id: CardId,
        expected_id: TerminalId,
        canonical: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        Self::adopt_chat_identity(self, card_id, expected_id, canonical)
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
    fn rebind_chat_resume(
        &self,
        card_id: CardId,
        source: &crate::chat::Session,
        replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        StateStore::rebind_chat_resume(self, card_id, source, replacement)
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
    fn adopt_chat_identity(
        &self,
        card_id: CardId,
        expected_id: TerminalId,
        canonical: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        StateStore::adopt_chat_identity(self, card_id, expected_id, canonical)
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
        if let Some(snapshot) = self
            .quarantine
            .workspace
            .as_ref()
            .map(|(snapshot, _, _)| snapshot)
            .or_else(|| self.quarantine.panes.as_ref().map(|(snapshot, _)| snapshot))
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
                    let canonical = recovery.canonical;
                    let repair = move |error: &StoreError| {
                        record_one_shot_refusal(
                            crate::diagnostics::RefusedWrite::TerminalRepair,
                            canonical.as_uuid(),
                            error,
                        );
                        error.to_string()
                    };
                    if let Err(error) = self.project_retained_chat_copies(&mut snapshot) {
                        let _ = reply.send(Err(repair(&error)));
                        continue;
                    }
                    let holds_refused_terminal = |state: &WorkspaceState| {
                        state.id == snapshot.id
                            && state.cards.iter().any(|card| {
                                card.id == recovery.canonical
                                    && card.existence.is_open()
                                    && matches!(&card.kind, CardKind::Terminal(session) if session.id == recovery.terminal)
                            })
                    };
                    let refused = self
                        .quarantine
                        .workspace
                        .as_ref()
                        .filter(|(state, card, _)| {
                            *card == Some(recovery.canonical) && holds_refused_terminal(state)
                        })
                        .map(|(state, _, _)| Arc::clone(state))
                        .or_else(|| {
                            self.quarantine
                                .panes
                                .as_ref()
                                .filter(|(state, panes)| {
                                    panes.contains_key(&recovery.canonical)
                                        && holds_refused_terminal(state)
                                })
                                .map(|(state, _)| Arc::clone(state))
                        });
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
                                self.quarantine.snapshot_landed(&snapshot, &commit.refused);
                                self.acknowledge_retained_copy_closures(&snapshot);
                                Ok(())
                            }
                            Err(error) => Err(repair(&error)),
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
                            record_one_shot_refusal(
                                crate::diagnostics::RefusedWrite::RetainedChatCopy,
                                copy.copy.id.as_uuid(),
                                &error,
                            );
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
                        ChatRecoveryKind::Resume => store.rebind_chat_resume(
                            recovery.card,
                            &recovery.source,
                            &recovery.replacement,
                        ),
                        ChatRecoveryKind::AuthenticatedUpgrade(witness) => {
                            if witness.source() != &recovery.source
                                || witness.replacement() != &recovery.replacement
                            {
                                Err(invalid_state!(
                                    "Authenticated recovery payload changed before commit"
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
                    let _ = recovery.reply.send(result.map_err(|error| {
                        record_one_shot_refusal(
                            crate::diagnostics::RefusedWrite::ConversationRecovery,
                            recovery.card.as_uuid(),
                            &error,
                        );
                        error.to_string()
                    }));
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
                            self.quarantine.snapshot_landed(&snapshot, &commit.refused);
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
        // Producers preserve their enqueue times. Drain time is not new
        // activity; using it would add a second debounce to a prepared offer.
        let incoming_changes = state
            .snapshot_changes
            .take()
            .or_else(|| received_changes.then(|| SnapshotChanges::at(Instant::now())));
        if !state.conversions.is_empty() {
            if let Some(previous) = writes.workspace.take() {
                writes
                    .conversions
                    .push_back(ConversionCheckpoint::Snapshot(previous));
            }
            writes.conversions.append(&mut state.conversions);
        }
        let retired_workspace = state
            .latest
            .take()
            .and_then(|latest| writes.workspace.replace(latest));
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
            due = schedule_flush_after_interval(
                due,
                incoming_changes,
                retry_exhausted,
                attempts_this_cycle,
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
        drop(retired_workspace);

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
            crate::diagnostics::record(final_flush.clone());
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

#[cfg(test)]
fn schedule_flush_after_changes(
    current_due: Option<Instant>,
    received_changes: bool,
    retry_exhausted: bool,
    attempts_this_cycle: u32,
    now: Instant,
    config: PersistenceConfig,
    oldest_change: &mut Option<Instant>,
) -> Option<Instant> {
    schedule_flush_after_interval(
        current_due,
        received_changes.then_some(SnapshotChanges::at(now)),
        retry_exhausted,
        attempts_this_cycle,
        config,
        oldest_change,
    )
}

fn schedule_flush_after_interval(
    current_due: Option<Instant>,
    incoming: Option<SnapshotChanges>,
    retry_exhausted: bool,
    attempts_this_cycle: u32,
    config: PersistenceConfig,
    oldest_change: &mut Option<Instant>,
) -> Option<Instant> {
    if let Some(incoming) = incoming.filter(|_| !retry_exhausted && attempts_this_cycle == 0) {
        let first = oldest_change.get_or_insert(incoming.first);
        *first = (*first).min(incoming.first);
        let trailing = incoming.latest + config.debounce;
        Some(
            current_due
                .map_or(trailing, |due| due.max(trailing))
                .min(*first + config.max_delay),
        )
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
/// It names each refused part of that state and the rule storage refused it
/// for. The guidance asks the reader to change or close "the item", and
/// whoever reads this is often not looking at the window - an updater relays
/// it to a terminal - so without the names there is nothing to act on, and
/// without the rule nothing to report. Neither carries a title, name or path.
fn window_state_refusal(category: SaveFailureCategory, refused: &[QuarantinedWrite]) -> String {
    let subjects = refused
        .iter()
        .filter(|write| write.subject.is_window_state())
        .map(|write| {
            format!(
                "{} (refused by rule: {})",
                write.subject.summary(),
                write.check
            )
        })
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
mod tests;
