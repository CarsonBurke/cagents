//! Durable terminal annotations in the existing guarded physical-account DB.
//! The parent Store owns namespace revocation and confirmed-session authority.
use super::Store;
use crate::{agent::{ActivityOutcome, Provider}, chat::turn_outcomes::{
    MAX_PROJECTED_TURNS, MAX_PROJECTION_BYTES, MAX_TURN_ID_BYTES, MAX_TURN_ITEM_IDS,
    MAX_TURN_REASON_BYTES, TurnIdentity, TurnIdentityKind, TurnOutcome,
    TurnOutcomeProjection, TurnUpdate, TurnWriteReceipt, VisibleItemOrigin, VisibleTurnOutcome,
}};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

pub(super) const SCHEMA: &str = "
 CREATE TABLE IF NOT EXISTS turn_records(
   session TEXT NOT NULL,kind INTEGER NOT NULL CHECK(kind IN(0,1)),identity TEXT NOT NULL,
   outcome BLOB,finished_at INTEGER CHECK(finished_at>=0),
   PRIMARY KEY(session,kind,identity),
   CHECK((outcome IS NULL AND finished_at IS NULL) OR(outcome IS NOT NULL AND finished_at IS NOT NULL)));
 CREATE TABLE IF NOT EXISTS turn_items(
   session TEXT NOT NULL,item_id TEXT NOT NULL,kind INTEGER NOT NULL,identity TEXT NOT NULL,
   PRIMARY KEY(session,item_id),
   FOREIGN KEY(session,kind,identity) REFERENCES turn_records(session,kind,identity) ON DELETE CASCADE);
 CREATE INDEX IF NOT EXISTS turn_items_owner ON turn_items(session,kind,identity);
 CREATE TABLE IF NOT EXISTS turn_revisions(session TEXT PRIMARY KEY,revision INTEGER NOT NULL CHECK(revision>=0));
 CREATE TABLE IF NOT EXISTS session_revocations(session_key BLOB PRIMARY KEY CHECK(length(session_key)=32));";

fn session_key(session: &str) -> [u8;32] {
    let mut hash=Sha256::new();
    hash.update(b"cagents-explicit-session-deletion-v1\0");
    hash.update((session.len() as u64).to_le_bytes()); hash.update(session.as_bytes());
    hash.finalize().into()
}

pub(super) fn session_revoked(db: &rusqlite::Connection, session: &str) -> Result<bool,String> {
    if !super::valid_id(session) { return Err("Invalid confirmed conversation identity".into()); }
    let key=session_key(session);
    db.query_row("SELECT 1 FROM session_revocations WHERE session_key=?1",[key.as_slice()],|row|row.get::<_,i64>(0))
        .optional().map(|row|row.is_some()).map_err(|error|error.to_string())
}

pub(super) fn reject_revoked_session(db: &rusqlite::Connection, session: &str) -> Result<(),String> {
    if session_revoked(db,session)? { return Err("This conversation's annotations were explicitly deleted; a new history epoch is required".into()); }
    Ok(())
}

/// Called inside the same transaction as deletion, after the caller proves
/// final semantic retirement. A generic Stop acknowledgement is insufficient.
pub(super) fn mark_session_revoked(db: &rusqlite::Connection, session: &str) -> Result<(),String> {
    if !super::valid_id(session) { return Err("Invalid confirmed conversation identity".into()); }
    let key=session_key(session);
    db.execute("INSERT OR IGNORE INTO session_revocations(session_key) VALUES(?1)",[key.as_slice()])
        .map(|_|()).map_err(|error|error.to_string())
}

/// The parent rejects versions above two before touching its base tables.
/// Complete the additive migration and version publication in one transaction.
pub(super) fn initialize(db: &rusqlite::Connection) -> Result<(), String> {
    let transaction = rusqlite::Transaction::new_unchecked(db,TransactionBehavior::Immediate).map_err(|error|error.to_string())?;
    // The write reservation and version read share a transaction. A future
    // migrator cannot publish schema three between our check and version two.
    let version: i64 = transaction.pragma_query_value(None,"user_version",|row|row.get(0)).map_err(|error|error.to_string())?;
    if version > 2 { return Err("Conversation annotations use a newer unsupported format".into()); }
    transaction.execute_batch(super::BASE_SCHEMA).map_err(|error|error.to_string())?;
    transaction.execute_batch(SCHEMA).map_err(|error|error.to_string())?;
    transaction.pragma_update(None,"user_version",2).map_err(|error|error.to_string())?;
    transaction.commit().map_err(|error|error.to_string())
}

