//! A source read may reuse only a lease owned by this Store, never an unknown
//! connection transaction. Its result is released only after explicit rollback.
use super::*;
use std::cell::Cell;

#[derive(Default)]
pub(super) struct State { depth: Cell<usize>, poisoned: Cell<bool> }
impl State {
    pub(super) fn guard_connection(&self,db:&Connection)->Result<(),String> {
        if self.poisoned.get() { return Err(POISONED.into()); }
        let transaction=db.transaction_state::<&str>(None).map_err(|error| {
            self.poisoned.set(true);
            format!("Source cache transaction state could not be verified: {error}")
        })?;
        let tracked=self.depth.get()>0;
        let valid=if tracked {
            !db.is_autocommit() && matches!(transaction,rusqlite::TransactionState::None|rusqlite::TransactionState::Read)
        } else {
            db.is_autocommit() && transaction==rusqlite::TransactionState::None
        };
        if !valid {
            self.poisoned.set(true);
            return Err("Source cache transaction ownership is unknown; this connection is unavailable".into());
        }
        Ok(())
    }
}
const MAX_DEPTH: usize=8;
const POISONED:&str="Source cache read transaction could not be released; this connection is unavailable";
struct Lease<'a> {
    db: &'a Connection,
    state: &'a State,
    transaction: Option<rusqlite::Transaction<'a>>,
    active: bool,
}
impl<'a> Lease<'a> {
    fn acquire(store:&'a Store)->Result<Self,String> {
        store.source_guard()?;
        let state=&store.source_reads;
        let depth=state.depth.get();
        if depth>=MAX_DEPTH { return Err("Source read nesting capacity was exceeded".into()); }
        let transaction=if depth==0 {
            if !store.db.is_autocommit() {
                state.poisoned.set(true);
                return Err("Source read refused an untracked active transaction".into());
            }
            match store.db.unchecked_transaction() {
                Ok(transaction)=>Some(transaction),
                Err(error)=>{
                    if !store.db.is_autocommit(){state.poisoned.set(true);}
                    return Err(error.to_string());
                }
            }
        } else {
            if store.db.is_autocommit() {
                state.poisoned.set(true);
                return Err("Owned source read lost its transaction".into());
            }
            None
        };
        state.depth.set(depth+1);
        Ok(Self {db:&store.db,state,transaction,active:true})
    }
    fn finish(&mut self)->Result<(),String> {
        if !self.active { return Ok(()); }
        self.active=false;
        let depth=self.state.depth.get();
        let owned=self.transaction.is_some();
        let ownership=self.state.guard_connection(self.db);
        let release=match self.transaction.take() {
            Some(transaction)=>transaction.rollback().map_err(|error|error.to_string()),
            None=>if self.db.is_autocommit(){Err("Nested source read lost its owner transaction".into())}else{Ok(())},
        };
        self.state.depth.set(depth.saturating_sub(1));
        let no_snapshot=if owned {
            self.db.is_autocommit() && matches!(self.db.transaction_state::<&str>(None),Ok(rusqlite::TransactionState::None))
        } else {true};
        if ownership.is_err() || release.is_err() || depth==0 || (owned && (depth!=1 || !no_snapshot)) {
            self.state.poisoned.set(true);
            return Err(format!("{POISONED}: {}",release.err().unwrap_or_else(||"source read ownership changed".into())));
        }
        Ok(())
    }
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        // Unwinding must never leave a reusable old snapshot. Preserve poison
        // even if rusqlite's own subsequent Drop retry happens to succeed.
        let _=self.finish();
    }
}
impl Store {
    /// The read is coherent with its admitted owner snapshot. This deliberately
    /// does not promise latest-state semantics after releasing that snapshot;
    /// consumers that require freshness add their own post-release guard.
    pub(super) fn with_source_read<T,E>(
        &self,
        map: impl Fn(String)->E,
        read: impl FnOnce(&Self)->Result<T,E>,
    )->Result<T,E> {
        let mut lease=Lease::acquire(self).map_err(&map)?;
        let result=self.source_guard().map_err(&map).and_then(|()|read(self));
        // Do not publish result bytes (even on a successful query) if ending
        // their snapshot failed. Drop alone discards rusqlite finish errors.
        lease.finish().map_err(&map)?;
        result
    }
}

