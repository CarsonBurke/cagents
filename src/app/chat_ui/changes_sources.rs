//! Exact file patches are read independently from bounded display fragments.
//! The metadata root never claims a transport prefix is a complete patch.
use super::*;
use chat::cache::changes::{File,Manifest,ManifestQuery,PatchPage,PatchQuery};

/// A completed read remains within the shared two-reader admission until the
/// caller either adopts its immutable text into the source pool or drops it.
pub(super) struct PendingPatch {
    pub manifest:Arc<Manifest>,
    pub file:File,
    pub text:Arc<str>,
    pub deadline:Instant,
    _reader:tokio::sync::OwnedSemaphorePermit,
    _capacity:chat::source_owners::ReaderCapacity,
}

pub(super) async fn read_manifest<F,Fut>(query:ManifestQuery,mut fetch:F)->Result<Arc<Manifest>,String>
where F:FnMut(ManifestQuery)->Fut,
      Fut:std::future::Future<Output=Result<Manifest,chat::cache::SourceError>> {
    if query.limit==0 || query.limit>chat::cache::changes::MANIFEST_FILES {
        return Err("The requested file page is invalid.".into());
    }
    let page=tokio::time::timeout(Duration::from_secs(40),fetch(query.clone())).await
        .map_err(|_|"The original file list could not be read in time.".to_owned())?
        .map_err(|_|"The original file list changed or is unavailable.".to_owned())?;
    let end=page.offset.checked_add(page.files.len()).ok_or("The original file list is invalid.")?;
    let terminal=end.checked_add(page.omitted_files)==Some(page.metadata.total_files);
    let next_valid=match page.next_offset {
        Some(next)=>next==end && next>page.offset && next<page.metadata.total_files && page.omitted_files==0,
        None=>terminal,
    };
    if page.source!=query.source || page.offset!=query.offset
        || query.expected_epoch.is_some_and(|epoch|epoch!=page.epoch)
        || page.files.len()>query.limit || !next_valid
        || (page.omitted_files>0 && page.metadata.unavailable.is_none())
        || page.files.iter().enumerate().any(|(index,file)|file.file_index!=page.offset+index || file.source_len>page.metadata.total_bytes)
        || postcard::to_allocvec(&page).map_err(|_|"The original file list is invalid.".to_owned())?.len()>chat::cache::PAGE_BYTES {
        return Err("The original file list is incomplete or changed. Retry loading it.".into());
    }
    Ok(Arc::new(page))
}

