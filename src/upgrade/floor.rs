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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StoppedSnapshot {
    pub source: Session,
    pub snapshot: crate::chat::Snapshot,
    #[serde(default)]
    pub limitation: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
        for previous in load()?.records {
            if previous.source != record.source {
                continue;
            }
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
        let bytes = encode_record(&mut record, LIMIT)?;
        use sha2::{Digest, Sha256};
        let name = format!(
            "{:x}.json",
            Sha256::digest(serde_json::to_vec(&record.source).map_err(|e| e.to_string())?)
        );
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
        self.source(session.id)
            .filter(|source| {
                *source == session
                    || self
                        .records
                        .iter()
                        .any(|record| record.aliases.contains(session))
                    || self.history_replacement(source, session)
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
        let Some(original) = self.source(source.id) else {
            return false;
        };
        if !(source == original
            || self
                .records
                .iter()
                .any(|record| record.aliases.contains(source)))
            || source.proof == replacement.proof
            || !replacement
                .proof
                .as_ref()
                .is_some_and(HostSessionProof::is_complete)
            || original.launch.session_id.is_none()
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

fn directory() -> Result<PathBuf, String> {
    crate::instance::project_dirs()
        .map(|dirs| dirs.data_local_dir().join("daemon-upgrade/update-floor"))
        .ok_or_else(|| "Cannot discover stopped update panes".into())
}

pub(crate) fn load() -> Result<Ledger, String> {
    load_at(&directory()?)
}

fn load_at(directory: &Path) -> Result<Ledger, String> {
    crate::secure_file::reject_symlink_components(directory).map_err(|e| e.to_string())?;
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Ledger::default()),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.is_dir()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err("Unsafe stopped update pane directory".into());
    }
    let root = crate::fs_safety::Root::open(directory).map_err(|e| e.to_string())?;
    let mut ledger = Ledger::default();
    let mut total = 0u64;
    for (index, entry) in fs::read_dir(directory)
        .map_err(|e| e.to_string())?
        .enumerate()
    {
        if index >= 1024 {
            return Err("Stopped update history exceeds its record limit".into());
        }
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let mut file = crate::fs_safety::open_beneath(
            &root,
            Path::new(path.file_name().ok_or("Invalid stopped update record")?),
        )
        .map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
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
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > LIMIT {
            return Err("Stopped update pane record exceeds its limit".into());
        }
        total = total.saturating_add(bytes.len() as u64);
        if total > 64 * 1024 * 1024 {
            return Err("Stopped update history exceeds its aggregate limit".into());
        }
        let record: Record = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if !record.source.is_complete()
            || record
                .chats
                .iter()
                .any(|chat| chat.proof.as_ref() != Some(&record.source))
        {
            return Err("Stopped update pane record has invalid ownership".into());
        }
        ledger.records.push(record);
    }
    Ok(ledger)
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

/// Each daemon/window reads its stopped inventory once. The updater writes it
/// before starting those processes; old processes keep their existing behavior.
pub(crate) fn cached() -> Result<&'static Ledger, &'static str> {
    static LEDGER: std::sync::OnceLock<Result<Ledger, String>> = std::sync::OnceLock::new();
    LEDGER.get_or_init(load).as_ref().map_err(String::as_str)
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

/// ChatClose's final publication is newer than the reviewed preview. Retain its
/// output and any conversation identity the source confirmed while closing.
pub(crate) fn retain_final_snapshot(
    source: &Session,
    snapshot: crate::chat::Snapshot,
) -> Result<(), String> {
    let mut ledger = load()?;
    let record = ledger
        .records
        .iter_mut()
        .find(|record| {
            source.proof.as_ref() == Some(&record.source)
                && record.chats.iter().any(|chat| chat.id == source.id)
        })
        .ok_or("The stopped conversation has no retained update record")?;
    let mut confirmed = source.clone();
    if let Some(id) = snapshot
        .session_id
        .as_ref()
        .filter(|id| !id.is_empty() && id.len() <= 1024)
    {
        confirmed.launch.session_id = Some(id.clone());
    }
    if !record.aliases.contains(source) {
        record.aliases.push(source.clone());
    }
    if let Some(chat) = record.chats.iter_mut().find(|chat| chat.id == source.id) {
        *chat = confirmed.clone();
    }
    let saved = StoppedSnapshot {
        source: confirmed,
        snapshot,
        limitation: None,
    };
    if let Some(previous) = record
        .snapshots
        .iter_mut()
        .find(|saved| saved.source.id == source.id)
    {
        *previous = saved;
    } else {
        record.snapshots.push(saved);
    }
    record.save()
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
        saved.limitation=Some("The saved page was shortened to fit the update record. Resume loads the full recorded conversation; provider transcript files were kept.".into());
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
