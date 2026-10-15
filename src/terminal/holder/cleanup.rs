//! Durable proof that an exact terminal root/PTY owner was explicitly released.
//! Active close first stops its inspected process family; natural completion
//! requires dedicated-subreaper ECHILD after terminating/reaping descendants.
//! Root exit and PTY drain alone never establish family cleanup.
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
    #[serde(default)]
    family_reaped: bool,
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

/// Bounded private lineage captured from an authenticated spawn/adoption.
/// Keep the digest rather than another copy of the issued recipe/environment.
pub(crate) struct SourceReceipt {
    digest: [u8;32],
    proof: HostSessionProof,
}

impl SourceReceipt {
    pub(crate) fn capture(source: &TerminalSession) -> Result<Self,String> {
        Ok(Self {digest:digest(source)?, proof:source.continuity.live_proof()
            .ok_or("Terminal close source has no ownership proof")?})
    }

    pub(crate) fn confirmed(&self, expected: &Record) -> Result<bool,String> {
        confirmed_receipt(expected,&self.proof,Some(self.digest))
    }
}

pub(crate) fn publish(record: &Record, recovery: &Recovery, _family: &crate::daemon::owned_children::ReapedFamily) -> Result<(), String> {
    if record.root.is_current() {
        return Err("Terminal root has not been reaped".into());
    }
    recovery.validate(&record.boot, record.terminal)?;
    let receipt = Receipt {
        family_reaped: true,
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
    Ok(receipt.family_reaped
        && receipt.record == *expected
        && receipt.record.kind == Kind::Terminal
        && receipt.record.version == 5
        && receipt.record.application == crate::instance::APPLICATION_ID
        && receipt.record.boot == proof.boot_id
        && holder_exited(&receipt.record.holder)?
        && !receipt.record.root.is_current()
        && source.is_none_or(|digest| receipt.sources.contains(&digest)))
}

/// An orphan holder may remain a zombie until its new parent reaps it. Its
/// exact identity still exists in /proc, but it can no longer own the socket.
/// Keep this distinct from the root's stronger requirement to have been reaped.
fn holder_exited(holder: &crate::daemon::holder_registry::Process) -> Result<bool, String> {
    let stat = match fs::read(format!("/proc/{}/stat", holder.pid)) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.to_string()),
    };
    let (_, start) = crate::model::agent_recovery::process_identity(&stat)
        .ok_or("Invalid terminal holder process identity")?;
    let state = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .and_then(|end| {
            stat[end + 1..]
                .iter()
                .find(|byte| !byte.is_ascii_whitespace())
        })
        .copied()
        .ok_or("Invalid terminal holder process state")?;
    Ok(start != holder.start || matches!(state, b'Z' | b'X'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exited_holder_is_proven_before_its_parent_reaps_it() {
        use crate::daemon::holder_registry::Process;
        let current = Process::read(std::process::id() as i32).unwrap();
        assert!(!holder_exited(&current).unwrap());
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 0.05"])
            .spawn()
            .unwrap();
        let holder = Process::read(child.id() as i32).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !holder_exited(&holder).unwrap() {
            assert!(std::time::Instant::now() < deadline, "holder did not exit");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            holder.is_current(),
            "fixture must remain an unreaped zombie"
        );
        child.wait().unwrap();
        assert!(holder_exited(&holder).unwrap());
    }
}
