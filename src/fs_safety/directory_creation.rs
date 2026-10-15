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
#[must_use = "creation provenance must be handled before mutating permissions or cleanup"]
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

/// Result of explicitly resolving an acquired cleanup entry. Restored does not
/// mean the original directory was removed; callers must preserve that distinction.
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum DirectoryRecovery {
    OriginalRemoved,
    OriginalRestored,
    ReplacementRestored,
}

/// Prepublication identity, retained even if later private staging cleanup fails.
///
/// Publication is not an fsync/durability claim. Callers must sync the relevant
/// directories and retain this receipt if subsequent fallible steps fail.
/// There is deliberately no automatic Drop cleanup of public or recovery names.
#[must_use = "retain the publication receipt through completion and cleanup errors"]
#[derive(Debug)]
pub struct DirectoryPublication {
    directory: Root,
    parent: Root,
    name: OsString,
    namespace: DirectoryNamespace,
    identity: Metadata,
    publication_error: Option<String>,
    private_staging: Option<(Root,OsString)>,
    quarantined: Option<OsString>,
    removed: bool,
}
impl DirectoryPublication {
    pub fn directory(&self) -> &Root { &self.directory }
    /// Captured before public publication; reading this cannot lose a known
    /// publication because a later fstat fails.
    pub fn identity(&self) -> (u64,u64) { (self.identity.dev(),self.identity.ino()) }
    pub fn initial_metadata(&self) -> &Metadata { &self.identity }
    pub fn publication_error(&self) -> Option<&str> { self.publication_error.as_deref() }
    /// Diagnostic opened spelling only; namespace moves can make it stale.
    /// The retained namespace and private leaf, not this path, are authority.
    pub fn pending_staging_path(&self)->Option<PathBuf> {
        self.private_staging.as_ref().map(|(_,name)|self.namespace.directory.path.join(name))
    }
    /// Diagnostic opened spelling of an unresolved acquired name. Namespace
    /// renames can make this stale; it is not a verified current absolute path
    /// or permission to delete its current entry.
    pub fn recovery_path(&self) -> Option<PathBuf> {
        self.quarantined.as_ref().map(|name| self.namespace.directory.path.join(name))
    }
    pub fn is_removed(&self) -> bool { self.removed }

    fn cleanup_private_staging(&mut self)->Result<(),ResolveError> {
        let Some((held,name))=&self.private_staging else {return Ok(());};
        namespace_protected(&self.namespace.directory).map_err(|e|ResolveError::io(&self.namespace.directory.path,&e))?;
        let named=resolve_directory(&self.namespace.directory,Path::new(name))?;
        let expected=held.metadata().map_err(|e|ResolveError::io(&held.path,&e))?;
        let actual=named.metadata().map_err(|e|ResolveError::io(&named.path,&e))?;
        if (expected.dev(),expected.ino())!=(actual.dev(),actual.ino()) {
            return Err(ResolveError::message(&named.path,"private staging name no longer identifies the retained directory; no replacement was removed"));
        }
        // Protected INTERNAL namespace bootstrap contract applies here; public
        // cleanup below uses acquire-before-check instead of this name check.
        remove_directory_within(&self.namespace.directory,name)?;
        self.private_staging=None;
        self.publication_error=None;
        Ok(())
    }

    /// Worker-only retry using the retained receipt, never a diagnostic path.
    /// Removes only the exact empty original; all other acquired entries are
    /// restored with no-clobber. A collision/error retains recovery ownership.
    /// OriginalRestored can still require user resolution of nonempty contents.
    pub fn resolve_recovery(&mut self)->Result<Option<DirectoryRecovery>,ResolveError> {
        self.cleanup_private_staging()?;
        let Some(name)=self.quarantined.clone() else {return Ok(None);};
        namespace_protected(&self.namespace.directory).map_err(|e|ResolveError::io(&self.namespace.directory.path,&e))?;
        let acquired=metadata_beneath(&self.namespace.directory,Path::new(&name))?;
        let original=(acquired.dev(),acquired.ino())==self.identity() && acquired.is_dir();
        if original && remove_directory_within(&self.namespace.directory,&name).is_ok() {
            self.quarantined=None;self.removed=true;
            return Ok(Some(DirectoryRecovery::OriginalRemoved));
        }
        rename_new_between(&self.namespace.directory,&name,&self.parent,&self.name)?;
        self.quarantined=None;
        Ok(Some(if original {DirectoryRecovery::OriginalRestored}else{DirectoryRecovery::ReplacementRestored}))
    }

