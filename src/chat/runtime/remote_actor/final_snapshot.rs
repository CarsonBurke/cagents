//! Exact bounded final publication retained until host cleanup is acknowledged.
use super::*;
use std::os::unix::fs::OpenOptionsExt;
#[derive(Serialize, Deserialize)]
struct Receipt {
    identity: ActorIdentity,
    proof: HostSessionProof,
    snapshot: Snapshot,
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
) -> Result<(), String> {
    let bytes = serde_json::to_vec(&Receipt {
        identity: identity.clone(),
        proof: proof.clone(),
        snapshot: snapshot.clone(),
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
pub(super) fn read(identity: &ActorIdentity, proof: &HostSessionProof) -> Result<Snapshot, String> {
    let file = crate::secure_file::open_existing_private_file(&path(&identity.record)?)
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take((MAX_FRAME + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_FRAME {
        return Err("Final retained snapshot is oversized".into());
    }
    let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if &receipt.identity != identity || &receipt.proof != proof {
        return Err("Final retained snapshot belongs to another actor or attachment".into());
    }
    Ok(receipt.snapshot)
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
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            1,
            "atomic publication leaves no startup temporary file"
        );
    }
}