pub(super) fn require_schema(db: &rusqlite::Connection) -> Result<(), String> {
    let version: i64 = db.pragma_query_value(None,"user_version",|row|row.get(0)).map_err(|error|error.to_string())?;
    if version != 2 { return Err("Turn outcomes require conversation annotation schema 2".into()); }
    Ok(())
}

fn kind_number(kind: TurnIdentityKind) -> i64 {
    match kind { TurnIdentityKind::NativeTurn => 0, TurnIdentityKind::AcceptedInput => 1 }
}

#[cfg(test)]
thread_local! {
    static BEFORE_TURN_TRANSACTION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn before_turn_transaction() {
    let hook=BEFORE_TURN_TRANSACTION.with(|slot|slot.borrow_mut().take());
    if let Some(hook)=hook { hook(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::scope::LaunchAccountScope;
    use std::{ffi::OsString,path::Path};

    fn account(root: &Path) -> LaunchAccountScope {
        std::fs::create_dir_all(root.join("home/.claude")).unwrap();
        LaunchAccountScope::resolve(Provider::Claude, root, |name| (name == "HOME").then(|| OsString::from(root.join("home")))).unwrap()
    }
    fn open(root: &Path, account: &LaunchAccountScope, session: &str) -> Store {
        let store = Store::at(root, account, session).unwrap();
        initialize(&store.db).unwrap(); store
    }
    fn identity(id: &str) -> TurnIdentity {
        TurnIdentity { session_id: "confirmed-session".into(), kind: TurnIdentityKind::AcceptedInput, id: id.into() }
    }
    fn bind(id: &str, item: &str) -> TurnUpdate {
        TurnUpdate::Bind { identity: identity(id), item_ids: vec![item.into()] }
    }
    fn finish(id: &str, outcome: ActivityOutcome, at: u64) -> TurnUpdate {
        TurnUpdate::Finish { identity: identity(id), outcome, finished_at: at }
    }
    fn origin(item: &str) -> VisibleItemOrigin { VisibleItemOrigin { item_id: item.into(), source_item_id: item.into() } }

    #[test]
    fn existing_handle_rechecks_future_format_inside_its_read_or_write_transaction() {
        for write in [false,true] {
            let root=tempfile::tempdir().unwrap(); let scope=account(root.path());
            let mut store=open(&root.path().join("annotations"),&scope,"confirmed-session");
            store.apply_turn_update(&bind("A","input-A")).unwrap();
            let future=rusqlite::Connection::open(store.directory.join("annotations.sqlite")).unwrap();
            BEFORE_TURN_TRANSACTION.with(|slot|*slot.borrow_mut()=Some(Box::new(move || {
                future.pragma_update(None,"user_version",3).unwrap();
            })));
            let rejected=if write { store.apply_turn_update(&finish("A",ActivityOutcome::Cancelled,10)).is_err() }
                else { store.project_turn_outcomes(&[origin("input-A")]).is_err() };
            assert!(rejected,"an existing handle must not read or write a future schema");
            assert_eq!(store.db.pragma_query_value(None,"user_version",|row|row.get::<_,i64>(0)).unwrap(),3);
            assert_eq!(store.db.query_row("SELECT count(*) FROM turn_records WHERE outcome IS NOT NULL",[],|row|row.get::<_,i64>(0)).unwrap(),0);
        }
    }

    #[test]
    fn cancelled_a_survives_queued_b_start_late_delivery_and_cold_restart() {
        let root = tempfile::tempdir().unwrap(); let scope = account(root.path()); let path = root.path().join("annotations");
        let mut store = open(&path, &scope, "confirmed-session");
        store.apply_turn_update(&bind("A","input-A")).unwrap();
        store.apply_turn_update(&bind("B","input-B")).unwrap();
        let receipt = store.apply_turn_update(&finish("A",ActivityOutcome::Cancelled,10)).unwrap();
        let duplicate = store.apply_turn_update(&finish("A",ActivityOutcome::Cancelled,20)).unwrap();
        assert!(!duplicate.changed); assert_eq!(duplicate.revision, receipt.revision);
        let projection = store.project_turn_outcomes(&[origin("input-A"),origin("input-B")]).unwrap();
        assert_eq!(projection.records.len(),1); assert_eq!(projection.records[0].outcome.identity.id,"A");
        assert_eq!(projection.records[0].outcome.finished_at,10);
        assert!(store.project_turn_outcomes(&[origin("input-B")]).unwrap().records.is_empty());
        drop(store);
        let mut reopened = open(&path,&scope,"confirmed-session");
        assert_eq!(reopened.project_turn_outcomes(&[origin("input-A"),origin("input-B")]).unwrap(),projection);
        reopened.apply_turn_update(&finish("B",ActivityOutcome::Succeeded,30)).unwrap();
        let projection = reopened.project_turn_outcomes(&[origin("input-A"),origin("input-B")]).unwrap();
        assert_eq!(projection.records.len(),2); assert_eq!(projection.records[0].outcome.outcome,ActivityOutcome::Cancelled);
    }

    #[test]
    fn finish_can_precede_exact_alias_binding_and_native_parts_join_explicitly() {
        let root=tempfile::tempdir().unwrap(); let scope=account(root.path()); let mut store=open(&root.path().join("annotations"),&scope,"confirmed-session");
        store.apply_turn_update(&finish("A",ActivityOutcome::Failed { reason:"exit code 3".into() },10)).unwrap();
        assert!(store.project_turn_outcomes(&[origin("actual-user:input/0")]).unwrap().records.is_empty());
        store.apply_turn_update(&bind("A","actual-user:input/0")).unwrap();
        let projection=store.project_turn_outcomes(&[VisibleItemOrigin { item_id:"opaque paged part, not parsed".into(),source_item_id:"actual-user:input/0".into() }]).unwrap();
        assert_eq!(projection.records[0].item_ids,["opaque paged part, not parsed"]);
        assert_eq!(projection.records[0].outcome.outcome.reason(),Some("exit code 3"));
    }

    #[test]
    fn alias_reassignment_or_conflicting_terminal_result_rolls_back_without_receipt() {
        let root=tempfile::tempdir().unwrap(); let scope=account(root.path()); let mut store=open(&root.path().join("annotations"),&scope,"confirmed-session");
        store.apply_turn_update(&bind("A","input-A")).unwrap(); store.apply_turn_update(&finish("A",ActivityOutcome::Cancelled,10)).unwrap();
        let before=store.project_turn_outcomes(&[origin("input-A")]).unwrap();
        assert!(store.apply_turn_update(&bind("B","input-A")).is_err());
        assert!(store.apply_turn_update(&finish("A",ActivityOutcome::Succeeded,20)).is_err());
        assert_eq!(store.project_turn_outcomes(&[origin("input-A")]).unwrap(),before);
        assert_eq!(store.db.query_row("SELECT count(*) FROM turn_records WHERE identity='B'",[],|row|row.get::<_,i64>(0)).unwrap(),0);
    }

    #[test]
    fn session_account_and_deleted_namespace_never_borrow_outcomes() {
        let root=tempfile::tempdir().unwrap(); let scope=account(root.path()); let path=root.path().join("annotations"); let mut store=open(&path,&scope,"confirmed-session");
        store.apply_turn_update(&bind("A","input-A")).unwrap(); store.apply_turn_update(&finish("A",ActivityOutcome::Cancelled,10)).unwrap();
        let other=open(&path,&scope,"different-session"); assert!(other.project_turn_outcomes(&[origin("input-A")]).unwrap().records.is_empty());
        let mut wrong=bind("A","another-input"); if let TurnUpdate::Bind { identity,.. }=&mut wrong { identity.session_id="wrong-session".into(); }
        assert!(store.apply_turn_update(&wrong).is_err());
        let root2=tempfile::tempdir().unwrap(); let scope2=account(root2.path()); let other=open(&path,&scope2,"confirmed-session");
        assert!(other.project_turn_outcomes(&[origin("input-A")]).unwrap().records.is_empty());
        super::super::purge_account_at(&path,&scope).unwrap();
        assert!(store.project_turn_outcomes(&[origin("input-A")]).is_err()); assert!(store.apply_turn_update(&bind("B","input-B")).is_err());
    }

    #[test]
    fn metadata_pages_and_byte_pressure_preserve_all_durable_results() {
        let root=tempfile::tempdir().unwrap(); let scope=account(root.path()); let mut store=open(&root.path().join("annotations"),&scope,"confirmed-session");
        for index in 0..160 {
            store.apply_turn_update(&bind(&format!("turn-{index}"),&format!("item-{index}"))).unwrap();
            store.apply_turn_update(&finish(&format!("turn-{index}"),ActivityOutcome::Failed { reason:"x".repeat(MAX_TURN_REASON_BYTES) },index+1)).unwrap();
        }
        let first: Vec<_>=(0..128).map(|index|origin(&format!("item-{index}"))).collect();
        let projection=store.project_turn_outcomes(&first).unwrap();
        assert!(!projection.complete); assert!(projection.records.len()<128); projection.validate().unwrap();
        assert!(postcard::to_stdvec(&projection).unwrap().len()<=MAX_PROJECTION_BYTES);
        let last: Vec<_>=(128..160).map(|index|origin(&format!("item-{index}"))).collect();
        let projection=store.project_turn_outcomes(&last).unwrap();assert!(projection.complete);assert_eq!(projection.records.len(),32);
        assert_eq!(projection.records[31].outcome.identity.id,"turn-159");
        assert_eq!(store.db.query_row("SELECT count(*) FROM turn_records",[],|row|row.get::<_,i64>(0)).unwrap(),160);
    }

    #[test]
    fn failed_revision_commit_has_no_false_ack_and_replay_can_commit_once() {
        let root=tempfile::tempdir().unwrap(); let scope=account(root.path()); let mut store=open(&root.path().join("annotations"),&scope,"confirmed-session");
        store.db.execute_batch("CREATE TRIGGER fixture_turn_error BEFORE INSERT ON turn_revisions BEGIN SELECT RAISE(FAIL,'fixture durable turn write failure');END;").unwrap();
        assert!(store.apply_turn_update(&finish("A",ActivityOutcome::Cancelled,10)).is_err());
        assert_eq!(store.db.query_row("SELECT count(*) FROM turn_records",[],|row|row.get::<_,i64>(0)).unwrap(),0);
        store.db.execute_batch("DROP TRIGGER fixture_turn_error").unwrap();
        assert!(store.apply_turn_update(&finish("A",ActivityOutcome::Cancelled,10)).unwrap().changed);
        assert!(!store.apply_turn_update(&finish("A",ActivityOutcome::Cancelled,10)).unwrap().changed);
    }

    #[test]
    fn schema_two_migration_preserves_accepted_originals_and_refuses_future_versions() {
        use crate::chat::annotations::{OriginalPrompt,OriginalSettings,Setting};
        let root=tempfile::tempdir().unwrap(); let scope=account(root.path()); let mut store=open(&root.path().join("annotations"),&scope,"confirmed-session");
        let prompt=OriginalPrompt { user_id:"accepted-user".into(),turn_id:"accepted-input".into(),text_bytes:8,attachments:vec![],
            settings:OriginalSettings { model:Setting::Value("native-model".into()),reasoning:Setting::Value("low".into()),fast:Setting::Unsupported,permission:Setting::Value("ask".into()) } };
        store.accept(&prompt,"original",&[],&mut []).unwrap();
        store.db.execute_batch("DROP TABLE turn_items;DROP TABLE turn_records;DROP TABLE turn_revisions;PRAGMA user_version=1;").unwrap();
        initialize(&store.db).unwrap();
        assert_eq!(store.original(Some("accepted-user"),0,32).unwrap().text,"original");
        assert_eq!(store.db.pragma_query_value(None,"user_version",|row|row.get::<_,i64>(0)).unwrap(),2);
        store.db.pragma_update(None,"user_version",3).unwrap();
        assert!(initialize(&store.db).is_err());
        assert!(store.apply_turn_update(&finish("A",ActivityOutcome::Cancelled,10)).is_err());
        assert!(store.project_turn_outcomes(&[origin("item-A")]).is_err());
        assert_eq!(store.db.pragma_query_value(None,"user_version",|row|row.get::<_,i64>(0)).unwrap(),3);
    }

    #[test]
    fn failed_additive_migration_keeps_schema_one_and_rolls_back_new_tables() {
        let db=rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA user_version=1;CREATE VIEW turn_items AS SELECT 1 AS fixture;").unwrap();
        assert!(initialize(&db).is_err());
        assert_eq!(db.pragma_query_value(None,"user_version",|row|row.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(db.query_row("SELECT count(*) FROM sqlite_master WHERE name='turn_records'",[],|row|row.get::<_,i64>(0)).unwrap(),0);
    }

    #[test]
    fn concurrent_future_migration_wins_without_schema_downgrade() {
        use std::sync::atomic::{AtomicBool,Ordering};
        static WAITING: AtomicBool=AtomicBool::new(false);
        WAITING.store(false,Ordering::SeqCst);
        let root=tempfile::tempdir().unwrap();let path=root.path().join("migration.sqlite");
        let old=rusqlite::Connection::open(&path).unwrap();old.pragma_update(None,"user_version",1).unwrap();
        old.busy_handler(Some(|attempt| { WAITING.store(true,Ordering::SeqCst);std::thread::sleep(std::time::Duration::from_millis(1));attempt<2000 })).unwrap();
        let mut future=rusqlite::Connection::open(&path).unwrap();
        let transaction=future.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        transaction.pragma_update(None,"user_version",3).unwrap();
        let worker=std::thread::spawn(move || initialize(&old));
        let started=std::time::Instant::now();
        while !WAITING.load(Ordering::SeqCst) {
            assert!(started.elapsed()<std::time::Duration::from_secs(2),"initializer never reached the held migration lock");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        transaction.commit().unwrap();
        assert!(worker.join().unwrap().is_err());
        assert_eq!(future.pragma_query_value(None,"user_version",|row|row.get::<_,i64>(0)).unwrap(),3);
        assert_eq!(future.query_row("SELECT count(*) FROM sqlite_master WHERE name='turn_records'",[],|row|row.get::<_,i64>(0)).unwrap(),0);
    }

    #[test]
    fn explicit_session_purge_is_permanent_idempotent_and_does_not_revoke_other_sessions() {
        let root=tempfile::tempdir().unwrap(); let scope=account(root.path()); let path=root.path().join("annotations");
        let mut deleted=open(&path,&scope,"confirmed-session");
        deleted.apply_turn_update(&bind("A","input-A")).unwrap(); deleted.apply_turn_update(&finish("A",ActivityOutcome::Cancelled,10)).unwrap();
        let mut other=open(&path,&scope,"other-session");
        let mut other_bind=bind("B","input-B"); let mut other_finish=finish("B",ActivityOutcome::Succeeded,20);
        for update in [&mut other_bind,&mut other_finish] { match update { TurnUpdate::Bind { identity,.. } | TurnUpdate::Finish { identity,.. } => identity.session_id="other-session".into() } }
        other.apply_turn_update(&other_bind).unwrap();other.apply_turn_update(&other_finish).unwrap();
        let before=other.project_turn_outcomes(&[origin("input-B")]).unwrap();
        // Fixture storage operation represents the caller having already
        // proved final semantic retirement. Stop alone is not this proof.
        deleted.purge_for_test().unwrap(); deleted.purge_for_test().unwrap();
        assert!(deleted.turn_session_revoked("confirmed-session").unwrap());
        assert!(deleted.apply_turn_update(&finish("A",ActivityOutcome::Cancelled,10)).is_err());
        assert!(deleted.project_turn_outcomes(&[origin("input-A")]).is_err());
        assert!(Store::at(&path,&scope,"confirmed-session").is_err());
        assert_eq!(other.project_turn_outcomes(&[origin("input-B")]).unwrap(),before);
        let key: Vec<u8>=deleted.db.query_row("SELECT session_key FROM session_revocations",[],|row|row.get(0)).unwrap();
        assert_eq!(key.len(),32);assert_ne!(key.as_slice(),b"confirmed-session");
        assert_eq!(deleted.db.query_row("SELECT count(*) FROM turn_records WHERE session='confirmed-session'",[],|row|row.get::<_,i64>(0)).unwrap(),0);
    }

    #[test]
    fn deleted_session_cannot_publish_or_rebind_pending_original_intents() {
        let root=tempfile::tempdir().unwrap();let scope=account(root.path());let path=root.path().join("annotations");
        let mut deleted=open(&path,&scope,"confirmed-session");
        let mut pending=Store::pending_for_test(&path,&scope).unwrap();
        let mut unknown=pending.capture_text("unconfirmed input").unwrap();
        pending.prepare_command("unconfirmed",None,&mut unknown,&mut[]).unwrap();drop(unknown);
        deleted.purge_for_test().unwrap();
        assert!(pending.associate_command("unconfirmed","confirmed-session").is_err());
        let mut text=pending.capture_text("must not publish").unwrap();
        assert!(pending.prepare_command("after-delete",Some("confirmed-session"),&mut text,&mut[]).is_err());
        assert!(deleted.accept_prepared("unconfirmed","user","turn",&[]).is_err());
        assert!(pending.prepared("unconfirmed").unwrap().expected_session.is_none());
        assert!(pending.prepared("after-delete").is_err());
    }
}

fn identity_kind(kind: i64) -> Result<TurnIdentityKind, String> {
    match kind { 0 => Ok(TurnIdentityKind::NativeTurn), 1 => Ok(TurnIdentityKind::AcceptedInput),
        _ => Err("Stored turn has an unsupported identity proof".into()) }
}

fn revision(db: &rusqlite::Connection, session: &str) -> Result<u64, String> {
    let revision = db.query_row("SELECT revision FROM turn_revisions WHERE session=?1", [session], |row| row.get::<_,i64>(0))
        .optional().map_err(|error| error.to_string())?.unwrap_or(0);
    u64::try_from(revision).map_err(|_| "Invalid durable turn revision".into())
}

impl Store {
    /// A tombstone is only a storage disposition. The host must separately
    /// prove the exact source's final retirement before acknowledging its tail.
    pub(crate) fn turn_session_revoked(&self, session: &str) -> Result<bool,String> {
        self.revalidate()?;
        let transaction=rusqlite::Transaction::new_unchecked(&self.db,TransactionBehavior::Deferred).map_err(|error|error.to_string())?;
        require_schema(&transaction)?;
        let revoked=session_revoked(&transaction,session)?;
        transaction.commit().map_err(|error|error.to_string())?;
        self.revalidate()?; Ok(revoked)
    }
    /// A semantic receipt exists only after the transaction commits. Retained
    /// helper replays must call this even if their host cursor already advanced.
    pub(crate) fn apply_turn_update(&mut self, update: &TurnUpdate) -> Result<TurnWriteReceipt, String> {
        self.revalidate()?;
        update.validate()?;
        let identity = update.identity();
        if self.confirmed()? != identity.session_id {
            return Err("Turn result belongs to another confirmed conversation".into());
        }
        match (self.scope.provider, identity.kind) {
            (Provider::Codex, TurnIdentityKind::NativeTurn) | (Provider::Claude, TurnIdentityKind::AcceptedInput) => {},
            _ => return Err("This provider has no confirmed proof for that turn identity".into()),
        }
        let kind = kind_number(identity.kind);
        #[cfg(test)] before_turn_transaction();
        let transaction = self.db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|error| error.to_string())?;
        require_schema(&transaction)?;
        reject_revoked_session(&transaction,&identity.session_id)?;
        let mut changed = transaction.execute(
            "INSERT OR IGNORE INTO turn_records(session,kind,identity) VALUES(?1,?2,?3)",
            params![identity.session_id, kind, identity.id],
        ).map_err(|error| error.to_string())? != 0;
        match update {
            TurnUpdate::Bind { item_ids, .. } => {
                for item in item_ids {
                    let previous = transaction.query_row(
                        "SELECT kind,identity FROM turn_items WHERE session=?1 AND item_id=?2",
                        params![identity.session_id,item], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)),
                    ).optional().map_err(|error| error.to_string())?;
                    match previous {
                        Some((previous_kind,previous_id)) if previous_kind == kind && previous_id == identity.id => {},
                        Some(_) => return Err("A transcript item is already bound to another confirmed turn".into()),
                        None => { transaction.execute(
                            "INSERT INTO turn_items(session,item_id,kind,identity) VALUES(?1,?2,?3,?4)",
                            params![identity.session_id,item,kind,identity.id],
                        ).map_err(|error| error.to_string())?; changed = true; },
                    }
                }
            }
            TurnUpdate::Finish { outcome, finished_at, .. } => {
                let previous: Option<Vec<u8>> = transaction.query_row(
                    "SELECT outcome FROM turn_records WHERE session=?1 AND kind=?2 AND identity=?3",
                    params![identity.session_id,kind,identity.id], |row| row.get(0),
                ).map_err(|error| error.to_string())?;
                if let Some(previous) = previous {
                    if previous.len() > MAX_TURN_REASON_BYTES + 256 { return Err("Stored turn outcome exceeds its byte budget".into()); }
                    let previous: ActivityOutcome = postcard::from_bytes(&previous).map_err(|error| error.to_string())?;
                    if &previous != outcome { return Err("A conflicting terminal result cannot replace a saved turn outcome".into()); }
                    // Preserve the first observation time on a duplicate
                    // terminal delivery, including replay after host restart.
                } else {
                    let finished_at = i64::try_from(*finished_at).map_err(|_| "Invalid turn completion timestamp".to_owned())?;
                    let bytes = postcard::to_stdvec(outcome).map_err(|error| error.to_string())?;
                    transaction.execute(
                        "UPDATE turn_records SET outcome=?4,finished_at=?5 WHERE session=?1 AND kind=?2 AND identity=?3",
                        params![identity.session_id,kind,identity.id,bytes,finished_at],
                    ).map_err(|error| error.to_string())?;
                    changed = true;
                }
            }
        }
        let old = revision(&transaction, &identity.session_id)?;
        let next = if changed { old.checked_add(1).filter(|next| *next <= i64::MAX as u64).ok_or("Turn outcome revision exhausted")? } else { old };
        if changed { transaction.execute(
            "INSERT INTO turn_revisions(session,revision) VALUES(?1,?2) ON CONFLICT(session) DO UPDATE SET revision=excluded.revision",
            params![identity.session_id,next as i64],
        ).map_err(|error| error.to_string())?; }
        transaction.commit().map_err(|error| error.to_string())?;
        self.revalidate()?;
        Ok(TurnWriteReceipt { revision: next, changed })
    }

    /// Cheap invalidation probe for an unchanged current transcript window.
    /// The same snapshot guards version and revocation before exposing a revision.
    pub(crate) fn turn_outcome_revision(&self) -> Result<u64,String> {
        self.revalidate()?;
        let session=self.confirmed()?;
        let transaction=rusqlite::Transaction::new_unchecked(&self.db,TransactionBehavior::Deferred).map_err(|error|error.to_string())?;
        require_schema(&transaction)?;
        reject_revoked_session(&transaction,session)?;
        let revision=revision(&transaction,session)?;
        transaction.commit().map_err(|error|error.to_string())?;
        self.revalidate()?;
        Ok(revision)
    }

    /// One indexed batched join against a bounded visible transcript window.
    /// No full-history outcomes or per-item request fan-out enters memory.
    pub(crate) fn project_turn_outcomes(&self, origins: &[VisibleItemOrigin]) -> Result<TurnOutcomeProjection, String> {
        self.revalidate()?;
        let session = self.confirmed()?;
        if origins.len() > MAX_TURN_ITEM_IDS { return Err("Visible turn window exceeds its identity budget".into()); }
        let mut visible = HashSet::new();
        let mut sources = HashSet::new();
        for origin in origins {
            if origin.item_id.is_empty() || origin.item_id.len() > MAX_TURN_ID_BYTES + 256
                || origin.source_item_id.is_empty() || origin.source_item_id.len() > MAX_TURN_ID_BYTES
                || !visible.insert(origin.item_id.as_str()) {
                return Err("Invalid or duplicate visible transcript origin".into());
            }
            sources.insert(origin.source_item_id.as_str());
        }
        #[cfg(test)] before_turn_transaction();
        let transaction = rusqlite::Transaction::new_unchecked(&self.db, TransactionBehavior::Deferred).map_err(|error| error.to_string())?;
        require_schema(&transaction)?;
        reject_revoked_session(&transaction,session)?;
        let mut projection = TurnOutcomeProjection { revision: revision(&transaction,session)?, records: vec![], complete: true };
        let mut rows_by_item = HashMap::new();
        if !sources.is_empty() {
            let placeholders = (2..=sources.len()+1).map(|index|format!("?{index}")).collect::<Vec<_>>().join(",");
            let query = format!("SELECT i.item_id,r.kind,r.identity,length(CAST(r.identity AS BLOB)),length(r.outcome),r.outcome,r.finished_at FROM turn_items i JOIN turn_records r ON r.session=i.session AND r.kind=i.kind AND r.identity=i.identity WHERE i.session=?1 AND i.item_id IN({placeholders}) AND r.outcome IS NOT NULL");
            let mut statement = transaction.prepare(&query).map_err(|error| error.to_string())?;
            let mut rows = statement.query(rusqlite::params_from_iter(std::iter::once(session).chain(sources.iter().copied()))).map_err(|error| error.to_string())?;
            while let Some(row) = rows.next().map_err(|error| error.to_string())? {
                let id_bytes: usize = row.get(3).map_err(|error| error.to_string())?;
                let bytes: usize = row.get(4).map_err(|error| error.to_string())?;
                if id_bytes > MAX_TURN_ID_BYTES || bytes > MAX_TURN_REASON_BYTES + 256 { return Err("Stored turn metadata exceeds its byte budget".into()); }
                let outcome: Vec<u8> = row.get(5).map_err(|error| error.to_string())?;
                let finished_at: i64 = row.get(6).map_err(|error| error.to_string())?;
                let outcome = TurnOutcome { identity: TurnIdentity { session_id: session.to_owned(),
                    kind: identity_kind(row.get(1).map_err(|error| error.to_string())?)?, id: row.get(2).map_err(|error| error.to_string())? },
                    outcome: postcard::from_bytes(&outcome).map_err(|error| error.to_string())?,
                    finished_at: u64::try_from(finished_at).map_err(|_| "Invalid stored turn completion time".to_owned())? };
                outcome.validate()?;
                rows_by_item.insert(row.get::<_,String>(0).map_err(|error| error.to_string())?, outcome);
            }
        }
        let mut indices = HashMap::<TurnIdentity,usize>::new();
        let mut budget = 32usize;
        for origin in origins {
            let Some(outcome) = rows_by_item.get(&origin.source_item_id) else { continue; };
            let additional = 8 + origin.item_id.len() + if indices.contains_key(&outcome.identity) { 0 }
                else { 64 + outcome.identity.session_id.len() + outcome.identity.id.len() + outcome.outcome.reason().map_or(0,str::len) };
            if budget.saturating_add(additional) > MAX_PROJECTION_BYTES { projection.complete = false; continue; }
            let index = match indices.get(&outcome.identity) {
                Some(index) => *index,
                None if projection.records.len() < MAX_PROJECTED_TURNS => {
                    let index = projection.records.len(); indices.insert(outcome.identity.clone(),index);
                    projection.records.push(VisibleTurnOutcome { outcome: outcome.clone(), item_ids: vec![] }); index
                },
                None => { projection.complete = false; continue; },
            };
            projection.records[index].item_ids.push(origin.item_id.clone()); budget += additional;
        }
        projection.validate()?;
        transaction.commit().map_err(|error| error.to_string())?;
        self.revalidate()?;
        Ok(projection)
    }
}
