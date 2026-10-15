//! Private matched-CLI execution pin. Durable terminal recipes never contain
//! this override, and no public spawn option can construct one.
use std::{fs::File,io::Read,os::{fd::AsRawFd,unix::fs::{MetadataExt,OpenOptionsExt}},path::Path,sync::{Arc,Mutex,OnceLock},time::{Duration,Instant}};
use serde::{Deserialize,Serialize};

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
struct FileIdentity {device:u64,inode:u64,length:u64,modified:(i64,i64),changed:(i64,i64)}
impl FileIdentity {
    fn capture(file:&File)->Result<Self,String> {
        let metadata=file.metadata().map_err(|error|error.to_string())?;
        let uid=unsafe {libc::geteuid()};
        if !metadata.is_file() || metadata.nlink()==0 || metadata.mode()&0o111==0 || metadata.mode()&0o022!=0
            || (metadata.uid()!=uid && metadata.uid()!=0) {
            return Err("The matched CLI must be a linked, executable regular file without group/world write access".into());
        }
        use std::os::unix::fs::FileExt;
        let mut magic=[0;4];file.read_exact_at(&mut magic,0).map_err(|error|error.to_string())?;
        if &magic!=b"\x7fELF" {return Err("The private console execution override requires the matched native CLI executable".into());}
        Ok(Self {device:metadata.dev(),inode:metadata.ino(),length:metadata.len(),modified:(metadata.mtime(),metadata.mtime_nsec()),changed:(metadata.ctime(),metadata.ctime_nsec())})
    }
}

#[derive(Debug,Clone,Serialize,Deserialize)]
pub(crate) struct Descriptor {file:FileIdentity,build:crate::console_identity::Identity}

#[derive(Debug)]
pub(crate) struct MatchedCli {file:Arc<File>,descriptor:Descriptor}
impl MatchedCli {
    #[cfg(test)]
    pub(crate) fn capture_for_test(path:&Path)->Result<Self,String> {Self::capture(path)}
    /// Call outside HostLock and before any predecessor teardown. One captured
    /// file/probe is shared across all consoles in this exact daemon image.
    pub(crate) fn current()->Result<Arc<Self>,String> {
        static CURRENT:OnceLock<Mutex<Option<Arc<MatchedCli>>>>=OnceLock::new();
        let mut current=CURRENT.get_or_init(||Mutex::new(None)).lock().unwrap_or_else(|p|p.into_inner());
        if let Some(pin)=current.as_ref() {pin.validate()?;return Ok(pin.clone());}
        if crate::VERSION.ends_with(".dirty") {
            return Err("Console remapping requires a clean matched CLI/daemon build; the existing provider and console recipe were preserved".into());
        }
        let path=crate::companion::Product::Cli.beside_current().map_err(|error|error.to_string())?;
        let pin=Arc::new(Self::capture(&path)?);*current=Some(pin.clone());Ok(pin)
    }
    fn capture(path:&Path)->Result<Self,String> {
        let file=Arc::new(std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path).map_err(|error|format!("Open the matched console CLI: {error}"))?);
        let before=FileIdentity::capture(&file)?;
        let nonce=uuid::Uuid::new_v4();
        let build=probe(&file,nonce,Instant::now()+Duration::from_secs(2))?;
        build.validate(nonce)?;
        let after=FileIdentity::capture(&file)?;
        if before!=after {return Err("The console executable changed while its matched build was verified".into());}
        Ok(Self {file,descriptor:Descriptor {file:after,build}})
    }
    pub(crate) fn validate(&self)->Result<(),String> {validate_received(&self.file,&self.descriptor)}
    pub(super) fn file(&self)->&Arc<File> {&self.file}
    pub(super) fn descriptor(&self)->Descriptor {self.descriptor.clone()}
}

pub(super) fn validate_received(file:&File,descriptor:&Descriptor)->Result<(),String> {
    descriptor.build.validate_current()?;
    if FileIdentity::capture(file)?!=descriptor.file {return Err("The pinned matched console executable changed before spawn".into());}
    Ok(())
}

fn probe(file:&File,nonce:uuid::Uuid,deadline:Instant)->Result<crate::console_identity::Identity,String> {
    use std::process::{Command,Stdio};
    let mut child=Command::new(format!("/proc/self/fd/{}",file.as_raw_fd()))
        .args([crate::console_identity::ARGUMENT,&nonce.to_string()]).env_clear()
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|error|format!("Verify the matched console CLI: {error}"))?;
    let result=(|| {
        let mut output=child.stdout.take().ok_or("The console identity probe has no output pipe")?;
        let flags=unsafe {libc::fcntl(output.as_raw_fd(),libc::F_GETFL)};
        if flags<0 || unsafe {libc::fcntl(output.as_raw_fd(),libc::F_SETFL,flags|libc::O_NONBLOCK)}<0 {return Err(std::io::Error::last_os_error().to_string());}
        let mut bytes=Vec::new();let mut status=None;
        loop {
            let mut part=[0;1024];
            loop {match output.read(&mut part) {
                Ok(0)=>break,
                Ok(read)=>{bytes.extend_from_slice(&part[..read]);if bytes.len()>4096 {return Err("The console identity probe exceeded its bounded output".into());}},
                Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=>break,
                Err(error) if error.kind()==std::io::ErrorKind::Interrupted=>continue,
                Err(error)=>return Err(error.to_string()),
            }}
            if status.is_some() {break;}
            status=child.try_wait().map_err(|error|error.to_string())?;
            if Instant::now()>=deadline {return Err("The matched console identity probe exceeded its deadline".into());}
            std::thread::sleep(Duration::from_millis(5));
        }
        if !status.is_some_and(|status|status.success()) {return Err("The sibling CLI refused matched console identity verification".into());}
        serde_json::from_slice(&bytes).map_err(|error|format!("The sibling CLI returned an invalid console identity: {error}"))
    })();
    if result.is_err() && !matches!(child.try_wait(),Ok(Some(_))) {let _=child.kill();let _=child.wait();}
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_script_and_nonexecutable_files_cannot_become_private_console_mappings() {
        use std::os::unix::fs::PermissionsExt;
        let root=tempfile::tempdir().unwrap();let path=root.path().join("cagents");
        std::fs::write(&path,b"#!/bin/sh\necho forged\n").unwrap();std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(MatchedCli::capture(&path).unwrap_err().contains("native CLI"));
        std::fs::write(&path,b"not executable").unwrap();std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(MatchedCli::capture(&path).is_err());
        assert!(MatchedCli::capture(Path::new("/bin/sleep")).is_err(),"an ELF from another product is not a matched CLI");
    }
}