pub(super) async fn read_patch<F,Fut>(manifest:Arc<Manifest>,file:File,mut fetch:F)->Result<PendingPatch,String>
where F:FnMut(PatchQuery)->Fut,
      Fut:std::future::Future<Output=Result<PatchPage,chat::cache::SourceError>> {
    if file.source_len>chat::source_owners::SOURCE_BYTES {
        return Err("This patch exceeds the 32 MiB display limit.".into());
    }
    if !manifest.files.iter().any(|listed|listed==&file) {
        return Err("The patch is not part of the verified file list.".into());
    }
    let deadline=Instant::now()+Duration::from_secs(40);
    tokio::time::timeout(Duration::from_secs(40),async move {
        let reader=chat::source_owners::readers().clone().acquire_owned().await
            .map_err(|_|"Patch loading stopped.".to_owned())?;
        let capacity=chat::source_owners::ReaderCapacity::default();
        let mut text=String::with_capacity(file.source_len);capacity.set(text.capacity());
        let mut offset=0;
        loop {
            let page=fetch(PatchQuery {source:manifest.source.clone(),file_index:file.file_index,offset,
                limit:chat::cache::PAGE_BYTES,expected_epoch:manifest.epoch,until:file.source_len}).await
                .map_err(|error|match error {
                    chat::cache::SourceError::Changed=>"The patch changed while loading. Retry its current contents.".to_owned(),
                    _=>"The complete patch is unavailable. Retry when the session is connected.".to_owned(),
                })?;
            if page.source!=manifest.source || page.epoch!=manifest.epoch || page.file!=file || page.offset!=offset
                || page.text.len()>chat::cache::PAGE_BYTES || page.next_offset>file.source_len || page.next_offset.checked_sub(offset)!=Some(page.text.len())
                || page.complete!=(page.next_offset==file.source_len)
                || (!page.complete && page.next_offset==offset) {
                return Err("The patch changed or returned an incomplete page.".into());
            }
            text.push_str(&page.text);offset=page.next_offset;
            if page.complete {break;}
        }
        // Arc<str> conversion can briefly overlap the builder allocation. Count
        // that explicit stage; it is not hidden inside a claimed RSS bound.
        capacity.set(text.capacity().saturating_add(text.len()));
        let text:Arc<str>=Arc::from(text);capacity.set(text.len());
        Ok(PendingPatch {manifest,file,text,deadline,_reader:reader,_capacity:capacity})
    }).await.map_err(|_|"The complete patch could not be read in time.".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    fn epoch(revision:u64)->u64 {revision}
    fn fixture(text:&str)->Arc<Manifest> {
        Arc::new(Manifest {source:chat::cache::SourceKey {agent_id:None,source_id:"opaque:native-part:looks-like-a-suffix".into()},
            epoch:epoch(7),metadata:chat::cache::changes::Metadata {role:chat::Role::Assistant,status:ToolStatus::Succeeded,
                at:Some(20),started_at:Some(10),total_files:1,total_bytes:text.len(),additions:Some(1),deletions:Some(1),unavailable:None},
            offset:0,next_offset:None,files:vec![File {file_index:0,path:"src/fixture.rs".into(),kind:ChangeKind::Updated,
                source_len:text.len(),additions:Some(1),deletions:Some(1)}],omitted_files:0})
    }
    fn page(query:PatchQuery,text:&str,file:File)->PatchPage {
        let mut end=(query.offset+query.limit).min(text.len());while !text.is_char_boundary(end) {end-=1;}
        PatchPage {source:query.source,epoch:query.expected_epoch,file,offset:query.offset,next_offset:end,
            text:text[query.offset..end].into(),complete:end==query.until}
    }
    #[tokio::test]
    async fn complete_patch_read_preserves_utf8_and_keeps_actual_output_owner_until_drop() {
        let text=Arc::<str>::from(format!("@@ -1,1 +1,1 @@\n-before\n+after\n{}LAST_PATCH_ANCHOR\n"," // αβ context\n".repeat(40_000)));
        let manifest=fixture(&text);let file=manifest.files[0].clone();
        let calls=Arc::new(Mutex::new(Vec::new()));let captured=calls.clone();
        let expected=text.clone();let expected_file=file.clone();
        let loaded=read_patch(manifest.clone(),file,move |query| {
            captured.lock().unwrap().push(query.clone());let page=page(query,&text,expected_file.clone());async move {Ok(page)}
        }).await.unwrap();
        assert_eq!(loaded.text.as_ref(),expected.as_ref());assert!(calls.lock().unwrap().len()>1);
        assert!(calls.lock().unwrap().iter().all(|query|query.source==manifest.source && query.expected_epoch==manifest.epoch
            && query.until==expected.len() && query.file_index==0));
        let weak=Arc::downgrade(&loaded.text);assert!(weak.upgrade().is_some());drop(loaded);
        assert!(weak.upgrade().is_none(),"discarded pending output releases the actual completed patch allocation");
    }
    #[tokio::test]
    async fn malformed_patch_pages_never_return_a_partial_complete_patch() {
        for malformed in 0..6 {
            let text="@@ -1,1 +1,1 @@\n-before\n+after\n";
            let manifest=fixture(text);let file=manifest.files[0].clone();let returned=file.clone();
            let result=read_patch(manifest,file,move |query| {
                let mut value=page(query,text,returned.clone());
                match malformed {
                    0=>value.epoch=epoch(8),
                    1=>value.file.file_index=99,
                    2=>value.file.path="different/path.rs".into(),
                    3=>value.next_offset-=1,
                    4=>value.complete=false,
                    _=>value.source.agent_id=Some("different-child".into()),
                }
                async move {Ok(value)}
            }).await;
            assert!(result.is_err(),"malformed page case {malformed}");
        }
    }
    #[tokio::test]
    async fn manifest_pages_require_exact_offsets_and_explicit_omission() {
        let manifest=fixture("patch");
        for malformed in 0..5 {
            let query=ManifestQuery {source:manifest.source.clone(),offset:0,limit:128,expected_epoch:Some(manifest.epoch)};
            let mut returned=(*manifest).clone();
            match malformed {
                0=>returned.files[0].file_index=1,
                1=>returned.metadata.total_files=2,
                2=>returned.omitted_files=1,
                3=>returned.next_offset=Some(0),
                _=>returned.epoch=epoch(8),
            }
            assert!(read_manifest(query,move |_| {let returned=returned.clone();async move {Ok(returned)}}).await.is_err());
        }
        let mut omitted=(*manifest).clone();omitted.files.clear();omitted.omitted_files=1;
        omitted.metadata.unavailable=Some(chat::cache::changes::Unavailable::MetadataTooLarge {limit:1024});
        let query=ManifestQuery {source:manifest.source.clone(),offset:0,limit:128,expected_epoch:Some(manifest.epoch)};
        let result=read_manifest(query,move |_| {let omitted=omitted.clone();async move {Ok(omitted)}}).await.unwrap();
        assert_eq!(result.omitted_files,1);assert!(result.metadata.unavailable.is_some());
    }
    #[test]
    fn reader_future_and_pending_output_are_send_without_ui_cache_owners() {
        fn assert_send<T:Send>() {}assert_send::<PendingPatch>();
        fn future_send<T:Send>(_:T) {}
        let manifest=fixture("patch");let file=manifest.files[0].clone();
        future_send(read_patch(manifest,file,|_|async {Err(chat::cache::SourceError::Changed)}));
    }
}
