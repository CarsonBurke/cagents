//! Original regular-file/symlink preimages for the local native Trash producer.
//!
//! This grants no creator, move, cleanup or settlement capability. A caller
//! must charge the complete batch before retaining proofs or starting effects.
//! Each proof owns two new descriptors (parent and original payload); its
//! source root is an alias of the caller's existing retained source. Every
//! effect still requires the current local physical topology and original custody.
use super::*;

pub(super) struct NativePreimage {
    source: Arc<fs_safety::Root>,
    path: RepoPath,
    parent: Option<Arc<fs_safety::Root>>,
    name: OsString,
    payload: Arc<fs::File>,
    metadata: fs::Metadata,
    content: Content,
}

enum Content {
    Regular {
        digest: [u8; 32],
        access: SavedFileAccess,
    },
    Symlink {
        target: PathBuf,
    },
}

fn same_version(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    publication::full_version(left) == publication::full_version(right)
        && left.nlink() == right.nlink()
}

fn same_directory(left: &fs_safety::Root, right: &fs_safety::Root) -> Result<bool, String> {
    let left = left.metadata().map_err(|error| error.to_string())?;
    let right = right.metadata().map_err(|error| error.to_string())?;
    Ok((left.dev(), left.ino()) == (right.dev(), right.ino()))
}

