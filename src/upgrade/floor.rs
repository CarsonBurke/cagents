//! Durable evidence of the exact panes a person chose to stop for an update.
//! It prevents orphan adoption and automatic reopening from undoing that choice.
use crate::{
    chat::Session,
    model::{HostSessionProof, TerminalId},
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

const LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Record {
    pub source: HostSessionProof,
    #[serde(default)]
    pub protocol: u16,
    #[serde(default)]
    pub service_stopped: bool,
    #[serde(default)]
    pub snapshots: Vec<StoppedSnapshot>,
    #[serde(default)]
    pub aliases: Vec<Session>,
    pub chats: Vec<Session>,
    pub terminals: Vec<TerminalId>,
    #[serde(default)]
    pub presentations: Vec<Presentation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoppedSnapshot {
    pub source: Session,
    pub snapshot: crate::chat::Snapshot,
    #[serde(default)]
    pub limitation: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Presentation {
    pub agent: crate::model::AgentId,
    pub session: Session,
    pub scope: crate::workspace_model::views::Scope,
    pub pane: crate::workspace_model::views::Pane,
    pub ordinal: usize,
    #[serde(default)]
    pub restored: bool,
}

#[derive(Default)]
pub(crate) struct Ledger {
    pub records: Vec<Record>,
}

/// The startup index deliberately skips transcript payloads while decoding.
/// Opening one stopped pane loads only its own source's bounded record.
#[derive(Deserialize)]
struct RecordIndex {
    source: HostSessionProof,
    #[serde(default)]
    protocol: u16,
    #[serde(default)]
    service_stopped: bool,
    #[serde(default, rename = "snapshots")]
    _snapshots: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    aliases: Vec<Session>,
    chats: Vec<Session>,
    terminals: Vec<TerminalId>,
    #[serde(default)]
    presentations: Vec<Presentation>,
}

impl From<RecordIndex> for Record {
    fn from(index: RecordIndex) -> Self {
        Self {
            source: index.source,
            protocol: index.protocol,
            service_stopped: index.service_stopped,
            snapshots: Vec::new(),
            aliases: index.aliases,
            chats: index.chats,
            terminals: index.terminals,
            presentations: index.presentations,
        }
    }
}

impl Record {
    pub fn new(source: HostSessionProof, chats: Vec<Session>, terminals: Vec<TerminalId>) -> Self {
        Self {
            source,
            protocol: 0,
            service_stopped: false,
            snapshots: Vec::new(),
            aliases: Vec::new(),
            chats,
            terminals,
            presentations: Vec::new(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let directory = directory()?;
        crate::secure_file::prepare_private_directory(&directory).map_err(|e| e.to_string())?;
        let root = crate::fs_safety::Root::open(&directory).map_err(|e| e.to_string())?;
        let mut record = self.clone();
        if let Some(previous) = record_for_source(&record.source)? {
            record.service_stopped |= previous.service_stopped;
            for alias in previous.aliases {
                if !record.aliases.contains(&alias) {
                    record.aliases.push(alias);
                }
            }
            for snapshot in previous.snapshots {
                if !record
                    .snapshots
                    .iter()
                    .any(|current| current.source.id == snapshot.source.id)
                {
                    record.snapshots.push(snapshot);
                }
            }
            for chat in previous.chats {
                if !record.chats.iter().any(|current| current.id == chat.id) {
                    record.chats.push(chat);
                }
            }
            for id in previous.terminals {
                if !record.terminals.contains(&id) {
                    record.terminals.push(id);
                }
            }
            for pane in previous.presentations {
                if !record
                    .presentations
                    .iter()
                    .any(|current| current.agent == pane.agent)
                {
                    record.presentations.push(pane);
                }
            }
        }
        if !record.source.is_complete()
            || record
                .chats
                .iter()
                .any(|chat| chat.proof.as_ref() != Some(&record.source))
        {
            return Err("Stopped update panes have no exact source proof".into());
        }
        record.write_to(&root)
    }

    fn file_name(&self) -> Result<String, String> {
        use sha2::{Digest, Sha256};
        Ok(format!(
            "{:x}.json",
            Sha256::digest(serde_json::to_vec(&self.source).map_err(|e| e.to_string())?)
        ))
    }

    pub(crate) fn encode_capture(&mut self) -> Result<Vec<u8>, String> {
        encode_record(self, LIMIT)
    }

    fn write_to(&self, root: &crate::fs_safety::Root) -> Result<(), String> {
        let bytes = self.clone().encode_capture()?;
        let name = self.file_name()?;
        let (mut file, temporary) =
            crate::fs_safety::create_temporary_in(&root, 0o600).map_err(|e| e.to_string())?;
        let result = (|| {
            file.write_all(&bytes).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            crate::fs_safety::rename_within(&root, &temporary, OsStr::new(&name))
                .map_err(|e| e.to_string())?;
            crate::fs_safety::sync_directory(&root).map_err(|e| e.to_string())
        })();
        let _ = crate::fs_safety::unlink_within(&root, &temporary);
        result
    }
}

impl Ledger {
    pub fn source(&self, id: TerminalId) -> Option<&Session> {
        self.records
            .iter()
            .flat_map(|record| &record.chats)
            .find(|source| source.id == id)
    }

    pub fn resume_source<'a>(&'a self, session: &'a Session) -> Option<&'a Session> {
        // Prefer the exact issued descriptor when the same allocation has
        // survived more than one source epoch. Directory enumeration order
        // must not choose an older snapshot for a newer stopped reader.
        self.records
            .iter()
            .flat_map(|record| &record.chats)
            .find(|source| *source == session)
            .or_else(|| {
                self.records.iter().find_map(|record| {
                    record
                        .aliases
                        .contains(session)
                        .then(|| record.chats.iter().find(|source| source.id == session.id))
                        .flatten()
                })
            })
            .or_else(|| {
                self.records
                    .iter()
                    .flat_map(|record| &record.chats)
                    .find(|source| Self::is_history_replacement(source, session))
            })
            .or_else(|| {
                self.records
                    .iter()
                    .any(|record| {
                        record.service_stopped && session.proof.as_ref() == Some(&record.source)
                    })
                    .then_some(session)
            })
    }

    pub fn history_replacement(&self, source: &Session, replacement: &Session) -> bool {
        self.records.iter().any(|record| {
            record.chats.iter().any(|original| {
                original.id == source.id
                    && (source == original || record.aliases.contains(source))
                    && source.proof != replacement.proof
                    && Self::is_history_replacement(original, replacement)
            })
        })
    }

    fn is_history_replacement(original: &Session, replacement: &Session) -> bool {
        if original.proof == replacement.proof
            || !replacement
                .proof
                .as_ref()
                .is_some_and(HostSessionProof::is_complete)
        {
            return false;
        }
        let mut expected = original.clone();
        expected.proof = replacement.proof.clone();
        expected.reset_owned_codex_listener();
        expected.native_continuity = crate::chat::NativeContinuity::ReconnectOnly;
        expected.terminal = None;
        expected.owned_codex_listener = None;
        expected == *replacement
    }

    pub fn blocks_chat(&self, source: &Session) -> bool {
        self.records.iter().any(|record| {
            source.proof.as_ref() == Some(&record.source)
                && record.chats.iter().any(|chat| chat.id == source.id)
        })
    }
    pub fn blocks_terminal(&self, id: TerminalId) -> bool {
        self.records.iter().any(|record| {
            record.terminals.contains(&id)
                || record.chats.iter().any(|chat| {
                    chat.id == id
                        || chat
                            .terminal
                            .as_ref()
                            .is_some_and(|terminal| terminal.id == id)
                })
        })
    }
    pub fn blocks_id(&self, id: TerminalId) -> bool {
        self.blocks_terminal(id)
    }
}

/// Drop allocations that are no longer in a saved pane. A resumed pane has a
/// new allocation ID, so the old stop evidence cannot interfere with it. Keep
/// the installed source's record while it serves: an aborted update may still
/// have panes which its window has not flushed to disk yet.
#[cfg(feature = "host")]
pub(crate) fn prune_unreferenced(
    referenced: &std::collections::BTreeSet<TerminalId>,
) -> Result<(), String> {
    let paths = crate::daemon::RuntimePaths::discover().ok();
    prune_at(&directory()?, referenced, |source| {
        paths
            .as_ref()
            .is_some_and(|paths| crate::daemon::lease_owner(&paths.lock, source).is_ok())
    })
}

#[cfg(any(feature = "host", test))]
fn prune_at(
    directory: &Path,
    referenced: &std::collections::BTreeSet<TerminalId>,
    mut installed: impl FnMut(&HostSessionProof) -> bool,
) -> Result<(), String> {
    let Some(root) = open_directory(directory)? else {
        return Ok(());
    };
    for path in record_paths(directory)? {
        let path = path?;
        let mut record = read_record(&root, &path, true)?;
        if installed(&record.source) {
            continue;
        }
        record.prune_allocations(referenced);
        if record.chats.is_empty() && record.terminals.is_empty() {
            crate::fs_safety::unlink_within(&root, OsStr::new(&record.file_name()?))
                .map_err(|error| error.to_string())?;
        } else {
            record.write_to(&root)?;
        }
    }
    crate::fs_safety::sync_directory(&root).map_err(|error| error.to_string())
}

impl Record {
    #[cfg(any(feature = "host", test))]
    fn prune_allocations(&mut self, referenced: &std::collections::BTreeSet<TerminalId>) {
        let pending: std::collections::BTreeSet<_> = self
            .presentations
            .iter()
            .filter(|pane| !pane.restored)
            .map(|pane| pane.session.id)
            .collect();
        self.chats
            .retain(|chat| referenced.contains(&chat.id) || pending.contains(&chat.id));
        let kept: std::collections::BTreeSet<_> = self.chats.iter().map(|chat| chat.id).collect();
        self.aliases.retain(|chat| kept.contains(&chat.id));
        self.snapshots
            .retain(|saved| kept.contains(&saved.source.id));
        self.presentations
            .retain(|pane| kept.contains(&pane.session.id));
        self.terminals.retain(|id| referenced.contains(id));
        // Broad service ownership is only needed while that source is still
        // installed. Retain the exact affected pane identities afterwards.
        self.service_stopped = false;
    }
}

fn directory() -> Result<PathBuf, String> {
    crate::instance::project_dirs()
        .map(|dirs| dirs.data_local_dir().join("daemon-upgrade/update-floor"))
        .ok_or_else(|| "Cannot discover stopped update panes".into())
}

/// Read the current durable index for a recovery or lineage decision. Both the
/// daemon and guarded update helpers can change these records while an existing
/// daemon or window survives, so a process startup snapshot is not authoritative.
/// Transcript payloads are skipped; rendering uses its attached snapshot instead.
pub(crate) fn load() -> Result<Ledger, String> {
    #[cfg(test)]
    INDEX_READS.with(|reads| reads.set(reads.get() + 1));
    load_at(&directory()?)
}

#[cfg(test)]
thread_local! {
    static INDEX_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn index_reads_for_test() -> usize {
    INDEX_READS.with(std::cell::Cell::get)
}

fn open_directory(directory: &Path) -> Result<Option<crate::fs_safety::Root>, String> {
    crate::secure_file::reject_symlink_components(directory).map_err(|e| e.to_string())?;
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.is_dir()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err("Unsafe stopped update pane directory".into());
    }
    crate::fs_safety::Root::open(directory)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn record_paths(directory: &Path) -> Result<impl Iterator<Item = Result<PathBuf, String>>, String> {
    Ok(fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .filter_map(|entry| match entry {
            Ok(entry)
                if entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json") =>
            {
                Some(Ok(entry.path()))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error.to_string())),
        }))
}

fn read_record(
    root: &crate::fs_safety::Root,
    path: &Path,
    snapshots: bool,
) -> Result<Record, String> {
    let mut file = crate::fs_safety::open_beneath(
        root,
        Path::new(path.file_name().ok_or("Invalid stopped update record")?),
    )
    .map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() > LIMIT
    {
        return Err("Unsafe stopped update pane record".into());
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > LIMIT {
        return Err("Stopped update pane record exceeds its limit".into());
    }
    let record: Record = if snapshots {
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?
    } else {
        let index: RecordIndex =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        index.into()
    };
    if !record.source.is_complete()
        || record
            .chats
            .iter()
            .any(|chat| chat.proof.as_ref() != Some(&record.source))
    {
        return Err("Stopped update pane record has invalid ownership".into());
    }
    Ok(record)
}

fn load_at(directory: &Path) -> Result<Ledger, String> {
    let Some(root) = open_directory(directory)? else {
        return Ok(Ledger::default());
    };
    let mut ledger = Ledger::default();
    for path in record_paths(directory)? {
        ledger.records.push(read_record(&root, &path?, false)?);
    }
    Ok(ledger)
}

fn record_for_source(source: &HostSessionProof) -> Result<Option<Record>, String> {
    let directory = directory()?;
    let Some(root) = open_directory(&directory)? else {
        return Ok(None);
    };
    let path = directory.join(Record::new(source.clone(), Vec::new(), Vec::new()).file_name()?);
    if !path.try_exists().map_err(|error| error.to_string())? {
        return Ok(None);
    }
    let record = read_record(&root, &path, true)?;
    if record.source != *source {
        return Err("Stopped update record changed its source".into());
    }
    Ok(Some(record))
}

/// Preparation budgets its captured pages together with already-approved
/// floor evidence before any source shutdown can become irreversible.
pub(crate) fn existing_capture_size(source: &HostSessionProof) -> Result<usize, String> {
    record_for_source(source)?.map_or(Ok(0), |mut record| {
        record.encode_capture().map(|bytes| bytes.len())
    })
}

#[cfg(feature = "host")]
pub(crate) fn snapshot(source: &Session) -> Result<StoppedSnapshot, String> {
    let proof = source
        .proof
        .as_ref()
        .ok_or("The stopped transcript has no source ownership")?;
    record_for_source(proof)?
        .into_iter()
        .flat_map(|record| record.snapshots)
        .find(|saved| saved.source == *source)
        .ok_or_else(|| "The stopped update transcript is unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> HostSessionProof {
        HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("host"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("source"),
        }
    }
    pub(super) fn chat() -> Session {
        Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: crate::model::LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: "/work".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some("recorded".into()),
            },
            proof: Some(source()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        }
    }
    #[test]
    fn a_stopped_source_blocks_automatic_recovery_but_not_a_resume_successor() {
        let chat = chat();
        let terminal = TerminalId::new();
        let ledger = Ledger {
            records: vec![Record::new(source(), vec![chat.clone()], vec![terminal])],
        };
        assert!(ledger.blocks_chat(&chat));
        assert!(ledger.blocks_id(chat.id));
        assert!(ledger.blocks_terminal(terminal));
        let mut successor = chat.clone();
        successor.id = TerminalId::new();
        assert!(!ledger.blocks_chat(&successor));
        assert!(!ledger.blocks_id(successor.id));
        let mut other_epoch = chat;
        other_epoch.proof.as_mut().unwrap().host_incarnation =
            crate::model::HostIncarnation::new("different");
        assert!(!ledger.blocks_chat(&other_epoch));
    }
    #[test]
    fn close_acknowledgement_binds_the_final_conversation_and_preserves_its_old_alias() {
        let mut original = chat();
        original.launch.session_id = None;
        let mut record = Record::new(source(), vec![original.clone()], vec![]);
        record
            .acknowledge_snapshot(
                &original,
                crate::chat::Snapshot {
                    session_id: Some("recorded-during-close".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            record.chats[0].launch.session_id.as_deref(),
            Some("recorded-during-close")
        );
        assert_eq!(record.snapshots[0].source, record.chats[0]);
        assert!(record.aliases.contains(&original));
        let canonical = record.chats[0].clone();
        let ledger = Ledger {
            records: vec![record],
        };
        assert_eq!(ledger.resume_source(&original), Some(&canonical));
        let mut another = original;
        another.id = TerminalId::new();
        let mut record = Record::new(source(), vec![canonical], vec![]);
        assert!(
            record
                .acknowledge_snapshot(&another, Default::default())
                .is_err()
        );
        assert!(record.snapshots.is_empty());
    }

    #[test]
    fn exact_stopped_source_wins_over_an_earlier_epoch_of_the_same_pane() {
        let old = chat();
        let mut current = old.clone();
        current.proof.as_mut().unwrap().host_incarnation =
            crate::model::HostIncarnation::new("newer");
        let ledger = Ledger {
            records: vec![
                Record::new(source(), vec![old], vec![]),
                Record::new(
                    current.proof.clone().unwrap(),
                    vec![current.clone()],
                    vec![],
                ),
            ],
        };
        assert_eq!(ledger.resume_source(&current), Some(&current));
    }

    #[test]
    fn an_aborted_floor_only_marks_acknowledged_closes_in_the_same_source() {
        let ended = chat();
        let still_running = chat();
        let ledger = Ledger {
            records: vec![Record::new(source(), vec![ended.clone()], vec![])],
        };
        assert!(ledger.blocks_chat(&ended));
        assert!(ledger.resume_source(&ended).is_some());
        assert!(!ledger.blocks_chat(&still_running));
        assert!(!ledger.blocks_id(still_running.id));
        assert!(!ledger.blocks_terminal(still_running.id));
        assert!(ledger.resume_source(&still_running).is_none());
        let cancelled = Ledger::default();
        assert!(!cancelled.blocks_chat(&still_running));
        assert!(cancelled.resume_source(&still_running).is_none());
    }

    #[test]
    fn pruning_removes_resumed_allocations_and_keeps_stopped_panes() {
        let kept = chat();
        let resumed = chat();
        let terminal = TerminalId::new();
        let mut record = Record::new(
            source(),
            vec![kept.clone(), resumed.clone()],
            vec![terminal],
        );
        record.service_stopped = true;
        record.aliases.push(resumed.clone());
        record.snapshots.push(StoppedSnapshot {
            source: resumed.clone(),
            snapshot: Default::default(),
            limitation: None,
        });
        record.prune_allocations(&[kept.id].into_iter().collect());
        let ledger = Ledger {
            records: vec![record],
        };
        assert!(ledger.blocks_chat(&kept));
        assert!(ledger.resume_source(&kept).is_some());
        assert!(!ledger.blocks_id(resumed.id));
        assert!(ledger.resume_source(&resumed).is_none());
        assert!(!ledger.blocks_terminal(terminal));
        assert!(ledger.records[0].aliases.is_empty());
        assert!(ledger.records[0].snapshots.is_empty());
    }

    #[test]
    fn startup_indexes_large_history_without_loading_it_and_prunes_stale_sources() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let root = crate::fs_safety::Root::open(directory.path()).unwrap();
        let mut raw_size = 0;
        for index in 0..3 {
            let mut chat = chat();
            chat.proof.as_mut().unwrap().host_incarnation =
                crate::model::HostIncarnation::new(format!("stale-{index}"));
            let mut record = Record::new(chat.proof.clone().unwrap(), vec![chat.clone()], vec![]);
            record.snapshots.push(StoppedSnapshot {
                source: chat,
                snapshot: crate::chat::Snapshot {
                    items: vec![std::sync::Arc::new(crate::chat::Item {
                        id: "large".into(),
                        role: crate::chat::Role::Assistant,
                        content: crate::chat::ItemContent::Text {
                            text: "x".repeat(512 * 1024),
                            thinking: false,
                            phase: Default::default(),
                        },
                        started_at: None,
                        at: None,
                    })],
                    ..Default::default()
                },
                limitation: None,
            });
            raw_size += record.encode_capture().unwrap().len();
            record.write_to(&root).unwrap();
        }
        // A deliberately small former aggregate budget is exceeded, but the
        // startup index never materializes any of these transcript payloads.
        assert!(raw_size > 1024 * 1024);
        let ledger = load_at(directory.path()).unwrap();
        assert_eq!(ledger.records.len(), 3);
        assert!(
            ledger
                .records
                .iter()
                .all(|record| record.snapshots.is_empty())
        );
        assert!(serde_json::to_vec(&ledger.records).unwrap().len() < 64 * 1024);
        prune_at(directory.path(), &Default::default(), |_| true).unwrap();
        assert_eq!(
            load_at(directory.path()).unwrap().records.len(),
            3,
            "an installed source keeps its evidence until its windows flush"
        );
        prune_at(directory.path(), &Default::default(), |_| false).unwrap();
        assert!(load_at(directory.path()).unwrap().records.is_empty());
    }

    #[test]
    fn more_than_1024_source_records_does_not_prevent_startup() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        for index in 0..1025 {
            let mut record = Record::new(source(), vec![], vec![]);
            record.source.host_incarnation =
                crate::model::HostIncarnation::new(format!("old-{index}"));
            let path = directory.path().join(record.file_name().unwrap());
            fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(load_at(directory.path()).unwrap().records.len(), 1025);
    }

    #[test]
    fn missing_history_is_empty_and_unsafe_history_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            load_at(&directory.path().join("absent"))
                .unwrap()
                .records
                .is_empty()
        );
        std::os::unix::fs::symlink(directory.path(), directory.path().join("link")).unwrap();
        assert!(load_at(&directory.path().join("link")).is_err());
    }
}

pub(crate) fn same_allocation(source: &Session, saved: &Session) -> bool {
    source.id == saved.id
        && source.launch.provider == saved.launch.provider
        && source.launch.recipe == saved.launch.recipe
        && saved
            .proof
            .as_ref()
            .is_none_or(|proof| Some(proof) == source.proof.as_ref())
        && saved
            .launch
            .session_id
            .as_ref()
            .is_none_or(|id| Some(id) == source.launch.session_id.as_ref())
}

impl Record {
    /// ChatClose's final publication may name a conversation which did not
    /// exist in ChatList yet. Bind that acknowledgement to the exact captured
    /// allocation and retain the old descriptor as its stopped-reader alias.
    pub(crate) fn acknowledge_snapshot(
        &mut self,
        source: &Session,
        snapshot: crate::chat::Snapshot,
    ) -> Result<(), String> {
        let current = self
            .chats
            .iter_mut()
            .find(|chat| &**chat == source)
            .ok_or("The acknowledged close has no exact captured source")?;
        if source.proof.as_ref() != Some(&self.source) {
            return Err("The acknowledged close changed its source ownership".into());
        }
        let mut confirmed = source.clone();
        if let Some(id) = snapshot
            .session_id
            .as_ref()
            .filter(|id| !id.is_empty() && id.len() <= 1024)
        {
            confirmed.launch.session_id = Some(id.clone());
        }
        *current = confirmed.clone();
        if !self.aliases.contains(source) {
            self.aliases.push(source.clone());
        }
        let saved = StoppedSnapshot {
            source: confirmed,
            snapshot,
            limitation: None,
        };
        if let Some(previous) = self
            .snapshots
            .iter_mut()
            .find(|saved| saved.source.id == source.id)
        {
            *previous = saved;
        } else {
            self.snapshots.push(saved);
        }
        Ok(())
    }

    /// Presentations were captured before ChatClose's final conversation ID.
    /// The source proof, allocation and launch still identify that same pane.
    #[cfg(feature = "host")]
    pub(crate) fn contains_capture_source(&self, captured: &Session) -> bool {
        self.chats.iter().any(|source| {
            let mut captured = captured.clone();
            captured.launch.session_id = source.launch.session_id.clone();
            same_allocation(source, &captured)
        })
    }
}

/// Transcript previews may shrink to fit; the exact source and managed-pane
/// evidence never does. Provider transcript files are outside this operation.
fn encode_record(record: &mut Record, limit: u64) -> Result<Vec<u8>, String> {
    loop {
        let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
        if bytes.len() as u64 <= limit {
            return Ok(bytes);
        }
        let Some(index) = record
            .snapshots
            .iter()
            .enumerate()
            .filter(|(_, saved)| !saved.snapshot.items.is_empty())
            .max_by_key(|(_, saved)| {
                serde_json::to_vec(&saved.snapshot.items).map_or(0, |bytes| bytes.len())
            })
            .map(|(index, _)| index)
        else {
            return Err("Stopped update pane identities exceed their durable limit".into());
        };
        let saved = &mut record.snapshots[index];
        // Keep the newest contiguous tail. Halving bounds reserialization work
        // for oversized captures; the provider retains the full conversation.
        let remove = saved.snapshot.items.len().div_ceil(2);
        saved.snapshot.items.drain(..remove);
        saved.snapshot.has_earlier = true;
        saved.limitation = Some(if saved.source.launch.session_id.is_some() {
            "The saved page was shortened to fit the update record. Resume loads the full recorded conversation; provider transcript files were kept."
        } else {
            "The saved page was shortened to fit the update record. No conversation was recorded; only this captured transcript is available. Start a new conversation to continue."
        }.into());
    }
}

#[cfg(test)]
mod compact_tests {
    use super::*;
    #[test]
    fn large_previews_keep_the_newest_tail_without_shrinking_identity_evidence() {
        let source = Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: crate::model::LaunchRecipe {
                    program: "claude".into(),
                    arguments: vec![],
                    working_directory: "/work".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some(uuid::Uuid::new_v4().to_string()),
            },
            proof: Some(HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("host"),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: crate::model::HostIncarnation::new("old"),
            }),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let mut record = Record::new(source.proof.clone().unwrap(), vec![source.clone()], vec![]);
        record.snapshots.push(StoppedSnapshot {
            source: source.clone(),
            snapshot: crate::chat::Snapshot {
                items: (0..8)
                    .map(|index| {
                        std::sync::Arc::new(crate::chat::Item {
                            id: index.to_string(),
                            role: crate::chat::Role::Assistant,
                            content: crate::chat::ItemContent::Text {
                                text: "x".repeat(1000),
                                thinking: false,
                                phase: Default::default(),
                            },
                            started_at: None,
                            at: None,
                        })
                    })
                    .collect(),
                ..Default::default()
            },
            limitation: None,
        });
        let mut empty = record.clone();
        empty.snapshots[0].snapshot.items.clear();
        let minimum = serde_json::to_vec(&empty).unwrap().len() as u64;
        let bytes = encode_record(&mut record, minimum + 2600).unwrap();
        assert!(bytes.len() as u64 <= minimum + 2600);
        assert_eq!(record.chats, vec![source]);
        let saved = &record.snapshots[0];
        assert!(saved.snapshot.has_earlier);
        assert!(saved.limitation.is_some());
        assert_eq!(saved.snapshot.items.last().unwrap().id, "7");
        assert!(saved.snapshot.items.len() < 8);
        assert!(
            encode_record(&mut record, 1).is_err(),
            "source evidence must never be discarded to fit"
        );
    }
}