    /// Acquire the public entry into the protected namespace before checking
    /// identity. A mismatched replacement is never chmod'ed or removed. If a
    /// no-clobber restoration is impossible, preserve it at recovery_path and
    /// report the failure. Keep this receipt/journal that path on error.
    pub fn cleanup_empty(&mut self) -> Result<(), ResolveError> {
        self.cleanup_empty_with_hooks(|| {},|| {})
    }
    fn cleanup_empty_with_hooks(&mut self, before_acquire: impl FnOnce(),after_acquire:impl FnOnce()) -> Result<(), ResolveError> {
        self.cleanup_private_staging()?;
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
        after_acquire();
        let acquired = resolve_directory(&self.namespace.directory, Path::new(&quarantine));
        let checked = acquired.and_then(|acquired| {
            let current = acquired.metadata().map_err(|e| ResolveError::io(&acquired.path,&e))?;
            if self.identity() != (current.dev(),current.ino()) {
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
/// publication_error; recovery may traverse Existing/Unverified without chmod.
/// Public payload permissions obey mkdir umask. Exact chmod, when required,
/// is a separate caller action on the Published receipt's retained Root.
pub fn create_directory_beneath(root: &Root, relative: &Path, mode: u32) -> Result<DirectoryCreation,ResolveError> {
    if let Some(existing)=existing_directory(root,relative)? {return Ok(DirectoryCreation::Existing(existing));}
    let (parent,name)=open_parent_beneath(root,relative)?;
    match DirectoryNamespace::acquire(&parent) {
        Ok(namespace) => create_directory_in_namespace(&namespace,&parent,Path::new(&name),mode),
        Err(reason) => create_unverified(&parent,&name,mode,reason.to_string()),
    }
}
fn create_unverified(parent:&Root,name:&OsStr,mode:u32,reason:String)->Result<DirectoryCreation,ResolveError> {
    let leaf=c_component(name,&parent.path)?;
    // No creation authority is inferred from mkdir followed by name resolution.
    let status=unsafe {libc::mkdirat(parent.descriptor(),leaf.as_ptr(),mode)};
    if status<0 {
        let error=io::Error::last_os_error();
        if error.kind()!=io::ErrorKind::AlreadyExists {return Err(ResolveError::io(&parent.path.join(name),&error));}
        return resolve_directory(parent,Path::new(name)).map(DirectoryCreation::Existing);
    }
    let directory=resolve_directory(parent,Path::new(name))?;
    Ok(DirectoryCreation::Unverified {directory,reason})
}

/// Publish a directory captured in an explicitly retained protected namespace.
/// The namespace must be the exact retained actual parent: unrelated staging
/// cannot reproduce every native directory inheritance policy. Different
/// parents create with ordinary mkdir semantics and return Unverified, never
/// a falsely owned receipt. Callers may acquire the actual protected parent.
pub fn create_directory_in_namespace(namespace:&DirectoryNamespace,root:&Root,relative:&Path,mode:u32)->Result<DirectoryCreation,ResolveError> {
    create_with_hooks(namespace,root,relative,mode,|_,_|{},|_,_|{})
}
fn create_with_hooks(namespace:&DirectoryNamespace,root:&Root,relative:&Path,mode:u32,
    after_capture:impl FnOnce(&Root,&OsStr),after_publish:impl FnOnce(&Root,&OsStr))->Result<DirectoryCreation,ResolveError> {
    // Borrowing an existing target never requires staging write permission.
    if let Some(existing)=existing_directory(root,relative)? {return Ok(DirectoryCreation::Existing(existing));}
    let (parent,name)=open_parent_beneath(root,relative)?;
    let parent_metadata=parent.metadata().map_err(|e|ResolveError::io(&parent.path,&e))?;
    let namespace_metadata=namespace.directory.metadata().map_err(|e|ResolveError::io(&namespace.directory.path,&e))?;
    if (parent_metadata.dev(),parent_metadata.ino())!=(namespace_metadata.dev(),namespace_metadata.ino()) {
        return create_unverified(&parent,&name,mode,
            "owned directory publication requires the exact protected destination parent; different-parent inheritance remains unverified".into());
    }
    namespace_protected(&namespace.directory).map_err(|e|ResolveError::io(&namespace.directory.path,&e))?;
    let retained_namespace=DirectoryNamespace {directory:namespace.directory.try_clone()
        .map_err(|e|ResolveError::io(&namespace.directory.path,&e))?};
    let stage=unused_name(&namespace.directory)?;
    let leaf=c_component(&stage,&namespace.directory.path)?;
    // Private OUTER staging never grants other-UID access. The payload inside
    // it uses the requested mode and the actual process umask, exactly as mkdir.
    if unsafe {libc::mkdirat(namespace.directory.descriptor(),leaf.as_ptr(),0o700)} !=0 {
        return Err(ResolveError::io(&namespace.directory.path.join(&stage),&io::Error::last_os_error()));
    }
    let prepare=(|| {
        let outer=resolve_directory(&namespace.directory,Path::new(&stage))?;
        // Repair only this captured internal boundary, never public payload
        // permissions. O_PATH/O_EVTONLY acquisition is separately qualified.
        let inherited=outer.metadata().map_err(|e|ResolveError::io(&outer.path,&e))?;
        set_directory_permissions(&outer,0o700|(inherited.mode()&0o2000)).map_err(|e|ResolveError::io(&outer.path,&e))?;
        namespace_protected(&outer).map_err(|e|ResolveError::io(&outer.path,&e))?;
        let repaired=outer.metadata().map_err(|e|ResolveError::io(&outer.path,&e))?;
        if repaired.gid()!=parent_metadata.gid() || repaired.mode()&0o2000!=parent_metadata.mode()&0o2000 {
            return Ok((outer,None));
        }
        if unsafe {libc::mkdirat(outer.descriptor(),c"directory".as_ptr(),mode)}!=0 {
            return Err(ResolveError::io(&outer.path.join("directory"),&io::Error::last_os_error()));
        }
        let payload=resolve_directory(&outer,Path::new("directory"))?;
        let identity=payload.metadata().map_err(|e|ResolveError::io(&payload.path,&e))?;
        Ok((outer,Some((payload,identity))))
    })();
    let (outer,payload)=match prepare {
        Ok(result)=>result,
        Err(error)=>{
            // Only private internal entries are addressed here. The payload
            // can exist if its descriptor acquisition failed after mkdir.
            if let Ok(outer)=resolve_directory(&namespace.directory,Path::new(&stage)) {
                let _=remove_directory_within(&outer,OsStr::new("directory"));
            }
            let cleanup=remove_directory_within(&namespace.directory,&stage);
            return Err(match cleanup {Ok(())=>error,Err(cleanup)=>ResolveError::message(
                &namespace.directory.path.join(&stage),&format!("{error}; private staging cleanup failed: {cleanup}"))});
        }
    };
    let Some((mut directory,identity))=payload else {
        remove_directory_within(&namespace.directory,&stage)?;
        return create_unverified(&parent,&name,mode,
            "internal owner-access repair could not preserve destination gid/SGID; creation ownership is unverified".into());
    };
    after_capture(&parent,&name);
    if let Err(error)=rename_exclusive(&outer,OsStr::new("directory"),&parent,&name) {
        let payload_cleanup=remove_directory_within(&outer,OsStr::new("directory"));
        let outer_cleanup=remove_directory_within(&namespace.directory,&stage);
        if let Err(cleanup)=payload_cleanup.and(outer_cleanup) {return Err(ResolveError::message(&namespace.directory.path.join(&stage),&format!("{error}; private staging cleanup failed: {cleanup}")));}
        if error.kind()==io::ErrorKind::AlreadyExists {
            return resolve_directory(&parent,Path::new(&name)).map(DirectoryCreation::Existing);
        }
        return Err(ResolveError::io(&parent.path.join(&name),&error));
    }
    directory.path=parent.path.join(&name);
    after_publish(&parent,&name);
    // Nothing after successful publication can erase the known identity from
    // the returned receipt. Cleanup failure retains the private staging root.
    let cleanup=remove_directory_within(&namespace.directory,&stage);
    let (publication_error,private_staging)=match cleanup {
        Ok(())=>(None,None),
        Err(error)=>(Some(error.to_string()),Some((outer,stage))),
    };
    Ok(DirectoryCreation::Published(DirectoryPublication {
        directory,parent,name,namespace:retained_namespace,identity,publication_error,private_staging,
        quarantined:None,removed:false,
    }))
}
fn existing_directory(root:&Root,relative:&Path)->Result<Option<Root>,ResolveError> {
    match resolve_directory(root,relative) {
        Ok(existing)=>Ok(Some(existing)),
        Err(error) if error.is_not_found()=>Ok(None),
        Err(error)=>Err(error),
    }
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
    let raw=unsafe {filesec_init()};
    if raw.is_null(){return Err(io::Error::last_os_error());}
    let security=Security(raw);
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
    use std::{fs,os::unix::fs::{PermissionsExt,DirBuilderExt}};
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
        assert!(receipt.publication_error().is_none());
        assert_eq!(receipt.directory().metadata().unwrap().ino(),fs::metadata(dir.path().join("retained")).unwrap().ino());
        assert_eq!(fs::metadata(dir.path().join("retained")).unwrap().mode()&0o777,receipt.initial_metadata().mode()&0o777);
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
        assert!(receipt.cleanup_empty_with_hooks(||{
            fs::rename(dir.path().join("target"),dir.path().join("original")).unwrap();
            fs::create_dir(dir.path().join("target")).unwrap();
        },||{}).is_err());
        assert!(dir.path().join("target").is_dir());
        assert!(dir.path().join("original").is_dir());
    }
    #[test]
    fn different_parent_preserves_native_creation_as_unverified() {
        let dir=tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("shared")).unwrap();
        fs::set_permissions(dir.path().join("shared"),fs::Permissions::from_mode(0o2777)).unwrap();
        let root=Root::open(dir.path()).unwrap();
        let namespace=DirectoryNamespace::acquire(&root).unwrap();
        let shared=open_directory_beneath(&root,Path::new("shared")).unwrap();
        let result=create_directory_in_namespace(&namespace,&shared,Path::new("target"),0o750).unwrap();
        assert!(matches!(result,DirectoryCreation::Unverified{..}));
        fs::DirBuilder::new().mode(0o750).create(dir.path().join("shared/native-reference")).unwrap();
        let actual=fs::metadata(dir.path().join("shared/target")).unwrap();
        let reference=fs::metadata(dir.path().join("shared/native-reference")).unwrap();
        assert_eq!(actual.gid(),reference.gid());
        assert_eq!(actual.mode()&0o7777,reference.mode()&0o7777);
    }
    #[test]
    fn same_parent_published_payload_preserves_native_gid_and_setgid() {
        let dir=tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(),fs::Permissions::from_mode(0o2755)).unwrap();
        let root=Root::open(dir.path()).unwrap();
        if root.metadata().unwrap().mode()&0o2000==0 {
            eprintln!("SGID fixture unavailable: kernel cleared requested flag (group membership/platform policy)");return;
        }
        fs::create_dir(dir.path().join("native-reference")).unwrap();
        let receipt=published(create_directory_beneath(&root,Path::new("target"),0o777).unwrap());
        let reference=fs::metadata(dir.path().join("native-reference")).unwrap();
        let actual=receipt.directory().metadata().unwrap();
        assert_eq!(actual.gid(),reference.gid());
        assert_eq!(actual.mode()&0o7777,reference.mode()&0o7777);
    }
    #[cfg(target_os="linux")]
    #[test]
    fn same_parent_published_payload_preserves_native_default_acl() {
        let dir=tempfile::tempdir().unwrap();
        let cpath=CString::new(dir.path().as_os_str().as_bytes()).unwrap();
        // Linux POSIX ACL xattr v2, independently seeded through the path API:
        // user_obj=rwx, group_obj=r-x, other=---. Not a production writer seam.
        let mut acl=2u32.to_le_bytes().to_vec();
        for (tag,permissions) in [(1u16,7u16),(4,5),(32,0)] {
            acl.extend(tag.to_le_bytes());acl.extend(permissions.to_le_bytes());
            acl.extend(u32::MAX.to_le_bytes());
        }
        if unsafe {libc::setxattr(cpath.as_ptr(),c"system.posix_acl_default".as_ptr(),acl.as_ptr().cast(),acl.len(),0)}!=0 {
            let error=io::Error::last_os_error();
            if error.raw_os_error()==Some(libc::EOPNOTSUPP) {
                eprintln!("default ACL fixture unavailable on this filesystem: {error}");return;
            }
            panic!("seed independent native default ACL: {error}");
        }
        fn observed(path:&Path)->Vec<u8> {
            let path=CString::new(path.as_os_str().as_bytes()).unwrap();
            let mut bytes=vec![0;65536];
            let size=unsafe {libc::getxattr(path.as_ptr(),c"system.posix_acl_default".as_ptr(),bytes.as_mut_ptr().cast(),bytes.len())};
            assert!(size>0,"fixture must retain an actual default ACL: {}",io::Error::last_os_error());
            bytes.truncate(size as usize);bytes
        }
        assert_eq!(observed(dir.path()),acl);
        fs::create_dir(dir.path().join("native-reference")).unwrap();
        let root=Root::open(dir.path()).unwrap();
        let receipt=published(create_directory_beneath(&root,Path::new("target"),0o777).unwrap());
        let reference=fs::metadata(dir.path().join("native-reference")).unwrap();
        let actual=receipt.directory().metadata().unwrap();
        assert_eq!(actual.gid(),reference.gid());
        assert_eq!(actual.mode()&0o7777,reference.mode()&0o7777);
        assert_eq!(observed(&dir.path().join("target")),observed(&dir.path().join("native-reference")));
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
    fn existing_target_in_read_only_root_needs_no_staging_write() {
        let dir=tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("existing")).unwrap();
        let root=Root::open(dir.path()).unwrap();
        fs::set_permissions(dir.path(),fs::Permissions::from_mode(0o555)).unwrap();
        let result=create_directory_beneath(&root,Path::new("existing"),0o777);
        fs::set_permissions(dir.path(),fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(result.unwrap(),DirectoryCreation::Existing(_)));
    }
    #[test]
    fn existing_target_does_not_need_explicit_namespace_write() {
        let namespace_dir=tempfile::tempdir().unwrap();
        let parent_dir=tempfile::tempdir().unwrap();
        fs::create_dir(parent_dir.path().join("existing")).unwrap();
        let namespace_root=Root::open(namespace_dir.path()).unwrap();
        let namespace=DirectoryNamespace::acquire(&namespace_root).unwrap();
        let parent=Root::open(parent_dir.path()).unwrap();
        fs::set_permissions(namespace_dir.path(),fs::Permissions::from_mode(0o555)).unwrap();
        let result=create_directory_in_namespace(&namespace,&parent,Path::new("existing"),0o777);
        fs::set_permissions(namespace_dir.path(),fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(result.unwrap(),DirectoryCreation::Existing(_)));
    }
    #[test]
    fn cleanup_restores_nonempty_original_without_removing_contents() {
        let dir=tempfile::tempdir().unwrap();
        let root=Root::open(dir.path()).unwrap();
        let mut receipt=published(create_directory_beneath(&root,Path::new("target"),0o700).unwrap());
        fs::write(dir.path().join("target/keep"),b"original").unwrap();
        assert!(receipt.cleanup_empty().is_err());
        assert!(receipt.recovery_path().is_none());
        assert_eq!(fs::read(dir.path().join("target/keep")).unwrap(),b"original");
        assert!(!receipt.is_removed());
    }
    #[test]
    fn cleanup_restore_collision_preserves_quarantined_replacement() {
        let dir=tempfile::tempdir().unwrap();
        let root=Root::open(dir.path()).unwrap();
        let mut receipt=published(create_directory_beneath(&root,Path::new("target"),0o700).unwrap());
        fs::rename(dir.path().join("target"),dir.path().join("original")).unwrap();
        fs::create_dir(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target/foreign"),b"foreign").unwrap();
        let result=receipt.cleanup_empty_with_hooks(||{},||{
            fs::create_dir(dir.path().join("target")).unwrap();
            fs::write(dir.path().join("target/newer"),b"newer").unwrap();
        });
        assert!(result.is_err());
        assert_eq!(fs::read(receipt.recovery_path().unwrap().join("foreign")).unwrap(),b"foreign");
        assert_eq!(fs::read(dir.path().join("target/newer")).unwrap(),b"newer");
        assert!(dir.path().join("original").is_dir());
        assert!(receipt.cleanup_empty().is_err(),"unresolved acquired entry cannot be replayed");
        assert!(receipt.resolve_recovery().is_err(),"collision remains authoritative");
        fs::remove_file(dir.path().join("target/newer")).unwrap();
        fs::remove_dir(dir.path().join("target")).unwrap();
        assert_eq!(receipt.resolve_recovery().unwrap(),Some(DirectoryRecovery::ReplacementRestored));
        assert!(receipt.recovery_path().is_none());
        assert_eq!(fs::read(dir.path().join("target/foreign")).unwrap(),b"foreign");
        assert!(!receipt.is_removed(),"restoring a replacement never claims original cleanup");
    }
    #[test]
    fn retained_nonempty_original_can_restore_then_retry_after_contents_resolved() {
        let dir=tempfile::tempdir().unwrap();
        let root=Root::open(dir.path()).unwrap();
        let mut receipt=published(create_directory_beneath(&root,Path::new("target"),0o700).unwrap());
        fs::write(dir.path().join("target/keep"),b"owned").unwrap();
        assert!(receipt.cleanup_empty_with_hooks(||{},||{
            fs::create_dir(dir.path().join("target")).unwrap();
        }).is_err());
        fs::remove_dir(dir.path().join("target")).unwrap();
        assert_eq!(receipt.resolve_recovery().unwrap(),Some(DirectoryRecovery::OriginalRestored));
        assert_eq!(fs::read(dir.path().join("target/keep")).unwrap(),b"owned");
        assert!(!receipt.is_removed());
        fs::remove_file(dir.path().join("target/keep")).unwrap();
        receipt.cleanup_empty().unwrap();
        assert!(receipt.is_removed());
    }
    #[test]
    fn postpublication_cleanup_error_retains_cached_identity_and_stage() {
        if unsafe {libc::geteuid()}==0 {return;} // EACCES fixture requires an unprivileged caller.
        let dir=tempfile::tempdir().unwrap();
        let root=Root::open(dir.path()).unwrap();
        let namespace=DirectoryNamespace::acquire(&root).unwrap();
        let result=create_with_hooks(&namespace,&root,Path::new("target"),0o700,|_,_|{},|_,_|{
            fs::set_permissions(dir.path(),fs::Permissions::from_mode(0o555)).unwrap();
        });
        fs::set_permissions(dir.path(),fs::Permissions::from_mode(0o700)).unwrap();
        let mut receipt=published(result.unwrap());
        assert!(receipt.publication_error().is_some());
        assert!(receipt.pending_staging_path().unwrap().is_dir());
        let actual=fs::metadata(dir.path().join("target")).unwrap();
        assert_eq!(receipt.identity(),(actual.dev(),actual.ino()));
        assert_eq!(receipt.initial_metadata().ino(),actual.ino());
        receipt.cleanup_empty().unwrap();
        assert!(receipt.is_removed());
        assert!(receipt.pending_staging_path().is_none());
        assert!(receipt.publication_error().is_none());
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
        assert!(receipt.publication_error().is_none());
        assert_eq!(receipt.directory().metadata().unwrap().mode()&0o777,0);
        set_directory_permissions(receipt.directory(),0o750).unwrap();
        assert_eq!(receipt.directory().metadata().unwrap().mode()&0o777,0o750);
    }
}
