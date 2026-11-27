//! The exact owner committed by installed Ready admission. Older binaries
//! still flock and read `daemon.lock`. Keeping those bytes intact lets a
//! successor die at any point in this write without tearing the legacy claim.

use super::*;
use crate::fs_safety::{self, Root};
use sha2::{Digest, Sha256};
#[cfg(any(feature = "host", test))]
use std::ffi::OsStr;

const OWNER: &str = "daemon.owner.json";
const FORMAT: u8 = 1;
const MAX_BYTES: u64 = 4096;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Witness {
    format: u8,
    anchor: Anchor,
    pid: u32,
    host_session: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Anchor {
    device: u64,
    inode: u64,
    digest: [u8; 32],
}

pub(super) fn read_path(path: &Path) -> io::Result<Option<Record>> {
    let file = match open_claim(path, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let directory = open_directory(path)?;
    let bytes = read_record_bytes(&file)?;
    let (owner, _) = observe(&directory, &file, decode_record(&bytes)?, &bytes)?;
    verify_named_claim(&directory, &file)?;
    Ok(Some(owner))
}

#[cfg(any(feature = "host", test))]
pub(super) fn acquire(
    path: &Path,
    ours: &Record,
    patience: Duration,
    accepts: impl FnOnce(&Record) -> bool,
) -> io::Result<StateClaim> {
    let directory = open_directory(path)?;
    acquire_with(
        path,
        ours,
        Claimant::Daemon,
        patience,
        false,
        |file, owner, bytes| {
            verify_named_claim(&directory, file)?;
            let (previous, anchor) = observe(&directory, file, owner, bytes)?;
            if previous.guard != ours.guard || previous.boot != ours.boot || !accepts(&previous) {
                return Err(io::Error::other(
                    "The saved work no longer belongs to the update's acknowledged source",
                ));
            }
            let host_session = ours.host_session.as_ref().ok_or_else(invalid)?;
            publish(
                &directory,
                &Witness {
                    format: FORMAT,
                    anchor,
                    pid: ours.pid,
                    host_session: host_session.clone(),
                },
            )
        },
    )
}

fn open_directory(path: &Path) -> io::Result<Root> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("claim has no directory"))?;
    crate::secure_file::reject_symlink_components(parent)?;
    let directory = Root::open(parent)?;
    let metadata = directory.metadata()?;
    if metadata.uid() != current_uid() || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "saved-data claim directory is not private",
        ));
    }
    Ok(directory)
}

fn verify_named_claim(directory: &Root, file: &File) -> io::Result<()> {
    let named = fs_safety::open_beneath(directory, Path::new(DAEMON_CLAIM))
        .map_err(io::Error::other)?
        .metadata()?;
    let opened = file.metadata()?;
    if (named.dev(), named.ino()) != (opened.dev(), opened.ino()) || named.nlink() != 1 {
        return Err(invalid());
    }
    Ok(())
}

fn observe(
    directory: &Root,
    file: &File,
    mut owner: Record,
    bytes: &[u8],
) -> io::Result<(Record, Anchor)> {
    let metadata = file.metadata()?;
    let anchor = Anchor {
        device: metadata.dev(),
        inode: metadata.ino(),
        digest: Sha256::digest(bytes).into(),
    };
    // A complete legacy record with its own identity supersedes a witness
    // bound to earlier bytes. Those earlier bytes on another inode, however,
    // cannot establish whether the preceding owner was the legacy or Ready one.
    if let Some(witness) = read(directory)?
        && witness.anchor.digest == anchor.digest
    {
        if (witness.anchor.device, witness.anchor.inode) != (anchor.device, anchor.inode) {
            return Err(invalid());
        }
        owner.pid = witness.pid;
        owner.host_session = Some(witness.host_session);
    }
    Ok((owner, anchor))
}

fn read(directory: &Root) -> io::Result<Option<Witness>> {
    let mut file = match fs_safety::open_beneath(directory, Path::new(OWNER)) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(io::Error::other(error)),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != current_uid()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(invalid());
    }
    let bytes = fs_safety::read_bounded(&mut file, MAX_BYTES)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid());
    }
    let witness: Witness = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    validate(&witness)?;
    Ok(Some(witness))
}

fn validate(witness: &Witness) -> io::Result<()> {
    if witness.format != FORMAT || witness.pid == 0 || witness.host_session.is_empty() {
        return Err(invalid());
    }
    Ok(())
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid saved-data owner witness",
    )
}

