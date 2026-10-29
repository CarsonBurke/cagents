use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{File, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
        Existence, Exposure, HostSessionProof, OperationId, ProjectChannel, ProjectId,
        ProjectState, Residency, TerminalContinuity, TerminalId, TerminalSession, Tombstone,
        TranscriptMetadata, WorkspaceDestination, WorkspaceId, WorkspaceState, WorktreeId,
        WorktreeState, normalize_project_channels,
    },
    secure_file::{
        migrate_existing_private_file, prepare_private_directory, validate_and_privatize_open_file,
    },
};

#[cfg(feature = "ui")]
mod review_cache;

// Format 14 fences readers which do not honor durable pane quarantine. An older
// reader must not restore a rejected source merely because it can decode the
// descriptor. This supplements the local IPC gate for old-set/remote opens.
pub const CURRENT_SCHEMA_VERSION: u32 = 14;
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
    /// Data decoded from storage, rather than content supplied by a writer.
    #[error("corrupt stored {field}: {reason}")]
    CorruptRow { field: String, reason: String },
    /// Storage failed on `card` within a larger write, such as a workspace
    /// snapshot, so the caller can say which card the failure concerns.
    #[error("card {card}: {error}")]
    Card {
        card: CardId,
        error: Box<StoreError>,
    },
    #[error("busy timeout must be greater than zero and no more than {maximum:?}")]
    InvalidBusyTimeout { maximum: Duration },
    #[error("state store mutex was poisoned")]
    MutexPoisoned,
    #[error("state store has already been shut down")]
    Closed,
}

impl StoreError {
    fn corrupt_row(field: impl Into<String>, reason: impl std::fmt::Display) -> Self {
        Self::CorruptRow {
            field: field.into(),
            reason: reason.to_string(),
        }
    }

    /// Whether storage refuses a write because of what the write contains, so
    /// offering the same content again can only fail the same way.
    ///
    /// Everything else is a condition of storage itself - a lock, a full disk,
    /// a read-only or corrupt file, a base generation that fell behind - which
    /// a retry can outlast and which every other write would hit too.
    ///
    /// A foreign-key violation is a refusal too. The only parent a caller can
    /// send a write ahead of is a worktree's project, and the session writer
    /// holds a worktree back until its project has a row; project rows are
    /// never deleted, since closing one stores a tombstone. A missing parent
    /// anywhere else is a fault in the write itself.
    pub fn refuses_content(&self) -> bool {
        use rusqlite::ErrorCode;

        match self {
            Self::Card { error, .. } => error.refuses_content(),
            Self::InvalidState(_) | Self::Serialization(_) => true,
            Self::Sqlite(rusqlite::Error::SqliteFailure(error, _)) => matches!(
                error.code,
                ErrorCode::ConstraintViolation | ErrorCode::TooBig | ErrorCode::TypeMismatch
            ),
            // Decode/missing-row failures describe storage, not this write's
            // content. Quarantining the write would silently strand valid edits.
            Self::Sqlite(_) | Self::CorruptRow { .. } | Self::InvalidUuid { .. } => false,
            Self::Io(_)
            | Self::UnsupportedSchema { .. }
            | Self::StaleGeneration { .. }
            | Self::InvalidBusyTimeout { .. }
            | Self::MutexPoisoned
            | Self::Closed => false,
        }
    }

    /// Attributes this error to `card`. A stale generation stays as it is:
    /// it concerns the whole snapshot's base, which the caller rebases.
    fn in_card(self, card: CardId) -> Self {
        match self {
            Self::StaleGeneration { .. } | Self::Card { .. } => self,
            error => Self::Card {
                card,
                error: Box::new(error),
            },
        }
    }
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
    Settings,
    Accounts,
    Automations,
    Work,
}

impl PersistedCardKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Chat => "chat",
            Self::Browser => "browser",
            Self::PullRequests => "pull_requests",
            Self::Repository => "repository",
            Self::Settings => "settings",
            Self::Accounts => "accounts",
            Self::Automations => "automations",
            Self::Work => "work",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "terminal" => Ok(Self::Terminal),
            "chat" => Ok(Self::Chat),
            "browser" => Ok(Self::Browser),
            "pull_requests" => Ok(Self::PullRequests),
            "repository" => Ok(Self::Repository),
            "settings" => Ok(Self::Settings),
            "accounts" => Ok(Self::Accounts),
            "automations" => Ok(Self::Automations),
            "work" => Ok(Self::Work),
            other => Err(StoreError::corrupt_row(
                "card kind",
                format!("unknown kind `{other}`"),
            )),
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

/// A retained native conversation, including cards closed in an earlier run.
#[derive(Debug, Clone)]
pub struct SavedConversation {
    pub card: CardId,
    pub title: String,
    pub session: crate::chat::Session,
    pub open: bool,
    pub updated_at_unix_ms: i64,
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
    pub card_scopes: HashMap<CardId, PathBuf>,
    pub chat_drafts: HashMap<CardId, String>,
    pub chat_draft_attachments: HashMap<CardId, Vec<crate::chat::Attachment>>,
    pub agent_updates: BTreeMap<CardId, Option<(ProjectId, WorktreeId)>>,
    pub workspace_history: Vec<WorkspaceDestination>,
    pub repository_navigation: HashMap<CardId, crate::model::RepositoryNavigation>,
    pub workspace_model_identity: Option<uuid::Uuid>,
    pub local_only_cards: HashSet<CardId>,
    pub quarantined_panes: HashMap<CardId, CardKind>,
    pub pending_chat_admissions: HashMap<CardId, crate::chat::Session>,
    pub pending_terminal_creations: HashSet<CardId>,
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
            other => Err(StoreError::corrupt_row(
                "pending-operation status",
                format!("unknown status `{other}`"),
            )),
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

/// A fresh local view of saved bytes. This grants no provider, retirement,
/// descriptor replacement, or canonical-pane ownership authority.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct RetainedChatCopy {
    pub workspace: WorkspaceId,
    pub source_card: CardId,
    pub source: crate::chat::Session,
    pub copy: CardState,
    pub draft: String,
    /// Exact managed-image draft metadata, committed with this inert copy.
    /// Empty omission preserves the digest of pre-image retained-copy receipts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::chat::Attachment>,
    pub scope: Option<PathBuf>,
    pub channel: Option<crate::model::ChannelId>,
}

impl RetainedChatCopy {
    fn validate(&self) -> Result<()> {
        if self.copy.id == self.source_card
            || !self.copy.existence.is_open()
            || !self.copy.width_fraction.is_finite()
            || self.copy.width_fraction <= 0.0
            || !matches!(&self.copy.kind, CardKind::Chat(session) if session == &self.source)
            || !self.source.has_valid_native_continuity()
            || (self.source.owned_codex_listener.is_some() && !self.source.owns_codex_listener())
        {
            return Err(StoreError::InvalidState(
                "Retained chat copy changed its captured source".into(),
            ));
        }
        let mut identities = std::collections::HashSet::new();
        for attachment in &self.attachments {
            let reference = attachment.reference.as_ref().ok_or_else(|| {
                StoreError::InvalidState(
                    "Retained image copies require explicit image identity".into(),
                )
            })?;
            reference.validate().map_err(StoreError::InvalidState)?;
            if !identities.insert((reference.device, reference.id)) {
                return Err(StoreError::InvalidState(
                    "Retained image copy repeats an image identity".into(),
                ));
            }
        }
        Ok(())
    }

    /// Keep a committed copy reachable in snapshots captured before its receipt.
    /// Only an explicit tombstone can remove it; omission cannot spend it.
    pub(crate) fn project(&self, state: &mut WorkspaceState) -> Result<()> {
        self.validate()?;
        if state.id != self.workspace {
            return Err(StoreError::InvalidState(
                "Retained chat copy belongs to another workspace".into(),
            ));
        }
        if let Some(card) = state.cards.iter().find(|card| card.id == self.copy.id) {
            if !card.existence.is_open() {
                return Ok(());
            }
            if card.kind != self.copy.kind {
                return Err(StoreError::InvalidState(
                    "Retained chat copy source changed before saving".into(),
                ));
            }
        } else {
            state.cards.push(self.copy.clone());
        }
        if state
            .chat_drafts
            .get(&self.copy.id)
            .is_some_and(|draft| draft != &self.draft)
        {
            return Err(StoreError::InvalidState(
                "Retained chat copy text changed before saving".into(),
            ));
        }
        if state
            .chat_draft_attachments
            .get(&self.copy.id)
            .is_some_and(|attachments| attachments != &self.attachments)
        {
            return Err(StoreError::InvalidState(
                "Retained chat copy images changed before saving".into(),
            ));
        }
        state.chat_drafts.insert(self.copy.id, self.draft.clone());
        if !self.attachments.is_empty() {
            state
                .chat_draft_attachments
                .insert(self.copy.id, self.attachments.clone());
        }
        state.local_only_cards.insert(self.copy.id);
        state
            .pending_chat_admissions
            .insert(self.copy.id, self.source.clone());
        let channel = self
            .channel
            .and_then(|id| state.channels.iter_mut().find(|channel| channel.id == id));
        if let Some(channel) = channel {
            if !channel.cards.contains(&self.copy.id) {
                let index = channel
                    .cards
                    .iter()
                    .position(|card| *card == self.source_card)
                    .map_or(channel.cards.len(), |index| index + 1);
                channel.cards.insert(index, self.copy.id);
            }
        } else {
            // Do not recreate a channel another client deleted. The source's
            // directory still provides a local navigation home for its text.
            state.card_scopes.insert(
                self.copy.id,
                self.scope
                    .clone()
                    .unwrap_or_else(|| self.source.launch.recipe.working_directory.clone()),
            );
        }
        if let Some(scope) = &self.scope {
            state.card_scopes.insert(self.copy.id, scope.clone());
        }
        Ok(())
    }
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

/// An explicit adoption of a daemon's canonical pane identity. The actor and
/// its transcript keep their identity; only the local historical alias retires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalPaneAdoption {
    pub alias: CardId,
    pub canonical: CardId,
    pub terminal: TerminalId,
}

/// A daemon-authorized canonical terminal whose archived owner is resolved by
/// the persistence writer. The UI never needs to read storage to enqueue it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalPaneRestoration {
    pub canonical: CardId,
    pub terminal: TerminalId,
}

fn select_closed_terminal_adoption(
    connection: &Connection,
    state: &WorkspaceState,
    canonical: CardId,
    terminal: TerminalId,
) -> Result<Option<TerminalPaneAdoption>> {
    let owner: Option<(String, String, String)> = connection
        .query_row(
            "SELECT c.card_id,c.workspace_id,c.existence_json FROM terminal_recipes t
             JOIN cards c ON c.card_id=t.card_id WHERE t.terminal_id=?1",
            [terminal.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((owner, workspace, existence)) = owner else {
        return Ok(None);
    };
    let alias = parse_id("terminal owner", &owner)?;
    if alias == canonical
        || workspace != state.id.to_string()
        || !matches!(
            from_json::<Existence>("terminal owner existence", &existence)?,
            Existence::Closed { .. }
        )
    {
        return Ok(None);
    }
    let Some(source) = state.cards.iter().find(|card| card.id == alias) else {
        return Ok(None);
    };
    if source.existence != from_json::<Existence>("terminal owner existence", &existence)?
        || !matches!(&source.kind, CardKind::Terminal(session) if session.id == terminal)
    {
        return Ok(None);
    }
    Ok(Some(TerminalPaneAdoption {
        alias,
        canonical,
        terminal,
    }))
}

fn validate_terminal_ownership_recovery(
    transaction: &Transaction<'_>,
    state: &WorkspaceState,
    base: SnapshotBase<'_>,
    recovery: TerminalPaneAdoption,
) -> Result<()> {
    let invalid = |reason: &str| {
        StoreError::InvalidState(format!("terminal ownership recovery refused: {reason}"))
    };
    if base.cards.contains_key(&recovery.canonical)
        || transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM cards WHERE card_id=?1)",
            [recovery.canonical.to_string()],
            |row| row.get::<_, bool>(0),
        )?
    {
        return Err(invalid("the live pane has already been saved"));
    }
    if select_closed_terminal_adoption(transaction, state, recovery.canonical, recovery.terminal)?
        != Some(recovery)
    {
        return Err(invalid("the exact closed owner changed"));
    }
    let source = state.cards.iter().find(|card| card.id == recovery.alias);
    let target = state
        .cards
        .iter()
        .find(|card| card.id == recovery.canonical);
    let (Some(source), Some(target)) = (source, target) else {
        return Err(invalid("both panes must remain in the window"));
    };
    let (CardKind::Terminal(archived), CardKind::Terminal(live)) = (&source.kind, &target.kind)
    else {
        return Err(invalid("both panes must retain their terminal descriptors"));
    };
    if !target.existence.is_open()
        || live.id != recovery.terminal
        || archived != live
        || load_terminal_record(transaction, recovery.alias)? != *archived
    {
        return Err(invalid(
            "the terminal recipe or continuity evidence changed",
        ));
    }
    Ok(())
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

/// The generations storage holds right now for a workspace and every card row
/// it owns.
///
/// This is [`WorkspaceCommit`] read back rather than returned by a commit, and
/// it exists for one caller: a writer whose base storage rejected as stale
/// needs the live generations to retry its own unchanged content against.
/// Tombstoned cards are included because the save path checks a snapshot
/// against every card row the workspace owns, not only the open ones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredGenerations {
    pub workspace: u64,
    pub cards: BTreeMap<CardId, u64>,
}

struct StoreInner {
    connection: Connection,
    closed: bool,
    refused_closures: RefusedClosures,
}

/// The closed cards whose final content storage has refused, by card.
type RefusedClosures = HashMap<CardId, RefusedClosure>;

/// A closed card's final content as storage refused it, with the generation
/// storage held for the card once the refusal was settled.
///
/// A refusal is a property of the content offered and the row it was offered
/// over ([`StoreError::refuses_content`]), and a window keeps offering the
/// cards it closed in every snapshot it saves. Remembering the refusal is what
/// keeps that from becoming the same failed write, and the same warning, on
/// every save for as long as the window stays open.
///
/// It is deliberately not persisted: the next run offers the content once
/// more, which is also how a build whose rules changed gets to accept it.
struct RefusedClosure {
    offered: CardState,
    generation: u64,
}

impl RefusedClosure {
    /// Whether offering `card` over `stored` is the offer already refused,
    /// with nothing left for it to store.
    fn repeats(&self, card: &CardState, stored: Option<&ExistingCard>) -> bool {
        // Destructured in full so that a field added to `CardState` has to be
        // classified here: what a card write stores is compared, and what
        // only describes the running window is not.
        let CardState {
            id: _,
            generation: _,
            title,
            width_fraction,
            existence,
            exposure: _,
            residency: _,
            kind,
        } = &self.offered;
        // An open row still has the tombstone to take, so only a row that is
        // absent or already closed makes the offer a pure repeat.
        stored.is_none_or(|stored| !stored.existence.is_open())
            && self.generation == stored.map_or(0, |stored| stored.generation)
            && *title == card.title
            && *width_fraction == card.width_fraction
            && *existence == card.existence
            && *kind == card.kind
    }
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
        enforce_private_database(source)?;
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

