//! Exact cleanup evidence outlives a lost Release acknowledgment. It contains
//! process/lease identities only, never provider configuration or messages.

use std::{fs, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    daemon::holder_registry::{self as registry, Kind, Process, Record},
    model::{HostSessionProof, TerminalId},
};

#[derive(Clone)]
pub(crate) struct Detached {
    pub record: Record,
    pub source: super::super::Session,
    pub proof: HostSessionProof,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Receipt {
    record: Record,
    proof: HostSessionProof,
    #[serde(default)]
    sources: Vec<[u8; 32]>,
    #[serde(default)]
    paired_closed: bool,
    #[serde(default)]
    paired_holder: Option<Record>,
}

pub(super) fn source_digest(source: &crate::chat::Session) -> Result<[u8; 32], String> {
    use sha2::{Digest, Sha256};
    Ok(Sha256::digest(serde_json::to_vec(source).map_err(|error| error.to_string())?).into())
}

fn path(terminal: TerminalId, holder: &Process) -> Result<PathBuf, String> {
    Ok(registry::directory(Kind::Native)?.join(format!(
        "{}-{}-{}.closed",
        terminal, holder.pid, holder.start
    )))
}

pub(super) fn publish(
    record: &Record,
    proof: &HostSessionProof,
    recovery: &super::Recovery,
) -> Result<(), String> {
    let receipt = Receipt {
        record: record.clone(),
        proof: proof.clone(),
        sources: recovery
            .previous_sources
            .iter()
            .chain(std::iter::once(&recovery.source))
            .map(source_digest)
            .collect::<Result<_, _>>()?,
        paired_closed: recovery.source.terminal.is_none(),
        paired_holder: recovery.paired_holder.clone(),
    };
    let path = path(record.terminal, &record.holder)?;
    save(&path, &receipt)
}

pub(crate) fn publish_actor_close(
    record: &Record,
    proof: &HostSessionProof,
    source: &crate::chat::Session,
    lineage: &[crate::chat::Session],
    paired_holder: Option<&Record>,
) -> Result<(), String> {
    let receipt = Receipt {
        record: record.clone(),
        proof: proof.clone(),
        sources: lineage
            .iter()
            .chain(std::iter::once(source))
            .map(source_digest)
            .collect::<Result<_, _>>()?,
        paired_closed: paired_holder.is_none(),
        paired_holder: paired_holder.cloned(),
    };
    save(&path(record.terminal, &record.holder)?, &receipt)
}

pub(crate) fn actor_close_proved(
    record: &Record,
    proof: &HostSessionProof,
) -> Result<bool, String> {
    // The sole caller recovers a lost provider-Close reply, then independently
    // authenticates its exact final ActorIdentity snapshot. Semantic/helper
    // retirement remains a separate operation and receipt.
    if read_actor_provider_close(record, proof)?.is_some() { return Ok(true); }
    // Preserve the existing fully-retired receipt retry after discovery is
    // gone. This path retains consume's original retirement requirement.
    consume(record.terminal, &record.holder, true, |receipt| {
        receipt.record == *record && receipt.proof == *proof
    })
}

/// Authenticated provider-family closure from one exact retained actor. The
/// helper may still own its journal/Store and discovery record. This evidence
/// grants neither semantic retirement nor permission to discard that owner.
pub(crate) struct ActorProviderClosed {
    receipt: Receipt,
    source: [u8; 32],
}
impl ActorProviderClosed {
    pub(crate) fn paired_closed(&self) -> bool { self.receipt.paired_closed }
}

pub(crate) fn actor_provider_closed(
    record: &Record,
    source: &crate::chat::Session,
    proof: &HostSessionProof,
) -> Result<Option<ActorProviderClosed>, String> {
    if source.id != record.terminal {
        return Err("Actor provider-close evidence names another conversation".into());
    }
    let digest = source_digest(source)?;
    Ok(read_actor_provider_close(record, proof)?.filter(|receipt| receipt.sources.contains(&digest))
        .map(|receipt| ActorProviderClosed { receipt, source: digest }))
}

/// Called only after the actual paired owner has proved cleanup. Re-read the
/// exact phase evidence before updating it; a replacement or changed receipt
/// cannot acquire the earlier close's pair acknowledgement.
pub(crate) fn confirm_actor_pair_closed(closed: ActorProviderClosed) -> Result<(), String> {
    let paired_was_closed = closed.paired_closed();
    let previous = closed.receipt;
    let mut current = read_actor_provider_close(&previous.record, &previous.proof)?
        .ok_or("The exact actor provider-close evidence is unavailable")?;
    if current.record != previous.record || current.proof != previous.proof
        || current.sources != previous.sources || current.paired_holder != previous.paired_holder
        || !current.sources.contains(&closed.source)
        || paired_was_closed && !current.paired_closed
    {
        return Err("Actor provider-close evidence changed before paired cleanup was recorded".into());
    }
    if current.paired_closed { return Ok(()); }
    current.paired_closed = true;
    save(&path(current.record.terminal, &current.record.holder)?, &current)
}

fn read_actor_provider_close(record: &Record, proof: &HostSessionProof) -> Result<Option<Receipt>, String> {
    if record.version != 4 || !registry::record_application_matches(record)
        || !crate::chat::runtime::Handle::is_remote_record(record)
        || !proof.is_complete() || record.boot != proof.boot_id
    {
        return Err("Provider-close evidence requires an exact retained actor and complete daemon proof".into());
    }
    let paths = crate::daemon::RuntimePaths::discover().map_err(|error| error.to_string())?;
    if crate::daemon::lease_owner(&paths.lock, proof).map_err(|error| error.to_string())?
        != std::process::id()
    {
        return Err("Actor provider-close caller no longer owns the daemon lease".into());
    }
    let directory = registry::directory(Kind::Native)?;
    let socket = registry::socket(&directory, record.terminal);
    let before = registry::evidence(&socket, Kind::Native)?;
    if before.as_ref().is_some_and(|current| current != record) {
        return Err("A replacement retained actor exists; its provider was preserved".into());
    }
    if before.is_none() { return Ok(None); }
    let file = match crate::secure_file::open_existing_private_file_readonly(&path(record.terminal, &record.holder)?) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(256 * 1024 + 1).read_to_end(&mut bytes).map_err(|error| error.to_string())?;
    if bytes.len() > 256 * 1024 { return Err("Actor provider-close evidence is oversized".into()); }
    let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if receipt.record != *record || receipt.proof != *proof || receipt.sources.is_empty() {
        return Err("Actor provider-close evidence belongs to another exact owner or daemon".into());
    }
    if registry::evidence(&socket, Kind::Native)? != before {
        return Err("Retained actor discovery changed while reading provider-close evidence".into());
    }
    if crate::daemon::lease_owner(&paths.lock, proof).map_err(|error| error.to_string())?
        != std::process::id()
    {
        return Err("Actor provider-close daemon authority changed while reading evidence".into());
    }
    Ok(Some(receipt))
}

fn save(path: &std::path::Path, receipt: &Receipt) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("Native cleanup receipt has no directory")?;
    let temporary = parent.join(format!(".cleanup-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        serde_json::to_writer(&mut file, &receipt).map_err(|error| error.to_string())?;
        file.flush().map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
        fs::File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

impl Detached {
    pub fn close(&self) -> Result<(), String> {
        self.close_inner(false)
    }

    pub fn close_retaining_receipt(&self) -> Result<(), String> {
        self.close_inner(true)
    }

    fn close_inner(&self, retain: bool) -> Result<(), String> {
        let paths = crate::daemon::RuntimePaths::discover().map_err(|error| error.to_string())?;
        if crate::daemon::lease_owner(&paths.lock, &self.proof)
            .map_err(|error| error.to_string())?
            != std::process::id()
        {
            return Err("Native cleanup caller no longer owns the daemon lease".into());
        }
        if self.receipt(retain)? {
            return Ok(());
        }
        let (recovery, _) = super::transport::inspect(&self.record, &self.proof)?;
        // A retained source is lineage evidence, not authority over a new
        // daemon owner. Adoption may not silently rotate a successor backwards.
        if recovery.source.proof.as_ref() != Some(&self.proof) {
            return Err("Native cleanup belongs to a different daemon owner".into());
        }
        let (mut holder, _) = super::transport::adopt(&self.record, &self.source, &self.proof)?;
        holder.close_inner(retain)
    }

    pub fn consume_receipt(&self) -> Result<bool, String> {
        self.receipt(false)
    }

    pub(super) fn receipt(&self, retain: bool) -> Result<bool, String> {
        consume(
            self.record.terminal,
            &self.record.holder,
            retain,
            |receipt| receipt.record == self.record && receipt.proof == self.proof,
        )
    }
}

/// Before a startup ACK, the parent knows its own exact helper but not yet the
/// provider root PID. Only that helper can attest its descendant cleanup.
#[derive(Clone)]
pub(super) struct Startup {
    pub terminal: TerminalId,
    pub holder: Process,
    pub proof: HostSessionProof,
}

impl Startup {
    pub fn consume(&self) -> Result<bool, String> {
        consume(self.terminal, &self.holder, false, |receipt| {
            matches!(receipt.record.version, 3 | 4)
                && receipt.record.terminal == self.terminal
                && receipt.record.holder == self.holder
                && receipt.record.kind == Kind::Native
                && crate::daemon::holder_registry::record_application_matches(&receipt.record)
                && receipt.record.boot == self.proof.boot_id
                && receipt.proof == self.proof
        })
    }
}

fn consume(
    terminal: TerminalId,
    holder: &Process,
    retain: bool,
    matches: impl FnOnce(&Receipt) -> bool,
) -> Result<bool, String> {
    let path = path(terminal, holder)?;
    let file = match if retain {
        crate::secure_file::open_existing_private_file_readonly(&path)
    } else {
        crate::secure_file::open_existing_private_file(&path)
    } {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(256 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > 256 * 1024 {
        return Err("Native cleanup receipt is oversized".into());
    }
    let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if !matches(&receipt) {
        return Err("Native cleanup receipt belongs to another owner".into());
    }
    // Publication precedes discovery retirement. Wait for that final step so
    // acknowledging cleanup also permits immediate same-session replacement.
    if registry::retirement_pending(&receipt.record).map_err(|error| error.to_string())? {
        return Ok(false);
    }
    if retain {
        return Ok(true);
    }
    crate::chat::runtime::remove_actor_final_snapshot(&receipt.record)?;
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    Ok(true)
}

/// An exact issued descriptor can retry a completed close after daemon loss.
/// No launch recipe or environment is written into this private receipt.
pub(crate) fn closed_source(
    source: &crate::chat::Session,
    proof: &HostSessionProof,
) -> Result<Option<(Record, bool)>, String> {
    Ok(find_source(source, proof)?.map(|receipt| (receipt.record, receipt.paired_closed)))
}

/// Return the exact historical receipt proof after authenticating the current
/// daemon lease and the complete issued source digest. Never rotate/backfill it.
pub(crate) fn closed_source_proof(
    source: &crate::chat::Session,
    current_proof: &HostSessionProof,
) -> Result<Option<HostSessionProof>, String> {
    Ok(find_source(source, current_proof)?.map(|receipt| receipt.proof))
}

pub(crate) fn closed_pair_holder(
    source: &crate::chat::Session,
    proof: &HostSessionProof,
) -> Result<Option<Record>, String> {
    Ok(find_source(source, proof)?.and_then(|receipt| receipt.paired_holder))
}

pub(crate) fn confirm_pair_closed(
    source: &crate::chat::Session,
    proof: &HostSessionProof,
) -> Result<(), String> {
    let mut receipt =
        find_source(source, proof)?.ok_or("The native cleanup receipt is unavailable")?;
    receipt.paired_closed = true;
    save(
        &path(receipt.record.terminal, &receipt.record.holder)?,
        &receipt,
    )
}

fn find_source(
    source: &crate::chat::Session,
    proof: &HostSessionProof,
) -> Result<Option<Receipt>, String> {
    let paths = crate::daemon::RuntimePaths::discover().map_err(|error| error.to_string())?;
    if crate::daemon::lease_owner(&paths.lock, proof).map_err(|error| error.to_string())?
        != std::process::id()
    {
        return Err("Native cleanup caller does not own the daemon lease".into());
    }
    let directory = registry::directory(Kind::Native)?;
    let current = registry::evidence(&registry::socket(&directory, source.id), Kind::Native)?;
    let digest = source_digest(source)?;
    let prefix = format!("{}-", source.id);
    let mut candidates = 0;
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let Some(name) = name
            .to_str()
            .and_then(|name| name.strip_prefix(&prefix))
            .and_then(|name| name.strip_suffix(".closed"))
        else {
            continue;
        };
        candidates += 1;
        if candidates > 128 {
            return Err("Too many native cleanup receipts for this identity".into());
        }
        let Some((pid, start)) = name.split_once('-') else {
            continue;
        };
        let (Ok(pid), Ok(start)) = (pid.parse(), start.parse()) else {
            continue;
        };
        let holder = Process { pid, start };
        let mut matched = None;
        let complete = consume(source.id, &holder, true, |receipt| {
            if matches!(receipt.record.version, 3 | 4)
                && receipt.record.terminal == source.id
                && receipt.record.holder == holder
                && receipt.record.kind == Kind::Native
                && crate::daemon::holder_registry::record_application_matches(&receipt.record)
                && receipt.record.boot == proof.boot_id
                && receipt.proof.boot_id == proof.boot_id
                && receipt.proof.is_complete()
                && receipt.sources.contains(&digest)
            {
                matched = Some(receipt.clone());
            }
            // Unrelated old receipts are harmless; only a matching digest is authority.
            true
        })?;
        if complete && matched.is_some() {
            if current.is_some() {
                return Err("A replacement native holder exists after this exact close receipt; it was not stopped".into());
            }
            return Ok(matched);
        }
    }
    Ok(None)
}