fn pinned_target(file: &fs::File) -> Result<PathBuf, String> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStringExt};
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    // Linux AT_EMPTY_PATH-style readlink reads the O_PATH symlink itself.
    // It never resolves the link's target or adopts a new named entry.
    let length = unsafe {
        libc::readlinkat(
            file.as_raw_fd(),
            c"".as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if length < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    if length as usize == buffer.len() {
        return Err("Native Trash symlink target is too large".into());
    }
    buffer.truncate(length as usize);
    Ok(PathBuf::from(OsString::from_vec(buffer)))
}

impl NativePreimage {
    /// Worker only; retained Root and confined resolver are the sole intake.
    /// Unreadable regular files refuse without chmod or filesystem effects.
    pub(super) fn capture(
        source: Arc<fs_safety::Root>,
        path: RepoPath,
        cancellation: &CancellationToken,
    ) -> Result<Self, String> {
        if cancellation.is_cancelled() {
            return Err("Native Trash preparation cancelled".into());
        }
        let (parent, name) = fs_safety::open_parent_beneath(&source, path.as_path())
            .map_err(|error| error.to_string())?;
        let metadata = fs_safety::metadata_beneath(&parent, Path::new(&name))
            .map_err(|error| error.to_string())?;
        let (payload, content) = if metadata.is_file() {
            let file = Arc::new(
                fs_safety::open_beneath(&parent, Path::new(&name))
                    .map_err(|error| error.to_string())?,
            );
            if !same_version(
                &metadata,
                &file.metadata().map_err(|error| error.to_string())?,
            ) {
                return Err("Native Trash source changed while being opened".into());
            }
            let access = SavedFileAccess::capture(&file)?;
            let digest = publication::digest_file(&file, metadata.len(), cancellation)?;
            (file, Content::Regular { digest, access })
        } else if metadata.file_type().is_symlink() {
            let file = Arc::new(publication::pin_symlink(&parent, Path::new(&name))?);
            let target = pinned_target(&file)?;
            (file, Content::Symlink { target })
        } else {
            return Err("Native Trash accepts only readable regular files and symlinks".into());
        };
        let proof = Self {
            source,
            path,
            parent: Some(Arc::new(parent)),
            name,
            payload,
            metadata,
            content,
        };
        proof.verify_source(cancellation)?;
        Ok(proof)
    }

    pub(super) fn source_root(&self) -> Arc<fs_safety::Root> {
        self.source.clone()
    }
    pub(super) fn path(&self) -> &RepoPath {
        &self.path
    }
    pub(super) fn parent(&self) -> &fs_safety::Root {
        self.parent
            .as_deref()
            .expect("active original source parent")
    }
    /// Only after the original source and both Trash directories are synced.
    /// Payload identity is never reopened or released here.
    #[cfg(test)]
    pub(super) fn matches_recorded_metadata(&self, expected: &fs::Metadata) -> bool {
        same_version(&self.metadata, expected)
    }
    #[cfg(test)]
    pub(super) fn has_source_parent(&self) -> bool {
        self.parent.is_some()
    }
    pub(super) fn retire_source_parent(&mut self) {
        self.parent = None;
    }
    pub(super) fn name(&self) -> &OsStr {
        &self.name
    }
    pub(super) fn verify_at(
        &mut self,
        parent: &fs_safety::Root,
        name: &OsStr,
        own_rename: bool,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        let before = self.payload.metadata().map_err(|e| e.to_string())?;
        let matches = |actual: &fs::Metadata| {
            if own_rename {
                moved_entry_version(actual) == moved_entry_version(&self.metadata)
                    && (actual.uid(), actual.gid(), actual.nlink())
                        == (
                            self.metadata.uid(),
                            self.metadata.gid(),
                            self.metadata.nlink(),
                        )
            } else {
                same_version(actual, &self.metadata)
            }
        };
        let named =
            fs_safety::metadata_beneath(parent, Path::new(name)).map_err(|e| e.to_string())?;
        if !matches(&before) || !matches(&named) {
            return Err("Native Trash entry changed; original custody was retained".into());
        }
        match &self.content {
            Content::Regular { digest, access } => {
                access.verify(&self.payload)?;
                if *digest
                    != publication::digest_file(&self.payload, self.metadata.len(), cancellation)?
                {
                    return Err("Native Trash contents changed; the entry was preserved".into());
                }
            }
            Content::Symlink { target } => {
                if pinned_target(&self.payload)? != *target {
                    return Err("Native Trash symlink changed".into());
                }
            }
        }
        let after = self.payload.metadata().map_err(|e| e.to_string())?;
        let named_after =
            fs_safety::metadata_beneath(parent, Path::new(name)).map_err(|e| e.to_string())?;
        if !same_version(&before, &after) || !same_version(&after, &named_after) {
            return Err("Native Trash entry changed during verification".into());
        }
        // No fresh content/access baseline is adopted: only known rename ctime.
        self.metadata = after;
        Ok(())
    }

    /// Shared original FD only; callers must return every clone before a
    /// future authenticated transport-disposition phase can close custody.
    pub(super) fn payload(&self) -> Arc<fs::File> {
        Arc::clone(&self.payload)
    }

    fn verify_location(&self) -> Result<(), String> {
        let original_root = self.source.metadata().map_err(|error| error.to_string())?;
        let named_root =
            fs::symlink_metadata(self.source.path()).map_err(|error| error.to_string())?;
        if !named_root.is_dir()
            || (named_root.dev(), named_root.ino()) != (original_root.dev(), original_root.ino())
        {
            return Err("The original native Trash source root was replaced".into());
        }
        let (parent, name) = fs_safety::open_parent_beneath(&self.source, self.path.as_path())
            .map_err(|error| error.to_string())?;
        if name != self.name || !same_directory(&parent, self.parent())? {
            return Err("The original native Trash source parent was replaced".into());
        }
        Ok(())
    }

    pub(super) fn verify_source(&self, cancellation: &CancellationToken) -> Result<(), String> {
        self.verify_source_observing(cancellation, || Ok(()))
    }

    fn verify_source_observing(
        &self,
        cancellation: &CancellationToken,
        before_final: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        if cancellation.is_cancelled() {
            return Err("Native Trash preparation cancelled".into());
        }
        self.verify_location()?;
        let named = fs_safety::metadata_beneath(self.parent(), Path::new(&self.name))
            .map_err(|error| error.to_string())?;
        if !same_version(&self.metadata, &named)
            || !same_version(
                &self.metadata,
                &self.payload.metadata().map_err(|error| error.to_string())?,
            )
        {
            return Err("The original native Trash source changed".into());
        }
        match &self.content {
            Content::Regular { digest, access } => {
                access.verify(&self.payload)?;
                if *digest
                    != publication::digest_file(&self.payload, self.metadata.len(), cancellation)?
                {
                    return Err("The original native Trash source bytes changed".into());
                }
            }
            Content::Symlink { target } => {
                if pinned_target(&self.payload)? != *target {
                    return Err("The original native Trash symlink changed".into());
                }
            }
        }
        // Bracket the complete hash/access/target read without adopting a new
        // baseline. Future rename phases require explicit own-change witnesses.
        before_final()?;
        if cancellation.is_cancelled() {
            return Err("Native Trash preparation cancelled".into());
        }
        self.verify_location()?;
        let named = fs_safety::metadata_beneath(self.parent(), Path::new(&self.name))
            .map_err(|error| error.to_string())?;
        if !same_version(&self.metadata, &named)
            || !same_version(
                &self.metadata,
                &self.payload.metadata().map_err(|error| error.to_string())?,
            )
        {
            return Err("Native Trash source changed during verification".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, os::unix::fs::symlink};

    #[test]
    fn native_preimage_retains_original_file_and_refuses_root_or_leaf_replacement() {
        for replace_root in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root_path = directory.path().join("source");
            fs::create_dir(&root_path).unwrap();
            fs::write(root_path.join("file"), b"original").unwrap();
            let root = Arc::new(fs_safety::Root::open(&root_path).unwrap());
            let proof = NativePreimage::capture(
                root,
                RepoPath::parse("file").unwrap(),
                &CancellationToken::default(),
            )
            .unwrap();
            proof.verify_source(&CancellationToken::default()).unwrap();
            if replace_root {
                fs::rename(&root_path, directory.path().join("retained")).unwrap();
                fs::create_dir(&root_path).unwrap();
            } else {
                fs::rename(root_path.join("file"), root_path.join("retained")).unwrap();
            }
            fs::write(root_path.join("file"), b"foreign!").unwrap();
            assert!(proof.verify_source(&CancellationToken::default()).is_err());
            let mut original = String::new();
            proof
                .payload()
                .as_ref()
                .read_to_string(&mut original)
                .unwrap();
            assert_eq!(original, "original");
            assert_eq!(fs::read(root_path.join("file")).unwrap(), b"foreign!");
        }
    }

    #[test]
    fn native_preimage_refuses_existing_fd_writes_with_restored_mtime() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        fs::write(&path, b"original").unwrap();
        let original_modified = fs::metadata(&path).unwrap().modified().unwrap();
        let root = Arc::new(fs_safety::Root::open(directory.path()).unwrap());
        let proof = NativePreimage::capture(
            root,
            RepoPath::parse("file").unwrap(),
            &CancellationToken::default(),
        )
        .unwrap();
        fs::write(&path, b"modified").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(original_modified))
            .unwrap();
        assert!(proof.verify_source(&CancellationToken::default()).is_err());
        assert_eq!(fs::read(path).unwrap(), b"modified");
    }

    #[test]
    fn native_preimage_final_check_refuses_root_or_nested_parent_replacement_during_read() {
        for replace_root in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root_path = directory.path().join("source");
            fs::create_dir_all(root_path.join("nested")).unwrap();
            fs::write(root_path.join("nested/file"), b"original").unwrap();
            let root = Arc::new(fs_safety::Root::open(&root_path).unwrap());
            let proof = NativePreimage::capture(
                root,
                RepoPath::parse("nested/file").unwrap(),
                &CancellationToken::default(),
            )
            .unwrap();
            let result = proof.verify_source_observing(&CancellationToken::default(), || {
                if replace_root {
                    fs::rename(&root_path, directory.path().join("retained")).unwrap();
                    fs::create_dir_all(root_path.join("nested")).unwrap();
                } else {
                    fs::rename(root_path.join("nested"), root_path.join("retained")).unwrap();
                    fs::create_dir(root_path.join("nested")).unwrap();
                }
                fs::write(root_path.join("nested/file"), b"foreign!").unwrap();
                Ok(())
            });
            assert!(
                result.is_err(),
                "old leaf identity alone cannot prove the current source location"
            );
            let mut original = String::new();
            proof
                .payload()
                .as_ref()
                .read_to_string(&mut original)
                .unwrap();
            assert_eq!(original, "original");
            assert_eq!(
                fs::read(root_path.join("nested/file")).unwrap(),
                b"foreign!"
            );
        }
    }

    #[test]
    fn native_preimage_pins_symlink_and_never_reads_its_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        fs::write(&target, b"private target").unwrap();
        let root_path = directory.path().join("source");
        fs::create_dir(&root_path).unwrap();
        symlink(&target, root_path.join("link")).unwrap();
        let root = Arc::new(fs_safety::Root::open(&root_path).unwrap());
        let proof = NativePreimage::capture(
            root,
            RepoPath::parse("link").unwrap(),
            &CancellationToken::default(),
        )
        .unwrap();
        proof.verify_source(&CancellationToken::default()).unwrap();
        fs::remove_file(root_path.join("link")).unwrap();
        symlink(&target, root_path.join("link")).unwrap();
        assert_eq!(proof.payload().metadata().unwrap().nlink(), 0);
        assert!(proof.verify_source(&CancellationToken::default()).is_err());
        assert_eq!(fs::read(target).unwrap(), b"private target");
    }

    #[test]
    fn native_preimage_directory_and_cancel_refuse_without_effects() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("child")).unwrap();
        let root = Arc::new(fs_safety::Root::open(directory.path()).unwrap());
        assert!(
            NativePreimage::capture(
                root.clone(),
                RepoPath::parse("child").unwrap(),
                &CancellationToken::default()
            )
            .is_err()
        );
        fs::write(directory.path().join("file"), b"keep").unwrap();
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        assert!(
            NativePreimage::capture(root, RepoPath::parse("file").unwrap(), &cancellation).is_err()
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
        assert_eq!(fs::read(directory.path().join("file")).unwrap(), b"keep");
    }
}
