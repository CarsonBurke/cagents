//! One update's causal authority to recover its own incomplete preparation.
//!
//! A Prepared checkpoint is never replay permission. This receipt identifies
//! bytes written by an acknowledged prepare while the same update and window
//! fences remain held. Its only startup action is adopting existing owners.
#[cfg(any(feature = "host", test))]
mod retention;
pub mod verified;
#[cfg(feature = "host")]
pub use retention::collect_superseded;

use crate::{
    chat::Session,
    daemon::lease_handoff::LeaseHandoff,
    instance::UiUpdateGuard,
    model::{HostSessionProof, TerminalId, TerminalSession},
    update::impact::{DaemonImpact, SessionKind},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
};

pub(crate) const RECOVERY_FD_ENV: &str = "CAGENTS_UPDATE_RECOVERY_FD";
const ENV: &str = RECOVERY_FD_ENV;
const LEGACY_FD: i32 = 189;
const RECEIPT_FD: i32 = 193;
const WORKER_FD: i32 = 194;
const FENCE_FD: i32 = 195;
const DAEMON_FD: i32 = 196;
const ORIGINAL_WORKER_FD: i32 = 198;
const LIMIT: u64 = 16 * 1024 * 1024;
const SNAPSHOT_LIMIT: usize = 8 * 1024 * 1024;
#[cfg(target_os = "linux")]
const SEALS: i32 = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
static INHERITED: OnceLock<io::Result<Mutex<Option<Inherited>>>> = OnceLock::new();

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Census {
    pub terminals: Vec<TerminalSession>,
    pub chats: Vec<Session>,
    pub snapshots: Vec<super::floor::StoppedSnapshot>,
    #[serde(default)]
    pub aliases: Vec<Session>,
    #[serde(default)]
    pub presentations: Vec<super::floor::Presentation>,
    /// The conversations with work in flight when the census was taken.
    /// Absent from censuses taken before it was recorded, and empty when
    /// nothing was under way, so their bytes are unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_flight: Vec<crate::update::in_flight::InFlight>,
}

/// Transcript pages captured before preparation. [`Census::fit_pages`] fits
/// their total to the evidence; a long conversation is never refused.
#[derive(Default)]
pub struct SnapshotCapture {
    snapshots: Vec<super::floor::StoppedSnapshot>,
    pages: Vec<super::floor::Page>,
}

/// Pages beyond this many bytes are shortened while capture continues, so a
/// daemon with hundreds of long chats is never held in memory at once.
const CAPTURE_COMPACTION: usize = 2 * SNAPSHOT_LIMIT;

impl SnapshotCapture {
    pub fn push(
        &mut self,
        source: Session,
        mut snapshot: crate::chat::Snapshot,
    ) -> Result<(), String> {
        if [&source.launch.session_id, &snapshot.session_id]
            .into_iter()
            .flatten()
            .any(|identity| identity.is_empty() || identity.len() > 1024)
        {
            return Err(
                "The captured conversation identity is invalid; the source was not prepared".into(),
            );
        }
        if let (Some(known), Some(captured)) = (&source.launch.session_id, &snapshot.session_id)
            && known != captured
        {
            return Err("The captured conversation changed from its reviewed identity; the source was not prepared".into());
        }
        super::floor::retain_reader_fields(&mut snapshot);
        let continuation = if source.launch.session_id.is_some() {
            "Resume explicitly to inspect whether the recorded conversation can continue."
        } else {
            "No conversation identity was recorded; start a new conversation explicitly to continue."
        };
        let saved = super::floor::StoppedSnapshot {
            source,
            snapshot,
            limitation: Some(format!(
                "Paused after an incomplete update. This page was captured before preparation; later output may remain in the provider's conversation storage. The original provider could not be reattached, and no replacement was started. {continuation}"
            )),
        };
        self.pages.push(super::floor::Page::of(&saved.snapshot)?);
        self.snapshots.push(saved);
        if self
            .pages
            .iter()
            .map(super::floor::Page::bytes)
            .sum::<usize>()
            > CAPTURE_COMPACTION
        {
            // No fitting grants pages more than the whole transcript budget
            // between them, so their fair shares of it bound what each keeps.
            super::floor::keep_fair_shares(&mut self.snapshots, &mut self.pages, SNAPSHOT_LIMIT);
        }
        Ok(())
    }
    pub fn finish(self) -> Vec<super::floor::StoppedSnapshot> {
        self.snapshots
    }
}

/// Bytes kept free beyond the evidence measured before preparation.
const FIT_MARGIN: usize = 64 * 1024;

fn encoded_size(value: &impl Serialize, limit: usize) -> Result<usize, String> {
    struct Count {
        used: usize,
        limit: usize,
    }
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.used = self
                .used
                .checked_add(bytes.len())
                .filter(|size| *size <= self.limit)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "Recovery evidence exceeds its {}-byte limit; the source was not prepared",
                        self.limit
                    ))
                })?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count { used: 0, limit };
    serde_json::to_writer(&mut count, value).map_err(|e| e.to_string())?;
    Ok(count.used)
}

// ChatList retains the terminal descriptor issued to a paired native view;
// List projects attachment state for its own connection and may enrich a
// resume hint. Normalize those observations only while both descriptors still
// prove the same complete live daemon owner. Preserve every identity field.
fn same_captured_terminal(
    left: &TerminalSession,
    right: &TerminalSession,
    source: &HostSessionProof,
) -> bool {
    if left == right {
        return true;
    }
    if !source.is_complete()
        || left.continuity.live_proof().as_ref() != Some(source)
        || right.continuity.live_proof().as_ref() != Some(source)
    {
        return false;
    }
    let mut projected = left.clone();
    projected.continuity = right.continuity.clone();
    projected.resume_hint = right.resume_hint.clone();
    projected == *right
}

impl Census {
    pub fn terminal_ids(&self) -> BTreeSet<TerminalId> {
        self.terminals
            .iter()
            .map(|source| source.id)
            .chain(
                self.chats
                    .iter()
                    .filter_map(|source| source.terminal.as_ref().map(|terminal| terminal.id)),
            )
            .collect()
    }
    /// Fit the captured transcript pages into every bound that carries them
    /// unshortened: this census, its stopped-pane record and the preparation
    /// receipt. Each page keeps its newest entries within a fair share of
    /// what the identity and ownership evidence leaves, and says when it lost
    /// older ones. Only that evidence, which is never shortened, can refuse.
    pub(crate) fn fit_pages(
        &mut self,
        source: &HostSessionProof,
        reviewed: &DaemonImpact,
        windows: &[String],
    ) -> Result<(), String> {
        let bounds = self.identity_bounds(source, reviewed, windows)?;
        if let Some(refusal) = self.refusal(&bounds, source, reviewed, windows)? {
            return Err(refusal);
        }
        let budget = bounds
            .iter()
            .map(|bound| bound.room())
            .min()
            .unwrap_or_default();
        let mut pages = self
            .snapshots
            .iter()
            .map(|saved| super::floor::Page::of(&saved.snapshot))
            .collect::<Result<Vec<_>, _>>()?;
        super::floor::keep_fair_shares(&mut self.snapshots, &mut pages, budget);
        Ok(())
    }

    /// What each bound needs with every page empty. The stopped-pane record
    /// is measured beside evidence already retained for this source, as
    /// recovery saves it; saving shortens pages around it, so it limits the
    /// identity evidence here, never the room left for pages.
    fn identity_bounds(
        &self,
        source: &HostSessionProof,
        reviewed: &DaemonImpact,
        windows: &[String],
    ) -> Result<[Bound; 4], String> {
        let mut minimal = self.clone();
        minimal
            .snapshots
            .iter_mut()
            .for_each(super::floor::empty_page);
        let receipt = Authority::provisional(source, reviewed, windows, &minimal);
        let record = minimal.record(source);
        Ok([
            Bound::pages(encoded_size(&minimal, usize::MAX)?, SNAPSHOT_LIMIT),
            Bound::pages(encoded_size(&record, usize::MAX)?, SNAPSHOT_LIMIT),
            Bound::pages(preparation_size(&receipt)?, PREPARATION_LIMIT),
            Bound {
                needed: record.retained_identity_size()?,
                limit: super::floor::LIMIT as usize,
                carries_pages: false,
            },
        ])
    }

    /// When identity evidence alone exceeds a bound, say how many chats to
    /// close. Chats dominate that evidence, so estimate each one's share
    /// from the same bound measured without any chat. Several bounds can
    /// fail at once, each with its own share per chat: the advice follows
    /// the one that needs the most chats closed, so following it is enough.
    fn refusal(
        &self,
        bounds: &[Bound; 4],
        source: &HostSessionProof,
        reviewed: &DaemonImpact,
        windows: &[String],
    ) -> Result<Option<String>, String> {
        if bounds.iter().all(Bound::fits) {
            return Ok(None);
        }
        let mut without = self.clone();
        without.chats.clear();
        without.snapshots.clear();
        without.aliases.clear();
        without.presentations.clear();
        let mut reviewed = reviewed.clone();
        reviewed
            .sessions
            .retain(|session| session.kind != SessionKind::NativeChat);
        let bases = without.identity_bounds(source, &reviewed, windows)?;
        let chats = self.chats.len();
        // Closing chats cannot resolve a bound (None) ranks above any count.
        let (bound, close) = bounds
            .iter()
            .zip(&bases)
            .filter(|(bound, _)| !bound.fits())
            .map(|(bound, base)| (bound, bound.chats_to_close(chats, base.needed)))
            .max_by_key(|(_, close)| close.map_or(usize::MAX, |close| close))
            .expect("a bound that does not fit");
        Ok(Some(bound.refusal(chats, close)))
    }

    fn record(&self, source: &HostSessionProof) -> super::floor::Record {
        let mut record = super::floor::Record::new(
            source.clone(),
            self.chats.clone(),
            self.terminal_ids().into_iter().collect(),
        );
        record.snapshots = self.snapshots.clone();
        record.aliases = self.aliases.clone();
        record.presentations = self.presentations.clone();
        record
    }

    /// Recovery later saves this capture's pages beside the stopped-pane
    /// evidence already retained for its source, shortening pages to fit.
    /// Its identity evidence must fit there unshortened.
    fn validate_capture(&self, source: &HostSessionProof) -> Result<(), String> {
        self.validate_evidence(source)?;
        if self.record(source).retained_identity_size()? > super::floor::LIMIT as usize {
            return Err("Stopped update pane identities exceed their durable limit; the source was not prepared".into());
        }
        Ok(())
    }

    fn validate_evidence(&self, source: &HostSessionProof) -> Result<(), String> {
        if self.snapshots.len() != self.chats.len()
            || self.chats.iter().any(|chat| {
                self.snapshots
                    .iter()
                    .filter(|saved| saved.source == *chat)
                    .count()
                    != 1
            })
        {
            return Err(
                "Recovery preparation requires one exact captured page per source conversation"
                    .into(),
            );
        }
        let terminals: BTreeSet<_> = self.terminals.iter().map(|item| item.id).collect();
        let chats: BTreeSet<_> = self.chats.iter().map(|item| item.id).collect();
        if !source.is_complete()
            || terminals.len() != self.terminals.len()
            || chats.len() != self.chats.len()
            || self.terminals.iter().any(|item| {
                item.continuity
                    .live_proof()
                    .as_ref()
                    .is_some_and(|proof| proof != source)
            })
            || self.chats.iter().any(|item| {
                item.proof.as_ref() != Some(source)
                    || item.terminal.as_ref().is_some_and(|terminal| {
                        terminal.id != item.id
                            || terminal
                                .continuity
                                .live_proof()
                                .as_ref()
                                .is_some_and(|proof| proof != source)
                            || self.terminals.iter().any(|known| {
                                known.id == terminal.id
                                    && !same_captured_terminal(known, terminal, source)
                            })
                    })
            })
            || self.terminals.iter().any(|item| {
                chats.contains(&item.id)
                    && !self.chats.iter().any(|chat| {
                        chat.terminal
                            .as_ref()
                            .is_some_and(|paired| same_captured_terminal(paired, item, source))
                    })
            })
        {
            return Err(
                "Recovery capture has duplicate, ambiguous, or different source owners".into(),
            );
        }
        self.validate_presentations()?;
        let mut working = BTreeSet::new();
        if self
            .in_flight
            .iter()
            .any(|work| work.is_empty() || !chats.contains(&work.id) || !working.insert(work.id))
        {
            return Err("Recovery capture records work for a conversation it does not hold".into());
        }
        encoded_size(self, SNAPSHOT_LIMIT)?;
        encoded_size(&self.record(source), SNAPSHOT_LIMIT)?;
        Ok(())
    }

