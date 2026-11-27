//! Consent follows the directory itself. The device number in stat is only
//! stable for the life of a mount on some filesystems.
use std::{fs::File, io, path::Path, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{
    automations::DirectoryIdentity, host::agent_execution::working_directory, model::BootId,
};

const REPLACED: &str =
    "Automation target was replaced; review and save its target again before running";

/// Private evidence in the automation revision and agent policy records.
/// Legacy records have just device and inode and still require an exact match.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::host) struct Identity {
    #[serde(flatten)]
    raw: DirectoryIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<Fingerprint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint {
    boot: BootId,
    filesystem: [i32; 2],
    filesystem_type: u64,
    created: std::time::Duration,
}

impl From<DirectoryIdentity> for Identity {
    fn from(raw: DirectoryIdentity) -> Self {
        Self {
            raw,
            fingerprint: None,
        }
    }
}

impl Identity {
    pub(in crate::host) fn capture(directory: &File) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = directory.metadata()?;
        Ok(Self {
            raw: DirectoryIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            fingerprint: fingerprint(directory, &metadata),
        })
    }

    pub(in crate::host) fn matches(&self, found: &Self) -> bool {
        if self.raw.inode != found.raw.inode {
            return false;
        }
        match (&self.fingerprint, &found.fingerprint) {
            (None, _) => self.raw.device == found.raw.device,
            (Some(_), None) => false,
            (Some(saved), Some(current)) => {
                saved.filesystem == current.filesystem
                    && saved.filesystem_type == current.filesystem_type
                    && saved.created == current.created
                    && (saved.boot != current.boot || self.raw.device == found.raw.device)
            }
        }
    }

    pub(in crate::host) fn has_fingerprint(&self) -> bool {
        self.fingerprint.is_some()
    }

    pub(in crate::host) fn raw(&self) -> &DirectoryIdentity {
        &self.raw
    }
}

#[cfg(target_os = "linux")]
fn fingerprint(directory: &File, metadata: &std::fs::Metadata) -> Option<Fingerprint> {
    use std::{mem::MaybeUninit, os::fd::AsRawFd};

    let created = metadata
        .created()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    let mut stats = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: the descriptor is held open, and stats is writable.
    if unsafe { libc::fstatfs(directory.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: fstatfs initialized the struct on success.
    let stats = unsafe { stats.assume_init() };
    // Btrfs includes the subvolume ID in its fsid. Ext4 uses its on-disk
    // UUID. Other filesystems can derive fsid from a volatile device number.
    if !matches!(
        stats.f_type,
        libc::BTRFS_SUPER_MAGIC | libc::EXT4_SUPER_MAGIC
    ) {
        return None;
    }
    // SAFETY: Linux fsid_t is two signed 32-bit words.
    let filesystem = unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(stats.f_fsid) };
    if filesystem == [0, 0] {
        return None;
    }
    Some(Fingerprint {
        boot: crate::model::current_boot_id().ok()?,
        filesystem,
        filesystem_type: stats.f_type as u64,
        created,
    })
}

#[cfg(not(target_os = "linux"))]
fn fingerprint(_: &File, _: &std::fs::Metadata) -> Option<Fingerprint> {
    None
}

pub(super) fn open(path: &Path) -> io::Result<Arc<File>> {
    working_directory::open(path)
        .map_err(|error| io::Error::other(format!("Automation target cannot be used: {error}")))
}

pub(super) fn verify(path: &Path, expected: &Identity) -> io::Result<(Arc<File>, Identity)> {
    let directory = open(path)?;
    let actual = Identity::capture(&directory)?;
    if !expected.matches(&actual) {
        return Err(io::Error::other(REPLACED));
    }
    Ok((directory, actual))
}

/// A precheck request is issued on this boot with the verified descriptor's
/// exact identity. The helper must open that same directory before starting.
pub(super) fn reopen(path: &Path, expected: &DirectoryIdentity) -> io::Result<Arc<File>> {
    let directory = open(path)?;
    let found = working_directory::identity(&directory).map_err(io::Error::other)?;
    if found.device != expected.device || found.inode != expected.inode {
        return Err(io::Error::other(REPLACED));
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn same_path_replacement_and_symlink_do_not_inherit_directory_consent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("worktree");
        std::fs::create_dir(&path).unwrap();
        let identity = Identity::capture(&open(&path).unwrap()).unwrap();
        let (held, _) = verify(&path, &identity).unwrap();
        std::fs::rename(&path, root.join("old")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(verify(&path, &identity).is_err());
        assert_eq!(held.metadata().unwrap().ino(), identity.raw.inode);
        std::fs::remove_dir(&path).unwrap();
        std::os::unix::fs::symlink(root.join("old"), &path).unwrap();
        assert!(verify(&path, &identity).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_renumbered_target_passes_its_current_identity_to_the_precheck() {
        let fixture = crate::fixture_tempdir().unwrap();
        let path = fixture.path().canonicalize().unwrap();
        let directory = open(&path).unwrap();
        let actual = Identity::capture(&directory).unwrap();
        let mut saved = actual.clone();
        let Some(evidence) = saved.fingerprint.as_mut() else {
            // The decision is also tested without depending on the host
            // filesystem below.
            return;
        };
        evidence.boot = BootId::new("previous");
        saved.raw.device ^= 1;
        let (_, checked) = verify(&path, &saved).unwrap();
        assert_eq!(checked, actual);
        assert!(reopen(&path, saved.raw()).is_err());
        assert!(reopen(&path, checked.raw()).is_ok());
    }

    #[test]
    fn a_renumbered_device_needs_matching_cross_boot_filesystem_evidence() {
        let actual = Identity {
            raw: DirectoryIdentity {
                device: 8,
                inode: 256,
            },
            fingerprint: Some(Fingerprint {
                boot: BootId::new("current"),
                filesystem: [123, 456],
                filesystem_type: 1,
                created: std::time::Duration::ZERO,
            }),
        };
        let mut saved = actual.clone();
        saved.raw.device = 7;
        assert!(!saved.matches(&actual), "same-boot device mismatch");
        saved.fingerprint.as_mut().unwrap().boot = BootId::new("previous");
        assert!(saved.matches(&actual), "the device was renumbered");
        saved.fingerprint.as_mut().unwrap().filesystem[0] += 1;
        assert!(
            !saved.matches(&actual),
            "a different filesystem can reuse an inode"
        );
        saved.raw.device = actual.raw.device;
        assert!(
            !saved.matches(&actual),
            "a new boot can reuse the old device number"
        );
        let mut replacement = actual.clone();
        replacement.fingerprint.as_mut().unwrap().created = std::time::Duration::from_secs(1);
        assert!(
            !actual.matches(&replacement),
            "a recreated inode needs fresh consent"
        );
        let legacy: Identity = serde_json::from_str(r#"{"device":7,"inode":256}"#).unwrap();
        assert!(
            !legacy.matches(&actual),
            "legacy identity cannot prove a match"
        );
        assert!(Identity::from(actual.raw.clone()).matches(&actual));
    }
}
