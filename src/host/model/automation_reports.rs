//! Immutable membership-removal receipts for new read-only report presentations.
//! These rows grant no provider authority and never change execution identities.
use super::*;
use crate::{automations::{ArchiveReport, archive::ArchivedPaneReadiness}, chat::{ArchiveReportReference, Session}, model::{AgentId, CardId}, workspace_model::views::{Pane, Scope}};

const MAX_DESCRIPTOR: usize = 3 * 1024 * 1024;

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let current: u32 = tx.pragma_query_value(None, "user_version", |row| row.get(0)).map_err(|error| error.to_string())?;
    if current > SCHEMA_VERSION { return Err(format!("workspace registry schema {current} is newer than supported schema {SCHEMA_VERSION}")); }
    if current >= 15 { return tx.commit().map_err(|error| error.to_string()); }
    if current != 14 { return Err("Complete archive discovery migrations before report presentation receipts".into()); }
    tx.execute_batch("CREATE TABLE automation_report_sources (
        operation TEXT PRIMARY KEY,
        agent TEXT NOT NULL REFERENCES automation_snapshots(agent) ON DELETE CASCADE,
        card TEXT NOT NULL,
        registry TEXT NOT NULL,
        scope TEXT NOT NULL,
        revision INTEGER NOT NULL CHECK(revision>=0),
        pane TEXT NOT NULL
    ) STRICT;
    CREATE INDEX automation_report_sources_by_card ON automation_report_sources(card,operation);
    PRAGMA user_version=15;").map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

/// Called only after the exact source, captured registry/scope/revision, closed
/// owner and bounded private snapshot qualified in the same removal transaction.
pub(super) fn capture_removal(db: &Connection, operation: Uuid, scope: &Scope, revision: u64, pane: &Pane) -> Result<(), String> {
    if operation.is_nil() { return Err("The archive removal operation is missing".into()); }
    pane.validate()?;
    let agent = super::automation_membership::archived_owner_in(db, pane)?
        .ok_or("The original archive source is no longer available")?;
    let encoded = serde_json::to_string(pane).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_DESCRIPTOR { return Err("The archive source exceeds its descriptor limit".into()); }
    let identity: String = db.query_row("SELECT identity FROM metadata WHERE key=0", [], |row| row.get(0)).map_err(|error| error.to_string())?;
    db.execute("INSERT INTO automation_report_sources(operation,agent,card,registry,scope,revision,pane) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![operation.to_string(),agent.id.to_string(),pane.id.to_string(),identity,serde_json::to_string(scope).map_err(|error| error.to_string())?,revision,encoded])
        .map_err(|error| error.to_string())?;
    Ok(())
}

struct Source {
    reference: ArchiveReportReference,
    scope: Scope,
    revision: u64,
    pane: Pane,
}
fn source(db: &Connection, reference: &ArchiveReportReference) -> Result<Source, String> {
    if !reference.is_supported() { return Err("The saved report reference is unsupported".into()); }
    let identity: String = db.query_row("SELECT identity FROM metadata WHERE key=0", [], |row| row.get(0)).map_err(|error| error.to_string())?;
    if identity != reference.registry.to_string() { return Err("The saved report belongs to a different registry".into()); }
    let row: Option<(String,String,String,Option<String>,u64,Option<String>)> = db.query_row(
        "SELECT CASE WHEN length(CAST(agent AS BLOB))<=36 THEN agent ELSE '' END,CASE WHEN length(CAST(registry AS BLOB))<=36 THEN registry ELSE '' END,CASE WHEN length(CAST(card AS BLOB))<=36 THEN card ELSE '' END,CASE WHEN length(CAST(scope AS BLOB))<=16384 THEN scope END,revision,CASE WHEN length(CAST(pane AS BLOB))<=3145728 THEN pane END FROM automation_report_sources WHERE operation=?1",
        [reference.source_operation.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)))
        .optional().map_err(|error| error.to_string())?;
    let Some((agent,registry,card,scope,revision,pane))=row else { return Err("The saved report source expired or was never committed".into()); };
    if agent != reference.agent.to_string() || registry != identity { return Err("The saved report source owner changed".into()); }
    let scope: Scope=serde_json::from_str(&scope.ok_or("The saved report scope exceeds its limit")?).map_err(|_| "The saved report scope is unreadable")?;
    let pane: Pane=serde_json::from_str(&pane.ok_or("The saved report descriptor exceeds its limit")?).map_err(|_| "The saved report descriptor is unreadable")?;
    if pane.id.to_string()!=card {return Err("The saved report source card changed".into());}
    pane.validate()?;
    Ok(Source {reference:reference.clone(),scope,revision,pane})
}

