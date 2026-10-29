//! Retained namespace barriers for metadata and newly seeded account homes.
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub(super) struct Directories(Vec<(PathBuf, fs::File)>);

impl Directories {
    pub(super) fn capture(directory: &Path) -> io::Result<Self> {
        crate::secure_file::reject_symlink_components(directory)?;
        let directory = directory.canonicalize()?;
        let mut held = Vec::new();
        // Sync every containing binding, including recursively created
        // ancestors. No credential files or shared-link targets are opened.
        for path in directory.ancestors() {
            let file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?;
            held.push((path.to_owned(), file));
        }
        let result = Self(held);
        result.revalidate()?;
        Ok(result)
    }

    pub(super) fn revalidate(&self) -> io::Result<()> {
        for (path, file) in &self.0 {
            let held = file.metadata()?;
            let named = fs::symlink_metadata(path)?;
            if !held.is_dir()
                || !named.is_dir()
                || (held.dev(), held.ino()) != (named.dev(), named.ino())
            {
                return Err(io::Error::other(
                    "The account storage namespace changed; files were retained",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn sync(&self) -> io::Result<()> {
        self.sync_with(|file| file.sync_all())
    }

    fn sync_with(&self, mut sync: impl FnMut(&fs::File) -> io::Result<()>) -> io::Result<()> {
        self.revalidate()?;
        for (_, file) in &self.0 {
            sync(file)?;
        }
        self.revalidate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_namespace_refuses_replacement_and_never_opens_shared_targets() {
        let fixture = tempfile::tempdir().unwrap();
        let directory = fixture.path().join("accounts/provider");
        fs::create_dir_all(&directory).unwrap();
        let target = fixture.path().join("outside");
        fs::write(&target, "preserve").unwrap();
        std::os::unix::fs::symlink(&target, directory.join("shared")).unwrap();
        let held = Directories::capture(&directory).unwrap();
        held.sync().unwrap();
        fs::rename(&directory, fixture.path().join("original")).unwrap();
        fs::create_dir(&directory).unwrap();
        assert!(held.sync().is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "preserve");
    }

    #[test]
    fn namespace_sync_failure_is_not_acknowledged_or_retried_inside_the_barrier() {
        let fixture = tempfile::tempdir().unwrap();
        let held = Directories::capture(fixture.path()).unwrap();
        let mut calls = 0;
        let result = held.sync_with(|_| {
            calls += 1;
            Err(io::Error::from_raw_os_error(libc::EIO))
        });
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
        assert_eq!(calls, 1);
        held.sync().unwrap();
    }
}
