//! A Ready daemon publishes which exact stopped source panes it retained.
//! The floor ledger alone is historical evidence, not a serving-owner proof.
use super::super::*;
use crate::terminal_ipc::TerminalDescriptor;

const FILE: &str = "daemon-upgrade.ready-outcome.json";

#[derive(Serialize, Deserialize)]
struct Publication {
    version: u16,
    serving: HostSessionProof,
    target: PathBuf,
    target_hash: String,
    partitions: Vec<Partition>,
}

#[derive(Serialize, Deserialize)]
struct Partition {
    source: HostSessionProof,
    checkpoint_hash: String,
    capture_hash: String,
    chats: Vec<Session>,
    aliases: Vec<Session>,
    terminals: Vec<TerminalSession>,
}

pub(crate) struct ReadyView(Publication);

/// Captured before normal restoration consumes the exact Ready checkpoint.
#[cfg(feature = "host")]
pub(crate) struct ReadyCapture {
    checkpoint_hash: String,
    capture_hash: String,
    capture: CapturedCensus,
}

#[cfg(feature = "host")]
pub(crate) fn capture() -> Result<Option<ReadyCapture>, String> {
    let Some(store) = super::super::super::handoff::Store::existing()? else {
        return Ok(None);
    };
    let Some(status) = store.checkpoint_status()? else {
        return Ok(None);
    };
    if status.phase != super::super::super::handoff::Phase::Ready {
        return Ok(None);
    }
    let bytes = read_checkpoint(&store.path)?
        .ok_or("The Ready checkpoint disappeared before restoration")?;
    super::super::super::handoff::validate_checkpoint_bytes(&bytes)?;
    if digest(&bytes) != status.digest {
        return Err("The Ready checkpoint changed before restoration".into());
    }
    let Some(bytes) = read_checkpoint(&bound_capture_path(&store.path, &status.digest))? else {
        return Ok(None);
    };
    let capture: CapturedCensus = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if capture.source != status.proof
        || capture.checkpoint_hash.as_deref() != Some(&status.digest)
        || !acknowledged(&capture)
    {
        return Err("The Ready checkpoint has no exact acknowledged original census".into());
    }
    Ok(Some(ReadyCapture {
        checkpoint_hash: status.digest,
        capture_hash: digest(&bytes),
        capture,
    }))
}

fn acknowledged(capture: &CapturedCensus) -> bool {
    matches!(
        capture.producer.as_ref(),
        Some(CaptureProducer::Acknowledged { .. })
    ) && capture
        .producer
        .as_ref()
        .is_some_and(CaptureProducer::valid)
}

fn terminal_is_listed(source: &TerminalSession, live: &[TerminalDescriptor]) -> bool {
    live.iter().any(|current| {
        current.session.id == source.id
            || current
                .upgrade_sources
                .iter()
                .any(|issued| same_terminal(source, issued))
            || current
                .upgrade_relaunches
                .iter()
                .any(|issued| same_terminal(source, &issued.source))
    })
}

fn captured_terminals(census: &Census) -> impl Iterator<Item = &TerminalSession> {
    census.terminals.iter().chain(
        census
            .chats
            .iter()
            .filter_map(|chat| chat.terminal.as_ref()),
    )
}

fn stopped_chat(record: &super::super::super::floor::Record, source: &Session) -> bool {
    record.chats.contains(source) || record.aliases.contains(source)
}

impl ReadyView {
    pub(crate) fn load(
        serving: &HostSessionProof,
        target: &Path,
        terminals: &[TerminalDescriptor],
        chats: &[Session],
    ) -> Result<Option<Self>, String> {
        let Some(store) = super::super::super::handoff::Store::existing()? else {
            return Ok(None);
        };
        Self::load_at(
            &store.path,
            serving,
            target,
            terminals,
            chats,
            &super::super::super::floor::load()?,
        )
    }

