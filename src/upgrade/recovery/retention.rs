//! Retention for completed upgrade evidence and unused preparation captures.
use super::*;
use crate::fs_safety::{self, Root};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    time::{Duration, SystemTime},
};

pub(super) const GRACE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[cfg(feature = "host")]
pub fn collect_superseded(serving: &HostSessionProof) -> Result<usize, String> {
    let Some(store) = super::super::handoff::Store::existing()? else {
        return Ok(0);
    };
    let RetirementAcquisition::Acquired(guard) =
        RetirementGuard::acquire_for(&store.path, Duration::ZERO)?
    else {
        return Ok(0);
    };
    guard.matches(&store.path)?;
    collect_at(&store.path, serving, SystemTime::now())
}

#[derive(Deserialize)]
struct CheckpointOwner {
    proof: HostSessionProof,
    phase: super::super::handoff::Phase,
}

#[derive(Default)]
struct Bundle<'a> {
    archive: Option<&'a str>,
    sidecars: Vec<&'a str>,
}

fn hash_suffix<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    name.strip_prefix(prefix)
        .and_then(|name| name.strip_suffix(".json"))
        .filter(|hash| valid_hash(hash))
}

/// Checkpoint archives written on a failed or deliberately interrupted path
/// can remain the only evidence of a pane, so retain their matching captures.
fn exceptional_checkpoint(name: &str) -> bool {
    name == "daemon-upgrade.abandoned.json"
        || [
            "daemon-upgrade.stopped.",
            "daemon-upgrade.interrupted-setup.",
        ]
        .iter()
        .any(|prefix| {
            name.strip_prefix(prefix)
                .and_then(|name| name.strip_suffix(".json"))
                .is_some_and(|id| id.len() == 36 && uuid::Uuid::parse_str(id).is_ok())
        })
}

fn expired(metadata: &fs::Metadata, now: SystemTime) -> bool {
    private_single_file(metadata)
        && metadata.len() <= LIMIT
        && metadata
            .modified()
            .ok()
            .and_then(|time| now.duration_since(time).ok())
            .is_some_and(|age| age >= GRACE)
}

