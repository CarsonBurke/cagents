//! Private matched-CLI execution pin. Durable terminal recipes never contain
//! this override, and no public spawn option can construct one.
use std::{fs::File,io::Read,os::{fd::AsRawFd,unix::fs::{MetadataExt,OpenOptionsExt}},path::Path,sync::{Arc,Mutex,OnceLock},time::{Duration,Instant}};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
const MAX_EXECUTABLE_BYTES:u64=64*1024*1024;

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
struct FileIdentity {device:u64,inode:u64,length:u64,modified:(i64,i64),changed:(i64,i64)}
impl FileIdentity {
    fn capture(file:&File, require_link:bool)->Result<Self,String> {
        let metadata=file.metadata().map_err(|error|error.to_string())?;
        let uid=unsafe {libc::geteuid()};
        if !metadata.is_file() || (require_link && metadata.nlink()==0) || metadata.len()>MAX_EXECUTABLE_BYTES || metadata.mode()&0o111==0 || metadata.mode()&0o022!=0
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
pub(crate) struct Descriptor {file:FileIdentity,digest:[u8;32],build:crate::console_identity::Identity}

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
        let before=FileIdentity::capture(&file,true)?;
        let digest=digest(&file,&before)?;
        let nonce=uuid::Uuid::new_v4();
        let build=probe(&file,nonce,Instant::now()+Duration::from_secs(2))?;
        build.validate(nonce)?;
        let after=FileIdentity::capture(&file,true)?;
        if before!=after {return Err("The console executable changed while its matched build was verified".into());}
        Ok(Self {file,descriptor:Descriptor {file:after,digest,build}})
    }
    pub(crate) fn validate(&self)->Result<(),String> {validate_received(&self.file,&self.descriptor)}
    pub(super) fn file(&self)->&Arc<File> {&self.file}
    pub(super) fn descriptor(&self)->Descriptor {self.descriptor.clone()}
}

pub(super) fn validate_received(file:&File,descriptor:&Descriptor)->Result<(),String> {
    descriptor.build.validate_current()?;
    // Initial admission requires a linked path. A retained exact inode may be
    // renamed/unlinked by a release switch: only ctime may differ, and then
    // bytes must still match the digest bound to the successful role probe.
    let current=FileIdentity::capture(file,false)?;
    let original=&descriptor.file;
    if current.device!=original.device || current.inode!=original.inode
        || current.length!=original.length || current.modified!=original.modified {
        return Err("The pinned matched console executable changed before spawn".into());
    }
    if current.changed!=original.changed && digest(file,&current)?!=descriptor.digest {
        return Err("The pinned matched console executable bytes changed before spawn".into());
    }
    Ok(())
}

fn digest(file:&File,expected:&FileIdentity)->Result<[u8;32],String> {
    use std::os::unix::fs::FileExt;
    let mut hash=Sha256::new();let mut offset=0;
    let mut bytes=[0u8;64*1024];
    while offset<expected.length {
        let wanted=(expected.length-offset).min(bytes.len() as u64) as usize;
        let read=file.read_at(&mut bytes[..wanted],offset).map_err(|error|error.to_string())?;
        if read==0 {return Err("The pinned console executable was truncated during verification".into());}
        hash.update(&bytes[..read]);offset+=read as u64;
    }
    if FileIdentity::capture(file,false)?!=*expected {return Err("The pinned console executable changed during content verification".into());}
    Ok(hash.finalize().into())
}

