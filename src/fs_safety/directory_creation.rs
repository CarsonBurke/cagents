//! A directory is owned before its requested public name is published.
//!
//! mkdir returns no descriptor. Bootstrap acquisition therefore depends on a
//! protected namespace: other UIDs cannot change its entries, and cooperating
//! same-UID writers do not modify our random internal staging names. This does
//! NOT exclude concurrent same-UID replacement of the requested public name.
//! Hostile same-UID/root interference inside the protected namespace is outside
//! this contract. No post-hoc stat can make that bootstrap assumption absolute.
use super::*;
use std::os::unix::fs::MetadataExt;

/// A retained namespace whose entries other UIDs cannot replace.
///
/// Linux POSIX ACL write grants are constrained by the group mode mask. Darwin
/// additionally requires successfully observed native ACL absence. A writable
/// shared namespace is not silently promoted to private acquisition authority.
#[derive(Debug)]
pub struct DirectoryNamespace { directory: Root }
impl DirectoryNamespace {
    pub fn acquire(directory: &Root) -> io::Result<Self> {
        namespace_protected(directory)?;
        Ok(Self { directory: directory.try_clone()? })
    }
    pub fn directory(&self) -> &Root { &self.directory }
}

/// Existing/unverified directories grant traversal, never creation ownership.
#[derive(Debug)]
pub enum DirectoryCreation {
    Existing(Root),
    Published(DirectoryPublication),
    Unverified { directory: Root, reason: String },
}
impl DirectoryCreation {
    pub fn directory(&self) -> &Root {
        match self { Self::Existing(root) | Self::Unverified { directory: root, .. } => root,
            Self::Published(published) => published.directory() }
    }
    /// Consumes any creation receipt. Keep Published itself when cleanup or
    /// exact-created-object bookkeeping is needed later.
    pub fn into_directory(self) -> Root {
        match self { Self::Existing(root) | Self::Unverified { directory: root, .. } => root,
            Self::Published(published) => published.directory }
    }
}

/// Prepublication identity, retained even if later permission setup fails.
///
/// Publication is not an fsync/durability claim. Callers must sync the relevant
/// directories and retain this receipt if subsequent fallible steps fail.
/// There is deliberately no automatic Drop cleanup of public or recovery names.
#[derive(Debug)]
pub struct DirectoryPublication {
    directory: Root,
    parent: Root,
    name: OsString,
    namespace: DirectoryNamespace,
    mode_error: Option<String>,
    quarantined: Option<OsString>,
    removed: bool,
}
impl DirectoryPublication {
    pub fn directory(&self) -> &Root { &self.directory }
    pub fn mode_error(&self) -> Option<&str> { self.mode_error.as_deref() }
    /// An unresolved acquired name, not permission to delete its current entry.
    pub fn recovery_path(&self) -> Option<PathBuf> {
        self.quarantined.as_ref().map(|name| self.namespace.directory.path.join(name))
    }
    pub fn is_removed(&self) -> bool { self.removed }