pub(super) fn report_in(db: &Connection, reference: &ArchiveReportReference, session: &Session) -> Result<ArchiveReport,String> {
    let source=source(db,reference)?;
    // Merely possessing a pointer is insufficient. Re-read and qualify the
    // exact original closed source plus bounded history before normalizing the
    // presentation-only reference out of its original native descriptor.
    let mut original_pane=source.pane.clone();
    original_pane.id=CardId::from_uuid(reference.agent.as_uuid());
    if let crate::model::CardKind::Chat(native)=&mut original_pane.kind {
        if native.archive_report.as_ref().is_some_and(|inner|
            !inner.is_supported() || inner.registry!=reference.registry || inner.agent!=reference.agent) {
            return Err("The committed report source has different ownership".into());
        }
        native.archive_report=None;
    }
    let owner=super::automation_membership::archived_owner_in(db,&original_pane)?
        .ok_or("The saved report is no longer retained")?;
    if owner.id!=reference.agent { return Err("The saved report owner changed".into()); }
    let encoded: String=db.query_row("SELECT snapshot FROM automation_snapshots WHERE agent=?1 AND length(CAST(snapshot AS BLOB))<=4194304",[owner.id.to_string()],|row|row.get(0)).map_err(|_|"The saved report is unavailable or exceeds its limit")?;
    let archive=crate::automations::ArchivedConversation {agent:owner,snapshot:serde_json::from_str(&encoded).map_err(|_|"The saved report is unreadable")?};
    if session.archive_report.as_ref().is_some_and(|value| value!=reference) { return Err("The report presentation reference changed".into()); }
    let mut original=session.clone(); original.archive_report=None;
    ArchivedPaneReadiness::qualify(reference.agent,&original,&archive)?;
    let mut issued=original;issued.archive_report=Some(reference.clone());
    let presentation_scope=match &source.scope {Scope::Worktree{path,..}=>Some(path.clone()),Scope::Channel(_)=>source.pane.scope.clone()};
    Ok(ArchiveReport {reference:source.reference,scope:source.scope,source_revision:source.revision,presentation_scope,session_digest:ArchiveReport::session_digest(&issued)?,archive})
}

