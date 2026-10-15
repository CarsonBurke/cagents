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

pub(crate) fn publish_actor_close(record:&Record,proof:&HostSessionProof,source:&crate::chat::Session,lineage:&[crate::chat::Session])->Result<(),String> {
    let receipt=Receipt {record:record.clone(),proof:proof.clone(),sources:lineage.iter().chain(std::iter::once(source)).map(source_digest).collect::<Result<_,_>>()?,paired_closed:true,paired_holder:None};
    save(&path(record.terminal,&record.holder)?,&receipt)
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
                && receipt.record.application == crate::instance::APPLICATION_ID
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
    let file = match crate::secure_file::open_existing_private_file(&path) {
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
                && receipt.record.application == crate::instance::APPLICATION_ID
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
