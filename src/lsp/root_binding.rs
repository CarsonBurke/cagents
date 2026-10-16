//! Retain the directory used by the editor's file reader, rather than
//! granting a replacement checkout authority through its reused pathname.

use super::{RootIdentity, discover_root};
use crate::fs_safety::{Root, open_directory_beneath};
use std::{ffi::CString, io, os::unix::ffi::OsStrExt, path::Path, sync::Arc};

#[derive(Debug)]
pub(super) struct ReadRoot {
    pub original: Arc<Root>,
    pub directory: Arc<Root>,
    original_identity: RootIdentity,
    directory_identity: RootIdentity,
}

impl ReadRoot {
    pub fn discover(
        original: Arc<Root>,
        path: &Path,
        worktree: &Path,
        markers: &[String],
    ) -> Result<Arc<Self>, String> {
        if original.path() != worktree || !path.starts_with(worktree) {
            return Err("the editor's retained root does not match this document's worktree".into());
        }
        let original_identity = RootIdentity::from_metadata(
            &original.metadata().map_err(|error| error.to_string())?,
        );
        verify_named(worktree, original_identity).map_err(refusal)?;
        let root = discover_root(path, worktree, markers);
        let relative = root.strip_prefix(worktree).map_err(|_| {
            "the language server root is outside the editor's retained worktree".to_owned()
        })?;
        let directory = if relative.as_os_str().is_empty() {
            Arc::clone(&original)
        } else {
            Arc::new(open_directory_beneath(&original, relative).map_err(|error| error.to_string())?)
        };
        let directory_identity = RootIdentity::from_metadata(
            &directory.metadata().map_err(|error| error.to_string())?,
        );
        let binding = Arc::new(Self {
            original,
            directory,
            original_identity,
            directory_identity,
        });
        binding.validate().map_err(refusal)?;
        Ok(binding)
    }

    pub fn directory_matches(&self, identity: RootIdentity) -> bool {
        self.directory_identity == identity
    }

    pub fn same_scope(&self, other: &Self) -> bool {
        self.original.path() == other.original.path()
            && self.original_identity == other.original_identity
    }

    // Only called on discovery/session workers, never in the UI reducer.
    pub fn validate(&self) -> io::Result<()> {
        verify_named(self.original.path(), self.original_identity)?;
        verify_named(self.directory.path(), self.directory_identity)
    }

    /// Construct everything that allocates before fork. The child validates
    /// both names and enters the retained directory, so a last-moment rename
    /// cannot redirect its cwd into the replacement checkout.
    pub fn bind_command(&self, command: &mut tokio::process::Command) -> io::Result<()> {
        let original_name = CString::new(self.original.path().as_os_str().as_bytes())?;
        let directory_name = CString::new(self.directory.path().as_os_str().as_bytes())?;
        let original_identity = self.original_identity;
        let directory_identity = self.directory_identity;
        // Keep the actual Roots alive in the closure as well as the session.
        let original = Arc::clone(&self.original);
        let directory = Arc::clone(&self.directory);
        // SAFETY: the callback uses only async-signal-safe fstatat/fchdir and
        // reads preallocated C strings and retained descriptors. It neither
        // allocates nor locks after fork.
        unsafe {
            command.pre_exec(move || {
                verify_child_name(&original_name, original_identity)?;
                verify_child_name(&directory_name, directory_identity)?;
                let _keep_original = &original;
                if libc::fchdir(directory.descriptor()) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }
}

fn refusal(error: io::Error) -> String {
    format!("the original editor worktree changed; reopen the document before starting its language server: {error}")
}

fn verify_named(path: &Path, identity: RootIdentity) -> io::Result<()> {
    let metadata = std::fs::metadata(path)?;
    if RootIdentity::from_metadata(&metadata) != identity {
        return Err(io::Error::from_raw_os_error(libc::ESTALE));
    }
    Ok(())
}

fn verify_child_name(name: &std::ffi::CStr, identity: RootIdentity) -> io::Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the string and output storage remain valid for the syscall.
    if unsafe { libc::fstatat(libc::AT_FDCWD, name.as_ptr(), stat.as_mut_ptr(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatat initialized the storage on success.
    let stat = unsafe { stat.assume_init() };
    if stat.st_dev as u64 != identity.device || stat.st_ino as u64 != identity.inode {
        return Err(io::Error::from_raw_os_error(libc::ESTALE));
    }
    Ok(())
}