    /// Acquire the public entry into the protected namespace before checking
    /// identity. A mismatched replacement is never chmod'ed or removed. If a
    /// no-clobber restoration is impossible, preserve it at recovery_path and
    /// report the failure. Keep this receipt/journal that path on error.
    pub fn cleanup_empty(&mut self) -> Result<(), ResolveError> {
        self.cleanup_empty_with_hook(|| {})
    }
    fn cleanup_empty_with_hook(&mut self, before_acquire: impl FnOnce()) -> Result<(), ResolveError> {
        if self.removed { return Ok(()); }
        if self.quarantined.is_some() {
            return Err(ResolveError::message(&self.parent.path.join(&self.name),
                "directory cleanup has an unresolved retained recovery entry"));
        }
        namespace_protected(&self.namespace.directory)
            .map_err(|error| ResolveError::io(&self.namespace.directory.path, &error))?;
        let quarantine = unused_name(&self.namespace.directory)?;
        before_acquire();
        rename_new_between(&self.parent, &self.name, &self.namespace.directory, &quarantine)?;
        self.quarantined = Some(quarantine.clone());
        let acquired = resolve_directory(&self.namespace.directory, Path::new(&quarantine));
        let checked = acquired.and_then(|acquired| {
            let original = self.directory.metadata().map_err(|e| ResolveError::io(&self.directory.path,&e))?;
            let current = acquired.metadata().map_err(|e| ResolveError::io(&acquired.path,&e))?;
            if (original.dev(),original.ino()) != (current.dev(),current.ino()) {
                return Err(ResolveError::message(&acquired.path,"directory cleanup acquired a replacement; it was preserved"));
            }
            remove_directory_within(&self.namespace.directory, &quarantine)
        });
        match checked {
            Ok(()) => { self.quarantined = None; self.removed = true; Ok(()) }
            Err(error) => {
                match rename_new_between(&self.namespace.directory,&quarantine,&self.parent,&self.name) {
                    Ok(()) => { self.quarantined=None; Err(error) }
                    Err(restore) => Err(ResolveError::message(&self.namespace.directory.path.join(&quarantine),
                        &format!("{error}; could not restore acquired directory: {restore}; retained recovery entry was not removed"))),
                }
            }
        }
    }
}

/// Preserve basic creation/traversal in shared namespaces without inventing
/// ownership. Private holding callers must require Published and handle its
/// mode_error; recovery callers may traverse Existing/Unverified without chmod.
pub fn create_directory_beneath(root: &Root, relative: &Path, mode: u32) -> Result<DirectoryCreation,ResolveError> {
    match DirectoryNamespace::acquire(root) {
        Ok(namespace) => create_directory_in_namespace(&namespace,root,relative,mode),
        Err(reason) => {
            let (parent,name)=open_parent_beneath(root,relative)?;
            let leaf=c_component(&name,&parent.path)?;
            // SAFETY: held parent and C leaf remain live; no creation ownership
            // is inferred from this syscall followed by name resolution.
            let status=unsafe {libc::mkdirat(parent.descriptor(),leaf.as_ptr(),mode)};
            if status<0 {
                let error=io::Error::last_os_error();
                if error.kind()!=io::ErrorKind::AlreadyExists {return Err(ResolveError::io(&parent.path.join(&name),&error));}
                return resolve_directory(&parent,Path::new(&name)).map(DirectoryCreation::Existing);
            }
            let directory=resolve_directory(&parent,Path::new(&name))?;
            Ok(DirectoryCreation::Unverified {directory,reason:reason.to_string()})
        }
    }
}