/// Reserve before spawn; the caller keeps ordinary lifecycle admission across
/// spawn and quarantine publication. Every failure retains the original Child
/// and existing pidfd instead of joining an unfinished child on this caller.
struct ProbeOwner {
    child:Option<std::process::Child>,
    pidfd:Option<File>,
    reservation:Option<crate::daemon::child_reaper::Reservation>,
}
impl ProbeOwner {
    fn capture(child:std::process::Child,reservation:crate::daemon::child_reaper::Reservation)->Result<Self,String> {
        use std::os::fd::FromRawFd;
        reservation.quarantine();
        let raw=unsafe {libc::syscall(libc::SYS_pidfd_open,child.id(),0)};
        if raw<0 {
            let error=std::io::Error::last_os_error();
            reservation.preserve_suspended(child,None,&format!("Console identity pidfd capture failed: {error}"));
            return Err(error.to_string());
        }
        Ok(Self {child:Some(child),pidfd:Some(unsafe {File::from_raw_fd(raw as i32)}),reservation:Some(reservation)})
    }
}
impl Drop for ProbeOwner {
    fn drop(&mut self) {
        let Some(mut child)=self.child.take() else {return;};
        let Some(reservation)=self.reservation.take() else {unreachable!("original probe owns its reserved reaper credit");};
        if matches!(child.try_wait(),Ok(Some(_))) {return;}
        let Some(pidfd)=self.pidfd.take() else {
            reservation.preserve_suspended(child,None,"The original console identity probe has no pidfd");return;
        };
        // Signalling is exact and nonblocking. The shared reaper retains the
        // original Child until actual wait confirms exit, even after timeout.
        if unsafe {libc::syscall(libc::SYS_pidfd_send_signal,pidfd.as_raw_fd(),libc::SIGKILL,std::ptr::null::<libc::siginfo_t>(),0)}<0 {
            tracing::error!(error=%std::io::Error::last_os_error(),"Console identity probe termination remains unconfirmed");
        }
        if let Err((error,child,pidfd,reservation))=reservation.retain(child,pidfd) {
            reservation.preserve_suspended(child,Some(pidfd),&format!("Console identity cleanup transfer failed: {error}"));
        }
    }
}

fn probe(file:&File,nonce:uuid::Uuid,deadline:Instant)->Result<crate::console_identity::Identity,String> {
    use std::process::{Command,Stdio};
    let reservation=crate::daemon::child_reaper::reserve().map_err(|error|error.to_string())?;
    let child=Command::new(format!("/proc/self/fd/{}",file.as_raw_fd()))
        .args([crate::console_identity::ARGUMENT,&nonce.to_string()]).env_clear()
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|error|format!("Verify the matched console CLI: {error}"))?;
    let mut owner=ProbeOwner::capture(child,reservation)?;
    let child=owner.child.as_mut().expect("probe retains original child");
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
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_executable_accepts_only_same_bytes_after_rename_and_unlink() {
        use std::os::unix::fs::FileExt;
        let root=tempfile::tempdir().unwrap();let path=root.path().join("cagents");
        std::fs::copy("/bin/true",&path).unwrap();
        let file=std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let identity=FileIdentity::capture(&file,true).unwrap();
        let descriptor=Descriptor {digest:digest(&file,&identity).unwrap(),file:identity.clone(),build:crate::console_identity::Identity::current(uuid::Uuid::new_v4())};
        let moved=root.path().join("retired-cagents");
        std::fs::rename(&path,&moved).unwrap();
        std::fs::write(&path,b"replacement path is not execution authority").unwrap();
        validate_received(&file,&descriptor).unwrap();
        std::fs::remove_file(&moved).unwrap();
        validate_received(&file,&descriptor).unwrap();
        assert!(FileIdentity::capture(&file,true).is_err(),"new admission still requires a linked executable");
        // Even an in-place byte change with the old mtime restored cannot use
        // rename permission to bypass the original content binding.
        let mut byte=[0];file.read_exact_at(&mut byte,identity.length-1).unwrap();
        byte[0]^=1;file.write_all_at(&byte,identity.length-1).unwrap();
        let times=[libc::timespec {tv_sec:0,tv_nsec:libc::UTIME_OMIT},libc::timespec {tv_sec:identity.modified.0,tv_nsec:identity.modified.1}];
        assert_eq!(unsafe {libc::futimens(file.as_raw_fd(),times.as_ptr())},0);
        assert!(validate_received(&file,&descriptor).is_err(),"same inode and mtime do not authorize changed bytes");
    }

    #[test]
    fn unfinished_probe_drop_transfers_original_without_waiting_for_exit() {
        let reservation=crate::daemon::child_reaper::reserve().unwrap();
        let child=std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let owner=ProbeOwner::capture(child,reservation).unwrap();
        let pidfd=owner.pidfd.as_ref().unwrap().try_clone().unwrap();
        let started=Instant::now();drop(owner);
        assert!(started.elapsed()<Duration::from_millis(500));
        let mut poll=libc::pollfd {fd:pidfd.as_raw_fd(),events:libc::POLLIN,revents:0};
        assert_eq!(unsafe {libc::poll(&raw mut poll,1,2000)},1,"the exact original receives cleanup while its reaper retains ownership");
    }

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
