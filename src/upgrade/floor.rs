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

/// The durable limit of one source's stopped-pane record.
pub(crate) const LIMIT: u64 = 8 * 1024 * 1024;

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

#[derive(Clone, Default)]
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
            record.absorb(previous);
        }
        if !record.source.is_complete()
            || record
                .chats
                .iter()
                .any(|chat| chat.proof.as_ref() != Some(&record.source))
        {
            return Err("Stopped update panes have no exact source proof".into());
        }
        record.write_to(&root)?;
        load().map(|_| ())
    }

    /// Join evidence retained earlier for the same source, as saving does.
    /// This record's own entries win over earlier ones for the same owner.
    fn absorb(&mut self, previous: Record) {
        self.service_stopped |= previous.service_stopped;
        for alias in previous.aliases {
            if !self.aliases.contains(&alias) {
                self.aliases.push(alias);
            }
        }
        for snapshot in previous.snapshots {
            if !self
                .snapshots
                .iter()
                .any(|current| current.source.id == snapshot.source.id)
            {
                self.snapshots.push(snapshot);
            }
        }
        for chat in previous.chats {
            if !self.chats.iter().any(|current| current.id == chat.id) {
                self.chats.push(chat);
            }
        }
        for id in previous.terminals {
            if !self.terminals.contains(&id) {
                self.terminals.push(id);
            }
        }
        for pane in previous.presentations {
            if !self
                .presentations
                .iter()
                .any(|current| current.agent == pane.agent)
            {
                self.presentations.push(pane);
            }
        }
    }

    /// The bytes this record needs once saved beside the evidence already
    /// retained for its source, with every transcript page empty. Saving
    /// shortens pages to fit around that evidence and never shortens it.
    pub(crate) fn retained_identity_size(&self) -> Result<usize, String> {
        let mut record = self.clone();
        if let Some(previous) = record_for_source(&record.source)? {
            record.absorb(previous);
        }
        record.identity_size()
    }

    /// The encoded size with every page emptied, plus one byte per page: an
    /// unshortened page may say `false` where the emptied one says `true`.
    fn identity_size(&self) -> Result<usize, String> {
        let mut minimal = self.clone();
        minimal.snapshots.iter_mut().for_each(empty_page);
        serde_json::to_vec(&minimal)
            .map(|bytes| bytes.len() + minimal.snapshots.len())
            .map_err(|error| error.to_string())
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
                self.records.iter().find_map(|record| {
                    record
                        .chats
                        .iter()
                        .find(|source| record.earlier_form(source, session))
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
                    && (source == original
                        || record.aliases.contains(source)
                        || record.earlier_form(original, source))
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
            (source.proof.as_ref() == Some(&record.source) || record.aliases.contains(source))
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

    /// A discovery retry can waive only its own source's earlier pause. A
    /// different update's explicit stop for the same allocation still wins.
    #[cfg(feature = "host")]
    pub(crate) fn blocks_id_except_source(
        &self,
        id: TerminalId,
        source: &HostSessionProof,
    ) -> bool {
        self.records.iter().any(|record| {
            &record.source != source
                && (record.terminals.contains(&id)
                    || record.chats.iter().any(|chat| {
                        chat.id == id
                            || chat
                                .terminal
                                .as_ref()
                                .is_some_and(|terminal| terminal.id == id)
                    }))
        })
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
    // Serialize reads and publication without holding the display lock over
    // filesystem work. An earlier read cannot overwrite a later local save.
    static LOADER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _loader = LOADER.lock().unwrap_or_else(|poison| poison.into_inner());
    #[cfg(test)]
    INDEX_READS.with(|reads| reads.set(reads.get() + 1));
    let ledger = load_at(&directory()?)?;
    *display_index()
        .write()
        .unwrap_or_else(|poison| poison.into_inner()) = std::sync::Arc::new(ledger.clone());
    Ok(ledger)
}

fn display_index() -> &'static std::sync::RwLock<std::sync::Arc<Ledger>> {
    static INDEX: std::sync::OnceLock<std::sync::RwLock<std::sync::Arc<Ledger>>> =
        std::sync::OnceLock::new();
    INDEX.get_or_init(|| std::sync::RwLock::new(std::sync::Arc::new(Ledger::default())))
}

/// Presentation reads the last published index without filesystem work.
/// Recovery and lineage decisions call load() for current durable evidence.
pub(crate) fn display_snapshot() -> std::sync::Arc<Ledger> {
    display_index()
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
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

pub(crate) fn record_for_source(source: &HostSessionProof) -> Result<Option<Record>, String> {
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
    fn an_exact_issued_display_alias_can_resolve_only_its_stopped_chat() {
        let original = chat();
        let mut issued = original.clone();
        issued.proof.as_mut().unwrap().host_incarnation =
            crate::model::HostIncarnation::new("issued");
        let mut record = Record::new(source(), vec![original.clone()], Vec::new());
        record.aliases.push(issued.clone());
        let ledger = Ledger {
            records: vec![record],
        };
        assert_eq!(ledger.resume_source(&issued), Some(&original));
        assert!(ledger.blocks_chat(&issued));
        let mut unissued = issued.clone();
        unissued.launch.recipe.arguments.push("unissued".into());
        assert!(!ledger.blocks_chat(&unissued));
        assert!(ledger.resume_source(&unissued).is_none());
    }

    #[cfg(feature = "host")]
    #[test]
    fn discovery_retries_never_bypass_another_sources_terminal_chat_or_pair_stop() {
        let original = chat();
        let mut later = source();
        later.host_incarnation = crate::model::HostIncarnation::new("later-stop");
        let paired = TerminalId::new();
        let mut later_chat = chat();
        later_chat.id = TerminalId::new();
        later_chat.proof = Some(later.clone());
        later_chat.terminal = Some(crate::model::TerminalSession {
            id: paired,
            launch: later_chat.launch.recipe.clone(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::DetachedSameBoot {
                host_session_id: later.host_session_id.clone(),
                boot_id: later.boot_id.clone(),
                host_incarnation: later.host_incarnation.clone(),
            },
        });
        let mut ledger = Ledger {
            records: vec![Record::new(source(), vec![original.clone()], vec![paired])],
        };
        assert!(!ledger.blocks_id_except_source(original.id, &source()));
        assert!(!ledger.blocks_id_except_source(paired, &source()));
        ledger.records.push(Record::new(
            later,
            vec![later_chat.clone()],
            vec![original.id],
        ));
        assert!(ledger.blocks_id_except_source(original.id, &source()));
        assert!(ledger.blocks_id_except_source(later_chat.id, &source()));
        assert!(ledger.blocks_id_except_source(paired, &source()));
        assert!(!ledger.blocks_id_except_source(TerminalId::new(), &source()));
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
    fn a_descriptor_saved_before_the_conversation_id_resolves_to_its_stopped_chat() {
        let original = chat();
        let mut earlier = original.clone();
        earlier.launch.session_id = None;
        let mut replacement = original.clone();
        replacement.proof.as_mut().unwrap().host_incarnation =
            crate::model::HostIncarnation::new("successor");
        replacement.native_continuity = crate::chat::NativeContinuity::ReconnectOnly;
        let ledger = Ledger {
            records: vec![Record::new(source(), vec![original.clone()], vec![])],
        };
        assert!(ledger.history_replacement(&original, &replacement));
        assert!(ledger.history_replacement(&earlier, &replacement));
        assert_eq!(ledger.resume_source(&earlier), Some(&original));

        let mut other_conversation = earlier.clone();
        other_conversation.launch.session_id = Some("other".into());
        assert!(!ledger.history_replacement(&other_conversation, &replacement));
        assert!(ledger.resume_source(&other_conversation).is_none());
        let mut other_epoch = earlier.clone();
        other_epoch.proof.as_mut().unwrap().host_incarnation =
            crate::model::HostIncarnation::new("different");
        assert!(!ledger.history_replacement(&other_epoch, &replacement));
        assert!(ledger.resume_source(&other_epoch).is_none());
        let mut other_recipe = earlier;
        other_recipe.launch.recipe.arguments.push("other".into());
        assert!(!ledger.history_replacement(&other_recipe, &replacement));
        assert!(ledger.resume_source(&other_recipe).is_none());
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
    /// A descriptor of a stopped pane captured before the source learned its
    /// conversation ID. Panes and agents saved earlier in the source epoch keep
    /// that form, so it must resolve to the same stopped chat as the exact one.
    fn earlier_form(&self, original: &Session, earlier: &Session) -> bool {
        earlier.proof.as_ref() == Some(&self.source)
            && earlier.launch.session_id.is_none()
            && same_allocation(original, earlier)
    }

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
/// Every page keeps its newest entries within a fair share of the room the
/// identity evidence leaves, so many long conversations all stay readable.
fn encode_record(record: &mut Record, limit: u64) -> Result<Vec<u8>, String> {
    for saved in &mut record.snapshots {
        retain_reader_fields(&mut saved.snapshot);
    }
    let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    if bytes.len() as u64 <= limit {
        return Ok(bytes);
    }
    let mut pages = record
        .snapshots
        .iter()
        .map(|saved| Page::of(&saved.snapshot))
        .collect::<Result<Vec<_>, _>>()?;
    let budget = usize::try_from(limit)
        .unwrap_or(usize::MAX)
        .checked_sub(record.identity_size()?)
        .ok_or("Stopped update pane identities exceed their durable limit")?;
    keep_fair_shares(&mut record.snapshots, &mut pages, budget);
    let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("Stopped update pane record exceeds its limit after fitting".into());
    }
    Ok(bytes)
}

/// Appended to a saved page whose older entries did not fit the update's
/// evidence. Only the page is shorter; the provider keeps the conversation.
pub(crate) const OLDER_HISTORY_OMITTED: &str =
    " Earlier messages were left out of this saved page to fit the update's size limit.";

/// A paused page is read through `floor_history`, which shows its entries,
/// title and conversation identity. Interaction state of a provider which no
/// longer exists (commands, skills, models, open requests, queued messages,
/// draft and copy alignment) is never shown there and is not retained.
pub(crate) fn retain_reader_fields(snapshot: &mut crate::chat::Snapshot) {
    use std::mem::take;
    snapshot.notification_preview = Default::default();
    take(&mut snapshot.text_copy_sources);
    take(&mut snapshot.requests);
    take(&mut snapshot.models);
    take(&mut snapshot.permission_denials);
    take(&mut snapshot.commands);
    take(&mut snapshot.skills);
    take(&mut snapshot.composer);
    take(&mut snapshot.prompt_suggestion);
    take(&mut snapshot.plan);
    take(&mut snapshot.subagents);
    take(&mut snapshot.queued_messages);
    take(&mut snapshot.queue_removals);
    take(&mut snapshot.branch_points);
    take(&mut snapshot.file_changes);
}

/// The encoded size of each entry of one page, oldest first. A JSON array
/// costs one separator per entry beyond its own bytes.
pub(crate) struct Page {
    entries: Vec<usize>,
}

impl Page {
    pub(crate) fn of(snapshot: &crate::chat::Snapshot) -> Result<Self, String> {
        snapshot
            .items
            .iter()
            .map(|item| serde_json::to_vec(&**item).map(|bytes| bytes.len() + 1))
            .collect::<Result<_, _>>()
            .map(|entries| Self { entries })
            .map_err(|error| error.to_string())
    }

    /// Bytes of all of its entries.
    pub(crate) fn bytes(&self) -> usize {
        self.entries.iter().sum()
    }

    /// Keep this page's newest `count` entries, marking it if any were lost.
    fn keep(&mut self, saved: &mut StoppedSnapshot, count: usize) {
        let omitted = self.entries.len() - count;
        if omitted == 0 {
            return;
        }
        self.entries.drain(..omitted);
        saved.snapshot.items.drain(..omitted);
        saved.snapshot.has_earlier = true;
        mark_omitted(saved);
    }
}

/// Shorten each page to its fair share of `budget`; see [`fair_shares`].
/// `pages` measures `snapshots`, one to one, and stays in step with them.
pub(crate) fn keep_fair_shares(
    snapshots: &mut [StoppedSnapshot],
    pages: &mut [Page],
    budget: usize,
) {
    debug_assert_eq!(snapshots.len(), pages.len());
    let shares = fair_shares(pages, budget);
    for ((saved, page), count) in snapshots.iter_mut().zip(pages).zip(shares) {
        page.keep(saved, count);
    }
}

/// The least a page can keep: no entries, saying older ones were omitted.
pub(crate) fn empty_page(saved: &mut StoppedSnapshot) {
    saved.snapshot.items.clear();
    saved.snapshot.has_earlier = true;
    mark_omitted(saved);
}

/// Say that a page lost its older entries, after the reason the reader
/// would otherwise show for it.
pub(crate) fn mark_omitted(saved: &mut StoppedSnapshot) {
    let limitation = saved
        .limitation
        .get_or_insert_with(|| default_limitation(&saved.source).into());
    if !limitation.ends_with(OLDER_HISTORY_OMITTED) {
        limitation.push_str(OLDER_HISTORY_OMITTED);
    }
}

/// Why a stopped page that records no limitation of its own is read-only.
pub(crate) fn default_limitation(source: &Session) -> &'static str {
    if source.launch.session_id.is_some() {
        "Stopped for the update. This saved page is a transcript preview; Resume loads the full conversation. Your draft is kept."
    } else {
        "Stopped for the update before a conversation was recorded. The captured transcript stays visible; start a new conversation to continue. Your draft is kept."
    }
}

/// How many newest entries each page keeps so that their total stays within
/// `budget`, shared max-min fairly in bytes: pages grow one entry at a time,
/// the smallest result first, so short pages keep all of their history and
/// leave the rest to longer ones. A page stops at an entry larger than what
/// remains; the others go on.
fn fair_shares(pages: &[Page], budget: usize) -> Vec<usize> {
    use std::{cmp::Reverse, collections::BinaryHeap};
    // The size a page reaches with its next-newest entry, that entry's own
    // size and the page, smallest result first.
    let next = |page: usize, count: usize, kept: usize| {
        let entries = &pages[page].entries;
        let size = *entries
            .len()
            .checked_sub(count + 1)
            .map(|at| &entries[at])?;
        Some(Reverse((kept + size, size, page)))
    };
    let mut counts = vec![0; pages.len()];
    let mut queue: BinaryHeap<_> = (0..pages.len())
        .filter_map(|page| next(page, 0, 0))
        .collect();
    let mut remaining = budget;
    while let Some(Reverse((kept, size, page))) = queue.pop() {
        // What remains only shrinks, so this entry never fits later either.
        let Some(left) = remaining.checked_sub(size) else {
            continue;
        };
        remaining = left;
        counts[page] += 1;
        queue.extend(next(page, counts[page], kept));
    }
    counts
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

    #[test]
    fn pages_share_the_budget_max_min_fairly_newest_first() {
        let page = |entries: &[usize]| Page {
            entries: entries.to_vec(),
        };
        let pages = [
            // Short: keeps everything and leaves the rest to longer pages.
            page(&[10; 3]),
            // Long: shares what remains evenly with the other long page.
            page(&[10; 100]),
            page(&[10; 100]),
            // Its newest entry fits nowhere; it alone keeps nothing.
            page(&[10, 10, 500]),
            // A large older entry stops this page, not the others.
            page(&[10, 300, 10]),
        ];
        assert_eq!(fair_shares(&pages, 0), [0; 5]);
        assert_eq!(fair_shares(&pages, 420), [3, 19, 19, 0, 1]);
        assert_eq!(fair_shares(&pages, usize::MAX), [3, 100, 100, 3, 3]);
    }
}