/// Publish a directory captured in an explicitly retained protected namespace.
/// This also supports shared nested parents when a protected worktree namespace
/// is available on the same filesystem. EXDEV refuses; there is no path fallback.
pub fn create_directory_in_namespace(namespace:&DirectoryNamespace,root:&Root,relative:&Path,mode:u32)->Result<DirectoryCreation,ResolveError> {
    create_with_hooks(namespace,root,relative,mode,|_,_|{},|_,_|{})
}
fn create_with_hooks(namespace:&DirectoryNamespace,root:&Root,relative:&Path,mode:u32,
    after_capture:impl FnOnce(&Root,&OsStr),after_publish:impl FnOnce(&Root,&OsStr))->Result<DirectoryCreation,ResolveError> {
    let (parent,name)=open_parent_beneath(root,relative)?;
    namespace_protected(&namespace.directory).map_err(|e|ResolveError::io(&namespace.directory.path,&e))?;
    let retained_namespace=DirectoryNamespace {directory:namespace.directory.try_clone()
        .map_err(|e|ResolveError::io(&namespace.directory.path,&e))?};
    let stage=unused_name(&namespace.directory)?;
    let leaf=c_component(&stage,&namespace.directory.path)?;
    // SAFETY: no other-UID namespace writer can replace this entry; our own
    // cooperating writers reserve these internal names. Mode never grants
    // other-UID access, including under a restrictive process umask.
    if unsafe {libc::mkdirat(namespace.directory.descriptor(),leaf.as_ptr(),0o700)} !=0 {
        return Err(ResolveError::io(&namespace.directory.path.join(&stage),&io::Error::last_os_error()));
    }
    let mut directory=match resolve_directory(&namespace.directory,Path::new(&stage)) {
        Ok(directory)=>directory,
        Err(error)=>{
            let cleanup=remove_directory_within(&namespace.directory,&stage);
            return Err(match cleanup {Ok(())=>error,Err(cleanup)=>ResolveError::message(
                &namespace.directory.path.join(&stage),&format!("{error}; private staging cleanup failed: {cleanup}"))});
        }
    };
    after_capture(&parent,&name);
    if let Err(error)=rename_exclusive(&namespace.directory,&stage,&parent,&name) {
        let cleanup=remove_directory_within(&namespace.directory,&stage);
        if let Err(cleanup)=cleanup {return Err(ResolveError::message(&namespace.directory.path.join(&stage),&format!("{error}; private staging cleanup failed: {cleanup}")));}
        if error.kind()==io::ErrorKind::AlreadyExists {
            return resolve_directory(&parent,Path::new(&name)).map(DirectoryCreation::Existing);
        }
        return Err(ResolveError::io(&parent.path.join(&name),&error));
    }
    directory.path=parent.path.join(&name);
    after_publish(&parent,&name);
    let mode_error=set_directory_permissions(&directory,mode).err().map(|error|error.to_string());
    Ok(DirectoryCreation::Published(DirectoryPublication {
        directory,parent,name,namespace:retained_namespace,mode_error,quarantined:None,removed:false,
    }))
}
fn rename_exclusive(source:&Root,from:&OsStr,destination:&Root,to:&OsStr)->io::Result<()> {
    let from=c_name(from)?;
    let to=c_name(to)?;
    // SAFETY: descriptors are retained and names are valid leaf C strings.
    #[cfg(target_os="linux")]
    let status=unsafe {libc::renameat2(source.descriptor(),from.as_ptr(),destination.descriptor(),to.as_ptr(),libc::RENAME_NOREPLACE)};
    #[cfg(target_os="macos")]
    let status=unsafe {libc::renameatx_np(source.descriptor(),from.as_ptr(),destination.descriptor(),to.as_ptr(),libc::RENAME_EXCL)};
    #[cfg(not(any(target_os="linux",target_os="macos")))]
    return Err(io::Error::new(io::ErrorKind::Unsupported,"exclusive directory publication unavailable"));
    #[cfg(any(target_os="linux",target_os="macos"))]
    if status==0 {Ok(())}else{Err(io::Error::last_os_error())}
}
fn unused_name(namespace:&Root)->Result<OsString,ResolveError> {
    // No existence check is authority. Every actual mkdir/rename is exclusive.
    temporary_name().map_err(|e|ResolveError::io(&namespace.path,&e))
}
fn namespace_protected(root:&Root)->io::Result<()> {
    let metadata=root.metadata()?;
    if metadata.uid()!=unsafe {libc::geteuid()} || metadata.mode()&0o022!=0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,
            "directory creation ownership requires an owned namespace without group/other write access"));
    }
    if !native_acl_absent(root)? {return Err(io::Error::new(io::ErrorKind::PermissionDenied,
        "directory creation ownership requires proven native ACL absence"));}
    Ok(())
}
#[cfg(target_os="linux")]
fn native_acl_absent(_: &Root)->io::Result<bool> {Ok(true)}
#[cfg(not(any(target_os="linux",target_os="macos")))]
fn native_acl_absent(_: &Root)->io::Result<bool> {Err(io::Error::new(io::ErrorKind::Unsupported,"protected creation namespaces are unavailable on this platform"))}
#[cfg(target_os="macos")]
fn native_acl_absent(root:&Root)->io::Result<bool> {
    unsafe extern "C" {
        fn filesec_init()->*mut libc::c_void;
        fn filesec_free(value:*mut libc::c_void);
        fn filesec_get_property(value:*mut libc::c_void,property:libc::c_int,output:*mut libc::c_void)->libc::c_int;
        #[cfg_attr(not(target_arch="aarch64"),link_name="fstatx_np$INODE64")]
        fn fstatx_np(fd:libc::c_int,metadata:*mut libc::stat,security:*mut libc::c_void)->libc::c_int;
        fn acl_free(value:*mut libc::c_void)->libc::c_int;
    }
    struct Security(*mut libc::c_void);
    impl Drop for Security {fn drop(&mut self){unsafe {filesec_free(self.0)}}}
    let security=Security(unsafe {filesec_init()});
    if security.0.is_null(){return Err(io::Error::last_os_error());}
    let mut stat=std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {fstatx_np(root.descriptor(),stat.as_mut_ptr(),security.0)} !=0{return Err(io::Error::last_os_error());}
    let mut acl: *mut libc::c_void=ptr::null_mut();
    let status=unsafe {filesec_get_property(security.0,5,(&mut acl as *mut *mut libc::c_void).cast())};
    if status==0 {
        if !acl.is_null() && acl as usize!=1 {unsafe {acl_free(acl)};}
        return Ok(acl as usize==1);
    }
    let error=io::Error::last_os_error();
    if error.raw_os_error()==Some(libc::ENOENT){Ok(true)}else{Err(error)}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs,os::unix::fs::PermissionsExt};
    fn published(result:DirectoryCreation)->DirectoryPublication {
        match result {DirectoryCreation::Published(p)=>p, other=>panic!("expected published receipt, got {other:?}")}
    }
    #[test]
    fn publication_retains_original_before_public_target_replacement() {
        let dir=tempfile::tempdir().unwrap();
        let root=Root::open(dir.path()).unwrap();
        let namespace=DirectoryNamespace::acquire(&root).unwrap();
        let receipt=published(create_with_hooks(&namespace,&root,Path::new("target"),0o750,
            |_,_|{},|_,_|{
                fs::rename(dir.path().join("target"),dir.path().join("retained")).unwrap();
                fs::create_dir(dir.path().join("target")).unwrap();
                fs::set_permissions(dir.path().join("target"),fs::Permissions::from_mode(0o711)).unwrap();
            }).unwrap());
        assert!(receipt.mode_error().is_none());
        assert_eq!(receipt.directory().metadata().unwrap().ino(),fs::metadata(dir.path().join("retained")).unwrap().ino());
        assert_eq!(fs::metadata(dir.path().join("retained")).unwrap().mode()&0o777,0o750);
        assert_eq!(fs::metadata(dir.path().join("target")).unwrap().mode()&0o777,0o711);
    }
    #[test]
    fn occupied_public_target_is_existing_never_owned() {
        let dir=tempfile::tempdir().unwrap();
        let root=Root::open(dir.path()).unwrap();
        let namespace=DirectoryNamespace::acquire(&root).unwrap();
        let result=create_with_hooks(&namespace,&root,Path::new("target"),0o777,|_,_|{
            fs::create_dir(dir.path().join("target")).unwrap();
            fs::set_permissions(dir.path().join("target"),fs::Permissions::from_mode(0o710)).unwrap();
        },|_,_|panic!("occupied target must not publish")).unwrap();
        assert!(matches!(result,DirectoryCreation::Existing(_)));
        assert_eq!(fs::metadata(dir.path().join("target")).unwrap().mode()&0o777,0o710);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(),1);
    }
    #[test]
    fn shared_namespace_returns_unverified_without_permission_claim() {
        let dir=tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(),fs::Permissions::from_mode(0o777)).unwrap();
        let root=Root::open(dir.path()).unwrap();
        assert!(DirectoryNamespace::acquire(&root).is_err());
        assert!(matches!(create_directory_beneath(&root,Path::new("target"),0o700).unwrap(),DirectoryCreation::Unverified{..}));
    }
    #[test]
    fn cleanup_restores_replacement_instead_of_removing_it() {
        let dir=tempfile::tempdir().unwrap();
        let root=Root::open(dir.path()).unwrap();
        let mut receipt=published(create_directory_beneath(&root,Path::new("target"),0o700).unwrap());
        fs::rename(dir.path().join("target"),dir.path().join("original")).unwrap();
        fs::create_dir(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target/foreign"),b"preserve").unwrap();
        assert!(receipt.cleanup_empty().is_err());
        assert!(receipt.recovery_path().is_none());
        assert_eq!(fs::read(dir.path().join("target/foreign")).unwrap(),b"preserve");
        assert!(dir.path().join("original").is_dir());
    }
    #[test]
    fn cleanup_checks_after_acquisition_and_removes_only_exact_empty_original() {
        let dir=tempfile::tempdir().unwrap();
        let root=Root::open(dir.path()).unwrap();
        let mut receipt=published(create_directory_beneath(&root,Path::new("target"),0o700).unwrap());
        receipt.cleanup_empty().unwrap();
        assert!(receipt.is_removed());
        assert!(!dir.path().join("target").exists());
        receipt.cleanup_empty().unwrap();
    }
    #[test]
    fn cleanup_late_replacement_is_preserved_and_restored() {
        let dir=tempfile::tempdir().unwrap();
        let root=Root::open(dir.path()).unwrap();
        let mut receipt=published(create_directory_beneath(&root,Path::new("target"),0o700).unwrap());
        assert!(receipt.cleanup_empty_with_hook(||{
            fs::rename(dir.path().join("target"),dir.path().join("original")).unwrap();
            fs::create_dir(dir.path().join("target")).unwrap();
        }).is_err());
        assert!(dir.path().join("target").is_dir());
        assert!(dir.path().join("original").is_dir());
    }
    #[test]
    fn explicit_protected_namespace_supports_shared_nested_parent() {
        let dir=tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("shared")).unwrap();
        fs::set_permissions(dir.path().join("shared"),fs::Permissions::from_mode(0o777)).unwrap();
        let root=Root::open(dir.path()).unwrap();
        let namespace=DirectoryNamespace::acquire(&root).unwrap();
        let shared=open_directory_beneath(&root,Path::new("shared")).unwrap();
        let receipt=published(create_directory_in_namespace(&namespace,&shared,Path::new("target"),0o750).unwrap());
        assert!(receipt.mode_error().is_none());
        assert_eq!(receipt.directory().metadata().unwrap().mode()&0o777,0o750);
        assert!(dir.path().join("shared/target").is_dir());
    }
    #[cfg(target_os="macos")]
    #[test]
    fn darwin_present_native_acl_is_not_private_namespace_authority() {
        let dir=tempfile::tempdir().unwrap();
        let identity=std::process::Command::new("/usr/bin/id").arg("-un").output().unwrap();
        assert!(identity.status.success());
        let username=String::from_utf8(identity.stdout).unwrap();
        let username=username.trim_end_matches(['\n','\r']);
        assert!(!username.is_empty() && !username.contains([':', '\n','\r']));
        assert!(std::process::Command::new("/bin/chmod").args(["+a",&format!("user:{username}:allow:readattr")]).arg(dir.path()).status().unwrap().success());
        let root=Root::open(dir.path()).unwrap();
        assert!(!native_acl_absent(&root).unwrap());
        assert!(DirectoryNamespace::acquire(&root).is_err());
        assert!(std::process::Command::new("/bin/chmod").arg("-N").arg(dir.path()).status().unwrap().success());
        assert!(native_acl_absent(&root).unwrap());
        assert!(DirectoryNamespace::acquire(&root).is_ok());
    }
    #[test]
    fn restrictive_umask_creation_uses_retained_descriptor() {
        const CHILD:&str="CAGENTS_DIRECTORY_CREATION_UMASK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","fs_safety::directory_creation::tests::restrictive_umask_creation_uses_retained_descriptor","--test-threads=1"])
                .env(CHILD,"1").status().unwrap();
            assert!(status.success()); return;
        }
        let dir=tempfile::tempdir().unwrap();
        let root=Root::open(dir.path()).unwrap();
        let previous=unsafe {libc::umask(0o777)};
        let result=create_directory_beneath(&root,Path::new("target"),0o750);
        unsafe {libc::umask(previous)};
        let receipt=published(result.unwrap());
        assert!(receipt.mode_error().is_none());
        assert_eq!(receipt.directory().metadata().unwrap().mode()&0o777,0o750);
    }
}
