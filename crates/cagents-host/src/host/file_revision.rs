use std::{fs::Metadata, os::unix::fs::MetadataExt as _};

/// Identity and change times for a snapshot of a regular file.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct FileRevision {
    device: u64,
    inode: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    pub(super) len: u64,
}

impl FileRevision {
    pub(super) fn of(metadata: &Metadata) -> Option<Self> {
        metadata.is_file().then(|| Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            len: metadata.len(),
        })
    }

    pub(super) fn same_file(self, other: Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}
