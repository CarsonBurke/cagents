//! Exact bounded final publication retained until host cleanup is acknowledged.
use super::*;
use std::os::unix::fs::OpenOptionsExt;
#[derive(Serialize, Deserialize)]
struct Receipt {
    identity: ActorIdentity,
    proof: HostSessionProof,
    snapshot: Snapshot,
    #[serde(default)]
    source_authority: Option<SourceAuthority>,
    #[serde(default)]
    source_authority_conflict: Option<SourceAuthority>,
}
fn path(record: &Record) -> Result<PathBuf, String> {
    Ok(registry::directory(Kind::Native)?.join(format!(
        "{}-{}-{}.final",
        record.terminal, record.holder.pid, record.holder.start
    )))
}
pub(super) fn save(
    identity: &ActorIdentity,
    proof: &HostSessionProof,
    snapshot: &Snapshot,
    source_authority: Option<SourceAuthority>,
    source_authority_conflict: Option<SourceAuthority>,
) -> Result<(), String> {
    validate_source_authority(snapshot, source_authority, source_authority_conflict)?;
    let bytes = serde_json::to_vec(&Receipt {
        identity: identity.clone(),
        proof: proof.clone(),
        snapshot: snapshot.clone(),
        source_authority,
        source_authority_conflict,
    })
    .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_FRAME - 64 * 1024 {
        return Err(
            "The final retained publication exceeds the private bounded snapshot capacity".into(),
        );
    }
    save_at(&path(&identity.record)?, &bytes)
}
fn save_at(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("Missing final snapshot directory")?;
    let temporary = parent.join(format!(".actor-final-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| error.to_string())?;
        std::fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
        File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
pub(super) struct Publication {
    pub snapshot: Snapshot,
    pub source_authority: Option<SourceAuthority>,
    pub source_authority_conflict: Option<SourceAuthority>,
}
pub(super) fn read_publication(identity: &ActorIdentity, proof: &HostSessionProof) -> Result<Publication, String> {
    let file = crate::secure_file::open_existing_private_file(&path(&identity.record)?)
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take((MAX_FRAME + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_FRAME {
        return Err("Final retained snapshot is oversized".into());
    }
    decode_publication(identity, proof, &bytes)
}
fn decode_publication(identity: &ActorIdentity, proof: &HostSessionProof, bytes: &[u8]) -> Result<Publication, String> {
    let receipt: Receipt = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if &receipt.identity != identity || &receipt.proof != proof {
        return Err("Final retained snapshot belongs to another actor or attachment".into());
    }
    validate_source_authority(&receipt.snapshot, receipt.source_authority, receipt.source_authority_conflict)?;
    Ok(Publication { snapshot: receipt.snapshot, source_authority: receipt.source_authority, source_authority_conflict: receipt.source_authority_conflict })
}
fn validate_source_authority(snapshot: &Snapshot, authority: Option<SourceAuthority>, conflict: Option<SourceAuthority>) -> Result<(), String> {
    if let Some(conflict) = conflict {
        let first = authority.ok_or("Final retained conflict lacks its original source token")?;
        if conflict.generation != first.generation || conflict.incarnation == first.incarnation {
            return Err("Final retained conflict does not contain the original contradictory tokens".into());
        }
        // Keep both exact facts even though the body is quarantined. They
        // neither grant source access nor certify provider/session retirement.
        return Ok(());
    }
    if authority.is_some_and(|authority| authority.incarnation.is_nil()
        || snapshot.source_incarnation != Some(authority.incarnation)
        || authority.generation > snapshot.generation) {
        return Err("Final retained source authority is invalid or disagrees with its body".into());
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Retirement {
    protocol: u32,
    semantic_acknowledged: bool,
    identity: ActorIdentity,
    proof: HostSessionProof,
}
fn retirement_path(record: &Record) -> Result<PathBuf, String> {
    Ok(path(record)?.with_extension("retired"))
}
pub(super) fn publish_retirement(
    identity: &ActorIdentity,
    proof: &HostSessionProof,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(&Retirement {
        protocol: PROTOCOL,
        semantic_acknowledged: true,
        identity: identity.clone(),
        proof: proof.clone(),
    })
    .map_err(|error| error.to_string())?;
    save_at(&retirement_path(&identity.record)?, &bytes)
}
pub(super) fn retirement_proved(
    identity: &ActorIdentity,
    proof: &HostSessionProof,
) -> Result<bool, String> {
    if identity.record.holder.is_current() {
        return Ok(false);
    }
    let file =
        match crate::secure_file::open_existing_private_file(&retirement_path(&identity.record)?) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
    let mut bytes = Vec::new();
    file.take(8193)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > 8192 {
        return Err("Retained semantic retirement proof is oversized".into());
    }
    let receipt: Retirement = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if receipt.protocol != PROTOCOL
        || !receipt.semantic_acknowledged
        || &receipt.identity != identity
        || &receipt.proof != proof
    {
        return Err(
            "Retained semantic retirement receipt belongs to another actor or attachment".into(),
        );
    }
    Ok(!registry::retirement_pending(&identity.record).map_err(|error| error.to_string())?)
}
pub(crate) fn remove(record: &Record) -> Result<(), String> {
    let path = path(record)?;
    match std::fs::remove_file(retirement_path(record)?) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    match std::fs::remove_file(&path) {
        Ok(()) => File::open(path.parent().ok_or("Missing final snapshot directory")?)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lost_close_reply_replays_exact_token_while_legacy_final_receipt_stays_unavailable() {
        let actor = RemoteActor::fixture();
        let identity = actor.identity();
        let proof = actor.0.proof.clone();
        let token = SourceAuthority { incarnation: uuid::Uuid::new_v4(), generation: 17 };
        let receipt = Receipt { identity: identity.clone(), proof: proof.clone(),
            snapshot: Snapshot { generation: 20, source_incarnation:Some(token.incarnation), ..Default::default() }, source_authority: Some(token), source_authority_conflict:None };
        let bytes = serde_json::to_vec(&receipt).unwrap();
        let read = decode_publication(&identity, &proof, &bytes).unwrap();
        assert_eq!(read.source_authority, Some(token));
        let mut legacy = serde_json::to_value(&receipt).unwrap();
        legacy.as_object_mut().unwrap().remove("source_authority");
        assert_eq!(decode_publication(&identity, &proof, &serde_json::to_vec(&legacy).unwrap()).unwrap().source_authority, None);
        let mut other = identity.clone(); other.instance = uuid::Uuid::new_v4();
        assert!(decode_publication(&other, &proof, &bytes).is_err());
        let mut contradictory = receipt;
        contradictory.source_authority_conflict = Some(SourceAuthority {incarnation:uuid::Uuid::new_v4(),generation:17});
        let reopened = decode_publication(&identity, &proof, &serde_json::to_vec(&contradictory).unwrap()).unwrap();
        assert_eq!(reopened.source_authority, Some(token));
        assert_eq!(reopened.source_authority_conflict, contradictory.source_authority_conflict);
        let mut malformed = contradictory;
        malformed.source_authority_conflict = None;
        malformed.source_authority = Some(SourceAuthority { generation:21, ..token });
        assert!(decode_publication(&identity, &proof, &serde_json::to_vec(&malformed).unwrap()).is_err());
    }
    #[test]
    fn final_publication_is_private_durable_and_bound_to_the_exact_actor() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let path = directory.path().join("final");
        let actor = RemoteActor::fixture();
        let identity = actor.identity();
        let proof = actor.0.proof.clone();
        let receipt = Receipt {
            identity: identity.clone(),
            proof: proof.clone(),
            source_authority: None,
            source_authority_conflict: None,
            snapshot: Snapshot {
                generation: 77,
                title: Some("final".into()),
                ..Default::default()
            },
        };
        save_at(&path, &serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let read: Receipt = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(read.identity, identity);
        assert_eq!(read.proof, proof);
        assert_eq!(read.snapshot, receipt.snapshot);
        assert_eq!(read.source_authority, receipt.source_authority);
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            1,
            "atomic publication leaves no startup temporary file"
        );
    }
}
