//! Reset admission survives a failed namespace transaction or a future reopen.
//! Every source body lives in the same SQLite connection. Keep the durable
//! intent until ALL namespaces have been reset in one Immediate transaction.
use super::*;
use rusqlite::{Transaction, TransactionBehavior};

const VERSION: i64 = 1;
const SCHEMA: &str = "CREATE TABLE transcript_reset_schema(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL,pending INTEGER NOT NULL CHECK(pending IN(0,1)))";
const UNAVAILABLE: &str = "Conversation Reset is not acknowledged; saved source reads remain unavailable until its atomic retry succeeds";
fn error(error: impl ToString) -> String { error.to_string() }
fn normalized(sql: &str) -> String {
    sql.chars().filter(|c| !c.is_ascii_whitespace()).flat_map(char::to_lowercase).collect()
}
/// This joins raw/child initialization BEFORE the old raw owner is rotated.
/// No implicit upgrade of an unknown layout, version or unresolved intent.
pub(super) fn initialize_in(tx: &Transaction<'_>) -> Result<(), String> {
    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='transcript_reset_schema')", [], |r| r.get(0)).map_err(error)?;
    if !exists {
        tx.execute_batch(SCHEMA).map_err(error)?;
        tx.execute("INSERT INTO transcript_reset_schema VALUES(1,?1,0)", [VERSION]).map_err(error)?;
    }
    let sql: Option<String> = tx.query_row("SELECT CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql ELSE NULL END FROM sqlite_master WHERE type='table' AND name='transcript_reset_schema'", [], |r| r.get(0)).map_err(error)?;
    if sql.as_deref().is_none_or(|sql| normalized(sql)!=normalized(SCHEMA)) {
        return Err("Conversation Reset schema is unsupported; no source ownership was changed".into());
    }
    guard(tx)
}
fn pending(db: &Connection) -> Result<bool, String> {
    let (version, pending): (i64,i64) = db.query_row("SELECT version,pending FROM transcript_reset_schema WHERE singleton=1", [], |r| Ok((r.get(0)?,r.get(1)?))).map_err(error)?;
    if version != VERSION || !(0..=1).contains(&pending) {
        return Err("Conversation Reset schema changed; source access was refused".into());
    }
    Ok(pending==1)
}
pub(super) fn guard(db: &Connection) -> Result<(), String> {
    if pending(db)? { return Err(UNAVAILABLE.into()); }
    Ok(())
}
impl Store {
    pub(super) fn source_guard(&self) -> Result<(), String> {
        if self.reset_pending { return Err(UNAVAILABLE.into()); }
        raw::guard(&self.db, self.raw_sources.nonce)
    }
    /// Even an unsuccessful Reset invalidates already-published source epochs.
    /// Metadata still describes the process; no retained private body is republished.
    pub(in crate::chat::runtime) fn publish_source_authority(&mut self, state: &mut crate::chat::Snapshot) {
        if let Err(reason) = self.source_guard() {
            if !self.reset_pending {
                self.reset_pending = true;
                self.canonical_generation = uuid::Uuid::new_v4();
                self.source_incarnation = uuid::Uuid::new_v4();
            }
            self.dirty = true;
            state.items.clear();
            for child in &mut state.subagents { child.items.clear(); }
            state.source_parts.clear();
            state.raw_source_parts.clear();
            state.file_changes = Default::default();
            state.has_earlier = false;
            state.has_later = false;
            state.error = Some(reason);
            self.presentation = None;
            self.presented_children.clear();
        }
        state.source_incarnation = Some(self.source_incarnation);
    }
    pub(super) fn reset_all(&mut self, reason: String) -> Result<Vec<Event>, String> {
        // Revoke local readers BEFORE even attempting the durable reservation.
        // Failed intent persistence is an explicit unacknowledged Reset, never
        // a claim that a future reopen can discover an unwritten intent.
        self.reset_pending = true;
        self.canonical_generation = uuid::Uuid::new_v4();
        self.source_incarnation = uuid::Uuid::new_v4();
        self.dirty = true;
        self.presentation = None;
        self.presented_generation = None;
        self.presented_children.clear();
        let result = (|| -> Result<raw::State, String> {
            {
                let tx = self.db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(error)?;
                raw::guard_owner(&tx, self.raw_sources.nonce)?;
                child_tools::reset_guard(&tx, self.raw_sources.nonce)?;
                let _already_pending = pending(&tx)?;
                tx.execute("UPDATE transcript_reset_schema SET pending=1 WHERE singleton=1", []).map_err(error)?;
                tx.commit().map_err(error)?;
            }
            let tx = self.db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(error)?;
            raw::guard_owner(&tx, self.raw_sources.nonce)?;
            child_tools::reset_guard(&tx, self.raw_sources.nonce)?;
            if !pending(&tx)? { return Err("Conversation Reset admission disappeared".into()); }
            child_tools::reset_in(&tx, self.raw_sources.nonce)?;
            source::reset_in(&tx, self.raw_sources.nonce)?;
            file_changes::reset_in(&tx, self.raw_sources.nonce)?;
            tx.execute("DELETE FROM items", []).map_err(error)?;
            // Rotate the owner LAST; every earlier namespace checks the same
            // admitted owner while this transaction remains unpublished.
            let raw = raw::reset_in(&tx, self.raw_sources.nonce)?;
            tx.execute("UPDATE transcript_reset_schema SET pending=0 WHERE singleton=1", []).map_err(error)?;
            tx.commit().map_err(error)?;
            Ok(raw)
        })();
        let raw = result.map_err(|cause| format!("{UNAVAILABLE}: {cause}"))?;
        self.raw_sources = raw;
        self.reset_pending = false;
        self.pending_input = None;
        self.pending_text = None;
        self.pending_child_tool = None;
        self.pending_child_source = None;
        self.child_tools_unavailable = None;
        self.child_tools_have_revocations = false;
        self.source_revision = 0;
        self.file_revision = self.file_revision.saturating_add(1);
        self.child_file_ordinal = 0;
        self.child_files_seen = false;
        self.file_index_incomplete = false;
        self.file_keys = 0;
        self.applied_files = 0;
        self.presented_file_changes = None;
        self.changed = Some(Default::default());
        self.end = None;
        self.first = None;
        self.newest = None;
        self.forward = None;
        self.paging = false;
        self.provider_earlier = false;
        self.cached_earlier = false;
        self.pending_earlier = None;
        self.latest_boundary = None;
        Ok(vec![Event::Reset { reason }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{Command, ToolStatus, FileChange, ChangeKind};
    use crate::chat::cache::{SourceKey, SourceQuery};
    use crate::chat::cache::raw::{RawSourceQuery, RawSourceField, RawToolCapture};
    use crate::chat::protocol::CanonicalItemKey;
    fn item(id: &str, content: ItemContent) -> Item {
        Item { id: id.into(), role: Role::Assistant, content, at: Some(1), started_at: Some(1) }
    }
    fn tool(id: &str) -> Item {
        item(id, ItemContent::Tool { name: "Opaque".into(), input: "{\"private\":true}".into(), output: "private output".into(), status: ToolStatus::Running })
    }
    fn text(id: &str) -> Item { item(id, ItemContent::Text { text: "private prose".into(), thinking: false, phase: Phase::Final }) }
    fn populate(store: &mut Store) {
        store.apply(Event::RawToolCapture(RawToolCapture::Full { source: SourceKey {agent_id: None, source_id: "tool".into()}, name: "Opaque".into(), input: "{\"private\":true}".into(), command: None })).unwrap();
        store.apply(Event::Item(tool("tool"))).unwrap();
        store.apply(Event::Item(text("prose"))).unwrap();
        store.apply(Event::SubagentItem { id: "child".into(), item: tool("ctool") }).unwrap();
        store.apply(Event::SubagentItem { id: "child".into(), item: text("cprose") }).unwrap();
        store.apply(Event::Item(item("patch", ItemContent::Changes { changes: vec![FileChange { path: "private.txt".into(), kind: ChangeKind::Added, diff: "@@ -0,0 +1 @@\n+private\n".into() }], status: ToolStatus::Succeeded }))).unwrap();
    }
    fn text_query(child: bool) -> SourceQuery {
        SourceQuery {source: SourceKey {agent_id: child.then(|| "child".into()), source_id: if child {"cprose"} else {"prose"}.into()}, offset: 0, limit: 64, expected_epoch: None, until: None}
    }
    fn raw_query() -> RawSourceQuery {
        RawSourceQuery {source: SourceKey {agent_id: None,source_id: "tool".into()}, field: RawSourceField::ToolInput, offset: 0, limit: 64, expected_epoch: None, until: None}
    }
    const BODIES: &[&str] = &["items","chunks","source_epochs","child_sources","child_source_chunks","child_tools","child_tool_chunks","raw_tool_sources","raw_tool_chunks","file_edits","file_totals","file_paths"];
    fn counts(store: &Store) -> Vec<usize> {
        BODIES.iter().map(|table|store.db.query_row(&format!("SELECT count(*) FROM {table}"),[],|r|r.get(0)).unwrap()).collect()
    }
    fn assert_unavailable(store: &mut Store) {
        assert!(store.source_page(text_query(false)).is_err());
        assert!(store.source_page(text_query(true)).is_err());
        assert!(store.raw_source_page(raw_query()).is_err());
        assert!(store.adapter_item(&CanonicalItemKey::new(None, "tool")).is_err());
        assert!(store.adapter_item(&CanonicalItemKey::new(Some("child"), "ctool")).is_err());
        assert!(store.file_query(&Command::LoadFileChanges {request_id: 1,after: None}).is_err());
        assert!(store.window().is_err());
        let mut state = crate::chat::Snapshot::default();
        state.items.push(std::sync::Arc::new(tool("previous publication")));
        store.publish_source_authority(&mut state);
        assert!(state.items.is_empty());
        assert_eq!(state.source_incarnation,Some(store.source_incarnation));
        assert!(state.error.is_some());
        assert!(store.publish_source_parts(&mut state).is_err());
        assert!(store.publish_files(&mut state).is_err());
    }
    #[test]
    fn a_failure_in_each_namespace_rolls_back_all_bodies_and_durably_refuses_every_reader() {
        for table in ["child_tools","child_sources","source_epochs","file_edits","file_totals","file_paths","items","raw_tool_sources"] {
            let mut store = Store::new().unwrap();
            populate(&mut store);
            let before = counts(&store);
            let incarnation = store.source_incarnation;
            store.pending_text = Some(("prose".into(),Role::Assistant,false,"uncommitted text".into()));
            store.pending_input = Some(("tool".into(),16,"uncommitted input".into()));
            store.db.execute_batch(&format!("CREATE TRIGGER refuse_reset BEFORE DELETE ON {table} BEGIN SELECT RAISE(FAIL,'injected reset failure');END;")).unwrap();
            let error = store.apply(Event::Reset {reason: "provider reset".into()}).unwrap_err();
            assert!(error.contains("not acknowledged"));
            assert_eq!(counts(&store),before,"{table} failure must roll back every body namespace");
            assert_ne!(store.source_incarnation,incarnation);
            assert!(store.pending_text.is_some() && store.pending_input.is_some());
            assert_unavailable(&mut store);
            // A fresh database handle cannot clear the durable gate or rotate
            // ownership merely by attempting the normal initializer.
            let path: String = store.db.query_row("SELECT file FROM pragma_database_list WHERE name='main'", [], |r| r.get(0)).unwrap();
            let mut reopened = Connection::open(path).unwrap();
            let owner: String = reopened.query_row("SELECT nonce FROM raw_source_schema", [], |r|r.get(0)).unwrap();
            assert!(raw::initialize_with(&mut reopened, child_tools::initialize_in).is_err());
            assert_eq!(reopened.query_row("SELECT nonce FROM raw_source_schema",[],|r|r.get::<_,String>(0)).unwrap(),owner);
            assert!(guard(&reopened).is_err());
            drop(reopened);
            store.db.execute_batch("DROP TRIGGER refuse_reset").unwrap();
            store.apply(Event::Reset {reason: "retry".into()}).unwrap();
            assert!(counts(&store).iter().all(|count|*count==0));
            assert!(!pending(&store.db).unwrap());
            assert!(store.pending_input.is_none() && store.pending_text.is_none());
            store.source_guard().unwrap();
            populate(&mut store);
            assert!(store.source_page(text_query(false)).is_ok());
            assert!(store.raw_source_page(raw_query()).is_ok());
        }
    }
    #[test]
    fn intent_refusal_keeps_bytes_and_local_gate_without_claiming_durable_reset() {
        let mut store = Store::new().unwrap();
        populate(&mut store);
        let before = counts(&store);
        store.db.execute_batch("CREATE TRIGGER refuse_intent BEFORE UPDATE OF pending ON transcript_reset_schema BEGIN SELECT RAISE(FAIL,'cannot record intent');END;").unwrap();
        assert!(store.apply(Event::Reset {reason: "refused".into()}).is_err());
        assert!(!pending(&store.db).unwrap(),"no durable intent was acknowledged");
        assert_eq!(counts(&store),before);
        assert_unavailable(&mut store);
        store.db.execute_batch("DROP TRIGGER refuse_intent").unwrap();
        store.apply(Event::Reset {reason: "retry".into()}).unwrap();
        store.source_guard().unwrap();
    }
    #[test]
    fn future_reset_or_child_schema_refuses_before_intent_and_preserves_body_bytes() {
        for table in ["transcript_reset_schema", "child_tool_schema"] {
            let mut store = Store::new().unwrap();
            populate(&mut store);
            let before=counts(&store);
            store.db.execute(&format!("UPDATE {table} SET version=9"),[]).unwrap();
            assert!(store.apply(Event::Reset {reason: "refused".into()}).is_err());
            assert_eq!(counts(&store),before);
            assert_eq!(store.db.query_row("SELECT pending FROM transcript_reset_schema",[],|r|r.get::<_,i64>(0)).unwrap(),0);
            assert_unavailable(&mut store);
        }
    }
    #[test]
    fn actual_runtime_publications_revoke_old_bodies_after_a_failed_same_session_reset() {
        use crate::chat::protocol::{Adapter,ProcessSpec,Step,TransportSpec};
        struct ResetAdapter;
        impl Adapter for ResetAdapter {
            fn initialize(&mut self) -> Result<Step,String> { Ok(Step { events: vec![Event::Session {id: "same-native-session".into(),title:None},Event::Ready(true)],..Step::default() }) }
            fn command(&mut self,_:Command)->Result<Step,String> { Ok(Step {events:vec![Event::Reset {reason:"same session reset".into()}],..Step::default()}) }
            fn receive(&mut self,_:serde_json::Value)->Result<Step,String> {Ok(Step::default())}
        }
        let spec=ProcessSpec { execution:None,messages:None,unset_environment:Default::default(),transport:TransportSpec::JsonLines,program:"/bin/cat".into(),arguments:Vec::new(),working_directory:std::env::temp_dir(),environment:Default::default() };
        let mut adapter:Box<dyn Adapter>=Box::new(ResetAdapter);
        let mut initial=adapter.initialize().unwrap();
        let mut store=super::super::preflight(&spec,&mut initial).unwrap();
        populate(&mut store);
        let handle=super::super::Handle::start(spec,adapter,initial,store,None,|_|{}).unwrap();
        let baseline=handle.snapshot(None).unwrap();
        assert!(!baseline.items.is_empty());
        let token=baseline.source_incarnation.unwrap();
        handle.0.store.lock().unwrap().db.execute_batch("CREATE TRIGGER refuse_runtime_reset BEFORE DELETE ON file_edits BEGIN SELECT RAISE(FAIL,'reset body failure');END;").unwrap();
        assert!(handle.command(Command::Refresh).is_err());
        let current=handle.snapshot(None).unwrap();
        assert_eq!(current.session_id,baseline.session_id);
        assert_ne!(current.source_incarnation,Some(token));
        assert!(current.items.is_empty() && current.source_parts.is_empty() && current.raw_source_parts.is_empty());
        assert!(current.subagents.iter().all(|child|child.items.is_empty()));
        let response=handle.presentation(crate::model::TerminalId::new(),Some(baseline.generation)).unwrap();
        let snapshot=match response {
            crate::terminal_ipc::ServerResponse::ChatSnapshot {snapshot:Some(snapshot),..}=>snapshot,
            crate::terminal_ipc::ServerResponse::ChatUpdate {update,..}=>update.apply(&baseline).unwrap(),
            _=>panic!("source invalidation publication expected"),
        };
        assert_eq!(snapshot.source_incarnation,current.source_incarnation);
        assert!(snapshot.items.is_empty() && snapshot.raw_source_parts.is_empty());
        let _=handle.close();
    }

}
