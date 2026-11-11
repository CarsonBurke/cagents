//! One update's causal authority to recover its own incomplete preparation.
//!
//! A Prepared checkpoint is never replay permission. This receipt identifies
//! bytes written by an acknowledged prepare while the same update and window
//! fences remain held. Its only startup action is adopting existing owners.
pub(crate) mod verified;

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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Census {
    pub terminals: Vec<TerminalSession>,
    pub chats: Vec<Session>,
    pub snapshots: Vec<super::floor::StoppedSnapshot>,
    #[serde(default)]
    pub aliases: Vec<Session>,
    #[serde(default)]
    pub presentations: Vec<super::floor::Presentation>,
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
        self.validate_evidence(source)?;
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
        record.aliases = self.aliases.clone();
        record.presentations = self.presentations.clone();
        encoded_size(&record, available)?;
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
                                known.id == terminal.id && !same_terminal(known, terminal)
                            })
                    })
            })
            || self.terminals.iter().any(|item| {
                chats.contains(&item.id)
                    && !self.chats.iter().any(|chat| {
                        chat.terminal
                            .as_ref()
                            .is_some_and(|paired| same_terminal(item, paired))
                    })
            })
        {
            return Err(
                "Recovery capture has duplicate, ambiguous, or different source owners".into(),
            );
        }
        self.validate_presentations()?;
        encoded_size(self, SNAPSHOT_LIMIT)?;
        let mut record = super::floor::Record::new(
            source.clone(),
            self.chats.clone(),
            self.terminal_ids().into_iter().collect(),
        );
        record.snapshots = self.snapshots.clone();
        record.aliases = self.aliases.clone();
        record.presentations = self.presentations.clone();
        encoded_size(&record, SNAPSHOT_LIMIT)?;
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
                    presentation.session.terminal.as_ref() == Some(saved)
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

/// Count the complete receipt plus more owner bookkeeping than any valid
/// outcome can contain. Leave room for the two future Linux paths (including
/// JSON escaping), hashes, successor identifiers and record field names.
fn preflight_preparation_size(authority: &Authority) -> Result<(), String> {
    // A restored terminal's source/relaunch lineage is bounded by the complete
    // checkpoint (8 MiB). Budget its published descriptor, paired sessions and
    // current chats as well as the receipt itself before asking for shutdown.
    // Incidental output/title/activity metadata is never retained as proof.
    const FUTURE_METADATA: usize = 1024 * 1024;
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
                .filter_map(|chat| chat.terminal.as_ref()),
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
        ),
        LIMIT as usize - SNAPSHOT_LIMIT - FUTURE_METADATA,
    )?;
    Ok(())
}

/// Retain only the immutable allocation/lineage and serving ownership fields
/// used by the proof reader. Output and attachment metadata can keep changing.
pub(crate) fn proof_terminal_descriptor(
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
    use std::os::unix::ffi::OsStrExt;
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
        let from =
            std::ffi::CString::new(temporary.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        let to = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        // Publish complete bytes atomically without ever replacing a prior
        // binding. A failed write cannot strand a partial bound-census file.
        if unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } != 0
        {
            return Err(format!(
                "Could not publish immutable recovery census: {}",
                io::Error::last_os_error()
            ));
        }
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
            || checkpoint.terminals.len() + checkpoint.carried.len() > 1024
            || checkpoint.chats.len() > 128
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
            || terminals.len() > 1024
            || chats.len() > 128
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

    pub(crate) fn verify(
        &self,
        serving: &HostSessionProof,
        live_terminals: &[crate::terminal_ipc::TerminalDescriptor],
        live_chats: &[Session],
    ) -> Result<RecoveryOutcome, String> {
        verify_record(
            &self.authority.checkpoint_path,
            &self.authority.checkpoint_hash,
            &self.authority.source,
            &self.authority.census,
            &self.authority.target,
            Some(&self.authority),
            Some(&self.authority.target_hash),
            serving,
            live_terminals,
            live_chats,
        )
    }
}

