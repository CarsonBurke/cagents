//! Source-qualified parent history aliases; separate from live input proof.
use super::*;
/// These are source row aliases, not accepted input or terminal outcome proof.
pub(super) fn events(account:&crate::chat::turn_outcomes::HistoryAccountOrigin,session:&str,events:&[Event])->Vec<Event>{
    use crate::chat::turn_outcomes::{MAX_TURN_ITEM_IDS,MAX_TURN_UPDATE_BYTES,MAX_TURN_ID_BYTES,validate_history_origins};
    use std::os::unix::ffi::OsStrExt;
    let base=96+account.canonical_home.as_os_str().as_bytes().len()+session.len();let mut bytes=base;let mut ids=Vec::new();let mut result=Vec::new();
    for event in events {
        let Event::Item(item)=event else{continue;};
        if item.id.is_empty() || item.id.len()>MAX_TURN_ID_BYTES || item.id.chars().any(char::is_control) || ids.contains(&item.id){continue;}
        let size=item.id.len()+8;
        if !ids.is_empty() && (ids.len()==MAX_TURN_ITEM_IDS || bytes.saturating_add(size)>MAX_TURN_UPDATE_BYTES){
            if validate_history_origins(account,session,&ids).is_ok(){result.push(Event::HistoryOrigins{account:account.clone(),session_id:session.into(),item_ids:std::mem::take(&mut ids)});}else{ids.clear();}bytes=base;
        }
        if base.saturating_add(size)>MAX_TURN_UPDATE_BYTES{continue;}
        bytes+=size;ids.push(item.id.clone());
    }
    if !ids.is_empty() && validate_history_origins(account,session,&ids).is_ok(){result.push(Event::HistoryOrigins{account:account.clone(),session_id:session.into(),item_ids:ids});}
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const SESSION:&str="00000000-0000-4000-8000-000000000001";
    fn source_history_fixture()->(tempfile::TempDir,Launch,std::path::PathBuf){
        use std::io::Write;
        let root=tempfile::tempdir().unwrap();let cwd=root.path().join("removed-project");std::fs::create_dir(&cwd).unwrap();
        let home=root.path().join("claude-home");
        let project:String=cwd.to_str().unwrap().chars().map(|c|if c.is_ascii_alphanumeric(){c}else{'-'}).collect();
        let directory=home.join("projects").join(project);std::fs::create_dir_all(&directory).unwrap();
        let mut file=File::create(directory.join(format!("{SESSION}.jsonl"))).unwrap();
        for index in 0..130 {
            let mut frame=json!({"type":"user","uuid":format!("row-{index}"),"parentUuid":(index>0).then(||format!("row-{}",index-1)),"message":{"role":"user","content":format!("history-{index}")}});
            if !matches!(index,0|50|129){frame["sessionId"]=json!(SESSION);}
            writeln!(file,"{frame}").unwrap();
        }
        drop(file);std::fs::remove_dir(&cwd).unwrap();
        let launch=Launch{provider:Provider::Claude,session_id:Some(SESSION.into()),recipe:crate::model::LaunchRecipe{program:"claude".into(),arguments:vec![],working_directory:cwd,environment:std::collections::BTreeMap::from([("CLAUDE_CONFIG_DIR".into(),home.to_string_lossy().into_owned())]),unset_environment:Default::default()}};
        (root,launch,home)
    }
    #[test]
    fn actual_read_only_initialize_and_earlier_history_retain_only_exact_source_origins(){
        use std::os::unix::fs::MetadataExt;
        let (_root,launch,home)=source_history_fixture();let (_,mut adapter)=create(&launch).unwrap();
        let first=adapter.initialize().unwrap();let earlier=adapter.command(Command::LoadEarlier).unwrap();
        let meta=std::fs::metadata(&home).unwrap();let mut all=std::collections::HashSet::new();
        for page in [&first,&earlier]{
            let visible:std::collections::HashSet<_>=page.events.iter().filter_map(|event|match event{Event::History{items,..}=>Some(items.iter().map(|item|item.id.clone())),_=>None}).flatten().collect();
            assert!(!visible.is_empty());let mut count=0;
            for event in &page.events {if let Event::HistoryOrigins{account,session_id,item_ids}=event {
                assert_eq!(session_id,SESSION);assert_eq!(account.provider,Provider::Claude);
                assert_eq!(account.canonical_home,home.canonicalize().unwrap());assert_eq!((account.device,account.inode),(meta.dev(),meta.ino()));
                crate::chat::turn_outcomes::validate_history_origins(account,session_id,item_ids).unwrap();
                assert!(item_ids.iter().all(|id|visible.contains(id)));all.extend(item_ids.iter().cloned());count+=1;
            }}assert!(count>0);
            assert!(page.events.iter().all(|event|!matches!(event,Event::Session{..}|Event::TurnOutcome(_))));
        }
        for index in [0,50,129]{assert!(!all.contains(&format!("row-{index}:0")));}
        assert!(all.contains("row-1:0"));assert!(all.contains("row-128:0"));
    }
    #[test]
    fn replaced_history_account_home_cannot_publish_old_source_provenance(){
        let (root,launch,home)=source_history_fixture();let (_,mut adapter)=create(&launch).unwrap();
        let first=adapter.initialize().unwrap();assert!(first.events.iter().any(|event|matches!(event,Event::HistoryOrigins{..})));
        std::fs::rename(&home,root.path().join("old-home")).unwrap();std::fs::create_dir(&home).unwrap();
        let earlier=adapter.command(Command::LoadEarlier).unwrap();
        assert!(earlier.events.iter().all(|event|!matches!(event,Event::HistoryOrigins{..})));
    }

}
