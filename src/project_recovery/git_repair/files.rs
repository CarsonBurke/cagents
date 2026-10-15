//! Directory-relative edits retain the originally inspected inode even if a
//! checkout or Git admin pathname is moved and reused during repair.
use std::{fs::{self,File,OpenOptions},io::{Read,Write},os::{fd::AsRawFd,unix::fs::{MetadataExt,OpenOptionsExt,PermissionsExt}},path::{Path,PathBuf},sync::Arc};

#[derive(Clone)]
pub(super) struct Directory { path:PathBuf, file:Arc<File> }
impl Directory {
    pub(super) fn open(path:&Path)->Result<Self,String> {
        let file=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path).map_err(|error|error.to_string())?;
        let directory=Self{path:path.to_owned(),file:Arc::new(file)};
        directory.require_current()?;Ok(directory)
    }
    pub(super) fn path(&self)->&Path { &self.path }
    pub(super) fn sync(&self)->Result<(),String> { self.file.sync_all().map_err(|error|error.to_string()) }
    pub(super) fn anchor(&self)->PathBuf { PathBuf::from(format!("/proc/{}/fd/{}",std::process::id(),self.file.as_raw_fd())) }
    pub(super) fn require_current(&self)->Result<(),String> {
        let original=self.file.metadata().map_err(|error|error.to_string())?;
        let current=fs::symlink_metadata(&self.path).map_err(|_|"The selected checkout or Git directory moved during Locate. No replacement directory was modified; locate it again.".to_owned())?;
        if !current.is_dir() || current.dev()!=original.dev() || current.ino()!=original.ino() { return Err("The selected checkout or Git directory was replaced during Locate. Its replacement was preserved; locate the original directory again.".into()); }
        Ok(())
    }
    pub(super) fn read(&self,name:&str,limit:u64)->Result<Vec<u8>,String> {
        let file=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(self.anchor().join(name)).map_err(|error|error.to_string())?;
        if !file.metadata().map_err(|error|error.to_string())?.is_file() {return Err("Git repair requires a regular metadata file".into());}
        let mut bytes=Vec::new();file.take(limit+1).read_to_end(&mut bytes).map_err(|error|error.to_string())?;
        if bytes.len() as u64>limit {return Err("Git metadata exceeds its bounded repair size".into());}Ok(bytes)
    }
}

pub(super) struct Edit { directory:Directory,name:&'static str,before:Vec<u8>,after:Vec<u8> }
impl Edit {
    pub(super) fn new(directory:Directory,name:&'static str,before:Vec<u8>,after:Vec<u8>)->Self {Self{directory,name,before,after}}
    pub(super) fn apply(&self)->Result<(),String> { self.apply_with(||{}) }
    pub(super) fn retained_bytes(&self)->usize {self.before.len().saturating_add(self.after.len())}
    pub(super) fn require_current(&self)->Result<(),String> {self.directory.require_current()}
    /// A prior attempt may have published the exact after-image before reporting
    /// a sync/name-check failure. Finish its durability, never publish it twice.
    pub(super) fn resume(&self)->Result<(),String> {
        self.directory.require_current()?;
        let current=self.directory.read(self.name,8*1024*1024)?;
        if current==self.after {self.directory.sync()?;return Ok(());}
        if current!=self.before {return Err("Git metadata changed while its original repair was pending. Its changes and the pending operation were retained.".into());}
        self.apply()
    }
    fn apply_with(&self,before_commit:impl FnOnce())->Result<(),String> {
        self.directory.require_current()?;
        if self.before==self.after {return Ok(());}
        let target=self.directory.anchor().join(self.name);
        let original=fs::symlink_metadata(&target).map_err(|error|error.to_string())?;
        if !original.is_file() {return Err("Git metadata changed while preparing Locate. Retry after its writer finishes.".into());}
        let lock_path=self.directory.anchor().join(format!("{}.lock",self.name));
        let file=OpenOptions::new().write(true).create_new(true).mode(original.mode()&0o777).custom_flags(libc::O_NOFOLLOW).open(&lock_path).map_err(|error|format!("Cannot reserve Git metadata for repair: {error}"))?;
        struct Lock {path:PathBuf,file:File}
        impl Drop for Lock {fn drop(&mut self){let same=self.file.metadata().ok().zip(fs::symlink_metadata(&self.path).ok()).is_some_and(|(held,current)|held.dev()==current.dev()&&held.ino()==current.ino());if same {let _=fs::remove_file(&self.path);}}}
        let mut lock=Lock{path:lock_path,file};
        let current=fs::symlink_metadata(&target).map_err(|error|error.to_string())?;
        if current.dev()!=original.dev() || current.ino()!=original.ino() || self.directory.read(self.name,8*1024*1024)?!=self.before {return Err("Git metadata changed while preparing Locate. Its changes were preserved; retry after its writer finishes.".into());}
        lock.file.set_permissions(fs::Permissions::from_mode(original.mode()&0o777)).map_err(|error|error.to_string())?;
        lock.file.write_all(&self.after).map_err(|error|error.to_string())?;
        lock.file.sync_all().map_err(|error|error.to_string())?;
        self.directory.require_current()?;
        // Both names resolve through the retained parent FD. A later rename
        // of that parent cannot redirect this write into a replacement repo.
        before_commit();
        fs::rename(&lock.path,&target).map_err(|error|error.to_string())?;
        self.directory.sync()?;
        self.directory.require_current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_replacement_after_final_check_cannot_redirect_the_anchored_write() {
        let fixture=tempfile::tempdir().unwrap();
        let path=fixture.path().join("selected");fs::create_dir(&path).unwrap();
        fs::write(path.join("config"),b"original").unwrap();
        let directory=Directory::open(&path).unwrap();
        let edit=Edit::new(directory,"config",b"original".to_vec(),b"original repaired".to_vec());
        let retained=fixture.path().join("retained");
        let result=edit.apply_with(|| {
            fs::rename(&path,&retained).unwrap();fs::create_dir(&path).unwrap();
            fs::write(path.join("config"),b"replacement").unwrap();
        });
        assert!(result.unwrap_err().contains("replaced"));
        assert_eq!(fs::read(path.join("config")).unwrap(),b"replacement");
        assert_eq!(fs::read(retained.join("config")).unwrap(),b"original repaired");
        assert!(!path.join("config.lock").exists());assert!(!retained.join("config.lock").exists());
    }
    #[test]
    fn concurrent_metadata_change_and_foreign_lock_are_preserved() {
        let fixture=tempfile::tempdir().unwrap();
        fs::write(fixture.path().join("gitdir"),b"before").unwrap();
        let directory=Directory::open(fixture.path()).unwrap();
        let edit=Edit::new(directory,"gitdir",b"before".to_vec(),b"after".to_vec());
        fs::write(fixture.path().join("gitdir"),b"concurrent").unwrap();
        assert!(edit.apply().is_err());
        assert_eq!(fs::read(fixture.path().join("gitdir")).unwrap(),b"concurrent");
        fs::write(fixture.path().join("gitdir.lock"),b"foreign").unwrap();
        assert!(edit.apply().is_err());
        assert_eq!(fs::read(fixture.path().join("gitdir.lock")).unwrap(),b"foreign");
    }
}