/// The same verifier serves a live sealed receipt and a read-only retry probe.
/// A record's shape, a held lease, or a binary rollback alone proves nothing.
fn verify_record(
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
) -> Result<RecoveryOutcome, String> {
    let record = read_verified_record(
        checkpoint_path,
        checkpoint_hash,
        source,
        census,
        target,
        request,
        target_hash,
        serving,
        live_terminals,
        live_chats,
    )?;
    Ok(RecoveryOutcome {
        paused_terminals: record.paused_terminals,
        paused_chats: record.paused_chats,
    })
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
    let path = checkpoint_path.with_file_name(recovery_name(checkpoint_hash));
    let bytes = read_checkpoint(&path)?.ok_or("The recovery daemon has no durable outcome")?;
    let record: RecoveryRecord = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let checkpoint = read_checkpoint(checkpoint_path)?
        .ok_or("The Prepared checkpoint disappeared after recovery")?;
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
                .filter_map(|chat| chat.terminal.as_ref()),
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
        origin: Origin::AcknowledgedPrepare,
        job: String::new(),
        request_identity: String::new(),
        job_request_hash: String::new(),
        reviewed: Some(daemon.clone()),
        source: daemon.capability.proof.clone(),
        census: census.clone(),
        windows: Vec::new(),
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
    let mut file = match OpenOptions::new()
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
pub(crate) struct SettledWitness {
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
pub(crate) struct AlreadyRecovered {
    pub source: HostSessionProof,
    pub census: Census,
    pub paused_terminals: Vec<TerminalId>,
    pub paused_chats: Vec<TerminalId>,
    pub witness: SettledWitness,
}

fn validate_settled_record(
    record: &RecoveryRecord,
    checkpoint: &[u8],
    checkpoint_path: &Path,
    target: &Path,
) -> Result<(), String> {
    super::handoff::validate_checkpoint_bytes(checkpoint)?;
    let index: CheckpointIndex = serde_json::from_slice(checkpoint).map_err(|e| e.to_string())?;
    let job = uuid::Uuid::parse_str(&record.job).map_err(|e| e.to_string())?;
    if record.version != 2
        || !record.settled
        || job.to_string() != record.job
        || record.checkpoint_hash != digest(checkpoint)
        || record.source != index.proof
        || index.phase != "Prepared"
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
            .iter()
            .flat_map(|reviewed| reviewed.sessions.iter())
            .map(|item| (item.kind, item.id))
            .collect(),
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
    if validate_checkpoint(&authority, checkpoint)? != "Prepared" {
        return Err("Settled recovery no longer names a Prepared checkpoint".into());
    }
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
    Ok(())
}

/// Classify exact settled history only. Orphan adoption still authenticates
/// every current holder and cannot execute a recipe based on this record.
pub(crate) fn classify_retained_prepared(
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
    let witness = SettledWitness {
        checkpoint_hash: hash,
        record_hash: digest(&bytes),
        target,
        target_hash: record.target_hash.clone(),
        successor: record
            .successor
            .clone()
            .ok_or("Settled recovery has no successor")?,
        adopted_chats: record.adopted_chat_descriptors.clone(),
        adopted_terminals: record.adopted_terminal_descriptors.clone(),
    };
    Ok(Some(AlreadyRecovered {
        source: record.source,
        census: record.census,
        paused_terminals: record.paused_terminals,
        paused_chats: record.paused_chats,
        witness,
    }))
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
        self.adopted_terminal_descriptors = std::mem::take(&mut self.adopted_terminal_descriptors)
            .into_iter()
            .map(proof_terminal_descriptor)
            .collect();
        self.successor = Some(successor.clone());
        self.settled = true;
        self.save()?;
        drop(self.authority.take());
        Ok(())
    }
}

/// Tests join real holder authentication to the ordinary read-only proof
/// consumer without fabricating a live receipt or automatic source ACK.
#[cfg(all(test, feature = "host"))]
pub(crate) fn fixture_settled_outcome(
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

    fn reviewed(proof: &HostSessionProof, terminal: TerminalId) -> DaemonImpact {
        DaemonImpact {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof: proof.clone(),
                token: "reviewed".into(),
                inventory_generation: 1,
                live_terminals: 1,
                background_subagents: 0,
                background_identity: String::new(),
                expires_at_unix_ms: 1,
            },
            policy: crate::update::impact::Policy::WhenIdle,
            sessions: vec![crate::update::impact::SessionImpact {
                id: terminal,
                kind: SessionKind::Terminal,
                title: String::new(),
                identity: "exact-owner".into(),
                disposition: crate::update::impact::Disposition::Resumes,
                reason: String::new(),
                interrupted_subagents: Vec::new(),
            }],
            blockers: Vec::new(),
        }
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
        let job = uuid::Uuid::new_v4().to_string();
        let request = b"the exact originating private update request";
        save(&root.join(format!("{job}.request")), request);
        let reviewed = reviewed(&source, terminal.id);
        let request_identity = digest(&serde_json::to_vec(&reviewed).unwrap());
        Preparation {
            worker: lock(&worker_path),
            fence: lock(&fence_path),
            legacy,
            authority: Authority {
                version: 1,
                origin: Origin::AcknowledgedPrepare,
                job,
                request_identity,
                job_request_hash: digest(request),
                reviewed: Some(reviewed),
                source,
                expected: vec![(SessionKind::Terminal, terminal.id)],
                windows: Vec::new(),
                census: Census {
                    terminals: vec![terminal],
                    chats: Vec::new(),
                    snapshots: Vec::new(),
                    ..Default::default()
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
            "terminals": [], "carried": [{"source": preparation.authority.census.terminals[0], "options": crate::terminal_ipc::TerminalSpawnOptions::default()}], "chats": [],
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
    fn empty_or_partial_recovery_claim_is_present_without_authorizing_a_retry() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let bytes = checkpoint(&preparation, "Prepared");
        save(&preparation.authority.checkpoint_path, &bytes);
        let pending = PendingRecovery::inspect_at(&preparation.authority.checkpoint_path)
            .unwrap()
            .unwrap();
        assert!(!pending.has_recovery_claim().unwrap());
        let claim = pending
            .checkpoint_path
            .with_file_name(recovery_name(&pending.checkpoint_hash));
        for contents in [b"".as_slice(), b"{\"settled\":".as_slice()] {
            save(&claim, contents);
            assert!(pending.has_recovery_claim().unwrap());
            assert_eq!(fs::read(&claim).unwrap(), contents);
            assert_eq!(fs::read(&pending.checkpoint_path).unwrap(), bytes);
        }
        fs::set_permissions(&claim, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            pending.has_recovery_claim().is_err(),
            "unsafe evidence must not be mistaken for claim absence"
        );
    }

    #[test]
    fn pending_recovery_rejects_reduced_index_with_missing_host_options_without_mutation() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let path = &preparation.authority.checkpoint_path;
        let mut reduced: serde_json::Value =
            serde_json::from_slice(&checkpoint(&preparation, "Prepared")).unwrap();
        reduced["carried"][0]
            .as_object_mut()
            .unwrap()
            .remove("options");
        let bytes = serde_json::to_vec(&reduced).unwrap();
        save(path, &bytes);
        assert!(PendingRecovery::inspect_at(path).is_err());
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert!(
            !path.with_file_name(recovery_name(&digest(&bytes))).exists(),
            "malformed checkpoint must not consume a one-shot claim"
        );
        assert!(
            !capture_path(path, &preparation.authority.source)
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn explicit_recovery_request_binds_original_census_windows_and_typed_origin() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut authority = preparation(root.path()).authority;
        authority.origin = Origin::ExplicitRecovery {
            checkpoint: "a".repeat(64),
        };
        authority.reviewed = None;
        authority.checkpoint_hash = "a".repeat(64);
        authority.windows = vec!["primary".into()];
        authority.target = PathBuf::from("/candidate/bin/cagentsd");
        authority.target_hash = "b".repeat(64);
        let request = ManualRequest {
            version: 1,
            job: authority.job.clone(),
            checkpoint: authority.checkpoint_hash.clone(),
            source: authority.source.clone(),
            target: authority.target.clone(),
            target_hash: authority.target_hash.clone(),
            census_hash: digest(&serde_json::to_vec(&authority.census).unwrap()),
            windows: authority.windows.clone(),
        };
        let bytes = serde_json::to_vec(&request).unwrap();
        authority.request_identity = digest(&bytes);
        assert!(validate_request_origin(&authority, &bytes).is_ok());
        let original = authority.census.clone();
        authority.census.terminals.clear();
        assert!(
            validate_request_origin(&authority, &bytes).is_err(),
            "a changed or empty retry census cannot reuse explicit authorization"
        );
        authority.census = original;
        authority.windows.clear();
        assert!(validate_request_origin(&authority, &bytes).is_err());
        authority.windows = request.windows;
        authority.origin = Origin::ExplicitRecovery {
            checkpoint: "c".repeat(64),
        };
        assert!(validate_request_origin(&authority, &bytes).is_err());
    }

    #[test]
    fn pending_prepared_keeps_original_census_and_exact_window_slots_for_retry() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut preparation = preparation(root.path());
        let path = &preparation.authority.checkpoint_path;
        let bytes = checkpoint(&preparation, "Prepared");
        save(path, &bytes);
        let partial = PendingRecovery::inspect_at(path).unwrap().unwrap();
        assert!(
            !partial.original_census_complete,
            "checkpoint subset alone is not the original full census"
        );
        let mut original = preparation.authority.census.clone();
        let mut stopped = original.terminals[0].clone();
        stopped.id = TerminalId::new();
        original.terminals.push(stopped.clone());
        persist_capture(
            path,
            &CapturedCensus {
                producer: None,
                source: preparation.authority.source.clone(),
                checkpoint_hash: Some(digest(&bytes)),
                census: original.clone(),
                windows: vec!["primary".into()],
            },
        )
        .unwrap();
        let unbound = PendingRecovery::inspect_at(path).unwrap().unwrap();
        assert!(
            !unbound.original_census_complete,
            "an abandoned same-source capture must never prove an arbitrary later checkpoint"
        );
        preparation.authority.census = original.clone();
        preparation.authority.windows = vec!["primary".into()];
        let captured = preparation.acknowledged().unwrap();
        let path = &captured.0.authority.checkpoint_path;
        let pending = PendingRecovery::inspect_at(path).unwrap().unwrap();
        assert!(pending.original_census_complete);
        assert_eq!(
            pending.census, original,
            "retry must retain stopped owners missing from the Prepared subset"
        );
        assert_eq!(pending.windows, ["primary"]);
        assert_eq!(pending.checkpoint_hash, digest(&bytes));
        assert_eq!(
            fs::read(path).unwrap(),
            bytes,
            "inspection must never promote or rewrite Prepared"
        );
        let command = pending.command(Path::new("/candidate with quote'/bin/cagents"), None);
        assert!(command.contains("--no-start daemon recover-update --checkpoint"));
        assert!(command.contains("--yes"));
        assert!(command.starts_with("'/candidate with quote'\\''/bin/cagents'"));
        let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        changed["version"] = serde_json::json!(3);
        save(path, &serde_json::to_vec(&changed).unwrap());
        assert!(
            !PendingRecovery::inspect_at(path)
                .unwrap()
                .unwrap()
                .original_census_complete,
            "same source and scope do not waive a bound checkpoint digest"
        );
    }

    #[test]
    fn pending_original_census_rejects_foreign_source_and_preserves_prepared_bytes() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let path = &preparation.authority.checkpoint_path;
        let bytes = checkpoint(&preparation, "Prepared");
        save(path, &bytes);
        let pending = PendingRecovery::inspect_at(path).unwrap().unwrap();
        let census_path = root.path().join("original.json");
        let mut original = serde_json::json!({
            "version":1, "transaction":uuid::Uuid::new_v4().to_string(),
            "source":preparation.authority.source, "terminals":preparation.authority.census.terminals,
            "chats":[], "checkpoint_before":null,
        });
        save(&census_path, &serde_json::to_vec(&original).unwrap());
        let bound = pending.clone().with_original_census(&census_path).unwrap();
        assert!(bound.original_census_complete);
        original["source"]["host_incarnation"] = serde_json::json!("foreign-owner");
        save(&census_path, &serde_json::to_vec(&original).unwrap());
        assert!(pending.clone().with_original_census(&census_path).is_err());
        original["source"] = serde_json::to_value(&preparation.authority.source).unwrap();
        original["checkpoint_before"] = serde_json::json!(pending.checkpoint_hash);
        save(&census_path, &serde_json::to_vec(&original).unwrap());
        assert!(
            pending.with_original_census(&census_path).is_err(),
            "a census captured after this Prepared checkpoint cannot replace the original scope"
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn exact_adopted_terminal_proof_ignores_output_but_not_allocation_changes() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let source = preparation.authority.source;
        let mut issued = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            preparation.authority.census.terminals[0].id,
            preparation.authority.census.terminals[0].launch.clone(),
            None,
        );
        issued.session = preparation.authority.census.terminals[0].clone();
        let mut actual = issued.clone();
        actual.event_sequence += 100;
        actual.generation += 1;
        actual.title = Some("Output changed the title".into());
        assert!(same_adopted_terminal(&issued, &actual, &source));
        actual
            .session
            .launch
            .arguments
            .push("foreign-command".into());
        assert!(!same_adopted_terminal(&issued, &actual, &source));
        actual = issued.clone();
        actual.upgrade_sources.push(issued.session.clone());
        assert!(!same_adopted_terminal(&issued, &actual, &source));
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
    fn captured_pane_aliases_and_presentations_bind_the_exact_confirmed_allocation() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let known = Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: preparation.authority.census.terminals[0].launch.clone(),
                session_id: Some("confirmed-thread".into()),
            },
            proof: Some(preparation.authority.source.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let mut alias = known.clone();
        alias.launch.session_id = None;
        let agent = crate::model::AgentId::new();
        let presentation = super::super::floor::Presentation {
            agent,
            session: known.clone(),
            scope: crate::workspace_model::views::Scope::Channel(crate::model::ChannelId::new()),
            pane: crate::workspace_model::views::Pane {
                id: crate::model::CardId::from_uuid(agent.as_uuid()),
                title: "Exact paused pane".into(),
                user_title: None,
                pending_terminal_start: false,
                width_fraction: 0.5,
                kind: crate::model::CardKind::Chat(alias.clone()),
                scope: None,
            },
            ordinal: 0,
            restored: false,
        };
        let census = Census {
            chats: vec![known],
            aliases: vec![alias],
            presentations: vec![presentation],
            ..Default::default()
        };
        census.validate_presentations().unwrap();
        for changed in ["proof", "thread", "recipe", "allocation", "owner", "pane"] {
            let mut invalid = census.clone();
            match changed {
                "proof" => {
                    invalid.aliases[0].proof.as_mut().unwrap().host_incarnation =
                        HostIncarnation::new("foreign")
                }
                "thread" => invalid.aliases[0].launch.session_id = Some("unrelated-thread".into()),
                "recipe" => invalid.aliases[0]
                    .launch
                    .recipe
                    .arguments
                    .push("changed".into()),
                "allocation" => invalid.aliases[0].id = TerminalId::new(),
                "owner" => invalid.presentations[0].agent = crate::model::AgentId::new(),
                "pane" => {
                    invalid.presentations[0].pane.kind = crate::model::CardKind::Chat({
                        let mut unrelated = invalid.aliases[0].clone();
                        unrelated.id = TerminalId::new();
                        unrelated
                    })
                }
                _ => unreachable!(),
            }
            assert!(invalid.validate_presentations().is_err(), "{changed}");
        }
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
    fn recovery_receipt_releases_its_daemon_reservation_before_owner_discovery() {
        const ISOLATED: &str = "CAGENTS_TEST_RECOVERY_RESERVATION_ISOLATED";
        if std::env::var_os(ISOLATED).is_none() {
            // Parallel libtests may fork while this test owns a flock. Even a
            // CLOEXEC duplicate extends that open-file description until exec.
            // Allocate every tested lock only after this single-test exec.
            let selector = std::thread::current().name().unwrap().to_owned();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
                .env(ISOLATED, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        save(
            &preparation.authority.checkpoint_path,
            &checkpoint(&preparation, "Prepared"),
        );
        let captured = preparation.acknowledged().unwrap();
        let handoff = handoff(root.path());
        let mut receipt = captured
            .receipt(&fs::canonicalize("/bin/sh").unwrap(), &handoff)
            .unwrap()
            .unwrap();
        let path = descriptor_path(receipt.daemon.as_ref().unwrap()).unwrap();
        let probe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        drop(handoff);
        let held = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        let held_error = io::Error::last_os_error();
        assert_eq!(held, -1, "the receipt must retain the daemon lease");
        assert_eq!(held_error.kind(), io::ErrorKind::WouldBlock, "{held_error}");
        receipt.release_daemon_reservation();
        let released = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        let released_error = io::Error::last_os_error();
        assert_eq!(
            released, 0,
            "the daemon lease was not released: {released_error}"
        );
        assert!(receipt.inherit(&mut Command::new("/bin/true")).is_err());
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
            &serving,
            Path::new("/candidate/bin/cagentsd")
        ));
        actual.session.launch.arguments.push("unrelated".into());
        assert!(!verified_terminal(
            &preparation.authority.census,
            &actual,
            &serving,
            Path::new("/candidate/bin/cagentsd")
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
    fn prepared_recovery_verifies_the_exact_retained_console_replacement_lineage() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let mut known = preparation.authority.census.terminals[0].clone();
        let agent = crate::model::AgentId::new();
        known.launch.program = "/original/bin/cagents".into();
        known.launch.arguments = vec!["agent".into(), "console".into(), agent.to_string()];
        let source = Session {
            id: known.id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    ..known.launch.clone()
                },
                session_id: Some("retained-conversation".into()),
            },
            proof: Some(preparation.authority.source.clone()),
            terminal: Some(known.clone()),
            owned_codex_listener: None,
            native_continuity: crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        };
        let census = Census {
            terminals: vec![known.clone()],
            chats: vec![source.clone()],
            snapshots: Vec::new(),
            ..Default::default()
        };
        let mut serving = preparation.authority.source.clone();
        serving.host_incarnation = HostIncarnation::new("successor");
        let target = Path::new("/candidate/bin/cagentsd");
        let mut actual = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            known.id,
            known.launch.clone(),
            None,
        );
        actual.session = known.clone();
        actual.session.launch.program = "/candidate/bin/cagents".into();
        actual.session.continuity = TerminalContinuity::DetachedSameBoot {
            host_session_id: serving.host_session_id.clone(),
            boot_id: serving.boot_id.clone(),
            host_incarnation: serving.host_incarnation.clone(),
        };
        actual.upgrade_sources.push(known.clone());
        actual
            .upgrade_relaunches
            .push(crate::terminal_ipc::UpgradeRelaunch {
                source: known.clone(),
                recipe: actual.session.launch.clone(),
            });
        let mut chat = source.clone();
        chat.proof = Some(serving.clone());
        chat.terminal = Some(actual.session.clone());
        assert!(verified_terminal(&census, &actual, &serving, target));
        assert!(verified_chat(
            &census,
            &chat,
            &serving,
            &[actual.clone()],
            target
        ));
        assert!(!verified_chat(&census, &chat, &serving, &[], target));

        let mut changed = actual.clone();
        changed.upgrade_relaunches.clear();
        assert!(!verified_terminal(&census, &changed, &serving, target));
        changed = actual.clone();
        changed.upgrade_sources.clear();
        assert!(!verified_terminal(&census, &changed, &serving, target));
        changed = actual.clone();
        changed.upgrade_relaunches[0].source.id = TerminalId::new();
        assert!(!verified_terminal(&census, &changed, &serving, target));
        changed = actual.clone();
        changed.upgrade_relaunches[0]
            .recipe
            .arguments
            .push("another-command".into());
        assert!(!verified_terminal(&census, &changed, &serving, target));
        assert!(!verified_terminal(
            &census,
            &actual,
            &serving,
            Path::new("/unrelated/bin/cagentsd")
        ));
        let mutations: [fn(&mut LaunchRecipe); 4] = [
            |recipe: &mut LaunchRecipe| recipe.arguments.push("another-command".into()),
            |recipe: &mut LaunchRecipe| recipe.working_directory.push("another-directory"),
            |recipe: &mut LaunchRecipe| {
                recipe.environment.insert("OTHER".into(), "value".into());
            },
            |recipe: &mut LaunchRecipe| {
                recipe.unset_environment.insert("OTHER".into());
            },
        ];
        for mutate in mutations {
            changed = actual.clone();
            mutate(&mut changed.session.launch);
            changed.upgrade_relaunches[0].recipe = changed.session.launch.clone();
            assert!(!verified_terminal(&census, &changed, &serving, target));
        }
        let mut unpaired = census.clone();
        unpaired.chats.clear();
        assert!(!verified_terminal(&unpaired, &actual, &serving, target));
        unpaired = census.clone();
        unpaired.chats[0].native_continuity = Default::default();
        assert!(!verified_terminal(&unpaired, &actual, &serving, target));
        chat.terminal
            .as_mut()
            .unwrap()
            .launch
            .environment
            .insert("OTHER".into(), "value".into());
        assert!(!verified_chat(
            &census,
            &chat,
            &serving,
            &[actual.clone()],
            target
        ));
        chat.terminal = Some(actual.session.clone());
        chat.launch.session_id = Some("different-conversation".into());
        assert!(!verified_chat(&census, &chat, &serving, &[actual], target));
    }

    #[test]
    fn recovery_outcome_requires_disjoint_complete_inventory_including_paired_terminals() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let terminal = preparation.authority.census.terminals[0].clone();
        let mut record = RecoveryRecord {
            version: 2,
            origin: preparation.authority.origin.clone(),
            job: preparation.authority.job.clone(),
            request_identity: preparation.authority.request_identity.clone(),
            job_request_hash: preparation.authority.job_request_hash.clone(),
            reviewed: preparation.authority.reviewed.clone(),
            windows: preparation.authority.windows.clone(),
            source: preparation.authority.source.clone(),
            checkpoint_hash: String::new(),
            census: preparation.authority.census.clone(),
            adopted_terminals: Vec::new(),
            adopted_chats: Vec::new(),
            adopted_chat_descriptors: Vec::new(),
            adopted_terminal_descriptors: Vec::new(),
            paused_terminals: vec![terminal.id],
            paused_chats: Vec::new(),
            settled: false,
            successor: None,
            target_hash: String::new(),
            target: None,
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
    #[test]
    fn reviewed_payload_budget_is_rejected_before_source_preparation() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut preparation = preparation(root.path());
        preflight_preparation_size(&preparation.authority).unwrap();
        preparation.authority.reviewed.as_mut().unwrap().sessions[0].reason =
            "x".repeat(LIMIT as usize);
        let error = preflight_preparation_size(&preparation.authority).unwrap_err();
        assert!(error.contains("byte limit"), "{error}");
        assert!(!preparation.authority.checkpoint_path.exists());
        assert!(preparation.authority.checkpoint_hash.is_empty());
    }

    #[test]
    fn preparation_budget_includes_future_adopted_descriptor_copies_and_checkpoint_lineage() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut preparation = preparation(root.path());
        preparation.authority.census.terminals[0]
            .launch
            .environment
            .insert(
                "LARGE_CAPTURED_ENVIRONMENT".into(),
                "x".repeat(4 * 1024 * 1024),
            );
        // A receipt alone fits. Its later descriptor also carries the current
        // allocation and checkpoint-issued sources, so preflight must reserve
        // those copies before any source shutdown can be requested.
        encoded_size(&preparation.authority, LIMIT as usize).unwrap();
        assert!(preflight_preparation_size(&preparation.authority).is_err());
        assert!(!preparation.authority.checkpoint_path.exists());
    }

    #[test]
    fn settled_prepared_is_read_only_history_without_active_update_authority() {
        let root = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/var/tmp")
            .unwrap();
        let preparation = preparation(root.path());
        let checkpoint_bytes = checkpoint(&preparation, "Prepared");
        let authority = preparation.authority.clone();
        drop(preparation); // Cold startup has no worker or window fence authority.
        save(&authority.checkpoint_path, &checkpoint_bytes);
        let prefix = root.path().join("install");
        let bin = prefix.join("versions/fixture/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            prefix.join(".cagents-install-root"),
            "cagents install root\n",
        )
        .unwrap();
        for name in ["cagents", "cagents-ui", "cagentsd"] {
            fs::write(bin.join(name), format!("private fake {name}")).unwrap();
        }
        let target = fs::canonicalize(bin.join("cagentsd")).unwrap();
        let state = prefix.join(".update");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let request = b"the immutable original job request";
        save(&state.join(format!("{}.request", authority.job)), request);
        let successor = HostSessionProof {
            host_session_id: HostSessionId::new("first-recovered-daemon"),
            boot_id: authority.source.boot_id.clone(),
            host_incarnation: HostIncarnation::new("recovered-incarnation"),
        };
        let record = RecoveryRecord {
            version: 2,
            origin: authority.origin,
            job: authority.job,
            request_identity: digest(
                &serde_json::to_vec(authority.reviewed.as_ref().unwrap()).unwrap(),
            ),
            job_request_hash: digest(request),
            reviewed: authority.reviewed,
            windows: authority.windows,
            source: authority.source,
            checkpoint_hash: digest(&checkpoint_bytes),
            census: authority.census,
            adopted_terminals: Vec::new(),
            adopted_chats: Vec::new(),
            adopted_chat_descriptors: Vec::new(),
            adopted_terminal_descriptors: Vec::new(),
            paused_terminals: vec![authority.expected[0].1],
            paused_chats: Vec::new(),
            settled: true,
            successor: Some(successor),
            target: Some(target.clone()),
            target_hash: executable_hash(&target).unwrap(),
            #[cfg(feature = "host")]
            authority: None,
        };
        let sidecar = authority
            .checkpoint_path
            .with_file_name(recovery_name(&record.checkpoint_hash));
        let original = serde_json::to_vec(&record).unwrap();
        save(&sidecar, &original);
        // No status.json or update lock exists; a later plain restart uses the
        // original immutable request and exact finished outcome only.
        for _ in 0..2 {
            let classified = classify_retained_prepared(&authority.checkpoint_path, &target)
                .unwrap()
                .unwrap();
            assert_eq!(classified.source, record.source);
            assert_eq!(classified.census, record.census);
            assert_eq!(classified.paused_terminals, record.paused_terminals);
            assert!(classified.paused_chats.is_empty());
            assert_eq!(
                fs::read(&authority.checkpoint_path).unwrap(),
                checkpoint_bytes
            );
            assert_eq!(fs::read(&sidecar).unwrap(), original);
        }
        let baseline: serde_json::Value = serde_json::from_slice(&original).unwrap();
        // Explicit manual recovery has its own immutable request, not an
        // invented source review or ACK. Cold classification validates both
        // origins exhaustively through the same owner.
        let manual = ManualRequest {
            version: 1,
            job: record.job.clone(),
            checkpoint: record.checkpoint_hash.clone(),
            source: record.source.clone(),
            target: target.clone(),
            target_hash: record.target_hash.clone(),
            census_hash: digest(&serde_json::to_vec(&record.census).unwrap()),
            windows: record.windows.clone(),
        };
        let manual_request = serde_json::to_vec(&manual).unwrap();
        let mut explicit = baseline.clone();
        explicit["origin"] =
            serde_json::json!({"kind":"explicit_recovery","checkpoint":record.checkpoint_hash});
        explicit["reviewed"] = serde_json::Value::Null;
        explicit["job_request_hash"] = digest(&manual_request).into();
        explicit["request_identity"] = digest(&manual_request).into();
        let request_path = state.join(format!("{}.request", record.job));
        save(&request_path, &manual_request);
        save(&sidecar, &serde_json::to_vec(&explicit).unwrap());
        assert!(
            classify_retained_prepared(&authority.checkpoint_path, &target)
                .unwrap()
                .is_some()
        );
        let mut mixed = explicit.clone();
        mixed["reviewed"] = baseline["reviewed"].clone();
        save(&sidecar, &serde_json::to_vec(&mixed).unwrap());
        assert!(
            classify_retained_prepared(&authority.checkpoint_path, &target).is_err(),
            "manual recovery must not invent an acknowledged review"
        );
        mixed = explicit;
        mixed["origin"] = serde_json::json!({"kind":"acknowledged_prepare"});
        save(&sidecar, &serde_json::to_vec(&mixed).unwrap());
        assert!(
            classify_retained_prepared(&authority.checkpoint_path, &target).is_err(),
            "automatic recovery requires its real reviewed source payload"
        );
        save(&request_path, request);
        save(&sidecar, &original);
        for (field, replacement) in [
            ("version", serde_json::json!(1)),
            ("settled", serde_json::json!(false)),
            ("job", serde_json::json!(uuid::Uuid::new_v4().to_string())),
            ("request_identity", serde_json::json!("0".repeat(64))),
            ("job_request_hash", serde_json::json!("0".repeat(64))),
            ("checkpoint_hash", serde_json::json!("0".repeat(64))),
            (
                "source",
                serde_json::to_value(record.successor.as_ref().unwrap()).unwrap(),
            ),
            ("successor", serde_json::Value::Null),
            ("successor", serde_json::to_value(&record.source).unwrap()),
            (
                "successor",
                serde_json::json!({
                    "host_session_id": "recovered-session",
                    "boot_id": "different-boot",
                    "host_incarnation": "recovered-daemon",
                }),
            ),
            ("target", serde_json::to_value(bin.join("cagents")).unwrap()),
            ("target_hash", serde_json::json!("0".repeat(64))),
            ("paused_terminals", serde_json::json!([])),
            (
                "adopted_terminals",
                serde_json::json!(record.paused_terminals),
            ),
        ] {
            let mut changed = baseline.clone();
            changed[field] = replacement;
            save(&sidecar, &serde_json::to_vec(&changed).unwrap());
            assert!(
                classify_retained_prepared(&authority.checkpoint_path, &target).is_err(),
                "accepted changed {field}"
            );
            assert_eq!(
                fs::read(&authority.checkpoint_path).unwrap(),
                checkpoint_bytes
            );
        }
        let mut duplicate = baseline.clone();
        duplicate["census"]["terminals"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::to_value(&record.census.terminals[0]).unwrap());
        save(&sidecar, &serde_json::to_vec(&duplicate).unwrap());
        assert!(classify_retained_prepared(&authority.checkpoint_path, &target).is_err());
        save(&sidecar, &original);
        let original_target = fs::read(&target).unwrap();
        fs::write(&target, b"different daemon bytes at the same path").unwrap();
        assert!(classify_retained_prepared(&authority.checkpoint_path, &target).is_err());
        fs::write(&target, original_target).unwrap();
        let request_path = state.join(format!("{}.request", record.job));
        save(&request_path, b"a different request for the same job");
        assert!(classify_retained_prepared(&authority.checkpoint_path, &target).is_err());
        save(&request_path, request);
        save(&sidecar, &original);
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(classify_retained_prepared(&authority.checkpoint_path, &target).is_err());
        fs::remove_file(&sidecar).unwrap();
        assert!(classify_retained_prepared(&authority.checkpoint_path, &target).is_err());
        assert_eq!(
            fs::read(&authority.checkpoint_path).unwrap(),
            checkpoint_bytes
        );
    }

    #[test]
    fn retained_checkpoint_without_prepared_phase_has_no_settled_classification() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let preparation = preparation(root.path());
        let path = &preparation.authority.checkpoint_path;
        assert!(
            classify_retained_prepared(path, Path::new("/no/daemon"))
                .unwrap()
                .is_none()
        );
        save(path, &checkpoint(&preparation, "Ready"));
        let before = fs::read(path).unwrap();
        assert!(
            classify_retained_prepared(path, Path::new("/no/daemon"))
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read(path).unwrap(), before);
        // A legacy Ready checkpoint may use recipe DTOs outside this
        // classifier's current census contract. Ordinary restore owns it.
        save(
            path,
            br#"{"phase":"Ready","chats":[{"legacy_recipe":"opaque"}]}"#,
        );
        assert!(
            classify_retained_prepared(path, Path::new("/no/daemon"))
                .unwrap()
                .is_none()
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