    fn load_at(
        checkpoint: &Path,
        serving: &HostSessionProof,
        target: &Path,
        terminals: &[TerminalDescriptor],
        chats: &[Session],
        ledger: &super::super::super::floor::Ledger,
    ) -> Result<Option<Self>, String> {
        let Some(bytes) = read_checkpoint(&checkpoint.with_file_name(FILE))? else {
            return Ok(None);
        };
        let publication: Publication = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if publication.version != 1
            || !serving.is_complete()
            || publication.serving != *serving
            || publication.target != target
            || publication.target_hash != executable_hash(target)?
        {
            return Err(
                "Stopped-pane publication does not belong to the exact serving daemon".into(),
            );
        }
        let mut sources = BTreeSet::new();
        for partition in &publication.partitions {
            if !partition.source.is_complete()
                || !sources.insert(digest(
                    &serde_json::to_vec(&partition.source).map_err(|e| e.to_string())?,
                ))
            {
                return Err(
                    "Stopped-pane publication has duplicate or incomplete source ownership".into(),
                );
            }
            let bytes =
                read_checkpoint(&bound_capture_path(checkpoint, &partition.checkpoint_hash))?
                    .ok_or("The exact original stopped-pane census is unavailable")?;
            let capture: CapturedCensus =
                serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            let stopped = ledger
                .records
                .iter()
                .find(|record| record.source == partition.source)
                .ok_or("The original stopped panes are no longer retained")?;
            let chat_ids: BTreeSet<_> = partition.chats.iter().map(|chat| chat.id).collect();
            let terminal_ids: BTreeSet<_> = partition
                .terminals
                .iter()
                .map(|terminal| terminal.id)
                .collect();
            if digest(&bytes) != partition.capture_hash
                || capture.source != partition.source
                || capture.checkpoint_hash.as_deref() != Some(&partition.checkpoint_hash)
                || !acknowledged(&capture)
                || chat_ids.len() != partition.chats.len()
                || terminal_ids.len() != partition.terminals.len()
                || partition.chats.iter().any(|source| {
                    source.proof.as_ref() != Some(&partition.source)
                        || !capture.census.chats.contains(source)
                        || capture
                            .census
                            .snapshots
                            .iter()
                            .filter(|saved| saved.source == *source)
                            .count()
                            != 1
                        || !stopped_chat(stopped, source)
                        || chats.iter().any(|current| current.id == source.id)
                })
                || partition.aliases.iter().any(|alias| {
                    alias.proof.as_ref() != Some(&partition.source)
                        || !chat_ids.contains(&alias.id)
                        || !capture.census.aliases.contains(alias)
                })
                || partition.terminals.iter().any(|source| {
                    source.continuity.live_proof().as_ref() != Some(&partition.source)
                        || !captured_terminals(&capture.census)
                            .any(|known| same_terminal(known, source))
                        || !stopped.terminals.contains(&source.id)
                        || terminal_is_listed(source, terminals)
                })
            {
                return Err("Stopped-pane publication differs from its exact captured and retained originals".into());
            }
        }
        Ok(Some(Self(publication)))
    }

    pub(crate) fn paused_chat(&self, original: &Session) -> bool {
        self.0.partitions.iter().any(|partition| {
            original.proof.as_ref() == Some(&partition.source)
                && (partition.chats.contains(original) || partition.aliases.contains(original))
        })
    }

    pub(crate) fn paused_terminal(&self, original: &TerminalSession) -> bool {
        self.0.partitions.iter().any(|partition| {
            original.continuity.live_proof().as_ref() == Some(&partition.source)
                && partition
                    .terminals
                    .iter()
                    .any(|source| same_terminal(source, original))
        })
    }
}

