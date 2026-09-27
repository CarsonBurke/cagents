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

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct Receipt {
    record: Record,
    proof: HostSessionProof,
}

fn path(terminal: TerminalId, holder: &Process) -> Result<PathBuf, String> {
    Ok(registry::directory(Kind::Native)?.join(format!(
        "{}-{}-{}.closed",
        terminal, holder.pid, holder.start
    )))
}

pub(super) fn publish(record: &Record, proof: &HostSessionProof) -> Result<(), String> {
    let receipt = Receipt {
        record: record.clone(),
        proof: proof.clone(),
    };
    let path = path(record.terminal, &record.holder)?;
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
        let paths = crate::daemon::RuntimePaths::discover().map_err(|error| error.to_string())?;
        if crate::daemon::lease_owner(&paths.lock, &self.proof)
            .map_err(|error| error.to_string())?
            != std::process::id()
        {
            return Err("Native cleanup caller no longer owns the daemon lease".into());
        }
        if self.consume_receipt()? {
            return Ok(());
        }
        let (recovery, _) = super::transport::inspect(&self.record, &self.proof)?;
        // A retained source is lineage evidence, not authority over a new
        // daemon owner. Adoption may not silently rotate a successor backwards.
        if recovery.source.proof.as_ref() != Some(&self.proof) {
            return Err("Native cleanup belongs to a different daemon owner".into());
        }
        let (mut holder, _) = super::transport::adopt(&self.record, &self.source, &self.proof)?;
        holder.close()
    }

    pub fn consume_receipt(&self) -> Result<bool, String> {
        consume(self.record.terminal, &self.record.holder, |receipt| {
            receipt.record == self.record && receipt.proof == self.proof
        })
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
        consume(self.terminal, &self.holder, |receipt| {
            receipt.record.version == 3
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
    file.take(16 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > 16 * 1024 {
        return Err("Native cleanup receipt is oversized".into());
    }
    let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if !matches(&receipt) {
        return Err("Native cleanup receipt belongs to another owner".into());
    }
    // Publication precedes discovery retirement. Wait for that final step so
    // acknowledging cleanup also permits immediate same-session replacement.
    let socket = registry::socket(&registry::directory(Kind::Native)?, terminal);
    if registry::evidence(&socket, Kind::Native)?.as_ref() == Some(&receipt.record) {
        return Ok(false);
    }
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    Ok(true)
}
