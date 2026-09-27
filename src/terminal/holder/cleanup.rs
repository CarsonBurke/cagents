//! Durable proof that an exact terminal root/PTY owner was explicitly released.
//! Active close first stops its inspected process family; natural completion
//! requires root exit and PTY drain. Neither case claims ownership of previously
//! disowned processes outside that terminal family. Native provider holders
//! independently require ECHILD for their provider/tool descendants.
//! Recipes remain in holder memory; only authenticated lineage digests persist.
use super::{Record, Recovery};
use crate::{
    daemon::holder_registry::Kind,
    model::{HostSessionProof, TerminalSession},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
};

#[derive(Serialize, Deserialize)]
struct Receipt {
    record: Record,
    sources: Vec<[u8; 32]>,
}

fn digest(source: &TerminalSession) -> Result<[u8; 32], String> {
    use sha2::{Digest, Sha256};
    let mut source = source.clone();
    let proof = source
        .continuity
        .live_proof()
        .ok_or("Terminal close source has no ownership proof")?;
    if !proof.is_complete() {
        return Err("Terminal close source has incomplete ownership proof".into());
    }
    // These are projection/observation fields, not the immutable worker owner.
    source.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
        host_session_id: proof.host_session_id,
        boot_id: proof.boot_id,
        host_incarnation: proof.host_incarnation,
    };
    source.resume_hint = None;
    Ok(Sha256::digest(serde_json::to_vec(&source).map_err(|e| e.to_string())?).into())
}

pub(crate) fn publish(record: &Record, recovery: &Recovery) -> Result<(), String> {
    if record.root.is_current() {
        return Err("Terminal root has not been reaped".into());
    }
    recovery.validate(&record.boot, record.terminal)?;
    let receipt = Receipt {
        record: record.clone(),
        sources: recovery
            .previous_sources
            .iter()
            .chain(std::iter::once(&recovery.source))
            .map(digest)
            .collect::<Result<_, _>>()?,
    };
    let directory = super::directory()?;
    let destination = directory.join(format!(
        "{}-{}-{}.closed",
        record.terminal, record.holder.pid, record.holder.start
    ));
    let temporary = directory.join(format!(".cleanup-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut file, &receipt).map_err(|e| e.to_string())?;
        file.flush()
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string())?;
        fs::rename(&temporary, &destination).map_err(|e| e.to_string())?;
        fs::File::open(&directory)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub(crate) fn authorize(proof: &HostSessionProof) -> Result<(), String> {
    let paths = crate::daemon::RuntimePaths::discover().map_err(|e| e.to_string())?;
    if crate::daemon::lease_owner(&paths.lock, proof).map_err(|e| e.to_string())?
        != std::process::id()
    {
        return Err("Terminal cleanup caller does not own the daemon lease".into());
    }
    Ok(())
}

/// Absence is not a receipt. A new daemon must present an exact authenticated
/// source alias and own this installation's current exclusive lease.
pub(crate) fn confirmed(
    source: &TerminalSession,
    expected: &Record,
    proof: &HostSessionProof,
) -> Result<bool, String> {
    if source.id != expected.terminal {
        return Ok(false);
    }
    confirmed_receipt(expected, proof, Some(digest(source)?))
}

/// Used only when another authenticated owner (the native holder) retained this
/// exact private Record. Public terminal IDs or launch recipes are insufficient.
pub(crate) fn confirmed_owner(expected: &Record, proof: &HostSessionProof) -> Result<bool, String> {
    confirmed_receipt(expected, proof, None)
}

fn confirmed_receipt(
    expected: &Record,
    proof: &HostSessionProof,
    source: Option<[u8; 32]>,
) -> Result<bool, String> {
    authorize(proof)?;
    let directory = super::directory()?;
    if super::evidence(&super::socket(&directory, expected.terminal))?.is_some() {
        return Ok(false);
    }
    let path = directory.join(format!(
        "{}-{}-{}.closed",
        expected.terminal, expected.holder.pid, expected.holder.start
    ));
    let file = match crate::secure_file::open_existing_private_file(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(256 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 256 * 1024 {
        return Err("Terminal close receipt is oversized".into());
    }
    let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok(receipt.record == *expected
        && receipt.record.kind == Kind::Terminal
        && receipt.record.version == 4
        && receipt.record.application == crate::instance::APPLICATION_ID
        && receipt.record.boot == proof.boot_id
        && !receipt.record.holder.is_current()
        && !receipt.record.root.is_current()
        && source.is_none_or(|digest| receipt.sources.contains(&digest)))
}