/// Runs under the startup daemon's lease, after restoration/presentation and
/// before Ready. It records no guessed replacement or provider launch intent.
#[cfg(feature = "host")]
pub(crate) fn publish(
    ready: Option<ReadyCapture>,
    serving: &HostSessionProof,
    target: &Path,
    terminals: &[TerminalDescriptor],
    chats: &[Session],
) -> Result<(), String> {
    if !serving.is_complete() {
        return Err("Ready publication has no complete daemon owner".into());
    }
    let store = super::super::super::handoff::Store::discover()?;
    let mut partitions = Vec::new();
    let ledger = super::super::super::floor::load()?;
    if let Some(ready) = ready
        && let Some(stopped) = ledger
            .records
            .iter()
            .find(|stopped| stopped.source == ready.capture.source)
    {
        let capture = ready.capture;
        let paused: Vec<_> = capture
            .census
            .chats
            .iter()
            .filter(|source| {
                source.proof.as_ref() == Some(&stopped.source)
                    && stopped_chat(&stopped, source)
                    && !chats.iter().any(|current| current.id == source.id)
            })
            .cloned()
            .collect();
        if paused.iter().any(|source| {
            capture
                .census
                .snapshots
                .iter()
                .filter(|saved| saved.source == *source)
                .count()
                != 1
        }) {
            return Err("The exact stopped conversation page was not captured; Ready recovery was not claimed".into());
        }
        let ids: BTreeSet<_> = paused.iter().map(|source| source.id).collect();
        let aliases = capture
            .census
            .aliases
            .iter()
            .filter(|alias| {
                ids.contains(&alias.id) && alias.proof.as_ref() == Some(&stopped.source)
            })
            .cloned()
            .collect();
        let mut seen = BTreeSet::new();
        let paused_terminals = captured_terminals(&capture.census)
            .filter(|source| {
                source.continuity.live_proof().as_ref() == Some(&stopped.source)
                    && stopped.terminals.contains(&source.id)
                    && !terminal_is_listed(source, terminals)
                    && seen.insert(source.id)
            })
            .cloned()
            .collect::<Vec<_>>();
        if !paused.is_empty() || !paused_terminals.is_empty() {
            partitions.push(Partition {
                source: stopped.source.clone(),
                checkpoint_hash: ready.checkpoint_hash,
                capture_hash: ready.capture_hash,
                chats: paused,
                aliases,
                terminals: paused_terminals,
            });
        }
    }
    let publication = Publication {
        version: 1,
        serving: serving.clone(),
        target: target.to_owned(),
        target_hash: executable_hash(target)?,
        partitions,
    };
    let bytes = serde_json::to_vec(&publication).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > LIMIT {
        return Err("Ready stopped-pane publication exceeds its size limit".into());
    }
    let path = store.path.with_file_name(FILE);
    let temporary = path.with_file_name(format!(".ready-outcome-{}", uuid::Uuid::new_v4()));
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
        fs::rename(&temporary, &path).map_err(|e| e.to_string())?;
        File::open(path.parent().ok_or("Ready publication has no directory")?)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::tests::{private_root, proof, source, write_private};
    use super::*;

    #[test]
    fn ready_paused_publication_requires_serving_owner_exact_capture_and_fresh_retention() {
        let directory = private_root();
        let checkpoint = directory.path().join("daemon-upgrade.json");
        let target = directory.path().join("cagentsd");
        write_private(&target, b"fixture; never executed");
        let original = source(crate::agent::Provider::Claude, directory.path());
        let checkpoint_hash = "b".repeat(64);
        let capture = CapturedCensus {
            producer: Some(CaptureProducer::Acknowledged {
                job: uuid::Uuid::new_v4().to_string(),
                request_hash: "a".repeat(64),
            }),
            source: proof("source"),
            checkpoint_hash: Some(checkpoint_hash.clone()),
            census: Census {
                chats: vec![original.clone()],
                snapshots: vec![super::super::super::super::floor::StoppedSnapshot {
                    source: original.clone(),
                    snapshot: Default::default(),
                    limitation: None,
                }],
                ..Default::default()
            },
            windows: vec![],
        };
        let bytes = serde_json::to_vec(&capture).unwrap();
        let capture_file = bound_capture_path(&checkpoint, &checkpoint_hash);
        write_private(&capture_file, &bytes);
        let stopped = super::super::super::super::floor::Record::new(
            proof("source"),
            vec![original.clone()],
            vec![],
        );
        let ledger = super::super::super::super::floor::Ledger {
            records: vec![stopped],
        };
        // Pruning may clear service_stopped, while the exact paused pane stays.
        assert!(!ledger.records[0].service_stopped);
        let publication = Publication {
            version: 1,
            serving: proof("successor"),
            target: target.clone(),
            target_hash: executable_hash(&target).unwrap(),
            partitions: vec![Partition {
                source: proof("source"),
                checkpoint_hash,
                capture_hash: digest(&bytes),
                chats: vec![original.clone()],
                aliases: vec![],
                terminals: vec![],
            }],
        };
        let path = checkpoint.with_file_name(FILE);
        let published = serde_json::to_vec(&publication).unwrap();
        write_private(&path, &published);
        let view = ReadyView::load_at(&checkpoint, &proof("successor"), &target, &[], &[], &ledger)
            .unwrap()
            .unwrap();
        assert!(view.paused_chat(&original));
        let mut different = original.clone();
        different.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(!view.paused_chat(&different));
        assert!(
            ReadyView::load_at(&checkpoint, &proof("foreign"), &target, &[], &[], &ledger).is_err()
        );
        assert!(
            ReadyView::load_at(
                &checkpoint,
                &proof("successor"),
                &target,
                &[],
                &[original.clone()],
                &ledger
            )
            .is_err()
        );
        assert!(
            ReadyView::load_at(
                &checkpoint,
                &proof("successor"),
                &target,
                &[],
                &[],
                &Default::default()
            )
            .is_err()
        );
        // Rewriting an otherwise identical source capture into a proposed or
        // explicit record cannot qualify a normal Ready paused publication.
        for producer in [
            None,
            Some(CaptureProducer::Proposed {
                job: uuid::Uuid::new_v4().to_string(),
                request_hash: "c".repeat(64),
            }),
            Some(CaptureProducer::ExplicitCensus {
                transaction: uuid::Uuid::new_v4().to_string(),
                census_hash: "d".repeat(64),
            }),
        ] {
            let mut unqualified = capture.clone();
            unqualified.producer = producer;
            let bytes = serde_json::to_vec(&unqualified).unwrap();
            write_private(&capture_file, &bytes);
            let mut changed: Publication = serde_json::from_slice(&published).unwrap();
            changed.partitions[0].capture_hash = digest(&bytes);
            write_private(&path, &serde_json::to_vec(&changed).unwrap());
            assert!(
                ReadyView::load_at(&checkpoint, &proof("successor"), &target, &[], &[], &ledger)
                    .is_err()
            );
        }
        write_private(&path, &published);
        write_private(&capture_file, b"different capture");
        assert!(
            ReadyView::load_at(&checkpoint, &proof("successor"), &target, &[], &[], &ledger)
                .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), published);
        assert!(
            !checkpoint.exists(),
            "reading paused history cannot create a checkpoint"
        );
    }
}