impl Registry {
    pub(crate) fn read_archive_report(&self, reference:&ArchiveReportReference, session:&Session)->Result<ArchiveReport,String>{
        report_in(&self.db,reference,session)
    }
    pub(crate) fn archive_report_source(&self, identity:Uuid, card:CardId, session:&Session)->Result<Option<ArchiveReport>,String>{
        let current:String=self.db.query_row("SELECT identity FROM metadata WHERE key=0",[],|row|row.get(0)).map_err(|error|error.to_string())?;
        if identity.is_nil() || current!=identity.to_string() {return Err("The selected report registry changed; wait for its current model".into());}
        let operation:Option<(String,String)>=self.db.query_row("SELECT operation,agent FROM automation_report_sources WHERE card=?1 ORDER BY operation LIMIT 1",[card.to_string()],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(|error|error.to_string())?;
        let Some((operation,agent))=operation else {
            if let Some(reference)=&session.archive_report {return self.read_archive_report(reference,session).map(Some);}
            // An ended original archive without a presentation receipt must
            // never fall through to provider resume, including after TTL.
            let owner:Option<Option<String>>=self.db.query_row(
                "SELECT CASE WHEN length(CAST(record AS BLOB))<=3145728 THEN record END FROM agents WHERE session_id=?1 AND json_type(record,'$.origin.Automation')='object' LIMIT 1",
                [session.id.to_string()],|row|row.get(0)).optional().map_err(|error|error.to_string())?;
            if let Some(owner)=owner {
                let owner:crate::agent_execution::Agent=serde_json::from_str(&owner.ok_or("The source owner exceeds its descriptor limit")?).map_err(|_|"The source owner is unreadable")?;
                // This is only a refusal fence, never a receipt or a permission
                // to learn another owner/native descriptor.
                if matches!(&owner.origin,crate::agent_execution::Origin::Automation{..}) {
                    return Err("Open this automation result from its run history; no committed read-only presentation source is available".into());
                }
            }
            return Ok(None);
        };
        let identity:String=self.db.query_row("SELECT identity FROM metadata WHERE key=0",[],|row|row.get(0)).map_err(|error|error.to_string())?;
        let reference=ArchiveReportReference {format:1,registry:identity.parse().map_err(|_|"The report registry identity is invalid")?,agent:agent.parse().map_err(|_|"The report owner identity is invalid")?,source_operation:operation.parse().map_err(|_|"The report operation is invalid")?};
        let source=source(&self.db,&reference)?;
        let mut requested=session.clone();
        if requested.archive_report.as_ref().is_some_and(|old|old!=&reference) {
            if !matches!(&source.pane.kind,crate::model::CardKind::Chat(captured) if captured==session) {
                return Err("The report's original closed presentation changed".into());
            }
            requested.archive_report=None;
        }
        self.read_archive_report(&reference,&requested).map(Some)
    }
}

pub(super) fn presentation_scope_in(db:&Connection,reference:&ArchiveReportReference)->Result<Option<PathBuf>,String>{
    let source=source(db,reference)?;
    Ok(match source.scope {Scope::Worktree{path,..}=>Some(path),Scope::Channel(_)=>source.pane.scope})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_receipt_migration_rechecks_the_reserved_version_and_preserves_newer_writers() {
        for newer in [15, SCHEMA_VERSION + 1] {
            let directory=tempfile::tempdir().unwrap();
            let path=directory.path().join("registry.sqlite3");
            let db=Connection::open(&path).unwrap();
            db.pragma_update(None,"user_version",14).unwrap();
            let observed:u32=db.pragma_query_value(None,"user_version",|row|row.get(0)).unwrap();
            assert_eq!(observed,14);
            // Advance after the opener's outer read, as a different writer
            // would do before this extension reserves its transaction.
            let competing=Connection::open(&path).unwrap();
            competing.pragma_update(None,"user_version",newer).unwrap();
            let result=migrate(&db);
            assert_eq!(result.is_ok(),newer==15);
            let current:u32=db.pragma_query_value(None,"user_version",|row|row.get(0)).unwrap();
            assert_eq!(current,newer);
            let tables:u64=db.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='automation_report_sources'",[],|row|row.get(0)).unwrap();
            assert_eq!(tables,0,"a compatible checkpoint is not replayed and a future one is never downgraded");
        }
    }

    #[test]
    fn report_receipt_migration_rolls_back_ddl_and_retries_without_losing_prior_state() {
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA user_version=14; CREATE TABLE preserved(value TEXT); INSERT INTO preserved VALUES('original'); CREATE TABLE automation_report_sources_by_card(value TEXT);").unwrap();
        assert!(migrate(&db).is_err());
        assert_eq!(db.pragma_query_value(None,"user_version",|row|row.get::<_,u32>(0)).unwrap(),14);
        assert_eq!(db.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='automation_report_sources'",[],|row|row.get::<_,u64>(0)).unwrap(),0);
        assert_eq!(db.query_row("SELECT value FROM preserved",[],|row|row.get::<_,String>(0)).unwrap(),"original");
        db.execute("DROP TABLE automation_report_sources_by_card",[]).unwrap();
        migrate(&db).unwrap();
        assert_eq!(db.pragma_query_value(None,"user_version",|row|row.get::<_,u32>(0)).unwrap(),15);
        assert_eq!(db.query_row("SELECT value FROM preserved",[],|row|row.get::<_,String>(0)).unwrap(),"original");
    }
}