#[cfg(test)]
unsafe extern "C" fn deny_rollback(_: *mut std::ffi::c_void,action:std::ffi::c_int,first:*const std::ffi::c_char,_:*const std::ffi::c_char,_:*const std::ffi::c_char,_:*const std::ffi::c_char)->std::ffi::c_int {
    if action==rusqlite::ffi::SQLITE_TRANSACTION && !first.is_null() && unsafe {std::ffi::CStr::from_ptr(first)}.to_bytes()==b"ROLLBACK" {rusqlite::ffi::SQLITE_DENY} else {rusqlite::ffi::SQLITE_OK}
}
#[cfg(test)]
pub(super) struct RollbackFault<'a>(*mut rusqlite::ffi::sqlite3, std::marker::PhantomData<&'a Connection>);
#[cfg(test)]
impl<'a> RollbackFault<'a> {
    pub(super) fn install(store:&'a Store)->Self {
        let db=unsafe {store.db.handle()};
        assert_eq!(unsafe {rusqlite::ffi::sqlite3_set_authorizer(db,Some(deny_rollback),std::ptr::null_mut())},rusqlite::ffi::SQLITE_OK);
        Self(db,std::marker::PhantomData)
    }
}
#[cfg(test)]
impl Drop for RollbackFault<'_> {fn drop(&mut self){unsafe {rusqlite::ffi::sqlite3_set_authorizer(self.0,None,std::ptr::null_mut());}}}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{Command,ToolStatus};
    use crate::chat::cache::{SourceKey,SourceQuery};
    use crate::chat::cache::raw::{RawSourceQuery,RawSourceField,RawToolCapture};
    use crate::chat::protocol::CanonicalItemKey;
    fn item(id:&str,content:ItemContent)->Item {Item {id:id.into(),role:Role::Assistant,content,at:None,started_at:None}}
    fn populated()->Store {
        let mut store=Store::new().unwrap();
        store.apply(Event::Item(item("prose",ItemContent::Text {text:"private old bytes".into(),thinking:false,phase:Phase::Final}))).unwrap();
        store.apply(Event::RawToolCapture(RawToolCapture::Full {source:SourceKey {agent_id:None,source_id:"tool".into()},name:"Opaque".into(),input:"null".into(),command:None})).unwrap();
        store.apply(Event::Item(item("tool",ItemContent::Tool {name:"Opaque".into(),input:"null".into(),output:String::new(),status:ToolStatus::Running}))).unwrap();
        store
    }
    fn assert_all_reads_and_retry_refuse(store:&mut Store) {
        assert!(store.source_guard().is_err());
        assert!(store.with_source_read(|e|e,|_|Ok::<_,String>(())).is_err());
        assert!(store.adapter_item(&CanonicalItemKey::new(None,"tool")).is_err());
        assert!(store.raw_original(&SourceKey {agent_id:None,source_id:"tool".into()}).is_err());
        assert!(store.raw_source_page(RawSourceQuery {source:SourceKey {agent_id:None,source_id:"tool".into()},field:RawSourceField::ToolInput,offset:0,limit:64,expected_epoch:None,until:None}).is_err());
        assert!(store.source_page(SourceQuery {source:SourceKey {agent_id:None,source_id:"prose".into()},offset:0,limit:64,expected_epoch:None,until:None}).is_err());
        assert!(store.file_query(&Command::LoadFileChanges {request_id:1,after:None}).is_err());
        for command in [Command::Interrupt,Command::Refresh,Command::Answer {request_id:"safe-denial".into(),action:"deny".into(),answers:Vec::new()}] {
            assert!(matches!(store.file_query(&command),Ok(None)),"unrelated control probes do not require a source read lease");
        }
        assert!(store.window().is_err());
        let mut state=crate::chat::Snapshot::default();
        state.items.push(std::sync::Arc::new(item("old-view",ItemContent::Text {text:"private retained view".into(),thinking:false,phase:Phase::Final})));
        store.publish_source_authority(&mut state);
        assert!(state.items.is_empty());
        assert!(state.error.is_some());
        assert!(store.apply(Event::Reset {reason:"retry cannot trust this connection".into()}).is_err());
    }
    #[test]
    fn denied_rollback_never_publishes_a_successful_body_or_reuses_its_old_snapshot() {
        let mut store=populated();
        store.db.pragma_update(None,"journal_mode","WAL").unwrap();
        let path=store.directory.join("transcript.sqlite");
        let hook=RollbackFault::install(&store);
        let result=store.with_source_read(|e|e,|store| {
            let old_pending:i64=store.db.query_row("SELECT pending FROM transcript_reset_schema",[],|r|r.get(0)).unwrap();
            assert_eq!(old_pending,0);
            let writer=Connection::open(&path).unwrap();
            writer.execute("UPDATE transcript_reset_schema SET pending=1 WHERE singleton=1",[]).unwrap();
            assert!(super::super::reset::guard(&writer).is_err());
            // Our admitted snapshot still sees zero. This is NOT a fresh
            // post-release view, and must never be reused after failed release.
            store.source_guard().unwrap();
            Ok(String::from_utf8(store.field("prose",0,0,17)?).unwrap())
        });
        assert!(result.unwrap_err().contains("could not be released"));
        assert!(store.source_reads.poisoned.get());
        assert!(!store.db.is_autocommit(),"both explicit and Drop rollback attempts were actually refused");
        drop(hook);
        store.db.execute_batch("ROLLBACK").unwrap();
        assert!(store.db.is_autocommit());
        assert_all_reads_and_retry_refuse(&mut store);
    }
    #[test]
    fn tracked_nesting_reuses_one_read_and_query_errors_still_release_it() {
        let store=populated();
        let answer=store.with_source_read(|e|e,|store| {
            assert!(!store.db.is_autocommit());
            let answer=store.with_source_read(|e|e,|store|store.db.query_row("SELECT 42",[],|r|r.get::<_,i64>(0)).map_err(|e|e.to_string()))?;
            assert!(!store.db.is_autocommit(),"inner completion cannot release its parent's lease");
            Ok(answer)
        }).unwrap();
        assert_eq!(answer,42);
        assert!(store.db.is_autocommit());
        assert_eq!(store.source_reads.depth.get(),0);
        assert_eq!(store.with_source_read(|e|e,|_|Err::<(),_>("actual query refusal".to_string())).unwrap_err(),"actual query refusal");
        store.source_guard().unwrap();
        assert!(store.db.is_autocommit());
    }
    #[test]
    fn untracked_transaction_is_refused_and_cannot_be_mistaken_for_nested_read_authority() {
        let mut store=populated();
        let inherited=store.db.unchecked_transaction().unwrap();
        assert!(store.with_source_read(|e|e,|_|Ok::<_,String>("must not be published")).is_err());
        inherited.rollback().unwrap();
        assert_all_reads_and_retry_refuse(&mut store);
    }
    #[test]
    fn unwind_release_failure_poison_is_retained_after_the_sqlite_denial_is_removed() {
        let mut store=populated();
        let hook=RollbackFault::install(&store);
        let panic=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _:Result<(),String>=store.with_source_read(|e|e,|_|panic!("injected reader unwind"));
        }));
        assert!(panic.is_err());
        assert!(store.source_reads.poisoned.get());
        drop(hook);
        store.db.execute_batch("ROLLBACK").unwrap();
        assert_all_reads_and_retry_refuse(&mut store);
    }
    #[test]
    fn actual_runtime_safe_denial_routes_remain_usable_when_cache_reads_are_poisoned() {
        use crate::chat::protocol::{Adapter,ProcessSpec,Step,TransportSpec};
        struct Controls(crossbeam_channel::Sender<String>);
        impl Adapter for Controls {
            fn initialize(&mut self)->Result<Step,String>{Ok(Step::default())}
            fn receive(&mut self,_:serde_json::Value)->Result<Step,String>{Ok(Step::default())}
            fn command(&mut self,command:Command)->Result<Step,String>{
                let Command::Answer {action,..}=command else {return Err("fixture expects actual denial".into())};
                self.0.send(action).unwrap();Ok(Step::default())
            }
        }
        let spec=ProcessSpec {execution:None,messages:None,unset_environment:Default::default(),transport:TransportSpec::JsonLines,program:"/bin/cat".into(),arguments:Vec::new(),working_directory:std::env::temp_dir(),environment:Default::default()};
        let (observed,received)=crossbeam_channel::bounded(1);
        let mut initial=Step {events:vec![Event::Ready(true)],..Step::default()};
        let store=super::super::preflight(&spec,&mut initial).unwrap();
        let handle=super::super::Handle::start(spec,Box::new(Controls(observed)),initial,store,None,|_|{}).unwrap();
        {
            let store=handle.0.store.lock().unwrap();
            let hook=RollbackFault::install(&store);
            assert!(store.with_source_read(|e|e,|_|Ok::<_,String>(())).is_err());
            drop(hook);
            store.db.execute_batch("ROLLBACK").unwrap();
            assert!(store.source_guard().is_err());
        }
        let denial=Command::Answer {request_id:"pending-approval".into(),action:"deny".into(),answers:Vec::new()};
        assert!(matches!(handle.file_query(&denial),Ok(None)),"host ordinary dispatch probes cache before provider command");
        assert!(handle.file_query(&Command::LoadFileChanges {request_id:1,after:None}).is_err());
        handle.command(denial).unwrap();
        assert_eq!(received.recv_timeout(std::time::Duration::from_secs(1)).unwrap(),"deny");
        handle.close().unwrap();
    }

    #[test]
    fn implicit_untracked_snapshot_is_refused_even_while_autocommit_is_true() {
        let mut store=populated();
        let mut statement=store.db.prepare("SELECT id FROM items ORDER BY ordinal").unwrap();
        let mut rows=statement.query([]).unwrap();
        assert!(rows.next().unwrap().is_some());
        assert!(store.db.is_autocommit());
        assert_eq!(store.db.transaction_state::<&str>(None).unwrap(),rusqlite::TransactionState::Read);
        assert!(store.with_source_read(|e|e,|_|Ok::<_,String>("untracked bytes")).is_err());
        drop(rows);drop(statement);
        assert_eq!(store.db.transaction_state::<&str>(None).unwrap(),rusqlite::TransactionState::None);
        assert_all_reads_and_retry_refuse(&mut store);
    }
    #[test]
    fn rollback_success_cannot_publish_if_an_unfinished_statement_retains_the_snapshot() {
        let mut store=populated();
        let mut statement=store.db.prepare("SELECT id FROM items ORDER BY ordinal").unwrap();
        let mut rows=statement.query([]).unwrap();
        assert_eq!(store.db.transaction_state::<&str>(None).unwrap(),rusqlite::TransactionState::None);
        let result=store.with_source_read(|e|e,|_| {
            assert!(rows.next().unwrap().is_some());
            Ok("a successful rollback alone is not release proof")
        });
        assert!(result.is_err());
        assert!(store.db.is_autocommit(),"explicit rollback itself succeeded");
        assert_eq!(store.db.transaction_state::<&str>(None).unwrap(),rusqlite::TransactionState::Read);
        drop(rows);drop(statement);
        assert_all_reads_and_retry_refuse(&mut store);
    }

}
