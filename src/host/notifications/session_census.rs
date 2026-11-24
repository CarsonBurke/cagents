//! Periodic, fail-closed census of session notice owners.
use std::{
    fs,
    io::ErrorKind,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    agent_execution::Lineage,
    daemon::holder_registry::{self, Kind},
    host::{
        model::{
            Registry,
            session_attention::{ModelVersion, References},
        },
        state::SharedHost,
    },
    model::TerminalId,
    notifications::Target,
    persistence::{
        MigrationInspectionBudget,
        attention::{SavedSessions, saved_sessions},
    },
    upgrade::{floor, handoff::Store},
};
use uuid::Uuid;

pub(in crate::host) struct Census {
    model: Arc<Mutex<Registry>>,
    version: ModelVersion,
    resident: References,
    windows: Windows,
    sources: EvidenceSources,
    retire: Vec<(Uuid, Target)>,
}

impl Census {
    /// Recheck immediately at the journal writer. The control admission stays
    /// held through the exact deletion, so a newly admitted owner cannot fall
    /// between checking an earlier census and committing its consequences.
    pub(in crate::host) fn apply<T>(
        mut self,
        host: &SharedHost,
        retire: impl FnOnce(Vec<(Uuid, Target)>) -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        let _window_admission = match crate::instance::attention_window_census_fence() {
            Ok(Some(fence)) => fence,
            Ok(None) => return Ok(None),
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        let current = window_paths()?;
        if !current.iter().eq(self.windows.iter().map(|(path, _)| path)) {
            return Ok(None);
        }
        // The leases borrow these connections. Keep them all alive through
        // the exact journal transaction, even on files using WAL.
        let mut windows = std::mem::take(&mut self.windows);
        let mut leases = Vec::with_capacity(windows.len());
        for (_, window) in &mut windows {
            let Some(lease) = window
                .lock_if_unchanged()
                .map_err(|error| error.to_string())?
            else {
                return Ok(None);
            };
            leases.push(lease);
        }
        self.apply_settled(host, retire)
    }

    fn apply_settled<T>(
        self,
        host: &SharedHost,
        retire: impl FnOnce(Vec<(Uuid, Target)>) -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        let Some(gate) = host.try_lock().map(|state| state.control_gate.clone()) else {
            return Ok(None);
        };
        let Some(_admission) = gate.try_exclusive() else {
            return Ok(None);
        };
        let Ok(model) = self.model.try_lock() else {
            return Ok(None);
        };
        let Some(_model_write_lease) = model.lock_attention_version(&self.version)? else {
            return Ok(None);
        };
        if !self.sources.unchanged()? {
            return Ok(None);
        }
        let Some(state) = host.try_lock() else {
            return Ok(None);
        };
        let settled = !state.shutting_down
            && state.startup_recovery().is_none()
            && state.attention_session_inventory() == self.resident;
        drop(state);
        if settled {
            retire(self.retire).map(Some)
        } else {
            Ok(None)
        }
    }
}

/// The runtime collector is started only after successful ordinary recovery.
/// Its first pass skips this operation, so no startup sweep can infer absence.
pub(in crate::host) fn collect(
    host: &SharedHost,
    model: &Arc<Mutex<Registry>>,
) -> Result<(), String> {
    let (notifications, resident, evidence) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down || state.startup_recovery().is_some() {
            return Ok(());
        }
        let Some(notifications) = &state.notifications else {
            return Ok(());
        };
        (
            notifications.clone(),
            state.attention_session_inventory(),
            state.upgrade_evidence.clone(),
        )
    };
    let candidates = notifications
        .snapshot()
        .notices
        .into_iter()
        .filter_map(|notice| {
            let id = match notice.event.target {
                Target::Session(id) | Target::SessionInbox(Lineage(id)) => id,
                _ => return None,
            };
            Some((id, notice.id, notice.event.target))
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Ok(());
    }

    let sources = EvidenceSources::capture(evidence.as_deref())?;
    let (mut references, windows) = saved_references()?;
    references.merge(&resident);
    for kind in [Kind::Terminal, Kind::Native] {
        let holders = holder_registry::observed(kind).map_err(|error| error.to_string())?;
        references.extend(holders.into_iter().map(|holder| holder.terminal));
    }
    if let Some(store) = evidence.as_deref() {
        add_recovery(&mut references, store)?;
    } else if let Some(store) = Store::existing()? {
        add_recovery(&mut references, &store)?;
    }
    if !sources.unchanged()? {
        return Ok(());
    }
    let registry = model.lock().unwrap_or_else(|p| p.into_inner());
    let (version, missing) =
        registry.attention_orphans(references, candidates.iter().map(|(id, _, _)| *id))?;
    if missing.is_empty() {
        return Ok(());
    }
    let retire = candidates
        .into_iter()
        .filter(|(id, _, _)| missing.contains(id))
        .map(|(_, notice, target)| (notice, target))
        .collect();
    drop(registry);
    notifications.retire_session_notices(Census {
        model: model.clone(),
        version,
        resident,
        windows,
        sources,
        retire,
    });
    Ok(())
}

fn window_paths() -> Result<Vec<PathBuf>, String> {
    let mut present = Vec::new();
    for path in crate::instance::window_database_paths().map_err(|error| error.to_string())? {
        match fs::symlink_metadata(&path) {
            Ok(_) => present.push(path),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(present)
}

type Windows = Vec<(PathBuf, SavedSessions)>;

/// These publishers add and replace evidence by changing directory membership.
/// Reject a census if any such directory changed while it was being built or
/// queued. In-place transcript appends do not change the owning identity.
#[derive(Default)]
struct EvidenceSources(Vec<(PathBuf, Option<DirectoryStamp>)>);

#[derive(PartialEq, Eq)]
struct DirectoryStamp {
    device: u64,
    inode: u64,
    changed: (i64, i64),
}

impl DirectoryStamp {
    fn read(path: &Path) -> Result<Option<Self>, String> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => Ok(Some(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
            })),
            Ok(_) => Err(format!(
                "Attention evidence directory is invalid: {}",
                path.display()
            )),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}

impl EvidenceSources {
    fn capture(evidence: Option<&Store>) -> Result<Self, String> {
        let dirs =
            crate::instance::project_dirs().ok_or("Cannot locate retained session evidence")?;
        let upgrade = dirs.data_local_dir().join("daemon-upgrade");
        let mut paths = vec![
            dirs.data_dir().join("transcripts"),
            upgrade.join("update-floor"),
            upgrade,
            holder_registry::directory(Kind::Terminal)?,
            holder_registry::directory(Kind::Native)?,
        ];
        if let Some(path) = evidence.and_then(|store| store.path.parent())
            && !paths.iter().any(|found| found == path)
        {
            paths.push(path.to_owned());
        }
        paths
            .into_iter()
            .map(|path| {
                let stamp = DirectoryStamp::read(&path)?;
                Ok((path, stamp))
            })
            .collect::<Result<Vec<_>, String>>()
            .map(Self)
    }

    fn unchanged(&self) -> Result<bool, String> {
        for (path, stamp) in &self.0 {
            if &DirectoryStamp::read(path)? != stamp {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn saved_references() -> Result<(References, Windows), String> {
    let mut references = References::default();
    let mut windows = Vec::new();
    let mut budget = MigrationInspectionBudget::new(Instant::now() + Duration::from_secs(30));
    for path in window_paths()? {
        let mut window = saved_sessions(&path, &mut budget).map_err(|error| error.to_string())?;
        references.extend(std::mem::take(&mut window.sessions));
        windows.push((path, window));
    }
    let directory = crate::instance::project_dirs()
        .ok_or("Cannot locate retained terminal transcripts")?
        .data_dir()
        .join("transcripts");
    add_transcripts(&mut references, &directory)?;
    for record in floor::load()?.records {
        references.extend(record.terminals);
        for chat in record.chats.iter().chain(&record.aliases) {
            references.chat(chat);
        }
        for presentation in record.presentations {
            references.chat(&presentation.session);
            references.pane(&presentation.pane.kind);
        }
    }
    Ok((references, windows))
}

fn add_transcripts(references: &mut References, directory: &Path) -> Result<(), String> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err("The transcript directory is not a directory".into()),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    }
    for (count, entry) in fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .enumerate()
    {
        if count >= 65_536 {
            return Err(
                "The terminal transcript directory exceeds the attention census limit".into(),
            );
        }
        let entry = entry.map_err(|error| error.to_string())?;
        if let Some(id) = crate::persistence::transcript_name(Path::new(&entry.file_name()))
            .and_then(|name| name.parse::<TerminalId>().ok())
        {
            references.extend([id]);
        }
    }
    Ok(())
}

fn add_recovery(references: &mut References, store: &Store) -> Result<(), String> {
    for checkpoint in [store.load()?, store.load_abandoned()?].iter().flatten() {
        references.checkpoint(checkpoint);
    }
    for entry in store.unattached_descriptors()? {
        references.extend([entry.summary.id]);
        references.recovery_chat(&entry.evidence);
        if let Some(terminal) = entry.terminal {
            references.recovery_terminal(&terminal);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        chat::{Launch, Session},
        host::state::{HostLock, HostState},
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe},
        upgrade::handoff::{Chat, ChatSource, UnattachedChat},
    };

    fn proof() -> HostSessionProof {
        HostSessionProof {
            boot_id: BootId::new("boot"),
            host_session_id: HostSessionId::new("host"),
            host_incarnation: HostIncarnation::new("incarnation"),
        }
    }

    #[test]
    fn unattached_replacement_and_standalone_transcript_protect_their_notices() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("recovery")).unwrap();
        let model = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        let transcript = TerminalId::new();
        fs::write(
            directory.path().join(format!("{transcript}.log")),
            b"output",
        )
        .unwrap();
        let old = TerminalId::new();
        let new = TerminalId::new();
        let launch = Launch {
            provider: Provider::Claude,
            recipe: LaunchRecipe::shell(directory.path().into()).unwrap(),
            session_id: Some("saved-thread".into()),
            draft_model: None,
        };
        let chat = |id| Session {
            id,
            launch: launch.clone(),
            proof: Some(proof()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let exact = Chat {
            source: chat(new),
            previous_sources: vec![ChatSource {
                source: chat(old),
                launch: launch.clone(),
            }],
            launch,
        };
        store
            .record_unattached(&[UnattachedChat::new(exact, "retained".into())], &[])
            .unwrap();
        let mut references = References::default();
        add_recovery(&mut references, &store).unwrap();
        add_transcripts(&mut references, directory.path()).unwrap();
        let orphan = TerminalId::new();
        let (_, missing) = model
            .attention_orphans(references, [old, new, transcript, orphan])
            .unwrap();
        assert_eq!(missing, [orphan].into_iter().collect());
    }

    #[test]
    fn a_new_saved_pin_or_unsettled_control_gate_voids_a_queued_census() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.sqlite3");
        let model = Arc::new(Mutex::new(Registry::at(&path).unwrap()));
        let host = Arc::new(HostLock::new(HostState::new(proof())));
        let terminal = TerminalId::new();
        let census = || Census {
            model: model.clone(),
            version: model.lock().unwrap().attention_data_version().unwrap(),
            resident: References::default(),
            windows: Vec::new(),
            sources: EvidenceSources::default(),
            retire: vec![(Uuid::new_v4(), Target::Session(terminal))],
        };
        let stale = census();
        let mut writer = Registry::at(&path).unwrap();
        writer.pin_window_terminals(&[terminal]).unwrap();
        assert_eq!(
            stale
                .apply_settled(&host, |_| -> Result<(), String> {
                    panic!("stale census reached deletion")
                })
                .unwrap(),
            None
        );

        let current = census();
        let gate = host.lock().unwrap().control_gate.clone();
        let admission = gate.shared();
        assert_eq!(
            current
                .apply_settled(&host, |_| -> Result<(), String> {
                    panic!("unsettled control reached deletion")
                })
                .unwrap(),
            None
        );
        drop(admission);

        let current = census();
        let competing = rusqlite::Connection::open(&path).unwrap();
        competing.busy_timeout(Duration::ZERO).unwrap();
        let result = current
            .apply_settled(&host, |notices| {
                assert!(gate.try_shared().is_none());
                assert!(competing.execute_batch("BEGIN IMMEDIATE").is_err());
                Ok(notices
                    .into_iter()
                    .map(|(_, target)| target)
                    .collect::<Vec<_>>())
            })
            .unwrap();
        assert_eq!(result, Some(vec![Target::Session(terminal)]));
        competing
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
            .unwrap();
    }

    #[test]
    fn newly_published_external_evidence_invalidates_an_earlier_census() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        let stamp = DirectoryStamp::read(&path).unwrap();
        let evidence = EvidenceSources(vec![(path, stamp)]);
        assert!(evidence.unchanged().unwrap());
        fs::write(directory.path().join("restorable.json"), b"new descriptor").unwrap();
        assert!(!evidence.unchanged().unwrap());
    }
}