    fn validate_presentations(&self) -> Result<(), String> {
        if self.aliases.iter().any(|alias| {
            !self
                .chats
                .iter()
                .any(|source| super::floor::same_allocation(source, alias))
        }) {
            return Err("Recovery pane alias changed its captured allocation".into());
        }
        let mut owners = BTreeSet::new();
        for presentation in &self.presentations {
            if !self.chats.contains(&presentation.session)
                || presentation.restored
                || presentation.pane.id.as_uuid() != presentation.agent.as_uuid()
                || !owners.insert(presentation.agent)
            {
                return Err("Recovery presentation changed its captured owner".into());
            }
            presentation.pane.validate()?;
            let matches = match &presentation.pane.kind {
                crate::model::CardKind::Chat(saved) => {
                    super::floor::same_allocation(&presentation.session, saved)
                }
                crate::model::CardKind::Terminal(saved) => {
                    presentation.session.terminal.as_deref() == Some(saved)
                }
                _ => false,
            };
            if !matches {
                return Err("Recovery presentation changed its captured pane source".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Authority {
    version: u16,
    #[serde(default)]
    origin: Origin,
    job: String,
    request_identity: String,
    job_request_hash: String,
    #[serde(default)]
    reviewed: Option<DaemonImpact>,
    source: HostSessionProof,
    census: Census,
    #[serde(default)]
    windows: Vec<String>,
    expected: Vec<(SessionKind, TerminalId)>,
    checkpoint_path: PathBuf,
    checkpoint_hash: String,
    worker_path: PathBuf,
    fence_path: PathBuf,
    legacy_path: PathBuf,
    daemon_path: PathBuf,
    target: PathBuf,
    target_hash: String,
}

/// Disk evidence alone never grants the automatic prepare/ACK authority.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Origin {
    #[default]
    AcknowledgedPrepare,
    ExplicitRecovery {
        checkpoint: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapturedCensus {
    #[serde(default)]
    producer: Option<CaptureProducer>,
    source: HostSessionProof,
    #[serde(default)]
    checkpoint_hash: Option<String>,
    census: Census,
    windows: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case", deny_unknown_fields)]
enum CaptureProducer {
    Proposed {
        job: String,
        request_hash: String,
    },
    Acknowledged {
        job: String,
        request_hash: String,
    },
    ExplicitCensus {
        transaction: String,
        census_hash: String,
    },
}

impl CaptureProducer {
    fn valid(&self) -> bool {
        let (job, hash) = match self {
            Self::Proposed { .. } => return false,
            Self::Acknowledged { job, request_hash } => (job, request_hash),
            Self::ExplicitCensus {
                transaction,
                census_hash,
            } => (transaction, census_hash),
        };
        uuid::Uuid::parse_str(job).is_ok()
            && hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }
}

/// Read-only retained evidence, never permission to replay or start a provider.
#[derive(Clone, Debug)]
pub(crate) struct PendingRecovery {
    pub source: HostSessionProof,
    pub checkpoint_hash: String,
    pub census: Census,
    pub windows: Vec<String>,
    pub original_census_complete: bool,
    producer: Option<CaptureProducer>,
    checkpoint_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManualRequest {
    version: u16,
    job: String,
    checkpoint: String,
    source: HostSessionProof,
    target: PathBuf,
    target_hash: String,
    census_hash: String,
    windows: Vec<String>,
}

/// A restored terminal's source/relaunch lineage is bounded by the complete
/// checkpoint (8 MiB). Budget its published descriptor, paired sessions and
/// current chats as well as the receipt itself before asking for shutdown.
/// Incidental output/title/activity metadata is never retained as proof.
const FUTURE_METADATA: usize = 1024 * 1024;
const PREPARATION_LIMIT: usize = LIMIT as usize - SNAPSHOT_LIMIT - FUTURE_METADATA;

fn preflight_preparation_size(authority: &Authority) -> Result<(), String> {
    let size = preparation_size(authority)?;
    if size > PREPARATION_LIMIT {
        return Err(format!(
            "Recovery evidence exceeds its {PREPARATION_LIMIT}-byte limit; the source was not prepared"
        ));
    }
    Ok(())
}

/// Count the complete receipt plus more owner bookkeeping than any valid
/// outcome can contain. Leave room for the two future Linux paths (including
/// JSON escaping), hashes, successor identifiers and record field names.
fn preparation_size(authority: &Authority) -> Result<usize, String> {
    let terminals: Vec<_> = authority.census.terminal_ids().into_iter().collect();
    let chats: Vec<_> = authority.census.chats.iter().map(|chat| chat.id).collect();
    let sources: Vec<_> = authority
        .census
        .terminals
        .iter()
        .chain(
            authority
                .census
                .chats
                .iter()
                .filter_map(|chat| chat.terminal.as_deref()),
        )
        .collect();
    encoded_size(
        &(
            authority,
            &terminals,
            &terminals,
            &chats,
            &chats,
            &sources,
            &sources,
            &authority.census.chats,
            &authority.source,
            &authority.census.chats,
            &authority.census.chats,
        ),
        usize::MAX,
    )
}

/// The largest path a receipt can name: Linux `PATH_MAX`.
const PATH_RESERVE: usize = 4096;

impl Authority {
    /// The receipt a preparation of `census` would write, with every path at
    /// its largest. Only its encoded size is used, to fit pages beforehand.
    fn provisional(
        source: &HostSessionProof,
        reviewed: &DaemonImpact,
        windows: &[String],
        census: &Census,
    ) -> Self {
        let path = || PathBuf::from("x".repeat(PATH_RESERVE));
        let hash = || "0".repeat(64);
        Self {
            version: 1,
            origin: Origin::AcknowledgedPrepare,
            job: uuid::Uuid::nil().to_string(),
            request_identity: hash(),
            job_request_hash: hash(),
            reviewed: Some(reviewed.clone()),
            source: source.clone(),
            census: census.clone(),
            windows: windows.to_vec(),
            expected: checkpointed_owners(reviewed),
            checkpoint_path: path(),
            checkpoint_hash: hash(),
            worker_path: path(),
            fence_path: path(),
            legacy_path: path(),
            daemon_path: path(),
            target: path(),
            target_hash: hash(),
        }
    }
}

/// One fixed limit on evidence that carries the census: what its identity
/// and ownership part needs with every transcript page empty.
#[derive(Clone, Copy, Debug)]
struct Bound {
    needed: usize,
    limit: usize,
    /// Whether transcript pages share this limit unshortened.
    carries_pages: bool,
}

impl Bound {
    fn pages(needed: usize, limit: usize) -> Self {
        Self {
            needed,
            limit,
            carries_pages: true,
        }
    }

    /// Later JSON differences, such as the small fields a receipt gains
    /// after acknowledgement, stay within this margin.
    fn reserved(&self) -> usize {
        self.needed.saturating_add(FIT_MARGIN)
    }

    fn fits(&self) -> bool {
        self.reserved() <= self.limit
    }

    /// Bytes transcript pages may use within this bound.
    fn room(&self) -> usize {
        if self.carries_pages {
            self.limit.saturating_sub(self.reserved())
        } else {
            usize::MAX
        }
    }

    /// How many of `chats` to close for this bound to fit, estimated from
    /// their average share; `base` is what it needs without any chat. None
    /// when closing chats alone cannot make it fit.
    fn chats_to_close(&self, chats: usize, base: usize) -> Option<usize> {
        let excess = self.reserved().saturating_sub(self.limit);
        let per_chat = self.needed.saturating_sub(base) / chats.max(1);
        (per_chat > 0 && base.saturating_add(FIT_MARGIN) <= self.limit)
            .then(|| excess.div_ceil(per_chat).min(chats))
    }

    fn refusal(&self, chats: usize, close: Option<usize>) -> String {
        let advice = match close {
            Some(close) => format!("Close at least {close} of them you no longer need"),
            None => "Close terminals and chats you no longer need".to_owned(),
        };
        // Approved interruptions may already have ended turns, so say only
        // what holds either way.
        format!(
            "This daemon has {chats} open chats, more than an update can record safely: even with \
             their saved transcript pages left empty, their recovery evidence needs {} KiB of its \
             {} KiB limit. {advice}, then run the update again. Nothing was prepared and no chat \
             was closed; the running daemon keeps serving them.",
            self.reserved().div_ceil(1024),
            self.limit / 1024,
        )
    }
}

/// Retain only the immutable allocation/lineage and serving ownership fields
/// used by the proof reader. Output and attachment metadata can keep changing.
#[cfg(feature = "host")]
pub fn proof_terminal_descriptor(
    mut descriptor: crate::terminal_ipc::TerminalDescriptor,
) -> crate::terminal_ipc::TerminalDescriptor {
    descriptor.generation = 0;
    descriptor.event_sequence = 0;
    descriptor.oldest_event_sequence = 0;
    descriptor.attached = false;
    descriptor.transcript = None;
    descriptor.activity = None;
    descriptor.agent_activity = None;
    descriptor.title = None;
    descriptor
}

pub(crate) struct Preparation {
    authority: Authority,
    worker: File,
    fence: File,
    legacy: File,
}

pub(crate) struct CapturedPreparation(Preparation);

pub(crate) struct Receipt {
    receipt: File,
    worker: File,
    fence: File,
    legacy: File,
    daemon: Option<File>,
    authority: Authority,
}

#[derive(Debug)]
pub(crate) struct RecoveryOutcome {
    pub paused_terminals: Vec<TerminalId>,
    pub paused_chats: Vec<TerminalId>,
}

#[derive(Debug)]
struct Inherited {
    receipt: File,
    worker: File,
    fence: File,
    legacy: File,
    daemon: File,
}

#[cfg(feature = "host")]
#[derive(Debug)]
struct Continuity {
    inherited: Inherited,
    paths: [PathBuf; 4],
}

#[derive(Deserialize)]
struct CheckpointIndex {
    version: u16,
    proof: HostSessionProof,
    phase: String,
    terminals: Vec<TerminalSource>,
    #[serde(default)]
    carried: Vec<TerminalSource>,
    chats: Vec<ChatSource>,
}

#[derive(Deserialize)]
struct TerminalSource {
    source: TerminalSession,
}
#[derive(Deserialize)]
struct ChatSource {
    source: Session,
    launch: crate::chat::Launch,
}

fn capture_path(checkpoint: &Path, source: &HostSessionProof) -> Result<PathBuf, String> {
    let hash = digest(&serde_json::to_vec(source).map_err(|e| e.to_string())?);
    Ok(checkpoint.with_file_name(format!("daemon-upgrade.capture.{hash}.json")))
}

fn bound_capture_path(checkpoint: &Path, hash: &str) -> PathBuf {
    checkpoint.with_file_name(format!("daemon-upgrade.capture-checkpoint.{hash}.json"))
}

fn persist_bound_capture(checkpoint: &Path, capture: &CapturedCensus) -> Result<(), String> {
    let hash = capture
        .checkpoint_hash
        .as_deref()
        .ok_or("Recovery capture has no exact checkpoint binding")?;
    if !capture
        .producer
        .as_ref()
        .is_some_and(CaptureProducer::valid)
    {
        return Err("Recovery capture has no exact original producer".into());
    }
    let path = bound_capture_path(checkpoint, hash);
    let bytes = serde_json::to_vec(capture).map_err(|e| e.to_string())?;
    if let Some(previous) = read_checkpoint(&path)? {
        if previous == bytes {
            return Ok(());
        }
        return Err("Different full original census evidence is already bound to this checkpoint; nothing was overwritten".into());
    }
    if bytes.len() as u64 > LIMIT {
        return Err("Bound recovery census exceeds its private size limit".into());
    }
    let parent = path.parent().ok_or("Recovery capture has no parent")?;
    let temporary = parent.join(format!(".bound-recovery-capture-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        // Publish complete bytes atomically without ever replacing a prior
        // binding. A failed write cannot strand a partial bound-census file.
        rename_noreplace(&temporary, &path)
            .map_err(|error| format!("Could not publish immutable recovery census: {error}"))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn persist_capture(checkpoint: &Path, capture: &CapturedCensus) -> Result<(), String> {
    let path = capture_path(checkpoint, &capture.source)?;
    let bytes = serde_json::to_vec(capture).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > LIMIT {
        return Err("Original recovery census exceeds its private size limit".into());
    }
    let parent = path.parent().ok_or("Recovery capture has no directory")?;
    let temporary = parent.join(format!(".recovery-capture-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        // Called only with no checkpoint and the continuous worker/UI fences.
        // Replacing an abandoned *capture* never replaces checkpoint authority.
        fs::rename(&temporary, &path).map_err(|e| e.to_string())?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// Durable full original scope before either legacy floor closure or prepare.
/// This grants no automatic recovery authority and never changes a checkpoint.
pub(crate) fn capture_before_floor(
    source: &HostSessionProof,
    census: &Census,
    guard: &UiUpdateGuard,
    state: &Path,
    job: &str,
) -> Result<(), String> {
    let store = super::handoff::Store::discover()?;
    require_absent_checkpoint(&store.path)?;
    census.validate_capture(source)?;
    persist_capture(
        &store.path,
        &CapturedCensus {
            producer: Some(CaptureProducer::Proposed {
                job: job.into(),
                request_hash: digest(
                    &read_checkpoint(&state.join(format!("{job}.request")))?
                        .ok_or("Original update request disappeared before census capture")?,
                ),
            }),
            source: source.clone(),
            checkpoint_hash: None,
            census: census.clone(),
            windows: guard.prepared_window_slots().to_vec(),
        },
    )
}

impl PendingRecovery {
    /// Read-only job routing, never acknowledgement or automatic recovery
    /// authority. Callers still bind the job's source and immutable request.
    pub(crate) fn matches_retained_job(
        &self,
        job: &str,
        request_bytes: &[u8],
        census_bytes: &[u8],
    ) -> bool {
        match self.producer.as_ref() {
            Some(CaptureProducer::Proposed {
                job: captured,
                request_hash,
            })
            | Some(CaptureProducer::Acknowledged {
                job: captured,
                request_hash,
            }) => captured == job && *request_hash == digest(request_bytes),
            Some(CaptureProducer::ExplicitCensus {
                transaction,
                census_hash,
            }) => transaction == job && *census_hash == digest(census_bytes),
            None => !self.original_census_complete,
        }
    }
    pub(crate) fn inspect() -> Result<Option<Self>, String> {
        let store = super::handoff::Store::discover()?;
        Self::inspect_at(&store.path)
    }

    fn inspect_at(path: &Path) -> Result<Option<Self>, String> {
        let Some(bytes) = read_checkpoint(path)? else {
            return Ok(None);
        };
        let checkpoint: CheckpointIndex =
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if checkpoint.phase == "Ready" {
            return Ok(None);
        }
        if !(1..=5).contains(&checkpoint.version)
            || checkpoint.phase != "Prepared"
            || !checkpoint.proof.is_complete()
            || checkpoint.terminals.len() + checkpoint.carried.len() > super::handoff::MAX_TERMINALS
            || checkpoint.chats.len() > super::handoff::MAX_CHATS
        {
            return Err(
                "Retained checkpoint is not a bounded Prepared source; it was preserved".into(),
            );
        }
        let captured = read_checkpoint(&bound_capture_path(path, &digest(&bytes)))?
            .map(|bytes| {
                serde_json::from_slice::<CapturedCensus>(&bytes).map_err(|e| e.to_string())
            })
            .transpose()?;
        let complete = captured.is_some();
        let producer = captured
            .as_ref()
            .and_then(|capture| capture.producer.clone());
        let (census, windows) = match captured {
            Some(capture)
                if capture.source == checkpoint.proof
                    && capture
                        .checkpoint_hash
                        .as_ref()
                        .is_some_and(|hash| *hash == digest(&bytes))
                    && capture
                        .producer
                        .as_ref()
                        .is_some_and(CaptureProducer::valid) =>
            {
                (capture.census, capture.windows)
            }
            Some(_) => return Err("Retained census belongs to another preparation source".into()),
            None => {
                let census = Census {
                    terminals: checkpoint
                        .terminals
                        .iter()
                        .chain(&checkpoint.carried)
                        .map(|terminal| terminal.source.clone())
                        .collect(),
                    chats: checkpoint
                        .chats
                        .iter()
                        .map(|chat| {
                            let mut source = chat.source.clone();
                            source.launch.session_id.clone_from(&chat.launch.session_id);
                            source
                        })
                        .collect(),
                    ..Default::default()
                };
                (census, Vec::new())
            }
        };
        let pending = Self {
            source: checkpoint.proof,
            checkpoint_hash: digest(&bytes),
            census,
            windows,
            original_census_complete: complete,
            producer,
            checkpoint_path: path.to_owned(),
        };
        pending.validate(&bytes)?;
        Ok(Some(pending))
    }

    fn validate(&self, bytes: &[u8]) -> Result<(), String> {
        super::handoff::validate_checkpoint_bytes(bytes)?;
        let terminals: BTreeSet<_> = self.census.terminals.iter().map(|item| item.id).collect();
        let chats: BTreeSet<_> = self.census.chats.iter().map(|item| item.id).collect();
        if terminals.len() != self.census.terminals.len()
            || chats.len() != self.census.chats.len()
            || terminals.len() > super::handoff::MAX_TERMINALS
            || chats.len() > super::handoff::MAX_CHATS
            || self
                .census
                .terminals
                .iter()
                .any(|item| item.continuity.live_proof().as_ref() != Some(&self.source))
            || self.census.chats.iter().any(|item| {
                item.proof.as_ref() != Some(&self.source)
                    || item.terminal.as_ref().is_some_and(|paired| {
                        paired.continuity.live_proof().as_ref() != Some(&self.source)
                            || self
                                .census
                                .terminals
                                .iter()
                                .any(|known| known.id == paired.id && !same_terminal(known, paired))
                    })
            })
            || self.windows.len() > 1024
            || self
                .windows
                .iter()
                .any(|slot| crate::instance::validate_window_slot(slot).is_err())
            || self.windows.iter().collect::<BTreeSet<_>>().len() != self.windows.len()
        {
            return Err(
                "Retained recovery census has duplicate, foreign or unbounded original owners"
                    .into(),
            );
        }
        if validate_checkpoint(&self.authority(), bytes)? != "Prepared"
            || digest(bytes) != self.checkpoint_hash
        {
            return Err(
                "The explicitly selected Prepared checkpoint changed; nothing was started".into(),
            );
        }
        Ok(())
    }

    fn authority(&self) -> Authority {
        Authority {
            version: 1,
            origin: Origin::ExplicitRecovery {
                checkpoint: self.checkpoint_hash.clone(),
            },
            job: String::new(),
            request_identity: String::new(),
            job_request_hash: String::new(),
            reviewed: None,
            source: self.source.clone(),
            census: self.census.clone(),
            windows: self.windows.clone(),
            expected: Vec::new(),
            checkpoint_path: self.checkpoint_path.clone(),
            checkpoint_hash: self.checkpoint_hash.clone(),
            worker_path: PathBuf::new(),
            fence_path: PathBuf::new(),
            legacy_path: PathBuf::new(),
            daemon_path: PathBuf::new(),
            target: PathBuf::new(),
            target_hash: String::new(),
        }
    }

    pub(crate) fn command(&self, candidate_cli: &Path, census: Option<&Path>) -> String {
        fn quoted(path: &Path) -> String {
            format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
        }
        let mut command = format!(
            "{} --no-start daemon recover-update --checkpoint {} --yes",
            quoted(candidate_cli),
            self.checkpoint_hash
        );
        if let Some(census) = census {
            command.push_str(&format!(" --census {}", quoted(census)));
        }
        command
    }

    /// A standalone installer has retained its complete pre-shutdown census.
    /// It is read-only scope evidence; the explicit hash+yes still grants the
    /// manual authority, and no disk artifact can mint automatic ACK authority.
    pub(crate) fn with_original_census(mut self, path: &Path) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Original {
            version: u8,
            transaction: String,
            source: Option<HostSessionProof>,
            terminals: Vec<TerminalSession>,
            chats: Vec<Session>,
            #[serde(default)]
            checkpoint_before: Option<String>,
        }
        let bytes = read_checkpoint(path)?.ok_or("The original installer census is unavailable")?;
        let original: Original = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if original.version != 1
            || uuid::Uuid::parse_str(&original.transaction).is_err()
            || original.source.as_ref() != Some(&self.source)
            || original.checkpoint_before.as_ref() == Some(&self.checkpoint_hash)
        {
            return Err(
                "The retained installer census does not identify this source preparation".into(),
            );
        }
        if self.original_census_complete {
            if self.census.terminals != original.terminals || self.census.chats != original.chats {
                return Err(
                    "A different complete original census is already bound to this checkpoint"
                        .into(),
                );
            }
            return Ok(self);
        }
        if let Some(bytes) = read_checkpoint(&capture_path(&self.checkpoint_path, &self.source)?)? {
            let capture: CapturedCensus =
                serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if let Some(CaptureProducer::Proposed { job, request_hash }) = &capture.producer
                && *job == original.transaction
                && capture.source == self.source
                && capture.census.terminals == original.terminals
                && capture.census.chats == original.chats
                && let Some(request) = read_checkpoint(
                    &path
                        .parent()
                        .ok_or("Original census has no directory")?
                        .join(format!("{job}.request")),
                )?
                && digest(&request) == *request_hash
            {
                self.census = capture.census;
                self.windows = capture.windows;
            }
        }
        self.census.terminals = original.terminals;
        self.census.chats = original.chats;
        for source in &self.census.chats {
            if !self
                .census
                .snapshots
                .iter()
                .any(|saved| saved.source == *source)
            {
                self.census.snapshots.push(super::floor::StoppedSnapshot {
                    source: source.clone(), snapshot: crate::chat::Snapshot::default(),
                    limitation: Some("Paused after an incomplete update. No transcript page was captured for this exact update request. The conversation identity remains available for explicit continuation; no provider was started.".into()),
                });
            }
        }
        self.producer = Some(CaptureProducer::ExplicitCensus {
            transaction: original.transaction,
            census_hash: digest(&bytes),
        });
        self.original_census_complete = true;
        let bytes =
            read_checkpoint(&self.checkpoint_path)?.ok_or("Recovery checkpoint disappeared")?;
        self.validate(&bytes)?;
        Ok(self)
    }

    pub(crate) fn retain_installer_census(path: &Path) -> Result<Option<Self>, String> {
        let Some(pending) = Self::inspect()? else {
            return Ok(None);
        };
        let pending = pending.with_original_census(path)?;
        persist_bound_capture(
            &pending.checkpoint_path,
            &CapturedCensus {
                producer: pending.producer.clone(),
                source: pending.source.clone(),
                checkpoint_hash: Some(pending.checkpoint_hash.clone()),
                census: pending.census.clone(),
                windows: pending.windows.clone(),
            },
        )?;
        let bytes =
            read_checkpoint(&pending.checkpoint_path)?.ok_or("Recovery checkpoint disappeared")?;
        pending.validate(&bytes)?;
        Ok(Some(pending))
    }

    /// Presence is an irreversible-claim diagnostic, never proof that the
    /// claim settled or authority to retry it. Empty/partial bytes veto a
    /// second manual operation; unsafe evidence is reported as an error.
    pub(crate) fn has_recovery_claim(&self) -> Result<bool, String> {
        Ok(read_checkpoint(
            &self
                .checkpoint_path
                .with_file_name(recovery_name(&self.checkpoint_hash)),
        )?
        .is_some())
    }

    pub(crate) fn authorize_manual(
        mut self,
        selected_hash: &str,
        yes: bool,
        state: &Path,
        job: &str,
        target: &Path,
        worker: &File,
        guard: &UiUpdateGuard,
        handoff: &LeaseHandoff,
    ) -> Result<Receipt, String> {
        if !yes || selected_hash != self.checkpoint_hash {
            return Err("Recovery requires --yes and the exact retained checkpoint SHA256; nothing was started".into());
        }
        if !self.original_census_complete {
            return Err("Recovery needs the complete original census. Repeat this exact command with --census PATH naming the retained pre-update installer census; the checkpoint was preserved".into());
        }
        if uuid::Uuid::parse_str(job).is_err() {
            return Err("Invalid explicit recovery job".into());
        }
        if self.has_recovery_claim()? {
            return Err("This checkpoint already has a recovery claim. Keep its evidence and inspect the serving daemon; a partial claim cannot be retried or overwritten".into());
        }
        let bytes =
            read_checkpoint(&self.checkpoint_path)?.ok_or("Recovery checkpoint disappeared")?;
        self.validate(&bytes)?;
        // Missing historical transcript capture is represented honestly. This
        // creates a paused history descriptor, never a provider launch recipe.
        for source in &self.census.chats {
            if !self
                .census
                .snapshots
                .iter()
                .any(|saved| saved.source == *source)
            {
                self.census.snapshots.push(super::floor::StoppedSnapshot {
                    source: source.clone(), snapshot: crate::chat::Snapshot::default(),
                    limitation: Some("Paused after an incomplete update. A transcript page was not captured before shutdown. The original conversation identity is retained; no provider was started. Resume explicitly to inspect its stored history.".into()),
                });
            }
        }
        self.census.validate_capture(&self.source)?;
        persist_bound_capture(
            &self.checkpoint_path,
            &CapturedCensus {
                producer: self.producer.clone(),
                source: self.source.clone(),
                checkpoint_hash: Some(self.checkpoint_hash.clone()),
                census: self.census.clone(),
                windows: self.windows.clone(),
            },
        )?;
        let target = fs::canonicalize(target).map_err(|e| e.to_string())?;
        let layout = crate::update::locate(&target).map_err(|e| e.to_string())?;
        let cli = fs::canonicalize(std::env::current_exe().map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        if crate::update::locate(&cli)
            .map_err(|e| e.to_string())?
            .version_root
            != layout.version_root
            || target != layout.version_root.join("bin/cagentsd")
        {
            return Err(
                "Explicit recovery requires the selected candidate's matched daemon".into(),
            );
        }
        let worker = worker.try_clone().map_err(|e| e.to_string())?;
        let worker_path = state.join("lock");
        validate_lock(&worker, &worker_path).map_err(|e| e.to_string())?;
        let (fence, legacy) = guard.duplicate_for_recovery().map_err(|e| e.to_string())?;
        let fence_path = descriptor_path(&fence).map_err(|e| e.to_string())?;
        let legacy_path = descriptor_path(&legacy).map_err(|e| e.to_string())?;
        validate_lock(&fence, &fence_path).map_err(|e| e.to_string())?;
        validate_lock_mode(&legacy, &legacy_path, "READ").map_err(|e| e.to_string())?;
        let daemon = handoff
            .duplicate_for_recovery()
            .map_err(|e| e.to_string())?;
        let daemon_path = descriptor_path(&daemon).map_err(|e| e.to_string())?;
        validate_lock(&daemon, &daemon_path).map_err(|e| e.to_string())?;
        let paths = crate::daemon::RuntimePaths::discover().map_err(|e| e.to_string())?;
        if daemon_path != paths.lock
            || std::os::unix::net::UnixStream::connect(&paths.socket).is_ok()
        {
            return Err(
                "Another daemon is serving or starting; explicit recovery did not replace it"
                    .into(),
            );
        }
        let target_hash = executable_hash(&target)?;
        let request = ManualRequest {
            version: 1,
            job: job.into(),
            checkpoint: self.checkpoint_hash.clone(),
            source: self.source.clone(),
            target: target.clone(),
            target_hash: target_hash.clone(),
            census_hash: digest(&serde_json::to_vec(&self.census).map_err(|e| e.to_string())?),
            windows: self.windows.clone(),
        };
        let request = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(state.join(format!("{job}.request")))
            .map_err(|e| e.to_string())?;
        file.write_all(&request)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        File::open(state)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| e.to_string())?;
        let bytes =
            read_checkpoint(&self.checkpoint_path)?.ok_or("Recovery checkpoint disappeared")?;
        self.validate(&bytes)?;
        let mut authority = self.authority();
        authority.job = job.into();
        authority.request_identity = digest(&request);
        authority.job_request_hash = digest(&request);
        authority.worker_path = worker_path;
        authority.fence_path = fence_path;
        authority.legacy_path = legacy_path;
        authority.daemon_path = daemon_path;
        authority.target = target;
        authority.target_hash = target_hash;
        validate_request_origin(&authority, &request)?;
        preflight_preparation_size(&authority)?;
        let receipt = sealed_receipt(&authority).map_err(|e| e.to_string())?;
        Ok(Receipt {
            receipt,
            worker,
            fence,
            legacy,
            daemon: Some(daemon),
            authority,
        })
    }
}

impl Preparation {
    /// The caller obtains the census and sends prepare on the authenticated
    /// exact source connection. A preexisting checkpoint has no causal link to
    /// this job, even when its proof happens to name that same source. Refuse
    /// before prepare: old sources would overwrite same-source Prepared bytes.
    pub(crate) fn begin(
        state: &Path,
        job: &str,
        daemon: &DaemonImpact,
        census: Census,
        guard: &UiUpdateGuard,
    ) -> Result<Self, String> {
        let store = super::handoff::Store::discover()?;
        require_absent_checkpoint(&store.path)?;
        census.validate_capture(&daemon.capability.proof)?;
        if uuid::Uuid::parse_str(job).is_err() || !daemon.capability.proof.is_complete() {
            return Err("The update preparation has no complete job or source identity".into());
        }
        let worker = clone_descriptor(ORIGINAL_WORKER_FD).map_err(|e| e.to_string())?;
        let worker_path = state.join("lock");
        validate_lock(&worker, &worker_path).map_err(|e| e.to_string())?;
        let (fence, legacy) = guard.duplicate_for_recovery().map_err(|e| e.to_string())?;
        let fence_path = descriptor_path(&fence).map_err(|e| e.to_string())?;
        validate_lock(&fence, &fence_path).map_err(|e| e.to_string())?;
        let legacy_path = descriptor_path(&legacy).map_err(|e| e.to_string())?;
        validate_lock_mode(&legacy, &legacy_path, "READ").map_err(|e| e.to_string())?;
        let request_identity = digest(&serde_json::to_vec(daemon).map_err(|e| e.to_string())?);
        let job_request_hash = digest(
            &read_checkpoint(&state.join(format!("{job}.request")))?
                .ok_or("The update's exact request is unavailable")?,
        );
        // Retain the original pages before shutdown. This file is evidence for
        // an explicitly authorized retry, never an automatic prepare receipt.
        persist_capture(
            &store.path,
            &CapturedCensus {
                producer: Some(CaptureProducer::Proposed {
                    job: job.into(),
                    request_hash: job_request_hash.clone(),
                }),
                source: daemon.capability.proof.clone(),
                checkpoint_hash: None,
                census: census.clone(),
                windows: guard.prepared_window_slots().to_vec(),
            },
        )?;
        let preparation = Self {
            authority: Authority {
                version: 1,
                origin: Origin::AcknowledgedPrepare,
                job: job.into(),
                request_identity,
                job_request_hash,
                reviewed: Some(daemon.clone()),
                source: daemon.capability.proof.clone(),
                census,
                windows: guard.prepared_window_slots().to_vec(),
                expected: checkpointed_owners(daemon),
                checkpoint_path: store.path,
                checkpoint_hash: String::new(),
                worker_path,
                fence_path,
                legacy_path,
                daemon_path: PathBuf::new(),
                target: PathBuf::new(),
                target_hash: String::new(),
            },
            worker,
            fence,
            legacy,
        };
        preflight_preparation_size(&preparation.authority)?;
        Ok(preparation)
    }

    /// Call only after the exact peer acknowledges PrepareConfirmedUpgrade.
    /// A connection loss or an unacknowledged request cannot mint this receipt.
    pub(crate) fn acknowledged(self) -> Result<CapturedPreparation, String> {
        let result = self.capture_acknowledged();
        if let Err(error) = &result {
            eprintln!(
                "The source accepted preparation, but its recovery checkpoint could not be captured: {error}"
            );
        }
        result
    }

    fn capture_acknowledged(mut self) -> Result<CapturedPreparation, String> {
        self.validate_continuity()?;
        let bytes = read_checkpoint(&self.authority.checkpoint_path)?
            .ok_or("The acknowledged preparation has no checkpoint")?;
        validate_checkpoint(&self.authority, &bytes)?;
        let request_path = self
            .authority
            .worker_path
            .parent()
            .ok_or("Original worker has no state directory")?
            .join(format!("{}.request", self.authority.job));
        let request =
            read_checkpoint(&request_path)?.ok_or("The acknowledged update request disappeared")?;
        if digest(&request) != self.authority.job_request_hash {
            return Err(
                "The original update request changed before acknowledged checkpoint binding".into(),
            );
        }
        self.authority.checkpoint_hash = digest(&bytes);
        persist_bound_capture(
            &self.authority.checkpoint_path,
            &CapturedCensus {
                producer: Some(CaptureProducer::Acknowledged {
                    job: self.authority.job.clone(),
                    request_hash: self.authority.job_request_hash.clone(),
                }),
                source: self.authority.source.clone(),
                checkpoint_hash: Some(self.authority.checkpoint_hash.clone()),
                census: self.authority.census.clone(),
                windows: self.authority.windows.clone(),
            },
        )?;
        Ok(CapturedPreparation(self))
    }

    fn validate_continuity(&self) -> Result<(), String> {
        validate_lock(&self.worker, &self.authority.worker_path).map_err(|e| e.to_string())?;
        validate_lock(&self.fence, &self.authority.fence_path).map_err(|e| e.to_string())?;
        validate_lock_mode(&self.legacy, &self.authority.legacy_path, "READ")
            .map_err(|e| e.to_string())
    }
}

impl CapturedPreparation {
    /// The source has exited and the caller holds its daemon lease handoff.
    /// A Ready checkpoint follows normal restoration; only byte-identical
    /// Prepared evidence can use the adoption-only recovery entry.
    pub(crate) fn receipt(
        self,
        target_daemon: &Path,
        handoff: &LeaseHandoff,
    ) -> Result<Option<Receipt>, String> {
        let result = self.make_receipt(target_daemon, None, handoff);
        if let Err(error) = &result {
            eprintln!(
                "The acknowledged preparation could not authorize successor recovery: {error}"
            );
        }
        result
    }

    /// A release archive already verified by the updater can name the daemon
    /// bytes before the installer creates their final absolute path.
    pub(crate) fn receipt_with_digest(
        self,
        target_daemon: &Path,
        verified_daemon_sha256: &str,
        handoff: &LeaseHandoff,
    ) -> Result<Option<Receipt>, String> {
        let result = self.make_receipt(target_daemon, Some(verified_daemon_sha256), handoff);
        if let Err(error) = &result {
            eprintln!(
                "The acknowledged preparation could not authorize successor recovery: {error}"
            );
        }
        result
    }

    fn make_receipt(
        self,
        target_daemon: &Path,
        verified_digest: Option<&str>,
        handoff: &LeaseHandoff,
    ) -> Result<Option<Receipt>, String> {
        let mut preparation = self.0;
        preparation.validate_continuity()?;
        let bytes = read_checkpoint(&preparation.authority.checkpoint_path)?
            .ok_or("The acknowledged checkpoint disappeared before recovery")?;
        let phase = validate_checkpoint(&preparation.authority, &bytes)?;
        if phase == "Ready" {
            // The same acknowledged source may finish Prepared -> Ready while
            // this worker continuously owns its fences. Retain the full
            // original scope for the exact final bytes consumed by normal
            // restoration, including panes ended before that checkpoint.
            persist_bound_capture(
                &preparation.authority.checkpoint_path,
                &CapturedCensus {
                    producer: Some(CaptureProducer::Acknowledged {
                        job: preparation.authority.job.clone(),
                        request_hash: preparation.authority.job_request_hash.clone(),
                    }),
                    source: preparation.authority.source.clone(),
                    checkpoint_hash: Some(digest(&bytes)),
                    census: preparation.authority.census.clone(),
                    windows: preparation.authority.windows.clone(),
                },
            )?;
            return Ok(None);
        }
        if digest(&bytes) != preparation.authority.checkpoint_hash {
            return Err("The Prepared checkpoint changed after this update acknowledged it; automatic recovery was refused".into());
        }
        if !target_daemon.is_absolute()
            || target_daemon
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err("Recovery needs the verified daemon's absolute installation path".into());
        }
        preparation.authority.target = target_daemon.to_owned();
        preparation.authority.target_hash = match verified_digest {
            Some(hash)
                if hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
            {
                hash.into()
            }
            Some(_) => return Err("Recovery daemon digest is invalid".into()),
            None => executable_hash(target_daemon)?,
        };
        let daemon = handoff
            .duplicate_for_recovery()
            .map_err(|e| e.to_string())?;
        preparation.authority.daemon_path = descriptor_path(&daemon).map_err(|e| e.to_string())?;
        validate_lock(&daemon, &preparation.authority.daemon_path).map_err(|e| e.to_string())?;
        let receipt = sealed_receipt(&preparation.authority).map_err(|e| e.to_string())?;
        Ok(Some(Receipt {
            receipt,
            worker: preparation.worker,
            fence: preparation.fence,
            legacy: preparation.legacy,
            daemon: Some(daemon),
            authority: preparation.authority,
        }))
    }
}

impl Receipt {
    pub(crate) fn release_daemon_reservation(&mut self) {
        self.daemon.take();
    }

    pub(crate) fn inherit(&self, command: &mut Command) -> io::Result<()> {
        forward(
            command,
            [
                &self.receipt,
                &self.worker,
                &self.fence,
                self.daemon.as_ref().ok_or_else(|| {
                    io::Error::other("Recovery daemon reservation was already released")
                })?,
                &self.legacy,
            ],
        )
    }

    /// Bind the target's verified serving proof to this exact Prepared job.
    /// The target's census verifier checks the live inventory on its own wire.
    pub(crate) fn verify_successor(
        &self,
        serving: &HostSessionProof,
    ) -> Result<RecoveryOutcome, String> {
        let record = read_verified_record_identity(
            &self.authority.checkpoint_path,
            &self.authority.checkpoint_hash,
            &self.authority.source,
            &self.authority.census,
            &self.authority.target,
            Some(&self.authority),
            Some(&self.authority.target_hash),
            serving,
        )?;
        Ok(RecoveryOutcome {
            paused_terminals: record.paused_terminals,
            paused_chats: record.paused_chats,
        })
    }
}

fn read_settled_checkpoint(
    checkpoint_path: &Path,
    checkpoint_hash: &str,
    outcome_hash: &str,
) -> Result<Vec<u8>, String> {
    if let Some(bytes) = read_checkpoint(checkpoint_path)? {
        if digest(&bytes) != checkpoint_hash {
            return Err("The canonical checkpoint differs from the settled outcome".into());
        }
        return Ok(bytes);
    }
    let (_, proof, _) = load_retirement(checkpoint_path, Some(checkpoint_hash))?
        .ok_or("The Prepared checkpoint and its verified retirement disappeared")?;
    if proof.outcome_sha256 != outcome_hash {
        return Err("Prepared retirement names a different original outcome".into());
    }
    let bytes =
        read_checkpoint(&proof.archive_path)?.ok_or("The verified Prepared archive disappeared")?;
    if digest(&bytes) != checkpoint_hash {
        return Err("The Prepared archive changed after retirement".into());
    }
    Ok(bytes)
}

/// Both the target's inventory verifier and the sealed receipt authenticate
/// the same durable outcome. Only the sealed receipt has the original authority.
fn read_verified_record_identity(
    checkpoint_path: &Path,
    checkpoint_hash: &str,
    source: &HostSessionProof,
    census: &Census,
    target: &Path,
    request: Option<&Authority>,
    target_hash: Option<&str>,
    serving: &HostSessionProof,
) -> Result<RecoveryRecord, String> {
    let path = checkpoint_path.with_file_name(recovery_name(checkpoint_hash));
    let bytes = read_checkpoint(&path)?.ok_or("The recovery daemon has no durable outcome")?;
    let record: RecoveryRecord = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let checkpoint = read_settled_checkpoint(checkpoint_path, checkpoint_hash, &digest(&bytes))?;
    validate_settled_record(&record, &checkpoint, checkpoint_path, target)?;
    if !record.settled
        || record.version != 2
        || request.is_some_and(|authority| {
            record.job != authority.job
                || record.request_identity != authority.request_identity
                || record.origin != authority.origin
                || record.job_request_hash != authority.job_request_hash
                || record.reviewed != authority.reviewed
                || record.windows != authority.windows
        })
        || &record.source != source
        || record.checkpoint_hash != checkpoint_hash
        || digest(&checkpoint) != checkpoint_hash
        || target_hash.is_some_and(|hash| record.target_hash != hash)
        || record.target.as_deref() != Some(target)
        || executable_hash(target)? != record.target_hash
        || &record.census != census
        || record.successor.as_ref() != Some(serving)
        || !serving.is_complete()
    {
        return Err("The serving daemon has not verified this exact recovery outcome".into());
    }
    let evidence = PendingRecovery {
        source: source.clone(),
        checkpoint_hash: checkpoint_hash.into(),
        census: census.clone(),
        windows: Vec::new(),
        original_census_complete: true,
        producer: None,
        checkpoint_path: checkpoint_path.to_owned(),
    };
    evidence.validate(&checkpoint)?;
    record.validate_census()?;
    Ok(record)
}

fn read_verified_record(
    checkpoint_path: &Path,
    checkpoint_hash: &str,
    source: &HostSessionProof,
    census: &Census,
    target: &Path,
    request: Option<&Authority>,
    target_hash: Option<&str>,
    serving: &HostSessionProof,
    live_terminals: &[crate::terminal_ipc::TerminalDescriptor],
    live_chats: &[Session],
) -> Result<RecoveryRecord, String> {
    let record = read_verified_record_identity(
        checkpoint_path,
        checkpoint_hash,
        source,
        census,
        target,
        request,
        target_hash,
        serving,
    )?;
    let terminals: BTreeSet<_> = live_terminals.iter().map(|item| item.session.id).collect();
    let chats: BTreeSet<_> = live_chats.iter().map(|item| item.id).collect();
    if terminals.len() != live_terminals.len()
        || chats.len() != live_chats.len()
        || terminals != record.adopted_terminals.iter().copied().collect()
        || chats != record.adopted_chats.iter().copied().collect()
        || record.adopted_chat_descriptors.len() != live_chats.len()
        || record.adopted_terminal_descriptors.len() != live_terminals.len()
        || live_terminals.iter().any(|item| {
            !record
                .adopted_terminal_descriptors
                .iter()
                .any(|issued| same_adopted_terminal(issued, item, serving))
                || !verified_terminal(census, item, serving, target)
        })
        || live_chats.iter().any(|item| {
            !record.adopted_chat_descriptors.contains(item)
                || !verified_chat(census, item, serving, live_terminals, target)
        })
    {
        return Err(
            "The serving recovery inventory differs from its verified original owners".into(),
        );
    }
    Ok(record)
}

fn same_adopted_terminal(
    issued: &crate::terminal_ipc::TerminalDescriptor,
    actual: &crate::terminal_ipc::TerminalDescriptor,
    serving: &HostSessionProof,
) -> bool {
    let mut before = issued.session.clone();
    let after = actual.session.clone();
    if before.continuity.live_proof().as_ref() != Some(serving)
        || after.continuity.live_proof().as_ref() != Some(serving)
    {
        return false;
    }
    // Attachment is per client; neither it nor output/activity advances a
    // terminal's issued allocation or authenticated ownership.
    before.continuity = after.continuity.clone();
    before == after
        && issued.upgrade_sources == actual.upgrade_sources
        && issued.upgrade_relaunches == actual.upgrade_relaunches
}

fn recovered_terminal(known: &TerminalSession, actual: &TerminalSession) -> bool {
    known.id == actual.id
        && known.launch == actual.launch
        && known.owned_codex_listener == actual.owned_codex_listener
        && known.recovery_origin == actual.recovery_origin
        && known.predecessor == actual.predecessor
}

fn verified_terminal(
    census: &Census,
    actual: &crate::terminal_ipc::TerminalDescriptor,
    serving: &HostSessionProof,
    target_daemon: &Path,
) -> bool {
    census
        .terminals
        .iter()
        .chain(
            census
                .chats
                .iter()
                .filter_map(|chat| chat.terminal.as_deref()),
        )
        .any(|known| {
            (recovered_terminal(known, &actual.session)
                || verified_console_terminal(census, known, actual, target_daemon))
                && actual
                    .upgrade_sources
                    .iter()
                    .any(|source| same_terminal(known, source))
                && match actual.session.continuity.live_proof() {
                    Some(proof) => proof == *serving,
                    None => matches!(
                        actual.session.continuity,
                        crate::model::TerminalContinuity::RelaunchAvailable { .. }
                    ),
                }
        })
}

fn verified_console_terminal(
    census: &Census,
    known: &TerminalSession,
    actual: &crate::terminal_ipc::TerminalDescriptor,
    target_daemon: &Path,
) -> bool {
    let paired = census.chats.iter().any(|chat| {
        chat.id == known.id
            && chat.launch.provider == crate::agent::Provider::Claude
            && matches!(
                chat.native_continuity,
                crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
            )
            && chat
                .terminal
                .as_ref()
                .is_some_and(|terminal| same_terminal(known, terminal))
    });
    if !paired
        || crate::agent_execution::console_agent(&known.launch).is_none()
        || Path::new(&actual.session.launch.program)
            != target_daemon.with_file_name(crate::companion::Product::Cli.file_name())
        || !actual.upgrade_relaunches.iter().any(|accepted| {
            same_terminal(known, &accepted.source) && accepted.recipe == actual.session.launch
        })
    {
        return false;
    }
    // The authenticated retained actor records the old view and its exact
    // replacement recipe. Only this installation's thin CLI may change.
    let mut normalized = actual.session.clone();
    normalized.launch.program.clone_from(&known.launch.program);
    recovered_terminal(known, &normalized)
}

fn verified_chat(
    census: &Census,
    actual: &Session,
    serving: &HostSessionProof,
    live_terminals: &[crate::terminal_ipc::TerminalDescriptor],
    target_daemon: &Path,
) -> bool {
    census.chats.iter().any(|known| {
        let mut normalized = actual.clone();
        if let (Some(before), Some(after)) = (&known.terminal, &mut normalized.terminal)
            && !recovered_terminal(before, after)
        {
            let verified_pair = live_terminals.iter().any(|terminal| {
                recovered_terminal(after, &terminal.session)
                    && verified_terminal(census, terminal, serving, target_daemon)
            });
            if !verified_pair {
                return false;
            }
            after.launch.program.clone_from(&before.launch.program);
        }
        recovered_chat(known, &normalized, serving)
    })
}

fn recovered_chat(known: &Session, actual: &Session, serving: &HostSessionProof) -> bool {
    recovered_chat_owner(known, actual, serving, true)
}

// Ordinary holder authentication may retain the same actor after /clear or a
// confirmed thread switch. That counts as a live original owner, but never as
// an alias for the original conversation's captured page.
fn current_recovered_chat(known: &Session, actual: &Session, serving: &HostSessionProof) -> bool {
    recovered_chat_owner(known, actual, serving, false)
}

fn captured_chat_alias(
    census: &Census,
    known: &Session,
    actual: &Session,
    serving: &HostSessionProof,
) -> bool {
    let captured = known.launch.session_id.as_ref().or_else(|| {
        census
            .snapshots
            .iter()
            .find(|saved| saved.source == *known)
            .and_then(|saved| saved.snapshot.session_id.as_ref())
    });
    captured == actual.launch.session_id.as_ref() && recovered_chat(known, actual, serving)
}

fn recovered_chat_owner(
    known: &Session,
    actual: &Session,
    serving: &HostSessionProof,
    exact_thread: bool,
) -> bool {
    known.id == actual.id
        && actual.proof.as_ref() == Some(serving)
        && known.launch.provider == actual.launch.provider
        && known.launch.recipe == actual.launch.recipe
        && (!exact_thread
            || known
                .launch
                .session_id
                .as_ref()
                .is_none_or(|id| actual.launch.session_id.as_ref() == Some(id)))
        && known.native_continuity == actual.native_continuity
        && actual.native_continuity.retains_provider()
        && known.owned_codex_listener == actual.owned_codex_listener
        && match (&known.terminal, &actual.terminal) {
            (None, None) => true,
            (Some(before), Some(after)) => {
                recovered_terminal(before, after)
                    && after.continuity.live_proof().as_ref() == Some(serving)
            }
            _ => false,
        }
}

/// A failed or ineligible recovery capture must still check Ready before any
/// ordinary successor starts. A matching proof alone never waives Prepared.
pub(crate) fn require_ready(daemon: &DaemonImpact, census: &Census) -> Result<(), String> {
    let authority = Authority {
        version: 1,
        origin: Origin::AcknowledgedPrepare,
        job: String::new(),
        request_identity: String::new(),
        job_request_hash: String::new(),
        reviewed: Some(daemon.clone()),
        source: daemon.capability.proof.clone(),
        census: census.clone(),
        windows: Vec::new(),
        expected: checkpointed_owners(daemon),
        checkpoint_path: PathBuf::new(),
        checkpoint_hash: String::new(),
        worker_path: PathBuf::new(),
        fence_path: PathBuf::new(),
        legacy_path: PathBuf::new(),
        daemon_path: PathBuf::new(),
        target: PathBuf::new(),
        target_hash: String::new(),
    };
    let bytes = super::handoff::Store::discover()?
        .read_bytes()?
        .ok_or("The acknowledged source has no Ready checkpoint")?;
    if validate_checkpoint(&authority, &bytes)? != "Ready" {
        return Err("The acknowledged preparation is still Prepared; previous provider shutdown is unconfirmed and ordinary startup is refused".into());
    }
    Ok(())
}

/// The reviewed owners the source's checkpoint must name. A conversation
/// whose provider already ended is reviewed, so the user sees it reopen, but
/// the source has nothing of it to checkpoint and leaves it out.
pub fn checkpointed_owners(reviewed: &DaemonImpact) -> Vec<(SessionKind, TerminalId)> {
    reviewed
        .sessions
        .iter()
        .filter(|item| item.checkpointed())
        .map(|item| (item.kind, item.id))
        .collect()
}

fn validate_checkpoint(authority: &Authority, bytes: &[u8]) -> Result<String, String> {
    let checkpoint: CheckpointIndex = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if !(1..=5).contains(&checkpoint.version)
        || checkpoint.proof != authority.source
        || !matches!(checkpoint.phase.as_str(), "Prepared" | "Ready")
    {
        return Err("The checkpoint is not this update's acknowledged source preparation".into());
    }
    let mut terminals = BTreeSet::new();
    for item in checkpoint.terminals.iter().chain(&checkpoint.carried) {
        let source = &item.source;
        if !terminals.insert(source.id)
            || source.continuity.live_proof().as_ref() != Some(&authority.source)
            || !authority
                .census
                .terminals
                .iter()
                .any(|known| same_terminal(known, source))
        {
            return Err(
                "The prepared terminal inventory does not match the acknowledged source census"
                    .into(),
            );
        }
    }
    let mut chats = BTreeSet::new();
    for item in &checkpoint.chats {
        let source = &item.source;
        if !chats.insert(source.id)
            || source.proof.as_ref() != Some(&authority.source)
            || !authority
                .census
                .chats
                .iter()
                .any(|known| same_chat(known, source, &item.launch))
        {
            return Err(
                "The prepared conversation inventory does not match the acknowledged source census"
                    .into(),
            );
        }
    }
    if authority.expected.iter().any(|(kind, id)| match kind {
        SessionKind::Terminal => !terminals.contains(id),
        SessionKind::NativeChat => !chats.contains(id),
    }) {
        return Err("The acknowledged checkpoint omitted a reviewed source owner".into());
    }
    Ok(checkpoint.phase)
}

fn same_terminal(known: &TerminalSession, source: &TerminalSession) -> bool {
    known.id == source.id
        && known.launch == source.launch
        && known.continuity.live_proof() == source.continuity.live_proof()
        && known.owned_codex_listener == source.owned_codex_listener
        && known.recovery_origin == source.recovery_origin
        && known.predecessor == source.predecessor
}

fn same_chat(known: &Session, source: &Session, effective: &crate::chat::Launch) -> bool {
    known.id == source.id
        && known.proof == source.proof
        && known.launch.provider == source.launch.provider
        && known.launch.recipe == source.launch.recipe
        && known.owned_codex_listener == source.owned_codex_listener
        && match (&known.terminal, &source.terminal) {
            (None, None) => true,
            (Some(known), Some(source)) => same_terminal(known, source),
            _ => false,
        }
        // ChatList projects the provider's confirmed identity over the issued
        // descriptor. Frozen sources keep that original descriptor unchanged
        // in `source`, and put the current conversation in the separate launch.
        && effective.provider == known.launch.provider
        && known
            .launch
            .session_id
            .as_ref()
            .is_none_or(|id| effective.session_id.as_ref() == Some(id))
        && same_effective_recipe(&known.launch, effective)
}

fn same_effective_recipe(known: &crate::chat::Launch, effective: &crate::chat::Launch) -> bool {
    let mut confirmed = known.clone();
    confirmed.session_id.clone_from(&effective.session_id);
    match (confirmed.terminal_recipe(), effective.terminal_recipe()) {
        (Some(before), Some(after)) => before == after,
        // A retained Claude actor can precede its first conversation ID. There
        // is no identity normalization to permit in its recipe at that point.
        (None, None) => confirmed.session_id.is_none() && confirmed.recipe == effective.recipe,
        _ => false,
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_checkpoint(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() > LIMIT
    {
        return Err("Unsafe or oversized preparation evidence; recovery was refused".into());
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > LIMIT {
        return Err("Preparation evidence exceeds its byte limit".into());
    }
    Ok(Some(bytes))
}

fn require_absent_checkpoint(path: &Path) -> Result<(), String> {
    if read_checkpoint(path)?.is_some() {
        return Err("An earlier upgrade checkpoint already exists. Its bytes were preserved and this update did not prepare the source; resolve that checkpoint before attempting another preparation.".into());
    }
    let directory = path.parent().ok_or("Upgrade checkpoint has no directory")?;
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".checkpoint-retirement.")
        {
            return Err(format!(
                "An earlier checkpoint remains in retirement custody at {}. Its bytes were preserved; inspect that evidence and the serving daemon before another preparation",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

fn executable_hash(path: &Path) -> Result<String, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file()
        || !crate::install_guard::trusted_executable_owner(metadata.uid())
        || metadata.len() > 512 * 1024 * 1024
    {
        return Err("The recovery daemon executable is not trusted".into());
    }
    let mut hash = Sha256::new();
    let mut reader = file.take(512 * 1024 * 1024 + 1);
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// A receipt's immutability rests on memfd seals, which Darwin lacks; see
/// `update::anonymous_file`.
#[cfg(not(target_os = "linux"))]
fn sealed_receipt(_: &Authority) -> io::Result<File> {
    Err(io::Error::other("self-update requires Linux"))
}

#[cfg(not(target_os = "linux"))]
fn validate_sealed_descriptor(_: i32) -> io::Result<()> {
    Err(io::Error::other("self-update requires Linux"))
}

fn clone_descriptor(fd: i32) -> io::Result<File> {
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(File::from(
        unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?,
    ))
}

fn descriptor_path(file: &File) -> io::Result<PathBuf> {
    fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn validate_lock(file: &File, path: &Path) -> io::Result<()> {
    validate_lock_mode(file, path, "WRITE")
}

fn validate_lock_mode(file: &File, path: &Path, access: &str) -> io::Result<()> {
    let actual = file.metadata()?;
    let expected = fs::symlink_metadata(path)?;
    if !actual.is_file()
        || actual.uid() != crate::daemon::current_uid()
        || actual.mode() & 0o077 != 0
        || actual.nlink() != 1
        || !expected.is_file()
        || actual.dev() != expected.dev()
        || actual.ino() != expected.ino()
    {
        return Err(io::Error::other(
            "Recovery lost its exact continuous update lease or fence",
        ));
    }
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    if !info
        .lines()
        .filter_map(|line| line.strip_prefix("lock:"))
        .any(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            fields.len() == 8
                && fields[1..4] == ["FLOCK", "ADVISORY", access]
                && fields[6..8] == ["0", "EOF"]
        })
    {
        return Err(io::Error::other(
            "Recovery's inherited descriptor no longer owns its exclusive lease",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn sealed_receipt(authority: &Authority) -> io::Result<File> {
    use std::io::{Seek, SeekFrom};
    let bytes = serde_json::to_vec(authority).map_err(io::Error::other)?;
    if bytes.len() as u64 > LIMIT {
        return Err(io::Error::other("Recovery receipt exceeds its byte limit"));
    }
    let fd = unsafe {
        libc::memfd_create(
            c"cagents-update-recovery".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(&bytes)?;
    file.seek(SeekFrom::Start(0))?;
    if unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, SEALS) } == -1 {
        return Err(io::Error::last_os_error());
    }
    validate_sealed_descriptor(fd)?;
    Ok(file)
}

#[cfg(target_os = "linux")]
fn validate_sealed_descriptor(fd: i32) -> io::Result<()> {
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    if seals < 0 {
        return Err(io::Error::last_os_error());
    }
    if seals & SEALS != SEALS {
        return Err(io::Error::other("Update recovery receipt is not sealed"));
    }
    let file = clone_descriptor(fd)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 0
        || metadata.len() > LIMIT
    {
        return Err(io::Error::other(
            "Update recovery receipt is not an owned bounded anonymous file",
        ));
    }
    Ok(())
}

fn forward(command: &mut Command, files: [&File; 5]) -> io::Result<()> {
    let copies = files
        .map(|file| {
            let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 200) };
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(unsafe { File::from_raw_fd(fd) })
            }
        })
        .into_iter()
        .collect::<io::Result<Vec<_>>>()?;
    command.env(ENV, RECEIPT_FD.to_string());
    unsafe {
        command.pre_exec(move || {
            for (file, target) in copies
                .iter()
                .zip([RECEIPT_FD, WORKER_FD, FENCE_FD, DAEMON_FD, LEGACY_FD])
            {
                if libc::dup2(file.as_raw_fd(), target) == -1
                    || libc::fcntl(target, libc::F_SETFD, 0) == -1
                {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

fn inherited() -> io::Result<&'static Mutex<Option<Inherited>>> {
    INHERITED
        .get_or_init(|| {
            if std::env::var_os(ENV).is_none() {
                return Ok(Mutex::new(None));
            }
            if std::env::var(ENV).ok().as_deref() != Some("193") {
                return Err(io::Error::other(
                    "Invalid inherited update recovery descriptor",
                ));
            }
            let mut files = Vec::new();
            for fd in [RECEIPT_FD, WORKER_FD, FENCE_FD, DAEMON_FD, LEGACY_FD] {
                if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
                    return Err(io::Error::last_os_error());
                }
                if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                    return Err(io::Error::last_os_error());
                }
                files.push(unsafe { File::from_raw_fd(fd) });
            }
            let mut files = files.into_iter();
            Ok(Mutex::new(Some(Inherited {
                receipt: files.next().unwrap(),
                worker: files.next().unwrap(),
                fence: files.next().unwrap(),
                daemon: files.next().unwrap(),
                legacy: files.next().unwrap(),
            })))
        })
        .as_ref()
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))
}

pub(crate) fn initialize_inherited() -> io::Result<()> {
    inherited().map(|_| ())
}

pub fn has_inherited_receipt() -> io::Result<bool> {
    Ok(inherited()?
        .lock()
        .map_err(|_| io::Error::other("Update recovery descriptors are unavailable"))?
        .is_some())
}

/// A receipt grants no authority to a schema helper, scope launcher or other
/// intermediate executable. Bind forwarding to the selected daemon's path and
/// verified bytes before installing its recovery descriptor remapping hook. The daemon
/// still validates the receipt and all continuous ownership after exec.
pub(crate) fn forward_target_daemon(command: &mut Command) -> io::Result<()> {
    if let Some(receipt) = inherited()?
        .lock()
        .map_err(|_| io::Error::other("Update recovery descriptors are unavailable"))?
        .as_ref()
    {
        validate_forward_target(command, &receipt.receipt)?;
        forward(
            command,
            [
                &receipt.receipt,
                &receipt.worker,
                &receipt.fence,
                &receipt.daemon,
                &receipt.legacy,
            ],
        )?;
    }
    Ok(())
}

fn validate_forward_target(command: &Command, receipt: &File) -> io::Result<()> {
    validate_sealed_descriptor(receipt.as_raw_fd())?;
    let mut bytes = vec![0; receipt.metadata()?.len() as usize];
    // F_DUPFD shares an open-file description with the selected CLI and daemon.
    // Read by offset so forwarding never consumes another receiver's receipt.
    std::os::unix::fs::FileExt::read_exact_at(receipt, &mut bytes, 0)?;
    let authority: Authority = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    let selected = Path::new(command.get_program());
    if authority.version != 1
        || !selected.is_absolute()
        || !authority.target.is_absolute()
        || fs::canonicalize(selected)? != fs::canonicalize(&authority.target)?
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Update recovery may only be forwarded to its verified daemon executable",
        ));
    }
    if executable_hash(selected).map_err(io::Error::other)? != authority.target_hash {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The recovery daemon executable changed after verification",
        ));
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RecoveryRecord {
    version: u16,
    #[serde(default)]
    origin: Origin,
    pub job: String,
    pub request_identity: String,
    job_request_hash: String,
    #[serde(default)]
    reviewed: Option<DaemonImpact>,
    #[serde(default)]
    windows: Vec<String>,
    pub source: HostSessionProof,
    pub checkpoint_hash: String,
    pub census: Census,
    pub adopted_terminals: Vec<TerminalId>,
    pub adopted_chats: Vec<TerminalId>,
    #[serde(default)]
    pub adopted_chat_descriptors: Vec<Session>,
    #[serde(default)]
    pub adopted_terminal_descriptors: Vec<crate::terminal_ipc::TerminalDescriptor>,
    pub paused_terminals: Vec<TerminalId>,
    pub paused_chats: Vec<TerminalId>,
    pub settled: bool,
    pub successor: Option<HostSessionProof>,
    target_hash: String,
    #[serde(default)]
    target: Option<PathBuf>,
    /// Dropping either a successful or failed adoption releases only these
    /// duplicates, without unlocking the worker's continuous original fences.
    #[cfg(feature = "host")]
    #[serde(skip)]
    authority: Option<Continuity>,
}

fn recovery_name(hash: &str) -> String {
    format!("daemon-upgrade.recovery.{hash}.json")
}

/// Immutable identity of the originally settled outcome, never renewed
/// receipt authority. A later incarnation must authenticate its own holders.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettledWitness {
    pub checkpoint_hash: String,
    pub record_hash: String,
    pub target: PathBuf,
    pub target_hash: String,
    pub successor: HostSessionProof,
    pub adopted_chats: Vec<Session>,
    pub adopted_terminals: Vec<crate::terminal_ipc::TerminalDescriptor>,
}

/// Durable evidence that these exact Prepared bytes have already been handled.
/// This value carries no locks, receipt, or authority to replay or adopt owners.
#[derive(Clone, Debug)]
pub struct AlreadyRecovered {
    pub source: HostSessionProof,
    pub census: Census,
    pub paused_terminals: Vec<TerminalId>,
    pub paused_chats: Vec<TerminalId>,
    pub witness: SettledWitness,
    pub adopted_chat_descriptors: Vec<Session>,
    #[cfg(any(test, feature = "test-support"))]
    pub adopted_terminals: Vec<TerminalId>,
    checkpoint_path: PathBuf,
    daemon_binary: PathBuf,
    checkpoint_hash: String,
    outcome_hash: String,
}

fn validate_settled_record(
    record: &RecoveryRecord,
    checkpoint: &[u8],
    checkpoint_path: &Path,
    target: &Path,
) -> Result<(), String> {
    super::handoff::validate_checkpoint_bytes(checkpoint)?;
    let index: CheckpointIndex = serde_json::from_slice(checkpoint).map_err(|e| e.to_string())?;
    if record.checkpoint_hash != digest(checkpoint)
        || record.source != index.proof
        || index.phase != "Prepared"
    {
        return Err("Settled recovery does not match the exact Prepared checkpoint".into());
    }
    let authority = validate_settled_identity(record, checkpoint_path, target)?;
    if validate_checkpoint(&authority, checkpoint)? != "Prepared" {
        return Err("Settled recovery no longer names a Prepared checkpoint".into());
    }
    Ok(())
}

/// Metadata can select candidates for ordinary holder authentication. It is
/// never evidence that an archive exists or that the current census is usable.
fn validate_settled_identity(
    record: &RecoveryRecord,
    checkpoint_path: &Path,
    target: &Path,
) -> Result<Authority, String> {
    let job = uuid::Uuid::parse_str(&record.job).map_err(|e| e.to_string())?;
    if record.version != 2
        || !record.settled
        || job.to_string() != record.job
        || record.successor.as_ref().is_none_or(|proof| {
            !proof.is_complete()
                || proof == &record.source
                || proof.boot_id != record.source.boot_id
        })
        || record.target.as_deref() != Some(target)
        || target.file_name().is_none_or(|name| name != "cagentsd")
        || executable_hash(target)? != record.target_hash
    {
        return Err("Prepared checkpoint has no exact settled recovery for this daemon; evidence was preserved".into());
    }
    validate_private_directory(
        checkpoint_path
            .parent()
            .ok_or("Prepared checkpoint has no directory")?,
    )?;
    record.census.validate_evidence(&record.source)?;
    record.validate_census()?;
    let layout = crate::update::locate(target).map_err(|e| e.to_string())?;
    let state = layout.prefix.join(".update");
    validate_private_directory(&state)?;
    let request = read_checkpoint(&state.join(format!("{}.request", record.job)))?
        .ok_or("Settled recovery's original update request disappeared")?;
    if digest(&request) != record.job_request_hash {
        return Err(
            "Settled recovery's original update request changed; evidence was preserved".into(),
        );
    }
    let authority = Authority {
        version: 1,
        origin: record.origin.clone(),
        job: record.job.clone(),
        request_identity: record.request_identity.clone(),
        job_request_hash: record.job_request_hash.clone(),
        reviewed: record.reviewed.clone(),
        source: record.source.clone(),
        census: record.census.clone(),
        windows: record.windows.clone(),
        expected: record
            .reviewed
            .as_ref()
            .map(checkpointed_owners)
            .unwrap_or_default(),
        checkpoint_path: checkpoint_path.to_owned(),
        checkpoint_hash: record.checkpoint_hash.clone(),
        worker_path: PathBuf::new(),
        fence_path: PathBuf::new(),
        legacy_path: PathBuf::new(),
        daemon_path: PathBuf::new(),
        target: target.to_owned(),
        target_hash: record.target_hash.clone(),
    };
    validate_request_origin(&authority, &request)?;
    let successor = record
        .successor
        .as_ref()
        .ok_or("Settled recovery has no successor")?;
    if record
        .adopted_chat_descriptors
        .iter()
        .map(|chat| chat.id)
        .collect::<BTreeSet<_>>()
        != record.adopted_chats.iter().copied().collect()
        || record.adopted_chat_descriptors.len() != record.adopted_chats.len()
        || record
            .adopted_terminal_descriptors
            .iter()
            .map(|terminal| terminal.session.id)
            .collect::<BTreeSet<_>>()
            != record.adopted_terminals.iter().copied().collect()
        || record.adopted_terminal_descriptors.len() != record.adopted_terminals.len()
        || record.adopted_chat_descriptors.iter().any(|chat| {
            !verified_chat(
                &record.census,
                chat,
                successor,
                &record.adopted_terminal_descriptors,
                target,
            )
        })
        || record
            .adopted_terminal_descriptors
            .iter()
            .any(|terminal| !verified_terminal(&record.census, terminal, successor, target))
    {
        return Err("Settled recovery lost the exact adopted owner descriptors".into());
    }
    record.validate_issued_chats()?;
    Ok(authority)
}

/// Classify exact settled history only. Orphan adoption still authenticates
/// every current holder and cannot execute a recipe based on this record.
pub fn classify_retained_prepared(
    checkpoint_path: &Path,
    daemon_binary: &Path,
) -> Result<Option<AlreadyRecovered>, String> {
    let Some(checkpoint) = read_checkpoint(checkpoint_path)? else {
        return Ok(None);
    };
    #[derive(Deserialize)]
    struct Phase {
        phase: String,
    }
    if serde_json::from_slice::<Phase>(&checkpoint)
        .map_err(|e| e.to_string())?
        .phase
        != "Prepared"
    {
        return Ok(None);
    }
    let hash = digest(&checkpoint);
    let bytes = read_checkpoint(&checkpoint_path.with_file_name(recovery_name(&hash)))?.ok_or(
        "Prepared checkpoint has no matching settled recovery record; evidence was preserved",
    )?;
    let record: RecoveryRecord = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let target = fs::canonicalize(daemon_binary).map_err(|e| e.to_string())?;
    validate_settled_record(&record, &checkpoint, checkpoint_path, &target)?;
    // Extra issued aliases are optional historical hints. The exact canonical
    // checkpoint and original settled record above remain the strict proof;
    // stale, unsafe or unsupported pointers must not veto their classification.
    let history = scoped_retirement(
        checkpoint_path,
        &target,
        Some(&record.source),
        Some(&record.checkpoint_hash),
    )
    .unwrap_or_else(|error| {
        tracing::warn!(%error, "optional prior retirement aliases were ignored");
        None
    });
    let additional = match history {
        Some((retired, proof, _)) => {
            if proof.outcome_sha256 != digest(&bytes) {
                return Err("Planned retirement changed the original outcome".into());
            }
            retired.adopted_chat_descriptors
        }
        None => Vec::new(),
    };
    Ok(Some(recovered_data(
        record,
        checkpoint_path,
        target,
        additional,
        digest(&bytes),
    )))
}

const ACTIVE_RECOVERY: &str = "daemon-upgrade.retirement-active.json";

#[derive(Default, Serialize, Deserialize)]
struct ActiveRecovery {
    version: u16,
    checkpoint_hash: String,
    retirement_hash: String,
    #[serde(default)]
    target: Option<PathBuf>,
    #[serde(default)]
    source: Option<HostSessionProof>,
}

#[derive(Serialize, Deserialize)]
struct Retirement {
    version: u16,
    checkpoint_hash: String,
    outcome_hash: String,
    source: HostSessionProof,
    job: String,
    request_identity: String,
    archive_path: PathBuf,
    adopted_chat_descriptors: Vec<Session>,
    previous: Option<String>,
}

/// Read-only evidence for an independent verifier. Neither the retirement nor
/// the archived checkpoint grants replay, lease transfer, or adoption authority.
#[derive(Clone, Debug)]
pub struct ArchiveProof {
    pub archive_path: PathBuf,
    pub checkpoint_sha256: String,
    pub outcome_path: PathBuf,
    pub outcome_sha256: String,
    pub retirement_path: PathBuf,
    pub retirement_sha256: String,
}

fn archive_path(checkpoint: &Path, hash: &str) -> PathBuf {
    checkpoint.with_file_name(format!("daemon-upgrade.prepared.{hash}.json"))
}
fn retirement_path(checkpoint: &Path, hash: &str) -> PathBuf {
    checkpoint.with_file_name(format!("daemon-upgrade.retirement.{hash}.json"))
}

fn load_retirement(
    checkpoint: &Path,
    expected: Option<&str>,
) -> Result<Option<(Retirement, ArchiveProof, RecoveryRecord)>, String> {
    let Some(bytes) = read_checkpoint(&checkpoint.with_file_name(ACTIVE_RECOVERY))? else {
        return Ok(None);
    };
    validate_private_directory(
        checkpoint
            .parent()
            .ok_or("Recovery storage has no directory")?,
    )?;
    #[derive(Deserialize)]
    struct Scope {
        checkpoint_hash: String,
    }
    let scope: Scope = match serde_json::from_slice(&bytes) {
        Ok(scope) => scope,
        Err(_) if expected.is_some() => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if expected.is_some_and(|hash| hash != scope.checkpoint_hash) {
        return Ok(None);
    }
    let active: ActiveRecovery = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if active.version != 2
        || !valid_hash(&active.checkpoint_hash)
        || !valid_hash(&active.retirement_hash)
    {
        return Err("Active retirement pointer has an invalid identity".into());
    }
    let path = retirement_path(checkpoint, &active.retirement_hash);
    let bytes = read_checkpoint(&path)?.ok_or("Active immutable retirement record disappeared")?;
    if digest(&bytes) != active.retirement_hash {
        return Err("Immutable retirement record changed".into());
    }
    let retired: Retirement = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if retired.version != 1
        || retired.checkpoint_hash != active.checkpoint_hash
        || !valid_hash(&retired.outcome_hash)
        || retired
            .previous
            .as_ref()
            .is_some_and(|hash| !valid_hash(hash))
        || retired.archive_path != archive_path(checkpoint, &active.checkpoint_hash)
    {
        return Err("Active retirement has a mismatched archive identity".into());
    }
    let outcome_path = checkpoint.with_file_name(recovery_name(&active.checkpoint_hash));
    let bytes =
        read_checkpoint(&outcome_path)?.ok_or("Retirement's original outcome disappeared")?;
    if digest(&bytes) != retired.outcome_hash {
        return Err("Retirement's immutable original outcome changed".into());
    }
    let record: RecoveryRecord = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if active
        .target
        .as_ref()
        .is_some_and(|target| record.target.as_ref() != Some(target))
        || active
            .source
            .as_ref()
            .is_some_and(|source| &record.source != source)
        || record.checkpoint_hash != retired.checkpoint_hash
        || record.source != retired.source
        || record.job != retired.job
        || record.request_identity != retired.request_identity
    {
        return Err("Retirement names a different original recovery outcome".into());
    }
    record.validate_additional_issued_chats(&retired.adopted_chat_descriptors)?;
    let proof = ArchiveProof {
        archive_path: retired.archive_path.clone(),
        checkpoint_sha256: retired.checkpoint_hash.clone(),
        outcome_path,
        outcome_sha256: retired.outcome_hash.clone(),
        retirement_path: path,
        retirement_sha256: active.retirement_hash,
    };
    Ok(Some((retired, proof, record)))
}

/// Inspect only the routing envelope first. It can reject unrelated history,
/// never establish continuity; matching candidates still undergo strict reads.
fn scoped_retirement(
    checkpoint: &Path,
    target: &Path,
    source: Option<&HostSessionProof>,
    expected: Option<&str>,
) -> Result<Option<(Retirement, ArchiveProof, RecoveryRecord)>, String> {
    #[derive(Deserialize)]
    struct Route {
        #[serde(default)]
        target: Option<PathBuf>,
        #[serde(default)]
        source: Option<HostSessionProof>,
        #[serde(default)]
        checkpoint_hash: Option<String>,
        #[serde(default)]
        retirement_hash: Option<String>,
    }
    let route = (|| {
        let bytes = read_checkpoint(&checkpoint.with_file_name(ACTIVE_RECOVERY))?;
        bytes
            .map(|bytes| serde_json::from_slice::<Route>(&bytes).map_err(|e| e.to_string()))
            .transpose()
    })();
    let Some(mut route) = (match route {
        Ok(route) => route,
        Err(_) => return Ok(None),
    }) else {
        return Ok(None);
    };
    if expected.is_some_and(|expected| route.checkpoint_hash.as_deref() != Some(expected)) {
        return Ok(None);
    }
    if route.target.as_deref().is_some_and(|known| known != target)
        || source.is_some_and(|source| route.source.as_ref().is_some_and(|known| known != source))
    {
        return Ok(None);
    }
    // Old v2 pointers lacked routing. A bounded projection of their outcome is
    // still only a routing hint; missing or unparseable history is unrelated.
    if route.target.is_none() || (source.is_some() && route.source.is_none()) {
        let Some(hash) = route
            .checkpoint_hash
            .as_deref()
            .filter(|hash| valid_hash(hash))
        else {
            return Ok(None);
        };
        let projection = read_checkpoint(&checkpoint.with_file_name(recovery_name(hash)))
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice::<Route>(&bytes).ok());
        let Some(projection) = projection else {
            return Ok(None);
        };
        if route.target.is_none() {
            route.target = projection.target;
        }
        if route.source.is_none() {
            route.source = projection.source;
        }
    }
    if route.target.as_deref() != Some(target)
        || source.is_some_and(|source| route.source.as_ref() != Some(source))
    {
        return Ok(None);
    }
    // Do not infer validity from the routing hash fields or caller identity.
    let _ = route.retirement_hash;
    load_retirement(checkpoint, route.checkpoint_hash.as_deref())
}

fn archived_discovery_strict(
    checkpoint: &Path,
    daemon: &Path,
    source: Option<&HostSessionProof>,
) -> Result<Option<AlreadyRecovered>, String> {
    match fs::symlink_metadata(checkpoint) {
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let target = fs::canonicalize(daemon).map_err(|e| e.to_string())?;
    let Some((retired, proof, record)) = scoped_retirement(checkpoint, &target, source, None)?
    else {
        return Ok(None);
    };
    validate_settled_identity(&record, checkpoint, &target)?;
    Ok(Some(recovered_data(
        record,
        checkpoint,
        target,
        retired.adopted_chat_descriptors,
        proof.outcome_sha256,
    )))
}

/// Optional proof metadata, consulted only after ordinary holder adoption.
/// This never directs restoration, adoption, paused panes or provider starts.
/// Broken, unsupported or unrelated history must never prevent normal startup.
#[cfg(any(feature = "host", test))]
pub fn archived_discovery(
    checkpoint: &Path,
    daemon: &Path,
) -> Result<Option<AlreadyRecovered>, String> {
    Ok(
        archived_discovery_strict(checkpoint, daemon, None).unwrap_or_else(|error| {
            tracing::warn!(%error, "optional historical update evidence is unavailable");
            None
        }),
    )
}

/// Strict matching history, without ever converting archive data into startup
/// recipes, paused panes, preserve-ID exceptions or restoration authority.
pub(crate) fn pending_retirement(
    checkpoint: &Path,
    daemon: &Path,
) -> Result<Option<AlreadyRecovered>, String> {
    match classify_retained_prepared(checkpoint, daemon)? {
        Some(recovered) => Ok(Some(recovered)),
        None => archived_discovery_strict(checkpoint, daemon, None),
    }
}

/// Scope lookup to the caller's actual original census before strict history
/// decoding. A completed A update cannot block verification of unrelated B.
pub(crate) fn pending_retirement_for_source(
    checkpoint: &Path,
    daemon: &Path,
    source: &HostSessionProof,
) -> Result<Option<AlreadyRecovered>, String> {
    if let Some(bytes) = read_checkpoint(checkpoint)? {
        #[derive(Deserialize)]
        struct Header {
            phase: String,
            #[serde(default)]
            proof: Option<HostSessionProof>,
        }
        let header: Header = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if header.phase != "Prepared" {
            return Ok(None);
        }
        let proof = header
            .proof
            .ok_or("Prepared checkpoint has no source proof")?;
        if &proof != source {
            return Ok(None);
        }
        return classify_retained_prepared(checkpoint, daemon);
    }
    archived_discovery_strict(checkpoint, daemon, Some(source))
}

/// Strict actual archive evidence. A durable routing pointer may precede the
/// move, so its presence alone never establishes completed archival.
pub fn archived_proof(checkpoint: &Path, daemon: &Path) -> Result<Option<ArchiveProof>, String> {
    let target = fs::canonicalize(daemon).map_err(|e| e.to_string())?;
    let Some((_, proof, record)) = scoped_retirement(checkpoint, &target, None, None)? else {
        return Ok(None);
    };
    let bytes = read_checkpoint(&proof.archive_path)?
        .ok_or("The routed recovery has no actual checkpoint archive")?;
    if digest(&bytes) != proof.checkpoint_sha256 {
        return Err("The actual checkpoint archive differs from its routing evidence".into());
    }
    validate_settled_record(&record, &bytes, checkpoint, &target)?;
    Ok(Some(proof))
}

fn recovered_data(
    record: RecoveryRecord,
    checkpoint: &Path,
    target: PathBuf,
    additional: Vec<Session>,
    outcome_hash: String,
) -> AlreadyRecovered {
    let witness = SettledWitness {
        checkpoint_hash: record.checkpoint_hash.clone(),
        record_hash: outcome_hash.clone(),
        target: target.clone(),
        target_hash: record.target_hash.clone(),
        successor: record
            .successor
            .clone()
            .expect("validated settled successor"),
        adopted_chats: record.adopted_chat_descriptors.clone(),
        adopted_terminals: record.adopted_terminal_descriptors.clone(),
    };
    let mut descriptors = record.adopted_chat_descriptors;
    descriptors.extend(additional);
    AlreadyRecovered {
        witness,
        source: record.source,
        census: record.census,
        paused_terminals: record.paused_terminals,
        paused_chats: record.paused_chats,
        adopted_chat_descriptors: descriptors,
        #[cfg(any(test, feature = "test-support"))]
        adopted_terminals: record.adopted_terminals,
        checkpoint_path: checkpoint.to_owned(),
        daemon_binary: target,
        checkpoint_hash: record.checkpoint_hash,
        outcome_hash,
    }
}

impl AlreadyRecovered {
    fn normalized_chat(&self, chat: &Session) -> Session {
        let mut normalized = chat.clone();
        if let Some(after) = normalized.terminal.as_mut()
            && let Some(before) = self
                .census
                .chats
                .iter()
                .find(|known| known.id == chat.id)
                .and_then(|known| known.terminal.as_ref())
            && self.witness.adopted_chats.iter().any(|issued| {
                issued.id == chat.id
                    && issued
                        .terminal
                        .as_ref()
                        .is_some_and(|terminal| recovered_terminal(terminal, after))
            })
            && self.witness.adopted_terminals.iter().any(|terminal| {
                recovered_terminal(&terminal.session, after)
                    && verified_console_terminal(
                        &self.census,
                        before,
                        terminal,
                        &self.daemon_binary,
                    )
            })
        {
            after.launch.program.clone_from(&before.launch.program);
        }
        normalized
    }
    pub(crate) fn checkpoint_hash(&self) -> &str {
        &self.checkpoint_hash
    }

    /// Validate fresh authenticated owners and the complete current live/paused
    /// partition without changing the checkpoint, original outcome or metadata.
    /// This mechanical check does not establish RootCensus Usable.
    pub(crate) fn validate_current_inventory(
        &self,
        serving: &HostSessionProof,
        live_terminals: &[crate::terminal_ipc::TerminalDescriptor],
        live_chats: &[Session],
        paused_terminals: &[TerminalId],
        paused_chats: &[TerminalId],
    ) -> Result<(), String> {
        if !serving.is_complete() {
            return Err("Recovery archival requires the serving daemon proof".into());
        }
        let current = match classify_retained_prepared(&self.checkpoint_path, &self.daemon_binary)?
        {
            Some(current) => current,
            None => archived_discovery_strict(&self.checkpoint_path, &self.daemon_binary, None)?
                .ok_or("Recovery archival no longer has its exact settled record")?,
        };
        if current.checkpoint_hash != self.checkpoint_hash
            || current.source != self.source
            || current.census != self.census
            || current.outcome_hash != self.outcome_hash
        {
            return Err("The settled recovery changed before archival".into());
        }
        let terminal_ids = self.census.terminal_ids();
        let chat_ids: BTreeSet<_> = self.census.chats.iter().map(|chat| chat.id).collect();
        let mut terminals = BTreeSet::new();
        let mut chats = BTreeSet::new();
        for actual in live_terminals
            .iter()
            .filter(|item| terminal_ids.contains(&item.session.id))
        {
            if current.paused_terminals.contains(&actual.session.id)
                || !terminals.insert(actual.session.id)
                || !verified_terminal(&self.census, actual, serving, &self.daemon_binary)
            {
                return Err(
                    "Recovery archival found a changed or duplicate original terminal owner".into(),
                );
            }
        }
        for actual in live_chats.iter().filter(|item| chat_ids.contains(&item.id)) {
            if current.paused_chats.contains(&actual.id)
                || !chats.insert(actual.id)
                || !self.census.chats.iter().any(|known| {
                    current_recovered_chat(known, &self.normalized_chat(actual), serving)
                })
            {
                return Err(
                    "Recovery archival found a changed or duplicate original conversation owner"
                        .into(),
                );
            }
        }
        fn accounted(
            expected: &BTreeSet<TerminalId>,
            live: &BTreeSet<TerminalId>,
            paused: &[TerminalId],
        ) -> bool {
            let missing: BTreeSet<_> = paused.iter().copied().collect();
            missing.len() == paused.len()
                && live.is_disjoint(&missing)
                && live.union(&missing).copied().collect::<BTreeSet<_>>() == *expected
        }
        if !accounted(&terminal_ids, &terminals, paused_terminals)
            || !accounted(&chat_ids, &chats, paused_chats)
        {
            return Err("Recovery archival requires every original owner to be currently authenticated or durably paused, without overlap".into());
        }
        let sidecar = self
            .checkpoint_path
            .with_file_name(recovery_name(&self.checkpoint_hash));
        let bytes = read_checkpoint(&sidecar)?
            .ok_or("Settled recovery record disappeared before archival")?;
        if digest(&bytes) != self.outcome_hash {
            return Err("Original settled outcome changed before completion".into());
        }
        let record: RecoveryRecord = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let checkpoint = read_settled_checkpoint(
            &self.checkpoint_path,
            &self.checkpoint_hash,
            &self.outcome_hash,
        )?;
        validate_settled_record(
            &record,
            &checkpoint,
            &self.checkpoint_path,
            &self.daemon_binary,
        )?;
        Ok(())
    }

    /// Complete archival only after the caller's fresh RootCensus reports
    /// Usable, following ordinary authenticated adoption, paused history
    /// restoration, and Ready. The live/paused counts below are mechanical
    /// safety checks; they do not establish that the user's recovery succeeded.
    /// Unrelated owners created since the original recovery are left alone.
    #[cfg(any(test, feature = "test-support"))]
    pub fn archive_completed(
        &self,
        serving: &HostSessionProof,
        live_terminals: &[crate::terminal_ipc::TerminalDescriptor],
        live_chats: &[Session],
        paused_terminals: &[TerminalId],
        paused_chats: &[TerminalId],
    ) -> Result<(), String> {
        let RetirementAcquisition::Acquired(guard) =
            RetirementGuard::acquire(&self.checkpoint_path)?
        else {
            return Err("Recovery evidence is busy; no completion was recorded".into());
        };
        self.archive_completed_with(
            &guard,
            serving,
            live_terminals,
            live_chats,
            paused_terminals,
            paused_chats,
        )
    }

    pub(crate) fn archive_completed_with(
        &self,
        guard: &RetirementGuard,
        serving: &HostSessionProof,
        live_terminals: &[crate::terminal_ipc::TerminalDescriptor],
        live_chats: &[Session],
        paused_terminals: &[TerminalId],
        paused_chats: &[TerminalId],
    ) -> Result<(), String> {
        guard.matches(&self.checkpoint_path)?;
        self.validate_current_inventory(
            serving,
            live_terminals,
            live_chats,
            paused_terminals,
            paused_chats,
        )?;
        let current = pending_retirement(&self.checkpoint_path, &self.daemon_binary)?
            .ok_or("Recovery completion no longer has its exact settled record")?;
        if current.checkpoint_hash != self.checkpoint_hash
            || current.outcome_hash != self.outcome_hash
        {
            return Err("The settled recovery changed before archival".into());
        }
        let sidecar = self
            .checkpoint_path
            .with_file_name(recovery_name(&self.checkpoint_hash));
        let bytes = read_checkpoint(&sidecar)?
            .ok_or("Settled recovery record disappeared before archival")?;
        if digest(&bytes) != self.outcome_hash {
            return Err("Original settled outcome changed before completion".into());
        }
        let record: RecoveryRecord = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let checkpoint = read_settled_checkpoint(
            &self.checkpoint_path,
            &self.checkpoint_hash,
            &self.outcome_hash,
        )?;
        validate_settled_record(
            &record,
            &checkpoint,
            &self.checkpoint_path,
            &self.daemon_binary,
        )?;
        let chats: BTreeSet<_> = live_chats.iter().map(|chat| chat.id).collect();
        let mut additional = current.adopted_chat_descriptors;
        additional.retain(|chat| !record.adopted_chat_descriptors.contains(chat));
        for chat in live_chats.iter().filter(|chat| {
            chats.contains(&chat.id)
                && record.census.chats.iter().any(|known| {
                    captured_chat_alias(
                        &record.census,
                        known,
                        &record.normalized_issued_chat(chat),
                        serving,
                    )
                })
        }) {
            if !record.adopted_chat_descriptors.contains(chat) && !additional.contains(chat) {
                additional.push(chat.clone());
            }
        }
        record.validate_additional_issued_chats(&additional)?;
        commit_archive(guard, &self.checkpoint_path, &record, &bytes, additional)
    }
}

/// Serializes retirement helpers only. Daemon checkpoint writers never take
/// this guard, so callers may hold it across read-only socket proofs without
/// introducing a daemon/control-lock cycle. It conveys no restore authority.
#[derive(Debug)]
pub(crate) struct RetirementGuard {
    lock: File,
    checkpoint: PathBuf,
}
#[derive(Debug)]
pub(crate) enum RetirementAcquisition {
    Acquired(RetirementGuard),
    Busy,
}

impl RetirementGuard {
    pub(crate) fn acquire(checkpoint: &Path) -> Result<RetirementAcquisition, String> {
        Self::acquire_for(checkpoint, std::time::Duration::from_secs(2))
    }

    fn acquire_for(
        checkpoint: &Path,
        timeout: std::time::Duration,
    ) -> Result<RetirementAcquisition, String> {
        let directory = checkpoint
            .parent()
            .ok_or("Recovery storage has no directory")?;
        validate_private_directory(directory)?;
        let directory = fs::canonicalize(directory).map_err(|e| e.to_string())?;
        let name = checkpoint
            .file_name()
            .ok_or("Recovery checkpoint has no name")?;
        let path = directory.join(".daemon-upgrade.retirement.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|e| e.to_string())?;
        let metadata = lock.metadata().map_err(|e| e.to_string())?;
        if !private_single_file(&metadata) {
            return Err("Retirement lock is not a private regular file".into());
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                return Err(error.to_string());
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(RetirementAcquisition::Busy);
            }
            std::thread::sleep(remaining.min(std::time::Duration::from_millis(20)));
        }
        let current = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
            return Err("Retirement lock changed while it was acquired".into());
        }
        sweep_archive_temporaries(&directory);
        Ok(RetirementAcquisition::Acquired(Self {
            lock,
            checkpoint: directory.join(name),
        }))
    }
    fn matches(&self, checkpoint: &Path) -> Result<(), String> {
        let directory = fs::canonicalize(
            checkpoint
                .parent()
                .ok_or("Recovery storage has no directory")?,
        )
        .map_err(|e| e.to_string())?;
        if directory.join(
            checkpoint
                .file_name()
                .ok_or("Recovery checkpoint has no name")?,
        ) != self.checkpoint
        {
            return Err("Retirement guard belongs to different recovery storage".into());
        }
        let held = self.lock.metadata().map_err(|e| e.to_string())?;
        let current = fs::symlink_metadata(directory.join(".daemon-upgrade.retirement.lock"))
            .map_err(|e| e.to_string())?;
        if !private_single_file(&held) || current.dev() != held.dev() || current.ino() != held.ino()
        {
            return Err("Retirement lock changed during verification".into());
        }
        Ok(())
    }
}

fn private_single_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
        && metadata.uid() == crate::daemon::current_uid()
        && metadata.mode() & 0o077 == 0
        && metadata.nlink() == 1
}

// Legacy writers did not lock temporary OFDs. Prove no same-UID process has
// the inode open before unlinking, rather than guessing from age or a lock.
#[cfg(test)]
fn unopened_temporary(metadata: &fs::Metadata, own_fd: i32) -> bool {
    unopened_temporary_at(metadata, own_fd, Path::new("/proc"))
}

fn unopened_temporary_at(metadata: &fs::Metadata, own_fd: i32, proc_root: &Path) -> bool {
    let Ok(processes) = fs::read_dir(proc_root) else {
        return false;
    };
    for entry in processes {
        let Ok(entry) = entry else {
            return false;
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let process = entry.path();
        // /proc/PID ownership changes for nondumpable processes. Only the
        // kernel status Uid fields route same-user processes; uncertainty
        // means potentially open, so old temporary evidence is retained.
        let status = match File::open(process.join("status")) {
            Ok(file) => {
                let mut bytes = Vec::new();
                if file.take(64 * 1024 + 1).read_to_end(&mut bytes).is_err()
                    || bytes.len() > 64 * 1024
                {
                    return false;
                }
                bytes
            }
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => {
                continue;
            }
            Err(_) => return false,
        };
        let Some(uids) = status_uids(&status) else {
            return false;
        };
        if !uids.contains(&crate::daemon::current_uid()) {
            continue;
        }
        let descriptors = match fs::read_dir(process.join("fd")) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return false,
        };
        for descriptor in descriptors {
            let Ok(descriptor) = descriptor else {
                return false;
            };
            if pid == std::process::id()
                && descriptor
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<i32>().ok())
                    == Some(own_fd)
            {
                continue;
            }
            match fs::metadata(descriptor.path()) {
                Ok(m) if m.dev() == metadata.dev() && m.ino() == metadata.ino() => return false,
                Ok(_) => {}
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => {}
                Err(_) => return false,
            }
        }
    }
    true
}

fn status_uids(bytes: &[u8]) -> Option<[u32; 4]> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut fields = text
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_ascii_whitespace();
    let uids = [
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
    ];
    fields.next().is_none().then_some(uids)
}

fn sweep_archive_temporaries(directory: &Path) {
    sweep_archive_temporaries_at(directory, Path::new("/proc"));
}

fn sweep_archive_temporaries_at(directory: &Path, proc_root: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut candidates = 0;
    let mut removed = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // tempfile's six random ASCII characters only. In particular this
        // excludes .recovery-capture-*, every checkpoint custody path, and all
        // content-addressed permanent evidence.
        let suffix = name
            .strip_prefix(".recovery-")
            .or_else(|| name.strip_prefix(".retirement-"));
        if !suffix.is_some_and(|s| s.len() == 6 && s.bytes().all(|b| b.is_ascii_alphanumeric())) {
            continue;
        }
        candidates += 1;
        if candidates > 64 {
            break;
        }
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(entry.path())
        {
            Ok(f) => f,
            Err(_) => continue,
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        if !private_single_file(&metadata)
            || metadata.len() > LIMIT
            || !unopened_temporary_at(&metadata, file.as_raw_fd(), proc_root)
        {
            continue;
        }
        let Ok(current) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if current.dev() == metadata.dev()
            && current.ino() == metadata.ino()
            && private_single_file(&current)
        {
            match fs::remove_file(entry.path()) {
                Ok(()) => removed = true,
                Err(error) => {
                    tracing::warn!(%error,"orphan retirement temporary could not be removed")
                }
            }
        }
    }
    if removed && let Err(error) = sync_directory(directory) {
        tracing::warn!(%error,"orphan retirement cleanup could not be synced");
    }
}

fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both paths are NUL-terminated and outlive the call.
    #[cfg(target_os = "linux")]
    let renamed = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    // SAFETY: as above; RENAME_EXCL is Darwin's no-replace rename.
    #[cfg(target_os = "macos")]
    let renamed = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if renamed == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! { static ARCHIVE_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&str)>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(any(test, feature = "test-support"))]
pub fn archive_test_hook(stage: &str) {
    ARCHIVE_TEST_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(stage);
        }
    });
}

/// Once renamed, this is checkpoint custody, never orphan scratch space.
/// Any failed restore leaves the exact path available for inspection.
fn archive_claimed_checkpoint(
    checkpoint: &Path,
    archive: &Path,
    expected: &[u8],
) -> Result<(), String> {
    let custody = checkpoint.with_file_name(format!(
        ".checkpoint-retirement.{}.{}.json",
        digest(expected),
        uuid::Uuid::new_v4()
    ));
    #[cfg(any(test, feature = "test-support"))]
    archive_test_hook("before-claim");
    match rename_noreplace(checkpoint, &custody) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return if read_checkpoint(archive)?.as_deref() == Some(expected) {
                Ok(())
            } else {
                Err("Checkpoint disappeared without its exact verified archive".into())
            };
        }
        Err(error) => return Err(error.to_string()),
    }
    let directory = checkpoint
        .parent()
        .ok_or("Checkpoint custody has no directory")?;
    // The custody entry must be durable before a crash can remove the only
    // canonical name. It is intentionally never swept or replayed.
    let result = (|| {
        sync_directory(directory)?;
        #[cfg(any(test, feature = "test-support"))]
        archive_test_hook("after-claim");
        let bytes = read_checkpoint(&custody)?.ok_or("Claimed checkpoint disappeared")?;
        if bytes != expected {
            return Err("Canonical checkpoint changed before the archive claim".into());
        }
        super::handoff::validate_checkpoint_bytes(&bytes)?;
        match rename_noreplace(&custody, archive) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if read_checkpoint(archive)?.as_deref() != Some(expected) {
                    return Err("Archive destination contains different evidence".into());
                }
                // Both copies are fully verified and identical; only the
                // duplicate claimed inode can now be discarded.
                fs::remove_file(&custody).map_err(|e| e.to_string())?;
            }
            Err(error) => return Err(error.to_string()),
        }
        sync_directory(directory)
    })();
    if let Err(error) = result {
        if fs::symlink_metadata(&custody).is_ok() {
            match rename_noreplace(&custody, checkpoint) {
                Ok(()) => {
                    sync_directory(directory)?;
                    return Err(format!(
                        "{error}; claimed bytes were safely restored to {}",
                        checkpoint.display()
                    ));
                }
                Err(restore) => {
                    return Err(format!(
                        "{error}; current checkpoint was kept and claimed evidence remains at {} (restore refused: {restore})",
                        custody.display()
                    ));
                }
            }
        }
        return Err(error);
    }
    Ok(())
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn atomic_metadata(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let directory = path.parent().ok_or("Recovery metadata has no directory")?;
    validate_private_directory(directory)?;
    // Refuse unsafe existing entries rather than replacing a symlink or a
    // multiply linked file, even though rename itself would not follow it.
    read_checkpoint(path)?;
    encoded_size(value, LIMIT as usize)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".recovery-")
        .permissions(fs::Permissions::from_mode(0o600))
        .tempfile_in(directory)
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(temporary.as_file_mut(), value).map_err(|e| e.to_string())?;
    temporary.as_file().sync_all().map_err(|e| e.to_string())?;
    temporary.persist(path).map_err(|e| e.to_string())?;
    sync_directory(directory)
}

fn sync_directory(directory: &Path) -> Result<(), String> {
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())
}

fn publish_active_retirement(checkpoint: &Path, active: &ActiveRecovery) -> Result<(), String> {
    let path = checkpoint.with_file_name(ACTIVE_RECOVERY);
    let usable = match read_checkpoint(&path) {
        Ok(None) => true,
        Ok(Some(bytes)) => serde_json::from_slice::<ActiveRecovery>(&bytes).is_ok_and(|pointer| {
            pointer.version == 2
                && valid_hash(&pointer.checkpoint_hash)
                && valid_hash(&pointer.retirement_hash)
        }),
        Err(_) => false,
    };
    let quarantine = if usable {
        None
    } else {
        let retained =
            checkpoint.with_file_name(format!(".retirement-pointer.{}.json", uuid::Uuid::new_v4()));
        match rename_noreplace(&path, &retained) {
            Ok(()) => {
                sync_directory(
                    checkpoint
                        .parent()
                        .ok_or("Retirement pointer has no directory")?,
                )?;
                Some(retained)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
        }
    };
    atomic_metadata(&path, active).map_err(|error| match quarantine {
        Some(ref path) => format!(
            "{error}; previous pointer evidence was retained at {}",
            path.display()
        ),
        None => error,
    })
}

fn commit_archive(
    guard: &RetirementGuard,
    checkpoint: &Path,
    record: &RecoveryRecord,
    outcome: &[u8],
    additional: Vec<Session>,
) -> Result<(), String> {
    guard.matches(checkpoint)?;
    let archive = archive_path(checkpoint, &record.checkpoint_hash);
    let current = read_checkpoint(checkpoint)?;
    let prior = if current.is_some() {
        scoped_retirement(checkpoint,record.target.as_deref().ok_or("Settled target is missing")?,Some(&record.source),Some(&record.checkpoint_hash)).unwrap_or_else(|error| {tracing::warn!(%error,"unusable prior retirement pointer was ignored for an independently verified canonical checkpoint");None})
    } else {
        load_retirement(checkpoint, Some(&record.checkpoint_hash))?
    };
    if let Some(bytes) = &current {
        if digest(bytes) != record.checkpoint_hash {
            return Err(
                "Prepared checkpoint changed before archival; all evidence was preserved".into(),
            );
        }
        if fs::symlink_metadata(&archive).is_ok()
            && read_checkpoint(&archive)?.as_ref() != Some(bytes)
        {
            return Err("Prepared archive destination contains different evidence; canonical bytes were kept".into());
        }
    } else {
        let (_, proof, _) = prior
            .as_ref()
            .ok_or("Prepared checkpoint has no matching immutable retirement")?;
        let bytes = read_checkpoint(&archive)?
            .ok_or("Prepared checkpoint and its archive are both missing")?;
        if proof.outcome_sha256 != digest(outcome) || digest(&bytes) != record.checkpoint_hash {
            return Err("Prepared archive does not match its immutable original outcome".into());
        }
    }
    let previous = prior
        .as_ref()
        .map(|(_, proof, _)| proof.retirement_sha256.clone());
    let same = prior
        .as_ref()
        .is_some_and(|(retired, _, _)| retired.adopted_chat_descriptors == additional);
    let retirement_hash = if same {
        previous.unwrap()
    } else {
        let retired = Retirement {
            version: 1,
            checkpoint_hash: record.checkpoint_hash.clone(),
            outcome_hash: digest(outcome),
            source: record.source.clone(),
            job: record.job.clone(),
            request_identity: record.request_identity.clone(),
            archive_path: archive.clone(),
            adopted_chat_descriptors: additional,
            previous,
        };
        encoded_size(&retired, LIMIT as usize)?;
        let bytes = serde_json::to_vec(&retired).map_err(|e| e.to_string())?;
        let hash = digest(&bytes);
        let path = retirement_path(checkpoint, &hash);
        match read_checkpoint(&path)? {
            Some(existing) if existing == bytes => {}
            Some(_) => {
                return Err("Immutable retirement destination contains different bytes".into());
            }
            None => {
                let directory = path.parent().ok_or("Retirement has no directory")?;
                let mut file = tempfile::Builder::new()
                    .prefix(".retirement-")
                    .permissions(fs::Permissions::from_mode(0o600))
                    .tempfile_in(directory)
                    .map_err(|e| e.to_string())?;
                file.write_all(&bytes)
                    .and_then(|()| file.as_file().sync_all())
                    .map_err(|e| e.to_string())?;
                if let Err(error) = file.persist_noclobber(&path)
                    && read_checkpoint(&path)?.as_deref() != Some(&bytes)
                {
                    return Err(error.to_string());
                }
                sync_directory(directory)?;
            }
        }
        hash
    };
    // This mutable pointer is durable routing intent, not completion proof.
    // Keeping it before the claim makes a crash discoverable; archived_proof
    // independently requires the actual exact archive before proving a move.
    // Original outcomes and content-addressed generations remain immutable.
    if read_checkpoint(&checkpoint.with_file_name(recovery_name(&record.checkpoint_hash)))?
        .as_deref()
        != Some(outcome)
    {
        return Err("Original settled outcome changed before archival".into());
    }
    publish_active_retirement(
        checkpoint,
        &ActiveRecovery {
            version: 2,
            checkpoint_hash: record.checkpoint_hash.clone(),
            retirement_hash,
            target: record.target.clone(),
            source: Some(record.source.clone()),
        },
    )?;
    if let Some(bytes) = current {
        archive_claimed_checkpoint(checkpoint, &archive, &bytes)?;
    }
    sync_directory(
        checkpoint
            .parent()
            .ok_or("Recovery storage has no directory")?,
    )
}

fn validate_private_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err("Recovery evidence directory is not privately owned".into());
    }
    Ok(())
}

fn validate_reviewed(
    reviewed: &DaemonImpact,
    source: &HostSessionProof,
    identity: &str,
) -> Result<(), String> {
    let owners: BTreeSet<_> = reviewed
        .sessions
        .iter()
        .map(|item| (item.kind, item.id))
        .collect();
    if !source.is_complete()
        || reviewed.capability.proof != *source
        || owners.len() != reviewed.sessions.len()
        || digest(&serde_json::to_vec(reviewed).map_err(|e| e.to_string())?) != identity
    {
        return Err(
            "Recovery record changed the original reviewed source or request identity".into(),
        );
    }
    Ok(())
}

impl RecoveryRecord {
    fn normalized_issued_chat(&self, descriptor: &Session) -> Session {
        let mut normalized = descriptor.clone();
        let Some(after) = normalized.terminal.as_mut() else {
            return normalized;
        };
        let Some(before) = self
            .census
            .chats
            .iter()
            .find(|known| known.id == descriptor.id)
            .and_then(|known| known.terminal.as_ref())
        else {
            return normalized;
        };
        if recovered_terminal(before, after) {
            return normalized;
        }
        // The original settled descriptor already proved this exact console
        // replacement recipe. Later aliases can rotate its serving proof, but
        // cannot choose another executable, allocation, arguments or account.
        if self.adopted_chat_descriptors.iter().any(|issued| {
            issued.id == descriptor.id
                && issued
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| recovered_terminal(terminal, after))
        }) && self.target.as_deref().is_some_and(|target| {
            self.adopted_terminal_descriptors.iter().any(|terminal| {
                recovered_terminal(&terminal.session, after)
                    && verified_console_terminal(&self.census, before, terminal, target)
            })
        }) {
            after.launch.program.clone_from(&before.launch.program);
        }
        normalized
    }

    fn validate_issued_chats(&self) -> Result<(), String> {
        self.validate_additional_issued_chats(&[])
    }
    fn validate_additional_issued_chats(&self, additional: &[Session]) -> Result<(), String> {
        let mut issued = BTreeSet::new();
        for (descriptor, later) in self
            .adopted_chat_descriptors
            .iter()
            .map(|chat| (chat, false))
            .chain(additional.iter().map(|chat| (chat, true)))
        {
            let proof = descriptor
                .proof
                .as_ref()
                .ok_or("Recovered chat alias has no issued host proof")?;
            let normalized = self.normalized_issued_chat(descriptor);
            if !proof.is_complete()
                || (!later && self.successor.as_ref() != Some(proof))
                || proof.boot_id != self.source.boot_id
                || !self.adopted_chats.contains(&descriptor.id)
                || !self
                    .census
                    .chats
                    .iter()
                    .any(|known| recovered_chat(known, &normalized, proof))
                || later
                    && !self
                        .census
                        .chats
                        .iter()
                        .any(|known| captured_chat_alias(&self.census, known, &normalized, proof))
                || !issued.insert((
                    descriptor.id,
                    digest(&serde_json::to_vec(proof).map_err(|e| e.to_string())?),
                ))
            {
                return Err(
                    "Recovery chat descriptor changed its original owner or issued host proof"
                        .into(),
                );
            }
        }
        if self.adopted_chats.iter().any(|id| {
            !self
                .adopted_chat_descriptors
                .iter()
                .any(|chat| chat.id == *id)
        }) {
            return Err(
                "Recovery lacks the exact issued descriptor for an adopted conversation".into(),
            );
        }
        Ok(())
    }

    fn validate_census(&self) -> Result<(), String> {
        fn complete(
            expected: BTreeSet<TerminalId>,
            adopted: &[TerminalId],
            paused: &[TerminalId],
        ) -> bool {
            let live: BTreeSet<_> = adopted.iter().copied().collect();
            let absent: BTreeSet<_> = paused.iter().copied().collect();
            live.len() == adopted.len()
                && absent.len() == paused.len()
                && live.is_disjoint(&absent)
                && live.union(&absent).copied().collect::<BTreeSet<_>>() == expected
        }
        if !complete(
            self.census.terminal_ids(),
            &self.adopted_terminals,
            &self.paused_terminals,
        ) || !complete(
            self.census.chats.iter().map(|source| source.id).collect(),
            &self.adopted_chats,
            &self.paused_chats,
        ) {
            return Err("Recovery did not account for every exact original pane owner without adding another".into());
        }
        Ok(())
    }
}

#[cfg(feature = "host")]
pub fn take_for_host() -> Result<Option<RecoveryRecord>, String> {
    use std::io::{Seek, SeekFrom};
    let Some(mut inherited) = inherited()
        .map_err(|e| e.to_string())?
        .lock()
        .map_err(|_| "Update recovery descriptors are unavailable")?
        .take()
    else {
        return Ok(None);
    };
    validate_sealed_descriptor(inherited.receipt.as_raw_fd()).map_err(|e| e.to_string())?;
    inherited
        .receipt
        .seek(SeekFrom::Start(0))
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    (&mut inherited.receipt)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > LIMIT {
        return Err("Update recovery receipt exceeds its byte limit".into());
    }
    let authority: Authority = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    validate_host_context(&authority)?;
    if authority.version != 1
        || fs::canonicalize(&authority.target).map_err(|e| e.to_string())?
            != fs::canonicalize(std::env::current_exe().map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?
        || executable_hash(&authority.target)? != authority.target_hash
    {
        return Err("Update recovery receipt names a different daemon executable".into());
    }
    for (file, path) in [
        (&inherited.worker, &authority.worker_path),
        (&inherited.fence, &authority.fence_path),
        (&inherited.daemon, &authority.daemon_path),
    ] {
        validate_lock(file, path).map_err(|e| e.to_string())?;
    }
    validate_lock_mode(&inherited.legacy, &authority.legacy_path, "READ")
        .map_err(|e| e.to_string())?;
    let paths = crate::daemon::RuntimePaths::discover().map_err(|e| e.to_string())?;
    if authority.daemon_path != paths.lock {
        return Err("Recovery inherited a different daemon namespace".into());
    }
    let checkpoint =
        read_checkpoint(&authority.checkpoint_path)?.ok_or("Recovery checkpoint disappeared")?;
    super::handoff::validate_checkpoint_bytes(&checkpoint)?;
    if validate_checkpoint(&authority, &checkpoint)? != "Prepared"
        || digest(&checkpoint) != authority.checkpoint_hash
    {
        return Err(
            "Recovery receipt does not identify these exact Prepared checkpoint bytes".into(),
        );
    }
    let store = super::handoff::Store::discover()?;
    if store.path != authority.checkpoint_path {
        return Err("Recovery checkpoint belongs to another application".into());
    }
    let record = RecoveryRecord {
        version: 2,
        origin: authority.origin,
        job: authority.job,
        request_identity: authority.request_identity,
        job_request_hash: authority.job_request_hash,
        reviewed: authority.reviewed,
        windows: authority.windows,
        source: authority.source,
        checkpoint_hash: authority.checkpoint_hash,
        census: authority.census,
        adopted_terminals: Vec::new(),
        adopted_chats: Vec::new(),
        adopted_chat_descriptors: Vec::new(),
        adopted_terminal_descriptors: Vec::new(),
        paused_terminals: Vec::new(),
        paused_chats: Vec::new(),
        settled: false,
        successor: None,
        target_hash: authority.target_hash,
        target: Some(authority.target),
        authority: Some(Continuity {
            inherited,
            paths: [
                authority.worker_path,
                authority.fence_path,
                authority.legacy_path,
                authority.daemon_path,
            ],
        }),
    };
    record.claim()?;
    Ok(Some(record))
}

#[cfg(feature = "host")]
fn validate_host_context(authority: &Authority) -> Result<(), String> {
    let layout = crate::update::locate(&std::env::current_exe().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let state = layout.prefix.join(".update");
    let metadata = fs::symlink_metadata(&state).map_err(|e| e.to_string())?;
    if !metadata.is_dir()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err("Recovery updater state is not privately owned".into());
    }
    let (fence, legacy) = crate::instance::update_recovery_fences().map_err(|e| e.to_string())?;
    if authority.worker_path != state.join("lock")
        || authority.fence_path != fence
        || authority.legacy_path != legacy
    {
        return Err(
            "Recovery receipt does not hold this installation's actual update leases".into(),
        );
    }
    let job_bytes =
        read_checkpoint(&state.join("status.json"))?.ok_or("Recovery has no current update job")?;
    let job: crate::update::UpdateJob =
        serde_json::from_slice(&job_bytes).map_err(|e| e.to_string())?;
    if job.id != authority.job
        || job.version != crate::version()
        || !matches!(
            job.phase,
            crate::update::UpdatePhase::ClosingWindows
                | crate::update::UpdatePhase::Installing
                | crate::update::UpdatePhase::Failed
        )
    {
        return Err("Recovery receipt does not belong to this active update's preparation".into());
    }
    let request = read_checkpoint(&state.join(format!("{}.request", authority.job)))?
        .ok_or("Recovery update request disappeared")?;
    if digest(&request) != authority.job_request_hash {
        return Err("The recovery update request changed after preparation".into());
    }
    validate_request_origin(authority, &request)?;
    Ok(())
}

fn validate_request_origin(authority: &Authority, request: &[u8]) -> Result<(), String> {
    match (&authority.origin, &authority.reviewed) {
        (Origin::AcknowledgedPrepare, Some(reviewed)) => {
            validate_reviewed(reviewed, &authority.source, &authority.request_identity)?;
        }
        (Origin::ExplicitRecovery { checkpoint }, None) => {
            let request: ManualRequest =
                serde_json::from_slice(request).map_err(|e| e.to_string())?;
            if request.version != 1
                || request.job != authority.job
                || request.checkpoint != *checkpoint
                || request.checkpoint != authority.checkpoint_hash
                || request.source != authority.source
                || request.target != authority.target
                || request.target_hash != authority.target_hash
                || request.census_hash
                    != digest(&serde_json::to_vec(&authority.census).map_err(|e| e.to_string())?)
                || request.windows != authority.windows
                || digest(&serde_json::to_vec(&request).map_err(|e| e.to_string())?)
                    != authority.request_identity
            {
                return Err("Explicit recovery does not match its exact authorized checkpoint, request and target".into());
            }
        }
        _ => {
            return Err(
                "Recovery origin does not match its acknowledged or explicit authority".into(),
            );
        }
    }
    Ok(())
}

#[cfg(feature = "host")]
impl RecoveryRecord {
    fn name(&self) -> String {
        recovery_name(&self.checkpoint_hash)
    }
    pub fn validate_continuity(&self) -> Result<(), String> {
        let held = self
            .authority
            .as_ref()
            .ok_or("Recovery no longer owns its continuous authority")?;
        for (file, path) in [
            (&held.inherited.worker, &held.paths[0]),
            (&held.inherited.fence, &held.paths[1]),
            (&held.inherited.daemon, &held.paths[3]),
        ] {
            validate_lock(file, path).map_err(|e| e.to_string())?;
        }
        validate_lock_mode(&held.inherited.legacy, &held.paths[2], "READ")
            .map_err(|e| e.to_string())
    }
    pub(crate) fn save(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > LIMIT {
            return Err("Recovery evidence exceeds its byte limit".into());
        }
        super::handoff::Store::discover()?.write_named(&self.name(), &bytes)
    }
    fn claim(&self) -> Result<(), String> {
        let store = super::handoff::Store::discover()?;
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > LIMIT {
            return Err("Recovery evidence exceeds its byte limit".into());
        }
        // Claim before adoption, atomically. A crash may leave an incomplete
        // claim requiring attention; it must never leave a reusable receipt.
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(store.path.with_file_name(self.name()))
            .map_err(|error| format!("This recovery receipt could not be claimed; existing recovery evidence was preserved: {error}"))?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        File::open(
            store
                .path
                .parent()
                .ok_or("Recovery storage has no parent directory")?,
        )
        .and_then(|directory| directory.sync_all())
        .map_err(|e| e.to_string())
    }
    pub fn finish(mut self, successor: &HostSessionProof) -> Result<(), String> {
        self.validate_continuity()?;
        self.validate_census()?;
        self.adopted_terminal_descriptors = std::mem::take(&mut self.adopted_terminal_descriptors)
            .into_iter()
            .map(proof_terminal_descriptor)
            .collect();
        self.successor = Some(successor.clone());
        self.validate_issued_chats()?;
        self.settled = true;
        self.save()?;
        drop(self.authority.take());
        Ok(())
    }
}

/// Tests join real holder authentication to the ordinary read-only proof
/// consumer without fabricating a live receipt or automatic source ACK.
#[cfg(all(any(test, feature = "test-support"), feature = "host"))]
pub fn fixture_settled_outcome(
    checkpoint: &Path,
    target: &Path,
    source: &HostSessionProof,
    census: Census,
    serving: &HostSessionProof,
    terminals: &[crate::terminal_ipc::TerminalDescriptor],
    chats: &[Session],
) -> Result<PathBuf, String> {
    let bytes = read_checkpoint(checkpoint)?.ok_or("Fixture has no checkpoint")?;
    super::handoff::validate_checkpoint_bytes(&bytes)?;
    census.validate_evidence(source)?;
    let target = fs::canonicalize(target).map_err(|e| e.to_string())?;
    let state = crate::update::locate(&target)
        .map_err(|e| e.to_string())?
        .prefix
        .join(".update");
    crate::secure_file::prepare_private_directory(&state).map_err(|e| e.to_string())?;
    let job = uuid::Uuid::new_v4().to_string();
    let hash = digest(&bytes);
    let request = ManualRequest {
        version: 1,
        job: job.clone(),
        checkpoint: hash.clone(),
        source: source.clone(),
        target: target.clone(),
        target_hash: executable_hash(&target)?,
        census_hash: digest(&serde_json::to_vec(&census).map_err(|e| e.to_string())?),
        windows: Vec::new(),
    };
    let request = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    let mut request_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(state.join(format!("{job}.request")))
        .map_err(|e| e.to_string())?;
    request_file
        .write_all(&request)
        .and_then(|_| request_file.sync_all())
        .map_err(|e| e.to_string())?;
    let live_terminals: BTreeSet<_> = terminals
        .iter()
        .map(|terminal| terminal.session.id)
        .collect();
    let live_chats: BTreeSet<_> = chats.iter().map(|chat| chat.id).collect();
    let record = RecoveryRecord {
        version: 2,
        origin: Origin::ExplicitRecovery {
            checkpoint: hash.clone(),
        },
        job: job.clone(),
        request_identity: digest(&request),
        job_request_hash: digest(&request),
        reviewed: None,
        windows: Vec::new(),
        source: source.clone(),
        checkpoint_hash: hash.clone(),
        census: census.clone(),
        adopted_terminals: live_terminals.iter().copied().collect(),
        adopted_chats: live_chats.iter().copied().collect(),
        adopted_terminal_descriptors: terminals
            .iter()
            .cloned()
            .map(proof_terminal_descriptor)
            .collect(),
        adopted_chat_descriptors: chats.to_vec(),
        paused_terminals: census
            .terminal_ids()
            .difference(&live_terminals)
            .copied()
            .collect(),
        paused_chats: census
            .chats
            .iter()
            .filter(|chat| !live_chats.contains(&chat.id))
            .map(|chat| chat.id)
            .collect(),
        settled: true,
        successor: Some(serving.clone()),
        target_hash: executable_hash(&target)?,
        target: Some(target.clone()),
        authority: None,
    };
    validate_settled_record(&record, &bytes, checkpoint, &target)?;
    persist_bound_capture(
        checkpoint,
        &CapturedCensus {
            producer: Some(CaptureProducer::ExplicitCensus {
                transaction: job,
                census_hash: digest(&serde_json::to_vec(&census).map_err(|e| e.to_string())?),
            }),
            source: source.clone(),
            checkpoint_hash: Some(hash.clone()),
            census,
            windows: Vec::new(),
        },
    )?;
    let path = checkpoint.with_file_name(recovery_name(&hash));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|e| e.to_string())?;
    file.write_all(&serde_json::to_vec(&record).map_err(|e| e.to_string())?)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    File::open(path.parent().ok_or("Fixture has no checkpoint directory")?)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests;
