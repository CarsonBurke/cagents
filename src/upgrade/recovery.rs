//! One update's causal authority to recover its own incomplete preparation.
//!
//! A Prepared checkpoint is never replay permission. This receipt identifies
//! bytes written by an acknowledged prepare while the same update and window
//! fences remain held. Its only startup action is adopting existing owners.
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
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd},
        unix::{
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
const SEALS: i32 = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
static INHERITED: OnceLock<io::Result<Mutex<Option<Inherited>>>> = OnceLock::new();

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Census {
    pub terminals: Vec<TerminalSession>,
    pub chats: Vec<Session>,
    pub snapshots: Vec<super::floor::StoppedSnapshot>,
}

#[derive(Default)]
pub(crate) struct SnapshotCapture {
    snapshots: Vec<super::floor::StoppedSnapshot>,
    used: usize,
}

impl SnapshotCapture {
    pub(crate) fn push(
        &mut self,
        source: Session,
        mut snapshot: crate::chat::Snapshot,
    ) -> Result<(), String> {
        snapshot.notification_preview = Default::default();
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
        let size = encoded_size(&saved, SNAPSHOT_LIMIT.saturating_sub(self.used))?;
        self.used = self
            .used
            .checked_add(size + 1)
            .filter(|size| *size <= SNAPSHOT_LIMIT)
            .ok_or("Recovery transcript capture exceeds 8 MiB; the source was not prepared")?;
        self.snapshots.push(saved);
        Ok(())
    }
    pub(crate) fn finish(self) -> Vec<super::floor::StoppedSnapshot> {
        self.snapshots
    }
}

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
                    io::Error::other("Recovery evidence exceeds 8 MiB; the source was not prepared")
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

impl Census {
    pub(crate) fn terminal_ids(&self) -> BTreeSet<TerminalId> {
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
    fn validate_capture(&self, source: &HostSessionProof) -> Result<(), String> {
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
        let existing = super::floor::existing_capture_size(source)?;
        let available = SNAPSHOT_LIMIT
            .checked_sub(existing)
            .ok_or("Existing recovery evidence exceeds 8 MiB")?;
        encoded_size(self, available)?;
        let mut record = super::floor::Record::new(
            source.clone(),
            self.chats.clone(),
            self.terminal_ids().into_iter().collect(),
        );
        record.snapshots = self.snapshots.clone();
        encoded_size(&record, available)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Authority {
    version: u16,
    job: String,
    request_identity: String,
    job_request_hash: String,
    source: HostSessionProof,
    census: Census,
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
    daemon: File,
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
        Ok(Self {
            authority: Authority {
                version: 1,
                job: job.into(),
                request_identity,
                job_request_hash,
                source: daemon.capability.proof.clone(),
                census,
                expected: daemon
                    .sessions
                    .iter()
                    .map(|item| (item.kind, item.id))
                    .collect(),
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
        })
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
        self.authority.checkpoint_hash = digest(&bytes);
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
            daemon,
            authority: preparation.authority,
        }))
    }
}

impl Receipt {
    pub(crate) fn inherit(&self, command: &mut Command) -> io::Result<()> {
        forward(
            command,
            [
                &self.receipt,
                &self.worker,
                &self.fence,
                &self.daemon,
                &self.legacy,
            ],
        )
    }

    pub(crate) fn verify(
        &self,
        serving: &HostSessionProof,
        live_terminals: &[crate::terminal_ipc::TerminalDescriptor],
        live_chats: &[Session],
    ) -> Result<RecoveryOutcome, String> {
        let store = super::handoff::Store::discover()?;
        let name = recovery_name(&self.authority.checkpoint_hash);
        let bytes = read_checkpoint(&store.path.with_file_name(name))?
            .ok_or("The recovery daemon has no durable outcome")?;
        let record: RecoveryRecord = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let checkpoint = store
            .read_bytes()?
            .ok_or("The Prepared checkpoint disappeared after recovery")?;
        if !record.settled
            || record.version != 1
            || record.job != self.authority.job
            || record.request_identity != self.authority.request_identity
            || record.source != self.authority.source
            || record.checkpoint_hash != self.authority.checkpoint_hash
            || digest(&checkpoint) != record.checkpoint_hash
            || record.target_hash != self.authority.target_hash
            || record.census != self.authority.census
            || record.successor.as_ref() != Some(serving)
        {
            return Err(
                "The serving daemon has not verified this update's exact recovery outcome".into(),
            );
        }
        record.validate_census()?;
        let terminals: BTreeSet<_> = live_terminals.iter().map(|item| item.session.id).collect();
        let chats: BTreeSet<_> = live_chats.iter().map(|item| item.id).collect();
        if terminals.len() != live_terminals.len()
            || chats.len() != live_chats.len()
            || terminals != record.adopted_terminals.iter().copied().collect()
            || chats != record.adopted_chats.iter().copied().collect()
            || live_terminals
                .iter()
                .any(|item| !verified_terminal(&record.census, item, serving))
            || live_chats.iter().any(|item| {
                !record
                    .census
                    .chats
                    .iter()
                    .any(|known| recovered_chat(known, item, serving))
            })
        {
            return Err(
                "The serving recovery inventory differs from its verified original owners".into(),
            );
        }
        Ok(RecoveryOutcome {
            paused_terminals: record.paused_terminals,
            paused_chats: record.paused_chats,
        })
    }
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
) -> bool {
    census
        .terminals
        .iter()
        .chain(
            census
                .chats
                .iter()
                .filter_map(|chat| chat.terminal.as_ref()),
        )
        .any(|known| {
            recovered_terminal(known, &actual.session)
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

fn recovered_chat(known: &Session, actual: &Session, serving: &HostSessionProof) -> bool {
    known.id == actual.id
        && actual.proof.as_ref() == Some(serving)
        && known.launch.provider == actual.launch.provider
        && known.launch.recipe == actual.launch.recipe
        && known
            .launch
            .session_id
            .as_ref()
            .is_none_or(|id| actual.launch.session_id.as_ref() == Some(id))
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
        job: String::new(),
        request_identity: String::new(),
        job_request_hash: String::new(),
        source: daemon.capability.proof.clone(),
        census: census.clone(),
        expected: daemon
            .sessions
            .iter()
            .map(|item| (item.kind, item.id))
            .collect(),
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

/// Frozen protocol68/77 sources cannot expose today's full census. Their
/// authenticated prepare can still authorize ordinary Ready restoration, but
/// never the candidate-owned recovery of an incomplete Prepared checkpoint.
pub(crate) fn require_ready_scope(daemon: &DaemonImpact) -> Result<(), String> {
    let bytes = super::handoff::Store::discover()?
        .read_bytes()?
        .ok_or("The acknowledged legacy source has no Ready checkpoint")?;
    validate_ready_scope(daemon, &bytes)
}

fn validate_ready_scope(daemon: &DaemonImpact, bytes: &[u8]) -> Result<(), String> {
    #[derive(Deserialize)]
    struct ScopeCheckpoint {
        version: u16,
        proof: HostSessionProof,
        phase: String,
        terminals: Vec<ScopeEntry>,
        #[serde(default)]
        carried: Vec<ScopeEntry>,
        chats: Vec<ScopeEntry>,
    }
    #[derive(Deserialize)]
    struct ScopeEntry {
        source: ScopeSource,
    }
    #[derive(Deserialize)]
    struct ScopeSource {
        id: TerminalId,
        #[serde(default)]
        terminal: Option<ScopePair>,
    }
    #[derive(Deserialize)]
    struct ScopePair {
        id: TerminalId,
    }

    if bytes.len() > SNAPSHOT_LIMIT {
        return Err("Legacy Ready checkpoint exceeds its bounded byte limit".into());
    }
    let checkpoint: ScopeCheckpoint =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    // Both qualified frozen sources write checkpoint formats 1 through 4,
    // with at most 1024 terminals and 128 native conversations.
    if !(1..=4).contains(&checkpoint.version)
        || checkpoint.phase != "Ready"
        || !daemon.capability.proof.is_complete()
        || checkpoint.proof != daemon.capability.proof
        || !daemon.ready()
        || checkpoint.terminals.len() + checkpoint.carried.len() > 1024
        || checkpoint.chats.len() > 128
    {
        return Err("The acknowledged legacy checkpoint is not Ready for this exact source scope; checkpoint evidence was preserved".into());
    }
    let mut expected_terminals = BTreeSet::new();
    let mut expected_chats = BTreeSet::new();
    for item in &daemon.sessions {
        let expected = match item.kind {
            SessionKind::Terminal => &mut expected_terminals,
            SessionKind::NativeChat => &mut expected_chats,
        };
        if !expected.insert(item.id) {
            return Err("The reviewed legacy source scope contains duplicate owners".into());
        }
    }
    if !expected_terminals.is_disjoint(&expected_chats) {
        return Err("The reviewed legacy source scope has an ambiguous paired owner".into());
    }
    let mut terminals = BTreeSet::new();
    for item in checkpoint.terminals.iter().chain(&checkpoint.carried) {
        if item.source.terminal.is_some() || !terminals.insert(item.source.id) {
            return Err(
                "The legacy Ready checkpoint contains duplicate or ambiguous terminal owners"
                    .into(),
            );
        }
    }
    let mut chats = BTreeSet::new();
    for item in &checkpoint.chats {
        let source = &item.source;
        if !chats.insert(source.id)
            || source
                .terminal
                .as_ref()
                .is_some_and(|pair| pair.id != source.id)
            || terminals.contains(&source.id) && source.terminal.is_none()
        {
            return Err("The legacy Ready checkpoint has an unproven native terminal pair".into());
        }
        // Historical UpgradeImpact represents a chat and its same-ID console
        // as one native owner. Only an explicitly recorded pair may account
        // for that otherwise-extra terminal slot in the checkpoint.
        if source.terminal.is_some() {
            terminals.remove(&source.id);
        }
    }
    if terminals != expected_terminals || chats != expected_chats {
        return Err(
            "The legacy Ready checkpoint differs from the exact reviewed owner scope".into(),
        );
    }
    Ok(())
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
        && known.terminal.as_ref().map(|item| item.id)
            == source.terminal.as_ref().map(|item| item.id)
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
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
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
    Ok(())
}

/// Refuse before any source prepare can overwrite earlier checkpoint bytes.
pub(crate) fn require_checkpoint_absent() -> Result<(), String> {
    require_absent_checkpoint(&super::handoff::Store::discover()?.path)
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

fn sealed_receipt(authority: &Authority) -> io::Result<File> {
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

pub(crate) fn has_inherited_receipt() -> io::Result<bool> {
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
pub(crate) struct RecoveryRecord {
    version: u16,
    pub job: String,
    pub request_identity: String,
    pub source: HostSessionProof,
    pub checkpoint_hash: String,
    pub census: Census,
    pub adopted_terminals: Vec<TerminalId>,
    pub adopted_chats: Vec<TerminalId>,
    pub paused_terminals: Vec<TerminalId>,
    pub paused_chats: Vec<TerminalId>,
    pub settled: bool,
    pub successor: Option<HostSessionProof>,
    target_hash: String,
    /// Dropping either a successful or failed adoption releases only these
    /// duplicates, without unlocking the worker's continuous original fences.
    #[cfg(feature = "host")]
    #[serde(skip)]
    authority: Option<Continuity>,
}

fn recovery_name(hash: &str) -> String {
    format!("daemon-upgrade.recovery.{hash}.json")
}

impl RecoveryRecord {
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
pub(crate) fn take_for_host() -> Result<Option<RecoveryRecord>, String> {
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
        version: 1,
        job: authority.job,
        request_identity: authority.request_identity,
        source: authority.source,
        checkpoint_hash: authority.checkpoint_hash,
        census: authority.census,
        adopted_terminals: Vec::new(),
        adopted_chats: Vec::new(),
        paused_terminals: Vec::new(),
        paused_chats: Vec::new(),
        settled: false,
        successor: None,
        target_hash: authority.target_hash,
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
    Ok(())
}

#[cfg(feature = "host")]
impl RecoveryRecord {
    fn name(&self) -> String {
        recovery_name(&self.checkpoint_hash)
    }
    pub(crate) fn validate_continuity(&self) -> Result<(), String> {
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
    pub(crate) fn finish(mut self, successor: &HostSessionProof) -> Result<(), String> {
        self.validate_continuity()?;
        self.validate_census()?;
        self.successor = Some(successor.clone());
        self.settled = true;
        self.save()?;
        drop(self.authority.take());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe, TerminalContinuity};

    fn lock(path: &Path) -> File {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        file
    }

    fn preparation(root: &Path) -> Preparation {
        let source = HostSessionProof {
            host_session_id: HostSessionId::new("acknowledged-source"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("exact-daemon"),
        };
        let terminal = TerminalSession {
            id: TerminalId::new(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec!["-c".into(), "exit 0".into()],
                working_directory: root.to_owned(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: source.host_session_id.clone(),
                boot_id: source.boot_id.clone(),
                host_incarnation: source.host_incarnation.clone(),
            },
        };
        let worker_path = root.join("worker.lock");
        let fence_path = root.join("fence.lock");
        let legacy_path = root.join("legacy.lock");
        let legacy = lock(&legacy_path);
        assert_eq!(unsafe { libc::flock(legacy.as_raw_fd(), libc::LOCK_SH) }, 0);
        Preparation {
            worker: lock(&worker_path),
            fence: lock(&fence_path),
            legacy,
            authority: Authority {
                version: 1,
                job: uuid::Uuid::new_v4().to_string(),
                request_identity: "accepted-request-digest".into(),
                job_request_hash: "exact-private-request".into(),
                source,
                expected: vec![(SessionKind::Terminal, terminal.id)],
                census: Census {
                    terminals: vec![terminal],
                    chats: Vec::new(),
                    snapshots: Vec::new(),
                },
                checkpoint_path: root.join("daemon-upgrade.json"),
                checkpoint_hash: String::new(),
                worker_path,
                fence_path,
                legacy_path,
                daemon_path: PathBuf::new(),
                target: PathBuf::new(),
                target_hash: String::new(),
            },
        }
    }

    fn checkpoint(preparation: &Preparation, phase: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": 2, "phase": phase, "proof": preparation.authority.source,
            "terminals": [], "carried": [{"source": preparation.authority.census.terminals[0]}], "chats": [],
        })).unwrap()
    }

    fn save(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
    }

    fn handoff(root: &Path) -> LeaseHandoff {
        let path = root.join("cagentsd.lock");
        drop(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap(),
        );
        LeaseHandoff::wait(
            &crate::daemon::RuntimePaths {
                directory: root.to_owned(),
                socket: root.join("cagentsd.sock"),
                lock: path,
            },
            std::process::id(),
        )
        .unwrap()
    }

    #[test]
    fn acknowledged_prepare_binds_source_and_every_reviewed_owner() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let bytes = checkpoint(&preparation, "Prepared");
        assert_eq!(
            validate_checkpoint(&preparation.authority, &bytes).unwrap(),
            "Prepared"
        );
        let mut foreign: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        foreign["proof"]["host_incarnation"] = "unrelated-daemon".into();
        assert!(
            validate_checkpoint(
                &preparation.authority,
                &serde_json::to_vec(&foreign).unwrap()
            )
            .is_err()
        );
        let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        changed["carried"][0]["source"]["launch"]["arguments"] =
            serde_json::json!(["-c", "different writer"]);
        assert!(
            validate_checkpoint(
                &preparation.authority,
                &serde_json::to_vec(&changed).unwrap()
            )
            .is_err()
        );
        changed["carried"] = serde_json::json!([]);
        assert!(
            validate_checkpoint(
                &preparation.authority,
                &serde_json::to_vec(&changed).unwrap()
            )
            .is_err()
        );
    }

    fn legacy_scope_fixture() -> (DaemonImpact, serde_json::Value) {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("frozen-source"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("legacy-daemon"),
        };
        let terminal = TerminalId::new();
        let chat = TerminalId::new();
        let daemon = DaemonImpact {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof: proof.clone(),
                token: "accepted-scope".into(),
                inventory_generation: 1,
                live_terminals: 2,
                background_subagents: 0,
                background_identity: String::new(),
                expires_at_unix_ms: 100,
            },
            policy: crate::update::impact::Policy::WhenIdle,
            sessions: [
                (SessionKind::Terminal, terminal),
                (SessionKind::NativeChat, chat),
            ]
            .into_iter()
            .map(|(kind, id)| crate::update::impact::SessionImpact {
                id,
                kind,
                title: String::new(),
                identity: "reviewed-source-owner".into(),
                disposition: crate::update::impact::Disposition::Resumes,
                reason: String::new(),
                interrupted_subagents: Vec::new(),
            })
            .collect(),
            blockers: Vec::new(),
        };
        // Source ID metadata only: this guard must not decode today's recipe,
        // continuity, provider, or snapshot DTOs to qualify a frozen source.
        let checkpoint = serde_json::json!({
            "version":4,"phase":"Ready","proof":proof,
            "terminals":[{"source":{"id":terminal,"launch":{"legacy_format":"not a current LaunchRecipe"}}}],
            "carried":[{"source":{"id":chat}}],
            "chats":[{"source":{"id":chat,"terminal":{"id":chat}},"launch":{"legacy_format":"not a current Launch"}}],
        });
        (daemon, checkpoint)
    }

    #[test]
    fn legacy_ready_scope_reads_only_frozen_identity_metadata_and_accounts_for_a_proven_pair() {
        let (mut daemon, mut checkpoint) = legacy_scope_fixture();
        validate_ready_scope(&daemon, &serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        // Format one has no carried entries; a simple terminal-only Ready
        // checkpoint remains a supported ordinary restore path.
        checkpoint["version"] = 1.into();
        checkpoint.as_object_mut().unwrap().remove("carried");
        checkpoint["chats"] = serde_json::json!([]);
        daemon
            .sessions
            .retain(|item| item.kind == SessionKind::Terminal);
        validate_ready_scope(&daemon, &serde_json::to_vec(&checkpoint).unwrap()).unwrap();
    }

    #[test]
    fn legacy_ready_scope_refuses_incomplete_foreign_changed_and_ambiguous_owners_without_touching_bytes()
     {
        let (daemon, original) = legacy_scope_fixture();
        let mut invalid = Vec::new();
        for phase in ["Prepared", "Restoring", "Unknown"] {
            let mut changed = original.clone();
            changed["phase"] = phase.into();
            invalid.push(changed);
        }
        let mut foreign = original.clone();
        foreign["proof"]["host_incarnation"] = "another-source".into();
        invalid.push(foreign);
        let mut unsupported = original.clone();
        unsupported["version"] = 5.into();
        invalid.push(unsupported);
        let mut missing = original.clone();
        missing["chats"] = serde_json::json!([]);
        invalid.push(missing);
        let mut extra = original.clone();
        extra["terminals"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"source":{"id":TerminalId::new()}}));
        invalid.push(extra);
        let mut duplicate = original.clone();
        duplicate["carried"]
            .as_array_mut()
            .unwrap()
            .push(original["terminals"][0].clone());
        invalid.push(duplicate);
        let mut unproven = original.clone();
        unproven["chats"][0]["source"]["terminal"] = serde_json::Value::Null;
        invalid.push(unproven);
        let mut wrong_pair = original.clone();
        wrong_pair["chats"][0]["source"]["terminal"]["id"] = TerminalId::new().to_string().into();
        invalid.push(wrong_pair);
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let path = root.path().join("daemon-upgrade.json");
        for checkpoint in invalid {
            let bytes = serde_json::to_vec(&checkpoint).unwrap();
            save(&path, &bytes);
            assert!(
                validate_ready_scope(&daemon, &read_checkpoint(&path).unwrap().unwrap()).is_err()
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                bytes,
                "scope refusal must preserve foreign or incomplete evidence"
            );
        }
        let mut duplicate_review = daemon.clone();
        duplicate_review.sessions.push(daemon.sessions[0].clone());
        assert!(
            validate_ready_scope(&duplicate_review, &serde_json::to_vec(&original).unwrap())
                .is_err()
        );
    }

    #[test]
    fn frozen_source_checkpoint_binds_confirmed_thread_to_effective_launch_not_issued_alias() {
        for original_id in [None, Some(uuid::Uuid::new_v4().to_string())] {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let mut preparation = preparation(root.path());
            let confirmed = uuid::Uuid::new_v4().to_string();
            // Frozen 991daa43e ChatList overlays the handle's confirmed ID,
            // while its checkpoint source retains the initially issued alias.
            let listed = Session {
                id: TerminalId::new(),
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Codex,
                    recipe: LaunchRecipe {
                        program: "codex".into(),
                        arguments: vec!["--remote".into(), "unix:///fixture/control.sock".into()],
                        working_directory: root.path().to_owned(),
                        environment: Default::default(),
                        unset_environment: Default::default(),
                    },
                    session_id: Some(confirmed.clone()),
                },
                proof: Some(preparation.authority.source.clone()),
                terminal: None,
                owned_codex_listener: None,
                native_continuity: crate::chat::NativeContinuity::ReconnectOnly,
            };
            let mut issued = listed.clone();
            issued.launch.session_id = original_id;
            preparation
                .authority
                .expected
                .push((SessionKind::NativeChat, listed.id));
            preparation.authority.census.chats.push(listed.clone());
            let mut captured_page = SnapshotCapture::default();
            captured_page
                .push(
                    listed.clone(),
                    crate::chat::Snapshot {
                        session_id: Some(confirmed.clone()),
                        ..Default::default()
                    },
                )
                .unwrap();
            preparation.authority.census.snapshots = captured_page.finish();
            let mut checkpoint: serde_json::Value =
                serde_json::from_slice(&checkpoint(&preparation, "Prepared")).unwrap();
            checkpoint["chats"] =
                serde_json::json!([{ "source": issued, "launch": listed.launch }]);
            let bytes = serde_json::to_vec(&checkpoint).unwrap();
            save(&preparation.authority.checkpoint_path, &bytes);
            let captured = preparation.acknowledged().unwrap();
            assert_eq!(captured.0.authority.checkpoint_hash, digest(&bytes));
            // Ready fallback uses this same distinction and still requires the
            // exact current conversation, including after an actor reset.
            checkpoint["phase"] = "Ready".into();
            assert_eq!(
                validate_checkpoint(
                    &captured.0.authority,
                    &serde_json::to_vec(&checkpoint).unwrap()
                )
                .unwrap(),
                "Ready"
            );
            checkpoint["chats"][0]["launch"]["session_id"] =
                uuid::Uuid::new_v4().to_string().into();
            assert!(
                validate_checkpoint(
                    &captured.0.authority,
                    &serde_json::to_vec(&checkpoint).unwrap()
                )
                .is_err(),
                "a different effective conversation is never recovery authority"
            );
            checkpoint["chats"][0]["launch"]["session_id"] = confirmed.into();
            checkpoint["chats"][0]["launch"]["recipe"]["arguments"] =
                serde_json::json!(["--different-provider-target"]);
            assert!(
                validate_checkpoint(
                    &captured.0.authority,
                    &serde_json::to_vec(&checkpoint).unwrap()
                )
                .is_err(),
                "normalization cannot change any other launch option"
            );
        }
    }

    #[test]
    fn preexisting_prepared_or_unknown_evidence_is_preserved_before_any_prepare() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let path = &preparation.authority.checkpoint_path;
        require_absent_checkpoint(path).unwrap();
        for bytes in [
            checkpoint(&preparation, "Prepared"),
            b"unknown evidence that must not be replaced".to_vec(),
        ] {
            save(path, &bytes);
            assert!(require_absent_checkpoint(path).is_err());
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn oversized_snapshot_capture_fails_before_preparation_and_keeps_earlier_pages() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let terminal = &preparation.authority.census.terminals[0];
        let source = Session {
            id: terminal.id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: terminal.launch.clone(),
                session_id: Some("thread".into()),
            },
            proof: Some(preparation.authority.source.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let mut captured = SnapshotCapture::default();
        captured
            .push(
                source.clone(),
                crate::chat::Snapshot {
                    title: Some("first page".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            captured
                .push(
                    source,
                    crate::chat::Snapshot {
                        composer: Some("x".repeat(SNAPSHOT_LIMIT)),
                        ..Default::default()
                    }
                )
                .is_err()
        );
        let pages = captured.finish();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].snapshot.title.as_deref(), Some("first page"));
        assert!(!preparation.authority.checkpoint_path.exists());
    }

    #[test]
    fn losing_the_continuous_worker_lease_prevents_receipt_even_with_matching_bytes() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let bytes = checkpoint(&preparation, "Prepared");
        save(&preparation.authority.checkpoint_path, &bytes);
        assert_eq!(
            unsafe { libc::flock(preparation.worker.as_raw_fd(), libc::LOCK_UN) },
            0
        );
        assert!(preparation.acknowledged().is_err());
        assert_eq!(
            fs::read(root.path().join("daemon-upgrade.json")).unwrap(),
            bytes
        );
    }

    #[test]
    fn a_changed_prepared_checkpoint_is_neither_reinterpreted_nor_modified() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let original = checkpoint(&preparation, "Prepared");
        save(&preparation.authority.checkpoint_path, &original);
        let captured = preparation.acknowledged().unwrap();
        let mut changed = original.clone();
        changed.push(b'\n'); // Same parsed inventory, different checkpoint identity.
        save(&captured.0.authority.checkpoint_path, &changed);
        assert!(
            captured
                .receipt(Path::new("/unused"), &handoff(root.path()))
                .is_err()
        );
        assert_eq!(
            fs::read(root.path().join("daemon-upgrade.json")).unwrap(),
            changed
        );
    }

    #[test]
    fn source_finishing_ready_keeps_normal_restore_and_preserves_checkpoint_bytes() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        save(
            &preparation.authority.checkpoint_path,
            &checkpoint(&preparation, "Prepared"),
        );
        let captured = preparation.acknowledged().unwrap();
        let ready = checkpoint(&captured.0, "Ready");
        save(&captured.0.authority.checkpoint_path, &ready);
        assert!(
            captured
                .receipt(Path::new("/unused"), &handoff(root.path()))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            fs::read(root.path().join("daemon-upgrade.json")).unwrap(),
            ready
        );
    }

    #[test]
    fn recovery_receipt_is_immutable_after_its_causal_identity_is_sealed() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let mut receipt = sealed_receipt(&preparation.authority).unwrap();
        assert_eq!(
            unsafe { libc::fcntl(receipt.as_raw_fd(), libc::F_GET_SEALS) } & SEALS,
            SEALS
        );
        assert_eq!(
            receipt
                .write_all(b"replacement")
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EPERM)
        );
    }

    #[test]
    fn mutable_files_and_invalid_descriptors_cannot_pass_the_receipt_seal_check() {
        let file = tempfile::tempfile().unwrap();
        assert!(validate_sealed_descriptor(file.as_raw_fd()).is_err());
        assert!(validate_sealed_descriptor(-1).is_err());
    }

    #[test]
    fn recovered_owner_verification_rejects_same_id_with_a_different_recipe_or_conversation() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let known = preparation.authority.census.terminals[0].clone();
        let mut serving = preparation.authority.source.clone();
        serving.host_incarnation = HostIncarnation::new("successor");
        let mut actual = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            known.id,
            known.launch.clone(),
            None,
        );
        actual.session = known.clone();
        actual.session.continuity = TerminalContinuity::DetachedSameBoot {
            host_session_id: serving.host_session_id.clone(),
            boot_id: serving.boot_id.clone(),
            host_incarnation: serving.host_incarnation.clone(),
        };
        actual.upgrade_sources.push(known.clone());
        assert!(verified_terminal(
            &preparation.authority.census,
            &actual,
            &serving
        ));
        actual.session.launch.arguments.push("unrelated".into());
        assert!(!verified_terminal(
            &preparation.authority.census,
            &actual,
            &serving
        ));
        let before = Session {
            id: known.id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: known.launch.clone(),
                session_id: Some("original-thread".into()),
            },
            proof: Some(preparation.authority.source.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: crate::chat::NativeContinuity::Durable {
                provider_version: "fixture".into(),
            },
        };
        let mut after = before.clone();
        after.proof = Some(serving.clone());
        assert!(recovered_chat(&before, &after, &serving));
        after.launch.session_id = Some("another-thread".into());
        assert!(!recovered_chat(&before, &after, &serving));
        after.launch.session_id = before.launch.session_id.clone();
        after.launch.provider = crate::agent::Provider::Claude;
        assert!(!recovered_chat(&before, &after, &serving));
    }

    #[test]
    fn recovery_outcome_requires_disjoint_complete_inventory_including_paired_terminals() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let terminal = preparation.authority.census.terminals[0].clone();
        let mut record = RecoveryRecord {
            version: 1,
            job: preparation.authority.job.clone(),
            request_identity: preparation.authority.request_identity.clone(),
            source: preparation.authority.source.clone(),
            checkpoint_hash: String::new(),
            census: preparation.authority.census.clone(),
            adopted_terminals: Vec::new(),
            adopted_chats: Vec::new(),
            paused_terminals: vec![terminal.id],
            paused_chats: Vec::new(),
            settled: false,
            successor: None,
            target_hash: String::new(),
            #[cfg(feature = "host")]
            authority: None,
        };
        assert!(record.validate_census().is_ok());
        record.paused_terminals.clear();
        assert!(
            record.validate_census().is_err(),
            "missing owners cannot disappear from the outcome"
        );
        record.adopted_terminals = vec![terminal.id, TerminalId::new()];
        assert!(
            record.validate_census().is_err(),
            "an unrelated holder cannot substitute for an original owner"
        );
        record.adopted_terminals = vec![terminal.id];
        record.paused_terminals = vec![terminal.id];
        assert!(
            record.validate_census().is_err(),
            "an original owner cannot be both live and paused"
        );
        record.paused_terminals.clear();
        // A retained native owner can adopt its paired terminal internally,
        // although that console was absent from the independent terminal list.
        record.census.terminals.clear();
        record.census.chats.push(Session {
            id: terminal.id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: terminal.launch.clone(),
                session_id: Some("thread".into()),
            },
            proof: Some(record.source.clone()),
            terminal: Some(terminal.clone()),
            owned_codex_listener: None,
            native_continuity: Default::default(),
        });
        record.paused_chats.push(terminal.id);
        assert!(
            record.validate_census().is_ok(),
            "paired console adoption is part of the original chat's census"
        );
    }
    mod forwarding_tests {
        use super::*;
        use std::process::Stdio;

        const TEST: &str = "upgrade::recovery::tests::forwarding_tests::bash_cli_forwards_recovery_only_to_the_verified_daemon";
        const MODE: &str = "CAGENTS_TEST_RECOVERY_FORWARD_MODE";
        const ROOT: &str = "CAGENTS_TEST_RECOVERY_FORWARD_ROOT";

        #[test]
        fn bash_cli_forwards_recovery_only_to_the_verified_daemon() {
            if let Some(mode) = std::env::var_os(MODE) {
                let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
                crate::daemon::lease_handoff::initialize_inherited_lease().unwrap();
                assert!(has_inherited_receipt().unwrap());
                assert!(crate::daemon::lease_handoff::has_inherited_lease().unwrap());
                for fd in [LEGACY_FD, RECEIPT_FD, WORKER_FD, FENCE_FD, DAEMON_FD] {
                    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                    assert_ne!(flags, -1, "Bash lost recovery descriptor {fd}");
                    assert_ne!(flags & libc::FD_CLOEXEC, 0);
                }

                // Model the store/schema maintenance launch: sanitize, seal,
                // then forward the ordinary reservation, never the receipt.
                let maintenance_marker = root.join("maintenance-executed");
                // Maintenance uses the very same daemon executable, so a
                // path/hash match alone must not turn the normal lease hook
                // into recovery forwarding.
                let mut maintenance = Command::new(root.join("bin/cagentsd"));
                maintenance
                    .args([
                        "-c",
                        r#"
                    test "$CAGENTS_DAEMON_LEASE_FD" = 199 || exit 10
                    test -e /proc/self/fd/199 || exit 11
                    test -z "${CAGENTS_UPDATE_RECOVERY_FD+x}" || exit 12
                    for fd in 189 193 194 195 196; do
                        test ! -e "/proc/self/fd/$fd" || exit 13
                    done
                    printf maintenance > "$1"
                "#,
                        "maintenance-fixture",
                    ])
                    .arg(&maintenance_marker);
                crate::install_guard::sanitize_child_environment(&mut maintenance);
                unsafe { maintenance.pre_exec(crate::daemon::seal_inherited_descriptors) };
                crate::daemon::lease_handoff::inherit_daemon_lease(&mut maintenance).unwrap();
                assert!(maintenance.status().unwrap().success());
                assert_eq!(fs::read(&maintenance_marker).unwrap(), b"maintenance");

                let wrong_marker = root.join("wrong-target-executed");
                let mut wrong = Command::new(root.join("other/cagentsd"));
                wrong
                    .args(["-c", "printf wrong > \"$1\"", "wrong-target-fixture"])
                    .arg(&wrong_marker);
                crate::install_guard::sanitize_child_environment(&mut wrong);
                unsafe { wrong.pre_exec(crate::daemon::seal_inherited_descriptors) };
                let rejected =
                    crate::daemon::lease_handoff::inherit_target_daemon_lease(&mut wrong)
                        .and_then(|()| wrong.status());
                assert!(
                    rejected.is_err(),
                    "identical bytes and basename are not the selected path"
                );
                assert!(!wrong_marker.exists());

                let target_marker = root.join("target-executed");
                let mut target = Command::new(root.join("bin/cagentsd"));
                target
                    .args([
                        "-c",
                        r#"
                    test "$CAGENTS_UPDATE_RECOVERY_FD" = 193 || exit 20
                    test "$CAGENTS_DAEMON_LEASE_FD" = 199 || exit 21
                    test -f /proc/self/fd/193 || exit 22
                    test /proc/self/fd/194 -ef "$1/worker.lock" || exit 23
                    test /proc/self/fd/195 -ef "$1/fence.lock" || exit 24
                    test /proc/self/fd/189 -ef "$1/legacy.lock" || exit 25
                    test /proc/self/fd/196 -ef "$2" || exit 26
                    test /proc/self/fd/199 -ef "$2" || exit 27
                    printf daemon > "$3"
                "#,
                        "target-daemon-fixture",
                    ])
                    .arg(&root)
                    .arg(crate::daemon::RuntimePaths::discover().unwrap().lock)
                    .arg(&target_marker);
                crate::install_guard::sanitize_child_environment(&mut target);
                unsafe { target.pre_exec(crate::daemon::seal_inherited_descriptors) };
                let result = crate::daemon::lease_handoff::inherit_target_daemon_lease(&mut target)
                    .and_then(|()| target.status());
                if mode == "bad-hash" {
                    assert!(
                        result.is_err(),
                        "changed verified bytes must fail before exec"
                    );
                    assert!(!target_marker.exists());
                } else {
                    assert_eq!(mode, "valid");
                    assert!(result.unwrap().success());
                    assert_eq!(fs::read(&target_marker).unwrap(), b"daemon");
                }
                return;
            }

            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            for mode in ["valid", "bad-hash"] {
                let case = root.path().join(mode);
                fs::create_dir(&case).unwrap();
                fs::set_permissions(&case, fs::Permissions::from_mode(0o700)).unwrap();
                for child in ["bin", "other", "runtime"] {
                    fs::create_dir(case.join(child)).unwrap();
                    fs::set_permissions(case.join(child), fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
                // Small ELF fixtures expose inherited descriptors without ever
                // starting a provider or the installed daemon. The negative
                // target deliberately has identical bytes and basename.
                for executable in ["bin/cagentsd", "other/cagentsd"] {
                    fs::copy("/bin/sh", case.join(executable)).unwrap();
                    fs::set_permissions(case.join(executable), fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
                let runtime = case.join("runtime");
                let daemon_directory = runtime.join(crate::instance::runtime_subdirectory());
                fs::create_dir(&daemon_directory).unwrap();
                fs::set_permissions(&daemon_directory, fs::Permissions::from_mode(0o700)).unwrap();
                let preparation = preparation(&case);
                save(
                    &preparation.authority.checkpoint_path,
                    &checkpoint(&preparation, "Prepared"),
                );
                let lease = handoff(&daemon_directory);
                let mut receipt = preparation
                    .acknowledged()
                    .unwrap()
                    .receipt(&case.join("bin/cagentsd"), &lease)
                    .unwrap()
                    .unwrap();
                if mode == "bad-hash" {
                    receipt.authority.target_hash = "0".repeat(64);
                    receipt.receipt = sealed_receipt(&receipt.authority).unwrap();
                }
                let mut command = Command::new("/bin/bash");
                command
                    .args([
                        "--noprofile",
                        "--norc",
                        "-p",
                        "-c",
                        "exec \"$@\"",
                        "receipt-bash-fixture",
                    ])
                    .arg(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                    .env(MODE, mode)
                    .env(ROOT, &case)
                    .env("XDG_RUNTIME_DIR", &runtime)
                    .env_remove("BASH_ENV")
                    .env_remove("ENV")
                    .stdin(Stdio::null());
                crate::install_guard::sanitize_child_environment(&mut command);
                unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
                // The updater explicitly grants the verified shell/selected CLI
                // its bundle. Later generic lease forwarding must not repeat it.
                lease.inherit(&mut command).unwrap();
                receipt.inherit(&mut command).unwrap();
                let output = command.output().unwrap();
                assert!(
                    output.status.success(),
                    "{mode}:\n{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
            }
        }
    }
}