fn remove_expired(root: &Root, name: &str, now: SystemTime) -> Result<bool, String> {
    let metadata = match fs_safety::metadata_beneath(root, Path::new(name)) {
        Ok(metadata) => metadata,
        Err(error) if error.is_not_found() => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    if !expired(&metadata, now) {
        return Ok(false);
    }
    fs_safety::unlink_within(root, OsStr::new(name)).map_err(|error| error.to_string())?;
    Ok(true)
}

fn completed_sidecar(directory: &Path, name: &str, hash: &str) -> bool {
    #[derive(Deserialize)]
    struct Identity {
        version: u16,
        checkpoint_hash: String,
        settled: bool,
    }
    #[derive(Deserialize)]
    struct Cold {
        version: u16,
        witness: Witness,
    }
    #[derive(Deserialize)]
    struct Witness {
        checkpoint_hash: String,
    }

    if name.starts_with("daemon-upgrade.retirement.") {
        // Its contents and filename were verified when grouping it.
        return true;
    }
    let Ok(Some(bytes)) = read_checkpoint(&directory.join(name)) else {
        return false;
    };
    if name.starts_with("daemon-upgrade.recovery.") {
        serde_json::from_slice::<Identity>(&bytes).is_ok_and(|record| {
            record.version == 2 && record.settled && record.checkpoint_hash == hash
        })
    } else {
        serde_json::from_slice::<Cold>(&bytes)
            .is_ok_and(|record| record.version == 1 && record.witness.checkpoint_hash == hash)
    }
}

fn orphan_outcome_source(directory: &Path, hash: &str) -> Option<HostSessionProof> {
    #[derive(Deserialize)]
    struct Outcome {
        source: HostSessionProof,
        checkpoint_hash: String,
    }
    let bytes = read_checkpoint(&directory.join(recovery_name(hash))).ok()??;
    let outcome = serde_json::from_slice::<Outcome>(&bytes).ok()?;
    (outcome.checkpoint_hash == hash && outcome.source.is_complete()).then_some(outcome.source)
}

pub(super) fn collect_at(
    checkpoint: &Path,
    serving: &HostSessionProof,
    now: SystemTime,
) -> Result<usize, String> {
    let directory = checkpoint
        .parent()
        .ok_or("Recovery storage has no directory")?;
    validate_private_directory(directory)?;
    let root = Root::open(directory).map_err(|error| error.to_string())?;
    let names = fs_safety::directory_entries_beneath(&root, Path::new(""), usize::MAX)
        .map_err(|error| error.to_string())?;
    let names: Vec<_> = names.iter().filter_map(|name| name.to_str()).collect();
    if names.iter().any(|name| {
        Some(OsStr::new(name)) == checkpoint.file_name()
            || name.starts_with(".checkpoint-retirement.")
            || name.starts_with(".retirement-pointer.")
    }) {
        // Quarantined routing evidence may still refer to any of the extant
        // immutable bundles or captures.
        return Ok(0);
    }

    let mut protected_sources = vec![serving.clone()];
    let mut protected_checkpoints = BTreeSet::new();
    let mut active_checkpoint = None;
    let mut protected_retirements = Vec::new();
    if let Some((retired, proof, _)) = load_retirement(checkpoint, None)? {
        let Some(bytes) = read_checkpoint(&proof.archive_path)? else {
            return Ok(0);
        };
        if digest(&bytes) != proof.checkpoint_sha256 {
            return Ok(0);
        }
        protected_sources.push(retired.source);
        protected_checkpoints.insert(proof.checkpoint_sha256.clone());
        active_checkpoint = Some(proof.checkpoint_sha256);
        protected_retirements.push(proof.retirement_sha256);
        protected_retirements.extend(retired.previous);
    }
    for (hash, source) in verified::ready::retained_captures(checkpoint)? {
        protected_checkpoints.insert(hash);
        protected_sources.push(source);
    }
    for name in names
        .iter()
        .copied()
        .filter(|name| exceptional_checkpoint(name))
    {
        let bytes = read_checkpoint(&directory.join(name))?
            .ok_or("Exceptional checkpoint disappeared during retention")?;
        let owner: CheckpointOwner =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if !owner.proof.is_complete() {
            return Err("Exceptional checkpoint has no complete source proof".into());
        }
        protected_checkpoints.insert(digest(&bytes));
        protected_sources.push(owner.proof);
    }

    let mut bundles: BTreeMap<String, Bundle<'_>> = BTreeMap::new();
    let mut captures = Vec::new();
    let mut superseded_metadata = Vec::new();
    for name in names {
        if let Some(hash) = hash_suffix(name, "daemon-upgrade.prepared.") {
            bundles.entry(hash.into()).or_default().archive = Some(name);
        } else if let Some(hash) = hash_suffix(name, "daemon-upgrade.recovery.")
            .or_else(|| hash_suffix(name, "daemon-upgrade.cold-outcome."))
        {
            bundles.entry(hash.into()).or_default().sidecars.push(name);
        } else if let Some(hash) = hash_suffix(name, "daemon-upgrade.retirement.") {
            if protected_retirements
                .iter()
                .any(|retirement| retirement == hash)
            {
                continue;
            }
            if let Ok(Some(bytes)) = read_checkpoint(&directory.join(name))
                && digest(&bytes) == hash
                && let Ok(retired) = serde_json::from_slice::<Retirement>(&bytes)
                && retired.version == 1
                && valid_hash(&retired.checkpoint_hash)
                && retired.archive_path == archive_path(checkpoint, &retired.checkpoint_hash)
            {
                if active_checkpoint.as_ref() == Some(&retired.checkpoint_hash) {
                    superseded_metadata.push(name);
                } else {
                    bundles
                        .entry(retired.checkpoint_hash)
                        .or_default()
                        .sidecars
                        .push(name);
                }
            }
        } else if hash_suffix(name, "daemon-upgrade.capture.").is_some()
            || hash_suffix(name, "daemon-upgrade.capture-checkpoint.").is_some()
        {
            captures.push(name);
        }
    }

    let mut removed = 0;
    for (hash, bundle) in bundles {
        if protected_checkpoints.contains(&hash) {
            continue;
        }
        let Some(archive) = bundle.archive else {
            // A recovery claim without a completed archive may still be
            // its only surviving evidence.
            if let Some(source) = orphan_outcome_source(directory, &hash) {
                protected_sources.push(source);
            }
            protected_checkpoints.insert(hash);
            continue;
        };
        let bytes = read_checkpoint(&directory.join(archive)).ok().flatten();
        let owner = bytes.as_ref().and_then(|bytes| {
            (digest(bytes) == hash)
                .then(|| serde_json::from_slice::<CheckpointOwner>(bytes).ok())
                .flatten()
        });
        let files = bundle
            .sidecars
            .iter()
            .copied()
            .chain(std::iter::once(archive));
        if !files.clone().all(|name| {
            fs_safety::metadata_beneath(&root, Path::new(name))
                .is_ok_and(|metadata| expired(&metadata, now))
        }) || !owner
            .as_ref()
            .is_some_and(|owner| owner.phase == super::super::handoff::Phase::Prepared)
            || !bytes.is_some_and(|bytes| {
                super::super::handoff::validate_checkpoint_bytes(&bytes).is_ok()
            })
            || !bundle
                .sidecars
                .iter()
                .all(|name| completed_sidecar(directory, name, &hash))
        {
            if let Some(owner) = owner {
                protected_sources.push(owner.proof);
            }
            protected_checkpoints.insert(hash);
            continue;
        }
        // The archive proves these sidecars are superseded. Remove it last so
        // a failed or interrupted pass can finish the same bundle next time.
        for name in files {
            if name == archive && !bundle.sidecars.is_empty() {
                fs_safety::sync_directory(&root).map_err(|error| error.to_string())?;
            }
            if !remove_expired(&root, name, now)? {
                if let Some(owner) = owner {
                    protected_sources.push(owner.proof);
                }
                protected_checkpoints.insert(hash);
                break;
            }
            removed += 1;
        }
    }
    for name in superseded_metadata {
        removed += usize::from(remove_expired(&root, name, now)?);
    }
    for name in captures {
        if !fs_safety::metadata_beneath(&root, Path::new(name))
            .is_ok_and(|metadata| expired(&metadata, now))
        {
            continue;
        }
        let Ok(Some(bytes)) = read_checkpoint(&directory.join(name)) else {
            continue;
        };
        let Ok(owner) = serde_json::from_slice::<CapturedCensus>(&bytes) else {
            continue;
        };
        if !owner.source.is_complete()
            || protected_sources.contains(&owner.source)
            || owner
                .checkpoint_hash
                .as_ref()
                .is_some_and(|hash| protected_checkpoints.contains(hash))
        {
            continue;
        }
        let expected = match owner.checkpoint_hash {
            Some(hash) if valid_hash(&hash) => bound_capture_path(checkpoint, &hash),
            None => capture_path(checkpoint, &owner.source)?,
            Some(_) => continue,
        };
        if expected.file_name() == Some(OsStr::new(name)) {
            removed += usize::from(remove_expired(&root, name, now)?);
        }
    }
    if removed > 0 {
        fs_safety::sync_directory(&root).map_err(|error| error.to_string())?;
    }
    Ok(removed)
}