#[cfg(any(feature = "host", test))]
fn publish(directory: &Root, witness: &Witness) -> io::Result<()> {
    validate(witness)?;
    let bytes = serde_json::to_vec(witness).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid());
    }
    let (mut file, temporary) =
        fs_safety::create_temporary_in(directory, PRIVATE_FILE_MODE).map_err(io::Error::other)?;
    let published = (|| {
        file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs_safety::rename_within(directory, &temporary, OsStr::new(OWNER)).map_err(io::Error::other)
    })();
    if published.is_err() {
        let _ = fs_safety::unlink_within(directory, &temporary);
    }
    published?;
    fs_safety::sync_directory(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SavedClaim {
        root: tempfile::TempDir,
        _runtime: OwnedLock,
        claim: PathBuf,
        source: Record,
    }

    impl SavedClaim {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            prepare_claim_directory(root.path(), Claimant::Daemon).unwrap();
            let guard = root.path().join("cagentsd.lock");
            let _runtime = owned_lock(&guard, libc::LOCK_EX | libc::LOCK_NB).unwrap();
            let claim = root.path().join(DAEMON_CLAIM);
            let source = record(&guard, Some("source")).unwrap();
            drop(create_claim(&claim, &source).unwrap());
            Self {
                root,
                _runtime,
                claim,
                source,
            }
        }

        fn owner(&self, host_session: &str, pid: u32) -> Record {
            Record {
                host_session: Some(host_session.to_owned()),
                pid,
                ..self.source.clone()
            }
        }

        fn handoff(&self, previous: &Record, next: &Record) -> io::Result<StateClaim> {
            acquire(&self.claim, next, Duration::ZERO, |current| {
                current == previous
            })
        }
    }

    #[test]
    fn successive_owners_keep_the_legacy_inode_and_bytes() {
        let fixture = SavedClaim::new();
        let source = &fixture.source;
        let first = fixture.owner("first", 7001);
        let second = fixture.owner("second", 7002);
        let bytes = fs::read(&fixture.claim).unwrap();
        let inode = fs::metadata(&fixture.claim).unwrap().ino();

        let first_claim = fixture.handoff(source, &first).unwrap();
        assert_eq!(read_path(&fixture.claim).unwrap().as_ref(), Some(&first));
        assert!(
            super::super::acquire(
                &fixture.claim,
                second.clone(),
                Claimant::Daemon,
                Duration::ZERO
            )
            .is_err()
        );
        drop(first_claim);
        assert!(fixture.handoff(source, &second).is_err());
        drop(fixture.handoff(&first, &second).unwrap());

        assert_eq!(read_path(&fixture.claim).unwrap(), Some(second));
        assert_eq!(
            super::super::read_path(&fixture.claim).unwrap().as_ref(),
            Some(source)
        );
        assert_eq!(fs::read(&fixture.claim).unwrap(), bytes);
        assert_eq!(fs::metadata(&fixture.claim).unwrap().ino(), inode);
    }

    #[test]
    fn an_intervening_legacy_owner_supersedes_the_witness() {
        let fixture = SavedClaim::new();
        let first = fixture.owner("first", 7001);
        let legacy = fixture.owner("legacy", 7002);
        let next = fixture.owner("next", 7003);
        drop(fixture.handoff(&fixture.source, &first).unwrap());
        drop(
            super::super::acquire(
                &fixture.claim,
                legacy.clone(),
                Claimant::Daemon,
                Duration::ZERO,
            )
            .unwrap(),
        );

        assert_eq!(read_path(&fixture.claim).unwrap().as_ref(), Some(&legacy));
        assert!(fixture.handoff(&first, &next).is_err());
        drop(fixture.handoff(&legacy, &next).unwrap());
        assert_eq!(read_path(&fixture.claim).unwrap(), Some(next));
        assert_eq!(
            super::super::read_path(&fixture.claim).unwrap(),
            Some(legacy)
        );
    }

    #[test]
    fn an_identical_legacy_record_on_another_inode_never_transfers_authority() {
        let fixture = SavedClaim::new();
        let first = fixture.owner("first", 7001);
        let next = fixture.owner("next", 7002);
        drop(fixture.handoff(&fixture.source, &first).unwrap());
        let original = fixture.root.path().join("original.lock");
        fs::rename(&fixture.claim, &original).unwrap();
        drop(create_claim(&fixture.claim, &fixture.source).unwrap());
        assert_eq!(
            fs::read(&fixture.claim).unwrap(),
            fs::read(original).unwrap()
        );

        assert_eq!(
            read_path(&fixture.claim).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(fixture.handoff(&first, &next).is_err());
        assert!(fixture.handoff(&fixture.source, &next).is_err());
        assert_eq!(
            super::super::read_path(&fixture.claim).unwrap(),
            Some(fixture.source)
        );
    }

    #[test]
    fn failure_before_rename_preserves_the_legacy_record() {
        let fixture = SavedClaim::new();
        let target = fixture.owner("target", 7001);
        let bytes = fs::read(&fixture.claim).unwrap();
        let file = open_claim(&fixture.claim, true).unwrap();
        let lock = owned_file_lock(file, libc::LOCK_EX | libc::LOCK_NB).unwrap();
        let directory = open_directory(&fixture.claim).unwrap();
        let (_, anchor) = observe(&directory, &lock, fixture.source.clone(), &bytes).unwrap();
        let witness = Witness {
            format: FORMAT,
            anchor,
            pid: target.pid,
            host_session: target.host_session.clone().unwrap(),
        };
        let occupied = fixture.root.path().join(OWNER);
        fs::create_dir(&occupied).unwrap();
        fs::write(occupied.join("child"), "occupied").unwrap();
        let entries_before = fs::read_dir(fixture.root.path()).unwrap().count();

        assert!(publish(&directory, &witness).is_err());
        drop(lock);
        assert_eq!(
            fs::read_dir(fixture.root.path()).unwrap().count(),
            entries_before
        );
        assert_eq!(fs::read(&fixture.claim).unwrap(), bytes);
        assert!(fixture.handoff(&fixture.source, &target).is_err());
        fs::remove_dir_all(occupied).unwrap();
        assert_eq!(
            read_path(&fixture.claim).unwrap().as_ref(),
            Some(&fixture.source)
        );
        drop(fixture.handoff(&fixture.source, &target).unwrap());
        assert_eq!(read_path(&fixture.claim).unwrap(), Some(target));
    }
}