    pub fn load_dismissed_update(&self) -> Result<Option<String>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        metadata(&inner.connection, "dismissed_update_v1")
    }

    pub fn save_dismissed_update(&self, identity: &str) -> Result<()> {
        if identity.is_empty() || identity.len() > 4096 {
            return Err(StoreError::InvalidState(
                "invalid update notice identity".into(),
            ));
        }
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        set_metadata(&transaction, "dismissed_update_v1", identity)?;
        transaction.commit()?;
        Ok(())
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
        enforce_private_database(path)?;
        enforce_sqlite_sidecars(path)?;

        Ok(Self {
            inner: Mutex::new(StoreInner {
                refused_closures: RefusedClosures::new(),
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
        let StoreInner {
            connection,
            refused_closures,
            ..
        } = &mut *inner;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let commit = save_workspace_transaction(&transaction, state, base, refused_closures)?;
        // A conversion defers its foreign keys, so a violation it caused would
        // surface here, attributed to no card. It cannot arise in practice:
        // the terminal and its transcript metadata move together.
        transaction.commit()?;
        Ok(commit)
    }

    /// Resolve an archived resource owner for a daemon-authorized canonical pane.
    /// Historical descriptors can share an identity, so only the recipe's actual
    /// owner can authorize this transfer. The commit rechecks ownership and CAS.
    pub fn closed_terminal_pane_adoption(
        &self,
        state: &WorkspaceState,
        canonical: CardId,
        terminal: TerminalId,
    ) -> Result<Option<TerminalPaneAdoption>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        select_closed_terminal_adoption(&inner.connection, state, canonical, terminal)
    }

    /// Repair one already-closed historical owner from an authoritative shared
    /// pane descriptor. This is maintenance of a live database: it deliberately
    /// does not run startup reconciliation or save a reconstructed workspace.
    /// Navigation, drafts, pending operations and unrelated actor proofs remain
    /// untouched. The caller must verify the supplied pane against the daemon's
    /// current shared view; this API does not authorize an actor start or resume.
    pub fn repair_closed_terminal_pane_owner(
        path: impl AsRef<Path>,
        boot_id: &BootId,
        workspace: WorkspaceId,
        canonical: &CardState,
        base: SnapshotBase<'_>,
    ) -> Result<Option<WorkspaceCommit>> {
        Self::repair_closed_terminal_pane_owner_checked(
            path, boot_id, workspace, canonical, base, None,
        )
    }

    /// Explicit maintenance of a reviewed offline pane whose authoritative
    /// configuration changed after its historical alias was closed. The caller
    /// must verify the exact shared view and absence from both daemon actor
    /// inventories before using this exception. The original launch must match
    /// storage, and neither this transfer nor its caller may start an actor.
    pub fn repair_closed_terminal_pane_owner_with_offline_recipe(
        path: impl AsRef<Path>,
        boot_id: &BootId,
        workspace: WorkspaceId,
        canonical: &CardState,
        base: SnapshotBase<'_>,
        expected_historical_launch: &crate::model::LaunchRecipe,
    ) -> Result<Option<WorkspaceCommit>> {
        Self::repair_closed_terminal_pane_owner_checked(
            path,
            boot_id,
            workspace,
            canonical,
            base,
            Some(expected_historical_launch),
        )
    }

    fn repair_closed_terminal_pane_owner_checked(
        path: impl AsRef<Path>,
        boot_id: &BootId,
        workspace: WorkspaceId,
        canonical: &CardState,
        base: SnapshotBase<'_>,
        expected_historical_launch: Option<&crate::model::LaunchRecipe>,
    ) -> Result<Option<WorkspaceCommit>> {
        let CardKind::Terminal(session) = &canonical.kind else {
            return Err(StoreError::InvalidState(
                "canonical pane must be a terminal".into(),
            ));
        };
        if !canonical.existence.is_open()
            || !canonical.width_fraction.is_finite()
            || canonical.width_fraction <= 0.0
            || boot_id.as_str().is_empty()
        {
            return Err(StoreError::InvalidState(
                "invalid canonical pane repair".into(),
            ));
        }
        let mut connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(DEFAULT_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != CURRENT_SCHEMA_VERSION {
            return Err(StoreError::InvalidState(
                "repair requires the current store schema".into(),
            ));
        }
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if metadata(&transaction, "last_boot_id")?.as_deref() != Some(boot_id.as_str()) {
            return Err(StoreError::InvalidState(
                "repair requires the store's current boot".into(),
            ));
        }
        let workspace_id = workspace.to_string();
        let actual = select_generation(&transaction, SELECT_WORKSPACE_GENERATION, &workspace_id)?;
        check_generation("workspace", workspace, base.workspace, actual)?;
        if !select_existence(&transaction, SELECT_WORKSPACE_EXISTENCE, &workspace_id)?
            .is_some_and(|existence| existence.is_open())
        {
            return Err(StoreError::InvalidState(
                "repair requires an open workspace".into(),
            ));
        }
        let owner: Option<(String, String)> = transaction
            .query_row(
                "SELECT c.card_id,c.workspace_id FROM terminal_recipes t
             JOIN cards c ON c.card_id=t.card_id WHERE t.terminal_id=?1",
                [session.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((owner, owner_workspace)) = owner else {
            return Err(StoreError::InvalidState(
                "terminal has no historical owner".into(),
            ));
        };
        if owner_workspace != workspace_id {
            return Err(StoreError::InvalidState(
                "terminal belongs to another workspace".into(),
            ));
        }
        let alias: CardId = parse_id("terminal owner", &owner)?;
        let stored = existing_cards(&transaction, &workspace_id)?;
        if alias == canonical.id {
            // Replays do not overwrite a canonical actor's newer runtime proof.
            return Ok(None);
        }
        let source = &stored[&alias];
        if !matches!(source.existence, Existence::Closed { .. })
            || source.kind != PersistedCardKind::Terminal
        {
            return Err(StoreError::InvalidState(
                "repair requires an already-closed terminal owner".into(),
            ));
        }
        check_generation(
            "card",
            alias,
            base.cards.get(&alias).copied().unwrap_or(0),
            Some(source.generation),
        )?;
        if let Some(target) = stored.get(&canonical.id) {
            if !target.existence.is_open() || target.kind != PersistedCardKind::Terminal {
                return Err(StoreError::InvalidState(
                    "repair cannot replace or resurrect a canonical pane".into(),
                ));
            }
            let target_session = load_terminal_record(&transaction, canonical.id)?;
            if target_session.id != session.id {
                return Err(StoreError::InvalidState(
                    "canonical pane owns a different terminal".into(),
                ));
            }
        }
        let expected_target = base.cards.get(&canonical.id).copied().unwrap_or(0);
        check_generation(
            "card",
            canonical.id,
            expected_target,
            stored.get(&canonical.id).map(|card| card.generation),
        )?;
        let archived = load_terminal_record(&transaction, alias)?;
        if expected_historical_launch.is_some_and(|expected| archived.launch != *expected) {
            return Err(StoreError::InvalidState(
                "historical launch changed since offline repair review".into(),
            ));
        }
        let reviewed_offline_recipe = expected_historical_launch.is_some_and(|expected|
            archived.launch == *expected
                && archived.launch.working_directory == session.launch.working_directory
                && matches!(&session.continuity,TerminalContinuity::RelaunchAvailable {recipe,..} if recipe==&session.launch)
        );
        if expected_historical_launch.is_some() && !reviewed_offline_recipe {
            return Err(StoreError::InvalidState(
                "reviewed recipe repair requires the same directory and offline continuity".into(),
            ));
        }
        if (archived.launch != session.launch && !reviewed_offline_recipe)
            || archived.owned_codex_listener != session.owned_codex_listener
            || archived.recovery_origin != session.recovery_origin
            || archived.predecessor != session.predecessor
            || archived.resume_hint != session.resume_hint
            || archived.transcript_path != session.transcript_path
        {
            return Err(StoreError::InvalidState(
                "canonical pane must preserve the historical terminal's identity and transcript"
                    .into(),
            ));
        }
        let timestamp = now_unix_ms();
        let source_generation = next_generation(source.generation, "card", alias)?;
        transaction.pragma_update(None, "defer_foreign_keys", "ON")?;
        // Preserve existence_json byte for byte: the user's closure has its own
        // timestamp and reason, independent of this later identity transfer.
        transaction.execute(
            "UPDATE cards SET content_json=?1,content_hash=NULL,generation=?2,updated_at_unix_ms=?3 WHERE card_id=?4",
            params![to_json(&archived)?,u64_to_i64(source_generation,"card generation")?,timestamp,alias.to_string()],
        )?;
        transaction.execute(
            "UPDATE terminal_recipes SET card_id=?1 WHERE terminal_id=?2 AND card_id=?3",
            params![
                canonical.id.to_string(),
                session.id.to_string(),
                alias.to_string()
            ],
        )?;
        let ordinal: i64 = transaction.query_row(
            "SELECT COALESCE((SELECT ordinal FROM cards WHERE card_id=?1),
                 (SELECT COALESCE(MAX(ordinal),-1)+1 FROM cards WHERE workspace_id=?2))",
            params![canonical.id.to_string(), workspace_id],
            |row| row.get(0),
        )?;
        let ordinal = usize::try_from(ordinal)
            .map_err(|_| StoreError::InvalidState("invalid card ordinal".into()))?;
        let target_generation = save_card(
            &transaction,
            &workspace_id,
            ordinal,
            canonical,
            expected_target,
            stored.get(&canonical.id),
            timestamp,
        )?;
        let generation = next_generation(base.workspace, "workspace", workspace)?;
        transaction.execute(
            "UPDATE workspaces SET generation=?1,updated_at_unix_ms=?2 WHERE workspace_id=?3",
            params![
                u64_to_i64(generation, "workspace generation")?,
                timestamp,
                workspace_id
            ],
        )?;
        let mut card_generations: BTreeMap<_, _> = stored
            .into_iter()
            .map(|(id, card)| (id, card.generation))
            .collect();
        card_generations.insert(alias, source_generation);
        card_generations.insert(canonical.id, target_generation);
        transaction.commit()?;
        Ok(Some(WorkspaceCommit {
            generation,
            card_generations,
        }))
    }

    /// Commit canonical pane adoption and the complete navigation snapshot in
    /// one transaction. Ordinary snapshots cannot transfer resource ownership.
    pub fn save_workspace_with_terminal_adoptions(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
        adoptions: &[TerminalPaneAdoption],
    ) -> Result<WorkspaceCommit> {
        self.save_workspace_with_terminal_panes(state, base, adoptions, &[])
    }

    /// Resolve restored recipe ownership and commit all canonical changes in
    /// the same ordered writer transaction as their navigation snapshot.
    pub(crate) fn save_workspace_with_terminal_panes(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
        adoptions: &[TerminalPaneAdoption],
        restorations: &[TerminalPaneRestoration],
    ) -> Result<WorkspaceCommit> {
        self.save_workspace_with_terminal_panes_and_recovery(
            state,
            base,
            adoptions,
            restorations,
            None,
        )
    }

    /// Explicitly recover an unsaved pane from an exact closed historical owner.
    /// The complete workspace and the ownership transfer commit together; this
    /// never permits an ordinary snapshot to take another pane's identity.
    pub fn save_workspace_recovering_terminal_owner(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
        recovery: TerminalPaneAdoption,
    ) -> Result<WorkspaceCommit> {
        self.save_workspace_with_terminal_panes_and_recovery(state, base, &[], &[], Some(recovery))
    }

    fn save_workspace_with_terminal_panes_and_recovery(
        &self,
        state: &WorkspaceState,
        base: SnapshotBase<'_>,
        adoptions: &[TerminalPaneAdoption],
        restorations: &[TerminalPaneRestoration],
        recovery: Option<TerminalPaneAdoption>,
    ) -> Result<WorkspaceCommit> {
        if adoptions.is_empty() && restorations.is_empty() && recovery.is_none() {
            return self.save_workspace(state, base);
        }
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let StoreInner {
            connection,
            refused_closures,
            ..
        } = &mut *inner;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch("PRAGMA defer_foreign_keys=ON")?;
        let mut prepared = state.clone();
        let mut generations = base.cards.clone();
        let mut adoptions = adoptions.to_vec();
        if let Some(recovery) = recovery {
            validate_terminal_ownership_recovery(&transaction, state, base, recovery)?;
            adoptions.push(recovery);
        }
        for restoration in restorations {
            if !state.cards.iter().any(|card| card.id == restoration.canonical
                && card.existence.is_open()
                && matches!(&card.kind, CardKind::Terminal(session) if session.id == restoration.terminal)) {
                return Err(StoreError::InvalidState("terminal restoration needs its exact open canonical pane".into()));
            }
            if let Some(adoption) = select_closed_terminal_adoption(
                &transaction,
                state,
                restoration.canonical,
                restoration.terminal,
            )? && !adoptions.contains(&adoption)
            {
                adoptions.push(adoption);
            }
        }
        for adoption in &adoptions {
            let source = state
                .cards
                .iter()
                .find(|card| card.id == adoption.alias)
                .ok_or_else(|| {
                    StoreError::InvalidState("pane adoption needs its closed alias".into())
                })?;
            let target = state
                .cards
                .iter()
                .find(|card| card.id == adoption.canonical)
                .ok_or_else(|| {
                    StoreError::InvalidState("pane adoption needs its canonical pane".into())
                })?;
            if adoption.alias == adoption.canonical
                || source.existence.is_open()
                || !target.existence.is_open()
                || !matches!(&source.kind, CardKind::Terminal(session) if session.id == adoption.terminal)
                || !matches!(&target.kind, CardKind::Terminal(session) if session.id == adoption.terminal)
            {
                return Err(StoreError::InvalidState(
                    "pane adoption must retain an exact terminal identity and retire its alias"
                        .into(),
                ));
            }
            let owner: Option<String> = transaction
                .query_row(
                    "SELECT card_id FROM terminal_recipes WHERE terminal_id=?1",
                    [adoption.terminal.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if owner.as_deref() == Some(&adoption.canonical.to_string()) {
                continue;
            }
            let row: Option<(String, u64)> = transaction
                .query_row(
                    "SELECT workspace_id,generation FROM cards WHERE card_id=?1",
                    [adoption.alias.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((workspace, generation)) = row else {
                if owner.is_some() {
                    return Err(StoreError::InvalidState(
                        "terminal is owned by an unrelated pane".into(),
                    ));
                }
                // Inventory discovery may not have been written yet. There is
                // no historical row to archive or resurrect in this case.
                prepared.cards.retain(|card| card.id != adoption.alias);
                continue;
            };
            if workspace != state.id.to_string()
                || owner.as_deref() != Some(&adoption.alias.to_string())
            {
                return Err(StoreError::InvalidState(
                    "pane adoption cannot take another pane's resource".into(),
                ));
            }
            let expected = base.cards.get(&adoption.alias).copied().unwrap_or(0);
            if generation != expected {
                return Err(StoreError::StaleGeneration {
                    entity: format!("card {}", adoption.alias),
                    expected,
                    actual: generation,
                });
            }
            let archived = load_terminal_record(&transaction, adoption.alias)?;
            let next = next_generation(generation, "card", adoption.alias)?;
            transaction.execute("UPDATE cards SET content_json=?1, content_hash=NULL, existence_json=?2,generation=?3,updated_at_unix_ms=?4 WHERE card_id=?5", params![to_json(&archived)?,to_json(&source.existence)?,u64_to_i64(next,"card generation")?,now_unix_ms(),adoption.alias.to_string()])?;
            let moved = transaction.execute(
                "UPDATE terminal_recipes SET card_id=?1 WHERE terminal_id=?2 AND card_id=?3",
                params![
                    adoption.canonical.to_string(),
                    adoption.terminal.to_string(),
                    adoption.alias.to_string()
                ],
            )?;
            if moved != 1 {
                return Err(StoreError::InvalidState(
                    "terminal ownership changed during pane adoption".into(),
                ));
            }
            transaction.execute(INVALIDATE_CARD_HASH, [adoption.canonical.to_string()])?;
            generations.insert(adoption.alias, next);
        }
        let commit = save_workspace_transaction(
            &transaction,
            &prepared,
            SnapshotBase {
                workspace: base.workspace,
                cards: &generations,
            },
            refused_closures,
        )?;
        transaction.commit()?;
        Ok(commit)
    }

    /// Reads the generations a rejected write has to be rebased onto.
    ///
    /// [`Self::save_workspace`] rejects a snapshot whose base generations are
    /// not the ones storage holds, and the writer lane cannot derive the live
    /// ones from the rejection: the error names the workspace that moved, not
    /// every card row. This returns the whole base instead, so the lane can
    /// write the same content against it.
    ///
    /// A workspace generation and its card rows are the whole base, because
    /// they are the only compare-and-set counters a snapshot is checked
    /// against. Everything `save_workspace_transaction` keeps in `metadata` -
    /// the navigation fields, the channel list and selected channel, the card
    /// scopes, and the shared workspace model identity - has no generation of
    /// its own: those rows are written inside the same transaction and are
    /// therefore guarded by the workspace generation returned here. A rebase
    /// that reseats the workspace and its cards consequently covers them too,
    /// and nothing has to be added here when a new navigation field is
    /// persisted alongside them.
    pub fn stored_generations(&self, id: WorkspaceId) -> Result<StoredGenerations> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let workspace_id = id.to_string();
        // A workspace with no row yet is written against generation zero, the
        // same base a never-committed snapshot starts from.
        let workspace = select_generation(
            &inner.connection,
            SELECT_WORKSPACE_GENERATION,
            &workspace_id,
        )?
        .unwrap_or(0);
        let mut statement = inner
            .connection
            .prepare_cached(SELECT_WORKSPACE_CARD_GENERATIONS)?;
        let rows = statement.query_map([&workspace_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut cards = BTreeMap::new();
        for row in rows {
            let (card_id, generation) = row?;
            cards.insert(
                parse_id("card", &card_id)?,
                i64_to_u64(generation, "card generation")?,
            );
        }
        Ok(StoredGenerations { workspace, cards })
    }

    /// The generation storage holds for one project, or `None` when it holds no
    /// row for it. Companion to [`Self::stored_generations`] for the catalog
    /// entities, which carry their base inside the state they write.
    pub fn stored_project_generation(&self, id: ProjectId) -> Result<Option<u64>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        select_generation(
            &inner.connection,
            SELECT_PROJECT_GENERATION,
            &id.to_string(),
        )
    }

    pub fn stored_worktree_generation(&self, id: WorktreeId) -> Result<Option<u64>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        select_generation(
            &inner.connection,
            SELECT_WORKTREE_GENERATION,
            &id.to_string(),
        )
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

    /// Atomically puts a new conversation in an existing chat card.
    ///
    /// This is the explicit restart path - another provider or account for a
    /// thread that has not begun, or a fresh start after a failed resume.
    /// Normal workspace snapshots still reject a chat card whose conversation
    /// identity changed through `validate_chat_identity`.
    pub fn replace_chat_identity(
        &self,
        card_id: CardId,
        expected_id: TerminalId,
        replacement: &crate::chat::Session,
    ) -> Result<()> {
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        replace_chat_identity_transaction(&transaction, card_id, expected_id, replacement)?;
        transaction.commit()?;
        Ok(())
    }

    /// A provider-confirmed account recovery retains the external conversation
    /// and changes only its account and runtime identity. Compare the complete
    /// source descriptor so a stale recovery cannot overwrite newer ownership.
    pub fn rebind_chat_account(
        &self,
        card_id: CardId,
        source: &crate::chat::Session,
        replacement: &crate::chat::Session,
    ) -> Result<()> {
        if !crate::chat::is_account_recovery(source, replacement)
            || !replacement
                .proof
                .as_ref()
                .is_some_and(HostSessionProof::is_complete)
        {
            return Err(StoreError::InvalidState(
                "Account recovery has no exact confirmed conversation".into(),
            ));
        }
        self.rebind_chat_descriptor(card_id, source, replacement)
    }

    /// Commit a provider-confirmed descriptor transition from authenticated daemon
    /// upgrade lineage. Ordinary snapshots cannot authorize this transition.
    pub fn rebind_chat_upgrade(
        &self,
        card_id: CardId,
        source: &crate::chat::Session,
        replacement: &crate::chat::Session,
    ) -> Result<()> {
        // Retained actor recovery requires the immutable witness from the
        // authenticated Resolve/Attach/snapshot handshake. Descriptor values
        // alone cannot certify either an actor epoch or a learned identity.
        if matches!(
            source.native_continuity,
            crate::chat::NativeContinuity::RetainedClaudeActor { .. }
        ) || matches!(
            replacement.native_continuity,
            crate::chat::NativeContinuity::RetainedClaudeActor { .. }
        ) || !(crate::chat::is_upgrade_recovery(source, replacement)
            || crate::chat::is_upgrade_identity_learning(source, replacement))
        {
            return Err(StoreError::InvalidState(
                "Upgrade recovery changed the saved conversation configuration or lacks complete distinct ownership proofs".into(),
            ));
        }
        self.rebind_chat_descriptor(card_id, source, replacement)
    }

    /// Consume only the immutable witness minted by the authenticated client
    /// handshake. Two descriptor values cannot authorize a retained reset.
    #[cfg(feature = "ui")]
    pub(crate) fn rebind_authenticated_chat_upgrade(
        &self,
        card_id: CardId,
        recovery: &crate::chat::client::UpgradeRecovery,
    ) -> Result<()> {
        if !recovery.validate() {
            return Err(StoreError::InvalidState(
                "Invalid authenticated chat recovery witness".into(),
            ));
        }
        self.rebind_chat_descriptor(card_id, recovery.source(), recovery.replacement())
    }

    fn rebind_chat_descriptor(
        &self,
        card_id: CardId,
        source: &crate::chat::Session,
        replacement: &crate::chat::Session,
    ) -> Result<()> {
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_current_descriptor_write(&transaction)?;
        let (kind, content, existence): (String, Option<String>, String) = transaction.query_row(
            "SELECT kind_tag, content_json, existence_json FROM cards WHERE card_id = ?1",
            [card_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if kind != PersistedCardKind::Chat.as_str()
            || !from_json::<Existence>("card existence", &existence)?.is_open()
        {
            return Err(StoreError::InvalidState(
                "The recovery card is no longer an open chat".into(),
            ));
        }
        let stored: crate::chat::Session = from_json(
            "chat card",
            &content.ok_or_else(|| StoreError::CorruptRow {
                field: "chat card".into(),
                reason: "The recovery source has no payload".into(),
            })?,
        )?;
        if &stored == replacement {
            return Ok(());
        }
        if &stored != source {
            return Err(StoreError::InvalidState(
                "The saved conversation changed before recovery committed".into(),
            ));
        }
        transaction.execute(
            "UPDATE cards SET content_json = ?1, updated_at_unix_ms = ?2 WHERE card_id = ?3",
            params![to_json(replacement)?, now_unix_ms(), card_id.to_string()],
        )?;
        transaction.execute(INVALIDATE_CARD_HASH, [card_id.to_string()])?;
        transaction.commit()?;
        Ok(())
    }

    /// Commit only a new inert copy, even when a newer canonical descriptor
    /// makes the caller's full workspace snapshot unacceptable. Existing card
    /// payloads and generations are never rewritten by this operation.
    pub fn preserve_chat_copy(&self, copy: &RetainedChatCopy) -> Result<WorkspaceCommit> {
        copy.validate()?;
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let transaction = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_current_descriptor_write(&transaction)?;
        let workspace = copy.workspace.to_string();
        let generation = select_generation(&transaction, SELECT_WORKSPACE_GENERATION, &workspace)?
            .ok_or_else(|| {
                StoreError::InvalidState("Retained chat copy needs its existing workspace".into())
            })?;
        if !select_existence(&transaction, SELECT_WORKSPACE_EXISTENCE, &workspace)?
            .is_some_and(|existence| existence.is_open())
        {
            return Err(StoreError::InvalidState(
                "Retained chat copy workspace is closed".into(),
            ));
        }
        let owner: Option<String> = transaction
            .query_row(
                SELECT_CARD_WORKSPACE,
                [copy.source_card.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if owner.as_deref() != Some(workspace.as_str()) {
            return Err(StoreError::InvalidState(
                "Retained chat copy origin is outside this workspace".into(),
            ));
        }
        let receipt_key = format!("workspace:{workspace}:retained_chat_copy:{}", copy.copy.id);
        use sha2::Digest;
        // A receipt verifies retries without retaining another hidden copy of
        // draft text or account configuration after an explicit discard.
        let digest = format!("{:x}", sha2::Sha256::digest(to_json(copy)?.as_bytes()));
        let mut manifest = load_workspace_manifest_connection(&transaction, copy.workspace)?
            .ok_or_else(|| {
                StoreError::InvalidState("Retained chat copy workspace disappeared".into())
            })?;
        let existing = existing_cards(&transaction, &workspace)?;
        if let Some(receipt) = metadata(&transaction, &receipt_key)? {
            if receipt != digest
                || !existing
                    .get(&copy.copy.id)
                    .is_some_and(|card| card.existence.is_open())
                || load_card_connection(&transaction, copy.copy.id, &self.boot_id, None)?
                    .is_none_or(|card| card.kind != copy.copy.kind)
                || manifest.chat_drafts.get(&copy.copy.id) != Some(&copy.draft)
                || manifest
                    .chat_draft_attachments
                    .get(&copy.copy.id)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    != copy.attachments.as_slice()
                || !manifest.local_only_cards.contains(&copy.copy.id)
                || manifest.pending_chat_admissions.get(&copy.copy.id) != Some(&copy.source)
                || !(manifest.card_scopes.contains_key(&copy.copy.id)
                    || manifest
                        .channels
                        .iter()
                        .any(|channel| channel.cards.contains(&copy.copy.id)))
            {
                return Err(StoreError::InvalidState(
                    "Retained chat copy retry changed its committed payload".into(),
                ));
            }
            return Ok(WorkspaceCommit {
                generation,
                card_generations: existing
                    .into_iter()
                    .map(|(id, card)| (id, card.generation))
                    .collect(),
            });
        }
        if transaction
            .query_row(SELECT_CARD_WORKSPACE, [copy.copy.id.to_string()], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
            .is_some()
        {
            return Err(StoreError::InvalidState(
                "Retained chat copy identity is already occupied".into(),
            ));
        }
        let ordinal: usize = transaction.query_row(
            "SELECT coalesce(max(ordinal) + 1, 0) FROM cards WHERE workspace_id = ?1",
            [&workspace],
            |row| row.get(0),
        )?;
        let card_generation = save_card(
            &transaction,
            &workspace,
            ordinal,
            &copy.copy,
            0,
            None,
            now_unix_ms(),
        )?;
        // Merge navigation from the current transaction, never from a captured
        // UI snapshot that could replace the canonical pane or unrelated edits.
        manifest
            .chat_drafts
            .insert(copy.copy.id, copy.draft.clone());
        if !copy.attachments.is_empty() {
            manifest
                .chat_draft_attachments
                .insert(copy.copy.id, copy.attachments.clone());
        }
        manifest.local_only_cards.insert(copy.copy.id);
        manifest
            .pending_chat_admissions
            .insert(copy.copy.id, copy.source.clone());
        let mut state = WorkspaceState::default();
        state.id = copy.workspace;
        state.channels = manifest.channels;
        state.card_scopes = manifest.card_scopes;
        copy.project(&mut state)?;
        for (name, value) in [
            ("chat_drafts", to_json(&manifest.chat_drafts)?),
            (
                "chat_draft_attachments",
                to_json(&manifest.chat_draft_attachments)?,
            ),
            ("local_only_cards", to_json(&manifest.local_only_cards)?),
            (
                "pending_chat_admissions",
                to_json(&manifest.pending_chat_admissions)?,
            ),
            (CHANNELS_FIELD, to_json(&state.channels)?),
            ("card_scopes", to_json(&state.card_scopes)?),
        ] {
            set_metadata(
                &transaction,
                &format!("workspace:{workspace}:navigation:{name}"),
                &value,
            )?;
        }
        set_metadata(&transaction, &receipt_key, &digest)?;
        let next = next_generation(generation, "workspace", copy.workspace)?;
        transaction.execute("UPDATE workspaces SET generation = ?1, updated_at_unix_ms = ?2 WHERE workspace_id = ?3", params![u64_to_i64(next, "workspace generation")?, now_unix_ms(), &workspace])?;
        let mut card_generations = existing
            .into_iter()
            .map(|(id, card)| (id, card.generation))
            .collect::<BTreeMap<_, _>>();
        card_generations.insert(copy.copy.id, card_generation);
        transaction.commit()?;
        Ok(WorkspaceCommit {
            generation: next,
            card_generations,
        })
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
                    updated_at_unix_ms, kind_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(project_id) DO UPDATE SET
                    generation = excluded.generation,
                    name = excluded.name,
                    repository_json = excluded.repository_json,
                    kind_json = excluded.kind_json,
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
                to_json(&project.kind)?,
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
                "SELECT generation, name, repository_json, existence_json, kind_json
                 FROM projects WHERE project_id = ?1",
                [id.to_string()],
                |row| {
                    let generation: i64 = row.get(0)?;
                    let name: String = row.get(1)?;
                    let repository: String = row.get(2)?;
                    let existence: String = row.get(3)?;
                    let kind: String = row.get(4)?;
                    Ok((generation, name, repository, existence, kind))
                },
            )
            .optional()?
            .map(|(generation, name, repository, existence, kind)| {
                Ok(ProjectState {
                    id,
                    generation: i64_to_u64(generation, "project generation")?,
                    name,
                    repository: from_json("project repository", &repository)?,
                    kind: from_json("project kind", &kind)?,
                    existence: from_json("project existence", &existence)?,
                })
            })
            .transpose()
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectState>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let mut statement = inner.connection.prepare(
            "SELECT project_id, generation, name, repository_json, existence_json, kind_json
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
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut projects = Vec::new();
        for row in rows {
            let (id, generation, name, repository, existence, kind) = row?;
            projects.push(ProjectState {
                id: parse_id("project", &id)?,
                generation: i64_to_u64(generation, "project generation")?,
                name,
                repository: from_json("project repository", &repository)?,
                kind: from_json("project kind", &kind)?,
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
        // Unlike a card or project, a tombstoned worktree may reopen. Its id is
        // derived from its path and Git is the authority on what exists there,
        // so a worktree created where a deleted one lived is that identity
        // again, not a stale write resurrecting it. Refusing it would fail
        // every retry, and once retries ran out the writer would stop
        // scheduling later changes.
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

    pub fn saved_conversations(&self) -> Result<Vec<SavedConversation>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let mut statement = inner.connection.prepare(
            "SELECT card_id, title, content_json, existence_json, updated_at_unix_ms
             FROM cards WHERE kind_tag = 'chat'
             ORDER BY updated_at_unix_ms DESC, card_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut conversations = Vec::new();
        for row in rows {
            let parsed: Result<Option<SavedConversation>> = (|| {
                let (id, title, content, existence, updated_at_unix_ms) = row?;
                let session: crate::chat::Session = from_json("saved conversation", &content)?;
                if !session.has_valid_native_continuity() {
                    return Err(StoreError::InvalidState(
                        "Invalid saved native continuity".into(),
                    ));
                }
                if session.launch.session_id.is_none() {
                    return Ok(None);
                }
                Ok(Some(SavedConversation {
                    card: parse_id("card", &id)?,
                    title,
                    session,
                    open: from_json::<Existence>("conversation existence", &existence)?.is_open(),
                    updated_at_unix_ms,
                }))
            })();
            match parsed {
                Ok(Some(conversation)) => conversations.push(conversation),
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "Skipping unreadable saved conversation"),
            }
        }
        Ok(conversations)
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
            card_scopes: manifest.card_scopes,
            chat_drafts: manifest.chat_drafts,
            chat_draft_attachments: manifest.chat_draft_attachments,
            agent_updates: manifest.agent_updates,
            workspace_history: manifest.workspace_history,
            repository_navigation: manifest.repository_navigation,
            workspace_model_identity: manifest.workspace_model_identity,
            local_only_cards: manifest.local_only_cards,
            quarantined_panes: manifest.quarantined_panes,
            pending_chat_admissions: manifest.pending_chat_admissions,
            pending_terminal_creations: manifest.pending_terminal_creations,
        };
        state.pending_chat_admissions.retain(|id, source| {
            state.cards.iter().any(|card| {
                card.id == *id
                    && card.existence.is_open()
                    && matches!(&card.kind, CardKind::Chat(session) if session == source)
            })
        });
        if !warnings.is_empty() {
            retarget_dropped_cards(&mut state);
        }
        // A stored unread owner cannot keep a worktree in Attention when its
        // card failed to restore, was closed, or no longer belongs here.
        if !state.agent_updates.is_empty() {
            let open: HashSet<_> = state
                .cards
                .iter()
                .filter(|card| card.existence.is_open())
                .map(|card| card.id)
                .collect();
            state.agent_updates.retain(|card, _| open.contains(card));
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
                        location: None,
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

    /// Forgets an applied worktree creation, freeing its identity for a new
    /// one. Only an applied creation can be forgotten: anything unresolved
    /// still has effects to reconcile, and a rolled-back one holds nothing.
    pub fn release_worktree_creation(&self, id: OperationId) -> Result<()> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let changed = inner.connection.execute(
            "DELETE FROM pending_operations
             WHERE operation_id = ?1 AND entity_kind = 'worktree_creation'
             AND status = 'applied'",
            params![id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::InvalidState(format!(
                "operation {id} is not an applied worktree creation"
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
        self.operation_rows(true, None)
    }

    pub fn journaled_worktree_operations(&self) -> Result<Vec<PendingOperation>> {
        self.operation_rows(false, None)
    }

    /// Read one creation without loading every retained journal into memory.
    pub fn journaled_worktree_operation(
        &self,
        id: OperationId,
    ) -> Result<Option<PendingOperation>> {
        Ok(self.operation_rows(false, Some(id))?.into_iter().next())
    }

    fn operation_rows(
        &self,
        unresolved_only: bool,
        id: Option<OperationId>,
    ) -> Result<Vec<PendingOperation>> {
        let inner = self.lock_inner()?;
        ensure_open(&inner)?;
        let mut statement = inner.connection.prepare(
            "SELECT operation_id, entity_kind, entity_id, action, base_generation, payload_json,
                    status, created_at_unix_ms, updated_at_unix_ms
             FROM pending_operations
             WHERE ((?1 AND status IN ('pending', 'needs_reconciliation'))
                OR (NOT ?1 AND entity_kind = 'worktree_creation'))
                AND (?2 IS NULL OR operation_id = ?2)
             ORDER BY created_at_unix_ms, operation_id",
        )?;
        let rows = statement.query_map(
            params![unresolved_only, id.map(|id| id.to_string())],
            |row| {
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
            },
        )?;
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

    #[cfg(test)]
    pub(crate) fn audit_writer_holds_connection(&self) -> bool {
        matches!(
            self.inner.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
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

/// Read navigation identities without migrating or acquiring another window's
/// writer. Closed chats remain navigable through the conversation resume path.
pub fn read_navigable_session_ids(path: &Path) -> Result<Vec<TerminalId>> {
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(DEFAULT_BUSY_TIMEOUT)?;
    let transaction = connection.transaction()?;
    let mut statement = transaction.prepare(
        "SELECT c.kind_tag, c.content_json, c.existence_json, t.terminal_id
         FROM cards c LEFT JOIN terminal_recipes t ON t.card_id = c.card_id
         WHERE c.kind_tag IN ('chat', 'terminal')",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut ids = Vec::new();
    for row in rows {
        let identity: Result<Option<TerminalId>> = (|| {
            let (kind, content, existence, terminal) = row?;
            let open = from_json::<Existence>("session existence", &existence)?.is_open();
            match kind.as_str() {
                "terminal" if open => terminal.map(|id| parse_id("terminal", &id)).transpose(),
                "chat" => {
                    #[derive(serde::Deserialize)]
                    struct Identity {
                        id: TerminalId,
                        launch: LaunchIdentity,
                    }
                    #[derive(serde::Deserialize)]
                    struct LaunchIdentity {
                        session_id: Option<String>,
                    }
                    let Some(content) = content else {
                        return Ok(None);
                    };
                    let identity = from_json::<Identity>("chat identity", &content)?;
                    Ok((open || identity.launch.session_id.is_some()).then_some(identity.id))
                }
                _ => Ok(None),
            }
        })();
        match identity {
            Ok(Some(id)) => ids.push(id),
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "Skipping unreadable saved session identity"),
        }
    }
    Ok(ids)
}

/// Saved ownership evidence from another window. Directory scopes are not
/// project identities: callers must classify them against current shared roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SavedSessionOwner {
    Project(PathBuf),
    Directory(PathBuf),
}

/// One budget spans all source windows. Per-row bounds alone would allow
/// thousands of individually valid descriptors to exhaust updater memory.
#[cfg(any(feature = "host", test))]
pub(crate) struct MigrationInspectionBudget {
    remaining_rows: usize,
    remaining_bytes: u64,
    deadline: Instant,
}

#[cfg(any(feature = "host", test))]
impl MigrationInspectionBudget {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self {
            remaining_rows: crate::upgrade::handoff::MAX_SESSIONS,
            remaining_bytes: 64 * 1024 * 1024,
            deadline,
        }
    }

    fn check_deadline(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            return Err(StoreError::InvalidState(
                "Saved pane inspection exceeded its bounded budget".into(),
            ));
        }
        Ok(())
    }

    fn reserve(&mut self, bytes: u64) -> Result<()> {
        self.check_deadline()?;
        if self.remaining_rows == 0 || bytes > self.remaining_bytes {
            return Err(StoreError::InvalidState(
                "Saved pane inventory exceeds the migration inspection limit".into(),
            ));
        }
        self.remaining_rows -= 1;
        self.remaining_bytes -= bytes;
        Ok(())
    }
}

#[cfg(test)]
fn read_open_migration_sessions(path: &Path) -> Result<Vec<(CardId, CardKind)>> {
    read_open_migration_sessions_with_budget(
        path,
        &mut MigrationInspectionBudget::new(Instant::now() + Duration::from_secs(30)),
    )
}

/// Read exact open descriptors without migrating or writing a source database.
/// Refuse unreadable or over-budget rows instead of omitting recovery work.
#[cfg(any(feature = "host", test))]
pub(crate) fn read_open_migration_sessions_with_budget(
    path: &Path,
    budget: &mut MigrationInspectionBudget,
) -> Result<Vec<(CardId, CardKind)>> {
    // Local presentation markers are not daemon authority. Read them under
    // the same snapshot and budget as their exact saved source descriptors.
    fn retained_metadata<T: serde::de::DeserializeOwned + Default>(
        transaction: &Transaction<'_>,
        workspace: WorkspaceId,
        name: &str,
        budget: &mut MigrationInspectionBudget,
    ) -> Result<T> {
        budget.check_deadline()?;
        let key = format!("workspace:{workspace}:navigation:{name}");
        let bytes: Option<u64> = transaction
            .query_row(
                "SELECT length(CAST(value AS BLOB)) FROM metadata WHERE key = ?1",
                [&key],
                |row| row.get(0),
            )
            .optional()?;
        let Some(bytes) = bytes else {
            return Ok(T::default());
        };
        if bytes > 4 * 1024 * 1024 {
            return Err(StoreError::InvalidState(
                "Saved pane admission metadata exceeds the migration inspection limit".into(),
            ));
        }
        if bytes > budget.remaining_bytes {
            return Err(StoreError::InvalidState(
                "Saved pane inventory exceeds the migration inspection limit".into(),
            ));
        }
        budget.remaining_bytes -= bytes;
        let value = metadata(transaction, &key)?.ok_or_else(|| {
            StoreError::InvalidState("Saved pane admission metadata disappeared".into())
        })?;
        let value = from_json("saved pane admission metadata", &value)?;
        budget.check_deadline()?;
        Ok(value)
    }

    budget.check_deadline()?;
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_millis(100))?;
    let transaction = connection.transaction()?;
    let version: u32 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > CURRENT_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found: version,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }
    let (origin, listener, predecessor) = terminal_provenance_columns(version);
    let mut statement = transaction.prepare(&format!(
        "SELECT CASE WHEN length(CAST(c.card_id AS BLOB)) <= 128 THEN c.card_id ELSE NULL END, c.kind_tag,
         CASE WHEN length(CAST(c.content_json AS BLOB)) <= 4194304 THEN c.content_json ELSE NULL END,
         CASE WHEN length(CAST(c.existence_json AS BLOB)) <= 4096 THEN c.existence_json ELSE 'null' END,
         CASE WHEN length(CAST(w.existence_json AS BLOB)) <= 4096 THEN w.existence_json ELSE 'null' END,
         CASE WHEN c.kind_tag = 'chat' THEN length(CAST(c.content_json AS BLOB))
         ELSE (SELECT length(CAST(r.terminal_id AS BLOB)) + length(CAST(r.recipe_json AS BLOB))
                    + length(CAST(r.continuity_json AS BLOB)) + coalesce(length(CAST(r.resume_hint AS BLOB)),0)
                    + coalesce(length(CAST({origin} AS BLOB)),0) + coalesce(length(CAST({listener} AS BLOB)),0)
                    + coalesce(length(CAST({predecessor} AS BLOB)),0) + coalesce(length(CAST(t.path_json AS BLOB)),0)
               FROM terminal_recipes r LEFT JOIN transcript_metadata t ON t.terminal_id=r.terminal_id WHERE r.card_id=c.card_id)
         END,
         CASE WHEN length(CAST(w.workspace_id AS BLOB)) <= 128 THEN w.workspace_id ELSE NULL END
         FROM cards c JOIN workspaces w ON w.workspace_id = c.workspace_id
         WHERE c.kind_tag IN ('chat', 'terminal') ORDER BY c.card_id"
    ))?;
    let mut rows = statement.query([])?;
    let mut sessions = Vec::new();
    let mut retained = HashMap::<
        WorkspaceId,
        (
            HashSet<CardId>,
            HashMap<CardId, crate::chat::Session>,
            HashMap<CardId, CardKind>,
        ),
    >::new();
    while let Some(row) = rows.next()? {
        budget.check_deadline()?;
        let existence: String = row.get(3)?;
        let workspace: String = row.get(4)?;
        if !from_json::<Existence>("migration card existence", &existence)?.is_open()
            || !from_json::<Existence>("migration workspace existence", &workspace)?.is_open()
        {
            continue;
        }
        let bytes: u64 = row.get(5)?;
        // Reserve before allocating the descriptor's JSON or normalized fields.
        budget.reserve(bytes)?;
        let card: String = row.get(0)?;
        let kind: String = row.get(1)?;
        let content: Option<String> = row.get(2)?;
        let card = parse_id("migration card", &card)?;
        let session = read_migration_session_at(&transaction, card, &kind, content.as_deref())?;
        let workspace: String = row.get(6)?;
        let workspace = parse_id("migration workspace", &workspace)?;
        let (local, pending, quarantined) = match retained.entry(workspace) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let local = retained_metadata(&transaction, workspace, "local_only_cards", budget)?;
                let pending =
                    retained_metadata(&transaction, workspace, "pending_chat_admissions", budget)?;
                let quarantined =
                    retained_metadata(&transaction, workspace, "quarantined_panes", budget)?;
                entry.insert((local, pending, quarantined))
            }
        };
        if let Some(rejected) = quarantined.get(&card) {
            if rejected != &session {
                return Err(StoreError::InvalidState(
                    "A quarantined saved pane no longer matches its rejected source".into(),
                ));
            }
            // This is saved rejection evidence, not an actor to resolve or
            // resume. Actual live daemon inventory is qualified independently.
            continue;
        }
        if let CardKind::Chat(source) = &session {
            // A shared cache awaiting authority may still name a hosted actor.
            // Only a deliberately retained local copy with an exact source
            // marker is inert text rather than required hosted migration.
            if local.contains(&card) && pending.get(&card) == Some(source) {
                if !source.has_valid_native_continuity()
                    || (source.owned_codex_listener.is_some() && !source.owns_codex_listener())
                {
                    return Err(StoreError::InvalidState(
                        "Retained saved pane has invalid native continuity".into(),
                    ));
                }
                continue;
            }
        }
        sessions.push((card, session));
    }
    budget.check_deadline()?;
    Ok(sessions)
}

/// Decode the exact row inside an existing read-only inspection transaction.
/// Terminal ownership lives in normalized tables, not cards.content_json.
/// The caller must read and validate this card's kind/existence in that same
/// transaction; this helper never opens a store or changes its schema/state.
pub(crate) fn read_migration_session_at(
    connection: &Connection,
    card: CardId,
    kind: &str,
    content: Option<&str>,
) -> Result<CardKind> {
    const MAX_DESCRIPTOR_BYTES: usize = 4 * 1024 * 1024;
    match kind {
        "terminal" => {
            let version: u32 =
                connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
            if version > CURRENT_SCHEMA_VERSION {
                return Err(StoreError::UnsupportedSchema {
                    found: version,
                    supported: CURRENT_SCHEMA_VERSION,
                });
            }
            let (origin, listener, predecessor) = terminal_provenance_columns(version);
            // Check lengths in SQLite before allocating any descriptor fields
            // in Rust, including the nullable provenance and transcript path.
            let bounded:Option<bool>=connection.query_row(
                &format!("SELECT length(CAST(r.terminal_id AS BLOB)) + length(CAST(r.recipe_json AS BLOB))
                    + length(CAST(r.continuity_json AS BLOB)) + coalesce(length(CAST(r.resume_hint AS BLOB)),0)
                    + coalesce(length(CAST({origin} AS BLOB)),0) + coalesce(length(CAST({listener} AS BLOB)),0)
                    + coalesce(length(CAST({predecessor} AS BLOB)),0) + coalesce(length(CAST(t.path_json AS BLOB)),0) <= ?2
                 FROM terminal_recipes r LEFT JOIN transcript_metadata t ON t.terminal_id=r.terminal_id WHERE r.card_id=?1"),
                params![card.to_string(),MAX_DESCRIPTOR_BYTES as u64],|row|row.get(0)).optional()?;
            if bounded != Some(true) {
                return Err(StoreError::corrupt_row(
                    format!("migration terminal card {card}"),
                    "missing or oversized immutable descriptor",
                ));
            }
            load_terminal_record_at_schema(connection, card, version).map(CardKind::Terminal)
        }
        "chat" => from_json(
            "migration chat",
            content
                .filter(|value| value.len() <= MAX_DESCRIPTOR_BYTES)
                .ok_or_else(|| {
                    StoreError::corrupt_row(
                        format!("migration chat card {card}"),
                        "missing or oversized session descriptor",
                    )
                })?,
        )
        .map(CardKind::Chat),
        _ => Err(StoreError::InvalidState(
            "Migration inspection requires a saved native or terminal pane".into(),
        )),
    }
}

/// Read one consistent snapshot without migrating or writing another window's
/// database. Only open cards with explicit navigation ownership contribute.
pub fn read_project_session_owners(path: &Path) -> Result<Vec<(TerminalId, SavedSessionOwner)>> {
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(DEFAULT_BUSY_TIMEOUT)?;
    let transaction = connection.transaction()?;
    let mut projects = HashMap::<ProjectId, PathBuf>::new();
    let mut statement = transaction.prepare("SELECT project_id, repository_json FROM projects")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, path) = row?;
        projects.insert(
            parse_id("project", &id)?,
            from_json("project repository", &path)?,
        );
    }
    let mut statement = transaction.prepare("SELECT workspace_id FROM workspaces")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut owners = Vec::new();
    for id in ids {
        let Some(manifest) =
            load_workspace_manifest_connection(&transaction, parse_id("workspace", &id)?)?
        else {
            continue;
        };
        if !manifest.existence.is_open() {
            continue;
        }
        // Keep conflicting saved memberships visible. UI restore normalizes
        // them, but removal must retain every foreign ownership claim.
        let channels: Vec<ProjectChannel> = metadata(
            &transaction,
            &format!("workspace:{id}:navigation:{CHANNELS_FIELD}"),
        )?
        .map(|value| from_json("workspace channels", &value))
        .transpose()?
        .unwrap_or_default();
        for card in manifest.cards.iter().filter(|card| {
            card.existence.is_open()
                && matches!(
                    card.kind,
                    PersistedCardKind::Terminal | PersistedCardKind::Chat
                )
        }) {
            let mut card_owners = Vec::new();
            for channel in channels
                .iter()
                .filter(|channel| channel.cards.contains(&card.id))
            {
                card_owners.push(SavedSessionOwner::Project(
                    projects
                        .get(&channel.project_id)
                        .ok_or_else(|| {
                            StoreError::InvalidState(format!(
                                "channel {} refers to missing project {}",
                                channel.id, channel.project_id
                            ))
                        })?
                        .clone(),
                ));
            }
            if card_owners.is_empty()
                && let Some(directory) = manifest.card_scopes.get(&card.id)
            {
                card_owners.push(SavedSessionOwner::Directory(directory.clone()));
            }
            if card_owners.is_empty() {
                continue;
            }
            // Identity-only reads keep unopened older window schemas readable
            // when terminal recipes gain new fields in a later migration.
            let ids = match card.kind {
                PersistedCardKind::Terminal => {
                    let id: String = transaction.query_row(
                        "SELECT terminal_id FROM terminal_recipes WHERE card_id = ?1",
                        [card.id.to_string()],
                        |row| row.get(0),
                    )?;
                    vec![parse_id("terminal", &id)?]
                }
                PersistedCardKind::Chat => {
                    #[derive(serde::Deserialize)]
                    struct Identity {
                        id: TerminalId,
                    }
                    #[derive(serde::Deserialize)]
                    struct ChatIdentity {
                        id: TerminalId,
                        terminal: Option<Identity>,
                    }
                    let content: String = transaction.query_row(
                        "SELECT content_json FROM cards WHERE card_id = ?1",
                        [card.id.to_string()],
                        |row| row.get(0),
                    )?;
                    let chat: ChatIdentity = from_json("chat ownership", &content)?;
                    let mut ids = vec![chat.id];
                    if let Some(terminal) = chat.terminal {
                        ids.push(terminal.id);
                    }
                    ids
                }
                _ => unreachable!("only terminal and chat summaries were selected"),
            };
            for id in ids {
                owners.extend(card_owners.iter().cloned().map(|owner| (id, owner)));
            }
        }
    }
    Ok(owners)
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
    let mut statement = connection.prepare(
        "SELECT content_json FROM cards WHERE kind_tag = 'terminal' AND content_json IS NOT NULL",
    )?;
    for row in statement.query_map([], |row| row.get::<_, String>(0))? {
        let terminal: TerminalSession = from_json("archived terminal", &row?)?;
        live.insert(terminal.id.to_string());
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

// SQLite locks a database and its wal-index with POSIX advisory locks, and
// closing any descriptor of a file drops every such lock the process holds
// on it. A store's files are therefore checked by path, never opened: a
// descriptor opened and closed beside a live connection would leave that
// connection unlocked, free for another to checkpoint and delete its WAL.

/// Checks, without opening it, that the database at `path` is a private
/// regular file of this user.
fn enforce_private_database(path: &Path) -> io::Result<()> {
    if migrate_existing_private_file(path)? {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} does not exist", path.display()),
        ))
    }
}

/// Checks, without opening them, whichever of the database's `-wal` and
/// `-shm` files exist.
fn enforce_sqlite_sidecars(path: &Path) -> io::Result<()> {
    for suffix in ["-wal", "-shm"] {
        migrate_existing_private_file(&sqlite_sidecar_path(path, suffix))?;
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
    if !migrate_existing_private_file(path)? {
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

    verify_connection_profile(connection)
}

// Read-only verification: never repair a connection while admitting a writer.
fn verify_connection_profile(connection: &Connection) -> Result<()> {
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

// Descriptor CAS is a separate UI StateStore writer, not a Host Registry
// transaction. Its already-open connection may have waited behind a newer
// application. Read admission from THIS database under its actual writer
// reservation, before even the idempotent descriptor-return path. Never
// migrate, stamp or repair here. Other StateStore writers are not covered by
// this helper until they explicitly adopt it.
fn require_current_descriptor_write(transaction: &Transaction<'_>) -> Result<()> {
    if transaction.transaction_state(Some("main"))? != rusqlite::TransactionState::Write {
        return Err(StoreError::InvalidState(
            "Descriptor replacement requires the main StateStore writer reservation".into(),
        ));
    }
    let found: u32 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > CURRENT_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }
    if found != CURRENT_SCHEMA_VERSION {
        return Err(StoreError::InvalidState(format!(
            "Descriptor replacement requires current StateStore schema {CURRENT_SCHEMA_VERSION}, found {found}"
        )));
    }
    verify_connection_profile(transaction)
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
        if current < 5 {
            transaction
                .execute_batch("ALTER TABLE terminal_recipes ADD COLUMN recovery_origin TEXT;")?;
        }
        if current < 6 {
            migrate_to_v6(&transaction)?;
        }
        if current < 7 {
            transaction.execute_batch(
                "ALTER TABLE projects ADD COLUMN kind_json TEXT NOT NULL DEFAULT '\"Git\"';",
            )?;
        }
        if current < 8 {
            transaction.execute_batch(
                "ALTER TABLE terminal_recipes ADD COLUMN owned_codex_listener TEXT;",
            )?;
        }
        if current < 9 {
            transaction.execute_batch(
                "CREATE TABLE review_cache (
                    repository TEXT NOT NULL,
                    number INTEGER NOT NULL CHECK(number > 0),
                    head TEXT NOT NULL,
                    viewer TEXT NOT NULL,
                    record TEXT NOT NULL,
                    saved_ms INTEGER NOT NULL,
                    byte_size INTEGER NOT NULL CHECK(byte_size >= 0),
                    write_epoch TEXT NOT NULL,
                    write_order INTEGER NOT NULL,
                    PRIMARY KEY(repository, number)
                 ) STRICT;
                 CREATE TABLE review_cache_files (
                    repository TEXT NOT NULL,
                    number INTEGER NOT NULL,
                    head TEXT NOT NULL,
                    path TEXT NOT NULL,
                    position INTEGER NOT NULL,
                    entry TEXT NOT NULL,
                    viewed INTEGER NOT NULL CHECK(viewed IN (0, 1)),
                    unverified INTEGER NOT NULL CHECK(unverified IN (0, 1)),
                    PRIMARY KEY(repository, number, head, path),
                    FOREIGN KEY(repository, number) REFERENCES review_cache(repository, number) ON DELETE CASCADE
                 ) STRICT;",
            )?;
        }
        if current < 10 {
            transaction.execute_batch(
                "CREATE TABLE cards_v10 (
                    card_id TEXT PRIMARY KEY NOT NULL,
                    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
                    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
                    generation INTEGER NOT NULL CHECK (generation >= 0),
                    title TEXT NOT NULL,
                    width_fraction REAL NOT NULL CHECK (width_fraction > 0.0),
                    existence_json TEXT NOT NULL,
                    kind_tag TEXT NOT NULL CHECK (kind_tag IN (
                        'terminal', 'chat', 'browser', 'pull_requests', 'repository',
                        'settings', 'accounts', 'automations')),
                    content_json TEXT,
                    updated_at_unix_ms INTEGER NOT NULL,
                    content_hash INTEGER
                 ) STRICT;
                 INSERT INTO cards_v10 SELECT * FROM cards;
                 DROP TABLE cards;
                 ALTER TABLE cards_v10 RENAME TO cards;
                 CREATE INDEX cards_workspace_order_idx ON cards(workspace_id, ordinal);",
            )?;
        }
        if current < 11 {
            transaction
                .execute_batch("ALTER TABLE terminal_recipes ADD COLUMN predecessor TEXT;")?;
        }
        // Version 12 is a reader-admission boundary for payloads in existing
        // columns. The v11 layout needs no additional DDL or data rewrite.
        if current < 13 {
            transaction.execute_batch(
                "CREATE TABLE cards_v13 (
                    card_id TEXT PRIMARY KEY NOT NULL,
                    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
                    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
                    generation INTEGER NOT NULL CHECK (generation >= 0),
                    title TEXT NOT NULL,
                    width_fraction REAL NOT NULL CHECK (width_fraction > 0.0),
                    existence_json TEXT NOT NULL,
                    kind_tag TEXT NOT NULL CHECK (kind_tag IN (
                        'terminal', 'chat', 'browser', 'pull_requests', 'repository',
                        'settings', 'accounts', 'automations', 'work')),
                    content_json TEXT,
                    updated_at_unix_ms INTEGER NOT NULL,
                    content_hash INTEGER
                 ) STRICT;
                 INSERT INTO cards_v13 SELECT * FROM cards;
                 DROP TABLE cards;
                 ALTER TABLE cards_v13 RENAME TO cards;
                 CREATE INDEX cards_workspace_order_idx ON cards(workspace_id, ordinal);",
            )?;
        }
        // Version 14 changes reader admission only. Quarantine is stored in
        // existing metadata rows; the v13 layout/data needs no rewrite.
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

/// Admits the payload-free utility panes: `settings` and `accounts`. Both are
/// admitted by one rebuild so the accounts pane needs no second migration, and
/// a build that knows the kind can always read a database that holds one.
fn migrate_to_v6(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "CREATE TABLE cards_v6 (
            card_id TEXT PRIMARY KEY NOT NULL,
            workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
            ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
            generation INTEGER NOT NULL CHECK (generation >= 0),
            title TEXT NOT NULL,
            width_fraction REAL NOT NULL CHECK (width_fraction > 0.0),
            existence_json TEXT NOT NULL,
            kind_tag TEXT NOT NULL CHECK (
                kind_tag IN (
                    'terminal', 'chat', 'browser', 'pull_requests', 'repository',
                    'settings', 'accounts'
                )
            ),
            content_json TEXT,
            updated_at_unix_ms INTEGER NOT NULL,
            content_hash INTEGER
         ) STRICT;
         INSERT INTO cards_v6 SELECT * FROM cards;
         DROP TABLE cards;
         ALTER TABLE cards_v6 RENAME TO cards;
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
const SELECT_WORKSPACE_CARD_GENERATIONS: &str =
    "SELECT card_id, generation FROM cards WHERE workspace_id = ?1";
const SELECT_WORKSPACE_EXISTENCE: &str =
    "SELECT existence_json FROM workspaces WHERE workspace_id = ?1";
const SELECT_PROJECT_GENERATION: &str = "SELECT generation FROM projects WHERE project_id = ?1";
const SELECT_PROJECT_EXISTENCE: &str = "SELECT existence_json FROM projects WHERE project_id = ?1";
const SELECT_WORKTREE_GENERATION: &str = "SELECT generation FROM worktrees WHERE worktree_id = ?1";
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
        updated_at_unix_ms, recovery_origin, owned_codex_listener, predecessor
     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
     ON CONFLICT(terminal_id) DO UPDATE SET
        card_id = excluded.card_id,
        generation = excluded.generation,
        recipe_json = excluded.recipe_json,
        continuity_json = excluded.continuity_json,
        resume_hint = excluded.resume_hint,
        updated_at_unix_ms = excluded.updated_at_unix_ms,
        recovery_origin = excluded.recovery_origin,
        owned_codex_listener = excluded.owned_codex_listener,
        predecessor = excluded.predecessor";

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
    refused_closures: &mut RefusedClosures,
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
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:card_scopes"),
        &to_json(&state.card_scopes)?,
    )?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:workspace_history"),
        &to_json(&state.workspace_history)?,
    )?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:agent_updates"),
        &to_json(&state.agent_updates)?,
    )?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:chat_drafts"),
        &to_json(&state.chat_drafts)?,
    )?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:chat_draft_attachments"),
        &to_json(&state.chat_draft_attachments)?,
    )?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:repository_navigation"),
        &to_json(&state.repository_navigation)?,
    )?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:workspace_model_identity"),
        &to_json(&state.workspace_model_identity)?,
    )?;

    let pending = state
        .pending_chat_admissions
        .iter()
        .filter(|(id, source)| {
            state.cards.iter().any(|card| {
                card.id == **id
                    && card.existence.is_open()
                    && matches!(&card.kind, CardKind::Chat(session) if session == *source)
            })
        })
        .collect::<HashMap<_, _>>();
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:pending_chat_admissions"),
        &to_json(&pending)?,
    )?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:local_only_cards"),
        &to_json(&state.local_only_cards)?,
    )?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:quarantined_panes"),
        &to_json(&state.quarantined_panes)?,
    )?;
    set_metadata(
        transaction,
        &format!("workspace:{workspace_id}:navigation:pending_terminal_creations"),
        &to_json(&state.pending_terminal_creations)?,
    )?;

    let existing = existing_cards(transaction, &workspace_id)?;
    let incoming_ids = state
        .cards
        .iter()
        .map(|card| card.id)
        .collect::<BTreeSet<_>>();
    let mut card_generations = BTreeMap::new();

    for (ordinal, card) in state.cards.iter().enumerate() {
        let expected = base.cards.get(&card.id).copied().unwrap_or(0);
        let stored = existing.get(&card.id);
        let generation = if card.existence.is_open() {
            save_card(
                transaction,
                &workspace_id,
                ordinal,
                card,
                expected,
                stored,
                timestamp,
            )
        } else {
            save_closed_card(
                transaction,
                &workspace_id,
                ordinal,
                card,
                expected,
                stored,
                timestamp,
                refused_closures,
            )
        }
        .map_err(|error| error.in_card(card.id))?;
        card_generations.insert(card.id, generation);
    }

    for (card_id, stored) in &existing {
        if incoming_ids.contains(card_id) {
            continue;
        }
        let tombstone = Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: timestamp,
                reason: Some("absent from a newer workspace snapshot".into()),
            },
        };
        let generation = tombstone_card(transaction, *card_id, stored, &tombstone, timestamp)?;
        card_generations.insert(*card_id, generation);
    }

    Ok(WorkspaceCommit {
        generation: next,
        card_generations,
    })
}

/// Writes one card of a workspace snapshot, returning the generation storage
/// holds for it afterwards.
fn save_card(
    transaction: &Transaction<'_>,
    workspace_id: &str,
    ordinal: usize,
    card: &CardState,
    expected: u64,
    stored: Option<&ExistingCard>,
    timestamp: i64,
) -> Result<u64> {
    let kind = card_kind(&card.kind);
    let card_id = card.id.to_string();

    if stored.is_none() {
        // Only a card this workspace has never owned can collide with a row
        // belonging to another workspace; every stored card is ours by
        // construction, which keeps this lookup off the steady-state path
        // entirely.
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

    let write = CardWrite::build(workspace_id, &card_id, ordinal, card, kind)?;
    if stored.is_some_and(|stored| stored.content_hash == Some(write.content_hash)) {
        // Every value this write would bind, apart from `generation` and
        // `updated_at_unix_ms`, is already stored byte for byte. Both of
        // those columns describe the last *content* change, so leaving the
        // rows untouched is not an approximation of the write — it is the
        // write, with zero dirtied pages and zero WAL frames.
        return Ok(expected);
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
    Ok(card_generation)
}

/// Writes a card the snapshot holds closed.
///
/// A closed card is final, but its content still matters: restore reads which
/// terminal it claims, so a hosted terminal it ran is never adopted as a new
/// card. It is written in full when storage accepts that. When storage refuses
/// the content instead - a conversion, a rebound identity - the tombstone
/// alone is stored over what storage holds: a card the user closed must never
/// keep the rest of the window from saving, and closing is the remedy offered
/// for exactly that refusal.
///
/// The window goes on offering the card in every later snapshot. A refusal is
/// therefore remembered in `refused_closures`, and the same content offered
/// over the same row is neither written nor reported a second time.
#[allow(clippy::too_many_arguments)]
fn save_closed_card(
    transaction: &Transaction<'_>,
    workspace_id: &str,
    ordinal: usize,
    card: &CardState,
    expected: u64,
    stored: Option<&ExistingCard>,
    timestamp: i64,
    refused_closures: &mut RefusedClosures,
) -> Result<u64> {
    if let Some(stored) = stored
        && stored.kind == PersistedCardKind::Terminal
        && transaction.query_row(
            "SELECT content_json IS NOT NULL FROM cards WHERE card_id = ?1",
            [card.id.to_string()],
            |row| row.get::<_, bool>(0),
        )?
    {
        if expected != stored.generation {
            return Err(StoreError::StaleGeneration {
                entity: format!("card {}", card.id),
                expected,
                actual: stored.generation,
            });
        }
        // This tombstone's terminal is now owned by another open card. Keep
        // its archived historical descriptor; a stale snapshot must never
        // reclaim that live identity or erase the archive.
        return tombstone_card(transaction, card.id, stored, &card.existence, timestamp);
    }
    if let Some(refused) = refused_closures.get(&card.id)
        && refused.repeats(card, stored)
    {
        return Ok(refused.generation);
    }
    transaction.execute_batch("SAVEPOINT closed_card")?;
    let written = save_card(
        transaction,
        workspace_id,
        ordinal,
        card,
        expected,
        stored,
        timestamp,
    );
    match written {
        Err(error) if error.refuses_content() => {
            transaction.execute_batch("ROLLBACK TO closed_card; RELEASE closed_card")?;
            tracing::warn!(
                card = %card.id,
                %error,
                "storage refused a closed card's final content; storing its tombstone alone"
            );
            let generation = match stored {
                Some(stored) => {
                    tombstone_card(transaction, card.id, stored, &card.existence, timestamp)?
                }
                // Storage never held the card, so there is nothing to close.
                None => 0,
            };
            // Recorded before the transaction commits, which is sound: a
            // rollback leaves the row at an earlier generation or still open,
            // and neither matches what is remembered here.
            refused_closures.insert(
                card.id,
                RefusedClosure {
                    offered: card.clone(),
                    generation,
                },
            );
            Ok(generation)
        }
        written => {
            transaction.execute_batch("RELEASE closed_card")?;
            if written.is_ok() {
                refused_closures.remove(&card.id);
            }
            written
        }
    }
}

/// Stores `tombstone` over the card row storage holds, leaving its content,
/// and returns the card's generation afterwards. A card storage already holds
/// closed stays as it is.
fn tombstone_card(
    transaction: &Transaction<'_>,
    card: CardId,
    stored: &ExistingCard,
    tombstone: &Existence,
    timestamp: i64,
) -> Result<u64> {
    if !stored.existence.is_open() {
        return Ok(stored.generation);
    }
    let generation = next_generation(stored.generation, "card", card)?;
    transaction
        .prepare_cached(TOMBSTONE_CARD)?
        .execute(params![
            u64_to_i64(generation, "card generation")?,
            to_json(tombstone)?,
            timestamp,
            card.to_string(),
        ])?;
    Ok(generation)
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
                || !(source_chat_launch_matches(source, session) || codex_cli_view(session, source))
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
            let proof = terminal.continuity.live_proof();
            let reattach = session.launch.provider == crate::agent::Provider::Codex
                && proof.is_some()
                && match &session.terminal {
                    Some(source) => {
                        terminal.id == source.id
                            && terminal.launch == source.launch
                            && proof == source.continuity.live_proof()
                    }
                    None => codex_cli_view(&session, terminal),
                };
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

/// Whether `view` is the CLI the daemon opened for a native Codex thread.
///
/// A native thread has no CLI until its return asks for one, and the daemon
/// then opens it under the chat's own identity, running the chat's exact
/// resume, in the host session the chat was proven live in. That view is
/// the one source a chat may gain after it was saved, and the card may move
/// between the two like any chat and the CLI it came from.
fn codex_cli_view(session: &crate::chat::Session, view: &TerminalSession) -> bool {
    let proof = view.continuity.live_proof();
    session.launch.provider == crate::agent::Provider::Codex
        && view.id == session.id
        && view.owned_codex_listener == session.owned_codex_listener
        && exact_chat_resume(session).as_ref() == Some(&view.launch)
        && proof.is_some()
        && proof == session.proof
}

fn validate_chat_identity(
    transaction: &Transaction<'_>,
    card: CardId,
    session: &crate::chat::Session,
    previous: Option<PersistedCardKind>,
) -> Result<()> {
    if !session.has_valid_native_continuity() {
        return Err(StoreError::InvalidState(format!(
            "chat card {card} has invalid native continuity evidence"
        )));
    }
    if session.owned_codex_listener.is_some() && !session.owns_codex_listener() {
        return Err(StoreError::InvalidState(format!(
            "chat card {card} has invalid owned listener evidence"
        )));
    }
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
        let changed = if stored.id != session.id {
            Some("identity")
        } else if stored.launch.provider != session.launch.provider {
            Some("provider")
        } else if stored.native_continuity.retains_provider()
            && stored.native_continuity != session.native_continuity
        {
            Some("native continuity")
        } else if stored.owned_codex_listener != session.owned_codex_listener
            && !(stored.owned_codex_listener.is_none()
                && session.owns_codex_listener()
                && session
                    .proof
                    .as_ref()
                    .is_some_and(HostSessionProof::is_complete))
        {
            Some("owned listener")
        } else if stored.launch.recipe != session.launch.recipe
            // The daemon gives a new Codex thread its own listener as it
            // starts it, so the started thread comes back naming that
            // endpoint where it had asked for one.
            && !stored.resolved_to_owned_codex_listener(session)
        {
            Some("launch recipe")
        } else if stored.terminal != session.terminal
            && !session.retains_native_pair_from(&stored)
            && !(stored.terminal.is_none()
                && session
                    .terminal
                    .as_ref()
                    .is_some_and(|view| codex_cli_view(session, view)))
        {
            Some("source terminal")
        } else if stored.launch.session_id != session.launch.session_id
            && (stored.launch.session_id.is_some() || stored.has_unnamed_retained_claude_actor())
            && (stored.proof.is_none() || stored.proof != session.proof)
        {
            Some("provider conversation")
        } else {
            None
        };
        if let Some(changed) = changed {
            return Err(StoreError::InvalidState(format!(
                "chat card {card} changed conversation identity ({changed})"
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
        recovery_origin: Option<String>,
        predecessor: Option<String>,
        owned_codex_listener: Option<String>,
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
    /// `cards.content_json` column, and the utility panes have none, so there
    /// is no side table to write.
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
        if let CardKind::Terminal(terminal) = &card.kind
            && terminal.owned_codex_listener.is_some()
            && !terminal.owns_codex_listener()
        {
            return Err(StoreError::InvalidState(format!(
                "terminal card {card_id} has invalid owned listener evidence"
            )));
        }
        let existence_json = to_json(&card.existence)?;
        let content_json = match &card.kind {
            CardKind::Chat(session) => Some(to_json(session)?),
            CardKind::PullRequests(view) => Some(to_json(view)?),
            CardKind::Repository(view) => Some(to_json(view)?),
            CardKind::Terminal(_)
            | CardKind::Browser(_)
            | CardKind::Settings
            | CardKind::Accounts
            | CardKind::Automations
            | CardKind::Work => None,
        };
        let payload = match &card.kind {
            CardKind::Terminal(terminal) => CardPayloadWrite::Terminal {
                terminal_id: terminal.id.to_string(),
                recovery_origin: terminal.recovery_origin.map(|id| id.to_string()),
                predecessor: terminal.predecessor.map(|id| id.to_string()),
                owned_codex_listener: terminal
                    .owned_codex_listener
                    .as_ref()
                    .map(to_json)
                    .transpose()?,
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
            CardKind::Chat(_)
            | CardKind::PullRequests(_)
            | CardKind::Repository(_)
            | CardKind::Settings
            | CardKind::Accounts
            | CardKind::Automations
            | CardKind::Work => CardPayloadWrite::Inline,
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
                recovery_origin,
                predecessor,
                owned_codex_listener,
                recipe_json,
                continuity_json,
                resume_hint,
                transcript_path_json,
            } => {
                hasher
                    .text("terminal")
                    .text(terminal_id)
                    .optional_text(recovery_origin.as_deref())
                    .optional_text(predecessor.as_deref())
                    .optional_text(owned_codex_listener.as_deref())
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
            recovery_origin,
            predecessor,
            owned_codex_listener,
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
                    recovery_origin,
                    owned_codex_listener,
                    predecessor,
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

fn replace_chat_identity_transaction(
    transaction: &Transaction<'_>,
    card_id: CardId,
    expected_id: TerminalId,
    replacement: &crate::chat::Session,
) -> Result<()> {
    let Some((kind, content)) = transaction
        .query_row(
            "SELECT kind_tag, content_json FROM cards WHERE card_id = ?1",
            [card_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?
    else {
        // The card has not reached storage yet; the snapshot following this
        // operation stores it with the replacement as its first conversation.
        return Ok(());
    };
    if kind != PersistedCardKind::Chat.as_str() {
        return Err(StoreError::InvalidState(format!(
            "card {card_id} is not a chat card"
        )));
    }
    let content = content.ok_or_else(|| {
        StoreError::corrupt_row("chat payload", format!("card {card_id} has no payload"))
    })?;
    let stored: crate::chat::Session = from_json("chat card", &content)?;
    if &stored == replacement {
        return Ok(());
    }
    if stored.id != expected_id {
        return Err(StoreError::InvalidState(format!(
            "chat card {card_id} expected conversation {expected_id}, found {}",
            stored.id
        )));
    }
    // Only a conversation that has not begun replaces another this way. One
    // that names a native session, or a daemon that proved it, has to reach
    // storage through a snapshot, which checks that proof.
    if replacement.launch.session_id.is_some() || replacement.proof.is_some() {
        return Err(StoreError::InvalidState(format!(
            "chat card {card_id} can only be replaced by a conversation that has not begun"
        )));
    }
    // It may keep the CLI the card already pairs with, launched the same way
    // under the same identity, but never gain one.
    if replacement.terminal.is_some()
        && (replacement.terminal != stored.terminal
            || replacement.id != stored.id
            || replacement.launch.provider != stored.launch.provider
            || replacement.launch.recipe != stored.launch.recipe)
    {
        return Err(StoreError::InvalidState(format!(
            "chat card {card_id} cannot take a source terminal with a new conversation"
        )));
    }
    transaction.execute(
        "UPDATE cards SET content_json = ?1, updated_at_unix_ms = ?2 WHERE card_id = ?3",
        params![to_json(replacement)?, now_unix_ms(), card_id.to_string()],
    )?;
    // As with a terminal relaunch, the payload changed behind the snapshot
    // lane's back, so the next snapshot must take the full write path.
    transaction.execute(INVALIDATE_CARD_HASH, [card_id.to_string()])?;
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

    let archived_owner = if let Some((existing_card, existence)) = transaction
        .query_row(
            "SELECT t.card_id, c.existence_json FROM terminal_recipes t
             JOIN cards c ON c.card_id = t.card_id WHERE t.terminal_id = ?1",
            [replacement.id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        && existing_card != card_id.to_string()
    {
        let existence: Existence = from_json("card existence", &existence)?;
        if !matches!(existence, Existence::Closed { .. }) {
            return Err(StoreError::InvalidState(format!(
                "terminal_id {} already belongs to card {existing_card}",
                replacement.id
            )));
        }
        Some(parse_id::<CardId>("card", &existing_card)?)
    } else {
        None
    };

    let Some(kind) = transaction
        .query_row(
            "SELECT kind_tag FROM cards WHERE card_id = ?1",
            [card_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        if archived_owner.is_some() {
            return Err(StoreError::InvalidState(format!(
                "terminal card {card_id} must be saved before adopting a closed card's terminal"
            )));
        }
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
            StoreError::corrupt_row(
                "terminal payload",
                format!("card {card_id} has no terminal payload"),
            )
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
    if let Some(owner) = archived_owner {
        let archived = load_terminal_record(transaction, owner)?;
        transaction.execute(
            "UPDATE cards SET content_json = ?1, content_hash = NULL, updated_at_unix_ms = ?2
             WHERE card_id = ?3",
            params![to_json(&archived)?, timestamp, owner.to_string()],
        )?;
    }
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
    // A closed historical owner has just been archived. Updating this same
    // terminal row transfers ownership without dropping its transcript metadata.
    transaction.execute(
        UPSERT_TERMINAL_RECIPE,
        params![
            replacement.id.to_string(),
            card_id.to_string(),
            generation,
            recipe_json,
            continuity_json,
            replacement.resume_hint,
            timestamp,
            replacement.recovery_origin.map(|id| id.to_string()),
            replacement
                .owned_codex_listener
                .as_ref()
                .map(to_json)
                .transpose()?,
            replacement.predecessor.map(|id| id.to_string()),
        ],
    )?;
    if let Some(path) = &replacement.transcript_path {
        transaction.execute(
            UPSERT_TRANSCRIPT_PATH,
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

/// What every workspace snapshot has to satisfy before any of it is written.
pub(crate) fn validate_workspace(state: &WorkspaceState) -> Result<()> {
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

    let mut channels = metadata(
        connection,
        &format!("workspace:{id}:navigation:{CHANNELS_FIELD}"),
    )?
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
        schema_version: u32::try_from(schema_version)
            .map_err(|_| StoreError::corrupt_row("workspace schema version", schema_version))?,
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
        card_scopes: metadata(
            connection,
            &format!("workspace:{id}:navigation:card_scopes"),
        )?
        .map(|value| from_json("workspace card scopes", &value))
        .transpose()?
        .unwrap_or_default(),
        workspace_history: metadata(
            connection,
            &format!("workspace:{id}:navigation:workspace_history"),
        )?
        .map(|value| from_json("workspace history", &value))
        .transpose()?
        .unwrap_or_default(),
        repository_navigation: metadata(
            connection,
            &format!("workspace:{id}:navigation:repository_navigation"),
        )?
        .map(|value| from_json("workspace editor navigation", &value))
        .transpose()?
        .unwrap_or_default(),
        agent_updates: metadata(
            connection,
            &format!("workspace:{id}:navigation:agent_updates"),
        )?
        .map(|value| from_json("workspace unread agent updates", &value))
        .transpose()?
        .unwrap_or_default(),
        chat_drafts: metadata(
            connection,
            &format!("workspace:{id}:navigation:chat_drafts"),
        )?
        .map(|value| from_json("workspace chat drafts", &value))
        .transpose()?
        .unwrap_or_default(),
        chat_draft_attachments: metadata(
            connection,
            &format!("workspace:{id}:navigation:chat_draft_attachments"),
        )?
        .map(|value| from_json("workspace chat image drafts", &value))
        .transpose()?
        .unwrap_or_default(),
        local_only_cards: metadata(
            connection,
            &format!("workspace:{id}:navigation:local_only_cards"),
        )?
        .map(|value| from_json("local-only pane identities", &value))
        .transpose()?
        .unwrap_or_default(),
        quarantined_panes: metadata(
            connection,
            &format!("workspace:{id}:navigation:quarantined_panes"),
        )?
        .map(|value| from_json("quarantined saved pane sources", &value))
        .transpose()?
        .unwrap_or_default(),
        pending_chat_admissions: metadata(
            connection,
            &format!("workspace:{id}:navigation:pending_chat_admissions"),
        )?
        .map(|value| from_json("pending shared chat admission", &value))
        .transpose()?
        .unwrap_or_default(),
        pending_terminal_creations: metadata(
            connection,
            &format!("workspace:{id}:navigation:pending_terminal_creations"),
        )?
        .map(|value| from_json("pending terminal creation identities", &value))
        .transpose()?
        .unwrap_or_default(),
        workspace_model_identity: metadata(
            connection,
            &format!("workspace:{id}:navigation:workspace_model_identity"),
        )?
        .map(|value| from_json("workspace model identity", &value))
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
        PersistedCardKind::Terminal
            if content.is_some()
                && matches!(
                    from_json::<Existence>("card existence", &existence)?,
                    Existence::Closed { .. }
                ) =>
        {
            CardKind::Terminal(from_json("archived terminal", content.as_deref().unwrap())?)
        }
        PersistedCardKind::Terminal => {
            CardKind::Terminal(load_terminal(connection, id, current_boot, proof)?)
        }
        PersistedCardKind::Chat => {
            let session: crate::chat::Session = from_json(
                "chat card",
                content.as_deref().ok_or_else(|| {
                    StoreError::corrupt_row(format!("chat card {id}"), "missing payload")
                })?,
            )?;
            if !session.has_valid_native_continuity() {
                return Err(StoreError::corrupt_row(
                    format!("chat card {id}"),
                    "invalid native continuity evidence",
                ));
            }
            CardKind::Chat(session)
        }
        PersistedCardKind::Browser => CardKind::Browser(load_browser(connection, id)?),
        PersistedCardKind::PullRequests => CardKind::PullRequests(from_json(
            "pull request card",
            content.as_deref().ok_or_else(|| {
                StoreError::corrupt_row(format!("pull request card {id}"), "missing payload")
            })?,
        )?),
        PersistedCardKind::Repository => CardKind::Repository(from_json(
            "repository card",
            content.as_deref().ok_or_else(|| {
                StoreError::corrupt_row(format!("repository card {id}"), "missing payload")
            })?,
        )?),
        PersistedCardKind::Settings => CardKind::Settings,
        PersistedCardKind::Accounts => CardKind::Accounts,
        PersistedCardKind::Automations => CardKind::Automations,
        PersistedCardKind::Work => CardKind::Work,
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
    let mut terminal = load_terminal_record(connection, card_id)?;
    terminal.continuity =
        continuity_for_restore(terminal.continuity, &terminal.launch, current_boot, proof);
    Ok(terminal)
}

fn load_terminal_record(connection: &Connection, card_id: CardId) -> Result<TerminalSession> {
    load_terminal_record_at_schema(connection, card_id, CURRENT_SCHEMA_VERSION)
}

// Read-only upgrade inspection must not run DDL against old window copies.
// Missing historical fields mean no recorded provenance, never new authority.
fn terminal_provenance_columns(version: u32) -> (&'static str, &'static str, &'static str) {
    (
        if version >= 5 {
            "r.recovery_origin"
        } else {
            "NULL"
        },
        if version >= 8 {
            "r.owned_codex_listener"
        } else {
            "NULL"
        },
        if version >= 11 {
            "r.predecessor"
        } else {
            "NULL"
        },
    )
}

fn load_terminal_record_at_schema(
    connection: &Connection,
    card_id: CardId,
    version: u32,
) -> Result<TerminalSession> {
    let (origin, listener, predecessor) = terminal_provenance_columns(version);
    // `.optional()` rather than `?`: a terminal card whose recipe row is gone
    // is a per-card restore failure the caller can report, not a raw
    // `QueryReturnedNoRows` that used to abort the whole workspace restore.
    let recipe = connection
        .query_row(
            &format!("SELECT r.terminal_id, r.recipe_json, r.continuity_json, r.resume_hint, {origin}, {listener}, {predecessor}
             FROM terminal_recipes r WHERE r.card_id = ?1"),
            [card_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((
        terminal_id,
        recipe_json,
        continuity_json,
        resume_hint,
        recovery_origin,
        owned_codex_listener,
        predecessor,
    )) = recipe
    else {
        return Err(StoreError::corrupt_row(
            format!("terminal card {card_id}"),
            "missing launch recipe",
        ));
    };
    let terminal_id: TerminalId = parse_id("terminal", &terminal_id)?;
    let launch = from_json("terminal recipe", &recipe_json)?;
    let continuity = from_json("terminal continuity", &continuity_json)?;
    let transcript_path = connection
        .query_row(
            "SELECT path_json FROM transcript_metadata WHERE terminal_id = ?1",
            [terminal_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|path| from_json("transcript path", &path))
        .transpose()?;

    let terminal = TerminalSession {
        owned_codex_listener: owned_codex_listener
            .map(|value| from_json("owned Codex listener", &value))
            .transpose()?,
        recovery_origin: recovery_origin
            .map(|id| parse_id("terminal recovery origin", &id))
            .transpose()?,
        predecessor: predecessor
            .map(|id| parse_id("terminal predecessor", &id))
            .transpose()?,
        id: terminal_id,
        launch,
        transcript_path,
        resume_hint,
        continuity,
    };
    if terminal.owned_codex_listener.is_some() && !terminal.owns_codex_listener() {
        return Err(StoreError::corrupt_row(
            format!("terminal card {card_id}"),
            "invalid owned listener evidence",
        ));
    }
    Ok(terminal)
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
        return Err(StoreError::corrupt_row(
            format!("browser card {card_id}"),
            "missing restore state",
        ));
    };
    let fidelity = match fidelity.as_str() {
        "history_and_scroll" => BrowserRestoreFidelity::HistoryAndScroll,
        "url_only" => BrowserRestoreFidelity::UrlOnly,
        other => {
            return Err(StoreError::corrupt_row(
                "browser restore fidelity",
                format!("invalid value `{other}`"),
            ));
        }
    };
    let mut history: Vec<url::Url> = from_json("browser history", &history_json)?;
    let mut history_index = i64_to_usize(history_index, "browser history index")?;
    // A vector written by a build that predates the cap is trimmed on the way
    // in, so an oversized history is reclaimed instead of carried forever.
    crate::model::trim_browser_history(&mut history, &mut history_index);
    let browser = BrowserSession {
        id: parse_id("browser", &browser_id)?,
        current_url: url::Url::parse(&current_url)
            .map_err(|error| StoreError::corrupt_row("browser URL", error))?,
        history,
        history_index,
        scroll_x,
        scroll_y,
        fidelity,
    };
    validate_browser_descriptor(&browser)
        .map_err(|error| StoreError::corrupt_row(format!("browser card {card_id}"), error))?;
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
        CardKind::Settings => PersistedCardKind::Settings,
        CardKind::Accounts => PersistedCardKind::Accounts,
        CardKind::Automations => PersistedCardKind::Automations,
        CardKind::Work => PersistedCardKind::Work,
    }
}

/// Reads one stored generation. Takes a connection rather than a transaction so
/// the save path's checks and the rebase path's plain read share it; a
/// `Transaction` derefs to its `Connection`.
fn select_generation(connection: &Connection, sql: &'static str, id: &str) -> Result<Option<u64>> {
    connection
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
    serde_json::from_str(value).map_err(|error| StoreError::corrupt_row(kind, error))
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
    u64::try_from(value).map_err(|_| StoreError::corrupt_row(kind, "negative stored value"))
}

fn i64_to_usize(value: i64, kind: &str) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| StoreError::corrupt_row(kind, "stored value outside supported range"))
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

    /// The refusal storage attributed to `card` within a failed snapshot.
    fn card_refusal<T: std::fmt::Debug>(result: Result<T>, card: CardId) -> StoreError {
        match result {
            Err(StoreError::Card {
                card: refused,
                error,
            }) if refused == card => *error,
            other => panic!("expected storage to refuse card {card}, got {other:?}"),
        }
    }
    use crate::model::{
        BrowserId, BrowserSession, CardState, HostIncarnation, HostSessionId, LaunchRecipe,
        TerminalSession,
    };

    fn boot(value: &str) -> BootId {
        BootId::new(value)
    }

    const VISITOR_TEST: &str = "persistence::tests::another_process_visits_the_store";
    const VISITOR_ENV: &str = "CAGENTS_STORE_VISITOR_DATABASE";

    /// Run only as the child of `an_open_store_keeps_its_wal_from_other_processes`:
    /// another process - a second window, `sqlite3`, a backup - that opens the
    /// store read-write and closes it again.
    #[test]
    fn another_process_visits_the_store() {
        let Some(path) = std::env::var_os(VISITOR_ENV) else {
            return;
        };
        let path = PathBuf::from(path);
        let connection = Connection::open(&path).unwrap();
        let _: i64 = connection
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
            .unwrap();
        connection.close().unwrap();
        fs::write(sqlite_sidecar_path(&path, "-visited"), "visited").unwrap();
    }

    /// SQLite holds a store's POSIX locks for as long as the store is open,
    /// which is how a closing connection elsewhere knows it is not the last
    /// one and leaves the WAL alone. Opening the store must keep them: closing
    /// any descriptor of a file drops every POSIX lock the process holds on it.
    #[test]
    #[cfg(target_os = "linux")]
    fn an_open_store_keeps_its_wal_from_other_processes() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("visited.sqlite3");
        let store = StateStore::open(&path, boot("visited")).unwrap();
        store.save_window_view("before").unwrap();
        let wal = sqlite_sidecar_path(&path, "-wal");
        assert!(wal.exists());

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", VISITOR_TEST, "--test-threads=1"])
            .env(VISITOR_ENV, &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        // `--exact` matching nothing also exits zero.
        assert!(sqlite_sidecar_path(&path, "-visited").exists());

        // Taking itself for the last connection, the visitor would have
        // checkpointed the WAL and deleted it, and the store would go on
        // committing into a file nothing reads.
        assert!(wal.exists(), "the visitor deleted the open store's WAL");
        store.save_window_view("after").unwrap();
        let reader = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let view: String = reader
            .query_row(
                "SELECT value FROM metadata WHERE key = 'window_view_v1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(view, "after");
    }

    fn recipe() -> LaunchRecipe {
        LaunchRecipe {
            unset_environment: Default::default(),
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
                    owned_codex_listener: None,
                    recovery_origin: None,
                    predecessor: None,
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
            card_scopes: HashMap::new(),
            chat_drafts: HashMap::new(),
            chat_draft_attachments: HashMap::new(),
            agent_updates: BTreeMap::new(),
            workspace_history: Vec::new(),
            repository_navigation: HashMap::new(),
            workspace_model_identity: None,
            local_only_cards: HashSet::new(),
            quarantined_panes: Default::default(),
            pending_chat_admissions: Default::default(),
            pending_terminal_creations: HashSet::new(),
        }
    }

    const RETAINED_UI_DRAFT: &str = "Unsent λ thought\n  café — preserve spacing  ";

    fn retained_ui_format_workspace() -> (WorkspaceState, HostSessionProof) {
        let boot_id = boot("retained-ui-format-boot");
        let proof = HostSessionProof {
            host_session_id: "retained-ui-format-host".into(),
            boot_id: boot_id.clone(),
            host_incarnation: "retained-ui-format-incarnation".into(),
        };
        let mut state = workspace_with_terminal(&boot_id);
        state.id = "10000000-0000-4000-8000-000000000001".parse().unwrap();
        state.cards[0].id = "20000000-0000-4000-8000-000000000001".parse().unwrap();
        state.cards[0].title = "Unrelated shell".into();
        let CardKind::Terminal(terminal) = &mut state.cards[0].kind else {
            unreachable!()
        };
        terminal.id = "30000000-0000-4000-8000-000000000001".parse().unwrap();
        terminal.continuity = TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        let mut retained = state.cards[0].clone();
        retained.id = "20000000-0000-4000-8000-000000000002".parse().unwrap();
        retained.title = "Retained Claude with unsent draft".into();
        retained.kind = CardKind::Chat(crate::chat::Session {
            id: "30000000-0000-4000-8000-000000000002".parse().unwrap(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: vec!["--model".into(), "fixture-model".into()],
                    working_directory: "/private/retained-fixture".into(),
                    environment: BTreeMap::from([(
                        "CLAUDE_CONFIG_DIR".into(),
                        "/private/retained-account".into(),
                    )]),
                    unset_environment: ["ANTHROPIC_API_KEY".into()].into(),
                },
                session_id: Some("40000000-0000-4000-8000-000000000002".into()),
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        });
        let mut ordinary = retained.clone();
        ordinary.id = "20000000-0000-4000-8000-000000000003".parse().unwrap();
        ordinary.title = "Ordinary Codex conversation".into();
        let CardKind::Chat(chat) = &mut ordinary.kind else {
            unreachable!()
        };
        chat.id = "30000000-0000-4000-8000-000000000003".parse().unwrap();
        chat.launch.provider = crate::agent::Provider::Codex;
        chat.launch.recipe.program = "codex".into();
        chat.launch.recipe.arguments = vec!["--remote".into(), "unix://fixture".into()];
        chat.launch.recipe.environment =
            BTreeMap::from([("CODEX_HOME".into(), "/private/codex-account".into())]);
        chat.launch.recipe.unset_environment.clear();
        chat.launch.session_id = Some("40000000-0000-4000-8000-000000000003".into());
        chat.native_continuity = crate::chat::NativeContinuity::ReconnectOnly;
        let mut browser = retained.clone();
        browser.id = "20000000-0000-4000-8000-000000000004".parse().unwrap();
        browser.title = "Saved browser".into();
        browser.kind = CardKind::Browser(BrowserSession {
            id: "50000000-0000-4000-8000-000000000004".parse().unwrap(),
            current_url: Url::parse("https://example.org/retained-fixture").unwrap(),
            history: vec![
                Url::parse("https://example.org/").unwrap(),
                Url::parse("https://example.org/retained-fixture").unwrap(),
            ],
            history_index: 1,
            scroll_x: 3.0,
            scroll_y: 127.0,
            fidelity: BrowserRestoreFidelity::HistoryAndScroll,
        });
        state.focused = Some(retained.id);
        state
            .chat_drafts
            .insert(retained.id, RETAINED_UI_DRAFT.into());
        state
            .chat_drafts
            .insert(ordinary.id, "ordinary draft".into());
        state.cards.extend([retained, ordinary, browser]);
        (state, proof)
    }

    fn write_retained_ui_fixture(path: &Path) -> (WorkspaceState, HostSessionProof) {
        let (state, proof) = retained_ui_format_workspace();
        let store = StateStore::open(path, proof.boot_id.clone()).unwrap();
        SaveLane::default().save(&store, &state).unwrap();
        store.save_window_view("unrelated-window-view-λ").unwrap();
        store
            .save_dismissed_update("unrelated-update-marker")
            .unwrap();
        assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
        store.shutdown().unwrap();
        drop(store);
        assert_eq!(
            fs::metadata(sqlite_sidecar_path(path, "-wal"))
                .map(|file| file.len())
                .unwrap_or(0),
            0
        );
        (state, proof)
    }

    fn assert_retained_ui_fixture(
        store: &StateStore,
        state: &WorkspaceState,
        proof: &HostSessionProof,
    ) {
        let restored = store
            .load_workspace_restored(state.id, Some(proof))
            .unwrap()
            .unwrap();
        assert!(restored.warnings.is_empty(), "{:?}", restored.warnings);
        assert_eq!(
            restored
                .state
                .cards
                .iter()
                .map(|card| card.id)
                .collect::<Vec<_>>(),
            state.cards.iter().map(|card| card.id).collect::<Vec<_>>()
        );
        for (before, after) in state.cards.iter().zip(&restored.state.cards) {
            assert_eq!(after.kind, before.kind);
            assert_eq!(after.title, before.title);
            assert_eq!(after.existence, before.existence);
        }
        assert_eq!(restored.state.chat_drafts, state.chat_drafts);
        assert_eq!(
            store.load_window_view().unwrap().as_deref(),
            Some("unrelated-window-view-λ")
        );
        assert_eq!(
            store.load_dismissed_update().unwrap().as_deref(),
            Some("unrelated-update-marker")
        );
    }

    #[test]
    fn retained_ui_format_normal_writer_reopens_and_exports_only_when_requested() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::DirBuilderExt;
        let temporary = tempdir().unwrap();
        let export = std::env::var_os("CAGENTS_TEST_RETAINED_UI_EXPORT_DIR").map(PathBuf::from);
        let root = export.as_deref().unwrap_or(temporary.path());
        let head = export.as_ref().map(|_| {
            assert!(
                root.is_absolute(),
                "fixture export requires an absolute private path"
            );
            let head = std::env::var("CAGENTS_TEST_RETAINED_UI_PRODUCER_HEAD")
                .expect("fixture export requires a full producer head pinned by the runner");
            assert!(
                head.len() == 40 && head.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "full producer head required"
            );
            fs::DirBuilder::new().mode(0o700).create(root).expect(
                "fixture export must own a new directory; existing artifacts are never overwritten",
            );
            head
        });
        let path = root.join("state.sqlite3");
        let (state, proof) = write_retained_ui_fixture(&path);
        let reopened = StateStore::open(&path, proof.boot_id.clone()).unwrap();
        assert_retained_ui_fixture(&reopened, &state, &proof);
        reopened.shutdown().unwrap();
        drop(reopened);
        if let Some(head) = head {
            assert_eq!(
                fs::metadata(sqlite_sidecar_path(&path, "-wal"))
                    .map(|file| file.len())
                    .unwrap_or(0),
                0
            );
            let digest = format!("{:x}", Sha256::digest(fs::read(&path).unwrap()));
            let manifest = serde_json::json!({
                "head": head, "schema_version": CURRENT_SCHEMA_VERSION,
                "producer_version": crate::VERSION,
                "boot_id": proof.boot_id, "workspace_id": state.id,
                "retained_card_id": state.cards[1].id, "ordinary_card_id": state.cards[2].id,
                "retained_draft": RETAINED_UI_DRAFT,
                "card_order": state.cards.iter().map(|card| card.id).collect::<Vec<_>>(),
                "sha256": digest,
            });
            let manifest_path = root.join("manifest.json");
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&manifest_path)
                .unwrap();
            use std::io::Write;
            output
                .write_all(&serde_json::to_vec_pretty(&manifest).unwrap())
                .unwrap();
            output.sync_all().unwrap();
            drop(output);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
            fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o400)).unwrap();
            File::open(root).unwrap().sync_all().unwrap();
            fs::set_permissions(root, fs::Permissions::from_mode(0o500)).unwrap();
        }
    }

    fn logical_ui_rows(connection: &Connection) -> BTreeMap<String, Vec<Vec<String>>> {
        let tables = connection.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<std::result::Result<Vec<_>, _>>().unwrap();
        let mut result = BTreeMap::new();
        for table in tables {
            let quoted = table.replace('"', "\"\"");
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{quoted}\""))
                .unwrap();
            let columns = statement.column_count();
            let mut rows = statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|column| row.get_ref(column).map(|value| format!("{value:?}")))
                        .collect::<std::result::Result<Vec<_>, _>>()
                })
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            rows.sort();
            result.insert(table, rows);
        }
        result
    }

    #[test]
    fn v13_quarantine_reader_admission_migration_preserves_rows_and_schema() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("v13-admission.sqlite3");
        let mut workspace = workspace_with_terminal(&boot("v13-admission"));
        workspace
            .quarantined_panes
            .insert(workspace.cards[0].id, workspace.cards[0].kind.clone());
        let store = StateStore::open(&path, boot("v13-admission")).unwrap();
        SaveLane::default().save(&store, &workspace).unwrap();
        drop(store);
        let mut connection = Connection::open(&path).unwrap();
        configure_connection(&connection, DEFAULT_BUSY_TIMEOUT).unwrap();
        connection.pragma_update(None, "user_version", 13).unwrap();
        let before = logical_ui_rows(&connection);
        let schema = connection.query_row("SELECT group_concat(sql, char(10)) FROM (SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name)", [], |row| row.get::<_, String>(0)).unwrap();
        migrate(&mut connection).unwrap();
        assert_eq!(
            connection
                .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            CURRENT_SCHEMA_VERSION
        );
        assert_eq!(logical_ui_rows(&connection), before);
        assert_eq!(connection.query_row("SELECT group_concat(sql, char(10)) FROM (SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name)", [], |row| row.get::<_, String>(0)).unwrap(), schema);
        drop(connection);
        let reopened = StateStore::open(&path, boot("v14-admission")).unwrap();
        assert_eq!(
            reopened
                .load_workspace(workspace.id, None)
                .unwrap()
                .unwrap()
                .quarantined_panes,
            workspace.quarantined_panes
        );
    }

    #[test]
    fn synthetic_v11_layout_migration_is_format_only_and_preserves_rows() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        write_retained_ui_fixture(&path);
        let mut connection = Connection::open(&path).unwrap();
        configure_connection(&connection, DEFAULT_BUSY_TIMEOUT).unwrap();
        // Synthetic layout test only. Exported before/after compatibility
        // fixtures above always use actual normal-writer schema admission.
        connection.pragma_update(None, "user_version", 11).unwrap();
        let before = logical_ui_rows(&connection);
        let schema = connection.query_row("SELECT group_concat(sql, char(10)) FROM (SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name)", [], |row| row.get::<_, String>(0)).unwrap();
        migrate(&mut connection).unwrap();
        assert_eq!(
            connection
                .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            CURRENT_SCHEMA_VERSION
        );
        assert_eq!(logical_ui_rows(&connection), before);
        assert_eq!(connection.query_row("SELECT group_concat(sql, char(10)) FROM (SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name)", [], |row| row.get::<_, String>(0)).unwrap(), schema);
    }

    #[test]
    fn future_ui_format_refuses_before_startup_reconciliation() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        write_retained_ui_fixture(&path);
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
            .unwrap();
        let before = logical_ui_rows(&connection);
        drop(connection);
        assert!(
            matches!(StateStore::open(&path, boot("different-boot")), Err(StoreError::UnsupportedSchema { found, supported }) if found == CURRENT_SCHEMA_VERSION + 1 && supported == CURRENT_SCHEMA_VERSION)
        );
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .unwrap();
        assert_eq!(
            logical_ui_rows(&connection),
            before,
            "refusal precedes logical recovery/load/write effects; SQLite setup may still touch files"
        );
    }

    #[test]
    fn explicit_pending_terminal_creation_survives_store_restore() {
        let directory = tempdir().unwrap();
        let boot_id = boot("pending-creation");
        let store =
            StateStore::open(directory.path().join("state.sqlite3"), boot_id.clone()).unwrap();
        let mut state = workspace_with_terminal(&boot_id);
        let id = state.cards[0].id;
        state.pending_terminal_creations.insert(id);
        state.card_scopes.insert(id, PathBuf::from("/repo"));
        let mut lane = SaveLane::default();
        lane.save(&store, &state).unwrap();
        let restored = store.load_workspace(state.id, None).unwrap().unwrap();
        assert_eq!(restored.pending_terminal_creations, HashSet::from([id]));
        let (CardKind::Terminal(before), CardKind::Terminal(after)) =
            (&state.cards[0].kind, &restored.cards[0].kind)
        else {
            panic!()
        };
        assert_eq!(after.id, before.id);
        assert_eq!(after.launch, before.launch);
        assert_eq!(restored.card_scopes, state.card_scopes);
    }

    #[test]
    fn closed_terminal_owner_repair_is_narrow_cas_and_idempotent() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("closed-adoption");
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        let mut state = workspace_with_terminal(&boot_id);
        let alias = state.cards[0].id;
        let mut canonical = state.cards[0].clone();
        canonical.id = CardId::new();
        let CardKind::Terminal(session) = &canonical.kind else {
            panic!()
        };
        let terminal = session.id;
        let mut lane = SaveLane::default();
        lane.save(&store, &state).unwrap();
        assert!(
            StateStore::repair_closed_terminal_pane_owner(
                &path,
                &boot_id,
                state.id,
                &canonical,
                SnapshotBase {
                    workspace: lane.workspace,
                    cards: &lane.cards
                }
            )
            .is_err()
        );
        state.cards[0].existence = Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: 123,
                reason: Some("closed by user".into()),
            },
        };
        state.focused = None;
        state.chat_drafts.insert(alias, "historical draft".into());
        state
            .chat_drafts
            .insert(canonical.id, "unsaved canonical draft".into());
        state.pending_terminal_creations.insert(CardId::new());
        lane.save(&store, &state).unwrap();
        assert_eq!(
            store
                .closed_terminal_pane_adoption(&state, canonical.id, terminal)
                .unwrap(),
            Some(TerminalPaneAdoption {
                alias,
                canonical: canonical.id,
                terminal
            })
        );
        assert!(
            StateStore::repair_closed_terminal_pane_owner(
                &path,
                &boot("other"),
                state.id,
                &canonical,
                SnapshotBase {
                    workspace: lane.workspace,
                    cards: &lane.cards
                }
            )
            .is_err()
        );
        assert!(
            StateStore::repair_closed_terminal_pane_owner(
                &path,
                &boot_id,
                state.id,
                &canonical,
                SnapshotBase {
                    workspace: lane.workspace - 1,
                    cards: &lane.cards
                }
            )
            .is_err()
        );
        let mut conflicting = canonical.clone();
        let CardKind::Terminal(conflict) = &mut conflicting.kind else {
            panic!()
        };
        conflict.launch.program = "/unrelated".into();
        assert!(
            StateStore::repair_closed_terminal_pane_owner(
                &path,
                &boot_id,
                state.id,
                &conflicting,
                SnapshotBase {
                    workspace: lane.workspace,
                    cards: &lane.cards
                }
            )
            .is_err()
        );
        let mut unrelated = workspace_with_terminal(&boot_id);
        unrelated.cards[0].id = canonical.id;
        unrelated.focused = Some(canonical.id);
        let mut unrelated_lane = SaveLane::default();
        unrelated_lane.save(&store, &unrelated).unwrap();
        assert!(
            StateStore::repair_closed_terminal_pane_owner(
                &path,
                &boot_id,
                state.id,
                &canonical,
                SnapshotBase {
                    workspace: lane.workspace,
                    cards: &lane.cards
                }
            )
            .is_err()
        );
        // Use a fresh canonical id after proving a foreign target stays untouched.
        assert_eq!(
            store.stored_generations(unrelated.id).unwrap().workspace,
            unrelated_lane.workspace
        );
        canonical.id = CardId::new();
        let CardKind::Terminal(session) = &canonical.kind else {
            panic!()
        };
        let before_metadata: Vec<(String, String)> = {
            let inner = store.lock_inner().unwrap();
            inner
                .connection
                .prepare("SELECT key,value FROM metadata ORDER BY key")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        };
        let mut stale_cards = lane.cards.clone();
        stale_cards.insert(alias, 0);
        assert!(
            StateStore::repair_closed_terminal_pane_owner(
                &path,
                &boot_id,
                state.id,
                &canonical,
                SnapshotBase {
                    workspace: lane.workspace,
                    cards: &stale_cards
                }
            )
            .is_err()
        );
        assert_eq!(
            store.stored_generations(state.id).unwrap().workspace,
            lane.workspace
        );
        let commit = StateStore::repair_closed_terminal_pane_owner(
            &path,
            &boot_id,
            state.id,
            &canonical,
            SnapshotBase {
                workspace: lane.workspace,
                cards: &lane.cards,
            },
        )
        .unwrap()
        .unwrap();
        let archived = store.load_card(alias, None).unwrap().unwrap();
        assert_eq!(archived.existence, state.cards[0].existence);
        assert_eq!(archived.kind, state.cards[0].kind);
        let inner = store.lock_inner().unwrap();
        assert_eq!(
            load_terminal_record(&inner.connection, canonical.id).unwrap(),
            *session
        );
        let after_metadata: Vec<(String, String)> = inner
            .connection
            .prepare("SELECT key,value FROM metadata ORDER BY key")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(after_metadata, before_metadata);
        drop(inner);
        assert_eq!(
            StateStore::repair_closed_terminal_pane_owner(
                &path,
                &boot_id,
                state.id,
                &canonical,
                SnapshotBase {
                    workspace: commit.generation,
                    cards: &commit.card_generations
                }
            )
            .unwrap(),
            None
        );
        assert_eq!(
            store
                .closed_terminal_pane_adoption(&state, canonical.id, terminal)
                .unwrap(),
            None
        );
        assert_eq!(
            store.stored_generations(state.id).unwrap().workspace,
            commit.generation
        );
    }

    #[test]
    fn closed_terminal_offline_recipe_repair_requires_reviewed_source_and_preserves_history() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("offline-recipe");
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        let mut state = workspace_with_terminal(&boot_id);
        let alias = state.cards[0].id;
        let CardKind::Terminal(source) = state.cards[0].kind.clone() else {
            panic!()
        };
        state.cards[0].existence = Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: 123,
                reason: Some("closed by user".into()),
            },
        };
        state.focused = None;
        let mut lane = SaveLane::default();
        lane.save(&store, &state).unwrap();
        let mut canonical = state.cards[0].clone();
        canonical.id = CardId::new();
        canonical.existence = Existence::Open;
        let CardKind::Terminal(target) = &mut canonical.kind else {
            panic!()
        };
        target.launch.program = "omp".into();
        target.launch.arguments = vec!["--resume".into(), "exact-thread".into()];
        target.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: target.launch.clone(),
            last_exit: None,
        };
        let base = SnapshotBase {
            workspace: lane.workspace,
            cards: &lane.cards,
        };
        assert!(
            StateStore::repair_closed_terminal_pane_owner(
                &path, &boot_id, state.id, &canonical, base
            )
            .is_err()
        );
        let mut wrong_source = source.launch.clone();
        wrong_source.arguments.push("unreviewed".into());
        assert!(
            StateStore::repair_closed_terminal_pane_owner_with_offline_recipe(
                &path,
                &boot_id,
                state.id,
                &canonical,
                base,
                &wrong_source
            )
            .is_err()
        );
        let mut wrong_directory = canonical.clone();
        let CardKind::Terminal(target) = &mut wrong_directory.kind else {
            panic!()
        };
        target.launch.working_directory = "/different".into();
        target.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: target.launch.clone(),
            last_exit: None,
        };
        assert!(
            StateStore::repair_closed_terminal_pane_owner_with_offline_recipe(
                &path,
                &boot_id,
                state.id,
                &wrong_directory,
                base,
                &source.launch
            )
            .is_err()
        );
        let mut current = canonical.clone();
        let CardKind::Terminal(target) = &mut current.kind else {
            panic!()
        };
        target.continuity = source.continuity.clone();
        assert!(
            StateStore::repair_closed_terminal_pane_owner_with_offline_recipe(
                &path,
                &boot_id,
                state.id,
                &current,
                base,
                &source.launch
            )
            .is_err()
        );
        StateStore::repair_closed_terminal_pane_owner_with_offline_recipe(
            &path,
            &boot_id,
            state.id,
            &canonical,
            base,
            &source.launch,
        )
        .unwrap()
        .unwrap();
        let archived = store.load_card(alias, None).unwrap().unwrap();
        assert_eq!(archived.kind, CardKind::Terminal(source));
        assert_eq!(archived.existence, state.cards[0].existence);
        let inner = store.lock_inner().unwrap();
        let CardKind::Terminal(target) = &canonical.kind else {
            panic!()
        };
        assert_eq!(
            load_terminal_record(&inner.connection, canonical.id).unwrap(),
            *target
        );
    }

    #[test]
    fn closed_terminal_admission_selects_actual_owner_among_archives() {
        let directory = tempdir().unwrap();
        let boot_id = boot("historical-aliases");
        let store =
            StateStore::open(directory.path().join("state.sqlite3"), boot_id.clone()).unwrap();
        let mut state = workspace_with_terminal(&boot_id);
        let first = state.cards[0].id;
        let CardKind::Terminal(session) = &state.cards[0].kind else {
            panic!()
        };
        let terminal = session.id;
        let mut lane = SaveLane::default();
        lane.save(&store, &state).unwrap();
        let mut second = state.cards[0].clone();
        second.id = CardId::new();
        state.cards[0].existence = Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: Some("old alias".into()),
            },
        };
        state.cards.push(second.clone());
        state.focused = Some(second.id);
        let commit = store
            .save_workspace_with_terminal_adoptions(
                &state,
                SnapshotBase {
                    workspace: lane.workspace,
                    cards: &lane.cards,
                },
                &[TerminalPaneAdoption {
                    alias: first,
                    canonical: second.id,
                    terminal,
                }],
            )
            .unwrap();
        lane.workspace = commit.generation;
        lane.cards = commit.card_generations;
        state.cards[1].existence = Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: 2,
                reason: Some("closed by user".into()),
            },
        };
        state.focused = None;
        lane.save(&store, &state).unwrap();
        let canonical = CardId::new();
        assert_eq!(
            store
                .closed_terminal_pane_adoption(&state, canonical, terminal)
                .unwrap(),
            Some(TerminalPaneAdoption {
                alias: second.id,
                canonical,
                terminal
            })
        );
        state.cards[1].existence = Existence::Open;
        assert_eq!(
            store
                .closed_terminal_pane_adoption(&state, canonical, terminal)
                .unwrap(),
            None
        );
        state.cards[1].existence = Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: 999,
                reason: Some("changed local closure".into()),
            },
        };
        assert_eq!(
            store
                .closed_terminal_pane_adoption(&state, canonical, terminal)
                .unwrap(),
            None
        );

        // The writer resolves the actual second owner, not the first matching archive.
        state.cards[1].existence = store.load_card(second.id, None).unwrap().unwrap().existence;
        let mut target = second.clone();
        target.id = canonical;
        state.cards.push(target);
        state.focused = Some(canonical);
        store
            .save_workspace_with_terminal_panes(
                &state,
                SnapshotBase {
                    workspace: lane.workspace,
                    cards: &lane.cards,
                },
                &[],
                &[TerminalPaneRestoration {
                    canonical,
                    terminal,
                }],
            )
            .unwrap();
        assert!(
            !store
                .load_card(first, None)
                .unwrap()
                .unwrap()
                .existence
                .is_open()
        );
        assert!(
            !store
                .load_card(second.id, None)
                .unwrap()
                .unwrap()
                .existence
                .is_open()
        );
        let inner = store.lock_inner().unwrap();
        let owner: String = inner
            .connection
            .query_row(
                "SELECT card_id FROM terminal_recipes WHERE terminal_id=?1",
                [terminal.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner, canonical.to_string());
    }

    #[test]
    fn canonical_terminal_adoption_is_atomic_and_preserves_history() {
        let directory = tempdir().unwrap();
        let boot_id = boot("adoption");
        let store =
            StateStore::open(directory.path().join("state.sqlite3"), boot_id.clone()).unwrap();
        let mut state = workspace_with_terminal(&boot_id);
        let mut lane = SaveLane::default();
        lane.save(&store, &state).unwrap();
        let alias = state.cards[0].id;
        let CardKind::Terminal(session) = state.cards[0].kind.clone() else {
            panic!()
        };
        let canonical = CardId::new();
        let mut pane = state.cards[0].clone();
        pane.id = canonical;
        state.cards[0].existence = Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: Some("adopted".into()),
            },
        };
        state.cards.push(pane);
        state.focused = Some(canonical);
        state.chat_drafts.insert(canonical, "draft".into());
        let adoption = TerminalPaneAdoption {
            alias,
            canonical,
            terminal: session.id,
        };
        assert!(
            store
                .save_workspace(
                    &state,
                    SnapshotBase {
                        workspace: lane.workspace,
                        cards: &lane.cards
                    }
                )
                .is_err()
        );
        let stale = store.save_workspace_with_terminal_adoptions(
            &state,
            SnapshotBase {
                workspace: 0,
                cards: &lane.cards,
            },
            &[adoption],
        );
        assert!(stale.is_err());
        assert!(
            store
                .load_card(alias, None)
                .unwrap()
                .unwrap()
                .existence
                .is_open()
        );
        let commit = store
            .save_workspace_with_terminal_adoptions(
                &state,
                SnapshotBase {
                    workspace: lane.workspace,
                    cards: &lane.cards,
                },
                &[adoption],
            )
            .unwrap();
        lane.workspace = commit.generation;
        lane.cards = commit.card_generations;
        let old = store.load_card(alias, None).unwrap().unwrap();
        assert!(!old.existence.is_open());
        assert!(
            matches!(old.kind,CardKind::Terminal(ref old) if old.id==session.id && old.transcript_path==session.transcript_path)
        );
        let restored = store.load_workspace(state.id, None).unwrap().unwrap();
        assert_eq!(restored.focused, Some(canonical));
        assert_eq!(
            restored.chat_drafts.get(&canonical).map(String::as_str),
            Some("draft")
        );
        let inner = store.lock_inner().unwrap();
        let owner: String = inner
            .connection
            .query_row(
                "SELECT card_id FROM terminal_recipes WHERE terminal_id=?1",
                [session.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner, canonical.to_string());
        let transcript: String = inner
            .connection
            .query_row(
                "SELECT path_json FROM transcript_metadata WHERE terminal_id=?1",
                [session.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(transcript.contains("transcript"));
        drop(inner);
        let replay = store
            .save_workspace_with_terminal_adoptions(
                &state,
                SnapshotBase {
                    workspace: lane.workspace,
                    cards: &lane.cards,
                },
                &[adoption],
            )
            .unwrap();
        lane.workspace = replay.generation;
        lane.cards = replay.card_generations;
        lane.save(&store, &state).unwrap();
    }

    /// Tracks base generations across successive saves the way the writer lane
    /// in `session` does, so tests exercise the same call shape production does.
    #[derive(Debug, Default)]
    struct SaveLane {
        workspace: u64,
        cards: BTreeMap<CardId, u64>,
    }

    fn refused_terminal_owner_fixture(
        store: &StateStore,
        boot_id: &BootId,
    ) -> (WorkspaceState, SaveLane, TerminalPaneAdoption) {
        let mut state = workspace_with_terminal(boot_id);
        let mut lane = SaveLane::default();
        lane.save(store, &state).unwrap();
        state.cards[0].title = "Archived shell".into();
        state.cards[0].existence = Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: 784,
                reason: Some("user closed".into()),
            },
        };
        state.focused = None;
        lane.save(store, &state).unwrap();
        let mut live = state.cards[0].clone();
        live.id = CardId::new();
        live.generation = 0;
        live.title = "Unsaved live shell".into();
        live.existence = Existence::Open;
        let CardKind::Terminal(terminal) = &live.kind else {
            unreachable!()
        };
        let recovery = TerminalPaneAdoption {
            alias: state.cards[0].id,
            canonical: live.id,
            terminal: terminal.id,
        };
        state
            .chat_drafts
            .insert(live.id, "Unsent work that must survive recovery".into());
        state
            .card_scopes
            .insert(live.id, PathBuf::from("/owned-project"));
        state.focused = Some(live.id);
        state.cards.push(live);
        let refusal = card_refusal(lane.save(store, &state), recovery.canonical);
        assert!(refusal.to_string().contains("already belongs to card"));
        (state, lane, recovery)
    }

    #[test]
    fn ownership_recovery_saves_the_new_pane_and_preserves_its_closed_history() {
        let directory = tempdir().unwrap();
        let boot_id = boot("reviewed-recovery");
        let store =
            StateStore::open(directory.path().join("state.sqlite3"), boot_id.clone()).unwrap();
        let (state, lane, recovery) = refused_terminal_owner_fixture(&store, &boot_id);
        let archived = state.cards[0].clone();
        let commit = store
            .save_workspace_recovering_terminal_owner(
                &state,
                SnapshotBase {
                    workspace: lane.workspace,
                    cards: &lane.cards,
                },
                recovery,
            )
            .unwrap();
        assert!(commit.card_generations.contains_key(&recovery.canonical));
        let restored = store.load_workspace(state.id, None).unwrap().unwrap();
        assert_eq!(restored.chat_drafts, state.chat_drafts);
        assert_eq!(restored.card_scopes, state.card_scopes);
        assert_eq!(restored.focused, Some(recovery.canonical));
        let old = store.load_card(recovery.alias, None).unwrap().unwrap();
        assert_eq!(old.title, archived.title);
        assert_eq!(old.existence, archived.existence);
        assert_eq!(old.kind, archived.kind);
        let inner = store.lock_inner().unwrap();
        let owner: String = inner
            .connection
            .query_row(
                "SELECT card_id FROM terminal_recipes WHERE terminal_id=?1",
                [recovery.terminal.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner, recovery.canonical.to_string());
        assert_eq!(
            load_terminal_record(&inner.connection, recovery.canonical).unwrap(),
            match &state.cards[1].kind {
                CardKind::Terminal(session) => session.clone(),
                _ => unreachable!(),
            }
        );
    }

    #[test]
    fn ownership_recovery_rechecks_closed_owner_evidence_and_new_target() {
        for case in [
            "open owner",
            "changed tombstone",
            "stale source",
            "forged continuity",
            "saved target",
        ] {
            let directory = tempdir().unwrap();
            let boot_id = boot(case);
            let store =
                StateStore::open(directory.path().join("state.sqlite3"), boot_id.clone()).unwrap();
            let (mut state, lane, recovery) = refused_terminal_owner_fixture(&store, &boot_id);
            match case {
                "open owner" | "changed tombstone" => {
                    let existence = if case == "open owner" {
                        Existence::Open
                    } else {
                        Existence::Closed {
                            tombstone: Tombstone {
                                operation_id: None,
                                closed_at_unix_ms: 999,
                                reason: None,
                            },
                        }
                    };
                    store
                        .lock_inner()
                        .unwrap()
                        .connection
                        .execute(
                            "UPDATE cards SET existence_json=?1 WHERE card_id=?2",
                            params![to_json(&existence).unwrap(), recovery.alias.to_string()],
                        )
                        .unwrap();
                }
                "stale source" => {
                    store
                        .lock_inner()
                        .unwrap()
                        .connection
                        .execute(
                            "UPDATE cards SET generation=generation+1 WHERE card_id=?1",
                            [recovery.alias.to_string()],
                        )
                        .unwrap();
                }
                "forged continuity" => {
                    for card in &mut state.cards {
                        let CardKind::Terminal(session) = &mut card.kind else {
                            unreachable!()
                        };
                        session.continuity = TerminalContinuity::Live {
                            boot_id: boot_id.clone(),
                            host_session_id: HostSessionId::new("unreviewed"),
                            host_incarnation: HostIncarnation::new("unreviewed"),
                        };
                    }
                }
                "saved target" => {
                    let mut saved = state.clone();
                    saved.cards[1].kind = CardKind::Settings;
                    store
                        .save_workspace(
                            &saved,
                            SnapshotBase {
                                workspace: lane.workspace,
                                cards: &lane.cards,
                            },
                        )
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let before = store.stored_generations(state.id).unwrap();
            assert!(
                store
                    .save_workspace_recovering_terminal_owner(
                        &state,
                        SnapshotBase {
                            workspace: lane.workspace,
                            cards: &lane.cards
                        },
                        recovery
                    )
                    .is_err(),
                "{case}"
            );
            assert_eq!(
                store.stored_generations(state.id).unwrap(),
                before,
                "{case}"
            );
            let owner: String = store
                .lock_inner()
                .unwrap()
                .connection
                .query_row(
                    "SELECT card_id FROM terminal_recipes WHERE terminal_id=?1",
                    [recovery.terminal.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(owner, recovery.alias.to_string(), "{case}");
        }
    }

    #[test]
    fn ownership_recovery_rolls_back_the_transfer_when_the_full_snapshot_is_refused() {
        let directory = tempdir().unwrap();
        let boot_id = boot("atomic-recovery");
        let store =
            StateStore::open(directory.path().join("state.sqlite3"), boot_id.clone()).unwrap();
        let (mut state, lane, recovery) = refused_terminal_owner_fixture(&store, &boot_id);
        let mut invalid = state.cards[1].clone();
        invalid.id = CardId::new();
        invalid.kind = CardKind::Settings;
        invalid.width_fraction = 0.0;
        state.cards.push(invalid);
        let before = store.load_card(recovery.alias, None).unwrap().unwrap();
        assert!(
            store
                .save_workspace_recovering_terminal_owner(
                    &state,
                    SnapshotBase {
                        workspace: lane.workspace,
                        cards: &lane.cards
                    },
                    recovery
                )
                .is_err()
        );
        assert!(store.load_card(recovery.canonical, None).unwrap().is_none());
        assert_eq!(
            store.load_card(recovery.alias, None).unwrap().unwrap(),
            before
        );
        assert_eq!(
            store.stored_generations(state.id).unwrap().workspace,
            lane.workspace
        );
        assert_eq!(
            load_terminal_record(&store.lock_inner().unwrap().connection, recovery.alias)
                .unwrap()
                .id,
            recovery.terminal
        );
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

    #[test]
    fn workspace_history_commits_restores_and_defaults_for_legacy_navigation() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("history.sqlite3");
        let boot_id = boot("history-boot");
        let store = StateStore::open(&database, boot_id.clone()).unwrap();
        let mut workspace = workspace_with_terminal(&boot_id);
        workspace.workspace_history = vec![
            WorkspaceDestination::Channel(ChannelId::new()),
            WorkspaceDestination::Worktree(WorktreeId::new()),
            WorkspaceDestination::Folder(ProjectId::new()),
        ];
        SaveLane::default().save(&store, &workspace).unwrap();
        drop(store);
        let store = StateStore::open(&database, boot_id).unwrap();
        assert_eq!(
            store
                .load_workspace(workspace.id, None)
                .unwrap()
                .unwrap()
                .workspace_history,
            workspace.workspace_history
        );
        assert_eq!(
            store
                .load_workspace_manifest(workspace.id)
                .unwrap()
                .unwrap()
                .workspace_history,
            workspace.workspace_history
        );
        drop(store);
        let db = Connection::open(&database).unwrap();
        db.execute(
            "DELETE FROM metadata WHERE key=?1",
            [format!(
                "workspace:{}:navigation:workspace_history",
                workspace.id
            )],
        )
        .unwrap();
        drop(db);
        let store = StateStore::open(&database, boot("history-boot")).unwrap();
        assert!(
            store
                .load_workspace(workspace.id, None)
                .unwrap()
                .unwrap()
                .workspace_history
                .is_empty()
        );
        let mut legacy = serde_json::to_value(&workspace).unwrap();
        legacy.as_object_mut().unwrap().remove("workspace_history");
        assert!(
            serde_json::from_value::<WorkspaceState>(legacy)
                .unwrap()
                .workspace_history
                .is_empty()
        );
    }

    #[test]
    fn unread_agent_updates_survive_autosave_restore_and_read() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("state.db");
        let boot_id = boot("unread");
        let store = StateStore::open(&database, boot_id.clone()).unwrap();
        let mut workspace = workspace_with_terminal(&boot_id);
        let card = workspace.cards[0].id;
        workspace
            .agent_updates
            .insert(card, Some((ProjectId::new(), WorktreeId::new())));
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        drop(store);
        let store = StateStore::open(&database, boot_id.clone()).unwrap();
        assert_eq!(
            store
                .load_workspace(workspace.id, None)
                .unwrap()
                .unwrap()
                .agent_updates,
            workspace.agent_updates
        );
        assert_eq!(
            store
                .load_workspace_manifest(workspace.id)
                .unwrap()
                .unwrap()
                .agent_updates,
            workspace.agent_updates
        );
        workspace.agent_updates.clear();
        lane.save(&store, &workspace).unwrap();
        drop(store);
        let store = StateStore::open(&database, boot_id).unwrap();
        assert!(
            store
                .load_workspace(workspace.id, None)
                .unwrap()
                .unwrap()
                .agent_updates
                .is_empty()
        );
    }

    #[test]
    fn unread_agent_updates_cold_restore_drops_unreadable_closed_and_missing_owners() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("partial.db");
        let boot_id = boot("partial-unread");
        let store = StateStore::open(&database, boot_id.clone()).unwrap();
        let mut workspace = workspace_with_terminal(&boot_id);
        let survivor = workspace.cards[0].id;
        let survivor_owner = (ProjectId::new(), WorktreeId::new());
        let mut broken = workspace.cards[0].clone();
        broken.id = CardId::new();
        let broken_id = broken.id;
        let broken_terminal = TerminalId::new();
        let CardKind::Terminal(session) = &mut broken.kind else {
            unreachable!()
        };
        session.id = broken_terminal;
        session.transcript_path = None;
        let mut closed = broken.clone();
        closed.id = CardId::new();
        let closed_id = closed.id;
        closed.existence = Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: None,
            },
        };
        let CardKind::Terminal(session) = &mut closed.kind else {
            unreachable!()
        };
        session.id = TerminalId::new();
        workspace.cards.extend([broken, closed]);
        workspace.agent_updates = BTreeMap::from([
            (survivor, Some(survivor_owner)),
            (broken_id, Some((ProjectId::new(), WorktreeId::new()))),
            (closed_id, Some((ProjectId::new(), WorktreeId::new()))),
            (CardId::new(), Some((ProjectId::new(), WorktreeId::new()))),
        ]);
        SaveLane::default().save(&store, &workspace).unwrap();
        drop(store);
        let db = Connection::open(&database).unwrap();
        db.execute(
            "DELETE FROM terminal_recipes WHERE terminal_id=?1",
            [broken_terminal.to_string()],
        )
        .unwrap();
        drop(db);
        let store = StateStore::open(&database, boot_id).unwrap();
        let restored = store
            .load_workspace_restored(workspace.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(restored.warnings.len(), 1);
        assert!(restored.warnings[0].contains(&broken_id.to_string()));
        assert!(!restored.state.cards.iter().any(|card| card.id == broken_id));
        assert_eq!(
            restored.state.agent_updates,
            BTreeMap::from([(survivor, Some(survivor_owner))])
        );
    }

    #[test]
    fn unread_agent_updates_legacy_workspace_metadata_defaults_to_read() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("legacy.db");
        let boot_id = boot("unread-legacy");
        let store = StateStore::open(&database, boot_id.clone()).unwrap();
        let mut workspace = workspace_with_terminal(&boot_id);
        workspace.agent_updates.insert(workspace.cards[0].id, None);
        SaveLane::default().save(&store, &workspace).unwrap();
        drop(store);
        let db = Connection::open(&database).unwrap();
        db.execute(
            "DELETE FROM metadata WHERE key=?1",
            [format!(
                "workspace:{}:navigation:agent_updates",
                workspace.id
            )],
        )
        .unwrap();
        drop(db);
        let store = StateStore::open(&database, boot_id).unwrap();
        assert!(
            store
                .load_workspace(workspace.id, None)
                .unwrap()
                .unwrap()
                .agent_updates
                .is_empty()
        );
        let mut legacy = serde_json::to_value(&workspace).unwrap();
        legacy.as_object_mut().unwrap().remove("agent_updates");
        assert!(
            serde_json::from_value::<WorkspaceState>(legacy)
                .unwrap()
                .agent_updates
                .is_empty()
        );
    }

    #[test]
    fn workspace_managed_image_drafts_roundtrip_additive_metadata_and_legacy_absence() {
        let directory = tempdir().unwrap();
        let boot_id = boot("image-draft-boot");
        let store = StateStore::open(
            directory.path().join("image-drafts.sqlite3"),
            boot_id.clone(),
        )
        .unwrap();
        let mut workspace = workspace_with_terminal(&boot_id);
        let card = workspace.cards[0].id;
        let reference = crate::chat::ImageReference {
            id: uuid::Uuid::new_v4(),
            number: 37,
            device: uuid::Uuid::new_v4(),
            name: "literal {draft} *.png".into(),
            sha256: "a".repeat(64),
            extension: "png".into(),
        };
        let image = crate::chat::Attachment {
            path: "/private/owned/image.png".into(),
            reference: Some(reference.clone()),
        };
        workspace
            .chat_drafts
            .insert(card, format!("Keep {}", reference.token()));
        workspace
            .chat_draft_attachments
            .insert(card, vec![image.clone()]);
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        assert_eq!(
            store
                .load_workspace(workspace.id, None)
                .unwrap()
                .unwrap()
                .chat_draft_attachments[&card],
            [image.clone()]
        );
        assert_eq!(
            store
                .load_workspace_manifest(workspace.id)
                .unwrap()
                .unwrap()
                .chat_draft_attachments[&card],
            [image]
        );
        let mut legacy = serde_json::to_value(&workspace).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("chat_draft_attachments");
        assert!(
            serde_json::from_value::<WorkspaceState>(legacy)
                .unwrap()
                .chat_draft_attachments
                .is_empty()
        );
        workspace.chat_draft_attachments.clear();
        lane.save(&store, &workspace).unwrap();
        assert!(
            store
                .load_workspace(workspace.id, None)
                .unwrap()
                .unwrap()
                .chat_draft_attachments
                .is_empty()
        );
    }

    #[test]
    fn workspace_chat_drafts_commit_and_restore_with_terminal_cards() {
        let directory = tempdir().unwrap();
        let boot_id = boot("draft-boot");
        let store =
            StateStore::open(directory.path().join("drafts.sqlite3"), boot_id.clone()).unwrap();
        let mut workspace = workspace_with_terminal(&boot_id);
        let card = workspace.cards[0].id;
        workspace
            .chat_drafts
            .insert(card, "unfinished\nmessage".into());
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(
            restored.chat_drafts.get(&card).unwrap(),
            "unfinished\nmessage"
        );
        assert_eq!(
            store
                .load_workspace_manifest(workspace.id)
                .unwrap()
                .unwrap()
                .chat_drafts,
            workspace.chat_drafts
        );

        workspace.chat_drafts.clear();
        lane.save(&store, &workspace).unwrap();
        assert!(
            store
                .load_workspace(workspace.id, None)
                .unwrap()
                .unwrap()
                .chat_drafts
                .is_empty()
        );
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
            native_continuity: Default::default(),
            owned_codex_listener: None,
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

    fn retained_image_fixture(number: u64) -> crate::chat::Attachment {
        crate::chat::Attachment {
            path: format!("/private/owned/image-{number}.png").into(),
            reference: Some(crate::chat::ImageReference {
                id: uuid::Uuid::from_u128(u128::from(number) + 1000),
                number,
                device: uuid::Uuid::from_u128(77),
                name: format!("Image {number}"),
                sha256: "a".repeat(64),
                extension: "png".into(),
            }),
        }
    }

    #[test]
    fn retained_chat_copy_is_atomic_idempotent_and_never_rewrites_canonical() {
        for stage in ["source", "upgraded", "replaced"] {
            let directory = tempdir().unwrap();
            let path = directory.path().join("copy.sqlite3");
            let store = StateStore::open(&path, boot("chat-boot")).unwrap();
            let (mut workspace, mut source) = agent_workspace(crate::agent::Provider::Claude);
            source.terminal = None;
            source.proof = Some(HostSessionProof {
                host_session_id: "old".into(),
                boot_id: boot("chat-boot"),
                host_incarnation: "old".into(),
            });
            let original = workspace.cards[0].id;
            workspace.cards[0].kind = CardKind::Chat(source.clone());
            workspace
                .chat_drafts
                .insert(original, "Canonical draft untouched".into());
            workspace.agent_updates.insert(original, None);
            workspace
                .chat_draft_attachments
                .insert(original, vec![retained_image_fixture(13)]);
            let mut lane = SaveLane::default();
            lane.save(&store, &workspace).unwrap();
            let mut canonical = source.clone();
            if stage != "source" {
                canonical.proof.as_mut().unwrap().host_incarnation = "new".into();
                store
                    .rebind_chat_upgrade(original, &source, &canonical)
                    .unwrap();
            }
            if stage == "replaced" {
                let expected = canonical.id;
                canonical.id = TerminalId::new();
                // An explicit replacement first installs an unstarted
                // conversation; its first confirmed snapshot names the thread.
                let proof = canonical.proof.take();
                canonical.launch.session_id = None;
                canonical.launch.recipe.arguments = vec!["--verbose".into()];
                store
                    .replace_chat_identity(original, expected, &canonical)
                    .unwrap();
                canonical.launch.session_id = Some("replacement-thread".into());
                canonical.proof = proof;
                let mut started = workspace.clone();
                started.cards[0].kind = CardKind::Chat(canonical.clone());
                lane.save(&store, &started).unwrap();
            }
            let before = store.load_card(original, None).unwrap().unwrap();
            let mut card = workspace.cards[0].clone();
            card.id = CardId::new();
            let copy = RetainedChatCopy {
                workspace: workspace.id,
                source_card: original,
                source: source.clone(),
                copy: card,
                draft: "Exact saved text  \nsecond line 🦀".into(),
                attachments: vec![retained_image_fixture(29)],
                scope: None,
                channel: None,
            };
            let committed = store.preserve_chat_copy(&copy).unwrap();
            let retry = store.preserve_chat_copy(&copy).unwrap();
            assert_eq!(retry, committed, "{stage}");
            assert_eq!(
                store.load_card(original, None).unwrap().unwrap(),
                before,
                "{stage}"
            );
            for mutation in [
                "text",
                "images",
                "image-path",
                "image-label",
                "image-duplicate",
                "source",
                "descriptor",
                "origin",
                "occupied",
            ] {
                let mut changed = copy.clone();
                match mutation {
                    "text" => changed.draft.push('!'),
                    "images" => changed.attachments.clear(),
                    "image-path" => changed.attachments[0].path = "/other/image.png".into(),
                    "image-label" => changed.attachments[0].reference.as_mut().unwrap().number += 1,
                    "image-duplicate" => changed.attachments.push(changed.attachments[0].clone()),
                    "source" => {
                        changed
                            .source
                            .launch
                            .recipe
                            .arguments
                            .push("--changed".into());
                        changed.copy.kind = CardKind::Chat(changed.source.clone());
                    }
                    "descriptor" => changed.source.id = TerminalId::new(),
                    "origin" => changed.source_card = CardId::new(),
                    "occupied" => changed.copy.id = original,
                    _ => unreachable!(),
                }
                assert!(
                    store.preserve_chat_copy(&changed).is_err(),
                    "{stage}: {mutation}"
                );
                assert_eq!(
                    store.stored_generations(workspace.id).unwrap().workspace,
                    committed.generation
                );
            }
            let mut projected = workspace.clone();
            copy.project(&mut projected).unwrap();
            assert_eq!(
                projected.chat_draft_attachments[&copy.copy.id],
                copy.attachments
            );
            assert_eq!(
                projected.chat_draft_attachments[&original],
                workspace.chat_draft_attachments[&original]
            );
            projected
                .chat_draft_attachments
                .insert(copy.copy.id, vec![retained_image_fixture(42)]);
            assert!(
                copy.project(&mut projected).is_err(),
                "projection cannot reinterpret accepted image metadata"
            );
            drop(store);
            let reopened = StateStore::open(&path, boot("chat-boot")).unwrap();
            let saved = reopened
                .load_workspace(workspace.id, None)
                .unwrap()
                .unwrap();
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
            assert_eq!(
                saved.chat_draft_attachments[&copy.copy.id],
                copy.attachments
            );
            assert_eq!(
                saved.chat_draft_attachments[&original],
                workspace.chat_draft_attachments[&original]
            );
            assert_eq!(saved.chat_drafts[&original], "Canonical draft untouched");
            assert_eq!(saved.agent_updates, workspace.agent_updates);
            assert!(saved.local_only_cards.contains(&copy.copy.id));
            assert_eq!(saved.pending_chat_admissions[&copy.copy.id], source);
            assert_eq!(
                saved.card_scopes[&copy.copy.id],
                source.launch.recipe.working_directory
            );
            let inner = reopened.lock_inner().unwrap();
            let receipt = metadata(
                &inner.connection,
                &format!(
                    "workspace:{}:retained_chat_copy:{}",
                    workspace.id, copy.copy.id
                ),
            )
            .unwrap()
            .unwrap();
            assert_eq!(receipt.len(), 64);
            assert!(!receipt.contains("Exact saved text"));
        }
    }

    #[test]
    fn retained_image_copy_rolls_back_all_fields_when_image_metadata_cannot_commit() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("copy-failure.sqlite3");
        let store = StateStore::open(&path, boot("chat-boot")).unwrap();
        let (mut workspace, source) = agent_workspace(crate::agent::Provider::Claude);
        let original = workspace.cards[0].id;
        workspace.cards[0].kind = CardKind::Chat(source.clone());
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        let before = store.load_workspace(workspace.id, None).unwrap().unwrap();
        let mut card = workspace.cards[0].clone();
        card.id = CardId::new();
        let copy = RetainedChatCopy {
            workspace: workspace.id,
            source_card: original,
            source,
            copy: card,
            draft: String::new(),
            attachments: vec![retained_image_fixture(29)],
            scope: None,
            channel: None,
        };
        store
            .lock_inner()
            .unwrap()
            .connection
            .execute_batch(
                "CREATE TRIGGER refuse_retained_images BEFORE INSERT ON metadata
             WHEN NEW.key LIKE '%:navigation:chat_draft_attachments'
             BEGIN SELECT RAISE(ABORT, 'fixture refuses image metadata'); END;",
            )
            .unwrap();
        assert!(store.preserve_chat_copy(&copy).is_err());
        assert!(store.load_card(copy.copy.id, None).unwrap().is_none());
        assert_eq!(
            store.load_workspace(workspace.id, None).unwrap().unwrap(),
            before
        );
        store
            .lock_inner()
            .unwrap()
            .connection
            .execute_batch("DROP TRIGGER refuse_retained_images")
            .unwrap();
        store.preserve_chat_copy(&copy).unwrap();
        drop(store);
        let reopened = StateStore::open(&path, boot("chat-boot")).unwrap();
        let saved = reopened
            .load_workspace(workspace.id, None)
            .unwrap()
            .unwrap();
        assert!(saved.chat_drafts[&copy.copy.id].is_empty());
        assert_eq!(
            saved.chat_draft_attachments[&copy.copy.id],
            copy.attachments
        );
    }

    #[test]
    fn empty_retained_image_metadata_preserves_legacy_receipt_bytes_and_replay_defaults() {
        #[derive(Serialize)]
        struct LegacyCopy<'a> {
            workspace: WorkspaceId,
            source_card: CardId,
            source: &'a crate::chat::Session,
            copy: &'a CardState,
            draft: &'a str,
            scope: &'a Option<PathBuf>,
            channel: Option<crate::model::ChannelId>,
        }
        let (workspace, source) = agent_workspace(crate::agent::Provider::Claude);
        let mut card = workspace.cards[0].clone();
        card.id = CardId::new();
        card.kind = CardKind::Chat(source.clone());
        let copy = RetainedChatCopy {
            workspace: workspace.id,
            source_card: workspace.cards[0].id,
            source,
            copy: card,
            draft: "Legacy exact bytes \n🦀".into(),
            attachments: Vec::new(),
            scope: None,
            channel: None,
        };
        let legacy = LegacyCopy {
            workspace: copy.workspace,
            source_card: copy.source_card,
            source: &copy.source,
            copy: &copy.copy,
            draft: &copy.draft,
            scope: &copy.scope,
            channel: copy.channel,
        };
        let old_bytes = to_json(&legacy).unwrap();
        assert_eq!(
            to_json(&copy).unwrap(),
            old_bytes,
            "old receipt SHA256 input is byte-identical"
        );
        let old_request = serde_json::from_str::<RetainedChatCopy>(&old_bytes).unwrap();
        assert_eq!(old_request, copy);
        let directory = tempdir().unwrap();
        let path = directory.path().join("legacy-copy.sqlite3");
        let store = StateStore::open(&path, boot("chat-boot")).unwrap();
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        let committed = store.preserve_chat_copy(&old_request).unwrap();
        drop(store);
        let reopened = StateStore::open(&path, boot("chat-boot")).unwrap();
        assert_eq!(
            reopened.preserve_chat_copy(&copy).unwrap(),
            committed,
            "legacy receipt remains replayable after reopening"
        );
    }

    #[test]
    fn migration_inspection_omits_exact_quarantined_sources_and_rejects_mismatch_or_malformed_markers()
     {
        for surface in ["chat", "terminal"] {
            for marker in ["exact", "mismatch", "malformed", "oversized", "absent"] {
                let directory = tempdir().unwrap();
                let path = directory.path().join("quarantine-migration.sqlite3");
                let store = StateStore::open(&path, boot("quarantine-migration")).unwrap();
                let mut workspace = workspace_with_terminal(&boot("quarantine-migration"));
                if surface == "chat" {
                    let (chat_workspace, source) = agent_workspace(crate::agent::Provider::Codex);
                    workspace = chat_workspace;
                    workspace.cards[0].kind = CardKind::Chat(source);
                }
                let rejected = workspace.cards[0].id;
                let source = workspace.cards[0].kind.clone();
                let mut healthy = workspace.cards[0].clone();
                healthy.id = CardId::new();
                match &mut healthy.kind {
                    CardKind::Terminal(session) => session.id = TerminalId::new(),
                    CardKind::Chat(session) => session.id = TerminalId::new(),
                    _ => unreachable!(),
                }
                workspace.cards.push(healthy.clone());
                let mut evidence = source.clone();
                if marker == "mismatch" {
                    match &mut evidence {
                        CardKind::Terminal(session) => session
                            .launch
                            .arguments
                            .push("--different-rejected-source".into()),
                        CardKind::Chat(session) => session
                            .launch
                            .recipe
                            .arguments
                            .push("--different-rejected-source".into()),
                        _ => unreachable!(),
                    }
                }
                if marker != "absent" {
                    workspace.quarantined_panes.insert(rejected, evidence);
                }
                SaveLane::default().save(&store, &workspace).unwrap();
                if matches!(marker, "malformed" | "oversized") {
                    let inner = store.lock_inner().unwrap();
                    let value = if marker == "malformed" {
                        "{broken".to_owned()
                    } else {
                        " ".repeat(4 * 1024 * 1024 + 1)
                    };
                    inner
                        .connection
                        .execute(
                            "UPDATE metadata SET value=?2 WHERE key=?1",
                            params![
                                format!("workspace:{}:navigation:quarantined_panes", workspace.id),
                                value
                            ],
                        )
                        .unwrap();
                }
                drop(store);
                let before = fs::read(&path).unwrap();
                let sessions = read_open_migration_sessions(&path);
                if matches!(marker, "mismatch" | "malformed" | "oversized") {
                    assert!(
                        sessions.is_err(),
                        "{surface}/{marker} cannot silently become runnable migration work"
                    );
                } else {
                    let sessions = sessions.unwrap();
                    assert!(
                        sessions.contains(&(healthy.id, healthy.kind.clone())),
                        "healthy sources remain independently qualified"
                    );
                    assert_eq!(
                        sessions.contains(&(rejected, source)),
                        marker == "absent",
                        "{surface}/{marker}"
                    );
                }
                assert_eq!(
                    fs::read(&path).unwrap(),
                    before,
                    "inspection never rewrites {surface}/{marker} evidence"
                );
            }
        }
    }

    #[test]
    fn migration_inspection_omits_only_exact_retained_local_chat_sources() {
        for mode in [
            "retained",
            "shared",
            "unmarked",
            "wrong-source",
            "wrong-card",
            "legacy",
        ] {
            let directory = tempdir().unwrap();
            let path = directory.path().join("source.sqlite3");
            let store = StateStore::open(&path, boot("chat-boot")).unwrap();
            let (mut workspace, source) = agent_workspace(crate::agent::Provider::Codex);
            let card = workspace.cards[0].id;
            workspace.cards[0].kind = CardKind::Chat(source.clone());
            workspace.chat_drafts.insert(card, "Saved text only".into());
            if mode != "shared" {
                workspace.local_only_cards.insert(card);
            }
            if mode != "unmarked" {
                workspace
                    .pending_chat_admissions
                    .insert(card, source.clone());
            }
            SaveLane::default().save(&store, &workspace).unwrap();
            // Valid writers prune mismatched markers. Model damaged/stale
            // metadata internally to prove the read-only intake also refuses
            // to infer inertness from a different source or card identity.
            if matches!(mode, "wrong-source" | "wrong-card" | "legacy") {
                let mut inner = store.lock_inner().unwrap();
                let transaction = inner.connection.transaction().unwrap();
                let key = format!(
                    "workspace:{}:navigation:pending_chat_admissions",
                    workspace.id
                );
                if mode == "legacy" {
                    transaction
                        .execute("DELETE FROM metadata WHERE key = ?1", [&key])
                        .unwrap();
                    transaction
                        .execute(
                            "DELETE FROM metadata WHERE key = ?1",
                            [format!(
                                "workspace:{}:navigation:local_only_cards",
                                workspace.id
                            )],
                        )
                        .unwrap();
                } else {
                    let mut marked = source.clone();
                    let marked_card = if mode == "wrong-card" {
                        CardId::new()
                    } else {
                        marked.launch.recipe.arguments.push("--different".into());
                        card
                    };
                    set_metadata(
                        &transaction,
                        &key,
                        &to_json(&HashMap::from([(marked_card, marked)])).unwrap(),
                    )
                    .unwrap();
                }
                transaction.commit().unwrap();
            }
            drop(store);
            let before = std::fs::read(&path).unwrap();
            let sessions = read_open_migration_sessions(&path).unwrap();
            if mode == "retained" {
                assert!(sessions.is_empty());
            } else {
                assert_eq!(sessions, vec![(card, CardKind::Chat(source))], "{mode}");
            }
            assert_eq!(std::fs::read(&path).unwrap(), before, "{mode}");
        }
    }

    #[test]
    fn migration_inspection_retained_metadata_is_bounded_and_fail_closed() {
        for value in ["not JSON".to_owned(), " ".repeat(4 * 1024 * 1024 + 1)] {
            let directory = tempdir().unwrap();
            let path = directory.path().join("source.sqlite3");
            let store = StateStore::open(&path, boot("chat-boot")).unwrap();
            let (mut workspace, source) = agent_workspace(crate::agent::Provider::Codex);
            let card = workspace.cards[0].id;
            workspace.cards[0].kind = CardKind::Chat(source.clone());
            workspace.local_only_cards.insert(card);
            workspace.pending_chat_admissions.insert(card, source);
            SaveLane::default().save(&store, &workspace).unwrap();
            let mut inner = store.lock_inner().unwrap();
            let transaction = inner.connection.transaction().unwrap();
            set_metadata(
                &transaction,
                &format!(
                    "workspace:{}:navigation:pending_chat_admissions",
                    workspace.id
                ),
                &value,
            )
            .unwrap();
            transaction.commit().unwrap();
            drop(inner);
            drop(store);
            let before = std::fs::read(&path).unwrap();
            assert!(read_open_migration_sessions(&path).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn migration_session_inventory_budget_spans_windows_and_precedes_json_decode() {
        let directory = tempdir().unwrap();
        let mut paths = Vec::new();
        for index in 0..2 {
            let path = directory.path().join(format!("source-{index}.sqlite3"));
            let store = StateStore::open(&path, boot("chat-boot")).unwrap();
            let (mut workspace, source) = agent_workspace(crate::agent::Provider::Codex);
            workspace.cards[0].kind = CardKind::Chat(source);
            SaveLane::default().save(&store, &workspace).unwrap();
            drop(store);
            paths.push(path);
        }
        let before = paths
            .iter()
            .map(std::fs::read)
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        let connection =
            Connection::open_with_flags(&paths[0], OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let inspection_bytes: u64 = connection
            .query_row(
                "SELECT length(CAST(content_json AS BLOB)) +
                 (SELECT coalesce(sum(length(CAST(value AS BLOB))), 0) FROM metadata
                  WHERE key LIKE '%:navigation:local_only_cards'
                     OR key LIKE '%:navigation:pending_chat_admissions')
                 FROM cards WHERE kind_tag='chat'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(connection);
        for row_limited in [false, true] {
            let mut budget =
                MigrationInspectionBudget::new(Instant::now() + Duration::from_secs(30));
            if row_limited {
                budget.remaining_rows = 1;
            } else {
                budget.remaining_bytes = inspection_bytes;
            }
            assert_eq!(
                read_open_migration_sessions_with_budget(&paths[0], &mut budget)
                    .unwrap()
                    .len(),
                1
            );
            assert!(
                read_open_migration_sessions_with_budget(&paths[1], &mut budget)
                    .unwrap_err()
                    .to_string()
                    .contains("inventory exceeds")
            );
        }
        for (path, original) in paths.iter().zip(before) {
            assert_eq!(std::fs::read(path).unwrap(), original);
        }

        let connection = Connection::open(&paths[1]).unwrap();
        connection
            .execute(
                "UPDATE cards SET content_json='not valid JSON' WHERE kind_tag='chat'",
                [],
            )
            .unwrap();
        drop(connection);
        let mut budget = MigrationInspectionBudget::new(Instant::now() + Duration::from_secs(30));
        budget.remaining_bytes = 0;
        assert!(
            read_open_migration_sessions_with_budget(&paths[1], &mut budget)
                .unwrap_err()
                .to_string()
                .contains("inventory exceeds"),
            "budget must refuse before decoding the oversized inventory"
        );
        let missing = directory.path().join("must-not-open.sqlite3");
        let mut expired = MigrationInspectionBudget::new(Instant::now());
        assert!(
            read_open_migration_sessions_with_budget(&missing, &mut expired)
                .unwrap_err()
                .to_string()
                .contains("bounded budget")
        );
        assert!(!missing.exists());
    }

    #[test]
    fn migration_session_inspection_refuses_future_window_schema_without_writing() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("future-source.sqlite3");
        let store = StateStore::open(&path, boot("chat-boot")).unwrap();
        let workspace = workspace_with_terminal(&boot("chat-boot"));
        SaveLane::default().save(&store, &workspace).unwrap();
        drop(store);
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
            .unwrap();
        drop(connection);
        let before = std::fs::read(&path).unwrap();
        let error = read_open_migration_sessions(&path).unwrap_err();
        assert!(
            matches!(error, StoreError::UnsupportedSchema { found, supported }
            if found == CURRENT_SCHEMA_VERSION + 1 && supported == CURRENT_SCHEMA_VERSION)
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn migration_inspection_reads_schema4_without_migrating_and_open_migrates_it() {
        let directory = tempdir().unwrap();
        let fixture = directory.path().join("current.sqlite3");
        let path = directory.path().join("september-window.sqlite3");
        let store = StateStore::open(&fixture, boot("recorded-boot")).unwrap();
        let mut workspace = workspace_with_terminal(&boot("recorded-boot"));
        let card = workspace.cards[0].id;
        workspace
            .chat_drafts
            .insert(card, "unsent retained draft".into());
        SaveLane::default().save(&store, &workspace).unwrap();
        drop(store);
        let mut connection = Connection::open(&path).unwrap();
        let transaction = connection.transaction().unwrap();
        migrate_to_v1(&transaction).unwrap();
        migrate_to_v2(&transaction).unwrap();
        migrate_to_v3(&transaction).unwrap();
        migrate_to_v4(&transaction).unwrap();
        transaction.pragma_update(None, "user_version", 4).unwrap();
        transaction.commit().unwrap();
        connection
            .execute("ATTACH DATABASE ?1 AS fixture", [fixture.to_str().unwrap()])
            .unwrap();
        connection.execute_batch(
            "INSERT INTO workspaces SELECT * FROM fixture.workspaces;
             INSERT INTO cards SELECT * FROM fixture.cards;
             INSERT INTO terminal_recipes SELECT terminal_id,card_id,generation,recipe_json,continuity_json,resume_hint,updated_at_unix_ms FROM fixture.terminal_recipes;
             INSERT INTO transcript_metadata SELECT * FROM fixture.transcript_metadata;
             INSERT INTO metadata SELECT * FROM fixture.metadata;
             DETACH DATABASE fixture;"
        ).unwrap();
        drop(connection);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            read_open_migration_sessions(&path).unwrap(),
            vec![(card, workspace.cards[0].kind.clone())]
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "preview must leave the historical store byte-for-byte intact"
        );
        let reopened = StateStore::open(&path, boot("recorded-boot")).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
        let loaded = reopened
            .load_workspace(workspace.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.chat_drafts.get(&card),
            Some(&"unsent retained draft".to_owned())
        );
        assert_eq!(loaded.cards[0].id, card);
    }

    #[test]
    fn migration_session_sql_bounds_all_normalized_fields_before_decoding() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("bounded-source.sqlite3");
        let store = StateStore::open(&path, boot("chat-boot")).unwrap();
        let workspace = workspace_with_terminal(&boot("chat-boot"));
        let card = workspace.cards[0].id;
        SaveLane::default().save(&store, &workspace).unwrap();
        drop(store);
        let connection = Connection::open(&path).unwrap();
        assert!(
            connection
                .query_row(
                    "SELECT content_json IS NULL FROM cards WHERE card_id=?1",
                    [card.to_string()],
                    |row| row.get::<_, bool>(0)
                )
                .unwrap()
        );
        assert_eq!(
            read_migration_session_at(&connection, card, "terminal", None).unwrap(),
            workspace.cards[0].kind
        );
        // Non-ASCII forces a byte limit rather than a character-count limit.
        let oversized = "🦀".repeat(1024 * 1024 + 1);
        // Deliberately corrupt a referenced terminal identity only inside this
        // rollback-only fixture transaction. Otherwise SQLite correctly refuses
        // the UPDATE before the read-only allocation guard can inspect it.
        connection
            .execute_batch("BEGIN; PRAGMA defer_foreign_keys=ON;")
            .unwrap();
        for column in [
            "terminal_id",
            "recipe_json",
            "continuity_json",
            "resume_hint",
            "recovery_origin",
            "owned_codex_listener",
            "predecessor",
        ] {
            let original: Option<String> = connection
                .query_row(
                    &format!("SELECT {column} FROM terminal_recipes WHERE card_id=?1"),
                    [card.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            connection
                .execute(
                    &format!("UPDATE terminal_recipes SET {column}=?2 WHERE card_id=?1"),
                    params![card.to_string(), &oversized],
                )
                .unwrap();
            let error = read_migration_session_at(&connection, card, "terminal", None)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("oversized immutable descriptor"),
                "field {column}: {error}"
            );
            connection
                .execute(
                    &format!("UPDATE terminal_recipes SET {column}=?2 WHERE card_id=?1"),
                    params![card.to_string(), original],
                )
                .unwrap();
        }
        let original:String=connection.query_row("SELECT path_json FROM transcript_metadata WHERE terminal_id=(SELECT terminal_id FROM terminal_recipes WHERE card_id=?1)",[card.to_string()],|row|row.get(0)).unwrap();
        connection.execute("UPDATE transcript_metadata SET path_json=?2 WHERE terminal_id=(SELECT terminal_id FROM terminal_recipes WHERE card_id=?1)",params![card.to_string(),&oversized]).unwrap();
        assert!(
            read_migration_session_at(&connection, card, "terminal", None)
                .unwrap_err()
                .to_string()
                .contains("oversized immutable descriptor")
        );
        connection.execute("UPDATE transcript_metadata SET path_json=?2 WHERE terminal_id=(SELECT terminal_id FROM terminal_recipes WHERE card_id=?1)",params![card.to_string(),original]).unwrap();
        assert_eq!(
            read_migration_session_at(&connection, card, "terminal", None).unwrap(),
            workspace.cards[0].kind
        );
        connection.execute_batch("ROLLBACK;").unwrap();
    }

    #[test]
    fn migration_session_inspection_is_read_only_and_reconstructs_exact_terminal_and_chat_rows() {
        for chat in [false, true] {
            let directory = tempdir().unwrap();
            let path = directory.path().join("migration-source.sqlite3");
            let store = StateStore::open(&path, boot("chat-boot")).unwrap();
            let (mut workspace, source) = agent_workspace(crate::agent::Provider::Codex);
            if chat {
                workspace.cards[0].kind = CardKind::Chat(source);
            }
            let expected = (workspace.cards[0].id, workspace.cards[0].kind.clone());
            let mut lane = SaveLane::default();
            lane.save(&store, &workspace).unwrap();
            drop(store);
            let before = std::fs::read(&path).unwrap();
            assert_eq!(read_open_migration_sessions(&path).unwrap(), vec![expected]);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "inspection cannot migrate or update the database"
            );
            let store = StateStore::open(&path, boot("chat-boot")).unwrap();
            workspace.cards[0].existence = Existence::Closed {
                tombstone: Tombstone {
                    operation_id: None,
                    closed_at_unix_ms: 1,
                    reason: Some("fixture closed".into()),
                },
            };
            workspace.focused = None;
            lane.save(&store, &workspace).unwrap();
            assert!(read_open_migration_sessions(&path).unwrap().is_empty());
        }
    }

    #[test]
    fn editor_navigation_is_window_local_and_legacy_metadata_defaults_empty() {
        use crate::model::{RepositoryNavigation, RepositoryView};
        use std::os::unix::ffi::OsStringExt;
        let directory = tempdir().unwrap();
        let boot_id = boot("editor-navigation");
        let store =
            StateStore::open(directory.path().join("editors.sqlite3"), boot_id.clone()).unwrap();
        let mut first = workspace_with_terminal(&boot_id);
        let card = first.cards[0].id;
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: "/repo/.git".into(),
            worktree: "/repo".into(),
            selected_file: None,
        };
        first.cards[0].kind = CardKind::Repository(view.clone());
        first.workspace_history = vec![WorkspaceDestination::Folder(view.project)];
        let raw_path = PathBuf::from(std::ffi::OsString::from_vec(b"file-\xff.txt".to_vec()));
        first.repository_navigation.insert(
            card,
            RepositoryNavigation {
                project: view.project,
                worktree: view.worktree.clone(),
                selected_file: Some(raw_path.clone()),
                editor: None,
            },
        );
        let mut second = first.clone();
        second.id = WorkspaceId::new();
        second.workspace_history.clear();
        second
            .repository_navigation
            .get_mut(&card)
            .unwrap()
            .selected_file = Some("different.txt".into());
        let mut lane = SaveLane::default();
        lane.save(&store, &first).unwrap();
        // Each window owns its storage while projecting the same shared pane.
        let second_store = StateStore::open(
            directory.path().join("second-window.sqlite3"),
            boot_id.clone(),
        )
        .unwrap();
        let mut second_lane = SaveLane::default();
        second_lane.save(&second_store, &second).unwrap();
        assert_eq!(
            store
                .load_workspace(first.id, None)
                .unwrap()
                .unwrap()
                .workspace_history,
            first.workspace_history
        );
        assert!(
            second_store
                .load_workspace(second.id, None)
                .unwrap()
                .unwrap()
                .workspace_history
                .is_empty()
        );
        assert_eq!(
            store
                .load_workspace(first.id, None)
                .unwrap()
                .unwrap()
                .repository_navigation[&card]
                .selected_file,
            Some(raw_path)
        );
        assert_eq!(
            second_store
                .load_workspace(second.id, None)
                .unwrap()
                .unwrap()
                .repository_navigation[&card]
                .selected_file,
            Some("different.txt".into())
        );
        let mut legacy = serde_json::to_value(&first).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("repository_navigation");
        let restored: WorkspaceState = serde_json::from_value(legacy).unwrap();
        assert!(restored.repository_navigation.is_empty());
        let empty = workspace_with_terminal(&boot_id);
        let mut lane = SaveLane::default();
        lane.save(&store, &empty).unwrap();
        store
            .lock_inner()
            .unwrap()
            .connection
            .execute(
                "DELETE FROM metadata WHERE key = ?1",
                [format!(
                    "workspace:{}:navigation:repository_navigation",
                    empty.id
                )],
            )
            .unwrap();
        assert!(
            store
                .load_workspace_manifest(empty.id)
                .unwrap()
                .unwrap()
                .repository_navigation
                .is_empty()
        );
    }

    #[test]
    fn session_navigation_reads_open_terminals_and_closed_conversations_without_writing() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("navigation.sqlite3");
        let (mut workspace, session) = agent_workspace(crate::agent::Provider::Claude);
        let store = StateStore::open(&path, boot("navigation")).unwrap();
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        let CardKind::Terminal(terminal) = &workspace.cards[0].kind else {
            panic!("terminal fixture");
        };
        assert_eq!(
            read_navigable_session_ids(&path).unwrap(),
            vec![terminal.id]
        );
        workspace.cards[0].kind = CardKind::Chat(session.clone());
        workspace.cards[0].existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: None,
            },
        };
        workspace.focused = None;
        lane.save(&store, &workspace).unwrap();
        let before = store.stored_generations(workspace.id).unwrap().workspace;
        assert_eq!(read_navigable_session_ids(&path).unwrap(), vec![session.id]);
        assert_eq!(
            store.stored_generations(workspace.id).unwrap().workspace,
            before
        );
    }

    #[test]
    fn quarantine_metadata_roundtrips_defaults_only_when_absent_and_rejects_malformed_sources() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("quarantine-metadata.sqlite3");
        let boot_id = boot("quarantine-metadata");
        let mut state = workspace_with_terminal(&boot_id);
        let card = state.cards[0].id;
        let source = state.cards[0].kind.clone();
        state.quarantined_panes.insert(card, source.clone());
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        store
            .save_workspace(
                &state,
                SnapshotBase {
                    workspace: 0,
                    cards: &Default::default(),
                },
            )
            .unwrap();
        assert_eq!(
            store
                .load_workspace(state.id, None)
                .unwrap()
                .unwrap()
                .quarantined_panes
                .get(&card),
            Some(&source)
        );
        let key = format!("workspace:{}:navigation:quarantined_panes", state.id);
        let writer = Connection::open(&path).unwrap();
        writer
            .execute("DELETE FROM metadata WHERE key = ?1", [&key])
            .unwrap();
        assert!(
            store
                .load_workspace(state.id, None)
                .unwrap()
                .unwrap()
                .quarantined_panes
                .is_empty(),
            "legacy workspaces without this metadata remain readable"
        );
        for malformed in [
            "{broken",
            "{\"not-a-card-id\":{}}",
            "{\"00000000-0000-4000-8000-000000000001\":\"not-a-pane-source\"}",
        ] {
            writer.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![&key, malformed]).unwrap();
            assert!(
                store.load_workspace_manifest(state.id).is_err(),
                "known malformed rejection evidence cannot become an empty allow-list"
            );
            assert!(store.load_workspace(state.id, None).is_err());
        }
    }

    #[test]
    fn saved_conversations_retain_closed_chats_after_restart_and_omit_drafts() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("resume.sqlite3");
        let (mut workspace, session) = agent_workspace(crate::agent::Provider::Claude);
        let card = workspace.cards[0].id;
        workspace.cards[0].kind = CardKind::Chat(session.clone());
        let store = StateStore::open(&path, boot("resume-boot")).unwrap();
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        workspace.cards[0].existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: Some("closed by user".into()),
            },
        };
        workspace.focused = None;
        lane.save(&store, &workspace).unwrap();
        drop(store);
        let store = StateStore::open(&path, boot("next-boot")).unwrap();
        let saved = store.saved_conversations().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].card, card);
        assert!(!saved[0].open);
        assert_eq!(saved[0].session, session);
        let mut draft = session;
        draft.launch.session_id = None;
        workspace.cards = vec![CardState {
            id: CardId::new(),
            existence: Existence::Open,
            kind: CardKind::Chat(draft),
            ..workspace.cards[0].clone()
        }];
        let base = store.stored_generations(workspace.id).unwrap();
        store
            .save_workspace(
                &workspace,
                SnapshotBase {
                    workspace: base.workspace,
                    cards: &base.cards,
                },
            )
            .unwrap();
        assert_eq!(store.saved_conversations().unwrap().len(), 1);
        store
            .lock_inner()
            .unwrap()
            .connection
            .execute(
                "UPDATE cards SET content_json = '{invalid' WHERE card_id = ?1",
                [workspace.cards[0].id.to_string()],
            )
            .unwrap();
        assert_eq!(store.saved_conversations().unwrap().len(), 1);
        assert_eq!(
            read_navigable_session_ids(&path).unwrap(),
            vec![saved[0].session.id]
        );
    }

    #[test]
    fn saved_session_ownership_reads_channels_and_scopes_without_changing_the_window() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("other-window.sqlite3");
        let store = StateStore::open(&path, boot("owner")).unwrap();
        let project = ProjectState {
            id: ProjectId::new(),
            generation: 0,
            name: "Child".into(),
            repository: directory.path().join("parent/child"),
            kind: crate::model::ProjectKind::Folder,
            existence: Existence::Open,
        };
        store.save_project(&project).unwrap();
        let mut workspace = workspace_with_terminal(&boot("owner"));
        let card = workspace.cards[0].id;
        let CardKind::Terminal(terminal) = &workspace.cards[0].kind else {
            unreachable!()
        };
        let terminal_id = terminal.id;
        workspace.channels.push(ProjectChannel {
            title_source: Default::default(),
            id: ChannelId::new(),
            project_id: project.id,
            title: "Owned outside cwd".into(),
            ordinal: 0,
            shared: false,
            cards: vec![card],
            focused: Some(card),
        });
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        let before = store
            .load_workspace_manifest(workspace.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            read_project_session_owners(&path).unwrap(),
            vec![(
                terminal_id,
                SavedSessionOwner::Project(project.repository.clone())
            )]
        );
        assert_eq!(
            store
                .load_workspace_manifest(workspace.id)
                .unwrap()
                .unwrap()
                .generation,
            before.generation
        );
        let mut channels = workspace.channels.clone();
        workspace.channels.clear();
        workspace
            .card_scopes
            .insert(card, directory.path().join("parent/child"));
        lane.save(&store, &workspace).unwrap();
        assert_eq!(
            read_project_session_owners(&path).unwrap(),
            vec![(
                terminal_id,
                SavedSessionOwner::Directory(directory.path().join("parent/child"))
            )]
        );
        let other = ProjectState {
            id: ProjectId::new(),
            repository: directory.path().join("parent"),
            ..project.clone()
        };
        store.save_project(&other).unwrap();
        channels.push(ProjectChannel {
            title_source: Default::default(),
            id: ChannelId::new(),
            project_id: other.id,
            ..channels[0].clone()
        });
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE metadata SET value = ?1 WHERE key = ?2",
                params![
                    to_json(&channels).unwrap(),
                    format!("workspace:{}:navigation:{CHANNELS_FIELD}", workspace.id)
                ],
            )
            .unwrap();
        // Ownership must not need to restore a recipe or normalize away a
        // contradictory claim in an older or damaged saved window.
        connection
            .execute("UPDATE terminal_recipes SET recipe_json = 'invalid'", [])
            .unwrap();
        assert_eq!(
            read_project_session_owners(&path).unwrap(),
            vec![
                (terminal_id, SavedSessionOwner::Project(project.repository)),
                (terminal_id, SavedSessionOwner::Project(other.repository)),
            ]
        );
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
            title_source: Default::default(),
            id,
            project_id,
            title: "Investigate startup".into(),
            ordinal: 0,
            // Both halves of the join rule have to survive a restart. If they
            // did not, a reopened window would treat every channel as unshared
            // and republish the ones that were deleted while it was closed.
            shared: true,
            cards: vec![card],
            focused: Some(card),
        });
        workspace.active_channel = Some(id);
        workspace.active_worktree = Some("/tmp".into());
        let identity = uuid::Uuid::new_v4();
        workspace.workspace_model_identity = Some(identity);
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
        assert_eq!(restored.workspace_model_identity, Some(identity));
        assert_eq!(
            restored
                .cards
                .iter()
                .map(|card| card.id)
                .collect::<Vec<_>>(),
            vec![card, unrelated_id]
        );
        workspace.channels.push(ProjectChannel {
            title_source: Default::default(),
            id: ChannelId::new(),
            project_id,
            title: "Conflicting owner".into(),
            ordinal: 0,
            shared: false,
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
            location: None,
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

    fn descriptor_admission_fixture(
        path: &Path,
    ) -> (
        StateStore,
        CardId,
        crate::chat::Session,
        crate::chat::Session,
    ) {
        let store = StateStore::open(path, boot("chat-boot")).unwrap();
        let (mut workspace, source) = agent_workspace(crate::agent::Provider::Claude);
        let card = workspace.cards[0].id;
        workspace.cards[0].kind = CardKind::Chat(source.clone());
        SaveLane::default().save(&store, &workspace).unwrap();
        let mut replacement = source.clone();
        replacement
            .launch
            .recipe
            .environment
            .insert("DESCRIPTOR_TEST".into(), "replacement".into());
        (store, card, source, replacement)
    }

    #[test]
    fn descriptor_writer_admission_preserves_exact_cas_and_idempotent_current_profile() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let (store, card, source, replacement) = descriptor_admission_fixture(&path);
        let mut stale = source.clone();
        stale
            .launch
            .recipe
            .environment
            .insert("DESCRIPTOR_TEST".into(), "stale".into());
        assert!(
            store
                .rebind_chat_descriptor(card, &stale, &replacement)
                .is_err()
        );
        assert_eq!(
            store.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(source.clone())
        );
        store
            .rebind_chat_descriptor(card, &source, &replacement)
            .unwrap();
        let changes = store.lock_inner().unwrap().connection.total_changes();
        store
            .rebind_chat_descriptor(card, &source, &replacement)
            .unwrap();
        assert_eq!(
            store.lock_inner().unwrap().connection.total_changes(),
            changes,
            "idempotence still writes nothing"
        );
        assert_eq!(
            store.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(replacement)
        );
    }

    #[test]
    fn descriptor_writer_refuses_old_schema_and_changed_connection_without_repair() {
        for refusal in ["old-schema", "foreign-keys", "synchronous", "journal"] {
            let directory = tempdir().unwrap();
            let path = directory.path().join("state.sqlite3");
            let (store, card, source, replacement) = descriptor_admission_fixture(&path);
            let changes = {
                let inner = store.lock_inner().unwrap();
                match refusal {
                    "old-schema" => inner
                        .connection
                        .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION - 1)
                        .unwrap(),
                    "foreign-keys" => inner
                        .connection
                        .pragma_update(None, "foreign_keys", false)
                        .unwrap(),
                    "synchronous" => inner
                        .connection
                        .pragma_update(None, "synchronous", 0)
                        .unwrap(),
                    "journal" => inner
                        .connection
                        .pragma_update(None, "journal_mode", "DELETE")
                        .unwrap(),
                    _ => unreachable!(),
                }
                inner.connection.total_changes()
            };
            assert!(
                matches!(
                    store.rebind_chat_descriptor(card, &source, &replacement),
                    Err(StoreError::InvalidState(_))
                ),
                "{refusal}"
            );
            assert!(
                store
                    .rebind_chat_descriptor(card, &source, &source)
                    .is_err(),
                "even idempotent return requires admission: {refusal}"
            );
            let inner = store.lock_inner().unwrap();
            assert_eq!(
                inner.connection.total_changes(),
                changes,
                "refusal must not mutate card/hash: {refusal}"
            );
            let stored: String = inner
                .connection
                .query_row(
                    "SELECT content_json FROM cards WHERE card_id=?1",
                    [card.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                from_json::<crate::chat::Session>("chat card", &stored).unwrap(),
                source
            );
            match refusal {
                "old-schema" => assert_eq!(
                    inner
                        .connection
                        .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                        .unwrap(),
                    CURRENT_SCHEMA_VERSION - 1
                ),
                "foreign-keys" => assert_eq!(
                    inner
                        .connection
                        .pragma_query_value::<i64, _>(None, "foreign_keys", |row| row.get(0))
                        .unwrap(),
                    0
                ),
                "synchronous" => assert_eq!(
                    inner
                        .connection
                        .pragma_query_value::<i64, _>(None, "synchronous", |row| row.get(0))
                        .unwrap(),
                    0
                ),
                "journal" => assert_eq!(
                    inner
                        .connection
                        .pragma_query_value::<String, _>(None, "journal_mode", |row| row.get(0))
                        .unwrap(),
                    "delete"
                ),
                _ => unreachable!(),
            }
        }
    }

    std::thread_local! {
        // This connection's real busy callback fences a second connection's
        // pending writer; no timing assertion stands in for actual contention.
        static DESCRIPTOR_BUSY_GATE: std::cell::RefCell<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>> = const { std::cell::RefCell::new(None) };
    }
    fn descriptor_busy_gate(attempt: i32) -> bool {
        if let Some((entered, proceed)) = DESCRIPTOR_BUSY_GATE.with(|gate| gate.borrow_mut().take())
        {
            if entered.send(()).is_err() || proceed.recv_timeout(Duration::from_secs(5)).is_err() {
                return false;
            }
        }
        std::thread::sleep(Duration::from_millis(1));
        attempt < 5000
    }

    #[test]
    fn descriptor_writer_readmits_future_schema_after_actual_second_connection_wait() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let (store, card, source, replacement) = descriptor_admission_fixture(&path);
        let observed = store.schema_version().unwrap();
        assert_eq!(observed, CURRENT_SCHEMA_VERSION);
        let changes = store.lock_inner().unwrap().connection.total_changes();
        let mut winner = Connection::open(&path).unwrap();
        configure_connection(&winner, DEFAULT_BUSY_TIMEOUT).unwrap();
        let transaction = winner
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        transaction.execute_batch("CREATE TABLE future_descriptor_owner (evidence TEXT NOT NULL) STRICT; INSERT INTO future_descriptor_owner VALUES ('keep future bytes');").unwrap();
        transaction
            .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
            .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
        let delayed = std::thread::spawn(move || {
            DESCRIPTOR_BUSY_GATE.with(|gate| *gate.borrow_mut() = Some((entered_tx, proceed_rx)));
            store
                .lock_inner()
                .unwrap()
                .connection
                .busy_handler(Some(descriptor_busy_gate))
                .unwrap();
            let result = store.rebind_chat_descriptor(card, &source, &replacement);
            (store, card, source, result)
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        transaction.commit().unwrap();
        let database_before = std::fs::read(&path).unwrap();
        let wal_path = path.with_file_name("state.sqlite3-wal");
        let wal_before = std::fs::read(&wal_path).unwrap();
        let schema_before: Vec<(String, Option<String>)> = winner
            .prepare("SELECT name, sql FROM sqlite_schema ORDER BY type, name")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        proceed_tx.send(()).unwrap();
        let (store, card, source, result) = delayed.join().unwrap();
        assert!(
            matches!(result, Err(StoreError::UnsupportedSchema {found, supported}) if found == CURRENT_SCHEMA_VERSION + 1 && supported == CURRENT_SCHEMA_VERSION)
        );
        assert!(
            matches!(
                store.rebind_chat_descriptor(card, &source, &source),
                Err(StoreError::UnsupportedSchema { .. })
            ),
            "future schema refuses idempotent return too"
        );
        assert_eq!(
            store.lock_inner().unwrap().connection.total_changes(),
            changes
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            database_before,
            "refusal must not write database bytes"
        );
        assert_eq!(
            std::fs::read(&wal_path).unwrap(),
            wal_before,
            "refusal must not append WAL frames"
        );
        let schema_after: Vec<(String, Option<String>)> = winner
            .prepare("SELECT name, sql FROM sqlite_schema ORDER BY type, name")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(schema_before, schema_after);
        assert_eq!(
            winner
                .query_row("SELECT evidence FROM future_descriptor_owner", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "keep future bytes"
        );
        let stored: String = winner
            .query_row(
                "SELECT content_json FROM cards WHERE card_id=?1",
                [card.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            from_json::<crate::chat::Session>("chat card", &stored).unwrap(),
            source
        );
    }

    #[test]
    fn codex_resume_normalization_is_transactional_and_ordinary_saves_stay_strict() {
        let directory = tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("chat.sqlite3"), boot("chat-boot")).unwrap();
        let (mut workspace, mut source) = agent_workspace(crate::agent::Provider::Codex);
        let previous = source
            .terminal
            .as_ref()
            .unwrap()
            .continuity
            .live_proof()
            .unwrap();
        let conversation = source.launch.session_id.take();
        source.proof = Some(previous.clone());
        source.terminal = None;
        source.launch.recipe.arguments = vec!["--remote".into(), "unix://".into()];
        assert!(source.claim_owned_codex_listener(Path::new("/run/test-native"), &previous));
        source.launch.session_id = conversation;
        source.native_continuity = crate::chat::NativeContinuity::Durable {
            provider_version: "0.100.0".into(),
        };
        let card = workspace.cards[0].id;
        workspace.cards[0].kind = CardKind::Chat(source.clone());
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        let mut replacement = source.clone();
        replacement.proof.as_mut().unwrap().host_incarnation = "next-daemon".into();
        replacement.launch.recipe.arguments.splice(
            0..0,
            [
                "resume".into(),
                replacement.launch.session_id.clone().unwrap(),
            ],
        );
        workspace.cards[0].kind = CardKind::Chat(replacement.clone());
        assert!(lane.save(&store, &workspace).is_err());
        let mut stale = source.clone();
        stale
            .launch
            .recipe
            .environment
            .insert("ACCOUNT_HOME".into(), "/other".into());
        assert!(
            store
                .rebind_chat_upgrade(card, &stale, &replacement)
                .is_err()
        );
        assert_eq!(
            store.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(source.clone())
        );
        store
            .rebind_chat_upgrade(card, &source, &replacement)
            .unwrap();
        store
            .rebind_chat_upgrade(card, &source, &replacement)
            .unwrap();
        lane.save(&store, &workspace).unwrap();
        drop(store);
        let reopened =
            StateStore::open(directory.path().join("chat.sqlite3"), boot("chat-boot")).unwrap();
        assert_eq!(
            reopened.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(replacement)
        );
    }

    #[test]
    fn legacy_and_durable_codex_hundred_panes_save_reopen_across_three_generations() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("migration.sqlite3");
        let boot_id = boot("chat-boot");
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        let (mut workspace, template) = agent_workspace(crate::agent::Provider::Codex);
        let card_template = workspace.cards[0].clone();
        let proof = template
            .terminal
            .as_ref()
            .unwrap()
            .continuity
            .live_proof()
            .unwrap();
        workspace.cards.clear();
        for index in 0..100 {
            let mut source = template.clone();
            source.id = TerminalId::new();
            source.proof = Some(proof.clone());
            source.terminal = None;
            source.launch.session_id = None;
            source.launch.recipe.arguments = vec![
                "--remote".into(),
                "unix://".into(),
                "--model".into(),
                "fixture-luna".into(),
            ];
            source.launch.recipe.environment.insert(
                "CODEX_HOME".into(),
                format!(
                    "/fixture/{}/account-{index}",
                    if index % 2 == 0 { "system" } else { "managed" }
                ),
            );
            source
                .launch
                .recipe
                .unset_environment
                .insert("OPENAI_API_KEY".into());
            assert!(source.claim_owned_codex_listener(Path::new("/fixture/listeners"), &proof));
            source.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
            if index % 3 == 0 {
                source.native_continuity = crate::chat::NativeContinuity::Durable {
                    provider_version: "0.162.0".into(),
                };
            }
            if index % 5 == 0 {
                let mut terminal = template.terminal.clone().unwrap();
                terminal.id = source.id;
                terminal.launch = source.launch.terminal_recipe().unwrap();
                source.terminal = Some(terminal);
            }
            let mut card = card_template.clone();
            card.id = CardId::new();
            card.exposure = if index == 0 {
                Exposure::Visible
            } else {
                Exposure::Offscreen
            };
            card.kind = CardKind::Chat(source);
            workspace
                .chat_drafts
                .insert(card.id, format!("Unsent draft for pane {index}"));
            workspace.cards.push(card);
        }
        workspace.focused = Some(workspace.cards[0].id);
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        for generation in 1..=3 {
            for card in &mut workspace.cards {
                let CardKind::Chat(source) = &card.kind else {
                    unreachable!()
                };
                let mut replacement = source.clone();
                replacement.proof.as_mut().unwrap().host_incarnation =
                    format!("migration-{generation}").into();
                if let Some(terminal) = replacement.terminal.as_mut() {
                    terminal.continuity = TerminalContinuity::Live {
                        host_session_id: proof.host_session_id.clone(),
                        boot_id: proof.boot_id.clone(),
                        host_incarnation: format!("migration-{generation}").into(),
                    };
                }
                assert!(crate::chat::is_upgrade_recovery(source, &replacement));
                store
                    .rebind_chat_upgrade(card.id, source, &replacement)
                    .unwrap();
                card.kind = CardKind::Chat(replacement);
            }
            lane.save(&store, &workspace).unwrap();
            let reopened = StateStore::open(&path, boot_id.clone()).unwrap();
            let restored = reopened
                .load_workspace(workspace.id, None)
                .unwrap()
                .unwrap();
            assert_eq!(restored.cards.len(), 100);
            assert_eq!(restored.chat_drafts, workspace.chat_drafts);
            for (expected, actual) in workspace.cards.iter().zip(&restored.cards) {
                assert_eq!(actual.kind, expected.kind);
            }
        }
    }

    #[test]
    fn authenticated_upgrade_identity_learning_is_transactional_and_keeps_pair_and_account_exact() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("identity-learning.sqlite3");
        let store = StateStore::open(&path, boot("chat-boot")).unwrap();
        let (mut workspace, mut source) = agent_workspace(crate::agent::Provider::Codex);
        source.proof = source.terminal.as_ref().unwrap().continuity.live_proof();
        let conversation = source.launch.session_id.take().unwrap();
        let mut replacement = source.clone();
        replacement.launch.session_id = Some(conversation);
        replacement.proof.as_mut().unwrap().host_incarnation = "learned-daemon".into();
        let proof = replacement.proof.as_ref().unwrap();
        replacement.terminal.as_mut().unwrap().continuity = TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        assert!(!crate::chat::is_upgrade_recovery(&source, &replacement));
        assert!(crate::chat::is_upgrade_identity_learning(
            &source,
            &replacement
        ));
        let card = workspace.cards[0].id;
        workspace.cards[0].kind = CardKind::Chat(source.clone());
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        for mutation in 0..9 {
            let mut changed = replacement.clone();
            match mutation {
                0 => {
                    changed
                        .launch
                        .recipe
                        .environment
                        .insert("CODEX_HOME".into(), "/different-account".into());
                }
                1 => {
                    changed
                        .launch
                        .recipe
                        .unset_environment
                        .insert("CHANGED_SETTING".into());
                }
                2 => changed
                    .launch
                    .recipe
                    .arguments
                    .push("--different-options".into()),
                3 => changed.launch.recipe.working_directory = "/different-project".into(),
                4 => changed.proof = source.proof.clone(),
                5 => changed.proof.as_mut().unwrap().boot_id = boot("another-boot"),
                6 => changed.terminal.as_mut().unwrap().resume_hint = Some("another-thread".into()),
                7 => changed
                    .terminal
                    .as_mut()
                    .unwrap()
                    .launch
                    .arguments
                    .push("--different-listener".into()),
                _ => changed.terminal.as_mut().unwrap().id = TerminalId::new(),
            }
            assert!(
                !crate::chat::is_upgrade_identity_learning(&source, &changed),
                "mutation {mutation}"
            );
            assert!(store.rebind_chat_upgrade(card, &source, &changed).is_err());
        }
        workspace.cards[0].kind = CardKind::Chat(replacement.clone());
        assert!(
            lane.save(&store, &workspace).is_err(),
            "ordinary save cannot learn a paired upgrade identity"
        );
        store
            .rebind_chat_upgrade(card, &source, &replacement)
            .unwrap();
        lane.save(&store, &workspace).unwrap();
        drop(store);
        let reopened = StateStore::open(path, boot("chat-boot")).unwrap();
        assert_eq!(
            reopened.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(replacement)
        );
    }

    #[test]
    fn upgrade_recovery_requires_exact_lineage_and_keeps_snapshots_strict() {
        let directory = tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("chat.sqlite3"), boot("chat-boot")).unwrap();
        let (mut workspace, mut source) = agent_workspace(crate::agent::Provider::Omp);
        source.proof = source.terminal.as_ref().unwrap().continuity.live_proof();
        source.terminal = None;
        let card = workspace.cards[0].id;
        workspace.cards[0].kind = CardKind::Chat(source.clone());
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();

        // Restarting the same provider conversation with its issued recipe
        // remains an ordinary save; an explicit upgrade still preserves its known ID.
        source.proof.as_mut().unwrap().host_incarnation = "second-daemon".into();
        workspace.cards[0].kind = CardKind::Chat(source.clone());
        lane.save(&store, &workspace).unwrap();
        let mut replacement = source.clone();
        replacement.proof.as_mut().unwrap().host_incarnation = "third-daemon".into();
        let mut wrong_thread = replacement.clone();
        wrong_thread.launch.session_id = Some("00000000-0000-4000-8000-000000000002".into());
        workspace.cards[0].kind = CardKind::Chat(wrong_thread.clone());
        assert!(lane.save(&store, &workspace).is_err());
        assert!(
            store
                .rebind_chat_upgrade(card, &source, &wrong_thread)
                .is_err()
        );
        assert_eq!(
            store.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(source.clone())
        );
        workspace.cards[0].kind = CardKind::Chat(replacement.clone());
        for altered in 0..8 {
            let mut refused = replacement.clone();
            match altered {
                0 => refused.id = TerminalId::new(),
                1 => refused.launch.provider = crate::agent::Provider::Claude,
                2 => refused.launch.recipe.program = "other-provider".into(),
                3 => {
                    refused
                        .launch
                        .recipe
                        .environment
                        .insert("ACCOUNT_HOME".into(), "/other".into());
                }
                4 => refused.proof = None,
                5 => refused.proof = source.proof.clone(),
                6 => refused.launch.session_id = None,
                _ => refused.proof.as_mut().unwrap().host_incarnation = "".into(),
            }
            assert!(
                store.rebind_chat_upgrade(card, &source, &refused).is_err(),
                "alteration {altered}"
            );
        }
        let mut stale = source.clone();
        stale.launch.recipe.arguments.push("--stale".into());
        let mut stale_replacement = replacement.clone();
        stale_replacement.launch.recipe = stale.launch.recipe.clone();
        assert!(
            store
                .rebind_chat_upgrade(card, &stale, &stale_replacement)
                .is_err()
        );
        assert_eq!(
            store.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(source.clone())
        );
        store
            .rebind_chat_upgrade(card, &source, &replacement)
            .unwrap();
        store
            .rebind_chat_upgrade(card, &source, &replacement)
            .unwrap();
        lane.save(&store, &workspace).unwrap();
        let reopened =
            StateStore::open(directory.path().join("chat.sqlite3"), boot("chat-boot")).unwrap();
        assert_eq!(
            reopened.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(replacement)
        );
    }

    #[test]
    fn raw_retained_upgrade_rebind_cannot_replace_authenticated_actor_evidence() {
        for known in [false, true] {
            let directory = tempdir().unwrap();
            let store = StateStore::open(
                directory.path().join("retained-upgrade.sqlite3"),
                boot("chat-boot"),
            )
            .unwrap();
            let (mut workspace, mut source) = agent_workspace(crate::agent::Provider::Claude);
            source.proof = source.terminal.as_ref().unwrap().continuity.live_proof();
            source.terminal = None;
            source.launch.recipe.arguments.clear();
            if !known {
                source.launch.session_id = None;
            }
            source.native_continuity =
                crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
            let card = workspace.cards[0].id;
            workspace.cards[0].kind = CardKind::Chat(source.clone());
            workspace
                .chat_drafts
                .insert(card, "Keep this unsent draft".into());
            let mut lane = SaveLane::default();
            lane.save(&store, &workspace).unwrap();
            for learn_or_replace in [false, true] {
                let mut replacement = source.clone();
                replacement.proof.as_mut().unwrap().host_incarnation = "successor".into();
                if learn_or_replace {
                    replacement.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
                }
                assert!(
                    store
                        .rebind_chat_upgrade(card, &source, &replacement)
                        .is_err(),
                    "descriptor-only rebind must require the authenticated retained witness"
                );
                assert_eq!(
                    store.load_card(card, None).unwrap().unwrap().kind,
                    CardKind::Chat(source.clone())
                );
                assert_eq!(
                    store
                        .load_workspace(workspace.id, None)
                        .unwrap()
                        .unwrap()
                        .chat_drafts,
                    workspace.chat_drafts,
                    "a refused transition preserves the committed draft"
                );
            }
        }
    }

    #[test]
    fn account_recovery_rebinds_only_the_exact_saved_conversation() {
        let directory = tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("chat.sqlite3"), boot("chat-boot")).unwrap();
        let (mut workspace, mut source) = agent_workspace(crate::agent::Provider::Codex);
        source.proof = source.terminal.as_ref().unwrap().continuity.live_proof();
        source.terminal = None;
        source
            .launch
            .recipe
            .environment
            .insert("CODEX_HOME".into(), "/accounts/removed".into());
        let card = workspace.cards[0].id;
        workspace.cards[0].kind = CardKind::Chat(source.clone());
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        let mut replacement = source.clone();
        replacement.id = TerminalId::new();
        replacement
            .launch
            .recipe
            .environment
            .insert("CODEX_HOME".into(), "/accounts/selected".into());
        replacement.launch.recipe.unset_environment.extend(
            crate::account::credential_overrides(crate::agent::Provider::Codex)
                .iter()
                .map(|name| (*name).to_owned()),
        );
        workspace.cards[0].kind = CardKind::Chat(replacement.clone());
        assert!(
            lane.save(&store, &workspace).is_err(),
            "ordinary snapshots cannot rebind an account"
        );
        for altered in 0..5 {
            let mut refused = replacement.clone();
            match altered {
                0 => refused.launch.session_id = Some("different-conversation".into()),
                1 => refused.proof = None,
                2 => refused
                    .launch
                    .recipe
                    .arguments
                    .push("--different-option".into()),
                3 => refused.launch.provider = crate::agent::Provider::Claude,
                _ => {
                    refused
                        .launch
                        .recipe
                        .environment
                        .insert("OPENAI_API_KEY".into(), "fixture".into());
                }
            }
            assert!(store.rebind_chat_account(card, &source, &refused).is_err());
        }
        let mut stale = source.clone();
        stale
            .launch
            .recipe
            .environment
            .insert("OTHER".into(), "stale".into());
        let mut stale_replacement = replacement.clone();
        stale_replacement
            .launch
            .recipe
            .environment
            .insert("OTHER".into(), "stale".into());
        assert!(
            store
                .rebind_chat_account(card, &stale, &stale_replacement)
                .is_err()
        );
        assert_eq!(
            store.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(source.clone())
        );
        store
            .rebind_chat_account(card, &source, &replacement)
            .unwrap();
        store
            .rebind_chat_account(card, &source, &replacement)
            .unwrap();
        lane.save(&store, &workspace).unwrap();
        let reopened =
            StateStore::open(directory.path().join("chat.sqlite3"), boot("chat-boot")).unwrap();
        assert_eq!(
            reopened.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(replacement)
        );
    }

    /// A chat card takes a new provider, account, or conversation only through
    /// explicit replacement, never through an ordinary workspace snapshot.
    #[test]
    fn a_chat_card_takes_a_new_conversation_only_through_its_replacement() {
        let directory = tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("chat.sqlite3"), boot("chat-boot")).unwrap();
        let (mut workspace, begun) = agent_workspace(crate::agent::Provider::Codex);
        let source = begun.terminal.clone().unwrap();
        let proof = source.continuity.live_proof();
        let fresh = |provider: crate::agent::Provider, id: TerminalId, home: Option<&str>| {
            let mut recipe = begun.launch.recipe.clone();
            recipe.program = provider.executable().into();
            recipe.arguments.clear();
            if let Some(home) = home {
                recipe
                    .environment
                    .insert("CLAUDE_CONFIG_DIR".into(), home.into());
            }
            crate::chat::Session {
                native_continuity: Default::default(),
                owned_codex_listener: None,
                id,
                launch: crate::chat::Launch {
                    provider,
                    recipe,
                    session_id: None,
                },
                proof: None,
                terminal: None,
            }
        };
        // A thread the daemon started already names its conversation, which
        // is why the store cannot tell on its own that it is empty.
        let started = |session: &crate::chat::Session| crate::chat::Session {
            launch: crate::chat::Launch {
                session_id: Some(uuid::Uuid::new_v4().to_string()),
                ..session.launch.clone()
            },
            proof: proof.clone(),
            ..session.clone()
        };
        let codex = started(&fresh(
            crate::agent::Provider::Codex,
            TerminalId::new(),
            None,
        ));
        let card = workspace.cards[0].id;
        workspace.cards[0].kind = CardKind::Chat(codex.clone());
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();

        // Another provider under the same identity.
        let claude = fresh(crate::agent::Provider::Claude, codex.id, None);
        workspace.cards[0].kind = CardKind::Chat(claude.clone());
        assert!(matches!(
            card_refusal(lane.save(&store, &workspace), workspace.cards[0].id),
            StoreError::InvalidState(_)
        ));
        store
            .replace_chat_identity(card, codex.id, &claude)
            .unwrap();
        // A retried replacement that already landed is not an error.
        store
            .replace_chat_identity(card, codex.id, &claude)
            .unwrap();
        lane.save(&store, &workspace).unwrap();
        // The new thread then names its conversation through the proof check.
        let claude = started(&claude);
        workspace.cards[0].kind = CardKind::Chat(claude.clone());
        lane.save(&store, &workspace).unwrap();

        // Another account, which also renews the identity.
        let managed = fresh(
            crate::agent::Provider::Claude,
            TerminalId::new(),
            Some("/accounts/claude/a"),
        );
        assert!(matches!(
            store.replace_chat_identity(card, TerminalId::new(), &managed),
            Err(StoreError::InvalidState(_))
        ));
        // Only a conversation that has not begun takes a card's place: a
        // replacement cannot smuggle in a native session the proof check
        // would have refused.
        assert!(matches!(
            store.replace_chat_identity(card, claude.id, &started(&managed)),
            Err(StoreError::InvalidState(_))
        ));
        store
            .replace_chat_identity(card, claude.id, &managed)
            .unwrap();
        workspace.cards[0].kind = CardKind::Chat(managed.clone());
        lane.save(&store, &workspace).unwrap();
        assert_eq!(
            store.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(managed.clone())
        );
        // What the next launch restores is the new conversation.
        drop(store);
        let reopened =
            StateStore::open(directory.path().join("chat.sqlite3"), boot("chat-boot")).unwrap();
        assert_eq!(
            reopened.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(managed.clone())
        );

        // A replacement never gives a card a source it did not have.
        let mut paired = fresh(crate::agent::Provider::Claude, managed.id, None);
        paired.terminal = Some(source);
        assert!(matches!(
            reopened.replace_chat_identity(card, managed.id, &paired),
            Err(StoreError::InvalidState(_))
        ));
    }

    /// A new Codex thread asks for a listener of its own with a bare
    /// `unix://` remote, and the daemon starts it on that thread's socket.
    /// The card is saved before the start completes, so the started thread
    /// must save over it; that used to be refused as a changed launch, which
    /// failed every later save and left the card with no conversation to
    /// restore.
    #[test]
    fn a_new_codex_thread_saves_as_the_daemon_started_it() {
        let directory = tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("chat.sqlite3"), boot("chat-boot")).unwrap();
        let (mut workspace, begun) = agent_workspace(crate::agent::Provider::Codex);
        let proof = begun.terminal.as_ref().unwrap().continuity.live_proof();
        let mut asked = begun.clone();
        asked.launch.recipe.arguments = vec!["--remote".into(), "unix://".into()];
        asked.launch.session_id = None;
        asked.proof = None;
        asked.terminal = None;
        asked.id = TerminalId::new();
        workspace.cards[0].kind = CardKind::Chat(asked.clone());
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        let started = |remote: String| {
            let mut session = asked.clone();
            session.launch.recipe.arguments[1] = remote;
            session.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
            session.proof = proof.clone();
            session
        };
        let daemon_root = directory.path().join("daemon-tmp");
        assert_ne!(daemon_root, std::env::temp_dir());
        let mut own = asked.clone();
        assert!(own.claim_owned_codex_listener(&daemon_root, proof.as_ref().unwrap()));
        own.proof = proof.clone();
        own.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        let mut save = |session: &crate::chat::Session| {
            let mut workspace = workspace.clone();
            workspace.cards[0].kind = CardKind::Chat(session.clone());
            lane.save(&store, &workspace)
        };

        // Any other endpoint is a different launch.
        let elsewhere = crate::chat::Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: TerminalId::new(),
            ..asked.clone()
        };
        for remote in [
            "unix:///tmp/elsewhere/control.sock".to_owned(),
            format!(
                "unix://{}/cagents-codex-{}/control.sock",
                daemon_root.display(),
                elsewhere.id
            ),
        ] {
            assert!(matches!(
                card_refusal(save(&started(remote)), workspace.cards[0].id),
                StoreError::InvalidState(message) if message.contains("launch recipe")
            ));
        }
        let mut other_option = own.clone();
        other_option.launch.recipe.arguments.push("--search".into());
        assert!(matches!(
            card_refusal(save(&other_option), workspace.cards[0].id),
            StoreError::InvalidState(_)
        ));

        assert!(own.owns_codex_listener());
        save(&own).unwrap();
        let reopened =
            StateStore::open(directory.path().join("chat.sqlite3"), boot("chat-boot")).unwrap();
        match reopened.load_card(workspace.cards[0].id, None).unwrap() {
            Some(CardState {
                kind: CardKind::Chat(stored),
                ..
            }) => assert_eq!(stored, own),
            other => panic!("the card is stored as the started thread: {other:?}"),
        }
        // Once begun, the thread keeps that endpoint.
        let mut rewound = own.clone();
        rewound.launch.recipe.arguments[1] = "unix://".into();
        assert!(matches!(
            card_refusal(save(&rewound), workspace.cards[0].id),
            StoreError::InvalidState(_)
        ));
        let mut changed_allocator = own.clone();
        changed_allocator
            .owned_codex_listener
            .as_mut()
            .unwrap()
            .allocator
            .host_incarnation = "unrelated".into();
        assert!(matches!(
            card_refusal(save(&changed_allocator), workspace.cards[0].id),
            StoreError::InvalidState(_)
        ));
        let mut next_host = own.clone();
        next_host.proof.as_mut().unwrap().host_incarnation = "next-host".into();
        save(&next_host).unwrap();
    }

    #[test]
    fn terminal_listener_evidence_survives_legacy_migration_and_restore() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("terminal.sqlite3");
        let store = StateStore::open(&database, boot("chat-boot")).unwrap();
        let (mut workspace, mut chat) = agent_workspace(crate::agent::Provider::Codex);
        let card = workspace.cards[0].id;
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        drop(store);
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE terminal_recipes DROP COLUMN owned_codex_listener;
                 ALTER TABLE terminal_recipes DROP COLUMN predecessor;
                 DROP TABLE review_cache_files;
                 DROP TABLE review_cache;
                 PRAGMA user_version = 7;",
            )
            .unwrap();
        drop(connection);
        let store = StateStore::open(&database, boot("chat-boot")).unwrap();
        let restored = store.load_card(card, None).unwrap().unwrap();
        let CardKind::Terminal(legacy) = restored.kind else {
            panic!("terminal")
        };
        assert!(legacy.owned_codex_listener.is_none());
        assert!(legacy.predecessor.is_none());
        let CardKind::Terminal(original) = &workspace.cards[0].kind else {
            panic!("terminal")
        };
        assert_eq!(legacy.launch, original.launch);

        let proof = original.continuity.live_proof().unwrap();
        chat.terminal = None;
        chat.launch.session_id = None;
        chat.launch.recipe.arguments = vec!["--remote".into(), "unix://".into()];
        assert!(chat.claim_owned_codex_listener(&directory.path().join("daemon-tmp"), &proof));
        chat.launch.session_id = Some("77777777-1111-4111-8111-111111111111".into());
        let CardKind::Terminal(terminal) = &mut workspace.cards[0].kind else {
            panic!("terminal")
        };
        terminal.launch = chat.launch.terminal_recipe().unwrap();
        terminal.resume_hint = None;
        terminal.owned_codex_listener = chat.owned_codex_listener.clone();
        lane.save(&store, &workspace).unwrap();
        let restored = store.load_card(card, None).unwrap().unwrap();
        let CardKind::Terminal(restored) = restored.kind else {
            panic!("terminal")
        };
        assert!(restored.owns_codex_listener());
        assert_eq!(restored.owned_codex_listener, chat.owned_codex_listener);
        assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
        let mut corrupt = chat.owned_codex_listener.unwrap();
        corrupt.socket = PathBuf::from("relative.sock");
        store
            .lock_inner()
            .unwrap()
            .connection
            .execute(
                "UPDATE terminal_recipes SET owned_codex_listener = ?1 WHERE card_id = ?2",
                [serde_json::to_string(&corrupt).unwrap(), card.to_string()],
            )
            .unwrap();
        let error = store.load_card(card, None).unwrap_err();
        assert!(matches!(error, StoreError::CorruptRow { .. }));
        assert!(!error.refuses_content());
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
            card_refusal(lane.save(&store, &workspace), workspace.cards[0].id),
            StoreError::InvalidState(_)
        ));
        workspace.cards[0].kind = CardKind::Chat(chat.clone());
        lane.save(&store, &workspace).unwrap();
        let mut wrong = chat.clone();
        wrong.id = TerminalId::new();
        workspace.cards[0].kind = CardKind::Chat(wrong);
        assert!(matches!(
            card_refusal(lane.save(&store, &workspace), workspace.cards[0].id),
            StoreError::InvalidState(_)
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

    /// A native Codex thread returns through a CLI view the daemon opens
    /// under the chat's own identity; refusing it left every later save of
    /// the window failing with "saved state is invalid".
    #[test]
    fn a_native_codex_thread_converts_to_the_cli_view_the_daemon_opened_for_it() {
        // The app records the view as the chat's source before it converts
        // the card, but the writer may coalesce the two into one write.
        for paired_first in [true, false] {
            let directory = tempdir().unwrap();
            let store =
                StateStore::open(directory.path().join("chat.sqlite3"), boot("chat-boot")).unwrap();
            let (mut workspace, paired) = agent_workspace(crate::agent::Provider::Codex);
            let source = paired.terminal.unwrap();
            let proof = source.continuity.live_proof().unwrap();
            let mut native = crate::chat::Session {
                native_continuity: Default::default(),
                owned_codex_listener: None,
                id: TerminalId::new(),
                launch: paired.launch,
                proof: Some(proof.clone()),
                terminal: None,
            };
            native.launch.recipe.arguments = vec!["--remote".into(), "unix://".into()];
            let conversation = native.launch.session_id.take();
            assert!(
                native.claim_owned_codex_listener(&directory.path().join("daemon-tmp"), &proof)
            );
            native.launch.session_id = conversation;
            workspace.cards[0].kind = CardKind::Chat(native.clone());
            let mut lane = SaveLane::default();
            lane.save(&store, &workspace).unwrap();

            let view = TerminalSession {
                owned_codex_listener: native.owned_codex_listener.clone(),
                recovery_origin: None,
                predecessor: None,
                id: native.id,
                launch: native.launch.terminal_recipe().unwrap(),
                transcript_path: None,
                resume_hint: None,
                continuity: source.continuity.clone(),
            };
            let mut other_host = view.clone();
            other_host.continuity = TerminalContinuity::DetachedSameBoot {
                host_session_id: "another-host".into(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            };
            let mut other_command = view.clone();
            other_command.launch.arguments.push("--yolo".into());
            let mut unproven = view.clone();
            unproven.continuity = TerminalContinuity::RelaunchAvailable {
                recipe: view.launch.clone(),
                last_exit: None,
            };
            for wrong in [other_host, other_command, unproven] {
                workspace.cards[0].kind = CardKind::Terminal(wrong.clone());
                assert!(matches!(
                    card_refusal(lane.save(&store, &workspace), workspace.cards[0].id),
                    StoreError::InvalidState(_)
                ));
                let mut paired = native.clone();
                paired.terminal = Some(wrong);
                workspace.cards[0].kind = CardKind::Chat(paired);
                assert!(matches!(
                    card_refusal(lane.save(&store, &workspace), workspace.cards[0].id),
                    StoreError::InvalidState(_)
                ));
            }
            if paired_first {
                let mut paired = native.clone();
                paired.terminal = Some(view.clone());
                workspace.cards[0].kind = CardKind::Chat(paired);
                lane.save(&store, &workspace).unwrap();
            }
            workspace.cards[0].kind = CardKind::Terminal(view.clone());
            lane.save(&store, &workspace).unwrap();
            assert_eq!(
                store
                    .load_card(workspace.cards[0].id, Some(&proof))
                    .unwrap()
                    .unwrap()
                    .kind,
                CardKind::Terminal(view.clone())
            );
            // And the CLI switches back to the chat it came from.
            let mut paired = native.clone();
            paired.terminal = Some(view);
            workspace.cards[0].kind = CardKind::Chat(paired.clone());
            lane.save(&store, &workspace).unwrap();
            let restored = store
                .load_card(workspace.cards[0].id, Some(&proof))
                .unwrap()
                .unwrap();
            assert_eq!(restored.kind, CardKind::Chat(paired));
        }
    }

    #[test]
    fn durable_native_adoption_persists_exact_pair_and_refuses_capability_loss() {
        let directory = tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("native.sqlite3"), boot("chat-boot")).unwrap();
        let (mut workspace, mut chat) = agent_workspace(crate::agent::Provider::Codex);
        let mut terminal = chat.terminal.take().unwrap();
        let proof = terminal.continuity.live_proof().unwrap();
        chat.proof = Some(proof.clone());
        let conversation = chat.launch.session_id.take();
        chat.launch.recipe.arguments = vec!["--remote".into(), "unix://".into()];
        assert!(chat.claim_owned_codex_listener(&directory.path().join("daemon-tmp"), &proof));
        chat.launch.session_id = conversation;
        chat.native_continuity = crate::chat::NativeContinuity::Durable {
            provider_version: "0.157.1".into(),
        };
        terminal.launch = chat.launch.terminal_recipe().unwrap();
        terminal.owned_codex_listener = chat.owned_codex_listener.clone();
        chat.terminal = Some(terminal);
        workspace.cards[0].kind = CardKind::Chat(chat.clone());
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();

        let mut successor = chat.clone();
        let mut next_proof = proof.clone();
        next_proof.host_incarnation = "next-daemon".into();
        successor.proof = Some(next_proof.clone());
        successor.terminal.as_mut().unwrap().continuity = TerminalContinuity::Live {
            host_session_id: next_proof.host_session_id.clone(),
            boot_id: next_proof.boot_id.clone(),
            host_incarnation: next_proof.host_incarnation.clone(),
        };
        workspace.cards[0].kind = CardKind::Chat(successor.clone());
        lane.save(&store, &workspace).unwrap();
        assert_eq!(
            store
                .load_card(workspace.cards[0].id, Some(&next_proof))
                .unwrap()
                .unwrap()
                .kind,
            CardKind::Chat(successor.clone())
        );
        for continuity in [
            crate::chat::NativeContinuity::ReconnectOnly,
            crate::chat::NativeContinuity::Durable {
                provider_version: "0.158.0".into(),
            },
        ] {
            let mut wrong = successor.clone();
            wrong.native_continuity = continuity;
            workspace.cards[0].kind = CardKind::Chat(wrong);
            assert!(matches!(
                card_refusal(lane.save(&store, &workspace), workspace.cards[0].id),
                StoreError::InvalidState(_)
            ));
        }
        let mut wrong = successor.clone();
        wrong
            .terminal
            .as_mut()
            .unwrap()
            .launch
            .arguments
            .push("--yolo".into());
        workspace.cards[0].kind = CardKind::Chat(wrong);
        assert!(matches!(
            card_refusal(lane.save(&store, &workspace), workspace.cards[0].id),
            StoreError::InvalidState(_)
        ));
        assert_eq!(
            store
                .load_card(workspace.cards[0].id, Some(&next_proof))
                .unwrap()
                .unwrap()
                .kind,
            CardKind::Chat(successor)
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
        // Use the current writer to populate the fixture, then restore the
        // historical schema before exercising the real migration chain.
        transaction
            .execute_batch(
                "ALTER TABLE terminal_recipes ADD COLUMN recovery_origin TEXT;
                 ALTER TABLE terminal_recipes ADD COLUMN owned_codex_listener TEXT;
                 ALTER TABLE terminal_recipes ADD COLUMN predecessor TEXT;",
            )
            .unwrap();
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
            &mut RefusedClosures::new(),
        )
        .unwrap();
        transaction.pragma_update(None, "user_version", 3).unwrap();
        transaction
            .execute_batch(
                "ALTER TABLE terminal_recipes DROP COLUMN recovery_origin;
                 ALTER TABLE terminal_recipes DROP COLUMN owned_codex_listener;
                 ALTER TABLE terminal_recipes DROP COLUMN predecessor;",
            )
            .unwrap();
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

    #[test]
    fn v6_migration_preserves_cards_and_admits_utility_panes() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("v5.sqlite3");
        let mut connection = Connection::open(&path).unwrap();
        configure_connection(&connection, DEFAULT_BUSY_TIMEOUT).unwrap();
        let transaction = connection.transaction().unwrap();
        migrate_to_v1(&transaction).unwrap();
        migrate_to_v2(&transaction).unwrap();
        migrate_to_v3(&transaction).unwrap();
        migrate_to_v4(&transaction).unwrap();
        transaction
            .execute_batch(
                "ALTER TABLE terminal_recipes ADD COLUMN recovery_origin TEXT;
                 ALTER TABLE terminal_recipes ADD COLUMN owned_codex_listener TEXT;
                 ALTER TABLE terminal_recipes ADD COLUMN predecessor TEXT;",
            )
            .unwrap();
        let (mut workspace, chat) = agent_workspace(crate::agent::Provider::Omp);
        let mut browser = workspace.cards[0].clone();
        browser.id = CardId::new();
        let url = Url::parse("https://example.com/kept").unwrap();
        browser.kind = CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: url.clone(),
            history: vec![url],
            history_index: 0,
            scroll_x: 0.0,
            scroll_y: 40.0,
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
            &mut RefusedClosures::new(),
        )
        .unwrap();
        // The v5 table must refuse the new kinds, or this test proves nothing.
        assert!(
            transaction
                .execute(
                    "UPDATE cards SET kind_tag = 'settings' WHERE card_id = ?1",
                    [browser.id.to_string()],
                )
                .is_err()
        );
        transaction.pragma_update(None, "user_version", 5).unwrap();
        transaction
            .execute_batch(
                "ALTER TABLE terminal_recipes DROP COLUMN owned_codex_listener;
                 ALTER TABLE terminal_recipes DROP COLUMN predecessor;",
            )
            .unwrap();
        transaction.commit().unwrap();
        drop(connection);

        let store = StateStore::open(&path, boot("v6-boot")).unwrap();
        assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(
            restored
                .cards
                .iter()
                .map(|card| card.id)
                .collect::<Vec<_>>(),
            workspace
                .cards
                .iter()
                .map(|card| card.id)
                .collect::<Vec<_>>()
        );
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
        for kind in [
            CardKind::Settings,
            CardKind::Accounts,
            CardKind::Automations,
            CardKind::Work,
        ] {
            let mut card = workspace.cards[1].clone();
            card.id = CardId::new();
            card.title = "Utility".into();
            card.kind = kind;
            workspace.cards.push(card);
        }
        lane.save(&store, &workspace).unwrap();
        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(
            restored
                .cards
                .iter()
                .map(|card| card.kind.clone())
                .skip(2)
                .collect::<Vec<_>>(),
            vec![
                CardKind::Settings,
                CardKind::Accounts,
                CardKind::Automations,
                CardKind::Work
            ]
        );
        let inner = store.lock_inner().unwrap();
        assert!(
            inner
                .connection
                .execute(
                    "UPDATE cards SET kind_tag = 'unknown' WHERE card_id = ?1",
                    [browser.id.to_string()],
                )
                .is_err(),
            "the rebuilt table still checks its kinds"
        );
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
    fn project_kind_migration_preserves_git_and_round_trips_folders() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let project = ProjectState {
            id: ProjectId::new(),
            generation: 0,
            name: "Existing repository".into(),
            repository: directory.path().join("repository/.git"),
            kind: crate::model::ProjectKind::Git,
            existence: Existence::Open,
        };
        let store = StateStore::open(&path, boot("before")).unwrap();
        store.save_project(&project).unwrap();
        drop(store);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE projects DROP COLUMN kind_json;
                 ALTER TABLE terminal_recipes DROP COLUMN owned_codex_listener;
                 ALTER TABLE terminal_recipes DROP COLUMN predecessor;
                 DROP TABLE review_cache_files;
                 DROP TABLE review_cache;
                 PRAGMA user_version = 6;",
            )
            .unwrap();
        drop(connection);

        let store = StateStore::open(&path, boot("after")).unwrap();
        let restored = store.load_project(project.id).unwrap().unwrap();
        assert_eq!(restored.kind, crate::model::ProjectKind::Git);
        assert_eq!(restored.repository, project.repository);
        let folder = ProjectState {
            id: ProjectId::new(),
            name: "Folder".into(),
            repository: directory.path().join("plain"),
            kind: crate::model::ProjectKind::Folder,
            ..project
        };
        store.save_project(&folder).unwrap();
        drop(store);
        let store = StateStore::open(&path, boot("restored")).unwrap();
        assert_eq!(
            store.load_project(folder.id).unwrap().unwrap().kind,
            crate::model::ProjectKind::Folder
        );
        assert!(
            store
                .list_projects()
                .unwrap()
                .iter()
                .any(|project| project.id == folder.id
                    && project.kind == crate::model::ProjectKind::Folder)
        );
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
            title_source: Default::default(),
            id: channel_id,
            project_id: workspace.project_order[0],
            title: "Legacy".into(),
            ordinal: 0,
            shared: false,
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
                        format!(
                            "workspace:{}:navigation:{ACTIVE_CHANNEL_FIELD}",
                            workspace.id
                        ),
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
                Err(StoreError::CorruptRow { .. })
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

    /// Only an applied creation gives up its identity; one still unresolved
    /// keeps it, as the lease that stops a second creation racing its effects.
    #[test]
    fn only_an_applied_worktree_creation_is_released() {
        let directory = tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("state.sqlite3"), boot("boot-a")).unwrap();
        let creation = |entity_id: &str| PendingOperation {
            id: OperationId::new(),
            entity_kind: "worktree_creation".into(),
            entity_id: entity_id.into(),
            action: "create".into(),
            base_generation: 1,
            payload: serde_json::json!({}),
            status: PendingOperationStatus::Pending,
            created_at_unix_ms: now_unix_ms(),
            updated_at_unix_ms: now_unix_ms(),
        };
        let applied = creation("created");
        store.begin_operation(&applied).unwrap();
        store
            .resolve_operation(applied.id, PendingOperationStatus::Applied)
            .unwrap();
        let running = creation("running");
        store.begin_operation(&running).unwrap();

        assert!(store.release_worktree_creation(running.id).is_err());
        assert!(store.begin_operation(&creation("running")).is_err());

        assert!(store.begin_operation(&creation("created")).is_err());
        store.release_worktree_creation(applied.id).unwrap();
        assert!(store.release_worktree_creation(applied.id).is_err());
        store.begin_operation(&creation("created")).unwrap();
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
        let error = card_refusal(lane.save(&store, &next), next.cards[0].id);

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
            owned_codex_listener: None,
            recovery_origin: Some(original_id),
            predecessor: Some(original_id),
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
    fn recovered_identity_transfers_from_closed_card_without_losing_history() {
        let directory = tempdir().unwrap();
        let boot_id = boot("boot-a");
        let store =
            StateStore::open(directory.path().join("state.sqlite3"), boot_id.clone()).unwrap();
        let mut workspace = workspace_with_terminal(&boot_id);
        let original_card = workspace.cards[0].id;
        let CardKind::Terminal(original) = workspace.cards[0].kind.clone() else {
            panic!();
        };
        let mut history = workspace.cards[0].clone();
        history.id = CardId::new();
        let mut replacement = original.clone();
        replacement.id = TerminalId::new();
        replacement.recovery_origin = Some(original.id);
        replacement.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: replacement.launch.clone(),
            last_exit: None,
        };
        history.kind = CardKind::Terminal(replacement.clone());
        let history_card = history.id;
        workspace.cards.push(history);
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        // An open card's identity remains exclusive.
        assert!(
            store
                .replace_terminal_identity(original_card, original.id, &replacement)
                .is_err()
        );
        workspace.cards[1].existence = Existence::Closing {
            operation_id: OperationId::new(),
        };
        lane.save(&store, &workspace).unwrap();
        assert!(
            store
                .replace_terminal_identity(original_card, original.id, &replacement)
                .is_err()
        );
        workspace.cards[1].existence = Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: 123,
                reason: Some("closed by user".into()),
            },
        };
        lane.save(&store, &workspace).unwrap();
        // A stale source cannot archive or transfer the closed owner's payload.
        assert!(
            store
                .replace_terminal_identity(CardId::new(), original.id, &replacement)
                .is_err()
        );
        assert!(
            store
                .replace_terminal_identity(original_card, TerminalId::new(), &replacement)
                .is_err()
        );
        assert!(
            store
                .lock_inner()
                .unwrap()
                .connection
                .query_row(
                    "SELECT content_json IS NULL FROM cards WHERE card_id = ?1",
                    [history_card.to_string()],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
        store
            .save_transcript_metadata(
                replacement.id,
                &TranscriptMetadata {
                    path: replacement.transcript_path.clone().unwrap(),
                    byte_len: 123,
                    durable_offset: 100,
                    updated_at_unix_ms: 42,
                    location: None,
                },
                0,
            )
            .unwrap();
        let transcript_before = store.load_transcript_metadata(replacement.id).unwrap();
        store
            .replace_terminal_identity(original_card, original.id, &replacement)
            .unwrap();
        assert_eq!(
            store
                .load_transcript_metadata(replacement.id)
                .unwrap()
                .map(|(_, metadata)| (metadata.byte_len, metadata.durable_offset)),
            transcript_before.map(|(_, metadata)| (metadata.byte_len, metadata.durable_offset))
        );
        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(
            restored.cards[0].kind,
            CardKind::Terminal(replacement.clone())
        );
        assert_eq!(
            restored.cards[1].kind,
            CardKind::Terminal(replacement.clone())
        );
        assert_eq!(restored.cards[1].existence, workspace.cards[1].existence);
        workspace.cards[0].kind = CardKind::Terminal(replacement.clone());
        lane.save(&store, &workspace).unwrap();
        // Even after active ownership is released, an old closed snapshot
        // must not reclaim the payload identity or erase its archived history.
        {
            let inner = store.lock_inner().unwrap();
            inner
                .connection
                .execute(
                    "DELETE FROM transcript_metadata WHERE terminal_id = ?1",
                    [replacement.id.to_string()],
                )
                .unwrap();
            inner
                .connection
                .execute(
                    "DELETE FROM terminal_recipes WHERE terminal_id = ?1",
                    [replacement.id.to_string()],
                )
                .unwrap();
        }
        let inner = store.lock_inner().unwrap();
        let mut live = HashSet::new();
        collect_live_terminal_ids(&inner.connection, &mut live).unwrap();
        assert!(live.contains(&replacement.id.to_string()));
        drop(inner);
        // Save just the closed card: the active card has been deliberately
        // removed from this fixture's snapshot and becomes a tombstone.
        workspace.cards.remove(0);
        workspace.focused = None;
        lane.save(&store, &workspace).unwrap();
        assert_eq!(
            store.load_card(history_card, None).unwrap().unwrap().kind,
            CardKind::Terminal(replacement)
        );
        let inner = store.lock_inner().unwrap();
        assert_eq!(
            inner
                .connection
                .query_row(
                    "SELECT count(*) FROM terminal_recipes WHERE card_id = ?1",
                    [history_card.to_string()],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
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
    fn missing_terminal_payload_does_not_refuse_identity_replacement() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let boot_id = boot("boot-a");
        let store = StateStore::open(&path, boot_id.clone()).unwrap();
        let workspace = workspace_with_terminal(&boot_id);
        let card_id = workspace.cards[0].id;
        let CardKind::Terminal(original) = &workspace.cards[0].kind else {
            panic!("expected terminal card");
        };
        SaveLane::default().save(&store, &workspace).unwrap();
        // Simulate damage outside the validated writer, including the now
        // orphaned transcript row, which normal foreign-key checks forbid.
        let damaged = Connection::open(&path).unwrap();
        damaged.pragma_update(None, "foreign_keys", false).unwrap();
        damaged
            .execute(
                "DELETE FROM terminal_recipes WHERE card_id = ?1",
                [card_id.to_string()],
            )
            .unwrap();
        let mut replacement = original.clone();
        replacement.id = TerminalId::new();
        replacement.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: replacement.launch.clone(),
            last_exit: None,
        };
        let error = store
            .replace_terminal_identity(card_id, original.id, &replacement)
            .unwrap_err();
        assert!(matches!(error, StoreError::CorruptRow { .. }));
        assert!(!error.refuses_content());
    }

    /// Counts how often `body` reports a closed card's final content as
    /// refused, on this thread only.
    fn closed_card_refusals(body: impl FnOnce()) -> usize {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        use tracing::field::{Field, Visit};
        use tracing_subscriber::{Layer, layer::SubscriberExt};

        struct IsRefusal(bool);
        impl Visit for IsRefusal {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                self.0 |= field.name() == "message"
                    && format!("{value:?}").contains("refused a closed card's final content");
            }
        }
        struct Count(Arc<AtomicUsize>);
        impl<S: tracing::Subscriber> Layer<S> for Count {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut refusal = IsRefusal(false);
                event.record(&mut refusal);
                if refusal.0 {
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let seen = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry().with(Count(seen.clone()));
        tracing::subscriber::with_default(subscriber, body);
        seen.load(Ordering::Relaxed)
    }

    fn closed_by_user() -> Existence {
        Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: Some("closed by user".into()),
            },
        }
    }

    /// A window offers the cards it closed in every snapshot it saves. One
    /// whose final content storage refused is settled by its tombstone, so
    /// offering it again must neither repeat the write nor the report.
    #[test]
    fn refused_closed_card_content_is_offered_to_storage_once() {
        let directory = tempdir().unwrap();
        let (mut workspace, session) = agent_workspace(crate::agent::Provider::Claude);
        let card = workspace.cards[0].id;
        workspace.cards[0].kind = CardKind::Chat(session.clone());
        let store = StateStore::open(directory.path().join("state.sqlite3"), boot("boot")).unwrap();
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();

        // The card closes holding a conversation storage will not let it
        // become, which is the refusal closing is the remedy for.
        let mut rebound = session.clone();
        rebound.launch.provider = crate::agent::Provider::Codex;
        workspace.cards[0].kind = CardKind::Chat(rebound);
        workspace.cards[0].existence = closed_by_user();
        workspace.focused = None;
        let refusals = closed_card_refusals(|| {
            lane.save(&store, &workspace).unwrap();
        });
        assert_eq!(refusals, 1, "the refusal is reported when it happens");
        let closed = stored_card_row(&store, card);
        let stored = store.load_card(card, None).unwrap().unwrap();
        assert_eq!(stored.existence, closed_by_user());
        assert_eq!(stored.kind, CardKind::Chat(session.clone()));

        // Every later save of the window carries the same closed card.
        let refusals = closed_card_refusals(|| {
            for _ in 0..3 {
                let commit = lane.save(&store, &workspace).unwrap();
                assert_eq!(commit.card_generations[&card], closed.0.cast_unsigned());
            }
        });
        assert_eq!(refusals, 0, "a settled refusal is not attempted again");
        assert_eq!(stored_card_row(&store, card), closed);

        // Different content is a different offer, and is tried on its merits.
        workspace.cards[0].title = "renamed after closing".into();
        let refusals = closed_card_refusals(|| {
            lane.save(&store, &workspace).unwrap();
            lane.save(&store, &workspace).unwrap();
        });
        assert_eq!(
            refusals, 1,
            "a new offer is refused once more, then settled"
        );

        // Content storage accepts is written, and forgets the refusal.
        workspace.cards[0].kind = CardKind::Chat(session.clone());
        let refusals = closed_card_refusals(|| {
            lane.save(&store, &workspace).unwrap();
        });
        assert_eq!(refusals, 0);
        let stored = store.load_card(card, None).unwrap().unwrap();
        assert_eq!(stored.title, "renamed after closing");
        assert!(stored_card_row(&store, card).2.is_some());
    }

    /// A closed card storage never held has no row to tombstone, so the
    /// refusal of its content is the whole outcome and is settled the same way.
    #[test]
    fn refused_closed_card_storage_never_held_is_offered_once() {
        let directory = tempdir().unwrap();
        let (mut workspace, mut session) = agent_workspace(crate::agent::Provider::Claude);
        let card = workspace.cards[0].id;
        // A source terminal that is not the conversation's own is refused
        // whatever storage holds.
        session.id = TerminalId::new();
        workspace.cards[0].kind = CardKind::Chat(session);
        workspace.cards[0].existence = closed_by_user();
        workspace.focused = None;
        let store = StateStore::open(directory.path().join("state.sqlite3"), boot("boot")).unwrap();
        let mut lane = SaveLane::default();

        let refusals = closed_card_refusals(|| {
            for _ in 0..3 {
                let commit = lane.save(&store, &workspace).unwrap();
                assert_eq!(commit.card_generations[&card], 0);
            }
        });
        assert_eq!(refusals, 1);
        assert!(store.load_card(card, None).unwrap().is_none());
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
                selected_file: None,
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
    #[test]
    fn retained_live_reset_keeps_same_host_snapshot_contract_and_rejects_downgrade() {
        let directory = tempdir().unwrap();
        let store =
            StateStore::open(directory.path().join("reset.sqlite3"), boot("chat-boot")).unwrap();
        let (mut workspace, mut source) = agent_workspace(crate::agent::Provider::Claude);
        source.proof = source.terminal.as_ref().unwrap().continuity.live_proof();
        source.terminal = None;
        source.native_continuity =
            crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        let card = workspace.cards[0].id;
        workspace.cards[0].kind = CardKind::Chat(source.clone());
        let mut lane = SaveLane::default();
        lane.save(&store, &workspace).unwrap();
        let mut reset = source.clone();
        reset.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        workspace.cards[0].kind = CardKind::Chat(reset.clone());
        lane.save(&store, &workspace).unwrap();
        let mut downgraded = reset.clone();
        downgraded.native_continuity = crate::chat::NativeContinuity::ReconnectOnly;
        workspace.cards[0].kind = CardKind::Chat(downgraded);
        assert!(lane.save(&store, &workspace).is_err());
        let mut unproved = source.clone();
        unproved.proof.as_mut().unwrap().host_incarnation = "another-daemon".into();
        assert!(store.rebind_chat_upgrade(card, &reset, &unproved).is_err());
        assert_eq!(
            store.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(reset)
        );
    }
}
