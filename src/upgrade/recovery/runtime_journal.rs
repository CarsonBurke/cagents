//! Evidence for finishing a runtime owner write without replacing its lock.

use super::{Authority, ReadyClaim, read_checkpoint, ready_evidence_path, validate_lock};
use crate::daemon::{
    lease_handoff::{decode_owner, read_owner_bytes},
    lock_owner_bytes, write_lock_owner_bytes,
};
use serde::{Deserialize, Serialize};
use std::{fs::File, io, os::unix::fs::MetadataExt};

const NAME: &str = "runtime-owner";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerWrite {
    version: u16,
    claim: String,
    inode: (u64, u64),
    before: Vec<u8>,
    after: Vec<u8>,
}

fn inode(lock: &File) -> io::Result<(u64, u64)> {
    let metadata = lock.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(feature = "host")]
pub(super) fn record(
    authority: &Authority,
    claim: &str,
    lock: &File,
    after: &[u8],
) -> io::Result<()> {
    validate_lock(lock, &authority.daemon_path)?;
    let before = read_owner_bytes(lock)?;
    let (owner, pid) = decode_owner(&before)?;
    if owner != authority.source || pid <= 1 {
        return Err(io::Error::other(
            "The Ready lease no longer names its acknowledged source",
        ));
    }
    let record = OwnerWrite {
        version: 1,
        claim: claim.into(),
        inode: inode(lock)?,
        before,
        after: after.to_vec(),
    };
    let path = ready_evidence_path(authority, NAME).map_err(io::Error::other)?;
    crate::update::persist_ready_evidence(&path, &serde_json::to_vec(&record)?)
        .map_err(io::Error::other)
}

/// The original coordinator still owns this OFD and has reaped the claimant.
/// A newly elected coordinator cannot tell whether an older writer touched
/// these bytes after the OFD was released, so it never calls this repair.
pub(super) fn finish_interrupted_write(
    authority: &Authority,
    claimant: &ReadyClaim,
    claim_digest: &str,
    lock: &File,
) -> Result<(), String> {
    validate_lock(lock, &authority.daemon_path).map_err(|error| error.to_string())?;
    let current = read_owner_bytes(lock).map_err(|error| error.to_string())?;
    if decode_owner(&current).is_ok() {
        // Complete owner records still go through the normal predecessor check.
        return Ok(());
    }
    let path = ready_evidence_path(authority, NAME)?;
    let evidence = read_checkpoint(&path)?.ok_or("No evidence explains the runtime owner write")?;
    let record: OwnerWrite =
        serde_json::from_slice(&evidence).map_err(|error| error.to_string())?;
    let source_pid = record.explains(authority, claimant, claim_digest, lock, &current)?;
    // A torn runtime write could not have reached the saved-data claim.
    if crate::instance::state_claim::last_daemon_pid(&authority.daemon_path, &authority.source)
        .map_err(|error| error.to_string())?
        != Some(source_pid)
    {
        return Err("The saved work changed before the runtime owner write finished".into());
    }
    write_lock_owner_bytes(lock, &record.after).map_err(|error| error.to_string())
}

impl OwnerWrite {
    fn explains(
        &self,
        authority: &Authority,
        claimant: &ReadyClaim,
        claim_digest: &str,
        lock: &File,
        current: &[u8],
    ) -> Result<u32, String> {
        let (source, source_pid) = decode_owner(&self.before).map_err(|error| error.to_string())?;
        let proof = claimant
            .successor_proof
            .as_ref()
            .ok_or("The Ready claim has no successor proof")?;
        let expected = lock_owner_bytes(claimant.successor.pid as u32, proof)
            .map_err(|error| error.to_string())?;
        if self.version != 1
            || self.claim != claim_digest
            || self.inode != inode(lock).map_err(|error| error.to_string())?
            || source != authority.source
            || source_pid <= 1
            || self.after != expected
            || current.len() >= self.after.len()
            || !self.after.starts_with(current)
        {
            return Err("The runtime owner bytes do not match the interrupted Ready write".into());
        }
        Ok(source_pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        daemon::holder_registry::Process,
        model::{HostIncarnation, HostSessionId},
    };

    #[test]
    fn runtime_journal_explains_only_the_claimants_prefix_on_the_original_inode() {
        let directory = tempfile::tempdir().unwrap();
        let preparation = super::super::tests::preparation(directory.path());
        let lock = &preparation.worker;
        let authority = &preparation.authority;
        let successor = crate::model::HostSessionProof {
            host_session_id: HostSessionId::new("replacement"),
            host_incarnation: HostIncarnation::new("replacement-incarnation"),
            boot_id: authority.source.boot_id.clone(),
        };
        let claimant = super::super::ready_claim_record(
            authority,
            "original capture",
            &successor,
            Process { pid: 20, start: 1 },
            None,
        );
        let journal = OwnerWrite {
            version: 1,
            claim: "original claim".into(),
            inode: inode(lock).unwrap(),
            before: lock_owner_bytes(10, &authority.source).unwrap(),
            after: lock_owner_bytes(20, &successor).unwrap(),
        };
        for current in [b"".as_slice(), &journal.after[..journal.after.len() / 2]] {
            write_lock_owner_bytes(lock, current).unwrap();
            let actual = read_owner_bytes(lock).unwrap();
            assert_eq!(
                journal.explains(authority, &claimant, "original claim", lock, &actual),
                Ok(10),
            );
        }
        let partial = read_owner_bytes(lock).unwrap();
        assert!(
            journal
                .explains(authority, &claimant, "other claim", lock, &partial)
                .is_err()
        );
        assert!(
            journal
                .explains(
                    authority,
                    &claimant,
                    "original claim",
                    &preparation.fence,
                    &partial
                )
                .is_err()
        );
        assert!(
            journal
                .explains(
                    authority,
                    &claimant,
                    "original claim",
                    lock,
                    b"{\"unknown\":"
                )
                .is_err()
        );

        write_lock_owner_bytes(lock, &journal.after).unwrap();
        assert_eq!(
            crate::daemon::lease_handoff::recorded_owner(lock).unwrap(),
            (successor, 20)
        );
        assert_eq!(inode(lock).unwrap(), journal.inode);
    }
}
