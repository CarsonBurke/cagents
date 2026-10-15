//! Future registry19 automation authority, inside the selected cumulative writer.
//! No descriptor, snapshot, notification receipt or retired helper is fabricated.
use rusqlite::{Connection, OptionalExtension, Transaction};

const OBJECTS: &[(&str,&str,&str)] = &[
    // Exact c924/dbc/2556 negative custody leaf: the outer controller alone
    // installs it. No original directory/owner authority is backfilled.
    ("table","project_operation_obligations",r###"CREATE TABLE project_operation_obligations (
 operation TEXT PRIMARY KEY CHECK(length(operation)=36),
 registry TEXT NOT NULL CHECK(length(registry)=36),
 repository TEXT NOT NULL CHECK(length(repository) BETWEEN 1 AND 16384),
 project_ordinal INTEGER NOT NULL CHECK(project_ordinal BETWEEN 0 AND 4294967295),
 request_digest TEXT NOT NULL CHECK(length(request_digest)=64),
 commit_digest TEXT NOT NULL CHECK(length(commit_digest)=64),
 phase TEXT NOT NULL CHECK(phase IN ('admitted','repairing','closing','pending_commit','model_committed','resolved','aborted')),
 scope TEXT CHECK(scope IS NULL OR length(CAST(scope AS BLOB))<=1048576),
 committed_sequence INTEGER CHECK(committed_sequence IS NULL OR committed_sequence>=0),
 CHECK((phase IN ('resolved','aborted')) = (scope IS NULL)),
 CHECK((phase IN ('model_committed','resolved')) = (committed_sequence IS NOT NULL))
) STRICT;"###),
    ("index","project_operation_obligations_pending_project",r###"CREATE INDEX project_operation_obligations_pending_project
 ON project_operation_obligations(registry,repository,project_ordinal)
 WHERE phase NOT IN ('resolved','aborted');"###),
    ("table","created_start_requirements",r###"CREATE TABLE created_start_requirements (
        session_id TEXT PRIMARY KEY, card_id TEXT NOT NULL,
        intent_operation TEXT NOT NULL, generation INTEGER NOT NULL CHECK(generation>=0),
        attempt TEXT NOT NULL UNIQUE, kind TEXT NOT NULL CHECK(kind IN('terminal','native_chat')),
        phase TEXT NOT NULL CHECK(phase IN('required','started','retired')), started_receipt TEXT,
        CHECK(phase!='required' OR started_receipt IS NULL),
        CHECK(phase!='started' OR started_receipt IS NOT NULL)
    ) STRICT;"###),
    ("index","created_start_requirements_required_card",r###"CREATE UNIQUE INDEX created_start_requirements_required_card ON created_start_requirements(card_id) WHERE phase='required';"###),
    ("index","created_start_requirements_card_phase",r###"CREATE INDEX created_start_requirements_card_phase ON created_start_requirements(card_id,phase);"###),
    ("table","automation_notifications",r###"CREATE TABLE automation_notifications (key TEXT PRIMARY KEY, event TEXT NOT NULL) STRICT;"###),
    ("table","automation_retired_manual_requests",r###"CREATE TABLE automation_retired_manual_requests (
        automation TEXT NOT NULL, request TEXT NOT NULL, revision INTEGER NOT NULL,
        PRIMARY KEY(automation,request)
    ) STRICT;"###),
    ("table","automation_snapshots",r###"CREATE TABLE automation_snapshots (
        firing TEXT PRIMARY KEY REFERENCES automation_firings(id) ON DELETE CASCADE,
        agent TEXT NOT NULL UNIQUE, snapshot TEXT NOT NULL
    ) STRICT;"###),
    ("table","automation_precheck_retirements",r###"CREATE TABLE automation_precheck_retirements (
        firing TEXT PRIMARY KEY REFERENCES automation_firings(id) ON DELETE RESTRICT,
        revision INTEGER NOT NULL, retired INTEGER NOT NULL CHECK(retired IN (0,1)),
        notify INTEGER NOT NULL CHECK(notify IN (0,1))
    ) STRICT;"###),
    ("table","automation_report_usage",r###"CREATE TABLE automation_report_usage (
        key INTEGER PRIMARY KEY CHECK(key=0), descriptors INTEGER NOT NULL DEFAULT 0 CHECK(descriptors>=0),
        receipts INTEGER NOT NULL DEFAULT 0 CHECK(receipts>=0), receipt_bytes INTEGER NOT NULL DEFAULT 0 CHECK(receipt_bytes>=0)
    ) STRICT;"###),
    ("table","automation_report_descriptors",r###"CREATE TABLE automation_report_descriptors (
        agent TEXT NOT NULL CHECK(length(CAST(agent AS BLOB))=36) REFERENCES automation_snapshots(agent) ON DELETE CASCADE,
        digest TEXT NOT NULL CHECK(length(CAST(digest AS BLOB))=64), content TEXT NOT NULL, PRIMARY KEY(agent,digest)
    ) STRICT;"###),
    ("table","automation_report_sources",r###"CREATE TABLE automation_report_sources (
        operation TEXT PRIMARY KEY CHECK(length(CAST(operation AS BLOB))=36),
        agent TEXT NOT NULL CHECK(length(CAST(agent AS BLOB))=36) REFERENCES automation_snapshots(agent) ON DELETE CASCADE,
        card TEXT NOT NULL CHECK(length(CAST(card AS BLOB))=36), registry TEXT NOT NULL CHECK(length(CAST(registry AS BLOB))=36), scope TEXT NOT NULL, repository TEXT NOT NULL,
        revision INTEGER NOT NULL CHECK(revision>=0), descriptor TEXT NOT NULL CHECK(length(CAST(descriptor AS BLOB))=64), presentation TEXT NOT NULL,
        FOREIGN KEY(agent,descriptor) REFERENCES automation_report_descriptors(agent,digest) ON DELETE CASCADE
    ) STRICT;"###),
    ("index","automation_report_sources_by_card",r###"CREATE INDEX automation_report_sources_by_card ON automation_report_sources(card,operation);"###),
    ("index","automation_report_sources_by_agent",r###"CREATE INDEX automation_report_sources_by_agent ON automation_report_sources(agent);"###),
    ("trigger","automation_report_descriptor_budget",r###"CREATE TRIGGER automation_report_descriptor_budget BEFORE INSERT ON automation_report_descriptors
      WHEN (SELECT count(*) FROM automation_report_usage WHERE key=0)!=1 OR length(CAST(NEW.content AS BLOB))>3145728 OR
        (SELECT descriptors FROM automation_report_usage WHERE key=0)+length(CAST(NEW.content AS BLOB))>1073741824
      BEGIN SELECT RAISE(ABORT,'Saved report descriptor budget is full; its pane and run report were preserved'); END;"###),
    ("trigger","automation_report_descriptor_added",r###"CREATE TRIGGER automation_report_descriptor_added AFTER INSERT ON automation_report_descriptors BEGIN
      UPDATE automation_report_usage SET descriptors=descriptors+length(CAST(NEW.content AS BLOB)) WHERE key=0; END;"###),
    ("trigger","automation_report_descriptor_removed",r###"CREATE TRIGGER automation_report_descriptor_removed AFTER DELETE ON automation_report_descriptors BEGIN
      UPDATE automation_report_usage SET descriptors=descriptors-length(CAST(OLD.content AS BLOB)) WHERE key=0; END;"###),
    ("trigger","automation_report_source_budget",r###"CREATE TRIGGER automation_report_source_budget BEFORE INSERT ON automation_report_sources
      WHEN (SELECT count(*) FROM automation_report_usage WHERE key=0)!=1 OR length(CAST(NEW.presentation AS BLOB))>16384 OR length(CAST(NEW.scope AS BLOB))>16384 OR length(CAST(NEW.repository AS BLOB))>16384 OR
        (SELECT receipts FROM automation_report_usage WHERE key=0)>=65536 OR
        (SELECT count(*) FROM automation_report_sources WHERE agent=NEW.agent)>=4096 OR
        (SELECT receipt_bytes FROM automation_report_usage WHERE key=0)+length(CAST(NEW.presentation AS BLOB))+length(CAST(NEW.scope AS BLOB))+length(CAST(NEW.repository AS BLOB))+256>67108864
      BEGIN SELECT RAISE(ABORT,'Saved report receipt budget is full; its pane and run report were preserved'); END;"###),
    ("trigger","automation_report_source_added",r###"CREATE TRIGGER automation_report_source_added AFTER INSERT ON automation_report_sources BEGIN
      UPDATE automation_report_usage SET receipts=receipts+1,receipt_bytes=receipt_bytes+length(CAST(NEW.presentation AS BLOB))+length(CAST(NEW.scope AS BLOB))+length(CAST(NEW.repository AS BLOB))+256 WHERE key=0; END;"###),
    ("trigger","automation_report_source_removed",r###"CREATE TRIGGER automation_report_source_removed AFTER DELETE ON automation_report_sources BEGIN
      UPDATE automation_report_usage SET receipts=receipts-1,receipt_bytes=receipt_bytes-length(CAST(OLD.presentation AS BLOB))-length(CAST(OLD.scope AS BLOB))-length(CAST(OLD.repository AS BLOB))-256 WHERE key=0; END;"###),
    ("trigger","automation_report_descriptor_immutable",r###"CREATE TRIGGER automation_report_descriptor_immutable BEFORE UPDATE ON automation_report_descriptors
      BEGIN SELECT RAISE(ABORT,'Saved report descriptors are immutable'); END;"###),
    ("trigger","automation_report_source_immutable",r###"CREATE TRIGGER automation_report_source_immutable BEFORE UPDATE ON automation_report_sources
      BEGIN SELECT RAISE(ABORT,'Saved report receipts are immutable'); END;"###),
    ("index","global_pending_automation_proposals",r###"CREATE INDEX global_pending_automation_proposals ON automation_proposals(id) WHERE pending=1;"###),
    ("trigger","automation_archive_snapshot_insert",r###"CREATE TRIGGER automation_archive_snapshot_insert AFTER INSERT ON automation_snapshots WHEN EXISTS(SELECT 1 FROM agents WHERE id=NEW.agent AND closed=1) BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1,sequence=sequence+1 WHERE key=0; END;"###),
    ("trigger","automation_archive_snapshot_update",r###"CREATE TRIGGER automation_archive_snapshot_update AFTER UPDATE ON automation_snapshots WHEN (OLD.snapshot IS NOT NEW.snapshot OR OLD.agent IS NOT NEW.agent) AND (EXISTS(SELECT 1 FROM agents WHERE id=OLD.agent AND closed=1) OR EXISTS(SELECT 1 FROM agents WHERE id=NEW.agent AND closed=1)) BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1,sequence=sequence+1 WHERE key=0; END;"###),
    ("trigger","automation_archive_snapshot_delete",r###"CREATE TRIGGER automation_archive_snapshot_delete AFTER DELETE ON automation_snapshots WHEN EXISTS(SELECT 1 FROM agents WHERE id=OLD.agent AND closed=1) BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1,sequence=sequence+1 WHERE key=0; END;"###),
    ("trigger","automation_archive_agent_insert",r###"CREATE TRIGGER automation_archive_agent_insert AFTER INSERT ON agents WHEN NEW.closed=1 AND EXISTS(SELECT 1 FROM automation_snapshots WHERE agent=NEW.id) BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1,sequence=sequence+1 WHERE key=0; END;"###),
    ("trigger","automation_archive_agent_update",r###"CREATE TRIGGER automation_archive_agent_update AFTER UPDATE OF record,closed ON agents WHEN (OLD.closed=1 OR NEW.closed=1) AND (OLD.record IS NOT NEW.record OR OLD.closed IS NOT NEW.closed) AND EXISTS(SELECT 1 FROM automation_snapshots WHERE agent=NEW.id) BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1,sequence=sequence+1 WHERE key=0; END;"###),
    ("trigger","automation_archive_agent_delete",r###"CREATE TRIGGER automation_archive_agent_delete AFTER DELETE ON agents WHEN OLD.closed=1 AND EXISTS(SELECT 1 FROM automation_snapshots WHERE agent=OLD.id) BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1,sequence=sequence+1 WHERE key=0; END;"###),
    ("trigger","automation_archive_view_insert",r###"CREATE TRIGGER automation_archive_view_insert AFTER INSERT ON shared_views  BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1 WHERE key=0; END;"###),
    ("trigger","automation_archive_view_update",r###"CREATE TRIGGER automation_archive_view_update AFTER UPDATE ON shared_views WHEN OLD.panes IS NOT NEW.panes OR OLD.retired IS NOT NEW.retired BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1 WHERE key=0; END;"###),
    ("trigger","automation_archive_view_delete",r###"CREATE TRIGGER automation_archive_view_delete AFTER DELETE ON shared_views  BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1 WHERE key=0; END;"###),
    ("trigger","automation_archive_project_insert",r###"CREATE TRIGGER automation_archive_project_insert AFTER INSERT ON projects  BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1 WHERE key=0; END;"###),
    ("trigger","automation_archive_project_delete",r###"CREATE TRIGGER automation_archive_project_delete AFTER DELETE ON projects  BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1 WHERE key=0; END;"###),
    ("trigger","automation_archive_channel_insert",r###"CREATE TRIGGER automation_archive_channel_insert AFTER INSERT ON channels  BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1 WHERE key=0; END;"###),
    ("trigger","automation_archive_channel_delete",r###"CREATE TRIGGER automation_archive_channel_delete AFTER DELETE ON channels  BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1 WHERE key=0; END;"###),
    ("trigger","automation_archive_visibility",r###"CREATE TRIGGER automation_archive_visibility AFTER UPDATE OF record ON automations WHEN json_extract(OLD.record,'$.definition.visibility') IS NOT json_extract(NEW.record,'$.definition.visibility') BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1 WHERE key=0; END;"###),
];
const PROJECTS_BASE:&str=r###"CREATE TABLE projects(
    repository TEXT PRIMARY KEY, name TEXT NOT NULL, ordinal INTEGER NOT NULL,
    kind TEXT NOT NULL DEFAULT 'git' CHECK(kind IN ('git', 'folder'))
) STRICT"###;
const PROJECTS_TARGET:&str=r###"CREATE TABLE projects(
    repository TEXT PRIMARY KEY, name TEXT NOT NULL, ordinal INTEGER NOT NULL,
    kind TEXT NOT NULL DEFAULT 'git' CHECK(kind IN ('git', 'folder')),
    checkout TEXT, primary_checkout TEXT
) STRICT"###;
const METADATA_BASE:&str=r###"CREATE TABLE metadata(
             key INTEGER PRIMARY KEY CHECK(key = 0),
             identity TEXT NOT NULL,
             sequence INTEGER NOT NULL,
             next_project_ordinal INTEGER NOT NULL,
             next_channel_ordinal INTEGER NOT NULL
         ) STRICT"###;
const METADATA_TARGET:&str=r###"CREATE TABLE metadata(
             key INTEGER PRIMARY KEY CHECK(key = 0),
             identity TEXT NOT NULL,
             sequence INTEGER NOT NULL,
             next_project_ordinal INTEGER NOT NULL,
             next_channel_ordinal INTEGER NOT NULL
         , automation_proposal_sequence INTEGER NOT NULL DEFAULT 0 CHECK(automation_proposal_sequence>=0), automation_archive_sequence INTEGER NOT NULL DEFAULT 0 CHECK(automation_archive_sequence>=0)) STRICT"###;

fn definition(db:&Connection,kind:&str,name:&str)->Result<Option<String>,String> {
    let row:Option<Option<String>>=db.query_row("SELECT CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql END FROM sqlite_schema WHERE type=?1 AND name=?2",[kind,name],|row|row.get(0)).optional().map_err(|error|error.to_string())?;
    match row {Some(None)=>Err("The automation schema definition exceeds its inspection budget".into()),Some(Some(sql))=>Ok(Some(sql)),None=>Ok(None)}
}

/// Called in both the constructor's pinned read transaction and the admitted
/// writer transaction. A number never authorizes an ambiguous partial layout.
pub(super) fn validate(db:&Connection,current:u32)->Result<(),String> {
    if current>19 {return Err("The automation registry requires a newer build".into());}
    // Discovery paths are retained metadata, never inferred filesystem or
    // execution authority. Published predecessors must not contain this fork.
    let projects=definition(db,"table","projects")?;
    if current<=12 {
        let private_columns:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('projects') WHERE name IN ('checkout','primary_checkout'))",[],|row|row.get(0)).map_err(|error|error.to_string())?;
        if private_columns {return Err("The published registry contains unpublished checkout discovery columns; refusing an ambiguous partial migration".into());}
    } else {
        let expected=if current==19{PROJECTS_TARGET}else{PROJECTS_BASE};
        if projects.as_deref().map(super::schema::normalized)!=Some(super::schema::normalized(expected)) {
            return Err("The project discovery layout does not match its admitted registry checkpoint".into());
        }
    }
    let metadata=definition(db,"table","metadata")?;
    let expected=if current==19 {METADATA_TARGET}else{METADATA_BASE};
    if !(current==0&&metadata.is_none()) && metadata.as_deref().map(super::schema::normalized)!=Some(super::schema::normalized(expected)) {
        return Err("The registry metadata layout does not match its admitted automation checkpoint".into());
    }
    for &(kind,name,sql) in OBJECTS {
        let actual=definition(db,kind,name)?;
        if current<=18 {
            if actual.is_some() {return Err(format!("Registry {current} unexpectedly contains private automation object {name}; refusing partial authority"));}
        } else if actual.as_deref().map(super::schema::normalized)!=Some(super::schema::normalized(sql)) {
            return Err(format!("Registry19 has missing or incompatible automation object {name}"));
        }
    }
    Ok(())
}

/// The caller owns BEGIN IMMEDIATE and has already re-admitted the header and
/// exact selected18 foundation. Stamp19 only with that same outer transaction.
pub(super) fn apply(tx:&Transaction<'_>)->Result<(),String> {
    apply_with_hook(tx, |_| Ok(()))
}

fn apply_with_hook(tx:&Transaction<'_>,mut after_step:impl FnMut(usize)->Result<(),String>)->Result<(),String> {
    let mut step=0;
    let mut completed=|| { let current=step;step+=1;after_step(current) };
    validate(tx,18)?;
    // Old identities/names/filing survive exactly. No checkout is guessed from
    // a repository key, path spelling, or former working directory.
    tx.execute_batch("ALTER TABLE projects ADD COLUMN checkout TEXT;").map_err(|error|error.to_string())?;
    completed()?;
    tx.execute_batch("ALTER TABLE projects ADD COLUMN primary_checkout TEXT;").map_err(|error|error.to_string())?;
    completed()?;
    tx.execute_batch(r###"ALTER TABLE metadata ADD COLUMN automation_proposal_sequence INTEGER NOT NULL DEFAULT 0 CHECK(automation_proposal_sequence>=0); ALTER TABLE metadata ADD COLUMN automation_archive_sequence INTEGER NOT NULL DEFAULT 0 CHECK(automation_archive_sequence>=0);"###).map_err(|error|error.to_string())?;
    completed()?;
    // Tables precede indexes/triggers; no observer can see a partial checkpoint.
    for &(kind,_,sql) in OBJECTS {
        if kind=="table" {tx.execute_batch(sql).map_err(|error|error.to_string())?;completed()?;}
    }
    tx.execute_batch(r###"INSERT INTO automation_report_usage(key) VALUES(0);"###).map_err(|error|error.to_string())?;
    completed()?;
    // Preserve unresolved legacy helper authority, never infer process exit.
    tx.execute_batch(r###"INSERT OR IGNORE INTO automation_precheck_retirements(firing,revision,retired,notify)
        SELECT f.id,f.revision,0,json_extract(f.record,'$.state')='prechecking' FROM automation_firings f
        JOIN automation_revisions r ON r.automation=f.automation AND r.revision=f.revision
        WHERE json_type(r.definition,'$.precheck')='object';"###).map_err(|error|error.to_string())?;
    completed()?;
    for &(kind,_,sql) in OBJECTS {
        if kind!="table" {tx.execute_batch(sql).map_err(|error|error.to_string())?;completed()?;}
    }
    validate(tx,19)
}

/// Legacy fixture constructors remove only this extension's fixed objects and
/// clock columns before replaying published primitives. No production repair.
#[cfg(test)]
pub(super) fn discard_for_legacy_fixture(db:&Connection) {
    for kind in ["trigger","index","table"] {
        for &(actual,name,_) in OBJECTS.iter().rev() {
            if actual==kind {db.execute_batch(&format!("DROP {kind} IF EXISTS {name}")).unwrap();}
        }
    }
    for name in ["primary_checkout","checkout"] {
        let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('projects') WHERE name=?1)",[name],|row|row.get(0)).unwrap();
        if exists {db.execute_batch(&format!("ALTER TABLE projects DROP COLUMN {name}")).unwrap();}
    }
    for name in ["automation_archive_sequence","automation_proposal_sequence"] {
        let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('metadata') WHERE name=?1)",[name],|row|row.get(0)).unwrap();
        if exists {db.execute_batch(&format!("ALTER TABLE metadata DROP COLUMN {name}")).unwrap();}
    }
}

/// Historical project identity/filing values are exact. Only the two admitted
/// discovery columns may appear, both NULL rather than guessed from old paths.
#[cfg(test)]
pub(super) fn assert_checkout_only_extension(old_ddl:&str,old_rows:&[Vec<rusqlite::types::Value>],new_ddl:&str,new_rows:&[Vec<rusqlite::types::Value>]) {
    assert_eq!(super::schema::normalized(old_ddl),super::schema::normalized(PROJECTS_BASE));
    assert_eq!(super::schema::normalized(new_ddl),super::schema::normalized(PROJECTS_TARGET));
    assert_eq!(new_rows.len(),old_rows.len(),"original projects must not be inserted or removed");
    for (old,new) in old_rows.iter().zip(new_rows) {
        assert_eq!(old.len(),4,"genuine predecessor project layout");
        assert_eq!(new.len(),6,"only two discovery columns are appended");
        assert_eq!(&new[..4],old,"original repository/name/ordinal/kind remain exact");
        assert_eq!(&new[4..],&[rusqlite::types::Value::Null,rusqlite::types::Value::Null],"no checkout is inferred from original project metadata");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::model::{Registry,schema};

    #[test]
    fn registry19_refuses_partial_extension_objects_and_incompatible_clocks_without_repair() {
        for partial in 0..2 {
            let root=tempfile::tempdir().unwrap();let path=root.path().join("registry.sqlite3");
            let registry=Registry::at(&path).unwrap();
            discard_for_legacy_fixture(&registry.db);
            registry.db.pragma_update(None,"user_version",18).unwrap();
            if partial==0 {registry.db.execute_batch("CREATE TABLE automation_snapshots(firing TEXT PRIMARY KEY, agent TEXT, snapshot TEXT)").unwrap();}
            else {registry.db.execute_batch("ALTER TABLE metadata ADD COLUMN automation_proposal_sequence INTEGER NOT NULL DEFAULT 0").unwrap();}
            let before=schema::stored_contents(&registry.db);
            assert!(schema::migrate(&registry.db).is_err());
            assert_eq!(schema::stored_contents(&registry.db),before);
            assert_eq!(registry.db.pragma_query_value(None,"user_version",|row|row.get::<_,u32>(0)).unwrap(),18);
        }
    }

    #[test]
    fn registry19_project_custody_is_empty_until_admitted_and_partial_catalogs_refuse() {
        for missing in ["index", "table"] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("registry.sqlite3");
            let registry = Registry::at(&path).unwrap();
            assert_eq!(registry.db.query_row("SELECT count(*) FROM project_operation_obligations", [], |row| row.get::<_, u64>(0)).unwrap(), 0,
                "migration must not invent repair, close, or retry custody");
            let sql = if missing == "index" {
                "DROP INDEX project_operation_obligations_pending_project"
            } else {
                "DROP TABLE project_operation_obligations"
            };
            registry.db.execute_batch(sql).unwrap();
            let before = schema::stored_contents(&registry.db);
            let version: u32 = registry.db.pragma_query_value(None, "user_version", |row| row.get(0)).unwrap();
            assert!(Registry::at(&path).is_err());
            assert_eq!(schema::stored_contents(&registry.db), before,
                "a matching number must not repair missing custody {missing}");
            assert_eq!(registry.db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0)).unwrap(), version);
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("registry.sqlite3");
        let registry = Registry::at(&path).unwrap();
        discard_for_legacy_fixture(&registry.db);
        registry.db.pragma_update(None, "user_version", 18).unwrap();
        for &(kind, name, sql) in OBJECTS {
            if name.starts_with("project_operation_obligations") {
                assert!(matches!(kind, "table" | "index"));
                registry.db.execute_batch(sql).unwrap();
            }
        }
        let before = schema::stored_contents(&registry.db);
        assert!(Registry::at(&path).is_err(), "private custody rows cannot hide under checkpoint18");
        assert_eq!(schema::stored_contents(&registry.db), before);
    }

    #[test]
    fn registry19_second_connection_advances_are_rechecked_without_downgrade_or_replay() {
        for future in [false,true] {
            let root=tempfile::tempdir().unwrap();let path=root.path().join("registry.sqlite3");
            let registry=Registry::at(&path).unwrap();
            discard_for_legacy_fixture(&registry.db);
            registry.db.pragma_update(None,"user_version",18).unwrap();
            let mut after=None;
            let outcome=schema::migrate_after_read(&registry.db,|| {
                let other=Connection::open(&path).map_err(|error|error.to_string())?;
                if future {other.pragma_update(None,"user_version",20).map_err(|error|error.to_string())?;}
                else {schema::migrate(&other)?;}
                other.execute_batch("CREATE TABLE concurrent_intent(value TEXT); INSERT INTO concurrent_intent VALUES('preserve exact second writer');").map_err(|error|error.to_string())?;
                after=Some(schema::stored_contents(&other));
                Ok(())
            });
            assert_eq!(outcome.is_ok(),!future);
            assert_eq!(schema::stored_contents(&registry.db),after.unwrap());
            assert_eq!(registry.db.pragma_query_value(None,"user_version",|row|row.get::<_,u32>(0)).unwrap(),if future{20}else{19});
        }
    }

    #[test]
    fn registry19_does_not_backfill_closed_snapshots_report_readers_or_executable_authority() {
        let root=tempfile::tempdir().unwrap();let path=root.path().join("registry.sqlite3");
        let db=Connection::open(&path).unwrap();
        let tx=db.unchecked_transaction().unwrap();
        crate::host::model::migrate_legacy(&tx,0,12).unwrap();
        let agent=crate::agent_execution::Agent {
            name:Some("retained closed automation".into()),id:crate::model::AgentId::new(),
            origin:crate::agent_execution::Origin::Automation{id:uuid::Uuid::new_v4()},
            spec:crate::agent_execution::Spec {
                provider:crate::agent::Provider::Claude,account:None,
                target:crate::agent_execution::Target{repository:"/original-missing".into(),directory:"/original-missing/child".into(),placement:None},
                surface:crate::agent_execution::Surface::Chat,model:None,permission_mode:None,reasoning_effort:None,
            },
            placement:crate::agent_execution::Placement::Channel(crate::model::ChannelId::new()),
            session:None,console:None,created_ms:1,closed:true,
        };
        let record=serde_json::to_string(&agent).unwrap();
        tx.execute("INSERT INTO agents(id,origin,record,closed) VALUES(?1,?2,?3,1)",rusqlite::params![agent.id.to_string(),serde_json::to_string(&agent.origin).unwrap(),record]).unwrap();
        tx.execute("INSERT INTO agent_policies(agent,directory) VALUES(?1,NULL)",[agent.id.to_string()]).unwrap();
        tx.execute("INSERT INTO projects(repository,name,ordinal,kind) VALUES('/original-missing','preserve original filing',1,'folder')",[]).unwrap();
        tx.pragma_update(None,"user_version",12).unwrap();tx.commit().unwrap();drop(db);
        let registry=Registry::at(&path).unwrap();
        assert_eq!(registry.db.query_row("SELECT record FROM agents WHERE id=?1",[agent.id.to_string()],|row|row.get::<_,String>(0)).unwrap(),record);
        assert_eq!(registry.db.query_row("SELECT name FROM projects WHERE repository='/original-missing'",[],|row|row.get::<_,String>(0)).unwrap(),"preserve original filing");
        let policy:Option<String>=registry.db.query_row("SELECT directory FROM agent_policies WHERE agent=?1",[agent.id.to_string()],|row|row.get(0)).unwrap();
        assert!(policy.is_none(),"path metadata must not mint a physical grant");
        for table in ["automation_snapshots","automation_report_descriptors","automation_report_sources","automation_notifications","automation_retired_manual_requests","created_start_requirements","project_operation_obligations"] {
            assert_eq!(registry.db.query_row(&format!("SELECT count(*) FROM {table}"),[],|row|row.get::<_,u64>(0)).unwrap(),0);
        }
        assert_eq!(registry.db.query_row("SELECT descriptors,receipts,receipt_bytes FROM automation_report_usage WHERE key=0",[],|row|Ok((row.get::<_,u64>(0)?,row.get::<_,u64>(1)?,row.get::<_,u64>(2)?))).unwrap(),(0,0,0));
        let before=schema::stored_contents(&registry.db);
        schema::migrate(&registry.db).unwrap();
        assert_eq!(schema::stored_contents(&registry.db),before);
    }
    #[test]
    fn registry19_created_start_required_card_is_unique_without_erasing_retired_session_history() {
        let root=tempfile::tempdir().unwrap();let path=root.path().join("registry.sqlite3");
        let registry=Registry::at(&path).unwrap();
        let card=uuid::Uuid::new_v4().to_string();
        let first=uuid::Uuid::new_v4().to_string();let second=uuid::Uuid::new_v4().to_string();
        let operation=uuid::Uuid::new_v4().to_string();let attempt=uuid::Uuid::new_v4().to_string();
        registry.db.execute("INSERT INTO created_start_requirements(session_id,card_id,intent_operation,generation,attempt,kind,phase) VALUES(?1,?2,?3,0,?4,'native_chat','required')",rusqlite::params![first,card,operation,attempt]).unwrap();
        assert!(registry.db.execute("INSERT INTO created_start_requirements(session_id,card_id,intent_operation,generation,attempt,kind,phase) VALUES(?1,?2,?3,1,?4,'terminal','required')",rusqlite::params![second,card,operation,uuid::Uuid::new_v4().to_string()]).is_err());
        assert!(registry.db.execute("UPDATE created_start_requirements SET phase='started' WHERE session_id=?1",[&first]).is_err(),"a successful RPC is not a start receipt");
        registry.db.execute("UPDATE created_start_requirements SET phase='retired' WHERE session_id=?1",[&first]).unwrap();
        registry.db.execute("INSERT INTO created_start_requirements(session_id,card_id,intent_operation,generation,attempt,kind,phase) VALUES(?1,?2,?3,1,?4,'terminal','required')",rusqlite::params![second,card,operation,uuid::Uuid::new_v4().to_string()]).unwrap();
        drop(registry);
        let registry=Registry::at(&path).unwrap();
        assert_eq!(registry.db.query_row("SELECT phase,started_receipt FROM created_start_requirements WHERE session_id=?1",[&first],|row|Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?))).unwrap(),("retired".into(),None));
        assert_eq!(registry.db.query_row("SELECT count(*) FROM created_start_requirements",[],|row|row.get::<_,u64>(0)).unwrap(),2);
    }

    #[test]
    fn registry19_started_history_lookup_seeks_the_exact_card_without_scanning_permanent_history() {
        let root=tempfile::tempdir().unwrap();let path=root.path().join("registry.sqlite3");
        let registry=Registry::at(&path).unwrap();
        let plan:Vec<String>=registry.db.prepare("EXPLAIN QUERY PLAN SELECT EXISTS(SELECT 1 FROM created_start_requirements WHERE card_id=?1 AND phase='started')").unwrap()
            .query_map([uuid::Uuid::new_v4().to_string()],|row|row.get(3)).unwrap().map(Result::unwrap).collect();
        assert!(plan.iter().any(|step|step.contains("SEARCH created_start_requirements USING COVERING INDEX created_start_requirements_card_phase (card_id=? AND phase=?)")),"{plan:?}");
        registry.db.execute_batch("DROP INDEX created_start_requirements_card_phase").unwrap();
        assert!(validate(&registry.db,19).unwrap_err().contains("created_start_requirements_card_phase"),"a matching version must not admit a partial cost-bound checkpoint");
    }

    #[test]
    fn registry19_checkout_discovery_preserves_original_values_and_never_guesses_legacy_paths() {
        let root=tempfile::tempdir().unwrap();let path=root.path().join("registry.sqlite3");
        let registry=Registry::at(&path).unwrap();
        registry.db.execute("INSERT INTO projects(repository,name,ordinal,kind,checkout,primary_checkout) VALUES('/metadata/project.git','preserved original name',1,'git','/known linked','/known primary')",[]).unwrap();
        drop(registry);
        for _ in 0..2 {
            let registry=Registry::at(&path).unwrap();
            let retained=registry.db.query_row("SELECT repository,name,ordinal,kind,checkout,primary_checkout FROM projects",[],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,u64>(2)?,row.get::<_,String>(3)?,row.get::<_,Option<String>>(4)?,row.get::<_,Option<String>>(5)?))).unwrap();
            assert_eq!(retained,("/metadata/project.git".into(),"preserved original name".into(),1,"git".into(),Some("/known linked".into()),Some("/known primary".into())));
        }
    }

    #[test]
    fn registry19_partial_checkout_forks_refuse_before_any_schema_repair() {
        for checkpoint in [12,17,18,19] {
            let root=tempfile::tempdir().unwrap();let path=root.path().join("registry.sqlite3");
            let registry=Registry::at(&path).unwrap();
            if checkpoint<=12 {schema::discard_selected_for_legacy_fixture(&registry.db);}
            else if checkpoint<19 {discard_for_legacy_fixture(&registry.db);}
            else {registry.db.execute_batch("ALTER TABLE projects DROP COLUMN primary_checkout").unwrap();}
            if checkpoint<19 {registry.db.execute_batch("ALTER TABLE projects ADD COLUMN checkout TEXT").unwrap();}
            registry.db.pragma_update(None,"user_version",checkpoint).unwrap();
            let before=schema::stored_contents(&registry.db);
            assert!(schema::migrate(&registry.db).is_err(),"partial checkpoint {checkpoint}");
            assert_eq!(schema::stored_contents(&registry.db),before);
            assert_eq!(registry.db.pragma_query_value(None,"user_version",|row|row.get::<_,u32>(0)).unwrap(),checkpoint);
        }
    }

    #[test]
    fn registry19_every_extension_step_rolls_back_metadata_rows_and_checkpoint_atomically() {
        // Metadata ALTERs, every table/index/trigger, usage seed and conservative
        // precheck ownership copy all run under the same cumulative writer.
        for fail_at in 0..OBJECTS.len()+5 {
            let root=tempfile::tempdir().unwrap();let path=root.path().join("registry.sqlite3");
            let registry=Registry::at(&path).unwrap();
            discard_for_legacy_fixture(&registry.db);
            registry.db.pragma_update(None,"user_version",18).unwrap();
            registry.db.execute("INSERT INTO projects(repository,name,ordinal,kind) VALUES('/preserved','original authority',1,'folder')",[]).unwrap();
            let before=schema::stored_contents(&registry.db);
            let tx=registry.db.unchecked_transaction().unwrap();
            let error=apply_with_hook(&tx,|step|if step==fail_at{Err("injected extension interruption".into())}else{Ok(())}).unwrap_err();
            assert!(error.contains("injected extension interruption"),"step {fail_at}: {error}");
            drop(tx);
            assert_eq!(schema::stored_contents(&registry.db),before,"rollback step {fail_at}");
            assert_eq!(registry.db.pragma_query_value(None,"user_version",|row|row.get::<_,u32>(0)).unwrap(),18);
            schema::migrate(&registry.db).unwrap();
            assert_eq!(registry.db.query_row("SELECT name FROM projects WHERE repository='/preserved'",[],|row|row.get::<_,String>(0)).unwrap(),"original authority");
            assert_eq!(registry.db.pragma_query_value(None,"user_version",|row|row.get::<_,u32>(0)).unwrap(),19);
        }
    }

}
