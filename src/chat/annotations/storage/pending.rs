//! Durable originals waiting for an exact provider acceptance. A posted intent
//! is not an accepted prompt, and is never exposed through original().
use super::*;

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub(crate) struct PreparedCommand {
    pub command_id:String,
    /// Guard only. This cannot authorize accepted metadata lookups.
    pub expected_session:Option<String>,
    pub text:OriginalAttachment,
    pub attachments:Vec<OriginalAttachment>,
    pub settings:Option<OriginalSettings>,
}
impl Store {
    /// Publish before dispatch. Until acceptance, only an explicit proven
    /// rejection/abandonment or history deletion can retire this durable intent.
    pub(crate) fn prepare_command(&mut self,id:&str,expected_session:Option<&str>,text:&mut PendingText,captures:&mut[PendingAttachment])->Result<PreparedCommand,OriginalError>{
        self.scope.revalidate().map_err(OriginalError::Unavailable)?;
        if !valid_id(id)||expected_session.is_some_and(|id|!valid_id(id))||captures.len()>32{return Err(OriginalError::TooLarge);}
        let command=PreparedCommand{command_id:id.to_owned(),expected_session:expected_session.map(str::to_owned),text:text.0.attachment.clone(),attachments:captures.iter().map(|capture|capture.attachment.clone()).collect(),settings:None};
        let metadata=postcard::to_stdvec(&command).map_err(|e|OriginalError::Unavailable(e.to_string()))?;
        if metadata.len()>META_BYTES{return Err(OriginalError::TooLarge);}
        let transaction=self.db.transaction().map_err(storage_error)?;
        transaction.execute("INSERT INTO pending_commands VALUES(?1,?2,?3)",params![id,expected_session,metadata]).map_err(storage_error)?;
        for attachment in std::iter::once(&command.text).chain(&command.attachments){
            if attachment.path.parent()!=Some(self.directory.as_path()){return Err(OriginalError::Changed);}
            let name=attachment.path.file_name().and_then(|name|name.to_str()).ok_or(OriginalError::Changed)?;
            if transaction.execute("INSERT INTO command_captures SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM retained_attachments WHERE path=?2 AND accepted=0 AND session IS NULL)",params![id,name]).map_err(storage_error)?!=1{return Err(OriginalError::Changed);}
        }
        transaction.commit().map_err(storage_error)?;
        text.0.retained=true;
        for capture in captures{capture.retained=true;}
        Ok(command)
    }
    pub(crate) fn prepared(&self,id:&str)->Result<PreparedCommand,OriginalError>{
        self.scope.revalidate().map_err(OriginalError::Unavailable)?;
        if !valid_id(id){return Err(OriginalError::TooLarge);}
        let metadata:Option<Vec<u8>>=self.db.query_row("SELECT metadata FROM pending_commands WHERE command_id=?1",[id],|row|row.get(0)).optional().map_err(storage_error)?;
        let metadata=metadata.ok_or_else(||OriginalError::Unavailable("No exact original command intent is retained".into()))?;
        if metadata.len()>META_BYTES{return Err(OriginalError::TooLarge);}
        let command:PreparedCommand=postcard::from_bytes(&metadata).map_err(|e|OriginalError::Unavailable(e.to_string()))?;
        if command.command_id!=id{return Err(OriginalError::Changed);}
        Ok(command)
    }
    /// The caller supplies fresh provider-reported evidence bound to the actual
    /// ordered dispatch. A later settings response cannot rewrite this record.
    pub(crate) fn prepared_settings(&mut self,id:&str,settings:&OriginalSettings)->Result<(),OriginalError>{
        if !settings.is_exact(){return Err(OriginalError::Unavailable("The original provider settings are not known exactly".into()));}
        let transaction=self.db.transaction().map_err(storage_error)?;
        self.scope.revalidate().map_err(OriginalError::Unavailable)?;
        let metadata:Vec<u8>=transaction.query_row("SELECT metadata FROM pending_commands WHERE command_id=?1",[id],|row|row.get(0)).map_err(storage_error)?;
        let mut command:PreparedCommand=postcard::from_bytes(&metadata).map_err(|e|OriginalError::Unavailable(e.to_string()))?;
        if command.settings.as_ref().is_some_and(|previous|previous!=settings){return Err(OriginalError::Changed);}
        command.settings=Some(settings.clone());let metadata=postcard::to_stdvec(&command).map_err(|e|OriginalError::Unavailable(e.to_string()))?;
        if metadata.len()>META_BYTES{return Err(OriginalError::TooLarge);}
        transaction.execute("UPDATE pending_commands SET metadata=?2 WHERE command_id=?1",params![id,metadata]).map_err(storage_error)?;
        transaction.commit().map_err(storage_error)
    }
    pub(crate) fn abandon_command(&mut self,id:&str)->Result<(),OriginalError>{
        self.scope.revalidate().map_err(OriginalError::Unavailable)?;
        self.db.execute("DELETE FROM pending_commands WHERE command_id=?1",[id]).map_err(storage_error)?;
        self.recover_pending().map_err(OriginalError::Unavailable)
    }
    /// Restore only one exact intent by provider-correlated ID. Neither the
    /// entire outbox nor any conversation body is hydrated into actor memory.
    pub(crate) fn accept_prepared(&mut self,id:&str,user_id:&str,turn_id:&str,aliases:&[String])->Result<(),OriginalError>{
        let session=self.confirmed().map_err(OriginalError::Unavailable)?.to_owned();
        let command=self.prepared(id)?;
        if command.expected_session.as_deref().is_some_and(|expected|expected!=session){return Err(OriginalError::Changed);}
        let settings=command.settings.ok_or_else(||OriginalError::Unavailable("No fresh settings evidence is bound to this original dispatch".into()))?;
        let mut text=PendingText(self.restore_capture(command.text)?);
        let mut captures=command.attachments.into_iter().map(|attachment|self.restore_capture(attachment)).collect::<Result<Vec<_>,_>>()?;
        let original=OriginalPrompt{user_id:user_id.into(),turn_id:turn_id.into(),settings,attachments:captures.iter().map(|capture|capture.attachment.clone()).collect(),text_bytes:text.bytes()};
        let file=crate::secure_file::open_existing_private_file(&text.0.attachment.path).map_err(|e|OriginalError::Unavailable(e.to_string()))?;
        self.accept_reader(&original,file,text.0.attachment.sha256,aliases,&mut captures,Some(id))?;
        text.0.retained=false;
        Ok(())
    }
    fn restore_capture(&self,attachment:OriginalAttachment)->Result<PendingAttachment,OriginalError>{
        use std::{io::Read,os::fd::AsRawFd};
        if attachment.path.parent()!=Some(self.directory.as_path())||attachment.bytes>32*1024*1024{return Err(OriginalError::Changed);}
        let mut file=crate::secure_file::open_existing_private_file(&attachment.path).map_err(|e|OriginalError::Unavailable(e.to_string()))?;
        // SAFETY: this is a validated, live regular descriptor. The outbox row
        // also fences cold collection until the acceptance transaction commits.
        if unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0{return Err(OriginalError::Unavailable("The original capture is currently in use".into()));}
        let mut hash=Sha256::new();let mut bytes=0u64;let mut chunk=[0u8;64*1024];
        loop{let count=file.read(&mut chunk).map_err(|e|OriginalError::Unavailable(e.to_string()))?;if count==0{break;}bytes=bytes.saturating_add(count as u64);if bytes>attachment.bytes{return Err(OriginalError::Changed);}hash.update(&chunk[..count]);}
        if bytes!=attachment.bytes||<[u8;32]>::from(hash.finalize())!=attachment.sha256{return Err(OriginalError::Changed);}
        Ok(PendingAttachment{attachment,_lease:file,retained:true,reserved_bytes:0,pending_bytes:Default::default()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope(root:&Path)->LaunchAccountScope {
        let home=root.join("codex-home");fs::create_dir_all(&home).unwrap();
        LaunchAccountScope::resolve(crate::agent::Provider::Codex,root,|key|(key=="CODEX_HOME").then(||home.as_os_str().to_owned())).unwrap()
    }
    fn settings()->OriginalSettings {OriginalSettings{model:Setting::Value("reported-model".into()),reasoning:Setting::Value("low".into()),fast:Setting::Unsupported,permission:Setting::Value("ask".into())}}
    #[test]
    fn pre_session_intent_survives_cold_open_and_binds_exact_accepted_identity_atomically(){
        let root=test_directory();let scope=scope(root.path());let directory=root.path().join("annotations");
        let image=root.path().join("image.png");fs::write(&image,b"original image").unwrap();
        let mut pending=Store::open(&directory,&scope,None).unwrap();
        let mut text=pending.capture_text("original /command 🐙").unwrap();let mut images=vec![pending.retain_attachment(&image).unwrap()];
        let posted=pending.prepare_command("exact-command",None,&mut text,&mut images).unwrap();
        drop(text);drop(images);drop(pending);fs::write(&image,b"later picker contents").unwrap();
        let mut accepted=Store::at(&directory,&scope,"provider-confirmed-session").unwrap();
        assert!(posted.text.path.exists());assert!(posted.attachments[0].path.exists());
        assert!(matches!(accepted.original(Some("actual-user"),0,1024),Err(OriginalError::Unavailable(_))));
        assert!(matches!(accepted.accept_prepared("exact-command","actual-user","actual-turn",&[]),Err(OriginalError::Unavailable(_))),"No requested/default settings may masquerade as provider evidence");
        accepted.prepared_settings("exact-command",&settings()).unwrap();
        let mut changed=settings();changed.model=Setting::Value("later-model".into());assert_eq!(accepted.prepared_settings("exact-command",&changed),Err(OriginalError::Changed));
        accepted.accept_prepared("exact-command","actual-user","actual-turn",&["projected-alias".into()]).unwrap();
        assert!(matches!(accepted.prepared("exact-command"),Err(OriginalError::Unavailable(_))));assert!(!posted.text.path.exists());
        let original=accepted.original(Some("projected-alias"),0,1024).unwrap();assert_eq!(original.text,"original /command 🐙");assert_eq!(original.original.user_id,"actual-user");assert_eq!(original.original.attachments[0].path,posted.attachments[0].path);assert_eq!(fs::read(&posted.attachments[0].path).unwrap(),b"original image");
        drop(accepted);
        let cold=Store::at(&directory,&scope,"provider-confirmed-session").unwrap();assert_eq!(cold.original(Some("actual-user"),0,1024).unwrap(),original);
        let other=Store::at(&directory,&scope,"different-session").unwrap();assert!(matches!(other.original(Some("actual-user"),0,1024),Err(OriginalError::Unavailable(_))));
    }
    #[test]
    fn wrong_session_and_modified_capture_fail_without_consuming_intent_then_rejection_collects(){
        let root=test_directory();let scope=scope(root.path());let directory=root.path().join("annotations");
        let mut pending=Store::open(&directory,&scope,None).unwrap();let mut text=pending.capture_text("original text").unwrap();
        let posted=pending.prepare_command("command",Some("expected-session"),&mut text,&mut []).unwrap();drop(text);
        pending.prepared_settings("command",&settings()).unwrap();drop(pending);
        let mut wrong=Store::at(&directory,&scope,"wrong-session").unwrap();assert_eq!(wrong.accept_prepared("command","user","turn",&[]),Err(OriginalError::Changed));drop(wrong);
        let mut exact=Store::at(&directory,&scope,"expected-session").unwrap();
        fs::write(&posted.text.path,b"modified text").unwrap();assert_eq!(exact.accept_prepared("command","user","turn",&[]),Err(OriginalError::Changed));assert!(exact.prepared("command").is_ok());assert!(matches!(exact.original(Some("user"),0,4),Err(OriginalError::Unavailable(_))));
        exact.abandon_command("command").unwrap();assert!(!posted.text.path.exists());assert!(matches!(exact.prepared("command"),Err(OriginalError::Unavailable(_))));
    }
    #[test]
    fn failed_publication_does_not_publish_or_retain_unowned_captures(){
        let root=test_directory();let scope=scope(root.path());let directory=root.path().join("annotations");let mut pending=Store::open(&directory,&scope,None).unwrap();
        let mut text=pending.capture_text("first").unwrap();let first=pending.prepare_command("duplicate",None,&mut text,&mut []).unwrap();drop(text);
        let mut second=pending.capture_text("second").unwrap();let second_path=second.0.attachment.path.clone();assert!(pending.prepare_command("duplicate",None,&mut second,&mut []).is_err());drop(second);assert!(!second_path.exists());assert!(first.text.path.exists());
        pending.abandon_command("duplicate").unwrap();assert!(!first.text.path.exists());
    }
}
