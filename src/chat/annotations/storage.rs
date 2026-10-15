//! Small durable command/decision authority, separate from ephemeral body caches.
use super::*;
use crate::account::scope::LaunchAccountScope;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::{ffi::OsStrExt, fs::OpenOptionsExt}, path::{Path, PathBuf}};

const CHUNK: usize = 128 * 1024;
const ID_BYTES: usize = 4096;
const META_BYTES: usize = 64 * 1024;

pub(crate) struct Store {
    db: Connection,
    scope: LaunchAccountScope,
    session:Option<String>,
    directory: PathBuf,
    pending_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
}
/// Owns an unaccepted attachment. Dropping/aborting removes its private bytes;
/// the file lock protects active captures from cold orphan recovery.
pub(crate) struct PendingAttachment {
    pub attachment: OriginalAttachment,
    _lease: fs::File,
    accepted: bool,
    reserved_bytes:u64,
    pending_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
}
impl Drop for PendingAttachment {
    fn drop(&mut self) {
        if !self.accepted {let _=fs::remove_file(&self.attachment.path);}
        self.pending_bytes.fetch_sub(self.reserved_bytes,std::sync::atomic::Ordering::AcqRel);
    }
}
fn valid_id(id: &str) -> bool { !id.is_empty() && id.len() <= ID_BYTES }
fn account_key(scope: &LaunchAccountScope) -> String {
    let mut hash = Sha256::new();
    hash.update(postcard::to_stdvec(&scope.provider).expect("provider serialization"));
    hash.update((scope.canonical_home.as_os_str().as_bytes().len() as u64).to_le_bytes());
    hash.update(scope.canonical_home.as_os_str().as_bytes());
    hash.update(scope.device.to_le_bytes());
    hash.update(scope.inode.to_le_bytes());
    format!("{:x}", hash.finalize())
}
fn installation_root() -> Result<PathBuf, String> {
    crate::instance::project_dirs().map(|dirs| dirs.data_dir().join("conversation-annotations"))
        .ok_or_else(|| "Cannot locate private conversation annotations".into())
}
/// Invoke while the lifecycle lock still protects the captured existing home.
/// Archive and cache eviction deliberately do not call this function.
pub(crate) fn purge_account(scope: &LaunchAccountScope) -> Result<(), String> {
    purge_account_at(&installation_root()?,scope)
}
pub(crate) fn purge_session(scope:&LaunchAccountScope,confirmed_session:&str)->Result<(),String>{
    let root=installation_root()?;
    scope.revalidate()?;
    if !valid_id(confirmed_session){return Err("Invalid confirmed conversation identity".into());}
    if !root.join(account_key(scope)).exists(){return Ok(());}
    Store::at(&root,scope,confirmed_session)?.purge_current_session()
}
fn purge_account_at(root: &Path, scope: &LaunchAccountScope) -> Result<(), String> {
    scope.revalidate()?;
    let path=root.join(account_key(scope));
    crate::secure_file::reject_symlink_components(&path).map_err(|e|e.to_string())?;
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind()==std::io::ErrorKind::NotFound => Ok(()),
        Err(e)=>Err(e.to_string()),
        Ok(_) => {
            crate::secure_file::validate_owned_directory(&path).map_err(|e|e.to_string())?;
            fs::remove_dir_all(path).map_err(|e|e.to_string())
        }
    }
}
/// Purge-only historical association. `None` means this installation has no
/// trustworthy prior association for the registered ID; it grants no read
/// access and must not be used to reconstruct an absent credential home.
pub(crate) fn purge_registered(id:&crate::account::AccountId)->Result<Option<usize>,String>{
    purge_registered_at(&installation_root()?,id)
}
fn purge_registered_at(root:&Path,id:&crate::account::AccountId)->Result<Option<usize>,String>{
    let directory=root.join("purge-index").join(format!("{:x}",Sha256::digest(id.as_str().as_bytes())));
    crate::secure_file::reject_symlink_components(&directory).map_err(|e|e.to_string())?;
    let entries=match fs::read_dir(&directory){Ok(entries)=>entries,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(None),Err(e)=>return Err(e.to_string())};
    crate::secure_file::validate_owned_directory(&directory).map_err(|e|e.to_string())?;
    let mut removed=0;
    for entry in entries {
        let entry=entry.map_err(|e|e.to_string())?;let name=entry.file_name();let name=name.to_str().ok_or("Invalid annotation purge association")?;
        if name.len()!=64||!name.bytes().all(|b|b.is_ascii_hexdigit()){return Err("Invalid annotation purge association".into());}
        crate::secure_file::open_existing_private_file(&entry.path()).map_err(|e|e.to_string())?;
        let path=root.join(name);crate::secure_file::reject_symlink_components(&path).map_err(|e|e.to_string())?;
        match fs::symlink_metadata(&path){Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.to_string()),Ok(_)=>{
            crate::secure_file::validate_owned_directory(&path).map_err(|e|e.to_string())?;fs::remove_dir_all(path).map_err(|e|e.to_string())?;removed+=1;
        }}
    }
    Ok(Some(removed))
}
fn associate_for_purge(root:&Path,scope:&LaunchAccountScope)->Result<(),String>{
    let Some(id)=&scope.registered_id else{return Ok(());};
    let directory=root.join("purge-index").join(format!("{:x}",Sha256::digest(id.as_str().as_bytes())));
    crate::secure_file::prepare_private_directory(&directory).map_err(|e|e.to_string())?;
    let path=directory.join(account_key(scope));
    let file=fs::OpenOptions::new().write(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(&path).map_err(|e|e.to_string())?;
    crate::secure_file::validate_and_privatize_open_file(&path,&file).map_err(|e|e.to_string())?;
    file.sync_all().map_err(|e|e.to_string())?;
    fs::File::open(directory).and_then(|file|file.sync_all()).map_err(|e|e.to_string())
}
impl Store {
    /// `session` must come from adapter confirmation, never a requested resume
    /// recipe or a later UI selection. Callers retain that provenance.
    pub(crate) fn installation(scope: &LaunchAccountScope, session: &str) -> Result<Self,String> {
        Self::at(&installation_root()?,scope,session)
    }
    /// Pending command bytes have physical account authority, but deliberately
    /// have no native-session authority to accept or read original records.
    pub(crate) fn pending_installation(scope:&LaunchAccountScope)->Result<Self,String>{Self::open(&installation_root()?,scope,None)}
    #[cfg(test)]
    pub(crate) fn for_test(root:&Path,scope:&LaunchAccountScope,session:&str)->Result<Self,String>{Self::at(root,scope,session)}
    fn at(root:&Path,scope:&LaunchAccountScope,session:&str)->Result<Self,String>{Self::open(root,scope,Some(session))}
    fn confirmed(&self)->Result<&str,String>{self.session.as_deref().ok_or_else(||"Pending command storage has no confirmed conversation authority".into())}
    fn open(root:&Path,scope:&LaunchAccountScope,session:Option<&str>)->Result<Self,String> {
        scope.revalidate()?;
        if session.is_some_and(|id|!valid_id(id)) {return Err("Invalid confirmed conversation identity".into());}
        associate_for_purge(root,scope)?;
        let account=root.join(account_key(scope));
        crate::secure_file::prepare_private_directory(&account).map_err(|e|e.to_string())?;
        let directory=account;
        let path=directory.join("annotations.sqlite");
        let file=fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(&path).map_err(|e|e.to_string())?;
        crate::secure_file::validate_and_privatize_open_file(&path,&file).map_err(|e|e.to_string())?;
        let db=Connection::open_with_flags(&path,OpenFlags::SQLITE_OPEN_READ_WRITE|OpenFlags::SQLITE_OPEN_NO_MUTEX|OpenFlags::SQLITE_OPEN_NOFOLLOW).map_err(|e|e.to_string())?;
        db.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e|e.to_string())?;
        let version:u32=db.pragma_query_value(None,"user_version",|row|row.get(0)).map_err(|e|e.to_string())?;
        if version>1{return Err(format!("Conversation annotation schema {version} is newer than supported schema 1"));}
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-512; PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON;
          CREATE TABLE IF NOT EXISTS prompts(session TEXT NOT NULL,user_id TEXT NOT NULL,metadata BLOB NOT NULL,digest BLOB NOT NULL,PRIMARY KEY(session,user_id));
          CREATE TABLE IF NOT EXISTS prompt_text(session TEXT NOT NULL,user_id TEXT NOT NULL,position INTEGER NOT NULL,body BLOB NOT NULL,PRIMARY KEY(session,user_id,position),FOREIGN KEY(session,user_id) REFERENCES prompts(session,user_id) ON DELETE CASCADE);
          CREATE TABLE IF NOT EXISTS aliases(session TEXT NOT NULL,alias TEXT NOT NULL,user_id TEXT NOT NULL,PRIMARY KEY(session,alias),FOREIGN KEY(session,user_id) REFERENCES prompts(session,user_id) ON DELETE CASCADE);
          CREATE TABLE IF NOT EXISTS approvals(session TEXT NOT NULL,agent TEXT NOT NULL,item TEXT NOT NULL,request TEXT NOT NULL,decision INTEGER NOT NULL,PRIMARY KEY(session,agent,item,request));
          CREATE TABLE IF NOT EXISTS current_approval(session TEXT NOT NULL,agent TEXT NOT NULL,item TEXT NOT NULL,request TEXT NOT NULL,PRIMARY KEY(session,agent,item));
          CREATE TABLE IF NOT EXISTS retained_attachments(path TEXT PRIMARY KEY,accepted INTEGER NOT NULL DEFAULT 0,bytes INTEGER NOT NULL,session TEXT);
          CREATE INDEX IF NOT EXISTS pending_attachments ON retained_attachments(accepted) WHERE accepted=0;
          CREATE INDEX IF NOT EXISTS session_attachments ON retained_attachments(session) WHERE session IS NOT NULL;
          CREATE TABLE IF NOT EXISTS latest(session TEXT PRIMARY KEY,user_id TEXT NOT NULL); PRAGMA user_version=1;")
            .map_err(|e|e.to_string())?;
        let store=Self{db,scope:scope.clone(),session:session.map(str::to_owned),directory,pending_bytes:Default::default()};
        store.recover_pending()?;
        Ok(store)
    }
    fn purge_current_session(&mut self)->Result<(),String>{
        self.scope.revalidate()?;let session=self.confirmed()?.to_owned();
        let transaction=self.db.transaction().map_err(|e|e.to_string())?;
        transaction.execute("DELETE FROM prompts WHERE session=?1",[&session]).map_err(|e|e.to_string())?;
        for table in ["approvals","current_approval","latest"]{transaction.execute(&format!("DELETE FROM {table} WHERE session=?1"),[&session]).map_err(|e|e.to_string())?;}
        transaction.execute("UPDATE retained_attachments SET accepted=0,session=NULL WHERE session=?1",[&session]).map_err(|e|e.to_string())?;
        transaction.commit().map_err(|e|e.to_string())?;
        self.recover_pending()
    }
    /// Sweep only pending rows, never accepted originals or the transcript.
    /// Locks make recovery safe against another actor's active capture.
    fn recover_pending(&self)->Result<(),String>{
        use std::os::fd::AsRawFd;
        let mut after=String::new();
        loop {
            let _publication=crate::instance::owned_lock(&self.directory.join("capture-publication.lock"),libc::LOCK_EX).map_err(|e|e.to_string())?;
            let transaction=rusqlite::Transaction::new_unchecked(&self.db,rusqlite::TransactionBehavior::Immediate).map_err(|e|e.to_string())?;
            let mut statement=transaction.prepare("SELECT path FROM retained_attachments WHERE accepted=0 AND path>?1 ORDER BY path LIMIT 128").map_err(|e|e.to_string())?;
            let paths=statement.query_map([&after],|row|row.get::<_,String>(0)).map_err(|e|e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
            drop(statement);
            if paths.is_empty(){break;}
            after=paths.last().unwrap().clone();
            for name in paths {
                if !name.starts_with("capture-")||Path::new(&name).components().count()!=1{return Err("Invalid pending attachment identity".into());}
                let path=self.directory.join(&name);
                match crate::secure_file::open_existing_private_file(&path){
                    Ok(file)=>{
                        // SAFETY: file is a live, validated regular descriptor.
                        if unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0 {continue;}
                        if let Err(error)=fs::remove_file(&path) && error.kind()!=std::io::ErrorKind::NotFound{return Err(error.to_string());}
                    },
                    Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{},
                    Err(error)=>return Err(error.to_string()),
                }
                transaction.execute("DELETE FROM retained_attachments WHERE path=?1 AND accepted=0",[name]).map_err(|e|e.to_string())?;
            }
            transaction.commit().map_err(|e|e.to_string())?;
        }
        Ok(())
    }
    /// Snapshot BEFORE dispatch and send this owned private path to the
    /// provider. The capture is retained only by a successful accept().
    pub(crate) fn retain_attachment(&self,path:&Path)->Result<PendingAttachment,OriginalError>{
        use std::{io::{Read,Write},os::fd::AsRawFd,sync::atomic::Ordering};
        self.scope.revalidate().map_err(OriginalError::Unavailable)?;
        self.recover_pending().map_err(OriginalError::Unavailable)?;
        let mut input=fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).map_err(|e|OriginalError::Unavailable(e.to_string()))?;
        let metadata=input.metadata().map_err(|e|OriginalError::Unavailable(e.to_string()))?;
        if !metadata.is_file(){return Err(OriginalError::Unavailable("The original attachment is not a regular file".into()));}
        let reserved_bytes=metadata.len();if reserved_bytes>32*1024*1024{return Err(OriginalError::TooLarge);}
        self.pending_bytes.fetch_update(Ordering::AcqRel,Ordering::Acquire,|bytes|bytes.checked_add(reserved_bytes).filter(|total|*total<=64*1024*1024)).map_err(|_|OriginalError::TooLarge)?;
        let extension=path.extension().and_then(|value|value.to_str()).filter(|value|!value.is_empty()&&value.len()<=16&&value.bytes().all(|byte|byte.is_ascii_alphanumeric()));
        let name=format!("capture-{}{}",uuid::Uuid::new_v4(),extension.map(|value|format!(".{value}")).unwrap_or_default());
        let retained=self.directory.join(&name);
        let mut created=false;
        let result=(||{
            // Publish the durable pending row first. Recovery holds the same
            // publication lock, so it cannot remove that row before the file
            // exists and carries its own lifetime lease. Death at either cut
            // point leaves an indexed, recoverable pending capture.
            let publication=crate::instance::owned_lock(&self.directory.join("capture-publication.lock"),libc::LOCK_EX).map_err(|e|OriginalError::Unavailable(e.to_string()))?;
            if self.db.execute("INSERT INTO retained_attachments(path,bytes) SELECT ?1,?2 WHERE (SELECT count(*) FROM retained_attachments WHERE accepted=0)<1024 AND (SELECT coalesce(sum(bytes),0) FROM retained_attachments WHERE accepted=0)+?2<=268435456",params![name,reserved_bytes]).map_err(storage_error)?!=1{return Err(OriginalError::TooLarge);}
            let mut output=fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&retained).map_err(|e|OriginalError::Unavailable(e.to_string()))?;
            created=true;
            // SAFETY: output owns this newly created regular file.
            if unsafe{libc::flock(output.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0{return Err(OriginalError::Unavailable(std::io::Error::last_os_error().to_string()));}
            drop(publication);
            let mut hash=Sha256::new();let mut bytes=0u64;let mut chunk=[0u8;64*1024];
            loop {let count=input.read(&mut chunk).map_err(|e|OriginalError::Unavailable(e.to_string()))?;if count==0{break;}bytes=bytes.saturating_add(count as u64);if bytes>reserved_bytes{return Err(OriginalError::Changed);}hash.update(&chunk[..count]);output.write_all(&chunk[..count]).map_err(|e|OriginalError::Unavailable(e.to_string()))?;}
            if bytes!=reserved_bytes{return Err(OriginalError::Changed);}
            output.sync_all().map_err(|e|OriginalError::Unavailable(e.to_string()))?;
            fs::File::open(&self.directory).and_then(|file|file.sync_all()).map_err(|e|OriginalError::Unavailable(e.to_string()))?;
            Ok(PendingAttachment{attachment:OriginalAttachment{original_path:path.to_owned(),path:retained.clone(),bytes,sha256:hash.finalize().into()},_lease:output,accepted:false,reserved_bytes,pending_bytes:self.pending_bytes.clone()})
        })();
        if result.is_err(){if created{let _=fs::remove_file(retained);}self.pending_bytes.fetch_sub(reserved_bytes,Ordering::AcqRel);}
        result
    }
    /// Persist only after a provider acknowledgment binds this exact original
    /// command to its real user item. Conflicting rewrites are refused.
    pub(crate) fn accept(&mut self, original:&OriginalPrompt,text:&str,aliases:&[String],captures:&mut [PendingAttachment])->Result<(),OriginalError> {
        self.scope.revalidate().map_err(OriginalError::Unavailable)?;
        let session=self.confirmed().map_err(OriginalError::Unavailable)?.to_owned();
        if !valid_id(&original.user_id)||!valid_id(&original.turn_id)||aliases.len()>128||aliases.iter().any(|id|!valid_id(id)) {return Err(OriginalError::TooLarge);}
        if !original.settings.is_exact() {return Err(OriginalError::Unavailable("The accepted provider settings are unknown".into()));}
        if text.len()!=original.text_bytes||text.len()>ORIGINAL_TEXT_BYTES||original.attachments.len()>32||original.attachments.iter().try_fold(0u64,|sum,file|sum.checked_add(file.bytes)).is_none_or(|bytes|bytes>64*1024*1024) {return Err(OriginalError::TooLarge);}
        if original.attachments.len()!=captures.len()||original.attachments.iter().zip(captures.iter()).any(|(expected,capture)|expected!=&capture.attachment||capture.attachment.path.parent()!=Some(self.directory.as_path())){return Err(OriginalError::Changed);}
        let metadata=postcard::to_stdvec(original).map_err(|e|OriginalError::Unavailable(e.to_string()))?;
        if metadata.len()>META_BYTES{return Err(OriginalError::TooLarge);}
        let mut hash=Sha256::new();hash.update(&metadata);hash.update(text.as_bytes());let digest=hash.finalize();
        let transaction=self.db.transaction().map_err(storage_error)?;
        let old:Option<Vec<u8>>=transaction.query_row("SELECT digest FROM prompts WHERE session=?1 AND user_id=?2",params![session,original.user_id],|r|r.get(0)).optional().map_err(storage_error)?;
        if old.as_deref().is_some_and(|old|old!=digest.as_slice()){return Err(OriginalError::Changed);}
        if old.is_none(){
            transaction.execute("INSERT INTO prompts VALUES(?1,?2,?3,?4)",params![session,original.user_id,metadata,digest.as_slice()]).map_err(storage_error)?;
            for (index,chunk) in text.as_bytes().chunks(CHUNK).enumerate(){transaction.execute("INSERT INTO prompt_text VALUES(?1,?2,?3,?4)",params![session,original.user_id,index*CHUNK,chunk]).map_err(storage_error)?;}
        }
        for alias in std::iter::once(&original.user_id).chain(aliases){
            let existing:Option<String>=transaction.query_row("SELECT user_id FROM aliases WHERE session=?1 AND alias=?2",params![session,alias],|r|r.get(0)).optional().map_err(storage_error)?;
            if existing.as_deref().is_some_and(|id|id!=original.user_id){return Err(OriginalError::Changed);}
            transaction.execute("INSERT OR IGNORE INTO aliases VALUES(?1,?2,?3)",params![session,alias,original.user_id]).map_err(storage_error)?;
        }
        for capture in captures.iter(){
            let name=capture.attachment.path.file_name().and_then(|name|name.to_str()).ok_or(OriginalError::Changed)?;
            if transaction.execute("UPDATE retained_attachments SET accepted=1,session=?2 WHERE path=?1 AND (session IS NULL OR session=?2)",params![name,session]).map_err(storage_error)?!=1{return Err(OriginalError::Changed);}
        }
        transaction.commit().map_err(storage_error)?;
        for capture in captures{capture.accepted=true;}
        Ok(())
    }
    /// Called only for the newest live confirmed user, in conversation order.
    /// An unannotated newest user intentionally makes latest lookup unavailable.
    pub(crate) fn latest_user(&mut self,user_id:&str)->Result<(),String>{
        self.scope.revalidate()?;let session=self.confirmed()?;if !valid_id(user_id){return Err("Invalid user identity".into());}
        self.db.execute("INSERT INTO latest VALUES(?1,?2) ON CONFLICT(session) DO UPDATE SET user_id=excluded.user_id",params![session,user_id]).map(|_|()).map_err(|e|e.to_string())
    }
    pub(crate) fn original(&self,user_id:Option<&str>,offset:usize,limit:usize)->Result<OriginalPage,OriginalError>{
        self.scope.revalidate().map_err(OriginalError::Unavailable)?;
        let session=self.confirmed().map_err(OriginalError::Unavailable)?;
        if !(4..=ORIGINAL_PAGE_BYTES).contains(&limit){return Err(OriginalError::InvalidRange);}
        let latest:Option<String>=if user_id.is_none(){self.db.query_row("SELECT user_id FROM latest WHERE session=?1",[session],|r|r.get(0)).optional().map_err(storage_error)?}else{None};
        let id=user_id.or(latest.as_deref()).ok_or_else(||OriginalError::Unavailable("No confirmed latest user message".into()))?;
        let found:Option<(String,Vec<u8>)>=self.db.query_row("SELECT prompts.user_id,metadata FROM prompts JOIN aliases ON aliases.session=prompts.session AND aliases.user_id=prompts.user_id WHERE aliases.session=?1 AND aliases.alias=?2",params![session,id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(storage_error)?;
        let (id,metadata)=found.ok_or_else(||OriginalError::Unavailable("The exact original accepted command is unavailable".into()))?;
        let original:OriginalPrompt=postcard::from_bytes(&metadata).map_err(|e|OriginalError::Unavailable(e.to_string()))?;
        if offset>original.text_bytes{return Err(OriginalError::InvalidRange);}
        let end=offset.saturating_add(limit).min(original.text_bytes);
        let mut statement=self.db.prepare("SELECT position,body FROM prompt_text WHERE session=?1 AND user_id=?2 AND position+length(body)>?3 AND position<?4 ORDER BY position").map_err(storage_error)?;
        let rows=statement.query_map(params![session,id,offset,end],|r|Ok((r.get::<_,usize>(0)?,r.get::<_,Vec<u8>>(1)?))).map_err(storage_error)?;
        let mut bytes=Vec::with_capacity(limit);for row in rows{let(position,body)=row.map_err(storage_error)?;let from=offset.saturating_sub(position);let to=(end-position).min(body.len());bytes.extend_from_slice(&body[from..to]);}
        if bytes.first().is_some_and(|byte|byte&0xc0==0x80){return Err(OriginalError::InvalidRange);}
        while std::str::from_utf8(&bytes).is_err(){if bytes.pop().is_none(){return Err(OriginalError::InvalidRange);}}
        let text=String::from_utf8(bytes).map_err(|_|OriginalError::InvalidRange)?;
        let next_offset=offset+text.len();
        let complete=next_offset==original.text_bytes;
        Ok(OriginalPage{original,offset,next_offset,text,complete})
    }
    /// Repeated delivery of an already seen Awaiting request is replay and
    /// cannot reopen a denied action. Only a distinct request starts a decision.
    pub(crate) fn approval(&mut self,key:&ApprovalKey,request:&str,decision:ApprovalDecision)->Result<(),String>{
        self.scope.revalidate()?;let session=self.confirmed()?.to_owned();let agent=key.agent_id.as_deref().unwrap_or("");
        if !valid_id(&key.item_id)||!valid_id(request)||agent.len()>ID_BYTES{return Err("Approval identity exceeds its limit".into());}
        let transaction=self.db.transaction().map_err(|e|e.to_string())?;
        let known:bool=transaction.query_row("SELECT EXISTS(SELECT 1 FROM approvals WHERE session=?1 AND agent=?2 AND item=?3 AND request=?4)",params![session,agent,key.item_id,request],|r|r.get(0)).map_err(|e|e.to_string())?;
        if decision==ApprovalDecision::Awaiting {
            if !known {
                transaction.execute("INSERT INTO approvals VALUES(?1,?2,?3,?4,0)",params![session,agent,key.item_id,request]).map_err(|e|e.to_string())?;
                transaction.execute("INSERT INTO current_approval VALUES(?1,?2,?3,?4) ON CONFLICT(session,agent,item) DO UPDATE SET request=excluded.request",params![session,agent,key.item_id,request]).map_err(|e|e.to_string())?;
            }
        } else {
            transaction.execute("UPDATE approvals SET decision=?5 WHERE session=?1 AND agent=?2 AND item=?3 AND request=?4 AND decision=0 AND EXISTS(SELECT 1 FROM current_approval WHERE session=?1 AND agent=?2 AND item=?3 AND request=?4)",params![session,agent,key.item_id,request,if decision==ApprovalDecision::Denied{2}else{1}]).map_err(|e|e.to_string())?;
        }
        transaction.commit().map_err(|e|e.to_string())
    }
    pub(crate) fn denied(&self,key:&ApprovalKey)->Result<bool,String>{
        self.scope.revalidate()?;
        let session=self.confirmed()?;
        self.db.query_row("SELECT EXISTS(SELECT 1 FROM current_approval c JOIN approvals a USING(session,agent,item,request) WHERE c.session=?1 AND c.agent=?2 AND c.item=?3 AND a.decision=2)",params![session,key.agent_id.as_deref().unwrap_or(""),key.item_id],|r|r.get(0)).map_err(|e|e.to_string())
    }
}
fn storage_error(error:rusqlite::Error)->OriginalError{OriginalError::Unavailable(error.to_string())}

#[cfg(test)]
pub(crate) fn test_directory()->tempfile::TempDir{
    let base=std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).or_else(||std::env::var_os("HOME").map(|home|PathBuf::from(home).join(".cache"))).expect("annotation tests require a disk-backed artifact directory").join("annotation-contract-tests");
    crate::secure_file::prepare_private_directory(&base).unwrap();
    tempfile::tempdir_in(base).unwrap()
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString,os::unix::fs::PermissionsExt};
    fn scope(root:&Path, provider:crate::agent::Provider)->LaunchAccountScope {
        fs::create_dir_all(root.join("home/.claude")).unwrap();
        fs::create_dir_all(root.join("home/.codex")).unwrap();
        LaunchAccountScope::resolve(provider,root,|name|(name=="HOME").then(||OsString::from(root.join("home")))).unwrap()
    }
    fn prompt(id:&str,text:&str)->OriginalPrompt {
        OriginalPrompt{user_id:id.into(),turn_id:format!("turn-{id}"),text_bytes:text.len(),attachments:vec![],settings:OriginalSettings{model:Setting::Value("confirmed-model".into()),reasoning:Setting::Value("low".into()),fast:Setting::Unsupported,permission:Setting::Value("ask".into())}}
    }
    #[test]
    fn originals_are_immutable_paged_exact_and_isolated_across_cold_open() {
        let root=test_directory();let account=scope(root.path(),crate::agent::Provider::Claude);
        let text="original /operator-command 🐙".repeat(20000);let original=prompt("provider-user-a",&text);
        {
            let mut store=Store::at(&root.path().join("annotations"),&account,"confirmed-session").unwrap();
            store.accept(&original,&text,&["provider-user-a:input/0".into()],&mut []).unwrap();
            store.latest_user(&original.user_id).unwrap();
            let mut modified=original.clone();modified.settings.model=Setting::Value("later-model".into());
            assert_eq!(store.accept(&modified,&text,&[],&mut []),Err(OriginalError::Changed));
        }
        let mut store=Store::at(&root.path().join("annotations"),&account,"confirmed-session").unwrap();
        let mut collected=String::new();let mut offset=0;
        loop {let page=store.original(None,offset,4097).unwrap();assert_eq!(page.original,original);assert_eq!(page.offset,offset);offset=page.next_offset;collected.push_str(&page.text);if page.complete{break;}}
        assert_eq!(collected,text);
        assert_eq!(store.original(Some("provider-user-a:input/0"),0,4).unwrap().original,original);
        let other=Store::at(&root.path().join("annotations"),&account,"different-session").unwrap();
        assert!(matches!(other.original(Some(&original.user_id),0,4096),Err(OriginalError::Unavailable(_))));
        let other_account=scope(root.path(),crate::agent::Provider::Codex);
        let other=Store::at(&root.path().join("annotations"),&other_account,"confirmed-session").unwrap();
        assert!(matches!(other.original(Some(&original.user_id),0,4096),Err(OriginalError::Unavailable(_))));
        store.latest_user("newest-unannotated-provider-user").unwrap();
        assert!(matches!(store.original(None,0,4096),Err(OriginalError::Unavailable(_))),"must not fall back to an older recorded prompt");
        let path=root.path().join("annotations").join(account_key(&account));
        assert_eq!(fs::metadata(path).unwrap().permissions().mode()&0o777,0o700);
    }
    #[test]
    fn denied_survives_more_than_256_actions_cold_reopen_and_stale_replay() {
        let root=test_directory();let account=scope(root.path(),crate::agent::Provider::Codex);
        let key=ApprovalKey{agent_id:None,item_id:"turn:tool".into()};
        {
            let mut store=Store::at(&root.path().join("annotations"),&account,"confirmed-session").unwrap();
            for n in 0..400 {let key=ApprovalKey{agent_id:None,item_id:format!("turn:tool-{n}")};let request=format!("approval-{n}");store.approval(&key,&request,ApprovalDecision::Awaiting).unwrap();store.approval(&key,&request,ApprovalDecision::Denied).unwrap();}
            store.approval(&key,"original-request",ApprovalDecision::Awaiting).unwrap();store.approval(&key,"original-request",ApprovalDecision::Denied).unwrap();
        }
        let mut store=Store::at(&root.path().join("annotations"),&account,"confirmed-session").unwrap();
        assert!(store.denied(&ApprovalKey{agent_id:None,item_id:"turn:tool-0".into()}).unwrap());
        assert!(store.denied(&key).unwrap());
        store.approval(&key,"original-request",ApprovalDecision::Awaiting).unwrap();
        store.approval(&key,"original-request",ApprovalDecision::Allowed).unwrap();
        assert!(store.denied(&key).unwrap(),"replayed resolved request cannot reset a denial");
        store.approval(&key,"genuinely-new-request",ApprovalDecision::Awaiting).unwrap();
        assert!(!store.denied(&key).unwrap());
        store.approval(&key,"genuinely-new-request",ApprovalDecision::Allowed).unwrap();
        assert!(!store.denied(&key).unwrap(),"later generic failure is free to remain Failed");
        store.approval(&key,"original-request",ApprovalDecision::Awaiting).unwrap();
        assert!(!store.denied(&key).unwrap(),"old request replay cannot become current");
        assert!(!store.denied(&ApprovalKey{agent_id:Some("child".into()),item_id:key.item_id.clone()}).unwrap());
    }
    #[test]
    fn unknown_settings_replacement_and_explicit_account_purge_are_checked() {
        let root=test_directory();let account=scope(root.path(),crate::agent::Provider::Claude);
        let annotations=root.path().join("annotations");
        let mut store=Store::at(&annotations,&account,"session").unwrap();let mut original=prompt("user","text");
        original.settings.fast=Setting::Unknown;
        assert!(matches!(store.accept(&original,"text",&[],&mut []),Err(OriginalError::Unavailable(_))));
        original.settings.fast=Setting::Unsupported;store.accept(&original,"text",&[],&mut []).unwrap();
        fs::rename(&account.canonical_home,root.path().join("old-home")).unwrap();fs::create_dir(&account.canonical_home).unwrap();
        assert!(matches!(store.original(Some("user"),0,4),Err(OriginalError::Unavailable(_))));
        assert!(purge_account_at(&annotations,&account).is_err(),"replacement cannot authorize deleting another physical home's annotations");
        fs::remove_dir(&account.canonical_home).unwrap();fs::rename(root.path().join("old-home"),&account.canonical_home).unwrap();
        drop(store);purge_account_at(&annotations,&account).unwrap();
        let reopened=Store::at(&annotations,&account,"session").unwrap();assert!(matches!(reopened.original(Some("user"),0,4),Err(OriginalError::Unavailable(_))));
    }
    #[test]
    fn attachment_snapshot_keeps_original_bytes_and_registered_purge_works_after_home_loss(){
        let root=test_directory();let mut account=scope(root.path(),crate::agent::Provider::Claude);
        account.registered_id=Some(format!("claude/{}",uuid::Uuid::new_v4()).parse().unwrap());
        let annotations=root.path().join("annotations");let store=Store::at(&annotations,&account,"session").unwrap();
        let path=root.path().join("picked.png");fs::write(&path,b"original image bytes").unwrap();
        let capture=store.retain_attachment(&path).unwrap();let attachment=&capture.attachment;fs::write(&path,b"changed later").unwrap();
        assert_eq!(attachment.original_path,path);assert_eq!(fs::read(&attachment.path).unwrap(),b"original image bytes");
        assert_eq!(attachment.sha256,< [u8;32] >::from(Sha256::digest(b"original image bytes")));
        assert_eq!(fs::metadata(&attachment.path).unwrap().permissions().mode()&0o777,0o600);
        drop(capture);drop(store);fs::remove_dir(&account.canonical_home).unwrap();
        assert_eq!(purge_registered_at(&annotations,account.registered_id.as_ref().unwrap()).unwrap(),Some(1));
        assert_eq!(purge_registered_at(&annotations,account.registered_id.as_ref().unwrap()).unwrap(),Some(0));
        let unknown=format!("claude/{}",uuid::Uuid::new_v4()).parse().unwrap();assert_eq!(purge_registered_at(&annotations,&unknown).unwrap(),None);
    }

    #[test]
    fn pending_captures_abort_cleanly_accepted_bytes_survive_and_cold_orphans_are_recovered(){
        let root=test_directory();let account=scope(root.path(),crate::agent::Provider::Claude);let annotations=root.path().join("annotations");
        let mut store=Store::at(&annotations,&account,"session").unwrap();let path=root.path().join("picked.png");fs::write(&path,b"image").unwrap();
        let abandoned=store.retain_attachment(&path).unwrap();let abandoned_path=abandoned.attachment.path.clone();drop(abandoned);assert!(!abandoned_path.exists());
        let mut captures=vec![store.retain_attachment(&path).unwrap()];let accepted_path=captures[0].attachment.path.clone();let mut original=prompt("accepted","text");original.attachments=vec![captures[0].attachment.clone()];
        store.accept(&original,"text",&[],&mut captures).unwrap();drop(captures);assert!(accepted_path.exists());
        let active=store.retain_attachment(&path).unwrap();let active_path=active.attachment.path.clone();
        let another=Store::at(&annotations,&account,"session").unwrap();assert!(active_path.exists(),"cold recovery must preserve a live leased capture");drop(another);
        // Model a crash by releasing the descriptor without running Drop.
        let orphan_path=active.attachment.path.clone();
        let mut orphan=active;orphan.accepted=true;drop(orphan);drop(store);
        let reopened=Store::at(&annotations,&account,"session").unwrap();assert!(!orphan_path.exists());assert!(accepted_path.exists());
        assert_eq!(reopened.original(Some("accepted"),0,4).unwrap().original,original);
    }

    #[test]
    fn tiny_multi_image_prompt_fits_but_actual_aggregate_pending_bytes_are_bounded(){
        let root=test_directory();let account=scope(root.path(),crate::agent::Provider::Claude);let annotations=root.path().join("annotations");
        let mut store=Store::at(&annotations,&account,"session").unwrap();let path=root.path().join("tiny.png");fs::write(&path,b"tiny image").unwrap();
        let mut captures=(0..4).map(|_|store.retain_attachment(&path).unwrap()).collect::<Vec<_>>();let mut original=prompt("four-images","text");original.attachments=captures.iter().map(|capture|capture.attachment.clone()).collect();
        store.accept(&original,"text",&[],&mut captures).unwrap();drop(captures);
        let large=root.path().join("large.png");fs::File::create(&large).unwrap().set_len(32*1024*1024).unwrap();
        let first=store.retain_attachment(&large).unwrap();let second=store.retain_attachment(&large).unwrap();
        assert!(matches!(store.retain_attachment(&path),Err(OriginalError::TooLarge)));drop(first);drop(second);
        assert!(store.retain_attachment(&path).is_ok());
    }

    #[test]
    fn cold_recovery_handles_each_interrupted_publication_cut_without_unindexed_files(){
        let root=test_directory();let account=scope(root.path(),crate::agent::Provider::Claude);let annotations=root.path().join("annotations");
        let store=Store::at(&annotations,&account,"session").unwrap();
        // Durable row committed, process died before file creation.
        store.db.execute("INSERT INTO retained_attachments VALUES('capture-before-file',0,4,NULL)",[]).unwrap();
        // Durable row committed and file created, process died before lease.
        store.db.execute("INSERT INTO retained_attachments VALUES('capture-before-lease',0,4,NULL)",[]).unwrap();
        let orphan=store.directory.join("capture-before-lease");fs::write(&orphan,b"data").unwrap();fs::set_permissions(&orphan,fs::Permissions::from_mode(0o600)).unwrap();
        drop(store);let restored=Store::at(&annotations,&account,"session").unwrap();
        assert!(!orphan.exists());let pending:usize=restored.db.query_row("SELECT count(*) FROM retained_attachments WHERE accepted=0",[],|row|row.get(0)).unwrap();assert_eq!(pending,0);
    }

    #[test]
    fn unconfirmed_capture_binds_atomically_without_changing_the_provider_visible_path(){
        let root=test_directory();let account=scope(root.path(),crate::agent::Provider::Claude);let annotations=root.path().join("annotations");
        let mut pending=Store::open(&annotations,&account,None).unwrap();let image=root.path().join("image.png");fs::write(&image,b"immutable actual dispatch").unwrap();let mut captures=vec![pending.retain_attachment(&image).unwrap()];let sent_path=captures[0].attachment.path.clone();
        let mut original=prompt("actual-provider-user","text");original.attachments=vec![captures[0].attachment.clone()];
        assert!(matches!(pending.accept(&original,"text",&[],&mut captures),Err(OriginalError::Unavailable(_))),"a dispatch selector is not session authority");
        assert!(matches!(pending.original(Some(&original.user_id),0,4),Err(OriginalError::Unavailable(_))));
        let mut confirmed=Store::at(&annotations,&account,"actual-confirmed-session").unwrap();confirmed.accept(&original,"text",&[],&mut captures).unwrap();drop(captures);drop(pending);drop(confirmed);
        let mut restored=Store::at(&annotations,&account,"actual-confirmed-session").unwrap();assert_eq!(fs::read(&sent_path).unwrap(),b"immutable actual dispatch");assert_eq!(restored.original(Some(&original.user_id),0,4).unwrap().original.attachments[0].path,sent_path);
        let mut other=Store::at(&annotations,&account,"another-session").unwrap();let other_original=prompt("actual-provider-user","else");other.accept(&other_original,"else",&[],&mut []).unwrap();
        restored.purge_current_session().unwrap();assert!(!sent_path.exists());assert!(matches!(restored.original(Some(&original.user_id),0,4),Err(OriginalError::Unavailable(_))));assert_eq!(other.original(Some(&original.user_id),0,4).unwrap().text,"else");
    }

    #[test]
    fn explicit_session_purge_drains_more_than_1024_files_in_bounded_batches(){
        let root=test_directory();let account=scope(root.path(),crate::agent::Provider::Claude);let annotations=root.path().join("annotations");let mut store=Store::at(&annotations,&account,"session-to-delete").unwrap();
        let transaction=store.db.transaction().unwrap();
        for index in 0..1100{let name=format!("capture-history-{index:04}");fs::write(store.directory.join(&name),b"").unwrap();fs::set_permissions(store.directory.join(&name),fs::Permissions::from_mode(0o600)).unwrap();transaction.execute("INSERT INTO retained_attachments VALUES(?1,1,0,'session-to-delete')",[name]).unwrap();}
        transaction.commit().unwrap();
        let preserved=store.directory.join("capture-other-session");fs::write(&preserved,b"other").unwrap();store.db.execute("INSERT INTO retained_attachments VALUES('capture-other-session',1,5,'other-session')",[]).unwrap();
        store.purge_current_session().unwrap();
        let remaining:usize=store.db.query_row("SELECT count(*) FROM retained_attachments",[],|row|row.get(0)).unwrap();assert_eq!(remaining,1);assert_eq!(fs::read(preserved).unwrap(),b"other");
        assert_eq!(fs::read_dir(&store.directory).unwrap().filter_map(Result::ok).filter(|entry|entry.file_name().to_string_lossy().starts_with("capture-history-")).count(),0);
    }

}
