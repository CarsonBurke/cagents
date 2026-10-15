//! Immutable creation basis stored with Git's worktree administrative identity.
use std::{fs, io::{self, Read, Write}, path::{Path, PathBuf}};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "cagents-created-from.json";
const MAX_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreationBasis {
    /// The resolved local/remote ref, or a literal object ID.
    pub reference: String,
    /// The commit selected when creation was authorized, independent of later ref movement.
    pub oid: String,
}

impl CreationBasis {
    pub fn new(reference: String, oid: String) -> io::Result<Self> {
        crate::repository::Oid::parse_object(oid.clone()).map_err(io::Error::other)?;
        if crate::repository::Oid::parse_object(reference.clone()).is_err() {
            crate::repository::ValidatedRefName::parse(reference.clone()).map_err(io::Error::other)?;
        }
        Ok(Self {reference, oid})
    }

    /// A label accepted by the comparison presentation. The commit itself is
    /// always passed separately, so this synthetic label never resolves Git refs.
    pub fn display_reference(&self) -> io::Result<crate::repository::ValidatedRefName> {
        crate::repository::ValidatedRefName::parse(self.reference.clone())
            .or_else(|_| crate::repository::ValidatedRefName::parse(format!("refs/heads/{}", self.reference)))
            .map_err(io::Error::other)
    }
}

fn bounded_read(path: &Path) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    crate::secure_file::reject_symlink_components(path)?;
    let mut file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_BYTES {
        return Err(io::Error::other("worktree creation receipt is not a bounded regular file"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES { return Err(io::Error::other("worktree creation receipt exceeds its size limit")); }
    Ok(bytes)
}

fn admin_dir(worktree: &Path) -> io::Result<PathBuf> {
    let git = worktree.join(".git");
    crate::secure_file::reject_symlink_components(&git)?;
    if git.is_dir() { return git.canonicalize(); }
    let bytes = bounded_read(&git)?;
    let text = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    let path = text.trim_end_matches(['\r', '\n']).strip_prefix("gitdir: ")
        .ok_or_else(|| io::Error::other("invalid worktree Git administrative pointer"))?;
    let path = Path::new(path);
    let path = if path.is_absolute() { path.to_owned() } else { worktree.join(path) };
    crate::secure_file::reject_symlink_components(&path)?;
    path.canonicalize()
}

pub fn read(worktree: &Path) -> io::Result<Option<CreationBasis>> {
    let admin = match admin_dir(worktree) {
        Ok(admin) => admin,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let bytes = match bounded_read(&admin.join(FILE_NAME)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let basis: CreationBasis = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    CreationBasis::new(basis.reference, basis.oid).map(Some)
}

/// Writes once after durable creation settlement; the record follows `git
/// worktree move` because it belongs to the administrative directory.
pub fn record(admin: &Path, basis: &CreationBasis) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    CreationBasis::new(basis.reference.clone(), basis.oid.clone())?;
    crate::secure_file::reject_symlink_components(admin)?;
    let destination = admin.join(FILE_NAME);
    match bounded_read(&destination) {
        Ok(bytes) => {
            let previous: CreationBasis = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            return if previous == *basis { Ok(()) } else { Err(io::Error::new(io::ErrorKind::AlreadyExists, "worktree creation basis already exists")) };
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let temporary = admin.join(format!(".cagents-created-from-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&temporary)?;
        file.write_all(&serde_json::to_vec(basis).map_err(io::Error::other)?)?;
        file.sync_all()?;
        // Link gives create-if-absent publication, never replacing a concurrent
        // writer's receipt or following a symlink at the destination.
        fs::hard_link(&temporary, &destination)?;
        fs::File::open(admin)?.sync_all()
    })();
    let _ = fs::remove_file(&temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_records_exact_base_and_survives_worktree_move() {
        let root = tempfile::tempdir().unwrap();
        let admin = root.path().join("admin"); fs::create_dir(&admin).unwrap();
        let original = root.path().join("original"); fs::create_dir(&original).unwrap();
        fs::write(original.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        let basis = CreationBasis::new("refs/heads/release/1.2".into(), "a".repeat(40)).unwrap();
        record(&admin, &basis).unwrap();
        assert_eq!(read(&original).unwrap(), Some(basis.clone()));
        let moved = root.path().join("moved"); fs::rename(&original, &moved).unwrap();
        assert_eq!(read(&moved).unwrap(), Some(basis.clone()));
        record(&admin, &basis).unwrap();
        let other = CreationBasis::new("refs/heads/main".into(), "b".repeat(40)).unwrap();
        assert_eq!(record(&admin, &other).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read(&moved).unwrap(), Some(basis));
    }
    #[test]
    fn legacy_worktrees_have_no_recorded_basis_and_invalid_receipts_are_errors() {
        let root = tempfile::tempdir().unwrap(); fs::create_dir(root.path().join(".git")).unwrap();
        assert_eq!(read(root.path()).unwrap(), None);
        fs::write(root.path().join(".git").join(FILE_NAME), "{}").unwrap();
        assert!(read(root.path()).is_err());
    }
    #[test]
    fn receipt_refuses_symlink_storage_and_invalid_commit_ids() {
        let root = tempfile::tempdir().unwrap(); fs::create_dir(root.path().join(".git")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.path().join(".git").join(FILE_NAME)).unwrap();
        assert!(read(root.path()).is_err());
        assert!(CreationBasis::new("refs/heads/main".into(), "not-an-oid".into()).is_err());
        assert!(CreationBasis::new("HEAD;echo dangerous".into(), "a".repeat(40)).is_err());
    }
}
